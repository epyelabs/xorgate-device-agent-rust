//! Fixture-driven tests for `xorgate_core::boot_config`, extracted from the
//! pure cases of `test/boot-config.test.ts` ("boot config parsing" + "rotation
//! in force"). The helper-script and `reconcileCameraMount` cases are I/O
//! orchestration (Phase 4).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::boot_config::*;
use xorgate_core::hardware_block::DeviceHardwareBlock;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

fn to_value<T: serde::Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap()
}

#[test]
fn boot_config_test_ts() {
    let f = common::fixture::<Case>("boot_config/boot-config.json");
    for c in &f.cases {
        let i = &c.input;
        let got = match c.op.as_str() {
            "parseOverlayLines" => to_value(parse_overlay_lines(i["text"].as_str().unwrap())),
            "overlayLinesForCamera" => {
                let lines = parse_overlay_lines(i["text"].as_str().unwrap());
                to_value(overlay_lines_for_camera(
                    &lines,
                    i["camToken"].as_str().unwrap(),
                ))
            }
            "rotationOfLine" => {
                let line: OverlayLine = serde_json::from_value(i["line"].clone()).unwrap();
                to_value(rotation_of_line(&line).map(xorgate_core::telemetry::JsNumber))
            }
            "bootConfigCamera" => to_value(boot_config_camera(
                i["text"].as_str().unwrap(),
                i["camToken"].as_str().unwrap(),
            )),
            "overlayCamToken" => to_value(overlay_cam_token(i["connector"].as_str().unwrap())),
            "resolveCamToken" => {
                let hw: Option<DeviceHardwareBlock> =
                    serde_json::from_value(i["hardware"].clone()).unwrap();
                to_value(resolve_cam_token(
                    i["streamKey"].as_str().unwrap(),
                    hw.as_ref(),
                ))
            }
            "resolveBootConfigPath" => {
                let candidates: Vec<&str> = i["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                let existing: Vec<&str> = i["existing"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                to_value(resolve_boot_config_path(&candidates, |p| {
                    existing.contains(&p)
                }))
            }
            "dtRotationForCamera" => {
                let files = i["files"].as_object().unwrap();
                to_value(dt_rotation_for_camera(
                    i["cameraName"].as_str().unwrap(),
                    |node| {
                        files.get(&format!("{node}/rotation")).map(|bytes| {
                            bytes
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|b| b.as_u64().unwrap() as u8)
                                .collect()
                        })
                    },
                ))
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 39);
}
