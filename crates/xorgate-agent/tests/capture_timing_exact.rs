//! Phase 7a WP2: the supervisor with `XORGATE_SIDECAR_TIMING=exact` — the
//! host's `XG-FRAGMENT*` stdout lines (WP1's `capture_timing` format) feed
//! the run's table, every line triggers a sweep, deferral is bounded by one
//! poll period (a one-shot timer), the final sweep never defers, a respawn
//! starts a fresh table, and `legacy` ignores the lines entirely.
//!
//! Paused tokio clock, a fake `CaptureBackend` whose children print what the
//! test sends; the test writes the fragments splitmuxsink would write and
//! pins their mtimes (the crash-cut / no-successor rule reads mtime).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::mpsc;
use xorgate_agent::ctx::AgentClock;
use xorgate_agent::recording::capture::*;
use xorgate_core::capture_timing::{FragmentEnd, FragmentStart, VERSION};
use xorgate_core::types::TimeSource;
use xorgate_core::video_presets::resolve_video_preset;
use xorgate_hal::child::{ChildDone, ChildEvent};

const T0: u64 = 1_760_000_000_000;
const LAG: u64 = 2_100;
const SEG: u64 = 60_000;
const POLL: u64 = 10_000;

struct Entry {
    tx: mpsc::UnboundedSender<ChildEvent>,
    exited: AtomicBool,
}

impl Entry {
    fn line(&self, l: String) {
        let _ = self.tx.send(ChildEvent::Stdout(l));
    }
    fn exit(&self, code: i32) {
        if !self.exited.swap(true, Ordering::SeqCst) {
            let _ = self.tx.send(ChildEvent::Done(ChildDone {
                exit_code: Some(code),
                failed: code != 0,
                ..ChildDone::default()
            }));
        }
    }
}

struct FakeChild(Arc<Entry>);

impl CaptureChild for FakeChild {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn signal(&self, _sig: i32) {
        self.0.exit(0);
    }
    fn renice(&self, _nice: i32) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FakeHost {
    spawns: Mutex<Vec<Arc<Entry>>>,
}

impl FakeHost {
    fn last(&self) -> Arc<Entry> {
        self.spawns.lock().unwrap().last().unwrap().clone()
    }
    fn len(&self) -> usize {
        self.spawns.lock().unwrap().len()
    }
}

impl CaptureBackend for FakeHost {
    fn program(&self) -> &str {
        "xorgate-capture"
    }
    fn spawn(
        &self,
        _args: &[String],
    ) -> (Box<dyn CaptureChild>, mpsc::UnboundedReceiver<ChildEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let e = Arc::new(Entry {
            tx,
            exited: AtomicBool::new(false),
        });
        self.spawns.lock().unwrap().push(e.clone());
        (Box::new(FakeChild(e)), rx)
    }
    fn emits_timing(&self) -> bool {
        true
    }
}

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "xg-timing-{tag}-{}-{}",
            std::process::id(),
            xorgate_hal::fragment_stat::hrtime_ns()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sup(home: &Home, be: Arc<FakeHost>, timing: SidecarTiming) -> VideoCaptureSupervisor {
    sup_with(home, be, timing, |_| {})
}

fn sup_with(
    home: &Home,
    be: Arc<FakeHost>,
    timing: SidecarTiming,
    tweak: impl FnOnce(&mut VideoCaptureOpts),
) -> VideoCaptureSupervisor {
    let mut o = VideoCaptureOpts::new(
        "dev-1",
        vec![CaptureCamera {
            stream_key: "cam0".into(),
            camera_name: "/base/cam0@1a".into(),
        }],
        true,
        resolve_video_preset("720p15-1mbps").clone(),
        60,
        home.0.join("video"),
        home.0.join("shm"),
        "boot-1",
        Arc::new(|| TimeSource::Ntp),
        Arc::new(AgentClock::following_tokio(T0 as i64)),
    );
    o.backend = be;
    o.fs_watch = false;
    o.sweep_poll_ms = POLL;
    o.stable_ms = 50;
    o.shm_segment_dir = home.0.join("devshm");
    o.sidecar_timing = timing;
    tweak(&mut o);
    VideoCaptureSupervisor::new(o)
}

fn dir(home: &Home, s: &VideoCaptureSupervisor) -> PathBuf {
    let sid = s.status()[0].session_id.clone().expect("session");
    home.0.join("video").join("cam0").join(sid)
}

/// Frame wall of fragment k, the callback wall (= frame + lag).
fn frame(k: u64) -> u64 {
    T0 + 300 + k * SEG
}
fn cb(k: u64) -> u64 {
    frame(k) + LAG
}

fn start_line(k: u64, lag: u64) -> String {
    FragmentStart {
        v: VERSION,
        id: k as u32,
        frame_mono_ns: 1_000_000_000_000 + k * SEG * 1_000_000,
        cb_mono_ns: 1_000_000_000_000 + k * SEG * 1_000_000 + lag * 1_000_000,
        cb_wall_ms: frame(k) + lag,
        lag_ms: lag,
        start_ts: frame(k),
    }
    .to_line()
}

fn end_line(k: u64, end_ts: u64) -> String {
    FragmentEnd {
        v: VERSION,
        id: k as u32,
        end_mono_ns: 1,
        cb_mono_ns: 2,
        cb_wall_ms: end_ts + 40,
        end_ts,
    }
    .to_line()
}

/// Write fragment k with its mtime pinned (epoch ms).
fn frag(d: &Path, k: u64, mtime_ms: u64) {
    let p = d.join(format!("{k:05}.mp4"));
    std::fs::write(&p, vec![7u8; 100 + k as usize]).unwrap();
    let f = std::fs::File::options().write(true).open(&p).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_millis(mtime_ms))
        .unwrap();
}

fn sidecar(d: &Path, k: u64) -> Option<Value> {
    std::fs::read(d.join(format!("{k:05}.json")))
        .ok()
        .map(|b| serde_json::from_slice(&b).unwrap())
}

async fn ms(n: u64) {
    tokio::time::sleep(Duration::from_millis(n)).await;
}

/// Let the consume tasks drain what was sent (no virtual time passes).
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn assert_exact(sc: &Value, start: u64, dur: u64, finalized: bool) {
    assert_eq!(sc["anchor"], "pts", "{sc}");
    assert_eq!(sc["startTs"], start, "{sc}");
    assert_eq!(sc["durationMs"], dur, "{sc}");
    assert_eq!(sc["finalized"], finalized, "{sc}");
}

#[tokio::test(start_paused = true)]
async fn lines_before_files_give_exact_sidecars_on_the_poll() {
    let home = Home::new("inorder");
    let be = Arc::new(FakeHost::default());
    let s = sup(&home, be.clone(), SidecarTiming::Exact);
    s.start();
    let d = dir(&home, &s);
    let c = be.last();
    for k in 0..3 {
        c.line(start_line(k, LAG));
        frag(&d, k, cb(k + 1));
    }
    settle().await;
    ms(POLL + 1).await;
    assert_exact(&sidecar(&d, 0).unwrap(), frame(0), SEG, true);
    assert_exact(&sidecar(&d, 1).unwrap(), frame(1), SEG, true);
    assert!(sidecar(&d, 2).is_none(), "the active fragment");
    // Fragment 0 is exact: no boundary walk-back, startTs = its first frame
    // (the legacy anchor would be ~2.1 s later).
    s.stop();
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn a_line_after_its_file_defers_n_and_the_line_itself_sweeps() {
    let home = Home::new("ooo");
    let be = Arc::new(FakeHost::default());
    let s = sup(&home, be.clone(), SidecarTiming::Exact);
    s.start();
    let d = dir(&home, &s);
    let c = be.last();
    c.line(start_line(0, LAG));
    c.line(start_line(1, LAG));
    settle().await;
    for k in 0..3 {
        frag(&d, k, cb(k + 1));
    }
    ms(POLL + 1).await; // the poll sees file 2 before line 2
    assert_exact(&sidecar(&d, 0).unwrap(), frame(0), SEG, true);
    assert!(sidecar(&d, 1).is_none(), "fragment 1 waits for stamp 2");
    ms(3).await;
    c.line(start_line(2, LAG));
    settle().await; // no time passes: the line's own sweep wrote it
    assert_exact(&sidecar(&d, 1).unwrap(), frame(1), SEG, true);
    s.stop();
    settle().await;
}

/// A lost line: the deferral ends one poll period after it began, by the
/// one-shot timer, not at the next-but-one poll tick.
#[tokio::test(start_paused = true)]
async fn a_lost_line_is_waited_for_one_poll_period_at_most() {
    let home = Home::new("lost");
    let be = Arc::new(FakeHost::default());
    let s = sup(&home, be.clone(), SidecarTiming::Exact);
    s.start();
    let d = dir(&home, &s);
    let c = be.last();
    c.line(start_line(0, LAG));
    settle().await; // line 0's own sweep runs before any file exists
    for k in 0..3 {
        frag(&d, k, cb(k + 1));
    }
    ms(5_000).await;
    // t = 5 s: line 1 arrives; its sweep writes 0 and defers 1 (line 2 lost).
    c.line(start_line(1, LAG));
    settle().await;
    assert_exact(&sidecar(&d, 0).unwrap(), frame(0), SEG, true);
    assert!(sidecar(&d, 1).is_none());
    ms(POLL - 1).await; // t ≈ 15 s: the 10 s poll tick passed, budget not spent
    assert!(sidecar(&d, 1).is_none(), "deferred < one poll period");
    ms(2).await; // t ≈ 15.001 s: the one-shot fired (the next poll is at 20 s)
                 // The successor's stamp never came but file 2 is listed (legacy): N ends
                 // at mtime, contiguous with the legacy fragment 2 (R-WP2-3).
    assert_exact(&sidecar(&d, 1).unwrap(), frame(1), cb(2) - frame(1), true);
    s.stop();
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn crash_cut_final_sweep_never_defers_and_respawn_starts_a_fresh_table() {
    let home = Home::new("crash");
    let be = Arc::new(FakeHost::default());
    let s = sup(&home, be.clone(), SidecarTiming::Exact);
    s.start();
    let d1 = dir(&home, &s);
    let c = be.last();
    c.line(start_line(0, LAG));
    c.line(start_line(1, LAG));
    c.line(start_line(2, LAG)); // emitted, but the host dies before file 2
    frag(&d1, 0, cb(1));
    frag(&d1, 1, cb(1) + 30_000); // cut 30 s in
    settle().await;
    c.exit(1); // crash: final sweep, newest finalized:false
    settle().await;
    assert_exact(&sidecar(&d1, 0).unwrap(), frame(0), SEG, true);
    assert_exact(
        &sidecar(&d1, 1).unwrap(),
        frame(1),
        cb(1) + 30_000 - LAG - frame(1),
        false,
    );
    // Respawn after the 2 s backoff: a new session, a new table.
    ms(2_001).await;
    settle().await;
    assert_eq!(be.len(), 2);
    let d2 = dir(&home, &s);
    assert_ne!(d1, d2);
    let c2 = be.last();
    // Only line 0 of the new child; its fragment 1's stamp is lost
    // (rejected lag), so fragment 0 must NOT pick up the old run's stamp 1.
    c2.line(start_line(0, LAG + 500));
    c2.line(start_line(1, 9_000));
    settle().await;
    frag(&d2, 0, cb(1) + 1_500);
    frag(&d2, 1, cb(2));
    ms(POLL + 1).await;
    // successor rejected, file 1 listed (legacy) → mtime − startTs (R-WP2-3)
    let sc = sidecar(&d2, 0).unwrap();
    assert_exact(&sc, frame(0), cb(1) + 1_500 - frame(0), true);
    assert_ne!(sc["durationMs"], SEG, "the old run's stamp 1 leaked in");
    s.stop();
    settle().await;
    // Clean stop: the newest (fragment 1, rejected stamp) falls back to legacy.
    let sc1 = sidecar(&d2, 1).unwrap();
    assert_ne!(sc1["anchor"], "pts");
    assert_eq!(sc1["finalized"], true);
}

#[tokio::test(start_paused = true)]
async fn clean_stop_uses_the_end_line_for_the_last_fragment() {
    let home = Home::new("clean");
    let be = Arc::new(FakeHost::default());
    let s = sup(&home, be.clone(), SidecarTiming::Exact);
    s.start();
    let d = dir(&home, &s);
    let c = be.last();
    c.line(start_line(0, LAG));
    c.line(start_line(1, LAG));
    frag(&d, 0, cb(1));
    frag(&d, 1, cb(1) + 45_000);
    c.line(end_line(1, frame(1) + 44_000));
    settle().await;
    s.stop(); // SIGINT → EOS → Done after the END line
    settle().await;
    assert_exact(&sidecar(&d, 0).unwrap(), frame(0), SEG, true);
    assert_exact(&sidecar(&d, 1).unwrap(), frame(1), 44_000, true);
}

/// The default: the lines are consumed (taped) but never acted on — no
/// "pts" anchor, no deferral, the birthtime anchors of today.
#[tokio::test(start_paused = true)]
async fn legacy_ignores_the_timing_lines() {
    let home = Home::new("legacy");
    let be = Arc::new(FakeHost::default());
    let s = sup(&home, be.clone(), SidecarTiming::Legacy);
    s.start();
    let d = dir(&home, &s);
    let c = be.last();
    c.line(start_line(0, LAG));
    for k in 0..3 {
        frag(&d, k, cb(k + 1));
    }
    ms(POLL + 1).await; // line 1 never came: exact would defer fragment 0
    for k in 0..2 {
        let sc = sidecar(&d, k).unwrap();
        assert_ne!(sc["anchor"], "pts", "{sc}");
        assert_ne!(sc["startTs"], frame(k), "{sc}");
    }
    s.stop();
    settle().await;
}

/// WP1 final report: the host prints `XG-FRAGMENT*` in `host` + `legacy` too
/// (it does not know the sidecar mode). Legacy must ignore them completely:
/// the same number of sweeps as a run that printed nothing (no per-line
/// sweep; reviewer mutant R8), and every sidecar byte-identical to the
/// legacy core plan of the same listing (`plan_sweep`, today's code).
#[tokio::test(start_paused = true)]
async fn legacy_with_real_host_lines_is_byte_identical_and_sweeps_no_more() {
    async fn run(with_lines: bool) -> (usize, PathBuf, Home) {
        let home = Home::new(if with_lines { "leg-lines" } else { "leg-quiet" });
        let be = Arc::new(FakeHost::default());
        let sweeps = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = {
            let n = sweeps.clone();
            sup_with(&home, be.clone(), SidecarTiming::Legacy, move |o| {
                o.on_active_path = Some(Arc::new(move |_k: &str, _p: Option<&Path>| {
                    n.fetch_add(1, Ordering::SeqCst);
                }))
            })
        };
        s.start();
        let d = dir(&home, &s);
        let c = be.last();
        for k in 0..4 {
            if with_lines {
                c.line(start_line(k, if k == 0 { 12 } else { 2_000 }));
            }
            frag(&d, k, cb(k + 1));
        }
        settle().await;
        ms(2 * POLL + 1).await; // two poll sweeps
        let n = sweeps.load(Ordering::SeqCst);
        s.stop();
        settle().await;
        (n, d, home)
    }
    let (quiet, _, _h1) = run(false).await;
    let (loud, d, _h2) = run(true).await;
    assert_eq!(loud, quiet, "timing lines triggered sweeps in legacy");
    assert!(quiet >= 2);
    // Byte identity with the legacy plan: re-plan the same listing with the
    // pair each sidecar was written with (startMono's derivation inverted).
    let manifest = xorgate_core::sidecar::read_session_manifest_text(
        &std::fs::read_to_string(d.join("session.json")).unwrap(),
    )
    .unwrap();
    let entries: Vec<xorgate_core::sidecar::DirEntry> = (0..4)
        .map(|k| {
            let name = format!("{k:05}.mp4");
            xorgate_core::sidecar::DirEntry {
                stat: xorgate_hal::fragment_stat::stat_fragment(&d.join(&name)).ok(),
                name,
            }
        })
        .collect();
    let mut checked = 0;
    for k in 0..4u64 {
        let p = d.join(format!("{k:05}.json"));
        let Ok(bytes) = std::fs::read_to_string(&p) else {
            continue;
        };
        let sc: Value = serde_json::from_str(&bytes).unwrap();
        assert_ne!(sc["anchor"], "pts");
        let pair = xorgate_core::sidecar::ClockPair {
            wall_ms: sc["startTs"].as_f64().unwrap(),
            mono_ms: sc["startMono"].as_f64().unwrap(),
        };
        let plan = xorgate_core::sidecar::plan_sweep(
            &d.to_string_lossy(),
            &manifest,
            &entries,
            &xorgate_core::sidecar::SweepOpts {
                include_newest: k == 3,
                finalized: (k == 3).then_some(true),
                time_source: Some(TimeSource::Ntp),
                clock_pair: pair,
            },
        );
        let w = plan
            .writes
            .iter()
            .find(|w| w.sidecar_path == p.to_string_lossy())
            .expect("planned");
        assert_eq!(w.sidecar.to_json(), bytes, "fragment {k}");
        checked += 1;
    }
    assert_eq!(checked, 4, "0..2 on the polls, 3 by the final sweep");
}

/// `exact` with a child that never prints a timing line (the host's clock
/// precondition failed): never deferred, legacy anchors.
#[tokio::test(start_paused = true)]
async fn an_untimed_run_in_exact_mode_is_never_deferred() {
    let home = Home::new("untimed");
    let be = Arc::new(FakeHost::default());
    let s = sup(&home, be.clone(), SidecarTiming::Exact);
    s.start();
    let d = dir(&home, &s);
    for k in 0..3 {
        frag(&d, k, cb(k + 1));
    }
    ms(POLL + 1).await;
    for k in 0..2 {
        let sc = sidecar(&d, k).expect("written on the first poll");
        assert_ne!(sc["anchor"], "pts", "{sc}");
    }
    s.stop();
    settle().await;
}

#[test]
fn mtime_pinning_works_here() {
    // Guard for the tests above: the platform honours set_modified.
    let home = Home::new("mt");
    frag(&home.0, 0, T0);
    let m = std::fs::metadata(home.0.join("00000.mp4"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(m, SystemTime::UNIX_EPOCH + Duration::from_millis(T0));
}
