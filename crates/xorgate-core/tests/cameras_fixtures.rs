//! Fixture-driven tests for `xorgate_core::cameras`, from
//! `test/hardware-block.test.ts` › "camera enumeration parsing" (2 it()s,
//! split into per-assertion cases) + TS-oracle extras for the line regex.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::cameras::*;
use xorgate_core::hardware_block::Platform;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

#[test]
fn hardware_block_test_ts_camera_enumeration() {
    let f = common::fixture::<Case>("cameras/enumeration.json");
    for c in &f.cases {
        let platform = Platform::parse(c.input["platform"].as_str().unwrap()).unwrap();
        let got: Value = match c.op.as_str() {
            "parseCameraList" => serde_json::to_value(parse_camera_list(
                c.input["output"].as_str().unwrap(),
                platform,
            ))
            .unwrap(),
            "connectorForPath" => {
                connector_for_path(platform, c.input["cameraName"].as_str().unwrap()).into()
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
        if c.op == "parseCameraList" {
            // Serde round trip of the wire shape (key order + null connector).
            let cams: Vec<EnumeratedCamera> = serde_json::from_value(c.expected.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&cams).unwrap(),
                c.expected,
                "{}",
                c.name
            );
        }
    }
    assert_eq!(f.cases.len(), 9);
}
