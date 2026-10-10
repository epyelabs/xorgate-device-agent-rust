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
//! NOT PORTED (P14 / Q6, replay mode): "VideoCaptureSupervisor replay hooks"
//! ×5 — "sourceHead replaces the libcamerasrc head on every spawn and nothing
//! after the tee", "reports PLAYING once per child and a clean EOS end;
//! respawns after the hold as a new session", "without eosIsClean an EOF exit
//! is still a crash (real-mode behaviour)", "respawnHoldMs null parks the
//! camera until cycle() spawns it again", "cycle() on a running child SIGINTs
//! it, finalizes, and respawns after the hold".

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
}

impl Entry {
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
    sup.stop();
}

#[tokio::test(start_paused = true)]
async fn reaps_the_killed_childs_orphaned_shmsink_segments_and_only_its_own() {
    let home = Home::new("shm");
    let shm = home.0.join("devshm");
    std::fs::create_dir_all(&shm).unwrap();
    for n in [
        "shmpipe.4242.    0",
        "shmpipe.4242.    1",
        "shmpipe.9999.    0",
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
    assert_eq!(left, ["lttng-ust-wait-8", "shmpipe.9999.    0"]);
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
    std::fs::write(shm.join("shmpipe.777.    0"), b"x").unwrap();
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
    assert!(!shm.join("shmpipe.777.    0").exists());
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
