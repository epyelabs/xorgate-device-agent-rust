//! Fixture-driven tests for `xorgate_core::sky_state` — the 8 `createSkyTracker`
//! cases of `test/sky-state.test.ts` (+2 extras) as op scripts on a hand-advanced
//! monotonic clock, the `SKY_DEFAULTS` / `SKY_STATE_CODES` tables, and the
//! `gnss-sky.json` document shape (`collectors/gps.ts:243-252`).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::nmea::parse_gsv;
use xorgate_core::sky_state::*;

#[derive(Deserialize)]
#[serde(tag = "op")]
enum Op {
    #[serde(rename = "feed")]
    Feed { lines: Vec<String> },
    #[serde(rename = "advance")]
    Advance { ms: u64 },
    #[serde(rename = "snapshot")]
    Snapshot { at: u64, expected: Value },
}

#[derive(Deserialize)]
struct Case {
    name: String,
    #[serde(rename = "startMono")]
    start_mono: u64,
    ops: Vec<Op>,
}

#[derive(Deserialize)]
struct FileCase {
    name: String,
    snapshot: SkySnapshot,
    #[serde(rename = "timestampMs")]
    timestamp_ms: i64,
    #[serde(rename = "expectedJson")]
    expected_json: String,
}

#[test]
fn sky_state_test_ts_tracker() {
    let f = common::fixture::<Case>("sky_state/sky-state.json");
    let mut snapshots = 0;
    for c in &f.cases {
        let mut mono = c.start_mono;
        let mut t = SkyTracker::with_defaults(mono);
        for op in &c.ops {
            match op {
                Op::Feed { lines } => {
                    for line in lines {
                        if let Some(msg) = parse_gsv(line) {
                            t.note(&msg, mono);
                        }
                    }
                }
                Op::Advance { ms } => mono += ms,
                Op::Snapshot { at, expected } => {
                    assert_eq!(*at, mono, "{}: clock drift in the script", c.name);
                    let snap = t.snapshot(mono);
                    assert_eq!(
                        serde_json::to_value(snap).unwrap(),
                        *expected,
                        "{}: snapshot at {mono}",
                        c.name
                    );
                    snapshots += 1;
                }
            }
        }
    }
    assert_eq!(f.cases.len(), 10);
    assert_eq!(snapshots, 26);
}

#[test]
fn sky_state_tables_and_file_shape() {
    let f = common::fixture::<Case>("sky_state/sky-state.json");
    assert_eq!(
        serde_json::to_value(SKY_DEFAULTS).unwrap(),
        f.extra["skyDefaults"],
        "SKY_DEFAULTS"
    );
    let codes: serde_json::Map<String, Value> = SKY_STATE_CODES
        .iter()
        .map(|(s, c)| (s.as_str().to_string(), Value::from(*c)))
        .collect();
    assert_eq!(
        Value::Object(codes),
        f.extra["skyStateCodes"],
        "SKY_STATE_CODES"
    );

    let files: Vec<FileCase> = serde_json::from_value(f.extra["gnssSkyFileCases"].clone()).unwrap();
    assert_eq!(files.len(), 4);
    for c in &files {
        let doc = GnssSkyFile::from_snapshot(&c.snapshot, c.timestamp_ms);
        assert_eq!(
            serde_json::to_string(&doc).unwrap(),
            c.expected_json,
            "{}",
            c.name
        );
        let back: GnssSkyFile = serde_json::from_str(&c.expected_json).unwrap();
        assert_eq!(back, doc, "{}: round trip", c.name);
    }
}
