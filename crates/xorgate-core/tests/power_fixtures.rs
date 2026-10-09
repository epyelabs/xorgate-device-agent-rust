//! Fixture-driven tests for `xorgate_core::power` — the whole of
//! `test/power.test.ts` (17 `it`: parseThrottled 4, parsePmicAdc 2, rail5vFrom
//! 2, readMaxCurrentMa 3 → 2 as bytes-in, powerReadingFromFields 6) replayed
//! through the `power` module (the vcgencmd parsers are Phase 0's `system`
//! re-exports; `fixtures/system/*` already pins 11 of these, this file is the
//! one-stop replay of the TS suite).

mod common;

use serde::Deserialize;
use serde_json::{json, Map, Value};
use xorgate_core::power::*;
use xorgate_core::telemetry::{JsNumber, MetricGroup};
use xorgate_core::types::PowerSampleReading;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
    #[serde(default, rename = "keyCount")]
    key_count: Option<usize>,
}

fn adc_to_value(adc: &PmicAdc) -> Value {
    let mut m = Map::new();
    for (name, rail) in adc {
        let mut r = Map::new();
        if let Some(a) = rail.amps {
            r.insert("amps".into(), serde_json::to_value(JsNumber(a)).unwrap());
        }
        if let Some(v) = rail.volts {
            r.insert("volts".into(), serde_json::to_value(JsNumber(v)).unwrap());
        }
        m.insert(name.clone(), Value::Object(r));
    }
    Value::Object(m)
}

fn adc_from_value(v: &Value) -> PmicAdc {
    let mut adc = PmicAdc::new();
    for (name, rail) in v.as_object().unwrap() {
        adc.insert(
            name.clone(),
            PmicRail {
                amps: rail.get("amps").and_then(Value::as_f64),
                volts: rail.get("volts").and_then(Value::as_f64),
            },
        );
    }
    adc
}

#[test]
fn power_test_ts() {
    let f = common::fixture::<Case>("power/power.json");
    let mut n = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *n.entry(c.op.clone()).or_default() += 1;
        match c.op.as_str() {
            "parseThrottled" => {
                let got = parse_throttled(c.input.as_str().unwrap())
                    .map(|t| json!({ "current": t.current, "sticky": t.sticky }))
                    .unwrap_or(Value::Null);
                assert_eq!(got, c.expected, "{}", c.name);
            }
            "parsePmicAdc" => {
                let got = parse_pmic_adc(c.input.as_str().unwrap());
                assert_eq!(
                    got.as_ref().map(adc_to_value).unwrap_or(Value::Null),
                    c.expected,
                    "{}",
                    c.name
                );
                assert_eq!(got.map(|a| a.len()), c.key_count, "{}: key count", c.name);
            }
            "rail5vFrom" => {
                let got = rail_5v_from(&adc_from_value(&c.input));
                assert_eq!(
                    serde_json::to_value(got.map(JsNumber)).unwrap(),
                    c.expected,
                    "{}",
                    c.name
                );
            }
            "maxCurrentFromBytes" => {
                let bytes: Vec<u8> = serde_json::from_value(c.input.clone()).unwrap();
                assert_eq!(
                    serde_json::to_value(max_current_ma_from_bytes(&bytes)).unwrap(),
                    c.expected,
                    "{}",
                    c.name
                );
            }
            "powerReadingFromFields" => {
                let fields: Option<MetricGroup> = if common::is_undefined_marker(&c.input) {
                    None
                } else {
                    Some(serde_json::from_value(c.input.clone()).unwrap())
                };
                let got = power_reading_from_fields(fields.as_ref());
                // `tempC: null` in the TS output reads as `None` here (WP1
                // ambiguity 4: the Option is omitted on re-serialisation), so
                // compare as parsed structs rather than as JSON text.
                let want: Option<PowerSampleReading> =
                    serde_json::from_value(c.expected.clone()).unwrap();
                assert_eq!(got, want, "{}", c.name);
                if let Some(r) = got {
                    assert!(r.soft_temp.is_some(), "{}: softTemp is always set", c.name);
                }
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert_eq!(f.cases.len(), 28);
    assert_eq!(n["parseThrottled"], 7);
    assert_eq!(n["parsePmicAdc"], 3);
    assert_eq!(n["rail5vFrom"], 3);
    assert_eq!(n["maxCurrentFromBytes"], 3);
    assert_eq!(n["powerReadingFromFields"], 12);

    let bits: Vec<Value> = THROTTLE_BITS
        .iter()
        .map(|b| json!({ "bit": b.bit, "label": b.label }))
        .collect();
    assert_eq!(Value::Array(bits), f.extra["throttleBits"], "THROTTLE_BITS");
}
