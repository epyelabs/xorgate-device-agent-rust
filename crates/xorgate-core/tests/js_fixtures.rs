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
