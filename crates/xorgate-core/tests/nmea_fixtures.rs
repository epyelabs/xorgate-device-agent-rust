//! Fixture-driven tests for `xorgate_core::nmea` — `test/nmea.test.ts` (20 `it`)
//! plus the `parseGsv` cases of `test/sky-state.test.ts:28-48` (2 `it`).
//! Expectations are the exact `JSON.stringify` values the TS produced
//! (floats compared bit-for-bit).

mod common;

use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_core::nmea::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

#[test]
fn nmea_test_ts() {
    let f = common::fixture::<Case>("nmea/nmea.json");
    let mut ops = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *ops.entry(c.op.clone()).or_default() += 1;
        let got: Value = match c.op.as_str() {
            "checksumValid" => Value::Bool(checksum_valid(c.input.as_str().unwrap())),
            "parseSentence" => {
                serde_json::to_value(parse_sentence(c.input.as_str().unwrap())).unwrap()
            }
            "parseGsvInView" => {
                serde_json::to_value(parse_gsv_in_view(c.input.as_str().unwrap())).unwrap()
            }
            "parseGsv" => serde_json::to_value(parse_gsv(c.input.as_str().unwrap())).unwrap(),
            "fixAggregator" => {
                let lines: Vec<String> = serde_json::from_value(c.input.clone()).unwrap();
                let mut agg = FixAggregator::new();
                let updated: Vec<bool> = lines.iter().map(|l| agg.update(l)).collect();
                json!({
                    "updated": updated,
                    "fix": agg.fix,
                    "hasPosition": has_position(&agg.fix),
                    "aggregateFix": aggregate_fix(&lines),
                })
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
        // Everything that serialises must also parse back to the same value
        // (GpsFix / GsvMessage cross the recorder's segment lines later).
        if c.op == "parseSentence" && !c.expected.is_null() {
            let back: GpsFix = serde_json::from_value(c.expected.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                c.expected,
                "{}: round trip",
                c.name
            );
        }
        if c.op == "parseGsv" && !c.expected.is_null() {
            let back: GsvMessage = serde_json::from_value(c.expected.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                c.expected,
                "{}: round trip",
                c.name
            );
        }
    }
    assert_eq!(f.cases.len(), 43);
    assert_eq!(ops["checksumValid"], 5);
    assert_eq!(ops["parseSentence"], 20);
    assert_eq!(ops["parseGsvInView"], 4);
    assert_eq!(ops["parseGsv"], 8);
    assert_eq!(ops["fixAggregator"], 6);
}
