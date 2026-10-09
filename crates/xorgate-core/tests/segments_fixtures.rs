//! Fixture-driven tests for `xorgate_core::segments`, from
//! `test/recording-segments.test.ts` (9 it()s, all F): the dumper drove the
//! real `SegmentWriter` / `finalizeOrphanSegments` over a tmp tree and
//! recorded per-step `activePath` + `closed` and the final tree (gz files
//! decoded). Here the same scripts drive `SegmentWriterCore`, whose
//! `SegmentOp`s are applied to an in-memory tree, and the orphan sweep is
//! replayed through `orphan_entry_action` + `finalize_plan`.

mod common;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::segments::*;
use xorgate_core::types::TimeSource;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

fn de<T: for<'de> Deserialize<'de>>(v: &Value) -> T {
    serde_json::from_value(v.clone()).unwrap()
}

/// What `listTree` printed: gz files decoded, a non-gzip `.gz` as `$raw:…`.
fn tree_text(v: &str) -> &str {
    v.strip_prefix("$raw:").unwrap_or(v)
}

fn apply(ops: Vec<SegmentOp>, files: &mut BTreeMap<String, String>, closed: &mut Vec<String>) {
    for op in ops {
        match op {
            SegmentOp::Open { path, line } => {
                assert!(!files.contains_key(&path), "open over an existing file");
                files.insert(path, format!("{line}\n"));
            }
            SegmentOp::Append { path, line } => {
                files
                    .get_mut(&path)
                    .expect("append to open file")
                    .push_str(&format!("{line}\n"));
            }
            SegmentOp::Close { plain, gz } => {
                let content = files.remove(&plain).expect("close an open file");
                if let Some(gz) = gz {
                    files.insert(gz.clone(), content);
                    closed.push(gz);
                }
            }
        }
    }
}

fn segment_writer(c: &Case) {
    let i = &c.input;
    let header: SegmentHeader = de(&i["header"]);
    // Header byte shape = what the TS wrote as line 1.
    assert_eq!(serde_json::to_value(&header).unwrap(), i["header"]);
    let header_fn = || header.clone();
    let ctor = &i["ctor"];
    let mut w = SegmentWriterCore::new(
        i["dir"].as_str().unwrap(),
        ctor.get("maxBytes").and_then(Value::as_u64),
        ctor.get("maxAgeMs").and_then(Value::as_u64),
    );
    let mut files = BTreeMap::new();
    let mut closed = Vec::new();
    let mut mono: u64 = 0;
    for (k, step) in c.expected["steps"].as_array().unwrap().iter().enumerate() {
        if let Some(m) = step.get("mono").and_then(Value::as_u64) {
            mono = m;
        }
        let ops = match step["op"].as_str().unwrap() {
            "writeLine" => {
                let line: SampleLine = de(&step["line"]);
                // Sample byte shape round trips.
                assert_eq!(serde_json::to_value(&line).unwrap(), step["line"]);
                w.write_line(&line, step["now"].as_i64().unwrap(), mono, &header_fn)
            }
            "noteTimeSource" => {
                let ts: TimeSource = de(&step["ts"]);
                w.note_time_source(ts, &header_fn)
            }
            "maybeRotate" => w.maybe_rotate(mono),
            "finalize" => w.finalize(),
            other => panic!("unknown step {other}"),
        };
        apply(ops, &mut files, &mut closed);
        assert_eq!(
            serde_json::to_value(w.active_path()).unwrap(),
            step["activePath"],
            "{} step {k}",
            c.name
        );
        assert_eq!(
            serde_json::to_value(&closed).unwrap(),
            step["closed"],
            "{} step {k}",
            c.name
        );
    }
    assert_eq!(
        serde_json::to_value(&closed).unwrap(),
        c.expected["closed"],
        "{}",
        c.name
    );
    let want: BTreeMap<String, String> = de(&c.expected["files"]);
    let want: BTreeMap<String, String> = want
        .iter()
        .map(|(k, v)| (k.clone(), tree_text(v).to_string()))
        .collect();
    assert_eq!(files, want, "{}", c.name);
}

fn finalize_orphans(c: &Case) {
    let root = c.input["telemetryRoot"].as_str().unwrap();
    let mut files: BTreeMap<String, String> = de(&c.input["files"]);
    let mut finalized = 0;
    let before: Vec<String> = files.keys().cloned().collect();
    for path in before {
        // `<root>/<session>/<name>`: the TS walks exactly that depth.
        let rel = path.strip_prefix(&format!("{root}/")).unwrap();
        let (_session, name) = rel.split_once('/').unwrap();
        assert!(!name.contains('/'));
        match orphan_entry_action(name) {
            OrphanEntryAction::DeleteTemp => {
                files.remove(&path);
            }
            OrphanEntryAction::Skip => {}
            OrphanEntryAction::Finalize => {
                let raw = files[&path].clone();
                match finalize_plan(raw.as_bytes()) {
                    OrphanPlan::Delete => {
                        files.remove(&path);
                    }
                    OrphanPlan::Finalize { keep_len } => {
                        files.remove(&path);
                        files.insert(format!("{path}.gz"), raw[..keep_len].to_string());
                        finalized += 1;
                    }
                }
            }
        }
    }
    assert_eq!(
        finalized,
        c.expected["finalized"].as_u64().unwrap(),
        "{}",
        c.name
    );
    let want: BTreeMap<String, String> = de(&c.expected["files"]);
    let want: BTreeMap<String, String> = want
        .iter()
        .map(|(k, v)| (k.clone(), tree_text(v).to_string()))
        .collect();
    assert_eq!(files, want, "{}", c.name);
}

#[test]
fn recording_segments_test_ts() {
    let f = common::fixture::<Case>("segments/recording-segments.json");
    for c in &f.cases {
        match c.op.as_str() {
            "segmentWriter" => segment_writer(c),
            "finalizeOrphanSegments" => finalize_orphans(c),
            other => panic!("unknown op {other}"),
        }
    }
    let vitest = f
        .cases
        .iter()
        .filter(|c| !c.name.starts_with("extra:"))
        .count();
    assert_eq!(vitest, 9);
    assert_eq!(SEGMENT_MAX_BYTES, 1024 * 1024);
    assert_eq!(SEGMENT_MAX_AGE_MS, 60_000);
}

#[test]
fn orphan_error_line_is_verbatim() {
    assert_eq!(
        orphan_finalize_failed_error("/r/t/s/seg-1-00000.jsonl", "EIO"),
        "[recording] orphan finalize failed for /r/t/s/seg-1-00000.jsonl: EIO"
    );
}

#[test]
fn sample_mono_prints_like_js_past_2_pow_53() {
    // `Number(process.hrtime.bigint())` after ~104 days: JSON.stringify prints
    // the zero-padded shortest digits, never an exponent.
    let line = SampleLine {
        ts: 1.0,
        mono: 2f64.powi(60),
        seq: 0.0,
        metrics: serde_json::Map::new(),
    };
    assert_eq!(
        serde_json::to_string(&line).unwrap(),
        r#"{"ts":1,"mono":1152921504606847000,"seq":0,"metrics":{}}"#
    );
}
