//! D86 (F-DA21-KVSLOOP): an absent sensor end to end, the bench-2 shape.
//!
//! The REAL `VideoCaptureSupervisor` (bridge-only, like a device with
//! recording off) and the REAL `MediaSupervisor`, wired the way the worker
//! wires them (`on_spawned` → `restart_channel`, `on_source_absent` →
//! `set_source_absent`), on a paused tokio clock. Fakes stand in for the two
//! child kinds only:
//! - capture: a cam1 child dies 100 ms after spawn with exit 1 and libcamera's
//!   `Remote I/O error` on stderr while the sensor is "absent"; a cam0 child
//!   (and a cam1 child once the sensor "works") runs until signalled;
//! - KVS master: runs until SIGINTed, then exits 0 after 5 ms.
//!
//! Pins: the capture back-off schedule (TS ladder once, then 60 → 120 → 240
//! → 300 s cap, one line per step), the master held while the source is down
//! (no restart loop, `failed` in the media block), automatic recovery when
//! the sensor starts working, and cam0 never disturbed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use xorgate_agent::ctx::AgentClock;
use xorgate_agent::media::{GuardFn, MasterBackend, MasterProc, MediaConfig, MediaSupervisor};
use xorgate_agent::recording::capture::*;
use xorgate_core::capture_ladder::CaptureState;
use xorgate_core::clock::Clock;
use xorgate_core::media_ladder::RuntimeDetect;
use xorgate_core::stream_guard::StreamGuard;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::types::TimeSource;
use xorgate_core::video_presets::resolve_video_preset;
use xorgate_hal::child::{ChildDone, ChildEvent, ChildSpec};
use xorgate_hal::io::null_tape;

fn ms(origin: tokio::time::Instant) -> u64 {
    origin.elapsed().as_millis() as u64
}

// ---------------------------------------------------------------------------
// capture fake

struct CapEntry {
    key: String,
    tx: mpsc::UnboundedSender<ChildEvent>,
    exited: AtomicBool,
}

impl CapEntry {
    fn exit(&self, code: Option<i32>) {
        if !self.exited.swap(true, Ordering::SeqCst) {
            let _ = self.tx.send(ChildEvent::Done(ChildDone {
                exit_code: code,
                failed: code.unwrap_or(0) != 0,
                ..ChildDone::default()
            }));
        }
    }
}

struct CapChild(Arc<CapEntry>);

impl CaptureChild for CapChild {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn signal(&self, _sig: i32) {
        self.0.exit(Some(0));
    }
    fn renice(&self, _nice: i32) -> std::io::Result<()> {
        Ok(())
    }
}

struct CapBackend {
    origin: tokio::time::Instant,
    /// The cam1 sensor is absent (its children die 100 ms after spawn).
    cam1_absent: Arc<AtomicBool>,
    /// When non-zero (and not absent): a cam1 child dies with exit 1 this
    /// many ms after spawn (a camera that RUNS, then crashes).
    cam1_dies_after_ms: Arc<AtomicU64>,
    /// Added to the capture clock's monotonic time when a cam1 child dies
    /// (fakes "it ran past the stable window" without the stable timer).
    cam1_exit_jump_ms: Arc<AtomicU64>,
    clock_offset: Arc<AtomicU64>,
    spawns: Mutex<Vec<(String, u64)>>,
}

impl CaptureBackend for CapBackend {
    fn program(&self) -> &str {
        "gst-launch-1.0"
    }
    fn spawn(
        &self,
        args: &[String],
    ) -> (Box<dyn CaptureChild>, mpsc::UnboundedReceiver<ChildEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let key = if args.iter().any(|a| a.contains("cam1")) {
            "cam1"
        } else {
            "cam0"
        };
        let e = Arc::new(CapEntry {
            key: key.to_string(),
            tx,
            exited: AtomicBool::new(false),
        });
        self.spawns
            .lock()
            .unwrap()
            .push((e.key.clone(), ms(self.origin)));
        let dies = self.cam1_dies_after_ms.load(Ordering::SeqCst);
        if key == "cam1" && self.cam1_absent.load(Ordering::SeqCst) {
            let e2 = e.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                // As on a two-camera bench: the child's first ERROR line is
                // about the OTHER camera (skipped by the camera report), then
                // the cause, then a consequence line.
                for line in [
                    "[0:00:19.158270951] [1114] ERROR V4L2 v4l2_device.cpp:390 'imx290 10-001a': Unable to set controls: Device or resource busy",
                    "ERROR: from element /GstPipeline:pipeline0/GstLibcameraSrc:libcamerasrc0: \
                     Failed to start the camera: Remote I/O error",
                    "ERROR: from element /GstPipeline:pipeline0/GstShmSink:shmsink0: Failed waiting on fd activity",
                ] {
                    let _ = e2.tx.send(ChildEvent::Stderr(line.into()));
                }
                e2.exit(Some(1));
            });
        } else if key == "cam1" && dies > 0 {
            let e2 = e.clone();
            let (jump, off) = (self.cam1_exit_jump_ms.clone(), self.clock_offset.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(dies)).await;
                off.fetch_add(jump.load(Ordering::SeqCst), Ordering::SeqCst);
                e2.exit(Some(1));
            });
        }
        (Box::new(CapChild(e)), rx)
    }
}

/// The tokio-following agent clock plus a jump the test controls.
struct OffsetClock {
    inner: AgentClock,
    offset: Arc<AtomicU64>,
}

impl Clock for OffsetClock {
    fn wall_ms(&self) -> i64 {
        self.inner.wall_ms() + self.offset.load(Ordering::SeqCst) as i64
    }
    fn mono_ms(&self) -> u64 {
        self.inner.mono_ms() + self.offset.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// KVS master fake

#[derive(Default)]
struct MasterLog {
    /// (channel, t) per spawn / per SIGINT.
    spawns: Vec<(String, u64)>,
    signals: Vec<(String, u64)>,
    live: std::collections::HashMap<String, bool>,
}

struct MProc {
    channel: String,
    tx: mpsc::UnboundedSender<ChildEvent>,
    log: Arc<Mutex<MasterLog>>,
    origin: tokio::time::Instant,
}

impl MasterProc for MProc {
    fn pid(&self) -> Option<u32> {
        Some(7)
    }
    fn signal(&self, sig: i32) {
        assert_eq!(sig, nix::libc::SIGINT);
        let mut l = self.log.lock().unwrap();
        l.signals.push((self.channel.clone(), ms(self.origin)));
        let tx = self.tx.clone();
        let log = self.log.clone();
        let ch = self.channel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            log.lock().unwrap().live.insert(ch, false);
            let _ = tx.send(ChildEvent::Done(ChildDone {
                exit_code: Some(0),
                ..Default::default()
            }));
        });
    }
}

struct MBackend {
    log: Arc<Mutex<MasterLog>>,
    origin: tokio::time::Instant,
}

impl MasterBackend for MBackend {
    fn detect(&self, _binary_path: &str) -> RuntimeDetect {
        RuntimeDetect {
            available: true,
            gstreamer: true,
            reason: None,
        }
    }
    fn spawn(
        &self,
        spec: &ChildSpec,
    ) -> (Arc<dyn MasterProc>, mpsc::UnboundedReceiver<ChildEvent>) {
        let channel = spec.args[0].clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut l = self.log.lock().unwrap();
        assert!(
            !l.live.get(&channel).copied().unwrap_or(false),
            "double spawn of {channel}"
        );
        l.live.insert(channel.clone(), true);
        l.spawns.push((channel.clone(), ms(self.origin)));
        (
            Arc::new(MProc {
                channel,
                tx,
                log: self.log.clone(),
                origin: self.origin,
            }),
            rx,
        )
    }
}

// ---------------------------------------------------------------------------
// harness

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "xg-cam-absent-{}-{}",
            std::process::id(),
            xorgate_hal::fragment_stat::hrtime_ns()
        ));
        std::fs::create_dir_all(p.join("pipelines")).unwrap();
        for k in ["cam0", "cam1"] {
            std::fs::write(p.join("pipelines").join(format!("{k}.gst")), "x\n").unwrap();
        }
        std::fs::write(
            p.join("kvs.json"),
            r#"{"deviceId":"dev","region":"us-east-1","credentialEndpoint":"c.example","roleAlias":"r",
                "channels":[{"streamKey":"cam0","channelName":"dev-cam0"},
                            {"streamKey":"cam1","channelName":"dev-cam1"}]}"#,
        )
        .unwrap();
        Self(p)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Events = Arc<Mutex<Vec<(u64, ActivityLevel, String)>>>;
/// Every D86 source edge the capture emitted: (t, key, absent).
type Edges = Arc<Mutex<Vec<(u64, String, bool)>>>;

struct World {
    origin: tokio::time::Instant,
    _home: Home,
    cam1_absent: Arc<AtomicBool>,
    cam1_dies_after_ms: Arc<AtomicU64>,
    cam1_exit_jump_ms: Arc<AtomicU64>,
    edges: Edges,
    cap_be: Arc<CapBackend>,
    cap: VideoCaptureSupervisor,
    media: MediaSupervisor,
    mlog: Arc<Mutex<MasterLog>>,
    events: Events,
}

fn world() -> World {
    let origin = tokio::time::Instant::now();
    let home = Home::new();
    let mlog: Arc<Mutex<MasterLog>> = Arc::default();
    let cfg = MediaConfig {
        enabled: true,
        binary_path: "/opt/xorgate-rs/media/xorgate-kvs-webrtc".into(),
        lib_dir: "/opt/xorgate-rs/media/lib".into(),
        manifest_path: home.0.join("kvs.json").to_string_lossy().into_owned(),
        source: "shm".into(),
        media_type: "video-only".into(),
        pipeline_dir: home.0.join("pipelines").to_string_lossy().into_owned(),
        pipeline_path: None,
        log_level: "3".into(),
        idle_teardown_secs: "30".into(),
        cert_path: "/etc/xorgate/cert.crt".into(),
        key_path: "/etc/xorgate/private.key".into(),
        ca_cert_path: "/opt/xorgate-rs/media/cert.pem".into(),
    };
    let guard: GuardFn = Arc::new(|_: &str| StreamGuard::Ok);
    let media = MediaSupervisor::with_backend(
        cfg,
        guard,
        null_tape(),
        Arc::new(MBackend {
            log: mlog.clone(),
            origin,
        }),
    );

    let cam1_absent = Arc::new(AtomicBool::new(true));
    let cam1_dies_after_ms = Arc::new(AtomicU64::new(0));
    let cam1_exit_jump_ms = Arc::new(AtomicU64::new(0));
    let clock_offset = Arc::new(AtomicU64::new(0));
    let cap_be = Arc::new(CapBackend {
        origin,
        cam1_absent: cam1_absent.clone(),
        cam1_dies_after_ms: cam1_dies_after_ms.clone(),
        cam1_exit_jump_ms: cam1_exit_jump_ms.clone(),
        clock_offset: clock_offset.clone(),
        spawns: Mutex::default(),
    });
    let mut o = VideoCaptureOpts::new(
        "dev",
        vec![
            CaptureCamera {
                stream_key: "cam0".into(),
                camera_name: "/base/i2c@88000/imx290@1a".into(),
            },
            CaptureCamera {
                stream_key: "cam1".into(),
                camera_name: "/base/i2c@70000/imx290@1a-cam1".into(),
            },
        ],
        false, // bridge-only: streaming, recording off (bench 2)
        resolve_video_preset("1080p15-2mbps").clone(),
        60,
        home.0.join("video"),
        home.0.join("shm"),
        "boot-1",
        Arc::new(|| TimeSource::Ntp),
        Arc::new(OffsetClock {
            inner: AgentClock::following_tokio(1_760_000_000_000),
            offset: clock_offset,
        }),
    );
    o.backend = cap_be.clone();
    o.fs_watch = false;
    o.shm_segment_dir = home.0.join("devshm");
    let events: Events = Arc::default();
    let ev = events.clone();
    o.on_event = Some(Arc::new(move |l, m| {
        ev.lock().unwrap().push((ms(origin), l, m.to_string()))
    }));
    let m1 = media.clone();
    o.on_spawned = Some(Arc::new(move |k: &str| m1.restart_channel(k)));
    let m2 = media.clone();
    let edges: Edges = Arc::default();
    let e2 = edges.clone();
    o.on_source_absent = Some(Arc::new(move |k: &str, a: bool, seq: u64| {
        e2.lock().unwrap().push((ms(origin), k.to_string(), a));
        m2.set_source_absent(k, a, seq)
    }));
    let cap = VideoCaptureSupervisor::new(o);
    World {
        origin,
        _home: home,
        cam1_absent,
        cam1_dies_after_ms,
        cam1_exit_jump_ms,
        edges,
        cap_be,
        cap,
        media,
        mlog,
        events,
    }
}

async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Advance the paused clock to `t_ms`, 1 s at a time (lets the pumps run).
async fn run_until(w: &World, t_ms: u64) {
    while ms(w.origin) < t_ms {
        let next = (ms(w.origin) + 1_000).min(t_ms);
        tokio::time::sleep_until(w.origin + Duration::from_millis(next)).await;
        settle().await;
    }
}

fn cam1_spawns(w: &World) -> Vec<u64> {
    w.cap_be
        .spawns
        .lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| k == "cam1")
        .map(|(_, t)| *t)
        .collect()
}

fn media_state(w: &World, key: &str) -> String {
    w.media
        .read()
        .streams
        .iter()
        .find(|s| s.stream_key == key)
        .map(|s| s.state.clone())
        .unwrap_or_default()
}

fn cap_state(w: &World, key: &str) -> CaptureState {
    w.cap
        .status()
        .into_iter()
        .find(|s| s.stream_key == key)
        .unwrap()
        .state
}

fn master_spawns(w: &World, ch: &str) -> Vec<u64> {
    w.mlog
        .lock()
        .unwrap()
        .spawns
        .iter()
        .filter(|(c, _)| c == ch)
        .map(|(_, t)| *t)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn absent_sensor_backs_off_holds_its_master_and_recovers() {
    let w = world();
    // The worker's first publish tick starts the masters; capture starts.
    let _ = w.media.read();
    w.cap.start();
    settle().await;

    // ---- 30 min with the cam1 sensor absent.
    run_until(&w, 1_800_000).await;

    // 1. Capture: the TS ladder once (0, +2 s, +4 s), park 60 s, then one
    //    probe per step: +120 s, +240 s, then every 300 s (each exit 100 ms
    //    after its spawn). The TS would have spawned cam1 ≈ 82 times.
    assert_eq!(
        cam1_spawns(&w),
        [0, 2_100, 6_200, 66_300, 186_400, 426_500, 726_600, 1_026_700, 1_326_800, 1_626_900]
    );
    // cam0's capture: one spawn, never disturbed.
    let cam0_caps = w
        .cap_be
        .spawns
        .lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| k == "cam0")
        .count();
    assert_eq!(cam0_caps, 1);
    assert_eq!(cap_state(&w, "cam0"), CaptureState::Running);
    assert_eq!(
        cap_state(&w, "cam1"),
        CaptureState::Failed,
        "parked = failed"
    );

    // 2. One agent line per back-off step for cam1 after the first park.
    let ev = w.events.lock().unwrap().clone();
    let cam1_lines: Vec<&(u64, ActivityLevel, String)> = ev
        .iter()
        .filter(|(_, _, m)| m.starts_with("video capture cam1"))
        .collect();
    let texts: Vec<&str> = cam1_lines.iter().map(|(_, _, m)| m.as_str()).collect();
    assert_eq!(
        texts,
        [
            "video capture cam1 exited (code 1); retry 1/3 in 2000ms",
            "video capture cam1 exited (code 1); retry 2/3 in 4000ms",
            "video capture cam1 exited (code 1) before stabilizing 3x; parking for 60s (camera in use? libcamera error?)",
            "video capture cam1 failed at start again (code 1, session ?…); backing off 120s (step 2; camera absent or broken?)",
            "video capture cam1 failed at start again (code 1, session ?…); backing off 240s (step 3; camera absent or broken?)",
            "video capture cam1 failed at start again (code 1, session ?…); backing off 300s (step 4; camera absent or broken?)",
            "video capture cam1 failed at start again (code 1, session ?…); backing off 300s (step 5; camera absent or broken?)",
            "video capture cam1 failed at start again (code 1, session ?…); backing off 300s (step 6; camera absent or broken?)",
            "video capture cam1 failed at start again (code 1, session ?…); backing off 300s (step 7; camera absent or broken?)",
            "video capture cam1 failed at start again (code 1, session ?…); backing off 300s (step 8; camera absent or broken?)",
        ]
    );

    // 3. The cam1 master: started at 0, bounced by the first streak's two
    //    respawns, then HELD from the first park (6.3 s) — no spawn since.
    let m1 = master_spawns(&w, "dev-cam1");
    assert!(
        m1.iter().all(|t| *t < 6_300),
        "cam1 master after the park: {m1:?}"
    );
    assert!(m1.len() <= 3, "{m1:?}");
    {
        let l = w.mlog.lock().unwrap();
        assert!(!l.live["dev-cam1"], "the held master is down");
        assert!(
            l.signals.iter().all(|(c, _)| c == "dev-cam1"),
            "cam0's master is never signalled: {:?}",
            l.signals
        );
    }
    assert_eq!(media_state(&w, "cam1"), "failed");
    // cam0's master: one spawn, promoted to running.
    assert_eq!(master_spawns(&w, "dev-cam0"), [0]);
    assert_eq!(media_state(&w, "cam0"), "running");

    // ---- The sensor starts working (re-seated ribbon): the next probe
    //      (1 927 s) stays up; 15 s later it is stable → master released.
    w.cam1_absent.store(false, Ordering::SeqCst);
    run_until(&w, 1_941_000).await;
    assert_eq!(cam1_spawns(&w).last(), Some(&1_927_000));
    assert_eq!(
        media_state(&w, "cam1"),
        "failed",
        "not until the capture is stable"
    );
    run_until(&w, 1_943_000).await;
    assert_eq!(cap_state(&w, "cam1"), CaptureState::Running);
    let m1 = master_spawns(&w, "dev-cam1");
    assert_eq!(m1.last(), Some(&1_942_000), "{m1:?}");
    assert_eq!(media_state(&w, "cam1"), "starting");
    run_until(&w, 1_960_000).await;
    assert_eq!(media_state(&w, "cam1"), "running");
    assert_eq!(
        master_spawns(&w, "dev-cam0"),
        [0],
        "cam0 untouched throughout"
    );
    assert_eq!(
        cam1_spawns(&w).len(),
        11,
        "no further capture spawns once healthy"
    );

    w.cap.stop();
    w.media.stop();
    settle().await;
}

/// A camera retired while held (disabled in settings / gone from the list)
/// releases its master: the gate never outlives the capture run.
#[tokio::test(start_paused = true)]
async fn retiring_a_held_camera_releases_its_master() {
    let w = world();
    let _ = w.media.read();
    w.cap.start();
    settle().await;
    run_until(&w, 10_000).await;
    assert_eq!(media_state(&w, "cam1"), "failed");
    let before = master_spawns(&w, "dev-cam1").len();

    // cam1 leaves the capture set (parked: no child → removed at once).
    w.cap
        .set_cameras(&[CaptureCamera {
            stream_key: "cam0".into(),
            camera_name: "/base/i2c@88000/imx290@1a".into(),
        }])
        .await;
    settle().await;
    assert_eq!(master_spawns(&w, "dev-cam1").len(), before + 1, "released");
    assert_eq!(media_state(&w, "cam1"), "starting");

    w.cap.stop();
    w.media.stop();
    settle().await;
}

fn cam1_edges(w: &World) -> Vec<(u64, bool)> {
    w.edges
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, k, _)| k == "cam1")
        .map(|(t, _, a)| (*t, *a))
        .collect()
}

/// Review S1: a camera that RUNS ~10 s and then crashes is not absent: the
/// plain TS ladder (2 s, 4 s, park 60 s, repeat), never a gap past 60 s,
/// never a `true` edge, so its master is never held.
#[tokio::test(start_paused = true)]
async fn a_camera_that_runs_10s_then_crashes_keeps_the_ts_ladder_and_its_master() {
    let w = world();
    w.cam1_absent.store(false, Ordering::SeqCst);
    w.cam1_dies_after_ms.store(10_000, Ordering::SeqCst);
    let _ = w.media.read();
    w.cap.start();
    settle().await;
    run_until(&w, 400_000).await;

    // spawn → +10 s exit → +2 s / +4 s / park 60 s, repeating.
    let sp = cam1_spawns(&w);
    assert_eq!(
        &sp[..7],
        [0, 12_000, 26_000, 96_000, 108_000, 122_000, 192_000],
        "{sp:?}"
    );
    let gaps: Vec<u64> = sp.windows(2).map(|p| p[1] - p[0]).collect();
    assert!(
        gaps.iter().all(|g| *g <= 70_000),
        "never backed off past 60 s: {gaps:?}"
    );
    assert!(
        cam1_edges(&w).iter().all(|(_, a)| !a),
        "never reported down: {:?}",
        cam1_edges(&w)
    );
    let ev = w.events.lock().unwrap().clone();
    assert!(ev.iter().all(|(_, _, m)| !m.contains("at start again")));
    // The master was bounced by the capture respawns (the TS ladder), never
    // held: it kept being respawned all along.
    let m1 = master_spawns(&w, "dev-cam1");
    assert!(m1.len() > 3 && *m1.last().unwrap() > 200_000, "{m1:?}");
    w.cap.stop();
    w.media.stop();
    settle().await;
}

/// Review S3 (mutant A, `capture/mod.rs` "an exit racing the stable
/// timer"): a probe that has run past the stable window when it exits,
/// before its stable timer fired, releases the held master AT that exit.
#[tokio::test(start_paused = true)]
async fn a_stable_exit_racing_the_stable_timer_releases_the_master() {
    let w = world();
    let _ = w.media.read();
    w.cap.start();
    settle().await;
    run_until(&w, 10_000).await;
    assert_eq!(media_state(&w, "cam1"), "failed", "held after the park");
    let before = master_spawns(&w, "dev-cam1").len();

    // The probe at 66.3 s runs 1 s by tokio time, but the capture clock
    // jumps 20 s at its exit: ran ≥ 15 s with the stable timer still pending.
    w.cam1_absent.store(false, Ordering::SeqCst);
    w.cam1_exit_jump_ms.store(20_000, Ordering::SeqCst);
    w.cam1_dies_after_ms.store(1_000, Ordering::SeqCst);
    run_until(&w, 67_500).await;
    assert_eq!(cam1_spawns(&w)[3], 66_300);
    assert_eq!(
        cam1_edges(&w).last(),
        Some(&(67_300, false)),
        "released at the exit itself"
    );
    let m1 = master_spawns(&w, "dev-cam1");
    assert_eq!(m1.len(), before + 1, "{m1:?}");
    assert_eq!(m1.last(), Some(&67_300));
    w.cap.stop();
    w.media.stop();
    settle().await;
}

/// Review S3 (mutant D, the retiring exit): cam1 is retired while its
/// probe child is still alive; that child's exit releases the master.
#[tokio::test(start_paused = true)]
async fn retiring_a_held_camera_during_a_probe_releases_its_master() {
    let w = world();
    let _ = w.media.read();
    w.cap.start();
    settle().await;
    run_until(&w, 66_350).await; // the probe spawned at 66.3 s, dies at 66.4 s
    assert_eq!(cam1_spawns(&w).last(), Some(&66_300));
    assert_eq!(media_state(&w, "cam1"), "failed");
    let before = master_spawns(&w, "dev-cam1").len();

    w.cap
        .set_cameras(&[CaptureCamera {
            stream_key: "cam0".into(),
            camera_name: "/base/i2c@88000/imx290@1a".into(),
        }])
        .await;
    settle().await;
    assert_eq!(cam1_edges(&w).last().map(|e| e.1), Some(false));
    assert_eq!(master_spawns(&w, "dev-cam1").len(), before + 1, "released");
    assert_eq!(media_state(&w, "cam1"), "starting");
    w.cap.stop();
    w.media.stop();
    settle().await;
}

/// Camera report (support-other-camera-models Phase 0, Rust-only): what
/// `VideoCaptureSupervisor::health` says about the same two cameras. The
/// healthy one reads `starting` then `streaming`; the absent one reads
/// `failing` at EVERY second of its ladder (the retries, the park, each
/// back-off probe: no flip a report could churn on), keeps its last `ERROR`
/// line, and reads `streaming` again only once a child has been stable for
/// `RECOVER_MS`. Nothing here changes the ladder: the spawn times are the
/// ones the test above pins.
#[tokio::test(start_paused = true)]
async fn camera_report_health_is_failing_through_the_ladder_and_streaming_after_recovery() {
    use xorgate_core::camera_truth::{capture_truth, CaptureLive, CaptureTruth, RECOVER_MS};
    let w = world();
    let _ = w.media.read();
    w.cap.start();
    settle().await;
    let health = |key: &str| -> CaptureLive {
        w.cap
            .health()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, l)| l)
            .unwrap()
    };
    let truth = |key: &str| capture_truth(Some(&health(key)), false);

    // A first start that has not failed.
    assert_eq!(truth("cam0"), CaptureTruth::Starting);
    assert_eq!(truth("cam1"), CaptureTruth::Starting);

    // cam1's first child died 100 ms in, with libcamera's error line.
    run_until(&w, 1_000).await;
    assert_eq!(truth("cam1"), CaptureTruth::Failing);
    let h = health("cam1");
    assert!(h.unstable);
    assert_eq!(h.stalled_preset, None, "a start failure, not a stall");
    // The child's FIRST error line that is not on the skip list: not the
    // other camera's busy-controls line before it, not shmsink's after it.
    assert_eq!(
        h.last_error.as_deref(),
        Some("ERROR: from element /GstPipeline:pipeline0/GstLibcameraSrc:libcamerasrc0: Failed to start the camera: Remote I/O error")
    );

    run_until(&w, 16_000).await;
    assert_eq!(truth("cam0"), CaptureTruth::Streaming);
    assert!(!health("cam0").unstable);

    // Ten minutes of ladder, sampled every second like the worker does.
    let mut t = 16_000;
    while t < 600_000 {
        t += 1_000;
        run_until(&w, t).await;
        assert_eq!(truth("cam1"), CaptureTruth::Failing, "t={t}");
        assert_eq!(truth("cam0"), CaptureTruth::Streaming, "t={t}");
    }
    assert_eq!(
        cam1_spawns(&w),
        [0, 2_100, 6_200, 66_300, 186_400, 426_500],
        "the ladder is the one the D86 test pins"
    );
    assert_eq!(health("cam1").backoff_step, 4);

    // The sensor starts working: the probe at 726.6 s stays up and is
    // stable at 741.6 s. It is NOT `streaming` yet …
    w.cam1_absent.store(false, Ordering::SeqCst);
    run_until(&w, 745_000).await;
    assert_eq!(cap_state(&w, "cam1"), CaptureState::Running);
    assert_eq!(truth("cam1"), CaptureTruth::Failing);
    assert_eq!(
        health("cam1").backoff_step,
        0,
        "the back-off itself is over"
    );
    run_until(&w, 741_600 + RECOVER_MS - 600).await;
    assert_eq!(truth("cam1"), CaptureTruth::Failing);
    // … until it has been stable for RECOVER_MS.
    run_until(&w, 741_600 + RECOVER_MS + 400).await;
    assert_eq!(truth("cam1"), CaptureTruth::Streaming);
    let h = health("cam1");
    assert!(!h.unstable);
    assert_eq!(h.last_error, None);
}
