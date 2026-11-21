//! support-other-camera-models Phase 0 (Rust-only): a B5 hold heals WITHOUT
//! an agent restart, end to end through the real pieces:
//!
//! - the REAL hardware reconcile (`MediaReconciler`, a scripted
//!   `rpicam-hello` listing),
//! - the REAL capture camera resolver (`policy_sources::cameras_fn`, the B5
//!   hold),
//! - the REAL `RecordingManager` (a fake capture backend that logs spawns),
//! - the REAL `RecordingHooks::hardware_recheck` (reconcile → bounce →
//!   `reapply`, the `hardware_changed` path),
//! - the REAL `CameraTruth` driving the re-listing schedule from its 1 s
//!   tick.
//!
//! A port whose driver is bound but whose camera was not listed at boot is
//! listed again 15 s and 30 s after the start; when the camera lists, the
//! stream's capture starts as it would at boot, the journal says so once,
//! and the "no camera enumerated" line is not repeated by the re-listings.

mod common;
mod recording_support;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use recording_support::*;
use serde_json::{json, Value};
use xorgate_agent::ctx::{NoopHooks, PhaseHooks};
use xorgate_agent::ops::camera_truth::{CameraTruth, CameraTruthOptions, REPORT_HOLD_MS};
use xorgate_agent::recording::hooks::{MediaReconciler, StreamHardware};
use xorgate_agent::recording::policy_sources::cameras_fn;
use xorgate_agent::recording::RecordingHooks;
use xorgate_core::camera_truth::PortNode;
use xorgate_core::hardware_block::{parse_hardware_block, EncoderToken};
use xorgate_core::pipeline_builder::{build_bridged_kvs_pipeline, build_direct_kvs_pipeline};
use xorgate_core::stream_guard::StreamHardwareStatus;
use xorgate_hal::exec::RunResult;
use xorgate_hal::io::null_tape;

const IMX708_CM4: &str = "/base/soc/i2c0mux/i2c@0/imx708@1a";

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

#[tokio::test(start_paused = true)]
async fn a_held_stream_starts_when_a_relisting_finds_its_camera() {
    let home = common::scratch("cam-heal");
    let mut ctx = common::test_ctx(&home, "dev", Arc::new(NoopHooks), null_tape());
    let pipelines = home.join("pipelines");
    let mut paths = (*ctx.paths).clone();
    paths.pipelines = pipelines.clone();
    paths.kvs_manifest = home.join("kvs.json");
    ctx.paths = Arc::new(paths);
    std::fs::write(
        home.join("kvs.json"),
        r#"{"deviceId":"dev","region":"us-east-1","credentialEndpoint":"c.example","roleAlias":"r",
            "channels":[{"streamKey":"cam0","channelName":"dev-cam0"}]}"#,
    )
    .unwrap();
    // The trio of an earlier boot (the same IMX708).
    std::fs::create_dir_all(&pipelines).unwrap();
    let sock = home.join("shm").join("cam0.sock").display().to_string();
    std::fs::write(
        pipelines.join("cam0.gst"),
        format!(
            "{}\n",
            build_bridged_kvs_pipeline(EncoderToken::HwH264, &sock)
        ),
    )
    .unwrap();
    std::fs::write(pipelines.join("cam0.camera"), format!("{IMX708_CM4}\n")).unwrap();
    std::fs::write(
        pipelines.join("cam0.gst.direct"),
        format!(
            "{}\n",
            build_direct_kvs_pipeline(EncoderToken::HwH264, "cam0", IMX708_CM4)
        ),
    )
    .unwrap();
    ctx.hardware.send_replace(Some(
        parse_hardware_block(&json!({
            "v": 2, "platform": "rpi-cm4", "encoder": "hw-h264", "rails": false,
            "statusLed": null, "imu": "bno085",
            "video": [{"key": "cam0", "connector": "csi0", "sensor": "imx708", "codec": "h264"}]
        }))
        .unwrap(),
    ));

    // `rpicam-hello --list-cameras`: nothing at boot (the listing came too
    // early), the camera later.
    let listing = Arc::new(Mutex::new(bench_listing(
        "no camera at all, as the benches print it",
    )));
    let runs = Arc::new(AtomicUsize::new(0));
    let hw = Arc::new(StreamHardware::default());
    let (l, n) = (listing.clone(), runs.clone());
    let reconciler = Arc::new(MediaReconciler {
        ctx: ctx.clone(),
        stream_hw: hw.clone(),
        pipeline_dir: pipelines.clone(),
        list_exec: Arc::new(move || {
            n.fetch_add(1, Ordering::SeqCst);
            let r = l.lock().unwrap().clone();
            Box::pin(async move { r })
        }),
        boot_config: None,
    });

    // The recording manager, its capture cameras from the REAL resolver.
    let mut rig = Rig::new("cam-heal");
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

    // ---- Boot: nothing lists, the stream is held, no capture is launched.
    hooks.reconcile_hardware_boot().await.unwrap();
    hooks.install(m.clone());
    hooks.recording_start().await;
    m.settled().await;
    assert_eq!(hw.status("cam0"), Some(StreamHardwareStatus::NoCamera));
    assert!(rig.log.spawns.lock().unwrap().is_empty(), "B5: held");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    const HELD: &str = "cam0: no camera enumerated on csi0 (expected imx708) — stream held down until one appears (check the ribbon + overlays, reboot after install)";
    assert_eq!(hardware_events(&ctx.paths.state), [HELD]);

    // ---- The camera report: the imx708 driver IS bound on the port.
    let (block, snap, recheck) = (ctx.hardware.clone(), hw.clone(), hooks.clone());
    let truth = CameraTruth::new(CameraTruthOptions {
        hardware: Box::new(move || block.borrow().clone()),
        stream_hw: Box::new(move || snap.snapshot()),
        boot_config: Arc::new(|| Some("dtoverlay=imx708,cam0\n".to_string())),
        port_nodes: Arc::new(|_| {
            Some(vec![PortNode {
                connector: "csi0".into(),
                node: "imx708@1a".into(),
                driver: Some("imx708".into()),
            }])
        }),
        pipeline_dir: pipelines.display().to_string(),
        capture: Box::new(Vec::new),
        publish: Box::new(|_| {}),
        on_change: Box::new(|| {}),
        recheck: Box::new(move || recheck.hardware_recheck()),
        hold_ms: REPORT_HOLD_MS,
    });
    truth.refresh();
    let cam0 = |t: &CameraTruth| t.current().unwrap()["cam0"].clone();
    assert_eq!(cam0(&truth)["verdict"], "not-detected");
    assert_eq!(cam0(&truth)["capture"]["state"], "absent");
    assert_eq!(
        cam0(&truth)["detail"],
        "The imx708 driver is bound on csi0, but libcamera did not list a camera there when the agent started. The agent is checking again."
    );

    // ---- 15 s: the first re-listing. Still nothing: still held, and the
    //      journal does not repeat itself.
    for _ in 0..16 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        truth.tick();
        yields().await;
    }
    m.settled().await;
    assert_eq!(runs.load(Ordering::SeqCst), 2, "boot + the 15 s re-listing");
    assert!(rig.log.spawns.lock().unwrap().is_empty());
    assert_eq!(
        hardware_events(&ctx.paths.state),
        [HELD],
        "a re-listing with the same outcome logs nothing"
    );
    assert_eq!(
        cam0(&truth)["detail"],
        "The imx708 driver is bound on csi0 but libcamera lists no camera there (checked again after the agent started): libcamera on this device may not support this sensor."
    );

    // ---- The camera lists from now on; the 30 s re-listing finds it.
    *listing.lock().unwrap() = bench_listing("CM4, IMX708 on CAM0");
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        truth.tick();
        yields().await;
    }
    m.settled().await;
    assert_eq!(runs.load(Ordering::SeqCst), 3, "the 30 s re-listing");
    assert_ne!(hw.status("cam0"), Some(StreamHardwareStatus::NoCamera));
    assert!(hw.capture_allowed("cam0"));
    // The stream's capture started, as it would have at boot.
    let spawned: Vec<String> = rig
        .log
        .spawns
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.cam.clone())
        .collect();
    assert_eq!(spawned, ["cam0"]);
    assert_eq!(
        hardware_events(&ctx.paths.state),
        [
            HELD,
            "cam0: a camera is enumerated now — starting the stream"
        ],
        "said once"
    );
    truth.tick();
    assert_eq!(cam0(&truth)["verdict"], "ok");
    assert_eq!(cam0(&truth)["bound"]["listed"], "imx708");

    // ---- Nothing is held any more: no further re-listing, ever.
    for _ in 0..700 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        truth.tick();
    }
    yields().await;
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    assert_eq!(rig.log.spawns.lock().unwrap().len(), 1);
    m.stop();
    let _ = std::fs::remove_dir_all(&home);
}
