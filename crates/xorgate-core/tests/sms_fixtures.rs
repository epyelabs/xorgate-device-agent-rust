//! Fixture-driven tests for `xorgate_core::sms` (the pure half of
//! `src/hardware/sms-modem.ts`), from the P subset of `test/sms-modem.test.ts`:
//! `parseCpmsQuery` 1, `clampSmsLimit` 1 (8 values), `buildSmsPayload` 4 and
//! the fixture inbox — 34 cases with extras. The `…/sms` payload expectations
//! are the exact `JSON.stringify` bytes, so this is also the serde proof.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::sms::*;
use xorgate_core::sms_pdu::SmsMessage;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    #[serde(rename = "expectedJson")]
    expected_json: String,
}

#[derive(Deserialize)]
struct Req {
    #[serde(rename = "deviceId")]
    device_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    limit: u32,
}

impl Req {
    fn into_request(self) -> SmsPayloadRequest {
        SmsPayloadRequest {
            device_id: self.device_id,
            request_id: self.request_id,
            limit: self.limit,
        }
    }
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

#[test]
fn sms_modem_test_ts_pure_subset() {
    let f = common::fixture::<Case>("sms/sms-modem.json");
    assert_eq!(f.extra["SMS_PAYLOAD_MAX_BYTES"], SMS_PAYLOAD_MAX_BYTES);
    assert_eq!(f.extra["SMS_LIMIT_DEFAULT"], SMS_LIMIT_DEFAULT);
    assert_eq!(f.extra["SMS_LIMIT_MAX"], SMS_LIMIT_MAX);
    assert_eq!(f.extra["SMS_TEXT_MAX"], xorgate_core::sms_pdu::SMS_TEXT_MAX);
    let mut ops = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *ops.entry(c.op.clone()).or_default() += 1;
        let got = match c.op.as_str() {
            "parseCpmsQuery" => json(&parse_cpms_query(c.input.as_str().unwrap())),
            "clampSmsLimit" => {
                let v = &c.input;
                let n = if common::is_undefined_marker(v) {
                    clamp_sms_limit(None)
                } else if v.get("$nan") == Some(&Value::Bool(true)) {
                    clamp_sms_limit_num(f64::NAN)
                } else if let Some(sign) = v.get("$inf").and_then(Value::as_i64) {
                    clamp_sms_limit_num(if sign < 0 {
                        f64::NEG_INFINITY
                    } else {
                        f64::INFINITY
                    })
                } else {
                    clamp_sms_limit(Some(v))
                };
                json(&n)
            }
            "fixtureSmsSweep" => {
                let s = fixture_sms_sweep(c.input.as_i64().unwrap());
                let again: SmsSweep = serde_json::from_str(&json(&s)).unwrap();
                assert_eq!(again, s, "{}: round trip", c.name);
                json(&s)
            }
            "buildSmsPayload" => {
                let sweep: SmsSweep = serde_json::from_value(c.input["sweep"].clone()).unwrap();
                let req: Req = serde_json::from_value(c.input["req"].clone()).unwrap();
                let p = build_sms_payload(&sweep, &req.into_request());
                let again: SmsResultPayload = serde_json::from_str(&json(&p)).unwrap();
                assert_eq!(again, p, "{}: round trip", c.name);
                json(&p)
            }
            "buildSmsPayloadCap" => {
                let mut sweep: SmsSweep = serde_json::from_value(c.input["sweep"].clone()).unwrap();
                let mut template: SmsMessage =
                    serde_json::from_value(c.input["messageTemplate"].clone()).unwrap();
                template.text = template
                    .text
                    .repeat(c.input["textRepeat"].as_u64().unwrap() as usize);
                sweep.messages =
                    vec![template; c.input["messageRepeat"].as_u64().unwrap() as usize];
                let req: Req = serde_json::from_value(c.input["req"].clone()).unwrap();
                let p = build_sms_payload(&sweep, &req.into_request());
                let bytes = json(&p).len();
                assert!(bytes <= SMS_PAYLOAD_MAX_BYTES, "{}: {bytes}", c.name);
                let mut uncapped = p.clone();
                uncapped.partial = false;
                uncapped.messages = sweep.messages.clone();
                json(&serde_json::json!({
                    "bytes": bytes,
                    "messagesLength": p.messages.len(),
                    "partial": p.partial,
                    "total": p.total,
                    "uncappedBytes": json(&uncapped).len(),
                }))
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 34);
    assert_eq!(ops["parseCpmsQuery"], 7);
    assert_eq!(ops["clampSmsLimit"], 17);
    assert_eq!(ops["buildSmsPayload"], 6);
    assert_eq!(ops["buildSmsPayloadCap"], 1);
    assert_eq!(ops["fixtureSmsSweep"], 3);
}
