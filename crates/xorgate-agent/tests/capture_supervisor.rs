//! `VideoCaptureSupervisor` scenarios ported from
//! `test/recording-video-capture.test.ts` @ 89b5c64 — "VideoCaptureSupervisor"
//! ×6, "progress watchdog" ×7, "camera spawn stagger" ×3, "setCameras
//! (Phase 6)" ×3 = 19 — step by step, on a paused tokio clock, through a
//! fake `CaptureBackend` that behaves like the TS `fakeSpawner()` (records
//! argv; `stop(sig)` records the signal and resolves `done` with exit 0;
//! `exit(code)` resolves it with `code`). The test writes the fragments
//! splitmuxsink would write into the session dir.
//!
//! Plus: the recover-orphans tree (CF28: a TS-valid but untyped manifest is
//! still swept), `stopAndWait` SIGKILL escalation, and the inotify trigger on
//! a real clock.
//!
//! Phase 7e: "VideoCaptureSupervisor replay hooks (device-simulator Phase 2)"
//! ×5 @ 09da38d (`test/recording-video-capture.test.ts:1480-1632`) — the
//! `replay_*` tests at the end — plus the 7e pins: `on_ended` after the final
//! sweep, a stale child's stdout (D72), and real mode ignoring gst's lines.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use xorgate_agent::ctx::AgentClock;
use xorgate_agent::recording::capture::*;
use xorgate_core::capture_ladder::CaptureState;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::types::TimeSource;
use xorgate_core::video_presets::resolve_video_preset;
use xorgate_hal::child::{ChildDone, ChildEvent};

// ---------------------------------------------------------------------------
// fakes

struct Entry {
    args: Vec<String>,
    stopped: Mutex<Option<i32>>,
    tx: mpsc::UnboundedSender<ChildEvent>,
    exited: AtomicBool,
    /// `stdoutSpawner`: `stop()` prints gst-launch's `-e` EOS line first.
    eos_on_signal: bool,
}

impl Entry {
    /// The child "prints" a stdout line (the TS spawner's `stdout?.(line)`).
    fn stdout(&self, line: &str) {
        let _ = self.tx.send(ChildEvent::Stdout(line.to_string()));
    }
    fn exit(&self, code: Option<i32>) {
        if !self.exited.swap(true, Ordering::SeqCst) {
            let _ = self.tx.send(ChildEvent::Done(ChildDone {
                exit_code: code,
                failed: code.unwrap_or(0) != 0,
                ..ChildDone::default()
            }));
        }
    }
    fn stopped(&self) -> Option<i32> {
        *self.stopped.lock().unwrap()
    }
    fn joined(&self) -> String {
        self.args.join(" ")
    }
}

struct FakeChild {
    entry: Arc<Entry>,
    pid: Option<u32>,
}

impl CaptureChild for FakeChild {
    fn pid(&self) -> Option<u32> {
        self.pid
    }
    fn signal(&self, sig: i32) {
        // TS fakeSpawner: `stop(sig)` records the signal and resolves exit 0.
        *self.entry.stopped.lock().unwrap() = Some(sig);
        if self.entry.eos_on_signal && !self.entry.exited.load(Ordering::SeqCst) {
            self.entry.stdout("Got EOS from element \"pipeline0\".");
        }
        self.entry.exit(Some(0));
    }
    fn renice(&self, _nice: i32) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FakeBackend {
    spawns: Mutex<Vec<Arc<Entry>>>,
    pid: Option<u32>,
    eos_on_signal: bool,
}

impl FakeBackend {
    fn with_pid(pid: u32) -> Self {
        Self {
            pid: Some(pid),
            ..Self::default()
        }
    }
    fn get(&self, i: usize) -> Arc<Entry> {
        self.spawns.lock().unwrap()[i].clone()
    }
    fn len(&self) -> usize {
        self.spawns.lock().unwrap().len()
    }
    fn last(&self) -> Arc<Entry> {
        self.spawns.lock().unwrap().last().unwrap().clone()
    }
}

impl CaptureBackend for FakeBackend {
    fn program(&self) -> &str {
        "gst-launch-1.0"
    }
    fn spawn(
        &self,
        args: &[String],
    ) -> (Box<dyn CaptureChild>, mpsc::UnboundedReceiver<ChildEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let entry = Arc::new(Entry {
            args: args.to_vec(),
            stopped: Mutex::new(None),
            tx,
            exited: AtomicBool::new(false),
            eos_on_signal: self.eos_on_signal,
        });
        self.spawns.lock().unwrap().push(entry.clone());
        (
            Box::new(FakeChild {
                entry,
                pid: self.pid,
            }),
            rx,
        )
    }
}

// ---------------------------------------------------------------------------
// harness

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "xg-capture-{tag}-{}-{}",
            std::process::id(),
            xorgate_hal::fragment_stat::hrtime_ns()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn video(&self) -> PathBuf {
        self.0.join("recordings").join("video")
    }
    fn session_dir(&self, key: &str, sid: &str) -> PathBuf {
        self.video().join(key).join(sid)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Events = Arc<Mutex<Vec<(ActivityLevel, String)>>>;

fn cam(key: &str, name: &str) -> CaptureCamera {
    CaptureCamera {
        stream_key: key.into(),
        camera_name: name.into(),
    }
}

fn cam0() -> Vec<CaptureCamera> {
    vec![cam("cam0", "/base/cam0@1a")]
}

fn two_cams() -> Vec<CaptureCamera> {
    vec![cam("cam0", "/base/cam0@1a"), cam("cam1", "/base/cam1@1a")]
}

/// `makeSup` of the TS suites: dev-1, 1080p15-2mbps, 60 s segments,
/// `timeSource: () => "ntp"`, `onEvent` collected.
fn opts(
    home: &Home,
    cameras: Vec<CaptureCamera>,
    record: bool,
    backend: Arc<FakeBackend>,
    events: &Events,
) -> VideoCaptureOpts {
    let mut o = VideoCaptureOpts::new(
        "dev-1",
        cameras,
        record,
        resolve_video_preset("1080p15-2mbps").clone(),
        60,
        home.video(),
        home.0.join("shm"),
        "boot-1",
        Arc::new(|| TimeSource::Ntp),
        Arc::new(AgentClock::following_tokio(1_760_000_000_000)),
    );
    o.backend = backend;
    let ev = events.clone();
    o.on_event = Some(Arc::new(move |l, m| {
        ev.lock().unwrap().push((l, m.to_string()))
    }));
    o.fs_watch = false; // paused clock: the poll sweep only
    o.shm_segment_dir = home.0.join("devshm");
    o
}

async fn delay(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn read_json(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .unwrap()
}

fn has_event(events: &Events, needle: &str) -> bool {
    events
        .lock()
        .unwrap()
        .iter()
        .any(|(_, m)| m.contains(needle))
}

fn first_session(sup: &VideoCaptureSupervisor) -> String {
    sup.status()[0].session_id.clone().expect("session")
}

// ---------------------------------------------------------------------------
// describe("VideoCaptureSupervisor") ×6

#[tokio::test(start_paused = true)]
async fn record_mode_mints_a_session_writes_session_json_and_reports_recording() {
    let home = Home::new("rec");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let mut o = opts(&home, cam0(), true, be.clone(), &ev);
    o.stable_ms = 50;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    assert_eq!(be.len(), 1);
    assert!(be.get(0).joined().contains("splitmuxsink"));

    let sid = first_session(&sup);
    let dir = home.session_dir("cam0", &sid);
    let m = read_json(&dir.join("session.json"));
    assert_eq!(m["sessionId"], sid.as_str());
    assert_eq!(m["deviceId"], "dev-1");
    assert_eq!(m["bootId"], "boot-1");
    assert_eq!(m["timeSource"], "ntp");
    assert_eq!(m["preset"], "1080p15-2mbps");
    let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "v",
            "deviceId",
            "streamKey",
            "sessionId",
            "bootId",
            "timeSource",
            "preset",
            "width",
            "height",
            "fps",
            "bitrateKbps",
            "segmentSeconds",
            "anchorTs",
            "anchorMono"
        ]
    );
    assert!(be
        .get(0)
        .args
        .contains(&format!("location={}/%05d.mp4", dir.to_string_lossy())));

    delay(80).await; // past stableMs
    assert_eq!(sup.status()[0].state, CaptureState::Running);
    assert!(sup.any_recording());
    sup.stop();
    delay(20).await;
    assert_eq!(be.get(0).stopped(), Some(libc::SIGINT));
    assert_eq!(sup.status()[0].state, CaptureState::Stopped);
}

#[tokio::test(start_paused = true)]
async fn bridge_only_mode_creates_no_session_and_never_reports_recording() {
    let home = Home::new("bridge");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let mut o = opts(&home, cam0(), false, be.clone(), &ev);
    o.stable_ms = 50;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    assert!(!be.get(0).joined().contains("splitmuxsink"));
    assert_eq!(sup.status()[0].session_id, None);
    assert!(!home.video().join("cam0").exists());
    delay(80).await;
    assert!(!sup.any_recording());
    assert_eq!(
        ev.lock().unwrap()[0],
        (
            ActivityLevel::Info,
            "video bridge started: cam0 (streaming only, recording off)".to_string()
        )
    );
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn restarts_a_crashed_child_with_a_new_session_and_sidecars_the_old_fragments() {
    let home = Home::new("crash");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let mut o = opts(&home, cam0(), true, be.clone(), &ev);
    o.stable_ms = 50;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let first = first_session(&sup);
    let dir1 = home.session_dir("cam0", &first);
    std::fs::write(dir1.join("00000.mp4"), vec![1u8; 100]).unwrap();
    std::fs::write(dir1.join("00001.mp4"), vec![1u8; 50]).unwrap();
    delay(80).await; // survive stableMs
    be.get(0).exit(Some(1));
    delay(20).await;

    // Crash sweep: BOTH fragments get sidecars; the newest is finalized:false.
    assert!(dir1.join("00000.json").exists());
    assert!(dir1.join("00001.json").exists());
    assert_eq!(read_json(&dir1.join("00001.json"))["finalized"], false);
    assert_eq!(read_json(&dir1.join("00000.json"))["finalized"], true);
    assert_eq!(sup.status()[0].session_id, None);
    assert!(has_event(
        &ev,
        &format!(
            "video capture cam0 exited (code 1, session {}…); restarting",
            &first[..8]
        )
    ));
    // The prompt restart (2 s) is a NEW session.
    delay(2_000).await;
    assert_eq!(be.len(), 2);
    assert_ne!(first_session(&sup), first);
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn sidecars_follow_a_mid_session_time_source_upgrade_session_json_keeps_the_spawn_value() {
    let home = Home::new("ts");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let ts = Arc::new(Mutex::new(TimeSource::Unsynced));
    let mut o = opts(&home, cam0(), true, be.clone(), &ev);
    let t = ts.clone();
    o.time_source = Arc::new(move || *t.lock().unwrap());
    o.stable_ms = 10;
    o.sweep_poll_ms = 25;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let dir = home.session_dir("cam0", &first_session(&sup));

    std::fs::write(dir.join("00000.mp4"), vec![1u8; 100]).unwrap();
    std::fs::write(dir.join("00001.mp4"), vec![1u8; 100]).unwrap();
    delay(80).await; // sweep closes 00000 while the clock is still unsynced
    assert_eq!(read_json(&dir.join("00000.json"))["timeSource"], "unsynced");

    *ts.lock().unwrap() = TimeSource::Ntp; // NTP lands mid-session
    std::fs::write(dir.join("00002.mp4"), vec![1u8; 100]).unwrap();
    delay(80).await; // sweep closes 00001 with the upgraded value
    assert_eq!(read_json(&dir.join("00001.json"))["timeSource"], "ntp");

    assert_eq!(
        read_json(&dir.join("session.json"))["timeSource"],
        "unsynced"
    );
    sup.stop();
    delay(20).await;
    // Final sweep also stamps the current value.
    assert_eq!(read_json(&dir.join("00002.json"))["timeSource"], "ntp");
}

#[tokio::test(start_paused = true)]
async fn fires_on_spawned_per_spawn_so_the_worker_can_bounce_the_kvs_master() {
    let home = Home::new("spawned");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let spawned = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut o = opts(&home, vec![cam("cam0", "/c")], false, be.clone(), &ev);
    let sp = spawned.clone();
    o.on_spawned = Some(Arc::new(move |k| sp.lock().unwrap().push(k.to_string())));
    o.stable_ms = 10;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    assert_eq!(*spawned.lock().unwrap(), ["cam0"]);
    sup.stop();
}

/// The TS `it()` stops after the first retry ("too slow for a unit test");
/// on a paused clock the whole ladder runs: retry 1/3 at 2 s, 2/3 at 4 s, then
/// the 3rd quick death parks for 60 s with the counters reset.
#[tokio::test(start_paused = true)]
async fn parks_after_repeated_quick_failures() {
    let home = Home::new("quick");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let mut o = opts(&home, vec![cam("cam0", "/c")], false, be.clone(), &ev);
    o.stable_ms = 10_000;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    be.last().exit(Some(1));
    delay(20).await;
    assert_eq!(sup.status()[0].state, CaptureState::Retrying);
    assert!(has_event(
        &ev,
        "video capture cam0 exited (code 1); retry 1/3 in 2000ms"
    ));
    delay(2_000).await;
    assert_eq!(be.len(), 2);
    be.last().exit(Some(1));
    delay(20).await;
    assert!(has_event(
        &ev,
        "video capture cam0 exited (code 1); retry 2/3 in 4000ms"
    ));
    delay(4_000).await;
    assert_eq!(be.len(), 3);
    be.last().exit(None);
    delay(20).await;
    assert_eq!(sup.status()[0].state, CaptureState::Failed);
    assert!(has_event(
        &ev,
        "video capture cam0 exited (code ?) before stabilizing 3x; parking for 60s (camera in use? libcamera error?)"
    ));
    delay(59_000).await;
    assert_eq!(be.len(), 3);
    delay(1_000).await;
    assert_eq!(be.len(), 4); // the slow retry
    sup.stop();
}

/// Accepted divergence D27 (review-wp4 M1): the shm dir, the session dir and
/// `session.json` cannot be created (their parents are regular files). The
/// TS throws from `start()` (`video-capture.ts:405`/`:608`/`:626`) and wedges
/// the manager's video transitions; Rust logs, spawns gst anyway (pointed at
/// the missing dir), and the ordinary quick-fail ladder takes over.
#[tokio::test(start_paused = true)]
async fn d27_spawn_time_fs_failure_logs_and_lets_the_ladder_retry() {
    let home = Home::new("d27");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let blocker = home.0.join("not-a-dir");
    std::fs::write(&blocker, b"").unwrap();
    let mut o = opts(&home, cam0(), true, be.clone(), &ev);
    o.video_root = blocker.join("video");
    o.shm_dir = blocker.join("shm");
    o.stable_ms = 10_000;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start(); // no panic, no Err: the spawn went ahead
    assert_eq!(be.len(), 1);
    let sid = first_session(&sup);
    let dir = blocker.join("video").join("cam0").join(&sid);
    assert!(!dir.exists());
    assert!(be
        .get(0)
        .joined()
        .contains(&dir.to_string_lossy().into_owned()));
    assert!(has_event(&ev, &format!("(session {}", &sid[..8])));
    assert_eq!(sup.status()[0].state, CaptureState::Starting);

    // gst fails on the missing dir → the normal ladder, with a NEW session.
    be.last().exit(Some(1));
    delay(20).await;
    assert_eq!(sup.status()[0].state, CaptureState::Retrying);
    assert!(has_event(
        &ev,
        "video capture cam0 exited (code 1); retry 1/3 in 2000ms"
    ));
    delay(2_000).await;
    assert_eq!(be.len(), 2);
    assert_ne!(first_session(&sup), sid);
    sup.stop();
}

// ---------------------------------------------------------------------------
// describe("VideoCaptureSupervisor progress watchdog") ×7

fn watchdog(
    home: &Home,
    be: Arc<FakeBackend>,
    ev: &Events,
    stall_ms: u64,
) -> VideoCaptureSupervisor {
    let mut o = opts(home, cam0(), true, be, ev);
    o.stable_ms = 10;
    o.sweep_poll_ms = 15;
    o.stall_ms = stall_ms;
    VideoCaptureSupervisor::new(o)
}

#[tokio::test(start_paused = true)]
async fn kills_a_live_but_silent_child_and_stops_reporting_recording() {
    let home = Home::new("silent");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = watchdog(&home, be.clone(), &ev, 120);
    sup.start();
    let sid = first_session(&sup);
    delay(40).await;
    assert_eq!(sup.status()[0].state, CaptureState::Running);
    assert!(sup.any_recording());

    delay(180).await; // past stallMs, with sweeps in between
    assert_eq!(be.get(0).stopped(), Some(libc::SIGKILL));
    let evs = ev.lock().unwrap().clone();
    let stall = format!(
        "video capture cam0 stalled: no bytes written for 0s (session {}…) — killing and restarting",
        &sid[..8]
    );
    let i = evs
        .iter()
        .position(|(_, m)| *m == stall)
        .expect("stall event");
    assert_eq!(evs[i].0, ActivityLevel::Warn);
    // The stall is announced before its ladder step.
    assert_eq!(
        evs[i + 1].1,
        "video capture cam0 stalled; retry 1/3 in 2000ms"
    );
    assert!(!sup.any_recording());
    assert!(!sup.status()[0].recording);
    assert_eq!(sup.status()[0].state, CaptureState::Retrying);
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn a_zero_byte_fragment_is_not_progress() {
    let home = Home::new("zero");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = watchdog(&home, be.clone(), &ev, 120);
    sup.start();
    let dir = home.session_dir("cam0", &first_session(&sup));
    // splitmuxsink DOES create the file at spawn; it just never fills it.
    std::fs::write(dir.join("00000.mp4"), b"").unwrap();
    delay(200).await;
    assert_eq!(be.get(0).stopped(), Some(libc::SIGKILL));
    // The final sweep drops the 0-byte fragment, never sidecars it.
    assert!(!dir.join("00000.mp4").exists());
    assert!(!dir.join("00000.json").exists());
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn leaves_a_child_that_keeps_writing_alone() {
    let home = Home::new("writing");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = watchdog(&home, be.clone(), &ev, 120);
    sup.start();
    let dir = home.session_dir("cam0", &first_session(&sup));
    for i in 1..=16 {
        std::fs::write(dir.join("00000.mp4"), vec![1u8; i * 1000]).unwrap();
        delay(30).await;
    }
    assert_eq!(be.get(0).stopped(), None);
    assert!(sup.any_recording());
    assert_eq!(be.len(), 1);
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn counts_a_rotation_as_progress_even_though_the_new_fragment_is_smaller() {
    let home = Home::new("rotate");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = watchdog(&home, be.clone(), &ev, 200);
    sup.start();
    let dir = home.session_dir("cam0", &first_session(&sup));
    std::fs::write(dir.join("00000.mp4"), vec![1u8; 50_000]).unwrap();
    delay(150).await;
    std::fs::write(dir.join("00001.mp4"), vec![1u8; 120]).unwrap();
    delay(120).await;
    assert_eq!(be.get(0).stopped(), None);
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn parks_after_three_consecutive_stalls_instead_of_hot_looping() {
    let home = Home::new("park");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = watchdog(&home, be.clone(), &ev, 60);
    sup.start();
    delay(4_600).await;
    assert!(has_event(&ev, "stalled; retry 1/3"));
    assert!(has_event(&ev, "stalled; retry 2/3"));
    assert!(has_event(
        &ev,
        "video capture cam0 stalled 3x in a row; parking for 60s (sensor mode unsupported? camera wedged?)"
    ));
    assert_eq!(sup.status()[0].state, CaptureState::Failed);
    assert!(!sup.status()[0].recording);
    assert_eq!(be.len(), 3); // three children, not a spawn storm
                             // Camera report (Rust-only): the health says this failure is a STALL
                             // and at which preset, so the report does not send the reader to the
                             // ribbon cable for the known 1080p30 recording stall.
    let (key, health) = sup.health().remove(0);
    assert_eq!(key, "cam0");
    assert!(health.unstable);
    assert_eq!(
        health.stalled_preset.as_deref(),
        Some("1080p15-2mbps"),
        "a watchdog kill, with the preset it ran"
    );
    assert_eq!(
        xorgate_core::camera_truth::capture_truth(Some(&health), false),
        xorgate_core::camera_truth::CaptureTruth::Failing
    );
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn reaps_the_killed_childs_orphaned_shmsink_segments_and_only_its_own() {
    let home = Home::new("shm");
    let shm = home.0.join("devshm");
    std::fs::create_dir_all(&shm).unwrap();
    // shmsink's REAL names ("shmpipe.%5d.%5d", gst shmpipe.c:307): a 4-digit
    // pid is space-padded (D41; the TS test's unpadded "shmpipe.4242.    0"
    // is a name shmsink never writes, which is how the TS bug hid).
    for n in [
        "shmpipe. 4242.    0",
        "shmpipe. 4242.    1",
        "shmpipe. 9999.    0",
        "shmpipe.14242.    0",
        "lttng-ust-wait-8",
    ] {
        std::fs::write(shm.join(n), [0u8; 8]).unwrap();
    }
    let be = Arc::new(FakeBackend::with_pid(4242));
    let ev = Events::default();
    let sup = watchdog(&home, be.clone(), &ev, 120);
    sup.start();
    delay(220).await; // stall → SIGKILL → onExit reaps
    assert_eq!(be.get(0).stopped(), Some(libc::SIGKILL));
    let mut left: Vec<String> = std::fs::read_dir(&shm)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "lttng-ust-wait-8",
            "shmpipe. 9999.    0",
            "shmpipe.14242.    0"
        ]
    );
    sup.stop();
}

/// Camera report (Rust-only, bench 2 2026-11-21): a child that printed a
/// fatal line (`Camera frontend has timed out!`, here with libcamera's
/// colour codes) and stays ALIVE is not delivering. The report reads it as
/// failing from that moment, with that line as the error, in plain text.
/// The supervisor itself does nothing different: the child is not
/// signalled, it is still promoted to `running` at the stable window, and
/// no event, restart or counter moves.
#[tokio::test(start_paused = true)]
async fn a_live_child_that_printed_a_fatal_line_is_not_streaming_for_the_report() {
    use xorgate_core::camera_truth::{capture_truth, CaptureTruth};
    let truth = |sup: &VideoCaptureSupervisor| {
        let (_, h) = sup.health().remove(0);
        (capture_truth(Some(&h), false), h)
    };
    let home = Home::new("fatal");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let mut o = opts(&home, cam0(), false, be.clone(), &ev);
    o.stable_ms = 50;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    delay(10).await;
    assert_eq!(truth(&sup).0, CaptureTruth::Starting);
    let events_before = ev.lock().unwrap().len();
    // One second after start on the bench; here before the stable window.
    be.get(0).stdout(
        "[0:00:23.561531467] [1236] \u{1b}[1;31mERROR \u{1b}[1;37mRPI \u{1b}[1;34mpipeline_base.cpp:1408 \u{1b}[0mCamera frontend has timed out!",
    );
    // … and its two lines of advice, which must not replace it.
    be.get(0).stdout("[0:00:23.561544158] [1236] ERROR RPI pipeline_base.cpp:1409 Please check that your camera sensor connector is attached securely.");
    be.get(0).stdout("[0:00:23.561547992] [1236] ERROR RPI pipeline_base.cpp:1410 Alternatively, try another cable and/or sensor.");
    delay(5).await;
    let (state, h) = truth(&sup);
    assert_eq!(
        state,
        CaptureTruth::Failing,
        "from the moment it is printed"
    );
    assert_eq!(
        h.last_error.as_deref(),
        Some("[0:00:23.561531467] [1236] ERROR RPI pipeline_base.cpp:1408 Camera frontend has timed out!"),
        "plain text, the fatal line"
    );
    // Long past the stable window and past the recovery time: the child is
    // alive, the supervisor calls it `running`, the report still does not.
    delay(200_000).await;
    assert_eq!(sup.status()[0].state, CaptureState::Running);
    assert_eq!(be.get(0).stopped(), None, "the child is not signalled");
    assert_eq!(be.len(), 1, "no restart");
    assert_eq!(ev.lock().unwrap().len(), events_before, "no ladder event");
    let (state, h) = truth(&sup);
    assert_eq!(state, CaptureTruth::Failing);
    assert_eq!(h.stable_ms, None);
    assert!(h.unstable);
    sup.stop();

    // The control: the same child without the line reads `streaming`.
    let home = Home::new("fatal-control");
    let be = Arc::new(FakeBackend::default());
    let mut o = opts(&home, cam0(), false, be.clone(), &ev);
    o.stable_ms = 50;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    be.get(0)
        .stdout("[0:00:23.5] [1236]  INFO Camera camera.cpp:1215 configuring streams");
    delay(200).await;
    assert_eq!(truth(&sup).0, CaptureTruth::Streaming);
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn bridge_only_mode_has_no_disk_signal_and_is_never_watchdogged() {
    let home = Home::new("bridgewd");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let mut o = opts(&home, cam0(), false, be.clone(), &ev);
    o.stable_ms = 10;
    o.sweep_poll_ms = 15;
    o.stall_ms = 60;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    delay(200).await;
    assert_eq!(be.get(0).stopped(), None);
    assert!(!has_event(&ev, "stalled"));
    sup.stop();
}

// ---------------------------------------------------------------------------
// describe("camera spawn stagger (Phase 2 staged boot)") ×3

fn staggered(
    home: &Home,
    be: Arc<FakeBackend>,
    ev: &Events,
    stagger_ms: u64,
) -> VideoCaptureSupervisor {
    let mut o = opts(home, two_cams(), true, be, ev);
    o.stagger_ms = stagger_ms;
    o.stable_ms = 3_600_000;
    o.sweep_poll_ms = 3_600_000;
    VideoCaptureSupervisor::new(o)
}

#[tokio::test(start_paused = true)]
async fn offsets_the_second_camera_by_stagger_ms() {
    let home = Home::new("stagger");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = staggered(&home, be.clone(), &ev, 60);
    sup.start();
    assert_eq!(be.len(), 1);
    assert!(be.get(0).joined().contains("cam0@1a"));
    let cam1 = sup
        .status()
        .into_iter()
        .find(|s| s.stream_key == "cam1")
        .unwrap();
    assert_eq!(cam1.state, CaptureState::Starting);
    delay(59).await;
    assert_eq!(be.len(), 1);
    delay(61).await;
    assert_eq!(be.len(), 2);
    assert!(be.get(1).joined().contains("cam1@1a"));
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn stop_before_the_stagger_elapses_cancels_the_pending_spawn() {
    let home = Home::new("stagstop");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = staggered(&home, be.clone(), &ev, 60);
    sup.start();
    sup.stop();
    delay(120).await;
    assert_eq!(be.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn stagger_ms_0_spawns_all_cameras_synchronously() {
    let home = Home::new("stag0");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = staggered(&home, be.clone(), &ev, 0);
    sup.start();
    assert_eq!(be.len(), 2);
    sup.stop();
}

// ---------------------------------------------------------------------------
// describe("VideoCaptureSupervisor.setCameras (Phase 6)") ×3

fn two(home: &Home, be: Arc<FakeBackend>, ev: &Events) -> VideoCaptureSupervisor {
    let mut o = opts(home, two_cams(), true, be, ev);
    o.stable_ms = 3_600_000;
    o.sweep_poll_ms = 3_600_000;
    VideoCaptureSupervisor::new(o)
}

#[tokio::test(start_paused = true)]
async fn removes_a_camera_cleanly_and_leaves_the_rest_alone() {
    let home = Home::new("remove");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = two(&home, be.clone(), &ev);
    sup.start();
    let cam0_sid = sup.status()[0].session_id.clone();
    let dir = home.session_dir("cam1", sup.status()[1].session_id.as_deref().unwrap());
    std::fs::write(dir.join("00000.mp4"), vec![1u8; 64]).unwrap();

    sup.set_cameras(&two_cams()[..1]).await;
    assert_eq!(be.get(1).stopped(), Some(libc::SIGINT)); // cam1 EOS-stopped
    assert_eq!(be.get(0).stopped(), None); // cam0 untouched
    let st = sup.status();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].session_id, cam0_sid);
    assert!(dir.join("00000.json").exists());
    assert_eq!(read_json(&dir.join("00000.json"))["finalized"], true);
    assert!(has_event(
        &ev,
        "video capture stopped: cam1 (camera disabled)"
    ));
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn adds_a_camera_without_disturbing_the_running_one() {
    let home = Home::new("add");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = two(&home, be.clone(), &ev);
    sup.start();
    sup.set_cameras(&two_cams()[..1]).await;
    let cam0_sid = sup.status()[0].session_id.clone();
    sup.set_cameras(&two_cams()).await;
    let mut keys: Vec<String> = sup.status().into_iter().map(|s| s.stream_key).collect();
    keys.sort();
    assert_eq!(keys, ["cam0", "cam1"]);
    let cam0 = sup
        .status()
        .into_iter()
        .find(|s| s.stream_key == "cam0")
        .unwrap();
    assert_eq!(cam0.session_id, cam0_sid);
    assert_eq!(be.len(), 3); // cam0, cam1, cam1-again
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn set_cameras_is_inert_after_stop() {
    let home = Home::new("inert");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let sup = two(&home, be.clone(), &ev);
    sup.start();
    sup.stop();
    sup.set_cameras(&two_cams()).await;
    assert_eq!(be.len(), 2);
}

// ---------------------------------------------------------------------------
// extras (not in the TS suite)

/// D41 agent-exit cleanup (orchestrator ruling 2026-11-06), REAL process: a
/// capture child that ignores SIGINT is still running when the agent exits
/// (the rec-disk rv5 race: its EOS outlived the agent). The exit cleanup
/// SIGKILLs it, waits, and unlinks its padded shmsink segment; another live
/// owner's segment stays. Before the cleanup existed the runtime drop's
/// `kill_on_drop` killed it with nobody left to reap, and the segment stayed.
#[tokio::test]
async fn d41_a_capture_still_running_at_exit_is_killed_and_its_segment_unlinked() {
    let home = Home::new("exit-reap");
    let devshm = home.0.join("devshm");
    std::fs::create_dir_all(&devshm).unwrap();
    // A live foreign owner (this test process) that must survive.
    let me = format!("shmpipe.{:>5}.    0", std::process::id());
    std::fs::write(devshm.join(&me), b"x").unwrap();
    let script = home.0.join("fake-gst.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntrap '' INT\nname=$(printf 'shmpipe.%5d.    0' $$)\n: > '{}'/\"$name\"\nwhile :; do sleep 0.05; done\n",
            devshm.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ev = Events::default();
    let mut o = opts(&home, cam0(), true, Arc::new(FakeBackend::default()), &ev);
    o.backend = Arc::new(GstLaunchBackend::with_program(script.display().to_string()));
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let child_seg = || {
        std::fs::read_dir(&devshm)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .find(|n| n != &me && n.starts_with("shmpipe."))
    };
    let t0 = std::time::Instant::now();
    let name = loop {
        if let Some(n) = child_seg() {
            break n;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the child never wrote its segment"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let pid = xorgate_core::capture_ladder::shm_owner_pid(&name).expect("a shmsink name");
    // shmsink's own "%5d" form (space-padded below 10000; the pid width
    // depends on the host — padded matching is pinned by the reaper's tests).
    assert_eq!(name, format!("shmpipe.{pid:>5}.    0"));
    // Two fragments on disk (the newest one is the child's open file).
    let sid = first_session(&sup);
    let sdir = home.session_dir("cam0", &sid);
    std::fs::write(sdir.join("00000.mp4"), vec![1u8; 4096]).unwrap();
    std::fs::write(sdir.join("00001.mp4"), vec![1u8; 2048]).unwrap();
    // The shutdown `stop()`: SIGINT, ignored — the child is still running.
    sup.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        xorgate_hal::shm_reaper::pid_alive(pid),
        "still running at exit"
    );
    assert!(devshm.join(&name).exists());
    // The agent-exit cleanup.
    let t1 = std::time::Instant::now();
    let killed = sup.kill_and_reap_at_exit(Duration::from_millis(500)).await;
    assert!(
        t1.elapsed() <= Duration::from_millis(600),
        "bounded: {:?}",
        t1.elapsed()
    );
    assert_eq!(
        killed,
        vec![("cam0".to_string(), vec![name.clone()])],
        "the real count"
    );
    assert!(
        !devshm.join(&name).exists(),
        "the child's segment is unlinked"
    );
    assert!(
        !xorgate_hal::shm_reaper::pid_alive(pid),
        "the child is gone"
    );
    assert!(
        devshm.join(&me).exists(),
        "a live owner's segment is never touched"
    );
    // Idempotent: nothing left to kill.
    assert!(sup
        .kill_and_reap_at_exit(Duration::from_millis(500))
        .await
        .is_empty());
    // review-wp5-r2 R-WP5R-1: the exit cleanup did NOT finalise anything —
    // the newest (killed mid-EOS) fragment has NO sidecar at exit, exactly as
    // when the runtime drop killed it unobserved (and as the TS) …
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !sdir.join("00001.json").exists(),
        "no newest-fragment sidecar at exit"
    );
    // … so the next boot's recovery stamps it finalized:false.
    let rec = recover_orphan_video_sessions(
        &home.video(),
        &RecoverOpts {
            active_session_ids: &HashSet::new(),
            clock: &AgentClock::following_tokio(1_760_000_000_000),
        },
    );
    assert!(rec.recovered >= 1, "recovered {}", rec.recovered);
    assert_eq!(
        read_json(&sdir.join("00001.json"))["finalized"],
        Value::Bool(false)
    );
    assert!(
        !has_event(&ev, "exited"),
        "no capture event from the exit cleanup's onExit: {:?}",
        ev.lock().unwrap()
    );
}

/// D41 agent-exit cleanup, the timeout arm: a child whose exit is never
/// observed within the bound (a fake that ignores every signal) still has its
/// segment unlinked after the bounded wait — the explicit reap, not only the
/// `onExit` one.
#[tokio::test(start_paused = true)]
async fn d41_exit_cleanup_unlinks_even_when_the_exit_is_not_observed_in_the_bound() {
    struct Deaf {
        inner: FakeBackend,
    }
    struct DeafChild(Arc<Entry>);
    impl CaptureChild for DeafChild {
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }
        fn signal(&self, sig: i32) {
            *self.0.stopped.lock().unwrap() = Some(sig); // never exits
        }
        fn renice(&self, _n: i32) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl CaptureBackend for Deaf {
        fn program(&self) -> &str {
            "gst-launch-1.0"
        }
        fn spawn(
            &self,
            args: &[String],
        ) -> (Box<dyn CaptureChild>, mpsc::UnboundedReceiver<ChildEvent>) {
            let (_c, rx) = self.inner.spawn(args);
            (Box::new(DeafChild(self.inner.last())), rx)
        }
    }
    let home = Home::new("exit-reap-deaf");
    let devshm = home.0.join("devshm");
    std::fs::create_dir_all(&devshm).unwrap();
    std::fs::write(devshm.join("shmpipe. 4242.    0"), b"x").unwrap();
    std::fs::write(devshm.join("shmpipe. 4243.    0"), b"x").unwrap();
    let be = Arc::new(Deaf {
        inner: FakeBackend::default(),
    });
    let ev = Events::default();
    let mut o = opts(&home, cam0(), true, Arc::new(FakeBackend::default()), &ev);
    o.backend = be.clone();
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    sup.stop();
    let t0 = tokio::time::Instant::now();
    let killed = sup.kill_and_reap_at_exit(Duration::from_millis(500)).await;
    assert_eq!(
        t0.elapsed(),
        Duration::from_millis(500),
        "waited exactly the bound"
    );
    assert_eq!(
        *be.inner.get(0).stopped.lock().unwrap(),
        Some(libc::SIGKILL)
    );
    assert_eq!(
        killed,
        vec![("cam0".to_string(), vec!["shmpipe. 4242.    0".to_string()])]
    );
    assert!(!devshm.join("shmpipe. 4242.    0").exists());
    assert!(
        devshm.join("shmpipe. 4243.    0").exists(),
        "only its own pid"
    );
}

/// `stopAndWait`: a child that ignores SIGINT is SIGKILLed after the grace,
/// and its shm segments are reaped once it is gone.
#[tokio::test(start_paused = true)]
async fn stop_and_wait_escalates_to_sigkill_after_the_grace() {
    struct Stubborn {
        inner: FakeBackend,
    }
    struct StubbornChild(Arc<Entry>);
    impl CaptureChild for StubbornChild {
        fn pid(&self) -> Option<u32> {
            Some(777)
        }
        fn signal(&self, sig: i32) {
            *self.0.stopped.lock().unwrap() = Some(sig);
            if sig == libc::SIGKILL {
                self.0.exit(None);
            }
        }
        fn renice(&self, _n: i32) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl CaptureBackend for Stubborn {
        fn program(&self) -> &str {
            "gst-launch-1.0"
        }
        fn spawn(
            &self,
            args: &[String],
        ) -> (Box<dyn CaptureChild>, mpsc::UnboundedReceiver<ChildEvent>) {
            let (_c, rx) = self.inner.spawn(args);
            (Box::new(StubbornChild(self.inner.last())), rx)
        }
    }
    let home = Home::new("saw");
    let shm = home.0.join("devshm");
    std::fs::create_dir_all(&shm).unwrap();
    std::fs::write(shm.join("shmpipe.  777.    0"), b"x").unwrap();
    let be = Arc::new(Stubborn {
        inner: FakeBackend::default(),
    });
    let ev = Events::default();
    let mut o = opts(&home, cam0(), true, Arc::new(FakeBackend::default()), &ev);
    o.backend = be.clone();
    o.stable_ms = 10;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let t0 = tokio::time::Instant::now();
    sup.stop_and_wait(Duration::from_millis(10_000)).await;
    assert!(t0.elapsed() >= Duration::from_millis(10_000));
    assert_eq!(be.inner.get(0).stopped(), Some(libc::SIGKILL));
    assert!(!shm.join("shmpipe.  777.    0").exists());
    assert_eq!(sup.status()[0].state, CaptureState::Stopped);
}

/// `cycle()` in real mode: SIGINT, the last fragment finalised (clean), and
/// the respawn follows the normal ladder (no `respawnHoldMs`).
#[tokio::test(start_paused = true)]
async fn cycle_finalizes_and_respawns_on_the_ladder() {
    let home = Home::new("cycle");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let mut o = opts(&home, cam0(), true, be.clone(), &ev);
    o.stable_ms = 50;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let sid = first_session(&sup);
    let dir = home.session_dir("cam0", &sid);
    std::fs::write(dir.join("00000.mp4"), vec![1u8; 10]).unwrap();
    delay(80).await;
    sup.cycle_all();
    delay(10).await;
    assert_eq!(be.get(0).stopped(), Some(libc::SIGINT));
    assert_eq!(read_json(&dir.join("00000.json"))["finalized"], true);
    delay(2_000).await;
    assert_eq!(be.len(), 2);
    assert_ne!(first_session(&sup), sid);
    sup.stop();
}

/// The inotify watcher (real clock): a rotation is sidecar'd at once, long
/// before the 10 s poll.
#[tokio::test]
async fn fs_watch_sweeps_on_rotation() {
    let home = Home::new("inotify");
    let be = Arc::new(FakeBackend::default());
    let ev = Events::default();
    let closed = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let mut o = opts(&home, cam0(), true, be.clone(), &ev);
    o.fs_watch = true;
    o.stable_ms = 3_600_000;
    o.sweep_poll_ms = 3_600_000;
    let c = closed.clone();
    o.on_segment_closed = Some(Arc::new(move |p| c.lock().unwrap().push(p.to_path_buf())));
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let dir = home.session_dir("cam0", &first_session(&sup));
    std::fs::write(dir.join("00000.mp4"), vec![1u8; 100]).unwrap();
    std::fs::write(dir.join("00001.mp4"), vec![1u8; 10]).unwrap();
    for _ in 0..100 {
        if dir.join("00000.json").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(dir.join("00000.json").exists());
    assert_eq!(closed.lock().unwrap().first(), Some(&dir.join("00000.mp4")));
    sup.stop();
}

/// `recoverOrphanVideoSessions`: every non-active session with a manifest
/// passing the TS rule is backfilled (newest `finalized:false`, 0-byte
/// dropped); a manifest that is TS-valid but untyped (CF28) is swept too and
/// its oddities ride into the sidecar; no/invalid manifest → untouched.
#[test]
fn recover_orphans_backfills_with_the_loose_ts_rule() {
    let home = Home::new("recover");
    let root = home.video();
    let typed = root.join("cam0").join("s-typed");
    let loose = root.join("cam0").join("s-loose");
    let bad = root.join("cam1").join("s-bad");
    let active = root.join("cam1").join("s-active");
    for d in [&typed, &loose, &bad, &active] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(root.join("stray-file"), b"x").unwrap();
    std::fs::write(
        typed.join("session.json"),
        r#"{"v":1,"deviceId":"dev-1","streamKey":"cam0","sessionId":"s-typed","bootId":"b","timeSource":"rtc","preset":"720p15-1mbps","width":1280,"height":720,"fps":15,"bitrateKbps":1000,"segmentSeconds":60,"anchorTs":1760000000000,"anchorMono":"1"}"#,
    )
    .unwrap();
    // TS-valid (v:1, string sessionId, numeric anchorTs) but not the typed
    // shape: no deviceId, segmentSeconds as a string, an unknown timeSource.
    std::fs::write(
        loose.join("session.json"),
        r#"{"v":1,"streamKey":"cam0","sessionId":"s-loose","timeSource":"sundial","width":640,"segmentSeconds":"30","anchorTs":1760000000000,"extra":true}"#,
    )
    .unwrap();
    std::fs::write(
        bad.join("session.json"),
        r#"{"v":2,"sessionId":"s-bad","anchorTs":1}"#,
    )
    .unwrap();
    std::fs::write(
        active.join("session.json"),
        std::fs::read(typed.join("session.json")).unwrap(),
    )
    .unwrap();
    for d in [&typed, &loose, &bad, &active] {
        std::fs::write(d.join("00000.mp4"), vec![1u8; 10]).unwrap();
        std::fs::write(d.join("00001.mp4"), vec![1u8; 10]).unwrap();
        std::fs::write(d.join("00002.mp4"), b"").unwrap();
    }
    let clock = AgentClock::system();
    let active_ids: HashSet<String> = ["s-active".to_string()].into();
    let res = recover_orphan_video_sessions(
        &root,
        &RecoverOpts {
            active_session_ids: &active_ids,
            clock: &clock,
        },
    );
    assert_eq!(res.recovered, 4);
    assert_eq!(res.reaped.len(), 2);
    let t1 = read_json(&typed.join("00001.json"));
    // TS quirk kept: `finalized = !isNewest` keys on the NEWEST NAME, which
    // is the 0-byte 00002 (dropped) — so 00001 reads finalized:true.
    assert_eq!(t1["finalized"], true);
    assert_eq!(t1["timeSource"], "rtc"); // manifest value (no live tracker)
    assert_eq!(read_json(&typed.join("00000.json"))["finalized"], true);
    assert!(!typed.join("00002.mp4").exists());
    let l0 = read_json(&loose.join("00000.json"));
    assert_eq!(l0["timeSource"], "sundial");
    assert_eq!(l0["width"], 640);
    assert!(l0.get("deviceId").is_none());
    assert!(l0.get("height").is_none());
    for d in [&bad, &active] {
        assert!(!d.join("00000.json").exists());
        assert!(d.join("00002.mp4").exists());
    }
    // No tree yet → 0.
    let none = recover_orphan_video_sessions(
        &home.0.join("absent"),
        &RecoverOpts {
            active_session_ids: &HashSet::new(),
            clock: &clock,
        },
    );
    assert_eq!(none.recovered, 0);
}

// ---------------------------------------------------------------------------
// tape hooks (phase-3 §Record/replay, role "capture")

/// A tape that records every Phase 3 call as one compact line.
#[derive(Default)]
struct RecTape(Mutex<Vec<String>>);

impl RecTape {
    fn push(&self, s: String) {
        self.0.lock().unwrap().push(s);
    }
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl xorgate_hal::io::TapeSink for RecTape {
    fn enabled(&self) -> bool {
        true
    }
    fn child_spawn(&self, r: &xorgate_hal::io::ChildSpawnRec) {
        self.push(format!(
            "spawn {} {} pid={} nice={:?} argv0={} n={}",
            r.role,
            r.key,
            r.pid,
            r.nice,
            r.argv[0],
            r.argv.len()
        ));
    }
    fn child_line(&self, role: &str, key: &str, fd: u8, line: &str) {
        self.push(format!("line {role} {key} {fd} {line}"));
    }
    fn child_signal(&self, role: &str, key: &str, pid: u32, sig: i32) {
        self.push(format!("signal {role} {key} {pid} {sig}"));
    }
    fn child_exit(&self, r: &xorgate_hal::io::ChildExitRec) {
        self.push(format!(
            "exit {} {} pid={} code={:?} external={}",
            r.role, r.key, r.pid, r.code, r.external
        ));
    }
    fn fs_out(&self, rel: &str, op: xorgate_hal::io::FsOp, size: u64) {
        self.push(format!("fs_out {} {rel} {size}", op.as_str()));
    }
    fn shm(&self, names: &[String]) {
        self.push(format!("shm {names:?}"));
    }
}

#[tokio::test(start_paused = true)]
async fn tape_hooks_report_spawn_lines_signals_exits_shm_and_fragments() {
    let home = Home::new("tape");
    let be = Arc::new(FakeBackend::with_pid(4242));
    let ev = Events::default();
    let tape = Arc::new(RecTape::default());
    let mut o = opts(&home, cam0(), true, be.clone(), &ev);
    o.stable_ms = 50;
    o.sweep_poll_ms = 1_000;
    o.home = Some(home.0.clone());
    o.tape = tape.clone();
    let devshm = home.0.join("devshm");
    std::fs::create_dir_all(&devshm).unwrap();
    std::fs::write(devshm.join("shmpipe. 4242.    0"), b"x").unwrap();
    std::fs::write(devshm.join("shmpipe. 9999.    0"), b"x").unwrap();
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let sid = first_session(&sup);
    let rel = format!("recordings/video/cam0/{sid}");
    let dir = home.session_dir("cam0", &sid);
    let argc = be.get(0).args.len() + 1;

    let _ = be
        .get(0)
        .tx
        .send(ChildEvent::Stdout("New clock: GstSystemClock".into()));
    let _ = be
        .get(0)
        .tx
        .send(ChildEvent::Stderr("WARN something".into()));
    std::fs::write(dir.join("00000.mp4"), vec![1u8; 100]).unwrap();
    delay(1_050).await; // first poll sweep: 00000 is the active fragment
    std::fs::write(dir.join("00001.mp4"), vec![1u8; 30]).unwrap();
    delay(1_000).await; // second sweep: 00000 closes, 00001 is active
    let sc0 = std::fs::metadata(dir.join("00000.json")).unwrap().len();
    // An external crash: not agent-signalled.
    be.get(0).exit(Some(1));
    delay(20).await;
    let sc1 = std::fs::metadata(dir.join("00001.json")).unwrap().len();
    delay(2_000).await; // backoff respawn
    assert_eq!(be.len(), 2);
    sup.stop();
    delay(20).await;

    let lines = tape.lines();
    let want: Vec<String> = vec![
        format!("spawn capture cam0 pid=4242 nice=Some(10) argv0=gst-launch-1.0 n={argc}"),
        "line capture cam0 1 New clock: GstSystemClock".into(),
        "line capture cam0 2 WARN something".into(),
        format!("fs_out create {rel}/00000.mp4 100"),
        format!("fs_out close {rel}/00000.mp4 100"),
        format!("fs_out sidecar {rel}/00000.json {sc0}"),
        format!("fs_out create {rel}/00001.mp4 30"),
        "exit capture cam0 pid=4242 code=Some(1) external=true".into(),
        // The dead child's own segment is reaped first; the JS-owned one stays.
        r#"shm ["shmpipe. 9999.    0"]"#.into(),
        format!("fs_out close {rel}/00001.mp4 30"),
        format!("fs_out sidecar {rel}/00001.json {sc1}"),
        format!("spawn capture cam0 pid=4242 nice=Some(10) argv0=gst-launch-1.0 n={argc}"),
        format!("signal capture cam0 4242 {}", libc::SIGINT),
        "exit capture cam0 pid=4242 code=Some(0) external=false".into(),
        r#"shm ["shmpipe. 9999.    0"]"#.into(),
    ];
    assert_eq!(lines, want);
}

// ---------------------------------------------------------------------------
// describe("VideoCaptureSupervisor replay hooks (device-simulator Phase 2)") ×5
// + Phase 7e pins

/// The TS hook options as one [`ReplayCaptureHooks`]: `sourceHead` from a
/// queue (else a fixed head), `onPlaying`/`onEnded` recorded, `respawnHoldMs`
/// from a cell, `eosIsClean` fixed. `on_ended` also records whether the
/// ending session's `00000.json` already exists (the final sweep ran first).
struct TestHooks {
    heads: Mutex<std::collections::VecDeque<Vec<String>>>,
    playing: Mutex<Vec<(String, i64)>>,
    ended: Mutex<Vec<bool>>,
    sidecar_at_end: Mutex<Vec<bool>>,
    watch: Mutex<Option<PathBuf>>,
    hold: Mutex<Option<u64>>,
    eos_clean: bool,
    /// `on_ended` / `respawn_hold_ms` call order (review-wp1 F5).
    calls: Mutex<Vec<&'static str>>,
}

impl TestHooks {
    fn new(hold: Option<u64>, eos_clean: bool) -> Arc<Self> {
        Arc::new(Self {
            heads: Mutex::default(),
            playing: Mutex::default(),
            ended: Mutex::default(),
            sidecar_at_end: Mutex::default(),
            watch: Mutex::default(),
            hold: Mutex::new(hold),
            eos_clean,
            calls: Mutex::default(),
        })
    }
    fn ended(&self) -> Vec<bool> {
        self.ended.lock().unwrap().clone()
    }
    fn playing(&self) -> Vec<String> {
        self.playing
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }
}

impl ReplayCaptureHooks for TestHooks {
    fn source_head(&self, _cam: &CaptureCamera) -> Vec<String> {
        self.heads
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec!["filesrc".into(), "location=/a/lap.h264".into()])
    }
    fn on_playing(&self, key: &str, wall_ms: i64) {
        self.playing
            .lock()
            .unwrap()
            .push((key.to_string(), wall_ms));
    }
    fn on_ended(&self, _key: &str, clean: bool) {
        self.calls.lock().unwrap().push("ended");
        self.ended.lock().unwrap().push(clean);
        if let Some(d) = self.watch.lock().unwrap().as_ref() {
            self.sidecar_at_end
                .lock()
                .unwrap()
                .push(d.join("00000.json").exists());
        }
    }
    fn respawn_hold_ms(&self, _key: &str) -> Option<u64> {
        self.calls.lock().unwrap().push("hold");
        *self.hold.lock().unwrap()
    }
    fn eos_is_clean(&self) -> bool {
        self.eos_clean
    }
}

fn replay_cams() -> Vec<CaptureCamera> {
    vec![cam("cam0", "replay")]
}

/// "sourceHead replaces the libcamerasrc head on every spawn and nothing after the tee"
#[tokio::test(start_paused = true)]
async fn replay_source_head_replaces_libcamerasrc_on_every_spawn() {
    let home = Home::new("rp-head");
    let be = Arc::new(FakeBackend {
        eos_on_signal: true,
        ..FakeBackend::default()
    });
    let ev = Events::default();
    let h = TestHooks::new(Some(0), true);
    h.heads.lock().unwrap().extend([
        vec!["filesrc".to_string(), "location=/a/lap.h264".to_string()],
        vec!["filesrc".to_string(), "location=/b/lap.h264".to_string()],
    ]);
    let mut o = opts(&home, replay_cams(), true, be.clone(), &ev);
    o.replay = Some(h.clone());
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    assert_eq!(be.get(0).args[1..3], ["filesrc", "location=/a/lap.h264"]);
    assert!(!be.get(0).joined().contains("libcamerasrc"));
    assert!(be.get(0).joined().contains("splitmuxsink"));
    // Everything after the head is the unchanged builder argv.
    let real = {
        let be2 = Arc::new(FakeBackend::default());
        let s2 = VideoCaptureSupervisor::new(opts(&home, replay_cams(), true, be2.clone(), &ev));
        s2.start();
        let a = be2.get(0).args.clone();
        s2.stop();
        a
    };
    let tail = |a: &[String]| {
        a.iter()
            .skip_while(|t| *t != "!")
            .cloned()
            .collect::<Vec<_>>()
    };
    let strip = |v: Vec<String>| {
        v.into_iter()
            .map(|t| {
                if t.starts_with("location=") && t.ends_with("%05d.mp4") {
                    "location=<dir>".into()
                } else {
                    t
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(tail(&be.get(0).args)), strip(tail(&real)));
    // Re-evaluated on the next spawn (a cycle respawns with hold 0).
    sup.cycle_all();
    delay(5).await;
    assert_eq!(be.len(), 2);
    assert_eq!(be.get(1).args[1..3], ["filesrc", "location=/b/lap.h264"]);
    sup.stop();
}

/// "reports PLAYING once per child and a clean EOS end; respawns after the hold as a new session"
#[tokio::test(start_paused = true)]
async fn replay_playing_once_per_child_clean_eos_end_and_respawn_after_the_hold() {
    let home = Home::new("rp-eos");
    let be = Arc::new(FakeBackend {
        eos_on_signal: true,
        ..FakeBackend::default()
    });
    let ev = Events::default();
    let h = TestHooks::new(Some(30), true);
    let mut o = opts(&home, replay_cams(), true, be.clone(), &ev);
    o.stable_ms = 10;
    o.sweep_poll_ms = 3_600_000;
    o.replay = Some(h.clone());
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let first = first_session(&sup);
    let dir = home.session_dir("cam0", &first);
    *h.watch.lock().unwrap() = Some(dir.clone());
    be.get(0).stdout("Setting pipeline to PLAYING ...");
    be.get(0).stdout("New clock: GstSystemClock");
    be.get(0).stdout("New clock: GstSystemClock"); // a second print is not a second lap
    delay(1).await;
    assert_eq!(h.playing(), ["cam0"]);
    assert!(
        h.playing.lock().unwrap()[0].1 >= 1_760_000_000_000,
        "Date.now() at the line"
    );
    // Natural EOF: EOS on stdout, then the shmsink teardown's exit code 1 (Phase 0 surprise 5).
    std::fs::write(dir.join("00000.mp4"), "x").unwrap();
    be.get(0).stdout("Got EOS from element \"pipeline0\".");
    be.get(0).exit(Some(1));
    delay(10).await;
    assert_eq!(h.ended(), [true]);
    assert_eq!(
        *h.sidecar_at_end.lock().unwrap(),
        [true],
        "on_ended runs after the final sweep"
    );
    assert_eq!(
        *h.calls.lock().unwrap(),
        ["ended", "hold"],
        "the TS order: onEnded, then respawnHoldMs (review-wp1 F5)"
    );
    assert_eq!(read_json(&dir.join("00000.json"))["finalized"], true);
    assert_eq!(sup.status()[0].state, CaptureState::Retrying);
    assert_eq!(be.len(), 1);
    assert!(has_event(
        &ev,
        &format!(
            "video capture cam0 ended cleanly (session {}…); next lap in 0s",
            &first[..8]
        )
    ));
    delay(25).await;
    assert_eq!(be.len(), 2);
    assert_ne!(first_session(&sup), first);
    // The new child reports its own PLAYING once.
    be.get(1).stdout("New clock: GstSystemClock");
    delay(1).await;
    assert_eq!(h.playing(), ["cam0", "cam0"]);
    sup.stop();
}

/// "without eosIsClean an EOF exit is still a crash (real-mode behaviour)"
#[tokio::test(start_paused = true)]
async fn replay_without_eos_is_clean_an_eof_exit_is_still_a_crash() {
    let home = Home::new("rp-noeos");
    let be = Arc::new(FakeBackend {
        eos_on_signal: true,
        ..FakeBackend::default()
    });
    let ev = Events::default();
    let h = TestHooks::new(Some(0), false);
    let mut o = opts(&home, replay_cams(), true, be.clone(), &ev);
    o.stable_ms = 10;
    o.sweep_poll_ms = 3_600_000;
    o.replay = Some(h.clone());
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let dir = home.session_dir("cam0", &first_session(&sup));
    std::fs::write(dir.join("00000.mp4"), "x").unwrap();
    be.get(0).stdout("Got EOS from element \"pipeline0\".");
    be.get(0).exit(Some(1));
    delay(10).await;
    assert_eq!(h.ended(), [false]);
    assert_eq!(read_json(&dir.join("00000.json"))["finalized"], false);
    sup.stop();
}

/// "respawnHoldMs null parks the camera until cycle() spawns it again"
#[tokio::test(start_paused = true)]
async fn replay_hold_none_parks_until_cycle() {
    let home = Home::new("rp-park");
    let be = Arc::new(FakeBackend {
        eos_on_signal: true,
        ..FakeBackend::default()
    });
    let ev = Events::default();
    let h = TestHooks::new(None, true);
    let mut o = opts(&home, replay_cams(), false, be.clone(), &ev);
    o.stable_ms = 10;
    o.replay = Some(h.clone());
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    be.get(0).stdout("Got EOS from element \"pipeline0\".");
    be.get(0).exit(Some(1));
    delay(10).await;
    assert_eq!(sup.status()[0].state, CaptureState::Stopped);
    assert!(has_event(
        &ev,
        "video capture cam0 ended cleanly; parked (no loop)"
    ));
    delay(60_000).await;
    assert_eq!(be.len(), 1, "parked: no respawn");
    *h.hold.lock().unwrap() = Some(0);
    sup.cycle("cam0");
    assert_eq!(be.len(), 2);
    sup.stop();
}

/// "cycle() on a running child SIGINTs it, finalizes, and respawns after the hold"
#[tokio::test(start_paused = true)]
async fn replay_cycle_sigints_finalizes_and_respawns_after_the_hold() {
    let home = Home::new("rp-cycle");
    let be = Arc::new(FakeBackend {
        eos_on_signal: true,
        ..FakeBackend::default()
    });
    let ev = Events::default();
    let h = TestHooks::new(Some(0), true);
    let mut o = opts(&home, replay_cams(), true, be.clone(), &ev);
    o.stable_ms = 10;
    o.sweep_poll_ms = 3_600_000;
    o.replay = Some(h.clone());
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let first = first_session(&sup);
    let dir = home.session_dir("cam0", &first);
    *h.watch.lock().unwrap() = Some(dir.clone());
    std::fs::write(dir.join("00000.mp4"), "x").unwrap();
    sup.cycle_all();
    assert_eq!(be.get(0).stopped(), Some(libc::SIGINT));
    delay(5).await;
    assert_eq!(be.len(), 2);
    assert_eq!(h.ended(), [true]);
    assert_eq!(*h.sidecar_at_end.lock().unwrap(), [true]);
    assert_eq!(read_json(&dir.join("00000.json"))["finalized"], true);
    assert_ne!(first_session(&sup), first);
    sup.stop();
}

/// 7e: a cycled child that prints no EOS line still ends its lap cleanly
/// (`cycled`), and the hold — not the crash ladder — decides the respawn.
/// `on_ended` fires for a stopped supervisor too (P5: the TS `onEnded` runs
/// before the `stopped` check).
#[tokio::test(start_paused = true)]
async fn replay_cycle_uses_the_hold_and_on_ended_fires_on_stop() {
    let home = Home::new("rp-ladder");
    let be = Arc::new(FakeBackend::default()); // stop() prints no EOS
    let ev = Events::default();
    let h = TestHooks::new(Some(5000), true);
    let mut o = opts(&home, replay_cams(), false, be.clone(), &ev);
    o.stable_ms = 10;
    o.replay = Some(h.clone());
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    sup.cycle("cam0");
    delay(5).await;
    assert_eq!(h.ended(), [true]);
    assert_eq!(sup.status()[0].state, CaptureState::Retrying);
    assert!(has_event(
        &ev,
        "video capture cam0 ended cleanly; next lap in 5s"
    ));
    delay(4990).await;
    assert_eq!(be.len(), 1);
    delay(10).await;
    assert_eq!(be.len(), 2);
    sup.stop();
    delay(5).await;
    assert_eq!(
        h.ended(),
        [true, true],
        "onEnded for the stopped supervisor's child"
    );
}

/// D72 (structural bound): `consume` ends at the child's `Done`, which the
/// child module delivers LAST, so a previous child's "late" line is dropped
/// with its channel and never reaches the successor. The child-id keying of
/// `note_stdout` itself is pinned directly by the in-module test
/// `recording::capture::tests::d72_note_stdout_ignores_a_stale_child_id`.
#[tokio::test(start_paused = true)]
async fn a_previous_childs_late_stdout_is_dropped_with_its_channel() {
    let home = Home::new("rp-stale");
    let be = Arc::new(FakeBackend {
        eos_on_signal: true,
        ..FakeBackend::default()
    });
    let ev = Events::default();
    let h = TestHooks::new(Some(0), true);
    let mut o = opts(&home, replay_cams(), false, be.clone(), &ev);
    o.stable_ms = 10;
    o.replay = Some(h.clone());
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    sup.cycle_all();
    delay(5).await;
    assert_eq!(be.len(), 2);
    be.get(0).stdout("New clock: GstSystemClock");
    be.get(0).stdout("Got EOS from element \"pipeline0\".");
    delay(5).await;
    assert!(h.playing().is_empty());
    // The successor's own crash is still a crash: the stale EOS was not counted.
    be.get(1).exit(Some(1));
    delay(5).await;
    assert_eq!(h.ended(), [true, false]);
    sup.stop();
}

/// Real mode (no hooks): gst's PLAYING/EOS lines change nothing — an EOF
/// exit after `Got EOS` is the crash ladder, byte for byte the pre-7e path.
#[tokio::test(start_paused = true)]
async fn real_mode_ignores_gst_stdout_and_an_eof_exit_is_a_crash() {
    let home = Home::new("rp-real");
    let be = Arc::new(FakeBackend {
        eos_on_signal: true,
        ..FakeBackend::default()
    });
    let ev = Events::default();
    let mut o = opts(&home, cam0(), true, be.clone(), &ev);
    o.stable_ms = 50;
    o.sweep_poll_ms = 3_600_000;
    let sup = VideoCaptureSupervisor::new(o);
    sup.start();
    let sid = first_session(&sup);
    let dir = home.session_dir("cam0", &sid);
    std::fs::write(dir.join("00000.mp4"), "x").unwrap();
    be.get(0).stdout("New clock: GstSystemClock");
    be.get(0).stdout("Got EOS from element \"pipeline0\".");
    be.get(0).exit(Some(1));
    delay(10).await;
    assert_eq!(read_json(&dir.join("00000.json"))["finalized"], false);
    assert_eq!(sup.status()[0].state, CaptureState::Retrying);
    // The crash ladder's quick-failure line, exactly (review-wp1 F11).
    let last = ev.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        last,
        (
            ActivityLevel::Warn,
            "video capture cam0 exited (code 1); retry 1/3 in 2000ms".to_string()
        )
    );
    sup.stop();
}
