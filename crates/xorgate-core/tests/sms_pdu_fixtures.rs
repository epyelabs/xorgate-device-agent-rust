//! Fixture-driven tests for `xorgate_core::sms_pdu`, from `test/sms-pdu.test.ts`
//! (27 `it()` → 99 cases incl. extras). The PDU hex was BUILT at test time by
//! `test/sms-pdu-builder.ts` and frozen into the fixture by the extractor.
//! Every expectation is the exact `JSON.stringify` the TS produced.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::sms_pdu::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    #[serde(rename = "expectedJson")]
    expected_json: String,
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

#[test]
fn sms_pdu_test_ts() {
    let f = common::fixture::<Case>("sms_pdu/sms-pdu.json");
    let mut ops = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *ops.entry(c.op.clone()).or_default() += 1;
        let got = match c.op.as_str() {
            "decodePdu" => {
                let hex = c.input["hex"].as_str().unwrap();
                let length = c
                    .input
                    .get("length")
                    .and_then(Value::as_u64)
                    .map(|l| l as usize);
                let r = decode_pdu(hex, length);
                // Decoding is total: the "decoder threw" fallback is unreachable.
                if let Some(d) = r.as_deliver() {
                    assert_eq!(d.text_lossy(), String::from_utf16_lossy(&d.text));
                }
                json(&r)
            }
            "dcsAlphabet" => json(&dcs_alphabet(c.input.as_u64().unwrap() as u8)),
            "dcsAlphabetTable" => {
                let mut m = serde_json::Map::new();
                for d in 0..=255u8 {
                    m.insert(
                        d.to_string(),
                        serde_json::to_value(dcs_alphabet(d)).unwrap(),
                    );
                }
                json(&m)
            }
            "decodeScts" => {
                let bytes: Vec<u8> = serde_json::from_value(c.input.clone()).unwrap();
                json(&decode_scts(&bytes))
            }
            "sentAtMs" => json(&parse_sent_at_ms(c.input.as_str().unwrap())),
            "assembleMessages" => {
                let records: Vec<StoredPdu> = serde_json::from_value(c.input.clone()).unwrap();
                let r = assemble_messages(&records);
                // The wire struct round-trips through its own serde.
                let again: AssembledMessages = serde_json::from_str(&json(&r)).unwrap();
                assert_eq!(again, r, "{}: round trip", c.name);
                json(&r)
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 99);
    assert_eq!(ops["decodePdu"], 54);
    assert_eq!(ops["assembleMessages"], 16);
    assert_eq!(ops["decodeScts"], 13);
    assert_eq!(ops["dcsAlphabet"], 11);
}

#[test]
fn tables_match_the_ts_literals() {
    assert_eq!(GSM7_BASIC_TABLE.len(), 128);
    assert_eq!(GSM7_BASIC_TABLE[0], '@');
    assert_eq!(GSM7_BASIC_TABLE[0x1b], '\u{1b}');
    assert_eq!(GSM7_BASIC_TABLE[0x7f], 'à');
    assert_eq!(GSM7_EXT_TABLE.len(), 10);
    assert_eq!(SMS_TEXT_MAX, 4000);
    assert_eq!(
        unpack_septets(&[0xE8, 0x32, 0x9B, 0xFD, 0x46, 0x97, 0xD9, 0xEC, 0x37], 10),
        b"hellohello".to_vec()
    );
    assert_eq!(gsm7_to_string(b"hellohello"), "hellohello");
}
