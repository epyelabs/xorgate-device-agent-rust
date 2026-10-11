//! Fixture-driven test for `xorgate_core::disk_plan`, from the REAL
//! `DiskManager` (`disk.ts` @ 89b5c64, driver
//! `phase-3/evidence/wp2-tools/dump-disk.mts`, linux node:20): for every
//! `enforce()` the dump holds the scan listing the pass saw, the active set,
//! the free bytes and retention, and every `unlinkSync` in order. Here the
//! listing is replayed through `plan_eviction` (the eviction list = the
//! unlinks of scanned CLOSED files; the rest are `.mp4` sidecars) and
//! `backlog_after`, and the persisted `counters.json` bytes through
//! `counters_json` / `parse_counters`.

mod common;

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::disk_plan::*;
use xorgate_core::recording_config::ResolvedRetention;

#[derive(Deserialize)]
struct Case {
    name: String,
    expected: Value,
}

#[test]
fn eviction_plan_matches_the_ts_disk_manager() {
    let f = common::fixture::<Case>("disk_plan/recording-disk.json");
    assert_eq!(f.cases.len(), 12);
    let mut passes = 0;
    for c in &f.cases {
        for s in c.expected["steps"].as_array().unwrap() {
            if s["op"] != "enforce" {
                continue;
            }
            passes += 1;
            let files: Vec<ScannedFile> = serde_json::from_value(s["listing"].clone()).unwrap();
            // The scan's own filters.
            for fl in &files {
                let name = fl.path.rsplit('/').next().unwrap();
                assert!(is_scanned_name(name), "{}", c.name);
                assert_eq!(fl.closed, is_closed_name(name), "{}: {}", c.name, fl.path);
            }
            let active: HashSet<String> = serde_json::from_value(s["active"].clone()).unwrap();
            let free = s["free"].as_f64();
            let r = &s["retention"];
            let retention = ResolvedRetention {
                max_bytes: r["maxBytes"].as_f64().unwrap(),
                min_free_bytes: r["minFreeBytes"].as_f64().unwrap(),
            };
            let plan = plan_eviction(&files, &active, free, &retention);
            let got: Vec<&str> = plan.iter().map(|&i| files[i].path.as_str()).collect();
            let unlinks: Vec<String> = serde_json::from_value(s["unlinks"].clone()).unwrap();
            let want: Vec<&str> = unlinks
                .iter()
                .filter(|u| files.iter().any(|f| &f.path == *u && f.closed))
                .map(String::as_str)
                .collect();
            assert_eq!(got, want, "{}", c.name);
            // Every other unlink is the evicted mp4's sidecar, right after it.
            let mut expect_all = Vec::new();
            for p in &got {
                expect_all.push(p.to_string());
                if let Some(sc) = sidecar_of_mp4(p) {
                    if unlinks.contains(&sc) {
                        expect_all.push(sc);
                    }
                }
            }
            assert_eq!(expect_all, unlinks, "{}", c.name);
            let ev = evictable_order(&files, &active);
            let deleted: HashSet<usize> = plan.iter().copied().collect();
            let (n, b) = backlog_after(&files, &ev, &deleted);
            assert_eq!(
                n,
                s["stats"]["backlogSegments"].as_u64().unwrap(),
                "{}",
                c.name
            );
            assert_eq!(
                b,
                s["stats"]["backlogBytes"].as_u64().unwrap(),
                "{}",
                c.name
            );
        }
        if let Some(text) = c.expected["counters"].as_str() {
            let v = parse_counters(Some(text));
            if text.starts_with("{\"v\":1,\"droppedSegments\":") {
                assert_eq!(counters_json(v), text, "{}", c.name);
            }
        }
    }
    assert_eq!(passes, 17);
}
