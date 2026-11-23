//! WP1 × WP5 wiring: `reconcileHardwareNow` (`ts:src/worker.ts:132-150` @
//! 89b5c64) and the media spawn guard closure (`worker.ts:205-218`) over the
//! REAL `reconcile_hardware` with a fake `rpicam-hello` listing. Listings,
//! log texts and statuses are the CF26 cases of
//! `fixtures/hardware_reconcile/reconcile.json` (tsx-dumped by WP5).
//!
//! - guard open (`"ok"`) until a pass that could enumerate (armed);
//! - armed: no status / `no-camera` → `no-camera`; else the pipeline resolves → `ok`, missing → `no-pipeline`;
//! - every reconcile log line → a `hardware: …` state event at its level;
//! - `shmSocketPathFor` = `$XORGATE_HOME/shm/<key>.sock` in the rendered `.gst`;
//! - offline / no hardware block → no pass at all (the listing is never run).

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;
use xorgate_agent::ctx::NoopHooks;
use xorgate_agent::media::{ListExec, MediaConfig, StreamGuard};
use xorgate_agent::recording::hooks::{
    stream_guard_fn, HardwareReconciler, MediaReconciler, StreamHardware,
};
use xorgate_core::hardware_block::parse_hardware_block;
use xorgate_hal::exec::RunResult;
use xorgate_hal::io::null_tape;

fn fixture_case(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hardware_reconcile/reconcile.json");
    let f: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no case {name}"))
        .clone()
}

fn listing(v: &Value) -> RunResult {
    RunResult {
        stdout: v["stdout"].as_str().unwrap().into(),
        stderr: v["stderr"].as_str().unwrap().into(),
        exit_code: v["exitCode"].as_i64().map(|c| c as i32),
        failed: v["failed"].as_bool().unwrap(),
        not_found: v["notFound"].as_bool().unwrap(),
        spawn_failed: v["notFound"].as_bool().unwrap(),
    }
}

fn counting_exec(r: RunResult, runs: Arc<AtomicUsize>) -> ListExec {
    Arc::new(move || {
        runs.fetch_add(1, Ordering::SeqCst);
        let r = r.clone();
        Box::pin(async move { r })
    })
}

/// `recentEvents` of the flushed state.json as `[level, msg]`.
fn events(state_path: &Path) -> Vec<(String, String)> {
    let v: Value = serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
    v["recentEvents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["level"].as_str().unwrap().to_string(),
                e["msg"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

const NO_CAMERA_CASE: &str = "reports no-camera when the connector is empty, touching nothing";
const UNAVAILABLE_CASE: &str = "touches nothing when enumeration is unavailable";

#[tokio::test]
async fn reconcile_arms_the_guard_and_journals_hardware_events() {
    let home = common::scratch("wp1-media-wiring");
    let ctx = common::test_ctx(&home, "dev", Arc::new(NoopHooks), null_tape());
    let pipelines = home.join("pipelines");
    let mut mcfg = MediaConfig::from_env(&ctx.env, false);
    mcfg.pipeline_dir = pipelines.display().to_string();
    mcfg.pipeline_path = None;

    let good = fixture_case(NO_CAMERA_CASE);
    let block = parse_hardware_block(&good["input"]["block"]).unwrap();
    let hw = Arc::new(StreamHardware::default());
    let guard = stream_guard_fn(ctx.hardware.clone(), hw.clone(), &mcfg);

    // No hardware block → guard open, reconcile is a no-op (listing never run).
    let runs = Arc::new(AtomicUsize::new(0));
    let unavailable = fixture_case(UNAVAILABLE_CASE);
    let rec = |exec: ListExec| MediaReconciler {
        ctx: ctx.clone(),
        stream_hw: hw.clone(),
        pipeline_dir: pipelines.clone(),
        list_exec: exec,
        boot_config: None,
        requirements: None,
    };
    let r0 = rec(counting_exec(
        listing(&unavailable["passes"][0]["listing"]),
        runs.clone(),
    ));
    assert!(r0.reconcile().await.unwrap().is_empty());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(guard("cam0"), StreamGuard::Ok);

    // Enumeration unavailable: one warn event, guard stays UNARMED (open).
    ctx.hardware.send_replace(Some(block.clone()));
    assert!(r0.reconcile().await.unwrap().is_empty());
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert!(!hw.armed());
    assert_eq!(guard("cam0"), StreamGuard::Ok);
    assert_eq!(guard("orphan"), StreamGuard::Ok);
    let want_warn = unavailable["passes"][0]["expected"]["logs"][0][1]
        .as_str()
        .unwrap();
    assert_eq!(
        events(&ctx.paths.state).last().unwrap(),
        &("warn".to_string(), format!("hardware: {want_warn}"))
    );

    // cam0 enumerated, cam1's connector empty: armed, verdicts per stream.
    let r1 = rec(counting_exec(
        listing(&good["passes"][0]["listing"]),
        runs.clone(),
    ));
    let changed = r1.reconcile().await.unwrap();
    assert_eq!(changed, vec!["cam0".to_string()]);
    assert!(hw.armed());
    assert_eq!(guard("cam0"), StreamGuard::Ok);
    assert_eq!(guard("cam1"), StreamGuard::NoCamera);
    assert_eq!(guard("orphan"), StreamGuard::NoCamera); // not in the block
    let evs = events(&ctx.paths.state);
    let want: Vec<(String, String)> = good["passes"][0]["expected"]["logs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l[0].as_str().unwrap().to_string(),
                format!("hardware: {}", l[1].as_str().unwrap()),
            )
        })
        .collect();
    assert_eq!(evs[evs.len() - want.len()..], want[..]);
    // shmSocketPathFor = `$XORGATE_HOME/shm/<key>.sock`.
    let gst = std::fs::read_to_string(pipelines.join("cam0.gst")).unwrap();
    let sock = ctx.paths.home.join("shm").join("cam0.sock");
    assert!(gst.contains(&sock.display().to_string()), "{gst}");

    // The pipeline vanishes → `no-pipeline` (resolved at guard time).
    std::fs::remove_file(pipelines.join("cam0.gst")).unwrap();
    assert_eq!(guard("cam0"), StreamGuard::NoPipeline);

    // A later unavailable pass keeps the last armed verdicts (TS: only a pass
    // with `cameras !== null` replaces the map).
    assert!(r0.reconcile().await.unwrap().is_empty());
    assert!(hw.armed());
    assert_eq!(guard("cam1"), StreamGuard::NoCamera);

    // The hardware block goes away → the guard opens again.
    ctx.hardware.send_replace(None);
    assert_eq!(guard("cam1"), StreamGuard::Ok);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn offline_skips_the_reconcile() {
    let home = common::scratch("wp1-media-wiring-offline");
    let mut env = common::test_env(&home, "dev");
    env.config.offline = true;
    let mut ctx = common::test_ctx(&home, "dev", Arc::new(NoopHooks), null_tape());
    ctx.env = Arc::new(env);
    let good = fixture_case(NO_CAMERA_CASE);
    ctx.hardware
        .send_replace(Some(parse_hardware_block(&good["input"]["block"]).unwrap()));
    let runs = Arc::new(AtomicUsize::new(0));
    let hw = Arc::new(StreamHardware::default());
    let rec = MediaReconciler {
        ctx: ctx.clone(),
        stream_hw: hw.clone(),
        pipeline_dir: home.join("pipelines"),
        list_exec: counting_exec(listing(&good["passes"][0]["listing"]), runs.clone()),
        boot_config: None,
        requirements: None,
    };
    assert!(rec.reconcile().await.unwrap().is_empty());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(!hw.armed());
    assert!(!home.join("pipelines").exists());
    let _ = std::fs::remove_dir_all(&home);
}
