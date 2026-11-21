//! support-other-camera-models Phase 0 (Rust-only): a B5 hold heals WITHOUT
//! an agent restart, and the re-listing that heals it can do nothing else.
//! End to end through the real pieces:
//!
//! - the REAL hardware reconcile (`MediaReconciler`, a scripted
//!   `rpicam-hello` listing taken from the benches),
//! - the REAL capture camera resolver (`policy_sources::cameras_fn`, the B5
//!   hold),
//! - the REAL `RecordingManager` (a fake capture backend that logs spawns),
//! - the REAL `RecordingHooks::hardware_recheck` (reconcile → bounce →
//!   `reapply`, the `hardware_changed` path),
//! - the REAL `CameraTruth` driving the re-listing schedule from its 1 s
//!   tick.
//!
//! The device is bench 2: two cameras, cam1 (imx219) streaming, cam0
//! (imx335) held because its driver is bound but libcamera did not list it.
//!
//! - The heal: when a re-listing finds cam0, its capture starts as at boot,
//!   the journal says so once, and cam1's capture is the SAME run.
//! - Heal-only: a re-listing that omits cam1 does not stop cam1; one that
//!   cannot enumerate changes nothing at all and is not a check.
//! - One pass at a time: a re-listing is skipped while a hardware-block
//!   pass runs, and a block change that arrives during a re-listing runs
//!   after it and wins.

mod common;
mod recording_support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use recording_support::*;
use serde_json::{json, Value};
use xorgate_agent::ctx::{AgentCtx, NoopHooks, PhaseHooks};
use xorgate_agent::ops::camera_truth::{CameraTruth, CameraTruthOptions, REPORT_HOLD_MS};
use xorgate_agent::recording::hooks::{MediaReconciler, StreamHardware};
use xorgate_agent::recording::policy_sources::cameras_fn;
use xorgate_agent::recording::{RecordingHooks, RecordingManager};
use xorgate_core::camera_truth::PortNode;
use xorgate_core::hardware_block::{parse_hardware_block, EncoderToken};
use xorgate_core::pipeline_builder::{build_bridged_kvs_pipeline, build_direct_kvs_pipeline};
use xorgate_core::stream_guard::StreamHardwareStatus;
use xorgate_hal::exec::RunResult;
use xorgate_hal::io::null_tape;

const IMX335: &str = "/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a";

// The benches' listings (`fixtures/camera_truth/bench-listings.json`).
const ONLY_CAM1: &str = "stock libcamera: only the IMX219 on CAM1 is listed";
const BOTH: &str = "IMX335 on CAM0 + IMX219 on CAM1, Arducam libcamera";
const NONE: &str = "no camera at all, as the benches print it";
const BROKEN: &str = "libcamera broken";

const HELD: &str = "cam0: no camera enumerated on csi0 (expected imx335) — stream held down until one appears (check the ribbon + overlays, reboot after install)";
const HEALED: &str = "cam0: a camera is enumerated now — starting the stream";

/// A bench listing by (a unique part of) its fixture name.
fn bench_listing(name_part: &str) -> RunResult {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/camera_truth/bench-listings.json");
    let f: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    let c = f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str().unwrap().contains(name_part))
        .unwrap_or_else(|| panic!("no bench listing {name_part}"))
        .clone();
    let code = c["exitCode"].as_i64().map(|c| c as i32);
    RunResult {
        stdout: c["stdout"].as_str().unwrap().into(),
        stderr: c["stderr"].as_str().unwrap().into(),
        exit_code: code,
        failed: code != Some(0),
        not_found: false,
        spawn_failed: false,
    }
}

/// The `hardware: …` events of the flushed state.json.
fn hardware_events(state_path: &Path) -> Vec<String> {
    let v: Value = serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
    v["recentEvents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["msg"].as_str())
        .filter_map(|m| m.strip_prefix("hardware: "))
        .map(str::to_string)
        .collect()
}

async fn yields() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Bench 2 after its boot: cam1 streaming, cam0 held with its driver bound.
struct World {
    home: PathBuf,
    ctx: AgentCtx,
    hw: Arc<StreamHardware>,
    /// What `rpicam-hello --list-cameras` answers next.
    listing: Arc<Mutex<RunResult>>,
    /// How many listings ran.
    runs: Arc<AtomicUsize>,
    /// The next listing waits for `release` (its answer is the one current
    /// when it STARTED).
    block_next: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
    rig: Rig,
    m: RecordingManager,
    hooks: Arc<RecordingHooks>,
    truth: CameraTruth,
    /// Config reports the camera report asked for.
    asked: Arc<AtomicUsize>,
    /// The journal right after boot.
    boot_events: Vec<String>,
}

impl World {
    async fn boot(tag: &str) -> World {
        let home = common::scratch(tag);
        let mut ctx = common::test_ctx(&home, "dev", Arc::new(NoopHooks), null_tape());
        let pipelines = home.join("pipelines");
        let mut paths = (*ctx.paths).clone();
        paths.pipelines = pipelines.clone();
        paths.kvs_manifest = home.join("kvs.json");
        ctx.paths = Arc::new(paths);
        std::fs::write(
            home.join("kvs.json"),
            r#"{"deviceId":"dev","region":"us-east-1","credentialEndpoint":"c.example","roleAlias":"r",
                "channels":[{"streamKey":"cam0","channelName":"dev-cam0"},
                            {"streamKey":"cam1","channelName":"dev-cam1"}]}"#,
        )
        .unwrap();
        // cam0's trio of an earlier boot (the same IMX335).
        std::fs::create_dir_all(&pipelines).unwrap();
        let sock = home.join("shm").join("cam0.sock").display().to_string();
        std::fs::write(
            pipelines.join("cam0.gst"),
            format!(
                "{}\n",
                build_bridged_kvs_pipeline(EncoderToken::SwH264, &sock)
            ),
        )
        .unwrap();
        std::fs::write(pipelines.join("cam0.camera"), format!("{IMX335}\n")).unwrap();
        std::fs::write(
            pipelines.join("cam0.gst.direct"),
            format!(
                "{}\n",
                build_direct_kvs_pipeline(EncoderToken::SwH264, "cam0", IMX335)
            ),
        )
        .unwrap();
        ctx.hardware.send_replace(Some(
            parse_hardware_block(&json!({
                "v": 2, "platform": "rpi-cm5", "encoder": "sw-h264", "rails": true,
                "statusLed": null, "imu": "bno085",
                "video": [
                    {"key": "cam0", "connector": "csi0", "sensor": "imx335", "codec": "h264"},
                    {"key": "cam1", "connector": "csi1", "sensor": "imx219", "codec": "h264"}
                ]
            }))
            .unwrap(),
        ));

        let listing = Arc::new(Mutex::new(bench_listing(ONLY_CAM1)));
        let runs = Arc::new(AtomicUsize::new(0));
        let block_next = Arc::new(AtomicBool::new(false));
        let release = Arc::new(tokio::sync::Notify::new());
        let hw = Arc::new(StreamHardware::default());
        let (l, n, b, rel) = (
            listing.clone(),
            runs.clone(),
            block_next.clone(),
            release.clone(),
        );
        let reconciler = Arc::new(MediaReconciler {
            ctx: ctx.clone(),
            stream_hw: hw.clone(),
            pipeline_dir: pipelines.clone(),
            list_exec: Arc::new(move || {
                n.fetch_add(1, Ordering::SeqCst);
                let r = l.lock().unwrap().clone();
                let wait = b.swap(false, Ordering::SeqCst).then(|| rel.clone());
                Box::pin(async move {
                    if let Some(rel) = wait {
                        rel.notified().await;
                    }
                    r
                })
            }),
            boot_config: None,
        });

        // The recording manager, its capture cameras from the REAL resolver.
        let mut rig = Rig::new(tag);
        rig.cameras = vec![];
        rig.cameras_override = Some(cameras_fn(&ctx, Some(hw.clone())));
        rig.save_recording(
            &json!({"video": {"enabled": true, "bootDefer": "off", "spawnStaggerSecs": 0}}),
        );
        let m = rig.build();
        let hooks = Arc::new(RecordingHooks::new(
            None,
            Some(reconciler),
            Arc::new(|| None),
            rig.clock.clone(),
        ));

        // ---- Boot: cam1 lists and streams; cam0 is held, nothing launched.
        hooks.reconcile_hardware_boot().await.unwrap();
        hooks.install(m.clone());
        hooks.recording_start().await;
        m.settled().await;
        assert_eq!(hw.status("cam0"), Some(StreamHardwareStatus::NoCamera));
        assert_eq!(hw.status("cam1"), Some(StreamHardwareStatus::Rendered));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let boot_events = hardware_events(&ctx.paths.state);
        assert_eq!(boot_events.iter().filter(|e| *e == HELD).count(), 1);

        // ---- The camera report: both drivers ARE bound.
        let (block, snap, recheck) = (ctx.hardware.clone(), hw.clone(), hooks.clone());
        let asked = Arc::new(AtomicUsize::new(0));
        let a = asked.clone();
        let truth = CameraTruth::new(CameraTruthOptions {
            hardware: Box::new(move || block.borrow().clone()),
            stream_hw: Box::new(move || snap.snapshot()),
            boot_config: Arc::new(|| {
                Some("dtoverlay=imx335,cam0\ndtoverlay=imx219,cam1,rotation=180\n".to_string())
            }),
            port_nodes: Arc::new(|_| {
                Some(vec![
                    PortNode {
                        connector: "csi0".into(),
                        node: "imx335@1a".into(),
                        driver: Some("imx335".into()),
                    },
                    PortNode {
                        connector: "csi1".into(),
                        node: "imx219@10".into(),
                        driver: Some("imx219".into()),
                    },
                ])
            }),
            pipeline_dir: pipelines.display().to_string(),
            capture: Box::new(Vec::new),
            publish: Box::new(|_| {}),
            on_change: Box::new(move || {
                a.fetch_add(1, Ordering::SeqCst);
            }),
            recheck: Box::new(move || recheck.hardware_recheck()),
            capture_wanted: Box::new(|_| true),
            hold_ms: REPORT_HOLD_MS,
        });
        truth.refresh();
        // The boot report went out with this block.
        let _ = truth.for_report();
        truth.report_sent();
        let w = World {
            home,
            ctx,
            hw,
            listing,
            runs,
            block_next,
            release,
            rig,
            m,
            hooks,
            truth,
            asked,
            boot_events,
        };
        assert_eq!(w.spawned(), ["cam1"], "B5: cam0 is held");
        assert_eq!(w.port("cam0")["verdict"], "not-detected");
        assert_eq!(w.port("cam0")["capture"]["state"], "absent");
        assert_eq!(
            w.port("cam0")["detail"],
            "The imx335 driver is bound on csi0, but libcamera did not list a camera there when the agent started. The agent is checking again."
        );
        assert_eq!(w.port("cam1")["verdict"], "ok");
        w
    }

    /// Run the 1 s tick for `secs` seconds.
    async fn run(&self, secs: u64) {
        for _ in 0..secs {
            tokio::time::sleep(Duration::from_secs(1)).await;
            self.truth.tick();
            yields().await;
        }
        self.m.settled().await;
    }

    fn set_listing(&self, name_part: &str) {
        *self.listing.lock().unwrap() = bench_listing(name_part);
    }

    /// Every capture spawn so far: `(camera, still running)`.
    fn captures(&self) -> Vec<(String, bool)> {
        self.rig
            .log
            .spawns
            .lock()
            .unwrap()
            .iter()
            .map(|s| (s.cam.clone(), !s.stopped))
            .collect()
    }

    fn spawned(&self) -> Vec<String> {
        self.captures().into_iter().map(|(c, _)| c).collect()
    }

    fn port(&self, key: &str) -> Value {
        self.truth.current().unwrap()[key].clone()
    }

    fn events(&self) -> Vec<String> {
        hardware_events(&self.ctx.paths.state)
    }

    fn end(self) {
        self.m.stop();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// The heal. The 15 s re-listing finds cam0: its capture starts, the journal
/// says so once, and cam1's capture is the same run as before.
#[tokio::test(start_paused = true)]
async fn a_held_stream_starts_when_a_relisting_finds_its_camera() {
    let w = World::boot("cam-heal").await;
    w.set_listing(BOTH);
    w.run(16).await;
    assert_eq!(
        w.runs.load(Ordering::SeqCst),
        2,
        "boot + the 15 s re-listing"
    );
    assert_ne!(w.hw.status("cam0"), Some(StreamHardwareStatus::NoCamera));
    assert!(w.hw.capture_allowed("cam0"));
    // cam0's capture started, as it would have at boot; cam1's run was not
    // touched: one spawn, still running.
    assert_eq!(
        w.captures(),
        [("cam1".to_string(), true), ("cam0".to_string(), true)]
    );
    // The journal: the healed stream's line, once; nothing about cam1.
    let mut want = w.boot_events.clone();
    want.push(HEALED.to_string());
    assert_eq!(w.events(), want);
    assert_eq!(
        w.hw.status("cam1"),
        Some(StreamHardwareStatus::Rendered),
        "a re-listing leaves a found port's verdict alone"
    );
    w.truth.tick();
    assert_eq!(w.port("cam0")["verdict"], "ok");
    assert_eq!(w.port("cam0")["bound"]["listed"], "imx335");
    assert_eq!(w.port("cam1")["verdict"], "ok");

    // Nothing is held any more: no further re-listing, ever.
    w.run(700).await;
    assert_eq!(w.runs.load(Ordering::SeqCst), 2);
    assert_eq!(w.captures().len(), 2);
    assert!(w.captures().iter().all(|(_, running)| *running));
    w.end();
}

/// Heal-only (a): a re-listing that can enumerate but OMITS a camera that
/// was found (here: lists nothing at all) must not stop that camera.
#[tokio::test(start_paused = true)]
async fn a_relisting_that_omits_a_found_camera_does_not_stop_it() {
    let w = World::boot("cam-heal-omit").await;
    let cameras_before = w.hw.snapshot().cameras;
    w.set_listing(NONE);
    w.run(16).await;
    assert_eq!(w.runs.load(Ordering::SeqCst), 2, "the re-listing ran");
    // cam1: same verdict, same listing entry, same capture run.
    assert_eq!(w.hw.status("cam1"), Some(StreamHardwareStatus::Rendered));
    assert!(w.hw.capture_allowed("cam1"));
    assert_eq!(w.hw.snapshot().cameras, cameras_before);
    assert_eq!(w.captures(), [("cam1".to_string(), true)]);
    assert_eq!(w.port("cam1")["verdict"], "ok");
    assert_eq!(w.port("cam1")["bound"]["listed"], "imx219");
    // The journal does not claim cam1 is held, and does not repeat cam0.
    assert_eq!(w.events(), w.boot_events);
    // It WAS a check (it could enumerate): cam0 is still held, and its
    // sentence now says it was checked again.
    assert_eq!(w.hw.snapshot().passes, 2);
    assert_eq!(w.hw.status("cam0"), Some(StreamHardwareStatus::NoCamera));
    assert_eq!(
        w.port("cam0")["detail"],
        "The imx335 driver is bound on csi0 but libcamera lists no camera there (checked again after the agent started): libcamera on this device may not support this sensor. The imx335 driver only weakly checks which chip is attached, so the camera on this port may be a different sensor."
    );
    // Every later re-listing with the same answer: still nothing stops.
    w.run(400).await;
    assert_eq!(w.captures(), [("cam1".to_string(), true)]);
    assert_eq!(w.events(), w.boot_events);
    w.end();
}

/// Heal-only (b): a re-listing that CANNOT enumerate (timeout, spawn
/// failure, a broken libcamera) is not a check: the block is byte-identical,
/// the pass count does not move, no report is asked for.
#[tokio::test(start_paused = true)]
async fn a_relisting_that_cannot_enumerate_changes_nothing() {
    let w = World::boot("cam-heal-broken").await;
    let block_before = serde_json::to_string(&w.truth.current()).unwrap();
    let snap_before = w.hw.snapshot();
    assert_eq!(snap_before.passes, 1);
    w.set_listing(BROKEN);
    // Three re-listings (15 s, 30 s, 60 s) and every hold after them.
    w.run(100).await;
    assert_eq!(w.runs.load(Ordering::SeqCst), 4, "three re-listings ran");
    assert_eq!(
        w.hw.snapshot(),
        snap_before,
        "verdicts, cameras, pass count"
    );
    assert_eq!(
        serde_json::to_string(&w.truth.current()).unwrap(),
        block_before
    );
    assert_eq!(w.asked.load(Ordering::SeqCst), 0, "no report offered");
    assert_eq!(w.captures(), [("cam1".to_string(), true)]);
    assert_eq!(w.events(), w.boot_events);
    // The healthy port never read "cannot list cameras".
    assert_eq!(w.port("cam1")["verdict"], "ok");
    assert_eq!(w.port("cam0")["verdict"], "not-detected");

    // A later re-listing that works still heals.
    w.set_listing(BOTH);
    w.run(300).await;
    assert_eq!(
        w.captures(),
        [("cam1".to_string(), true), ("cam0".to_string(), true)]
    );
    w.end();
}

/// One pass at a time. A re-listing is skipped while a hardware-block pass
/// runs; a block change that arrives during a re-listing waits for it and
/// then replaces its result wholesale.
#[tokio::test(start_paused = true)]
async fn a_relisting_never_runs_beside_or_over_a_block_change_pass() {
    let w = World::boot("cam-heal-turns").await;

    // ---- A hardware-block pass is in flight (its listing has not answered).
    w.block_next.store(true, Ordering::SeqCst);
    w.hooks.hardware_changed();
    yields().await;
    assert_eq!(
        w.runs.load(Ordering::SeqCst),
        2,
        "the block pass is listing"
    );
    // A re-listing now is skipped: no second listing runs beside it.
    w.set_listing(BOTH);
    w.hooks.hardware_recheck();
    yields().await;
    assert_eq!(w.runs.load(Ordering::SeqCst), 2, "skipped");
    w.release.notify_one();
    yields().await;
    w.m.settled().await;
    // The block pass landed (it saw ONLY_CAM1, its answer when it started).
    assert_eq!(w.hw.status("cam0"), Some(StreamHardwareStatus::NoCamera));
    assert_eq!(w.captures(), [("cam1".to_string(), true)]);

    // ---- A re-listing is in flight (it will answer BOTH) when the block
    //      changes; the block pass then lists nothing at all.
    w.block_next.store(true, Ordering::SeqCst);
    w.hooks.hardware_recheck();
    yields().await;
    assert_eq!(
        w.runs.load(Ordering::SeqCst),
        3,
        "the re-listing is listing"
    );
    w.set_listing(NONE);
    w.hooks.hardware_changed();
    yields().await;
    assert_eq!(w.runs.load(Ordering::SeqCst), 3, "the block pass waits");
    w.release.notify_one();
    yields().await;
    w.m.settled().await;
    yields().await;
    w.m.settled().await;
    assert_eq!(w.runs.load(Ordering::SeqCst), 4, "then it runs");
    // The NEWER pass wins: a full replacement, as a block change always was.
    assert_eq!(w.hw.status("cam0"), Some(StreamHardwareStatus::NoCamera));
    assert_eq!(w.hw.status("cam1"), Some(StreamHardwareStatus::NoCamera));
    assert_eq!(w.hw.snapshot().cameras, Some(vec![]));
    assert!(w.captures().iter().all(|(_, running)| !*running));
    w.end();
}
