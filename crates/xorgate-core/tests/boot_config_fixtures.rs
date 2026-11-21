//! Fixture-driven tests for `xorgate_core::boot_config`, extracted from the
//! pure cases of `test/boot-config.test.ts` ("boot config parsing" + "rotation
//! in force"). The helper-script and `reconcileCameraMount` cases are I/O
//! orchestration (Phase 4).
//!
//! Since Phase 1 of the support-other-camera-models plan the Rust agent no
//! longer has the TS `rotationOfLine` / `overlayLinesForCamera` /
//! `bootConfigCamera`: it reads a camera line through the sensor catalog
//! (`camera_lines_for_port`, `rotation_token`, `line_rotation`). Their 16 TS
//! cases stay here as PINS of how the two readers relate: [`ts_view`]
//! rebuilds the TS answer from the Rust reader, and the cases where the
//! answers differ BY DESIGN are listed one by one ([`RUST_READS_DIFFERENTLY`],
//! accepted divergence D97).

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

/// The camera lines the Rust reader attributes to `cam_token` (board
/// unknown: `[section]` filters do not hide a line, as in the TS).
fn rust_lines(text: &str, cam_token: &str) -> Vec<OverlayLine> {
    let Some(port) = CamPort::from_token(cam_token) else {
        return Vec::new(); // no camera overlay can target another port
    };
    let scoped = parse_scoped_lines(text, Board::Unknown);
    camera_lines_for_port(&scoped, port, xorgate_core::camera_catalog::catalog())
        .into_iter()
        .map(|l| l.line.clone())
        .collect()
}

/// The TS `bootConfigCamera` answer (`{sensor, rotation, lines}`), rebuilt
/// from the Rust reader: the first line's overlay, the rotation every line
/// agrees on (`null` otherwise), the line count.
fn ts_view(text: &str, cam_token: &str) -> Value {
    let lines = rust_lines(text, cam_token);
    let catalog = xorgate_core::camera_catalog::catalog();
    let rotations: Vec<Option<u16>> = lines
        .iter()
        .map(|l| effective_rotation(l, catalog))
        .collect();
    let agreed = match rotations.first() {
        Some(first) if rotations.iter().all(|r| r.is_some() && r == first) => *first,
        _ => None,
    };
    serde_json::json!({
        "sensor": lines.first().map(|l| l.name.clone()),
        "rotation": agreed,
        "lines": lines.len(),
    })
}

/// D97: the TS reads the `rotation=` value through `Number()`; the Rust
/// reader understands the two exact tokens `rotation=0` and `rotation=180`
/// and calls anything else a line that needs a rewrite (`null` here). Case
/// name → (the TS answer, the Rust answer).
const RUST_READS_DIFFERENTLY: &[(&str, u64, Option<u64>)] = &[
    (
        "[extra] rotationOfLine rotation= (empty → Number('') = 0)",
        0,
        None,
    ),
    ("[extra] rotationOfLine rotation=0x0", 0, None),
    ("[extra] rotationOfLine rotation=1.8e2", 180, None),
];

#[test]
fn boot_config_test_ts() {
    let f = common::fixture::<Case>("boot_config/boot-config.json");
    let (mut pinned, mut differs) = (0, 0);
    for c in &f.cases {
        let i = &c.input;
        let got = match c.op.as_str() {
            "parseOverlayLines" => to_value(parse_overlay_lines(i["text"].as_str().unwrap())),
            "overlayLinesForCamera" => to_value(rust_lines(
                i["text"].as_str().unwrap(),
                i["camToken"].as_str().unwrap(),
            )),
            "rotationOfLine" => {
                // The TS answers 0 for "no token"; so does the Rust reader
                // for THIS overlay (imx290, default 0), through the catalog.
                let line: OverlayLine = serde_json::from_value(i["line"].clone()).unwrap();
                assert_eq!(line.name, "imx290", "{}", c.name);
                pinned += 1;
                to_value(effective_rotation(
                    &line,
                    xorgate_core::camera_catalog::catalog(),
                ))
            }
            "bootConfigCamera" => {
                let mut got = ts_view(i["text"].as_str().unwrap(), i["camToken"].as_str().unwrap());
                if let Some((_, ts, rust)) = RUST_READS_DIFFERENTLY
                    .iter()
                    .find(|(name, _, _)| *name == c.name)
                {
                    assert_eq!(c.expected["rotation"], *ts, "{}: the TS answer", c.name);
                    assert_eq!(got["rotation"], to_value(rust), "{}: the Rust answer", c.name);
                    got["rotation"] = c.expected["rotation"].clone();
                    differs += 1;
                }
                pinned += 1;
                got
            }
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
    assert_eq!(pinned, 14, "the bootConfigCamera + rotationOfLine cases");
    assert_eq!(differs, RUST_READS_DIFFERENTLY.len());
}

/// B3, beside the TS case "treats a missing rotation token as the overlay's
/// own default of 0": that sentence is true of `imx290` only. The same
/// token-less line of a default-180 overlay reads 180, and "no token" is a
/// case of its own.
#[test]
fn a_missing_rotation_token_is_the_overlays_default_which_is_not_always_0() {
    let catalog = xorgate_core::camera_catalog::catalog();
    for (text, want) in [
        ("dtoverlay=imx290,cam1", 0),
        ("dtoverlay=imx219,cam1", 180),
        ("dtoverlay=imx708,cam0", 180),
        ("dtoverlay=imx335,cam0", 180),
    ] {
        let line = parse_overlay_lines(text).remove(0);
        assert_eq!(rotation_token(&line), RotationToken::Absent, "{text}");
        assert_eq!(effective_rotation(&line, catalog), Some(want), "{text}");
    }
}
