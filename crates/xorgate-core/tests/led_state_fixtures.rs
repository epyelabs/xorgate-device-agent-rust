//! Fixture-driven tests for `xorgate_core::led_state`
//! (`test/led-state.test.ts`, all 31 `it()`s).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::led_state::*;
use xorgate_core::telemetry::{ConnectionState, RecordingStatus};
use xorgate_core::types::{GnssView, SkyState};

#[derive(Deserialize)]
struct Call {
    op: String,
    input: Value,
    expected: Value,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    constants: Vec<String>,
    calls: Vec<Call>,
}

/// The typed `RecordingStatus` the Rust agent hands the resolver. The TS
/// guards `typeof block?.reason === "string"` (`led-state.ts:150`); a
/// non-string reason is unrepresentable in the Rust type, so the harness
/// drops such a block exactly as that guard ignores it.
fn recording(v: &Value) -> RecordingStatus {
    let mut v = v.clone();
    if v.get("liveBlock").is_some_and(|b| !b["reason"].is_string()) {
        v.as_object_mut().unwrap().remove("liveBlock");
    }
    serde_json::from_value(v).unwrap()
}

/// `GnssView` from the test's literal. The suite uses `sky: "open"`, which is
/// not a `SkyState`; the resolver reads only `fix`, so an unknown sky maps to
/// `Unknown` here.
fn gnss(v: &Value) -> Option<GnssView> {
    if v.is_null() {
        return None;
    }
    Some(GnssView {
        fix: v["fix"].as_bool().unwrap(),
        sky: serde_json::from_value::<SkyState>(v["sky"].clone()).unwrap_or(SkyState::Unknown),
    })
}

#[test]
fn led_state_test_ts() {
    let f = common::fixture::<Case>("led_state/led-state.json");
    // The ladder table is pinned verbatim (key order included).
    assert_eq!(
        serde_json::to_value(LED_LADDER).unwrap(),
        f.extra["LED_LADDER"],
        "LED_LADDER"
    );
    let mut calls = 0;
    for c in &f.cases {
        for k in &c.constants {
            assert_eq!(k, "LED_LADDER", "{}", c.name);
        }
        assert!(!c.calls.is_empty() || !c.constants.is_empty(), "{}", c.name);
        for (i, call) in c.calls.iter().enumerate() {
            let at = format!("{} [call {i}]", c.name);
            let got = match call.op.as_str() {
                "rung" => serde_json::to_value(rung(call.input.as_str().unwrap())).unwrap(),
                "resolveLedState" => {
                    let s = &call.input;
                    let rec = recording(&s["recording"]);
                    let connection: ConnectionState =
                        serde_json::from_value(s["connection"].clone()).unwrap();
                    let signals = LedSignals {
                        connection,
                        recording: &rec,
                        lte: LteStatusView::from_value(&s["lte"]),
                        gnss: gnss(&s["gnss"]),
                        starting: s["starting"].as_bool().unwrap(),
                    };
                    serde_json::to_value(resolve_led_state(&signals)).unwrap()
                }
                other => panic!("{at}: unknown op {other}"),
            };
            assert_eq!(got, call.expected, "{at}");
            calls += 1;
        }
    }
    // The table assertions of the three "ladder table" it()s.
    let pri: Vec<&str> = LED_LADDER.iter().map(|r| r.pri).collect();
    assert_eq!(pri, ["0", "1", "2", "4", "5", "6", "7", "—"]);
    assert!(LED_LADDER.iter().all(|r| r.reason != "data-loss"));
    for r in &LED_LADDER {
        assert!(["red", "green", "blue", "purple", "cyan", "white"].contains(&r.colour.as_str()));
        assert!(!r.label.is_empty());
        assert_eq!(
            LED_LADDER.iter().filter(|x| x.reason == r.reason).count(),
            1
        );
    }
    assert_eq!(f.cases.len(), 31);
    assert_eq!(calls, 40);
}
