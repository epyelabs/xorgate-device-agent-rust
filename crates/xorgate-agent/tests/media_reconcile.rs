//! `reconcile_hardware` against `fixtures/hardware_reconcile/reconcile.json`:
//! the REAL `reconcileHardware` (`src/media/hardware-reconcile.ts:77-182`)
//! run pass by pass on a tmp dir with a fake `listExec`
//! (`phase-3/evidence/wp5-tools/dump-hardware-reconcile.mts`).
//!
//! Covers the 4 async it()s of `test/hardware-block.test.ts` ›
//! reconcileHardware (CF26) and, for D4, the bench's two-camera CM5 block
//! with the bench listing under both socket roots: the renders are
//! byte-equal to the JS renders. Each pass compares the log calls (level +
//! text, in order), statuses (Map order), changedStreams, cameras and EVERY
//! file's bytes afterwards (or that the directory does not exist).
//!
//! Plus the M1 (review-wp5) parity cases: the first failed mkdir/write ends
//! the pass with `Err(<Node err.message>)` where the TS throws
//! (`hardware-reconcile.ts:108,146,160,170`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use xorgate_agent::media::{reconcile_hardware, ReconcileOpts};
use xorgate_core::hardware_block::parse_hardware_block;
use xorgate_hal::exec::RunResult;

fn fixture() -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hardware_reconcile/reconcile.json");
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-wp5-rec-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn snapshot(dir: &Path) -> Value {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Value::Null;
    };
    let mut files = BTreeMap::new();
    for e in rd {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        files.insert(name, Value::from(std::fs::read_to_string(&p).unwrap()));
    }
    Value::Object(files.into_iter().collect())
}

#[tokio::test]
async fn hardware_block_test_ts_reconcile_hardware_and_the_bench_block() {
    let f = fixture();
    let cases = f["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 10);
    let mut passes_run = 0;
    for (ci, c) in cases.iter().enumerate() {
        let name = c["name"].as_str().unwrap();
        let block = parse_hardware_block(&c["input"]["block"]).expect(name);
        let sock_root = c["input"]["sockRoot"].as_str().unwrap().to_string();
        let root = scratch(&format!("c{ci}"));
        let dir = root.join(c["input"]["dir"].as_str().unwrap());
        for (pi, p) in c["passes"].as_array().unwrap().iter().enumerate() {
            if let Some(pre) = p["pre"].as_object() {
                std::fs::create_dir_all(&dir).unwrap();
                for (file, content) in pre {
                    match content.as_str() {
                        Some(t) => std::fs::write(dir.join(file), t).unwrap(),
                        None => {
                            let _ = std::fs::remove_file(dir.join(file));
                        }
                    }
                }
            }
            let l = &p["listing"];
            let listing = RunResult {
                stdout: l["stdout"].as_str().unwrap().into(),
                stderr: l["stderr"].as_str().unwrap().into(),
                exit_code: l["exitCode"].as_i64().map(|c| c as i32),
                failed: l["failed"].as_bool().unwrap(),
                not_found: l["notFound"].as_bool().unwrap(),
                spawn_failed: l["notFound"].as_bool().unwrap(),
            };
            let logs: Arc<Mutex<Vec<Value>>> = Arc::default();
            let sink = logs.clone();
            let sr = sock_root.clone();
            let res = reconcile_hardware(
                &block,
                ReconcileOpts {
                    pipeline_dir: dir.clone(),
                    shm_socket_path_for: Arc::new(move |k| format!("{sr}/{k}.sock")),
                    list_exec: Some(Arc::new(move || {
                        let r = listing.clone();
                        Box::pin(async move { r })
                    })),
                    log: Some(Arc::new(move |level, msg| {
                        sink.lock().unwrap().push(json!([level.as_str(), msg]));
                    })),
                },
            )
            .await
            .expect("fixture passes never fail an fs write");
            let want = &p["expected"];
            let tag = format!("{name} pass {pi}");
            assert_eq!(
                Value::from(logs.lock().unwrap().clone()),
                want["logs"],
                "{tag}: logs"
            );
            let statuses: Vec<Value> = res
                .statuses
                .iter()
                .map(|(k, v)| json!([k, v.as_str()]))
                .collect();
            assert_eq!(Value::from(statuses), want["statuses"], "{tag}: statuses");
            assert_eq!(
                serde_json::to_value(&res.changed_streams).unwrap(),
                want["changedStreams"],
                "{tag}: changedStreams"
            );
            assert_eq!(
                serde_json::to_value(&res.cameras).unwrap(),
                want["cameras"],
                "{tag}: cameras"
            );
            assert_eq!(snapshot(&dir), want["files"], "{tag}: files");
            passes_run += 1;
        }
        let _ = std::fs::remove_dir_all(&root);
    }
    assert_eq!(passes_run, 16);
}

/// The bench CM5 two-camera block + listing (fixture case 4).
fn bench_case() -> (xorgate_core::hardware_block::DeviceHardwareBlock, RunResult) {
    let f = fixture();
    let c = &f["cases"][4];
    let block = parse_hardware_block(&c["input"]["block"]).unwrap();
    let l = &c["passes"][0]["listing"];
    let listing = RunResult {
        stdout: l["stdout"].as_str().unwrap().into(),
        stderr: String::new(),
        exit_code: Some(0),
        failed: false,
        not_found: false,
        spawn_failed: false,
    };
    (block, listing)
}

type Logs = Arc<Mutex<Vec<Value>>>;

fn opts(dir: &Path, listing: RunResult, logs: &Logs) -> ReconcileOpts {
    let sink = logs.clone();
    ReconcileOpts {
        pipeline_dir: dir.to_path_buf(),
        shm_socket_path_for: Arc::new(|k| format!("/var/lib/xorgate-rs/shm/{k}.sock")),
        list_exec: Some(Arc::new(move || {
            let r = listing.clone();
            Box::pin(async move { r })
        })),
        log: Some(Arc::new(move |level, msg| {
            sink.lock().unwrap().push(json!([level.as_str(), msg]));
        })),
    }
}

/// A write failing mid-pass (cam1's `.camera` is a directory → EISDIR) ends
/// the pass: Err with the Node message, no result (so no `rendered` status
/// and no `changed_streams` entry for cam1 — nor for cam0, whose renders the
/// TS also never reports), no `[hardware]` line for the failure, and the
/// files written before the throw stay on disk.
#[tokio::test]
async fn write_failure_mid_pass_is_an_error_like_the_ts_throw() {
    let (block, listing) = bench_case();
    let root = scratch("m1-write");
    let dir = root.join("pipelines");
    std::fs::create_dir_all(dir.join("cam1.camera")).unwrap();
    let logs: Logs = Arc::default();
    let res = reconcile_hardware(&block, opts(&dir, listing, &logs)).await;
    let cam_path = dir.join("cam1.camera");
    assert_eq!(
        res,
        Err(format!(
            "EISDIR: illegal operation on a directory, open '{}'",
            cam_path.display()
        ))
    );
    let logs = logs.lock().unwrap().clone();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert_eq!(logs[0][0], "info");
    assert!(logs[0][1]
        .as_str()
        .unwrap()
        .starts_with("cam0: pipelines rendered"));
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    // cam1.gst was written before the throw; cam1.gst.direct never was.
    assert_eq!(
        names,
        [
            "cam0.camera",
            "cam0.gst",
            "cam0.gst.direct",
            "cam1.camera",
            "cam1.gst"
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `mkdirSync(pipelineDir, { recursive: true })` failing (a path component
/// is a file → ENOTDIR) ends the pass before any stream is looked at.
#[tokio::test]
async fn mkdir_failure_is_an_error_like_the_ts_throw() {
    let (block, listing) = bench_case();
    let root = scratch("m1-mkdir");
    std::fs::write(root.join("file"), "x").unwrap();
    let dir = root.join("file").join("pipelines");
    let logs: Logs = Arc::default();
    let res = reconcile_hardware(&block, opts(&dir, listing, &logs)).await;
    assert_eq!(
        res,
        Err(format!(
            "ENOTDIR: not a directory, mkdir '{}'",
            dir.display()
        ))
    );
    assert!(logs.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}
