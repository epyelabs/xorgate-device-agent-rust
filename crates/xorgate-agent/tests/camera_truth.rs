//! support-other-camera-models plan, Phase 0 ("tell the truth"), RUST-ONLY
//! behaviour over the REAL hardware reconcile and the REAL capture camera
//! resolver, with the benches' real listings
//! (`fixtures/camera_truth/bench-listings.json`):
//!
//! - B5: a stream whose port enumerated no camera gets no capture camera,
//!   whatever name the stale `<key>.camera` / `<key>.gst.direct` still carry;
//!   a camera that IS listed is launched as before (the D86 back-off for one
//!   that then fails at start is `tests/capture_absent_backoff.rs`).
//! - B6: after a sensor change the reconcile re-renders OUR `.gst.direct`
//!   (recognised by the camera name the file carries: the software and the
//!   hardware render, and a legacy x264enc one-liner) and never an
//!   operator's pipeline, a hand-written v4l2h264enc one-liner included.
//! - The mismatch warning compares with the declared sensor (config.txt
//!   line, then model) and takes libcamera's variant names for the sensor.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use xorgate_agent::ctx::NoopHooks;
use xorgate_agent::media::{reconcile_hardware, ReconcileOpts};
use xorgate_agent::recording::hooks::{HardwareReconciler, MediaReconciler, StreamHardware};
use xorgate_agent::recording::policy_sources::{
    cameras_fn, capture_held_warning, hold_cameras_without_a_sensor,
};
use xorgate_core::hardware_block::{parse_hardware_block, DeviceHardwareBlock, EncoderToken};
use xorgate_core::pipeline_builder::{build_bridged_kvs_pipeline, build_direct_kvs_pipeline};
use xorgate_core::stream_guard::StreamHardwareStatus;
use xorgate_core::video_capture::{resolve_bridged_cameras, CaptureCamera, KvsChannel};
use xorgate_hal::exec::RunResult;
use xorgate_hal::io::null_tape;

const IMX290_CM4: &str = "/base/soc/i2c0mux/i2c@0/imx290@1a";
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

/// The CM4 bench's hardware block: one camera, and the MODEL says imx290.
fn cm4_block() -> DeviceHardwareBlock {
    parse_hardware_block(&json!({
        "v": 2, "platform": "rpi-cm4", "encoder": "hw-h264", "rails": false,
        "statusLed": null, "imu": "bno085",
        "video": [{"key": "cam0", "connector": "csi0", "sensor": "imx290", "codec": "h264"}]
    }))
    .unwrap()
}

type Logs = Arc<Mutex<Vec<(String, String)>>>;

fn opts(dir: &Path, listing: RunResult, logs: &Logs, boot_config: Option<&str>) -> ReconcileOpts {
    let sink = logs.clone();
    ReconcileOpts {
        pipeline_dir: dir.to_path_buf(),
        shm_socket_path_for: Arc::new(|k| format!("/var/lib/xorgate-rs/shm/{k}.sock")),
        list_exec: Some(Arc::new(move || {
            let r = listing.clone();
            Box::pin(async move { r })
        })),
        log: Some(Arc::new(move |level, msg| {
            sink.lock()
                .unwrap()
                .push((level.as_str().to_string(), msg.to_string()));
        })),
        boot_config: boot_config.map(str::to_string),
    }
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

/// The shm socket root [`opts`] renders with.
const SHM: &str = "/var/lib/xorgate-rs/shm";

/// The trio the agent rendered for `camera` on a CM4 (hardware encoder),
/// its bridged `.gst` reading `<shm>/<key>.sock`.
fn write_trio(dir: &Path, key: &str, camera: &str, shm: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let sock = format!("{shm}/{key}.sock");
    let gst = build_bridged_kvs_pipeline(EncoderToken::HwH264, &sock);
    std::fs::write(dir.join(format!("{key}.gst")), format!("{gst}\n")).unwrap();
    std::fs::write(dir.join(format!("{key}.camera")), format!("{camera}\n")).unwrap();
    let direct = build_direct_kvs_pipeline(EncoderToken::HwH264, key, camera);
    std::fs::write(dir.join(format!("{key}.gst.direct")), format!("{direct}\n")).unwrap();
}

// ---------------------------------------------------------------------------
// B5
// ---------------------------------------------------------------------------

/// The CM4 on 2026-11-21 with a wrong `imx219` overlay: nothing enumerates,
/// the reconcile says "stream held down", and v0.1.1 still launched a
/// capture for `…/imx708@1a` from the stale files (`Could not find a camera
/// named '/base/soc/i2c0mux/i2c@0/imx708@1a'`).
#[tokio::test]
async fn b5_no_capture_camera_for_a_port_that_enumerated_nothing() {
    let home = common::scratch("cam-truth-b5");
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
    // What the previous sensor (an IMX708) left behind.
    // (`MediaReconciler` renders with `$XORGATE_HOME/shm`.)
    write_trio(
        &pipelines,
        "cam0",
        IMX708_CM4,
        &home.join("shm").display().to_string(),
    );
    let before = (
        read(&pipelines, "cam0.gst"),
        read(&pipelines, "cam0.camera"),
        read(&pipelines, "cam0.gst.direct"),
    );
    ctx.hardware.send_replace(Some(cm4_block()));

    let hw = Arc::new(StreamHardware::default());
    let cameras = cameras_fn(&ctx, Some(hw.clone()));
    let stale = vec![CaptureCamera {
        stream_key: "cam0".into(),
        camera_name: IMX708_CM4.into(),
    }];

    // No reconcile pass yet: nothing is known, so nothing is held (the
    // resolver's answer, as before).
    assert_eq!(cameras(), stale);
    // Without the verdicts (the TS behaviour) it never holds.
    assert_eq!(cameras_fn(&ctx, None)(), stale);

    // The boot pass: no camera enumerates.
    let rec = |listing: RunResult| MediaReconciler {
        ctx: ctx.clone(),
        stream_hw: hw.clone(),
        pipeline_dir: pipelines.clone(),
        list_exec: Arc::new(move || {
            let r = listing.clone();
            Box::pin(async move { r })
        }),
        boot_config: None,
    };
    rec(bench_listing("no camera at all, as the benches print it"))
        .reconcile()
        .await
        .unwrap();
    assert!(hw.armed());
    assert_eq!(hw.status("cam0"), Some(StreamHardwareStatus::NoCamera));
    assert!(!hw.capture_allowed("cam0"));
    let snap = hw.snapshot();
    assert_eq!((snap.passes, snap.cameras), (1, Some(vec![])));

    // B5: no capture camera for it …
    assert_eq!(cameras(), vec![]);
    // … although the files still name the old sensor (the reconcile leaves
    // a camera-less stream's files alone, as the TS does) …
    assert_eq!(
        (
            read(&pipelines, "cam0.gst"),
            read(&pipelines, "cam0.camera"),
            read(&pipelines, "cam0.gst.direct"),
        ),
        before
    );
    // … and clearing `<key>.camera` would NOT have been enough: the resolver
    // falls back to the name inside `<key>.gst.direct`.
    std::fs::remove_file(pipelines.join("cam0.camera")).unwrap();
    let channels = [KvsChannel {
        stream_key: "cam0".into(),
        channel_name: "dev-cam0".into(),
    }];
    let dir = pipelines.display().to_string();
    let (resolved, _) =
        resolve_bridged_cameras(&dir, &channels, &|p: &str| std::fs::read_to_string(p).ok());
    assert_eq!(
        resolved, stale,
        "the resolver alone still finds the stale name"
    );
    assert_eq!(cameras(), vec![]);

    // A pass that cannot enumerate at all keeps the last verdicts (and the
    // hold): the guard reads the last pass that could.
    rec(bench_listing("libcamera broken"))
        .reconcile()
        .await
        .unwrap();
    assert_eq!(cameras(), vec![]);
    let snap = hw.snapshot();
    assert_eq!((snap.passes, snap.cameras), (2, None));

    // The right overlay and a reboot later the IMX708 enumerates: the
    // camera is launched again, from the fresh identity.
    rec(bench_listing("CM4, IMX708 on CAM0"))
        .reconcile()
        .await
        .unwrap();
    assert_eq!(hw.status("cam0"), Some(StreamHardwareStatus::Rendered));
    assert_eq!(cameras(), stale);
    assert_eq!(read(&pipelines, "cam0.camera"), format!("{IMX708_CM4}\n"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn b5_only_the_held_keys_leave_the_set() {
    let cam = |k: &str| CaptureCamera {
        stream_key: k.into(),
        camera_name: format!("/c/{k}"),
    };
    let (keep, held) =
        hold_cameras_without_a_sensor(vec![cam("cam0"), cam("cam1")], |k| k != "cam0");
    assert_eq!(keep, vec![cam("cam1")]);
    assert_eq!(held, vec!["cam0".to_string()]);
    let (keep, held) = hold_cameras_without_a_sensor(vec![cam("cam0"), cam("cam1")], |_| true);
    assert_eq!(keep, vec![cam("cam0"), cam("cam1")]);
    assert!(held.is_empty());
    assert_eq!(
        capture_held_warning("cam0"),
        "[video-capture] cam0: no camera enumerated on its port — capture not started (the camera name left in cam0.camera / cam0.gst.direct is stale)"
    );
}

// ---------------------------------------------------------------------------
// B6
// ---------------------------------------------------------------------------

/// The CM4 moved from an IMX290 to an IMX708: v0.1.1 refreshed `.camera`
/// and left `camera-name=…/imx290@1a` in `cam0.gst.direct`.
#[tokio::test]
async fn b6_a_sensor_change_rerenders_our_direct_backup() {
    let root = common::scratch("cam-truth-b6");
    let dir = root.join("pipelines");
    let block = cm4_block();
    let want_direct = format!(
        "{}\n",
        build_direct_kvs_pipeline(EncoderToken::HwH264, "cam0", IMX708_CM4)
    );
    let legacy_hw =
        format!("libcamerasrc camera-name={IMX290_CM4} ! video/x-raw,width=1280,height=720 ! videoconvert ! v4l2h264enc ! appsink sync=TRUE emit-signals=TRUE name=appsink-video\n");
    let legacy_sw =
        format!("libcamerasrc camera-name={IMX290_CM4} ! video/x-raw,width=1280,height=720 ! x264enc bitrate=1000 ! appsink sync=TRUE emit-signals=TRUE name=appsink-video\n");
    let sw_exact = format!(
        "{}\n",
        build_direct_kvs_pipeline(EncoderToken::SwH264, "cam0", IMX290_CM4)
    );
    let hw_exact = format!(
        "{}\n",
        build_direct_kvs_pipeline(EncoderToken::HwH264, "cam0", IMX290_CM4)
    );
    // Every variant of OUR render for the previous camera is replaced.
    for (what, old_direct) in [
        ("hardware render", &hw_exact),
        ("software render", &sw_exact),
        ("legacy x264enc render", &legacy_sw),
    ] {
        let _ = std::fs::remove_dir_all(&dir);
        write_trio(&dir, "cam0", IMX290_CM4, SHM);
        std::fs::write(dir.join("cam0.gst.direct"), old_direct).unwrap();
        let gst_before = read(&dir, "cam0.gst");
        let logs: Logs = Arc::default();
        let res = reconcile_hardware(
            &block,
            opts(&dir, bench_listing("CM4, IMX708 on CAM0"), &logs, None),
        )
        .await
        .unwrap();
        assert_eq!(read(&dir, "cam0.gst.direct"), want_direct, "{what}");
        assert_eq!(
            read(&dir, "cam0.camera"),
            format!("{IMX708_CM4}\n"),
            "{what}"
        );
        // The bridged `.gst` has no camera in it: untouched, no master bounce.
        assert_eq!(read(&dir, "cam0.gst"), gst_before, "{what}");
        assert!(res.changed_streams.is_empty(), "{what}");
        assert_eq!(
            res.statuses.get("cam0"),
            Some(&StreamHardwareStatus::Rendered),
            "{what}"
        );
    }

    // An operator's direct pipeline is never overwritten, sensor change or not.
    for custom in [
        "libcamerasrc ! my direct ! appsink\n".to_string(),
        format!("libcamerasrc camera-name={IMX290_CM4} ! tee name=t\nt. ! x264enc ! appsink\n"),
        format!("libcamerasrc camera-name={IMX290_CM4} ! jpegenc ! appsink\n"),
        format!("videotestsrc camera-name={IMX290_CM4} ! x264enc ! appsink\n"),
        // a hand-written CM4 one-liner (v4l2h264enc): the legacy clause is
        // not widened to it
        legacy_hw.clone(),
        "libcamerasrc camera-name=/base/soc/i2c0mux/i2c@0/imx708@1a af-mode=continuous ! video/x-raw,width=1920,height=1080 ! videoconvert ! v4l2h264enc extra-controls=\"controls,video_bitrate=4000000\" ! video/x-h264,level=(string)4 ! appsink name=appsink-video\n".to_string(),
    ] {
        let _ = std::fs::remove_dir_all(&dir);
        write_trio(&dir, "cam0", IMX290_CM4, SHM);
        std::fs::write(dir.join("cam0.gst.direct"), &custom).unwrap();
        let logs: Logs = Arc::default();
        reconcile_hardware(
            &block,
            opts(&dir, bench_listing("CM4, IMX708 on CAM0"), &logs, None),
        )
        .await
        .unwrap();
        assert_eq!(read(&dir, "cam0.gst.direct"), custom);
    }

    // The same camera as before: nothing is written (a healthy device).
    let _ = std::fs::remove_dir_all(&dir);
    write_trio(&dir, "cam0", IMX708_CM4, SHM);
    let logs: Logs = Arc::default();
    let res = reconcile_hardware(
        &block,
        opts(
            &dir,
            bench_listing("CM4, IMX708 on CAM0"),
            &logs,
            Some("dtoverlay=imx708,cam0\n"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(res.statuses.get("cam0"), Some(&StreamHardwareStatus::Ok));
    assert!(logs.lock().unwrap().is_empty(), "no write, no warning");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// The mismatch warning
// ---------------------------------------------------------------------------

async fn warnings(listing: RunResult, boot_config: Option<&str>) -> Vec<String> {
    let root = common::scratch("cam-truth-warn");
    let logs: Logs = Arc::default();
    reconcile_hardware(
        &cm4_block(),
        opts(&root.join("pipelines"), listing, &logs, boot_config),
    )
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    let warns = logs
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, _)| level == "warn")
        .map(|(_, msg)| msg.clone())
        .collect();
    warns
}

#[tokio::test]
async fn mismatch_warning_compares_with_the_declared_sensor() {
    let imx708 = || bench_listing("CM4, IMX708 on CAM0");
    // The CM4 bench: config.txt declares imx708, the model says imx290, an
    // imx708 enumerates. Declared = the config.txt line: no warning.
    assert!(
        warnings(imx708(), Some("dtoverlay=imx708,cam0,rotation=180\n"))
            .await
            .is_empty()
    );
    // No config.txt known: the model's word, the TS line word for word.
    assert_eq!(
        warnings(imx708(), None).await,
        ["cam0: model expects imx290 on csi0, enumerated imx708 — using it anyway"]
    );
    // config.txt has no line for the port: the model again.
    assert_eq!(
        warnings(imx708(), Some("dtoverlay=vc4-kms-v3d\ndtoverlay=imx290\n")).await,
        ["cam0: model expects imx290 on csi0, enumerated imx708 — using it anyway"]
    );
    // config.txt declares another sensor than the one that enumerated.
    assert_eq!(
        warnings(imx708(), Some("dtoverlay=imx219,cam0\n")).await,
        ["cam0: config.txt declares imx219 on csi0, enumerated imx708 — using it anyway"]
    );
    // libcamera's variant names are the declared sensor.
    for variant in ["imx708_wide", "imx708_noir", "imx708_wide_noir"] {
        let mut l = imx708();
        l.stdout = l.stdout.replace("0 : imx708 ", &format!("0 : {variant} "));
        assert!(l.stdout.contains(variant));
        assert!(
            warnings(l, Some("dtoverlay=imx708,cam0\n"))
                .await
                .is_empty(),
            "{variant}"
        );
    }
    // An `imx462` line lists as `imx290` (one driver): not a mismatch.
    let mut l = imx708();
    l.stdout = l.stdout.replace("imx708", "imx290");
    assert!(warnings(l, Some("dtoverlay=imx462,cam0\n"))
        .await
        .is_empty());
}
