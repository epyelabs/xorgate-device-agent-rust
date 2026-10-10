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
            .await;
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
