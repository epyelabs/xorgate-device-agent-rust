//! Replays `test/telemetry-scope.test.ts` (20 `it()`s) as op scripts against
//! `xorgate_core::telemetry_scope::TelemetryScope`. Every call the vitest made
//! and the exact value the TS class returned were recorded by a throwaway
//! tsx script (`fixtures/telemetry_scope/telemetry-scope.json`); the Rust port
//! must return the identical `ScopeChange` / topic / report at every step.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::telemetry_scope::{
    ScopeChange, TelemetryScope, TelemetryScopeOptions, TelemetryScopeReport, TenantScope,
};

#[derive(Deserialize)]
struct Opts {
    #[serde(rename = "graceMs")]
    grace_ms: Option<i64>,
    #[serde(rename = "reprobeAfterMs")]
    reprobe_after_ms: Option<i64>,
    #[serde(rename = "maxReprobeMs")]
    max_reprobe_ms: Option<i64>,
}

#[derive(Deserialize)]
struct Op {
    op: String,
    scope: Option<TenantScope>,
    at: Option<i64>,
    n: Option<u32>,
    #[serde(default)]
    expect: Value,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    opts: Opts,
    ops: Vec<Op>,
}

fn change_json(c: &Option<ScopeChange>) -> Value {
    match c {
        None => Value::Null,
        Some(c) => serde_json::to_value(c).unwrap(),
    }
}

#[test]
fn telemetry_scope_test_ts() {
    let f = common::fixture::<Case>("telemetry_scope/telemetry-scope.json");
    let device = f.extra["device"].as_str().unwrap().to_string();
    let unscoped = f.extra["unscopedTopic"].as_str().unwrap().to_string();
    for c in &f.cases {
        let mut s = TelemetryScope::new(TelemetryScopeOptions {
            unscoped_topic: unscoped.clone(),
            device_id: device.clone(),
            grace_ms: c.opts.grace_ms,
            reprobe_after_ms: c.opts.reprobe_after_ms,
            max_reprobe_ms: c.opts.max_reprobe_ms,
            ..Default::default()
        });
        for (i, op) in c.ops.iter().enumerate() {
            let ctx = format!("{} op#{i} {}", c.name, op.op);
            match op.op.as_str() {
                "applyScope" => {
                    let got = s.apply_scope(op.scope.clone().unwrap(), op.at.unwrap());
                    assert_eq!(change_json(&got), op.expect, "{ctx}");
                }
                "connect" => s.note_connect(op.at.unwrap()),
                "publish" => {
                    for _ in 0..op.n.unwrap_or(1) {
                        s.note_publish();
                    }
                }
                "close" => {
                    let got = s.note_close(op.at.unwrap());
                    assert_eq!(change_json(&got), op.expect, "{ctx}");
                }
                "tick" => {
                    let got = s.note_tick(op.at.unwrap());
                    assert_eq!(change_json(&got), op.expect, "{ctx}");
                }
                "topic" => assert_eq!(Value::String(s.topic()), op.expect, "{ctx}"),
                "fallenBack" => assert_eq!(Value::Bool(s.fallen_back()), op.expect, "{ctx}"),
                "report" => {
                    let r = s.report();
                    assert_eq!(serde_json::to_value(&r).unwrap(), op.expect, "{ctx}");
                    // Serde exactness: the report block reaches the wire inside
                    // `effective.telemetryScope`.
                    let back: TelemetryScopeReport =
                        serde_json::from_value(op.expect.clone()).unwrap();
                    assert_eq!(back, r, "{ctx}: round trip");
                }
                other => panic!("{ctx}: unknown op {other}"),
            }
        }
    }
    assert_eq!(f.cases.len(), 20);
}
