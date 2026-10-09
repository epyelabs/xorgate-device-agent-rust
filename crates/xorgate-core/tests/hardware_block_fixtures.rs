//! Fixture-driven tests for `xorgate_core::hardware_block`, from
//! `test/hardware-block.test.ts` `parseHardwareBlock` (3 cases, the F
//! round-trip expressed as the serialised form) + extras.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::hardware_block::*;

#[test]
fn hardware_block_test_ts_parse() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        op: String,
        input: Value,
        #[serde(rename = "expectedJson")]
        expected_json: Option<String>,
    }
    let f = common::fixture::<Case>("hardware_block/parse.json");
    for c in &f.cases {
        let got = match c.op.as_str() {
            "parseHardwareBlock" => {
                let b = parse_hardware_block(&c.input);
                // `null` is what JSON.stringify prints for a rejected block.
                let s = serde_json::to_string(&b).unwrap();
                if let Some(b) = &b {
                    // Serialised form re-parses to itself (save/load round trip).
                    let v: Value = serde_json::from_str(&s).unwrap();
                    assert_eq!(
                        parse_hardware_block(&v).as_ref(),
                        Some(b),
                        "{}: round trip",
                        c.name
                    );
                    let derived: DeviceHardwareBlock = serde_json::from_str(&s).unwrap();
                    assert_eq!(&derived, b, "{}: derive", c.name);
                }
                Some(s)
            }
            "defaultEncoderForPlatform" => Some(
                serde_json::to_string(&default_encoder_for_platform(
                    Platform::parse(c.input.as_str().unwrap()).unwrap(),
                ))
                .unwrap(),
            ),
            "hardwareBlocksEqual" => {
                let a = parse_hardware_block(&c.input[0]);
                let b = parse_hardware_block(&c.input[1]);
                Some(serde_json::to_string(&hardware_blocks_equal(a.as_ref(), b.as_ref())).unwrap())
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 23);
}
