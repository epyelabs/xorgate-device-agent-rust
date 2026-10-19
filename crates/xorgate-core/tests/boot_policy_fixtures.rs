//! Fixture-driven tests for `xorgate_core::boot_policy`
//! (`test/recording-boot-policy.test.ts`, all 67 `it()`s). The V8 built-ins
//! it relies on are pinned in `js_fixtures.rs`.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::boot_policy::*;
use xorgate_core::clock::FixedClock;
use xorgate_core::types::{GnssView, PowerSampleReading, ShedConfig};

/// The fixture's monotonic stamp as a clock (P8: the policy reads `&dyn Clock`;
/// the wall half is never consulted by `LoadShedPolicy`).
fn at_mono(mono: u64) -> FixedClock {
    FixedClock::new(0, mono)
}

#[derive(Deserialize)]
struct Call {
    cfg: BootDeferConfig,
    input: BootDeferInputs,
    expected: Value,
}

#[derive(Deserialize)]
struct DecideCase {
    name: String,
    calls: Vec<Call>,
}

#[test]
fn decide_boot_defer_test_ts() {
    let f = common::fixture::<DecideCase>("boot_policy/decide-boot-defer.json");
    let mut calls = 0;
    for c in &f.cases {
        assert!(!c.calls.is_empty(), "{}", c.name);
        for (i, call) in c.calls.iter().enumerate() {
            let got = serde_json::to_value(decide_boot_defer(&call.cfg, &call.input)).unwrap();
            assert_eq!(got, call.expected, "{} [call {i}]", c.name);
            calls += 1;
        }
    }
    assert_eq!(f.cases.len(), 29);
    assert_eq!(calls, 51);
}

/// The recorded file contents at each evaluate, replayed through the pure
/// decode halves of the TS readers.
struct Snapshot {
    budget: Option<Vec<u8>>,
    telemetry: Option<String>,
}

impl BootDeferSources for Snapshot {
    fn read_budget_ma(&mut self) -> Option<u32> {
        self.budget.as_deref().and_then(parse_budget_ma)
    }
    fn read_telemetry(&mut self) -> Option<PolicyTelemetry> {
        self.telemetry.as_deref().and_then(parse_policy_telemetry)
    }
    fn gnss(&mut self) -> Option<GnssView> {
        None
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn boot_defer_policy_test_ts() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        ops: Vec<Value>,
    }
    let f = common::fixture::<Case>("boot_policy/boot-defer-policy.json");
    for c in &f.cases {
        let mut policies: Vec<BootDeferPolicy> = Vec::new();
        for (k, op) in c.ops.iter().enumerate() {
            let at = format!("{} [op {k}]", c.name);
            let inst = op["inst"].as_u64().unwrap() as usize;
            match op["op"].as_str().unwrap() {
                "new" => {
                    assert_eq!(inst, policies.len(), "{at}");
                    let clock = FixedClock::new(0, op["mono"].as_u64().unwrap());
                    policies.push(BootDeferPolicy::new(&clock, None));
                }
                "evaluate" => {
                    let cfg: BootDeferConfig = serde_json::from_value(op["cfg"].clone()).unwrap();
                    let clock =
                        FixedClock::new(op["now"].as_i64().unwrap(), op["mono"].as_u64().unwrap());
                    let mut src = Snapshot {
                        budget: op["budgetHex"].as_str().map(hex),
                        telemetry: op["telemetryText"].as_str().map(str::to_string),
                    };
                    let got = policies[inst].evaluate(&cfg, &clock, &mut src);
                    assert_eq!(serde_json::to_value(got).unwrap(), op["expected"], "{at}");
                }
                "statusString" => {
                    let got = serde_json::to_value(policies[inst].status_string()).unwrap();
                    assert_eq!(got, op["expected"], "{at}");
                }
                other => panic!("{at}: unknown op {other}"),
            }
        }
    }
    assert_eq!(f.cases.len(), 4);
}

#[test]
fn load_shed_test_ts() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        ops: Vec<Value>,
    }
    let f = common::fixture::<Case>("boot_policy/load-shed.json");
    let mut ticks = 0u64;
    for c in &f.cases {
        let mut policies: Vec<LoadShedPolicy> = Vec::new();
        for (k, op) in c.ops.iter().enumerate() {
            let at = format!("{} [op {k}]", c.name);
            let inst = op["inst"].as_u64().map(|i| i as usize);
            let cfg = || -> ShedConfig { serde_json::from_value(op["cfg"].clone()).unwrap() };
            let reading =
                || -> PowerSampleReading { serde_json::from_value(op["reading"].clone()).unwrap() };
            match op["op"].as_str().unwrap() {
                "new" => {
                    assert_eq!(inst, Some(policies.len()), "{at}");
                    policies.push(LoadShedPolicy::new(&at_mono(op["mono"].as_u64().unwrap())));
                }
                "tick" => {
                    let p = &mut policies[inst.unwrap()];
                    let (cfg, reading) = (cfg(), reading());
                    let first = op["firstMono"].as_u64().unwrap();
                    let n = op["n"].as_u64().unwrap();
                    let mut events = Vec::new();
                    let mut last = None;
                    for i in 0..n {
                        let mono = first + i * 1000;
                        p.push(&reading, &at_mono(mono));
                        let v = p.evaluate(&cfg, &at_mono(mono));
                        if let Some(e) = &v.event {
                            events.push(serde_json::json!({ "i": i, "event": e }));
                        }
                        last = Some(v);
                        ticks += 1;
                    }
                    assert_eq!(Value::Array(events), op["events"], "{at}: events");
                    assert_eq!(
                        serde_json::to_value(last).unwrap(),
                        op["last"],
                        "{at}: last"
                    );
                }
                "push" => {
                    policies[inst.unwrap()]
                        .push(&reading(), &at_mono(op["mono"].as_u64().unwrap()));
                }
                "evaluate" => {
                    let v = policies[inst.unwrap()]
                        .evaluate(&cfg(), &at_mono(op["mono"].as_u64().unwrap()));
                    assert_eq!(serde_json::to_value(v).unwrap(), op["expected"], "{at}");
                }
                "reset" => policies[inst.unwrap()].reset(&at_mono(op["mono"].as_u64().unwrap())),
                "statusString" => {
                    let got =
                        serde_json::to_value(policies[inst.unwrap()].status_string()).unwrap();
                    assert_eq!(got, op["expected"], "{at}");
                }
                "computeShedSignals" => {
                    let samples: Vec<PowerSample> =
                        serde_json::from_value(op["samples"].clone()).unwrap();
                    let got = compute_shed_signals(&samples, &cfg(), op["nowMs"].as_i64().unwrap());
                    assert_eq!(serde_json::to_value(got).unwrap(), op["expected"], "{at}");
                }
                other => panic!("{at}: unknown op {other}"),
            }
        }
    }
    assert_eq!(f.cases.len(), 34);
    assert!(ticks > 5000, "{ticks}");
}
