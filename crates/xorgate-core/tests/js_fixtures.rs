//! The V8 built-ins `xorgate_core::js` reproduces, against values dumped
//! from node: `fixtures/js/js-fmt.json` (WP3's 180 probes: `String(n)`,
//! `toFixed`, `Date.parse`) and `fixtures/js/number-format.json` (Phase 1
//! integration: `String(x)` / `JSON.stringify(x)` for 5 141 doubles given by
//! bit pattern, driving `js::number_to_string`, `telemetry::JsNumber` and a
//! `serde_json` parse → re-emit round trip).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::js::{date_parse, number_to_string, to_fixed};
use xorgate_core::telemetry::JsNumber;

#[test]
fn js_fmt_probes() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        op: String,
        input: Value,
        expected: Value,
    }
    let num = |v: &Value| -> f64 {
        if v.get("$negZero") == Some(&Value::Bool(true)) {
            -0.0
        } else {
            v.as_f64().unwrap()
        }
    };
    let f = common::fixture::<Case>("js/js-fmt.json");
    for c in &f.cases {
        let got = match c.op.as_str() {
            "numberToString" => Value::String(number_to_string(num(&c.input))),
            "toFixed" => Value::String(to_fixed(
                num(&c.input[0]),
                c.input[1].as_u64().unwrap() as u32,
            )),
            "dateParse" => {
                let v = date_parse(c.input.as_str().unwrap());
                if v.is_nan() {
                    Value::Null
                } else {
                    serde_json::json!(v as i64)
                }
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 180);
}

#[derive(Deserialize)]
struct NumCase {
    name: String,
    /// IEEE-754 bit pattern, 16 hex digits.
    input: String,
    /// `String(x)` === `JSON.stringify(x)` in node.
    expected: String,
}

fn from_bits(hex: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(hex, 16).expect("hex bits"))
}

/// Integration item 2 (review-wp6 M2): `js::number_to_string` and
/// `telemetry::JsNumber` (bare and inside an array) print every double
/// exactly as V8 does.
#[test]
fn number_format_matches_v8() {
    let f = common::fixture::<NumCase>("js/number-format.json");
    let mut bad = Vec::new();
    for c in &f.cases {
        let x = from_bits(&c.input);
        let s = number_to_string(x);
        let j = serde_json::to_string(&JsNumber(x)).unwrap();
        let arr = serde_json::to_string(&[JsNumber(x)]).unwrap();
        if s != c.expected || j != c.expected || arr != format!("[{}]", c.expected) {
            bad.push(format!(
                "{} {}: want {} got {s} / {j} / {arr}",
                c.name, c.input, c.expected
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} mismatches: {:#?}",
        bad.len(),
        &bad[..bad.len().min(10)]
    );
    assert_eq!(f.cases.len(), 5141);
}

/// Integration item 3 (review-wp5 NIT 5 / review-wp6 S5): with `serde_json`'s
/// `float_roundtrip` feature every V8-printed double parses back to the exact
/// f64 node had (as `f64` and inside a `Value`), and re-emitting it through
/// `JsNumber` reproduces node's text — `JSON.parse` → `JSON.stringify` parity.
#[test]
fn number_format_parse_round_trip() {
    let f = common::fixture::<NumCase>("js/number-format.json");
    let mut bad = Vec::new();
    for c in &f.cases {
        let x = from_bits(&c.input);
        let direct: f64 = serde_json::from_str(&c.expected).unwrap();
        let in_value: Value = serde_json::from_str(&format!("{{\"v\":[{}]}}", c.expected)).unwrap();
        let via_value = in_value["v"][0].as_f64().unwrap();
        // `-0` prints as `0`, so it parses back as +0 (as in JS).
        let same = |p: f64| p.to_bits() == x.to_bits() || (x == 0.0 && p == 0.0);
        let re_emitted = serde_json::to_string(&JsNumber(via_value)).unwrap();
        if !same(direct) || !same(via_value) || re_emitted != c.expected {
            bad.push(format!(
                "{} {}: {} -> {direct:e} / {via_value:e} -> {re_emitted}",
                c.name, c.input, c.expected
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} mismatches: {:#?}",
        bad.len(),
        &bad[..bad.len().min(10)]
    );
}
