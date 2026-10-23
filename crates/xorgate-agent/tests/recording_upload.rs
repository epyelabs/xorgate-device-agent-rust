//! Phase 3 WP3: `recording::{sources, uploader}` against the TS suites
//! `test/recording-uploader.test.ts` (telemetry segment source ×3, Uploader
//! ×6) and `test/recording-video-source.test.ts` (video segment source ×7,
//! uploader with video pairs ×2) @ 89b5c64, step by step, with a fake
//! `ObjectPut` and `tokio::time::pause` instead of real delays; plus the full
//! runtime replay of `fixtures/upload_step/uploader-drain.json` (13 scenarios
//! dumped from the REAL TS Uploader: events, retry delays, PUTs, status
//! edges, onUploaded calls, files left).

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use xorgate_agent::ctx::AgentClock;
use xorgate_agent::recording::sources::{
    telemetry_source, video_source, SegmentSource, SidecarRef, UploadSegment, VideoSourceOpts,
};
use xorgate_agent::recording::uploader::{Uploader, UploaderOpts};
use xorgate_cloud::s3::{ObjectPut, PutError, PutFuture};
use xorgate_core::settings::UploadMode;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_hal::io::{FsOp, NullTape, S3PutRec, TapeSink};

const S1: &str = "0198aaaa-0000-7000-8000-000000000001";
const S2: &str = "0198bbbb-0000-7000-8000-000000000002";

// ---------------------------------------------------------------------------
// harness

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-wp3-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `seg(root, session, startTs, seq)`: a closed telemetry segment.
fn seg(root: &Path, session: &str, start_ts: u64, seq: u32) -> PathBuf {
    let dir = root.join(session);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("seg-{start_ts}-{seq:05}.jsonl.gz"));
    std::fs::write(&path, format!("payload-{session}-{seq}")).unwrap();
    path
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PutCall {
    key: String,
    body: String,
    ct: String,
}

type PutHook = Arc<
    dyn Fn(PutCall, CancellationToken) -> Pin<Box<dyn Future<Output = Result<(), PutError>> + Send>>
        + Send
        + Sync,
>;

struct FakePut(PutHook);

impl ObjectPut for FakePut {
    fn put<'a>(
        &'a self,
        key: &'a str,
        body: Bytes,
        content_type: &'a str,
        cancel: &'a CancellationToken,
    ) -> PutFuture<'a> {
        (self.0)(
            PutCall {
                key: key.to_string(),
                body: String::from_utf8_lossy(&body).into_owned(),
                ct: content_type.to_string(),
            },
            cancel.clone(),
        )
    }
}

type Events = Arc<Mutex<Vec<(ActivityLevel, String)>>>;

struct Rig {
    up: Arc<Uploader>,
    events: Events,
    statuses: Arc<Mutex<Vec<bool>>>,
    uploaded: Arc<Mutex<Vec<(String, String)>>>,
}

fn rig(
    sources: Vec<Arc<dyn SegmentSource>>,
    put: PutHook,
    online: Arc<AtomicBool>,
    mode: Arc<Mutex<UploadMode>>,
    backoff: Option<(u64, u64)>,
) -> Rig {
    rig_with(
        sources,
        put,
        online,
        mode,
        backoff,
        None,
        PathBuf::from("/"),
    )
}

fn rig_with(
    sources: Vec<Arc<dyn SegmentSource>>,
    put: PutHook,
    online: Arc<AtomicBool>,
    mode: Arc<Mutex<UploadMode>>,
    backoff: Option<(u64, u64)>,
    tape: Option<Arc<dyn TapeSink>>,
    home_of_tape: PathBuf,
) -> Rig {
    let events: Events = Arc::default();
    let statuses: Arc<Mutex<Vec<bool>>> = Arc::default();
    let uploaded: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let (e, s, u) = (events.clone(), statuses.clone(), uploaded.clone());
    let up = Uploader::new(UploaderOpts {
        sources,
        put: Arc::new(FakePut(put)),
        is_online: Arc::new(move || online.load(Ordering::SeqCst)),
        mode: Arc::new(move || *mode.lock().unwrap()),
        on_status: Some(Arc::new(move |st| s.lock().unwrap().push(st.uploading))),
        on_uploaded: Some(Arc::new(move |seg: &UploadSegment, key: &str| {
            u.lock().unwrap().push((seg.name.clone(), key.to_string()))
        })),
        on_event: Some(Arc::new(move |l, m: &str| {
            e.lock().unwrap().push((l, m.to_string()))
        })),
        backoff_min_ms: backoff.map(|b| b.0),
        backoff_max_ms: backoff.map(|b| b.1),
        clock: Arc::new(AgentClock::following_tokio(1_750_000_000_000)),
        tape: tape.unwrap_or_else(|| Arc::new(NullTape)),
        tape_home: Some(home_of_tape),
    });
    Rig {
        up: Arc::new(up),
        events,
        statuses,
        uploaded,
    }
}

/// A put hook that records every call and succeeds.
fn recording_put(calls: Arc<Mutex<Vec<PutCall>>>) -> PutHook {
    Arc::new(move |c, _| {
        calls.lock().unwrap().push(c);
        Box::pin(async { Ok(()) })
    })
}

fn on() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(true))
}

fn auto() -> Arc<Mutex<UploadMode>> {
    Arc::new(Mutex::new(UploadMode::Auto))
}

/// Let spawned tasks run, then await the drain (`await up.settled()`).
async fn settle(up: &Uploader) {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    up.settled().await;
}

fn keys(calls: &Arc<Mutex<Vec<PutCall>>>) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.key.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// recording-uploader.test.ts › telemetry segment source

#[test]
fn telemetry_derives_the_s3_key_from_the_segment_name_seg_prefix_stripped() {
    let home = scratch("t1");
    let root = home.join("recordings/telemetry");
    let path = seg(&root, S1, 1_753_000_000_000, 3);
    let src = telemetry_source("dev-1", &root);
    let listed = src.list();
    assert_eq!(
        listed.iter().map(|s| s.path.clone()).collect::<Vec<_>>(),
        [path]
    );
    assert_eq!(
        src.s3_key_for(&listed[0]).as_deref(),
        Some(
            "telemetry/v1/dev-1/0198aaaa-0000-7000-8000-000000000001/1753000000000-00003.jsonl.gz"
        )
    );
}

#[test]
fn telemetry_lists_sessions_and_segments_oldest_first_ignoring_active_jsonl() {
    let home = scratch("t2");
    let root = home.join("recordings/telemetry");
    seg(&root, S2, 2000, 0);
    seg(&root, S1, 1000, 1);
    seg(&root, S1, 1000, 0);
    std::fs::write(root.join(S1).join("seg-999-00009.jsonl"), "active").unwrap();
    let names: Vec<String> = telemetry_source("d", &root)
        .list()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(
        names,
        [
            format!("{S1}/seg-1000-00000.jsonl.gz"),
            format!("{S1}/seg-1000-00001.jsonl.gz"),
            format!("{S2}/seg-2000-00000.jsonl.gz"),
        ]
    );
}

#[test]
fn telemetry_returns_null_keys_for_unrecognized_names() {
    let home = scratch("t3");
    let src = telemetry_source("d", &home.join("none"));
    let s = |name: &str| UploadSegment {
        path: "/x".into(),
        name: name.into(),
        sidecar: None,
    };
    assert_eq!(src.s3_key_for(&s("sess/whatever.jsonl.gz")), None);
    assert_eq!(src.s3_key_for(&s("no-slash")), None);
    assert!(src.list().is_empty());
}

// ---------------------------------------------------------------------------
// recording-uploader.test.ts › Uploader

#[tokio::test(start_paused = true)]
async fn drains_oldest_first_and_deletes_each_file_only_after_a_successful_put() {
    let home = scratch("u1");
    let root = home.join("recordings/telemetry");
    let p1 = seg(&root, S1, 1000, 0);
    let p2 = seg(&root, S1, 1000, 1);
    let calls = Arc::new(Mutex::new(vec![]));
    let r = rig(
        vec![telemetry_source("d", &root)],
        recording_put(calls.clone()),
        on(),
        auto(),
        Some((20, 100)),
    );
    r.up.kick();
    settle(&r.up).await;
    assert_eq!(
        keys(&calls),
        [
            format!("telemetry/v1/d/{S1}/1000-00000.jsonl.gz"),
            format!("telemetry/v1/d/{S1}/1000-00001.jsonl.gz"),
        ]
    );
    assert!(!p1.exists());
    assert!(!p2.exists());
    assert!(!r.up.status().uploading);
    assert!(r.up.status().last_upload_at.is_some());
}

/// The TS test's fake source: fixed files, `s3KeyFor` = name, and uploaded
/// entries dropped on the next key lookup (simulated delete-on-success).
struct FakeSource {
    kind: String,
    priority: u8,
    files: Mutex<Vec<UploadSegment>>,
}

impl SegmentSource for FakeSource {
    fn kind(&self) -> &str {
        &self.kind
    }
    fn priority(&self) -> u8 {
        self.priority
    }
    fn content_type(&self) -> &str {
        "application/octet-stream"
    }
    fn list(&self) -> Vec<UploadSegment> {
        self.files.lock().unwrap().clone()
    }
    fn s3_key_for(&self, seg: &UploadSegment) -> Option<String> {
        self.files.lock().unwrap().retain(|f| f.path.exists());
        Some(seg.name.clone())
    }
}

#[tokio::test(start_paused = true)]
async fn orders_sources_by_priority_telemetry_before_video() {
    let home = scratch("u2");
    let fake = |kind: &str, priority: u8, n: usize| -> Arc<dyn SegmentSource> {
        let files = (0..n)
            .map(|i| {
                let path = home.join(format!("{kind}-{i}"));
                std::fs::write(&path, kind).unwrap();
                UploadSegment {
                    path,
                    name: format!("{kind}-{i}"),
                    sidecar: None,
                }
            })
            .collect();
        Arc::new(FakeSource {
            kind: kind.into(),
            priority,
            files: Mutex::new(files),
        })
    };
    let video = fake("video", 1, 1);
    let telem = fake("telemetry", 0, 2);
    let calls = Arc::new(Mutex::new(vec![]));
    let r = rig(
        vec![video, telem],
        recording_put(calls.clone()),
        on(),
        auto(),
        Some((20, 100)),
    );
    r.up.kick();
    settle(&r.up).await;
    assert_eq!(keys(&calls), ["telemetry-0", "telemetry-1", "video-0"]);
}

#[tokio::test(start_paused = true)]
async fn does_not_run_while_offline_or_when_upload_mode_is_off() {
    let home = scratch("u3");
    let root = home.join("recordings/telemetry");
    seg(&root, S1, 1000, 0);
    let online = Arc::new(AtomicBool::new(false));
    let mode = auto();
    let calls = Arc::new(Mutex::new(vec![]));
    let r = rig(
        vec![telemetry_source("d", &root)],
        recording_put(calls.clone()),
        online.clone(),
        mode.clone(),
        Some((20, 100)),
    );

    r.up.kick(); // offline — no-op
    settle(&r.up).await;
    assert_eq!(calls.lock().unwrap().len(), 0);

    online.store(true, Ordering::SeqCst);
    *mode.lock().unwrap() = UploadMode::Off;
    r.up.kick(); // mode off — no-op
    settle(&r.up).await;
    assert_eq!(calls.lock().unwrap().len(), 0);

    *mode.lock().unwrap() = UploadMode::Auto;
    r.up.kick();
    settle(&r.up).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn backs_off_on_failure_and_retries_until_success() {
    let home = scratch("u4");
    let root = home.join("recordings/telemetry");
    let p = seg(&root, S1, 1000, 0);
    let calls = Arc::new(Mutex::new(vec![]));
    let failures = Arc::new(Mutex::new(2));
    let c = calls.clone();
    let put: PutHook = Arc::new(move |call, _| {
        let mut f = failures.lock().unwrap();
        let fail = *f > 0;
        *f -= 1;
        if !fail {
            c.lock().unwrap().push(call);
        }
        Box::pin(async move {
            if fail {
                Err(PutError("boom".into()))
            } else {
                Ok(())
            }
        })
    });
    let r = rig(
        vec![telemetry_source("d", &root)],
        put,
        on(),
        auto(),
        Some((20, 100)),
    );
    r.up.kick();
    tokio::time::sleep(Duration::from_millis(300)).await; // 2 backoff rounds at 20/40 ms + the success
    settle(&r.up).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(!p.exists());
    let retries = r
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, m)| m.contains("retrying"))
        .count();
    assert!(retries >= 2, "{retries}");
}

#[tokio::test(start_paused = true)]
async fn pause_aborts_the_in_flight_put_and_a_later_kick_resumes() {
    let home = scratch("u5");
    let root = home.join("recordings/telemetry");
    let p = seg(&root, S1, 1000, 0);
    let online = on();
    let aborted_seen = Arc::new(AtomicBool::new(false));
    let block = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(Mutex::new(vec![]));
    let (a, b, c) = (aborted_seen.clone(), block.clone(), calls.clone());
    let put: PutHook = Arc::new(move |call, cancel| {
        let (a, b, c) = (a.clone(), b.clone(), c.clone());
        Box::pin(async move {
            if b.load(Ordering::SeqCst) {
                cancel.cancelled().await;
                a.store(true, Ordering::SeqCst);
                return Err(PutError("aborted".into()));
            }
            c.lock().unwrap().push(call);
            Ok(())
        })
    });
    let r = rig(
        vec![telemetry_source("d", &root)],
        put,
        online.clone(),
        auto(),
        Some((20, 100)),
    );
    r.up.kick();
    tokio::time::sleep(Duration::from_millis(30)).await; // let the PUT start and block
    assert!(r.up.status().uploading);
    online.store(false, Ordering::SeqCst);
    r.up.pause();
    settle(&r.up).await;
    assert!(aborted_seen.load(Ordering::SeqCst));
    assert!(p.exists()); // not deleted — PUT never confirmed
    assert_eq!(calls.lock().unwrap().len(), 0);
    assert_eq!(
        r.events.lock().unwrap().last().cloned(),
        Some((ActivityLevel::Info, "upload paused (offline)".to_string()))
    );

    block.store(false, Ordering::SeqCst);
    online.store(true, Ordering::SeqCst);
    r.up.kick();
    settle(&r.up).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(!p.exists());
}

#[tokio::test(start_paused = true)]
async fn skips_unparseable_segment_names_without_blocking_the_queue() {
    let home = scratch("u6");
    let root = home.join("recordings/telemetry");
    let dir = root.join(S1);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("weird.jsonl.gz"), "x").unwrap();
    seg(&root, S1, 1000, 0);
    let calls = Arc::new(Mutex::new(vec![]));
    let r = rig(
        vec![telemetry_source("d", &root)],
        recording_put(calls.clone()),
        on(),
        auto(),
        Some((20, 100)),
    );
    r.up.kick();
    settle(&r.up).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(r
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|(_, m)| m.contains("unrecognized")));
    // The weird file is left alone (never deleted, never uploaded).
    assert!(dir.join("weird.jsonl.gz").exists());
}

// ---------------------------------------------------------------------------
// recording-video-source.test.ts

fn set_mtime(path: &Path, ms: f64) {
    let t = if ms >= 0.0 {
        UNIX_EPOCH + Duration::from_secs_f64(ms / 1000.0)
    } else {
        UNIX_EPOCH - Duration::from_secs_f64(-ms / 1000.0)
    };
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

/// `frag(streamKey, session, seq, { sidecar, mtimeMs })`.
fn frag(
    root: &Path,
    stream: &str,
    session: &str,
    seq: u32,
    sidecar: bool,
    mtime_ms: Option<f64>,
) -> PathBuf {
    let dir = root.join(stream).join(session);
    std::fs::create_dir_all(&dir).unwrap();
    let name = format!("{seq:05}.mp4");
    let path = dir.join(&name);
    std::fs::write(&path, format!("mp4-{stream}-{seq}")).unwrap();
    if sidecar {
        std::fs::write(
            dir.join(name.replace(".mp4", ".json")),
            format!("{{\"seq\":{seq}}}"),
        )
        .unwrap();
    }
    if let Some(ms) = mtime_ms {
        set_mtime(&path, ms);
    }
    path
}

fn manifest(root: &Path, stream: &str, session: &str, mtime_ms: Option<f64>) -> PathBuf {
    let dir = root.join(stream).join(session);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");
    std::fs::write(&path, format!("{{\"v\":1,\"sessionId\":\"{session}\"}}")).unwrap();
    if let Some(ms) = mtime_ms {
        set_mtime(&path, ms);
    }
    path
}

/// `src(active, now)`.
fn vsrc(root: &Path, active: &[&str], now: Option<f64>) -> Arc<dyn SegmentSource> {
    let active: HashSet<String> = active.iter().map(|s| s.to_string()).collect();
    video_source(VideoSourceOpts {
        device_id: "dev-1".into(),
        video_root: root.to_path_buf(),
        active_session_ids: Arc::new(move || active.clone()),
        now_ms: now.map(|n| Arc::new(move || n) as Arc<dyn Fn() -> f64 + Send + Sync>),
    })
}

fn vroot(tag: &str) -> PathBuf {
    scratch(tag).join("recordings/video")
}

#[test]
fn video_lists_only_sidecard_pairs_with_s3_keys_and_sidecar_descriptors() {
    let root = vroot("v1");
    manifest(&root, "cam0", S1, None);
    let closed = frag(&root, "cam0", S1, 0, true, None);
    frag(&root, "cam0", S1, 1, false, None); // active fragment — not listed
    let listed = vsrc(&root, &[S1], None).list();
    assert_eq!(
        listed.iter().map(|s| s.path.clone()).collect::<Vec<_>>(),
        std::slice::from_ref(&closed)
    );
    assert_eq!(listed[0].name, format!("cam0/{S1}/00000.mp4"));
    assert_eq!(
        vsrc(&root, &[], None).s3_key_for(&listed[0]).as_deref(),
        Some(format!("media-sessions/v1/dev-1/cam0/{S1}/00000.mp4").as_str())
    );
    assert_eq!(
        listed[0].sidecar,
        Some(SidecarRef {
            path: closed.with_extension("json"),
            s3_key: format!("media-sessions/v1/dev-1/cam0/{S1}/00000.json"),
            content_type: "application/json".into(),
        })
    );
}

#[test]
fn video_orders_chronologically_across_streams_by_mtime_not_by_name() {
    let root = vroot("v2");
    manifest(&root, "cam0", S1, None);
    manifest(&root, "cam1", S2, None);
    frag(&root, "cam0", S1, 0, true, Some(1000.0));
    frag(&root, "cam1", S2, 0, true, Some(2000.0));
    frag(&root, "cam0", S1, 1, true, Some(3000.0));
    let names: Vec<String> = vsrc(&root, &[S1, S2], None)
        .list()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(
        names,
        [
            format!("cam0/{S1}/00000.mp4"),
            format!("cam1/{S2}/00000.mp4"),
            format!("cam0/{S1}/00001.mp4"),
        ]
    );
}

#[test]
fn video_returns_null_keys_for_unparseable_names_and_skips_bad_path_segments() {
    let root = vroot("v3");
    let s = vsrc(&root, &[], None);
    let k = |name: &str| {
        s.s3_key_for(&UploadSegment {
            path: "/x".into(),
            name: name.into(),
            sidecar: None,
        })
    };
    assert_eq!(k("cam0/not-a-uuid/00000.mp4"), None);
    assert_eq!(k(&format!("cam0/{S1}/evil.mp4")), None);
    assert_eq!(k("no-slashes"), None);
    assert_eq!(k(&format!("bad key/{S1}/00000.mp4")), None);
}

#[test]
fn video_reaps_orphan_sidecars_mp4_already_uploaded_but_never_session_json() {
    let root = vroot("v4");
    manifest(&root, "cam0", S1, None);
    let dir = root.join("cam0").join(S1);
    std::fs::write(dir.join("00000.json"), "{}").unwrap(); // pairless sidecar
    frag(&root, "cam0", S1, 1, true, None);
    vsrc(&root, &[S1], None).list();
    assert!(!dir.join("00000.json").exists());
    assert!(dir.join("session.json").exists());
    assert!(dir.join("00001.mp4").exists());
}

#[test]
fn video_reaps_a_drained_inactive_old_session_dir_manifest_included() {
    let root = vroot("v5");
    let path = manifest(&root, "cam0", S1, Some(1_000.0));
    let dir = root.join("cam0").join(S1);
    assert!(vsrc(&root, &[], Some(10.0 * 60_000.0 + 2_000.0))
        .list()
        .is_empty());
    assert!(!path.exists());
    assert!(!dir.exists());
}

#[test]
fn video_reaps_a_drained_session_whose_manifest_mtime_is_in_the_future() {
    // The 2026-07-29 NITZ excursion stamped four session dirs "Jan 5 2080".
    let root = vroot("v6");
    let future = xorgate_core::js::date_utc(2080.0, 0.0, 5.0, 0.0, 0.0, 0.0);
    let now = xorgate_core::js::date_utc(2026.0, 6.0, 30.0, 0.0, 0.0, 0.0);
    let path = manifest(&root, "cam0", S1, Some(future));
    vsrc(&root, &[], Some(now)).list();
    assert!(!path.exists());
    assert!(!root.join("cam0").join(S1).exists());
}

#[test]
fn video_does_not_reap_empty_sessions_that_are_active_or_too_young() {
    let root = vroot("v7");
    manifest(&root, "cam0", S1, Some(1_000.0)); // old but active
    manifest(&root, "cam0", S2, Some(5_000.0)); // inactive but young
    vsrc(&root, &[S1], Some(10.0 * 60_000.0 + 2_000.0)).list();
    assert!(root.join("cam0").join(S1).join("session.json").exists());
    assert!(root.join("cam0").join(S2).join("session.json").exists());
}

// recording-video-source.test.ts › uploader with video pairs

fn fail_once_put(
    calls: Arc<Mutex<Vec<PutCall>>>,
    fail_next: Arc<Mutex<Option<String>>>,
) -> PutHook {
    Arc::new(move |call, _| {
        let mut f = fail_next.lock().unwrap();
        if f.as_ref().is_some_and(|s| call.key.ends_with(s.as_str())) {
            *f = None;
            return Box::pin(async { Err(PutError("boom".into())) });
        }
        calls.lock().unwrap().push(call);
        Box::pin(async { Ok(()) })
    })
}

#[tokio::test(start_paused = true)]
async fn puts_sidecar_before_mp4_and_deletes_both_only_on_mp4_success() {
    let root = vroot("vu1");
    manifest(&root, "cam0", S1, None);
    let mp4 = frag(&root, "cam0", S1, 0, true, None);
    let calls = Arc::new(Mutex::new(vec![]));
    let r = rig(
        vec![vsrc(&root, &[], None)],
        fail_once_put(calls.clone(), Arc::default()),
        on(),
        auto(),
        Some((5, 10)),
    );
    r.up.kick();
    settle(&r.up).await;
    assert_eq!(
        keys(&calls),
        [
            format!("media-sessions/v1/dev-1/cam0/{S1}/00000.json"),
            format!("media-sessions/v1/dev-1/cam0/{S1}/00000.mp4"),
        ]
    );
    assert_eq!(calls.lock().unwrap()[0].ct, "application/json");
    assert_eq!(calls.lock().unwrap()[1].ct, "video/mp4");
    assert!(!mp4.exists());
    assert!(!mp4.with_extension("json").exists());
}

#[tokio::test(start_paused = true)]
async fn keeps_both_files_and_re_puts_the_sidecar_when_the_mp4_put_fails() {
    let root = vroot("vu2");
    manifest(&root, "cam0", S1, None);
    let mp4 = frag(&root, "cam0", S1, 0, true, None);
    let calls = Arc::new(Mutex::new(vec![]));
    let fail = Arc::new(Mutex::new(Some("00000.mp4".to_string()))); // sidecar succeeds, mp4 fails once
    let r = rig(
        vec![vsrc(&root, &[], None)],
        fail_once_put(calls.clone(), fail),
        on(),
        auto(),
        Some((5, 10)),
    );
    r.up.kick();
    settle(&r.up).await;
    assert!(mp4.exists());
    assert!(mp4.with_extension("json").exists());
    // Backoff retry re-PUTs the sidecar too, then the mp4.
    tokio::time::sleep(Duration::from_millis(50)).await;
    settle(&r.up).await;
    let tails: Vec<String> = keys(&calls)
        .iter()
        .map(|k| k.rsplit('/').next().unwrap().to_string())
        .collect();
    assert_eq!(tails, ["00000.json", "00000.json", "00000.mp4"]);
    assert!(!mp4.exists());
}

// ---------------------------------------------------------------------------
// fixtures/upload_step/uploader-drain.json — the real TS Uploader, replayed

#[tokio::test(start_paused = true)]
async fn uploader_drain_fixture_replays_the_ts_runtime() {
    let text = std::fs::read_to_string(format!(
        "{}/../../fixtures/upload_step/uploader-drain.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let f: Value = serde_json::from_str(&text).unwrap();
    assert!(
        [
            "09da38dcc8b75c248625a3a0bee43b70e7def55d",
            "89b5c6407ef88e20f7168bd767d22a7be04ebcb6"
        ]
        .contains(&f["jsCommit"].as_str().unwrap()),
        "jsCommit {} (CF78)",
        f["jsCommit"]
    );
    let session = f["sessionId"].as_str().unwrap();
    let cases = f["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 13);
    for (n, c) in cases.iter().enumerate() {
        let name = c["name"].as_str().unwrap();
        let input = &c["input"];
        let want = &c["expected"];
        let home = scratch(&format!("fx{n}"));
        let root = home.join("recordings/telemetry");
        let dir = root.join(session);
        std::fs::create_dir_all(&dir).unwrap();
        for s in input["segments"].as_array().unwrap() {
            let s = s.as_str().unwrap();
            std::fs::write(dir.join(s), format!("payload-{s}")).unwrap();
        }
        let script: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new(
            input["script"]
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .collect(),
        ));
        let online = on();
        let mode = auto();
        let failures = Arc::new(Mutex::new(0u64));
        let up_cell: Arc<OnceLock<Arc<Uploader>>> = Arc::new(OnceLock::new());
        let start = tokio::time::Instant::now();
        let puts: Arc<Mutex<Vec<(Value, u64)>>> = Arc::default();
        let (sc, on2, mo2, fl, cell, pu) = (
            script.clone(),
            online.clone(),
            mode.clone(),
            failures.clone(),
            up_cell.clone(),
            puts.clone(),
        );
        let put: PutHook = Arc::new(move |call, _| {
            let o = sc.lock().unwrap().pop_front().unwrap_or(Value::from("ok"));
            let outcome = match &o {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap(),
            };
            pu.lock().unwrap().push((
                serde_json::json!({ "key": call.key, "contentType": call.ct, "outcome": outcome }),
                start.elapsed().as_millis() as u64,
            ));
            let res = if o == "ok" {
                Ok(())
            } else {
                *fl.lock().unwrap() += 1;
                let (kind, msg) = o.as_object().unwrap().iter().next().unwrap();
                match kind.as_str() {
                    "failOffline" => on2.store(false, Ordering::SeqCst),
                    "failStop" => cell.get().unwrap().stop(),
                    "failModeOff" => *mo2.lock().unwrap() = UploadMode::Off,
                    _ => {}
                }
                Err(PutError(msg.as_str().unwrap().to_string()))
            };
            Box::pin(async move { res })
        });
        let backoff = input["backoffMinMs"]
            .as_u64()
            .map(|min| (min, input["backoffMaxMs"].as_u64().unwrap()));
        let r = rig(
            vec![telemetry_source("d", &root)],
            put,
            online.clone(),
            mode.clone(),
            backoff,
        );
        let _ = up_cell.set(r.up.clone());

        // The TS driver's loop: kick + settle, then fire each armed timer in
        // turn (here: sleep exactly its delay — a shorter/longer Rust delay
        // shows up as a PUT at the wrong instant), or pause()+kick() instead
        // when the scenario says so.
        r.up.kick();
        settle(&r.up).await;
        let pause_after = input["pauseAfterFailures"].as_u64();
        let mut paused = false;
        let mut starts = vec![0u64];
        for t in want["timers"].as_array().unwrap() {
            if pause_after == Some(*failures.lock().unwrap()) && !paused {
                assert_eq!(t["state"], "cleared", "{name}");
                paused = true;
                r.up.pause();
                r.up.kick();
                starts.push(start.elapsed().as_millis() as u64);
                settle(&r.up).await;
                continue;
            }
            assert_eq!(t["state"], "fired", "{name}");
            tokio::time::sleep(Duration::from_millis(t["ms"].as_u64().unwrap())).await;
            starts.push(start.elapsed().as_millis() as u64);
            settle(&r.up).await;
        }
        tokio::time::sleep(Duration::from_secs(3600)).await; // nothing else may happen
        settle(&r.up).await;

        // events
        let lvl = |l: &ActivityLevel| match l {
            ActivityLevel::Info => "info",
            ActivityLevel::Warn => "warn",
            ActivityLevel::Error => "error",
        };
        let got_events: Vec<Value> = r
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|(l, m)| serde_json::json!({ "level": lvl(l), "msg": m }))
            .collect();
        assert_eq!(Value::from(got_events), want["events"], "{name}: events");
        // PUTs (key, content type, outcome) and their instants: each drain's
        // PUTs happen at its start; a failed PUT ends the drain.
        let puts = puts.lock().unwrap().clone();
        let got_puts: Vec<Value> = puts.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(Value::from(got_puts), want["puts"], "{name}: puts");
        let mut drain = 0;
        for (p, at) in &puts {
            assert_eq!(*at, starts[drain], "{name}: PUT {p} at {at} ms");
            if p["outcome"] != "ok" {
                drain += 1;
            }
        }
        // status edges, onUploaded, files left, lastUploadAt
        let st: Vec<Value> = r
            .statuses
            .lock()
            .unwrap()
            .iter()
            .map(|b| Value::from(*b))
            .collect();
        assert_eq!(Value::from(st), want["statuses"], "{name}: statuses");
        let up: Vec<Value> = r
            .uploaded
            .lock()
            .unwrap()
            .iter()
            .map(|(n, k)| serde_json::json!({ "name": n, "key": k }))
            .collect();
        assert_eq!(Value::from(up), want["uploaded"], "{name}: uploaded");
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(serde_json::json!(left), want["left"], "{name}: left");
        assert_eq!(
            Value::from(r.up.status().last_upload_at.is_some()),
            want["lastUploadAtSet"],
            "{name}"
        );
        r.up.stop();
    }
}

// ---------------------------------------------------------------------------
// tape hooks (phase-3 §Record/replay: `s3_put` per settled PUT, `fs_out`
// delete per local file removed after its unit)

#[derive(Default)]
struct RecTape {
    puts: Mutex<Vec<S3PutRec>>,
    fs: Mutex<Vec<(String, FsOp, u64)>>,
}

impl TapeSink for RecTape {
    fn enabled(&self) -> bool {
        true
    }
    fn s3_put(&self, r: &S3PutRec) {
        self.puts.lock().unwrap().push(r.clone());
    }
    fn fs_out(&self, rel: &str, op: FsOp, size: u64) {
        self.fs.lock().unwrap().push((rel.to_string(), op, size));
    }
}

#[tokio::test(start_paused = true)]
async fn tape_records_each_put_in_order_and_the_deletes_after_the_unit() {
    let home = scratch("tape");
    let root = home.join("recordings/video");
    manifest(&root, "cam0", S1, None);
    let mp4 = frag(&root, "cam0", S1, 0, true, None);
    let tape = Arc::new(RecTape::default());
    let fail = Arc::new(Mutex::new(Some("00000.mp4".to_string())));
    let calls = Arc::new(Mutex::new(vec![]));
    let r = rig_with(
        vec![vsrc(&root, &[], None)],
        fail_once_put(calls.clone(), fail),
        on(),
        auto(),
        Some((5, 10)),
        Some(tape.clone()),
        home.clone(),
    );
    r.up.kick();
    settle(&r.up).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    settle(&r.up).await;
    assert!(!mp4.exists());
    let puts = tape.puts.lock().unwrap().clone();
    let k = |t: &str| format!("media-sessions/v1/dev-1/cam0/{S1}/00000.{t}");
    type Row = (String, String, u64, Option<u16>, Option<String>);
    let summary: Vec<Row> = puts
        .iter()
        .map(|p| {
            (
                p.key.clone(),
                p.content_type.clone(),
                p.size,
                p.status,
                p.error.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (k("json"), "application/json".into(), 9, Some(200), None),
            (k("mp4"), "video/mp4".into(), 10, None, Some("boom".into())),
            (k("json"), "application/json".into(), 9, Some(200), None),
            (k("mp4"), "video/mp4".into(), 10, Some(200), None),
        ]
    );
    assert_eq!(
        puts[3].sha256,
        xorgate_cloud::sigv4::sha256_hex(b"mp4-cam0-0")
    );
    let rel = |t: &str| format!("recordings/video/cam0/{S1}/00000.{t}");
    assert_eq!(
        *tape.fs.lock().unwrap(),
        [
            (rel("mp4"), FsOp::Delete, 10),
            (rel("json"), FsOp::Delete, 9)
        ]
    );
}
