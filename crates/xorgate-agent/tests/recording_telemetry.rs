//! WP2: the telemetry recorder end to end over a real tmp tree.
//!
//! 1. Byte comparison (common-brief rule 9): `fixtures/recorder_gate/
//!    telemetry-recorder.json` holds, per scripted tick sequence, every
//!    finalized segment of the REAL `TelemetryRecorder` + `SegmentWriter` @
//!    89b5c64 DECOMPRESSED (name, bytes, line count, sha256, small ones in
//!    full) and the `activePath()` after each tick. The Rust recorder runs the
//!    same steps with the same clocks/stamps/time source and must produce the
//!    same names and the same decompressed bytes (gz bytes may differ, P6/D24).
//!    This covers the five `test/recording-recorder.test.ts` it()s.
//! 2. The segment-I/O scenarios of the WP2 brief: disk-full gzip failure →
//!    the next tick recovers (S2); a failed sample append; torn last line →
//!    orphan finalize; leftover temp + header-only orphans.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;
use sha2::{Digest, Sha256};
use xorgate_agent::ctx::GroupSample;
use xorgate_agent::recording::telemetry::{
    finalize_orphan_segments, TelemetryRecorder, TelemetryRecorderOpts,
};
use xorgate_agent::recording::time_source::{ProbeFn, TimeSourceHandle};
use xorgate_core::clock::Clock;
use xorgate_core::segments::GzipFinalizeStep;
use xorgate_core::session::ProbedTimeSource;
use xorgate_core::telemetry::{MetricGroup, MetricLeaf};
use xorgate_core::types::SteppedClockSource;
use xorgate_hal::segment_io::{IoPoint, SegmentExecutor};

#[derive(Default)]
struct TestClock {
    wall: AtomicI64,
    mono: AtomicU64,
}

impl Clock for TestClock {
    fn wall_ms(&self) -> i64 {
        self.wall.load(Ordering::SeqCst)
    }
    fn mono_ms(&self) -> u64 {
        self.mono.load(Ordering::SeqCst)
    }
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-wp2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A time source the test drives: probe values via `refresh()`, gps/nitz
/// via `note_step` (how the real tracker gets each value).
struct Ts {
    handle: TimeSourceHandle,
    probe: Arc<Mutex<ProbedTimeSource>>,
}

impl Ts {
    fn new(clock: Arc<TestClock>) -> Self {
        let probe = Arc::new(Mutex::new(ProbedTimeSource::Unsynced));
        let p = probe.clone();
        let f: ProbeFn = Arc::new(move || {
            let v = *p.lock().unwrap();
            Box::pin(async move { v })
        });
        Ts {
            handle: TimeSourceHandle::create(i64::MAX / 4, f, clock),
            probe,
        }
    }

    async fn set(&self, v: &str) {
        match v {
            "gps" => self.handle.note_step(SteppedClockSource::Gps),
            "nitz" => self.handle.note_step(SteppedClockSource::Nitz),
            other => {
                *self.probe.lock().unwrap() = serde_json::from_value(Value::from(other)).unwrap();
                self.handle.refresh().await;
            }
        }
    }
}

fn leaf(v: &Value) -> MetricLeaf {
    serde_json::from_value(v.clone()).unwrap()
}

fn fields(v: &Value) -> MetricGroup {
    v.as_object()
        .unwrap()
        .iter()
        .map(|(k, l)| (k.clone(), leaf(l)))
        .collect()
}

struct Step {
    wall: i64,
    mono: u64,
    mono_ns: u64,
    time_source: Option<String>,
    groups: Vec<(String, u64, MetricGroup)>,
}

fn steps(input: &Value) -> Vec<Step> {
    if let Some(w) = input.pointer("/gen/wide") {
        let n = w["n"].as_u64().unwrap();
        let k = w["fields"].as_u64().unwrap();
        return (0..n)
            .map(|i| {
                let mut f = MetricGroup::new();
                for j in 0..k {
                    f.insert(
                        format!("field_{j}_with_a_long_name"),
                        MetricLeaf::with_unit(j as f64 + 0.123456789, "unit-string-x"),
                    );
                }
                f.insert("i".into(), MetricLeaf::new(i as f64));
                Step {
                    wall: 1_700_000_000_000 + i as i64 * 50,
                    mono: i * 50,
                    mono_ns: i * 50_000_000,
                    time_source: None,
                    groups: vec![("imu".into(), i + 1, f)],
                }
            })
            .collect();
    }
    input["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| Step {
            wall: s["wall"].as_i64().unwrap(),
            mono: s["mono"].as_u64().unwrap(),
            mono_ns: s["monoNs"].as_u64().unwrap(),
            time_source: s["timeSource"].as_str().map(str::to_string),
            groups: s["groups"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(g, v)| (g.clone(), v[0].as_u64().unwrap(), fields(&v[1])))
                .collect(),
        })
        .collect()
}

type Shared<T> = Arc<Mutex<T>>;

struct Rig {
    clock: Arc<TestClock>,
    ts: Ts,
    samples: Shared<IndexMap<String, GroupSample>>,
    mono_ns: Arc<AtomicU64>,
    closed: Shared<Vec<PathBuf>>,
    base: tokio::time::Instant,
}

impl Rig {
    fn new() -> Self {
        let clock = Arc::new(TestClock::default());
        Rig {
            ts: Ts::new(clock.clone()),
            clock,
            samples: Arc::new(Mutex::new(IndexMap::new())),
            mono_ns: Arc::new(AtomicU64::new(0)),
            closed: Arc::new(Mutex::new(Vec::new())),
            base: tokio::time::Instant::now(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn recorder(
        &self,
        root: &Path,
        rate_hz: u32,
        max_bytes: Option<u64>,
        max_age_ms: Option<u64>,
        cadence: Option<IndexMap<String, f64>>,
        executor: Option<SegmentExecutor>,
    ) -> TelemetryRecorder {
        let samples = self.samples.clone();
        let mono_ns = self.mono_ns.clone();
        let closed = self.closed.clone();
        TelemetryRecorder::new(TelemetryRecorderOpts {
            root: root.to_path_buf(),
            device_id: "dev-1".into(),
            session_id: "session-x".into(),
            boot_id: "boot-1".into(),
            rate_hz,
            time_source: self.ts.handle.clone(),
            sample_groups: Arc::new(move || {
                let s = samples.lock().unwrap().clone();
                Box::pin(async move { s })
            }),
            on_segment_closed: Some(Arc::new(move |p: &Path| {
                closed.lock().unwrap().push(p.to_path_buf())
            })),
            on_active_path: None,
            clock: self.clock.clone(),
            tape: xorgate_hal::io::null_tape(),
            segment_max_bytes: max_bytes,
            segment_max_age_ms: max_age_ms,
            native_cadence_hz: cadence,
            mono_ns: Some(Arc::new(move || mono_ns.load(Ordering::SeqCst))),
            executor,
        })
    }

    async fn step(&self, s: &Step) {
        self.clock.wall.store(s.wall, Ordering::SeqCst);
        self.clock.mono.store(s.mono, Ordering::SeqCst);
        self.mono_ns.store(s.mono_ns, Ordering::SeqCst);
        if let Some(ts) = &s.time_source {
            self.ts.set(ts).await;
        }
        let mut m = self.samples.lock().unwrap();
        m.clear();
        for (g, um, f) in &s.groups {
            m.insert(
                g.clone(),
                GroupSample {
                    fields: f.clone(),
                    updated_mono: self.base + Duration::from_nanos(*um),
                },
            );
        }
    }
}

fn gunzip(b: &[u8]) -> String {
    let mut d = flate2::read::GzDecoder::new(b);
    let mut s = String::new();
    d.read_to_string(&mut s).unwrap();
    s
}

fn sorted_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn finalized_segments_match_the_ts_recorder_byte_for_byte() {
    let text = std::fs::read_to_string(format!(
        "{}/../../fixtures/recorder_gate/telemetry-recorder.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["jsCommit"], "89b5c6407ef88e20f7168bd767d22a7be04ebcb6");
    let cases = doc["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 14);
    for (ci, c) in cases.iter().enumerate() {
        let name = c["name"].as_str().unwrap();
        let input = &c["input"];
        let want = &c["expected"];
        let home = scratch(&format!("rec{ci}"));
        let root = home.join("recordings");
        let rig = Rig::new();
        let st = steps(input);
        // The tracker's value before the first tick.
        rig.clock.wall.store(st[0].wall, Ordering::SeqCst);
        rig.clock.mono.store(st[0].mono, Ordering::SeqCst);
        rig.ts
            .set(input["initialTimeSource"].as_str().unwrap())
            .await;
        let cadence = input
            .get("nativeCadenceHz")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value(v.clone()).unwrap());
        let rec = rig.recorder(
            &root,
            input["rateHz"].as_u64().unwrap() as u32,
            input["maxBytes"].as_u64(),
            input["maxAgeMs"].as_u64(),
            cadence,
            None,
        );
        let dir = PathBuf::from(rec.dir());
        let want_active = want["active"].as_array().unwrap();
        for (i, s) in st.iter().enumerate() {
            rig.step(s).await;
            rec.tick().await;
            let active = rec
                .active_path()
                .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned());
            assert_eq!(
                active.as_deref(),
                want_active[i].as_str(),
                "{name}: active after tick {i}"
            );
        }
        rec.stop().unwrap();
        let want_files = want["files"].as_array().unwrap();
        let names = sorted_names(&dir);
        let want_names: Vec<&str> = want_files
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, want_names, "{name}: file names");
        for f in want_files {
            let fname = f["name"].as_str().unwrap();
            let raw = std::fs::read(dir.join(fname)).unwrap();
            let got = gunzip(&raw);
            if let Some(t) = f["text"].as_str() {
                assert_eq!(got, t, "{name}: {fname}");
            }
            assert_eq!(
                got.len() as u64,
                f["bytes"].as_u64().unwrap(),
                "{name}: {fname} bytes"
            );
            assert_eq!(
                got.split('\n').filter(|l| !l.is_empty()).count() as u64,
                f["lines"].as_u64().unwrap(),
                "{name}: {fname} lines"
            );
            assert_eq!(
                hex::encode(Sha256::digest(got.as_bytes())),
                f["sha256"].as_str().unwrap(),
                "{name}: {fname} sha256"
            );
            // Node's gzip header (P6): mtime 0, OS 3.
            assert_eq!(
                &raw[..10],
                &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3],
                "{name}: header"
            );
        }
        // onSegmentClosed fired once per gz, in order.
        let closed: Vec<String> = rig
            .closed
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let mut closed_sorted = closed.clone();
        closed_sorted.sort();
        assert_eq!(closed_sorted, names, "{name}: onSegmentClosed");
        let _ = std::fs::remove_dir_all(&home);
    }
}

fn imu(rig: &Rig, wall: i64, mono: u64, um: u64) -> Step {
    let _ = rig;
    let mut f = MetricGroup::new();
    f.insert("a".into(), MetricLeaf::new(um as f64));
    Step {
        wall,
        mono,
        mono_ns: mono * 1_000_000,
        time_source: None,
        groups: vec![("imu".into(), um, f)],
    }
}

fn lines_of(text: &str) -> Vec<Value> {
    text.split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// S2 / CF35: a gzip that fails on a full disk inside `writeLine`'s rotation
/// aborts the write BEFORE `open` (`segments.ts:116-121`): the plain segment
/// stays on disk closed, the temp gz is left for the next boot's sweep, the
/// seq consumed by the lost sample stays consumed (`this.seq++` ran), and the
/// NEXT tick opens a fresh segment with the segment seq the failed open
/// never consumed. The next boot's orphan sweep then ships the stranded one.
#[tokio::test]
async fn disk_full_gzip_failure_then_next_tick_recovers() {
    let home = scratch("diskfull");
    let root = home.join("recordings");
    let rig = Rig::new();
    rig.ts.set("ntp").await;
    let arm = Arc::new(AtomicUsize::new(0));
    let a2 = arm.clone();
    let exec = SegmentExecutor::with_fault(Box::new(move |at, _p| {
        if at == IoPoint::Close(GzipFinalizeStep::WriteGzip) && a2.load(Ordering::SeqCst) == 1 {
            a2.store(2, Ordering::SeqCst);
            return Err(std::io::Error::from_raw_os_error(28)); // ENOSPC
        }
        Ok(())
    }));
    let rec = rig.recorder(&root, 10, None, Some(1000), None, Some(exec));
    let dir = PathBuf::from(rec.dir());
    rig.step(&imu(&rig, 5_000, 0, 1)).await;
    rec.tick().await; // opens seg-5000-00000, sample seq 0
    rig.step(&imu(&rig, 5_500, 500, 2)).await;
    rec.tick().await; // sample seq 1
    arm.store(1, Ordering::SeqCst);
    rig.step(&imu(&rig, 6_000, 1000, 3)).await;
    rec.tick().await; // rotate due → gzip ENOSPC → tick failed, seq 2 lost
    assert_eq!(arm.load(Ordering::SeqCst), 2, "fault fired");
    assert_eq!(rec.active_path(), None, "closed, not reopened");
    let pid = std::process::id();
    assert_eq!(
        sorted_names(&dir),
        vec![
            "seg-5000-00000.jsonl".to_string(),
            format!("seg-5000-00000.jsonl.gz.tmp.{pid}")
        ]
    );
    assert!(rig.closed.lock().unwrap().is_empty());
    rig.step(&imu(&rig, 6_100, 1100, 4)).await;
    rec.tick().await; // recovers: a new segment, seq 1 reused for its NAME
    assert_eq!(rec.active_path().unwrap(), dir.join("seg-6100-00001.jsonl"));
    rec.stop().unwrap();
    let gz = std::fs::read(dir.join("seg-6100-00001.jsonl.gz")).unwrap();
    let l = lines_of(&gunzip(&gz));
    assert_eq!(l.len(), 2);
    assert_eq!(l[1]["seq"], 3);
    // Next boot: the sweep removes the temp and ships the stranded segment.
    assert_eq!(finalize_orphan_segments(&root.join("telemetry")), 1);
    assert_eq!(
        sorted_names(&dir),
        vec!["seg-5000-00000.jsonl.gz", "seg-6100-00001.jsonl.gz"]
    );
    let l = lines_of(&gunzip(
        &std::fs::read(dir.join("seg-5000-00000.jsonl.gz")).unwrap(),
    ));
    assert_eq!(l.len(), 3);
    assert_eq!(l[0]["type"], "header");
    assert_eq!(
        (l[1]["seq"].clone(), l[2]["seq"].clone()),
        (Value::from(0), Value::from(1))
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// A failed sample `writeSync` (`segments.ts:172`) counts nothing: a segment
/// whose every sample failed is header-only at rotation and is dropped.
#[tokio::test]
async fn failed_appends_count_nothing() {
    let home = scratch("append");
    let root = home.join("recordings");
    let rig = Rig::new();
    rig.ts.set("rtc").await;
    let writes = Arc::new(AtomicUsize::new(0));
    let w2 = writes.clone();
    let exec = SegmentExecutor::with_fault(Box::new(move |at, _p| {
        if at == IoPoint::Write {
            // 1st write = the header; every later one fails.
            if w2.fetch_add(1, Ordering::SeqCst) >= 1 {
                return Err(std::io::Error::from_raw_os_error(28));
            }
        }
        Ok(())
    }));
    let rec = rig.recorder(&root, 10, None, Some(1000), None, Some(exec));
    let dir = PathBuf::from(rec.dir());
    rig.step(&imu(&rig, 1, 0, 1)).await;
    rec.tick().await;
    rig.step(&imu(&rig, 2, 500, 2)).await;
    rec.tick().await;
    assert!(rec.active_path().is_some());
    rig.step(&imu(&rig, 3, 1500, 2)).await;
    rec.tick().await; // idle age rotation: header-only → unlinked
    assert_eq!(rec.active_path(), None);
    assert!(sorted_names(&dir).is_empty());
    assert!(rig.closed.lock().unwrap().is_empty());
    rec.stop().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// `finalizeOrphanSegments` (`segments.ts:224-270`): torn last line
/// truncated, header-only / no-newline orphans deleted, temps removed,
/// closed gz and other files untouched, non-directories skipped.
#[test]
fn orphan_sweep_rules_on_disk() {
    let home = scratch("orphans");
    let troot = home.join("recordings").join("telemetry");
    assert_eq!(finalize_orphan_segments(&troot), 0, "no tree yet");
    let s = troot.join("s1");
    std::fs::create_dir_all(&s).unwrap();
    std::fs::write(troot.join("stray-file"), "x").unwrap();
    std::fs::write(
        s.join("seg-1-00000.jsonl"),
        "{\"type\":\"header\"}\n{\"ts\":1}\n{\"ts\":2,\"mo",
    )
    .unwrap();
    std::fs::write(s.join("seg-2-00001.jsonl"), "{\"type\":\"header\"}\n").unwrap();
    std::fs::write(s.join("seg-3-00002.jsonl"), "{\"ts\":1,").unwrap();
    std::fs::write(s.join("seg-0-00009.jsonl.gz.tmp.77"), "junk").unwrap();
    std::fs::write(s.join("seg-0-00008.jsonl.gz"), "keep").unwrap();
    std::fs::write(s.join("session.json"), "{}").unwrap();
    assert_eq!(finalize_orphan_segments(&troot), 1);
    assert_eq!(
        sorted_names(&s),
        vec![
            "seg-0-00008.jsonl.gz",
            "seg-1-00000.jsonl.gz",
            "session.json"
        ]
    );
    assert_eq!(
        gunzip(&std::fs::read(s.join("seg-1-00000.jsonl.gz")).unwrap()),
        "{\"type\":\"header\"}\n{\"ts\":1}\n"
    );
    assert_eq!(
        std::fs::read(s.join("seg-0-00008.jsonl.gz")).unwrap(),
        b"keep"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// `start()` ticks on its own interval (first tick one period in) and
/// `stop()` finalizes synchronously; no tick lands after stop.
#[tokio::test(start_paused = true)]
async fn own_interval_and_sync_stop() {
    let home = scratch("interval");
    let root = home.join("recordings");
    let rig = Rig::new();
    rig.ts.set("ntp").await;
    let rec = rig.recorder(&root, 5, None, None, None, None);
    let dir = PathBuf::from(rec.dir());
    let mut um = 0;
    rig.step(&imu(&rig, 100, 0, 0)).await;
    rec.start();
    rec.start(); // idempotent
    for t in 1..=10u64 {
        um += 1;
        rig.step(&imu(&rig, 100 + t as i64 * 200, t * 200, um))
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
    }
    rec.stop().unwrap();
    let names = sorted_names(&dir);
    assert_eq!(names.len(), 1, "{names:?}");
    let l = lines_of(&gunzip(&std::fs::read(dir.join(&names[0])).unwrap()));
    assert!(l.len() >= 9 && l.len() <= 11, "{} lines", l.len());
    rig.step(&imu(&rig, 9_999, 9_999, um + 1)).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    rec.tick().await;
    assert_eq!(sorted_names(&dir).len(), 1, "nothing after stop");
    let _ = std::fs::remove_dir_all(&home);
}
