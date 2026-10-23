//! WP2: `DiskManager` over a real tmp tree, replaying
//! `fixtures/disk_plan/recording-disk.json` — the REAL `DiskManager` @
//! 89b5c64 run by `phase-3/evidence/wp2-tools/dump-disk.mts` (linux node:20)
//! over the same trees (the eight `test/recording-disk.test.ts` it()s + mixed
//! telemetry/video trees, odd counters files, equal-mtime ordering). Per
//! step: `stats()` after it; at the end: the whole remaining tree (files and
//! directories — eviction list, sidecars, pruned session dirs), the
//! `counters.json` BYTES, and whether the root exists (enforce never creates
//! the tree).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;
use xorgate_agent::recording::disk::{DiskManager, FreeBytesFn};
use xorgate_core::recording_config::ResolvedRetention;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-wp2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn put(root: &Path, f: &Value) {
    let p = root.join(f["rel"].as_str().unwrap());
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, vec![1u8; f["size"].as_u64().unwrap() as usize]).unwrap();
    let ms = f["mtimeMs"].as_f64().unwrap();
    let t = UNIX_EPOCH + Duration::from_micros((ms * 1000.0) as u64);
    let file = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
    file.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

fn tree(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
        if e.file_type().unwrap().is_dir() {
            out.push(format!("{rel}/"));
            tree(root, &p, out);
        } else {
            out.push(rel);
        }
    }
}

fn retention(v: &Value) -> ResolvedRetention {
    ResolvedRetention {
        max_bytes: v["maxBytes"].as_f64().unwrap(),
        min_free_bytes: v["minFreeBytes"].as_f64().unwrap(),
    }
}

fn stats_json(dm: &DiskManager) -> Value {
    serde_json::to_value(dm.stats()).unwrap()
}

#[tokio::test]
async fn disk_manager_matches_the_ts_over_the_same_trees() {
    let text = std::fs::read_to_string(format!(
        "{}/../../fixtures/disk_plan/recording-disk.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    assert!(
        [
            "09da38dcc8b75c248625a3a0bee43b70e7def55d",
            "89b5c6407ef88e20f7168bd767d22a7be04ebcb6"
        ]
        .contains(&doc["jsCommit"].as_str().unwrap()),
        "jsCommit {} (CF78)",
        doc["jsCommit"]
    );
    let cases = doc["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 12);
    for (ci, c) in cases.iter().enumerate() {
        let name = c["name"].as_str().unwrap();
        let input = &c["input"];
        let home = scratch(&format!("disk{ci}"));
        let root = home.join("recordings");
        for d in input["dirs"].as_array().into_iter().flatten() {
            std::fs::create_dir_all(root.join(d.as_str().unwrap())).unwrap();
        }
        for f in input["files"].as_array().unwrap() {
            put(&root, f);
        }
        if let Some(t) = input["counters"].as_str() {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("counters.json"), t).unwrap();
        }
        let free: Arc<Mutex<Option<f64>>> = Arc::new(Mutex::new(input["free"].as_f64()));
        let mut ret = retention(&input["retention"]);
        let mk = |ret: &ResolvedRetention| {
            let free = free.clone();
            let f: FreeBytesFn = Arc::new(move || {
                let v = *free.lock().unwrap();
                Box::pin(async move { v })
            });
            DiskManager::with_free_bytes(root.clone(), ret.clone(), f)
        };
        let mut dm = mk(&ret);
        let want_steps = c["expected"]["steps"].as_array().unwrap();
        for (i, s) in input["steps"].as_array().unwrap().iter().enumerate() {
            let want = &want_steps[i];
            match s["op"].as_str().unwrap() {
                "enforce" => {
                    dm.enforce().await;
                    assert_eq!(stats_json(&dm), want["stats"], "{name}: step {i}");
                }
                "stats" => assert_eq!(stats_json(&dm), want["stats"], "{name}: step {i}"),
                "writeCounters" => {
                    std::fs::write(root.join("counters.json"), s["text"].as_str().unwrap()).unwrap()
                }
                "add" => {
                    for f in s["files"].as_array().unwrap() {
                        put(&root, f);
                    }
                }
                "setActive" => {
                    let p = s["rel"].as_str().map(|r| root.join(r));
                    dm.set_active_path(s["owner"].as_str().unwrap(), p.as_deref());
                }
                "setFree" => *free.lock().unwrap() = s["value"].as_f64(),
                "setRetention" => {
                    ret = retention(s);
                    dm.set_retention(ret.clone());
                }
                "newManager" => {
                    dm = mk(&ret);
                    assert_eq!(stats_json(&dm), want["stats"], "{name}: step {i}");
                }
                other => panic!("op {other}"),
            }
        }
        let mut got = Vec::new();
        tree(&root, &root, &mut got);
        let want_tree: Vec<String> = serde_json::from_value(c["expected"]["tree"].clone()).unwrap();
        assert_eq!(got, want_tree, "{name}: tree");
        let counters = std::fs::read_to_string(root.join("counters.json")).ok();
        assert_eq!(
            counters.as_deref(),
            c["expected"]["counters"].as_str(),
            "{name}: counters.json bytes"
        );
        assert_eq!(
            root.exists(),
            c["expected"]["rootExists"].as_bool().unwrap(),
            "{name}: root exists"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// Overlapping `enforce()` calls collapse to one (`disk.ts:114`).
#[tokio::test]
async fn overlapping_enforce_collapses() {
    let home = scratch("overlap");
    let root = home.join("recordings");
    put(
        &root,
        &serde_json::json!({"rel": "telemetry/s/seg-1-00000.jsonl.gz", "size": 10, "mtimeMs": 1000}),
    );
    let gate = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (g, n) = (gate.clone(), calls.clone());
    let f: FreeBytesFn = Arc::new(move || {
        let g = g.clone();
        n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            g.notified().await;
            Some(1e12)
        })
    });
    let dm = Arc::new(DiskManager::with_free_bytes(
        root.clone(),
        ResolvedRetention {
            max_bytes: 1.0,
            min_free_bytes: 0.0,
        },
        f,
    ));
    let d1 = dm.clone();
    let first = tokio::spawn(async move { d1.enforce().await });
    tokio::task::yield_now().await;
    dm.enforce().await; // returns at once: the first holds the slot
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    gate.notify_one();
    first.await.unwrap();
    assert_eq!(dm.stats().dropped_segments, 1);
    let _ = std::fs::remove_dir_all(&home);
}

/// Captures `fs_out` as `"<op> <rel> <size>"`.
#[derive(Default)]
struct FsTape(Mutex<Vec<String>>);

impl xorgate_hal::io::TapeSink for FsTape {
    fn fs_out(&self, rel: &str, op: xorgate_hal::tape::FsOp, size: u64) {
        self.0
            .lock()
            .unwrap()
            .push(format!("{} {rel} {size}", op.as_str()));
    }
}

/// Every eviction (and the mp4's sidecar) is a tape `fs_out` `delete`,
/// relative to the agent home, oldest first. As in `disk.ts:121,143-151`
/// the 7-byte sidecar counts in `total` but is not subtracted when it goes:
/// 97 → 57 → 17 → 7, so the gz (17 > 15) is evicted too.
#[tokio::test]
async fn evictions_are_taped_as_fs_out_deletes() {
    let home = scratch("tape");
    let root = home.join("recordings");
    for f in [
        serde_json::json!({"rel": "video/cam0/s/00000.mp4", "size": 40, "mtimeMs": 1000}),
        serde_json::json!({"rel": "video/cam0/s/00000.json", "size": 7, "mtimeMs": 1000}),
        serde_json::json!({"rel": "video/cam0/s/00001.mp4", "size": 40, "mtimeMs": 2000}),
        serde_json::json!({"rel": "telemetry/s/seg-1-00000.jsonl.gz", "size": 10, "mtimeMs": 3000}),
    ] {
        put(&root, &f);
    }
    let tape = Arc::new(FsTape::default());
    let f: FreeBytesFn = Arc::new(|| Box::pin(async { Some(1e12) }));
    let dm = DiskManager::with_free_bytes(
        root.clone(),
        ResolvedRetention {
            max_bytes: 15.0,
            min_free_bytes: 0.0,
        },
        f,
    )
    .with_tape(tape.clone());
    dm.enforce().await;
    assert_eq!(
        *tape.0.lock().unwrap(),
        vec![
            "delete recordings/video/cam0/s/00000.mp4 40".to_string(),
            "delete recordings/video/cam0/s/00000.json 7".to_string(),
            "delete recordings/video/cam0/s/00001.mp4 40".to_string(),
            "delete recordings/telemetry/s/seg-1-00000.jsonl.gz 10".to_string(),
        ]
    );
    assert_eq!(dm.stats().dropped_segments, 3);
    let _ = std::fs::remove_dir_all(&home);
}
