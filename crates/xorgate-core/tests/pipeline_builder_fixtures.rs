//! Fixture-driven tests for `xorgate_core::pipeline_builder`, from
//! `test/hardware-block.test.ts` › "pipeline templates" (the template it()s;
//! the `buildCapturePipelineArgs` one lives in `video_capture_fixtures.rs`)
//! and › reconcileHardware › "recognizes both encoder renders as
//! overwritable" (pure). Every expected string is what the TS returned.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::hardware_block::EncoderToken;
use xorgate_core::pipeline_builder::*;
use xorgate_core::video_capture::bridged_kvs_pipeline;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_else(|| panic!("missing {k}"))
}

fn enc(v: &Value) -> EncoderToken {
    EncoderToken::parse(s(v, "encoder")).unwrap()
}

#[test]
fn hardware_block_test_ts_pipeline_templates() {
    let f = common::fixture::<Case>("pipeline_builder/templates.json");
    assert_eq!(f.extra["SHM_BRIDGE_CAPS"], SHM_BRIDGE_CAPS);
    for c in &f.cases {
        let i = &c.input;
        let got: Value = match c.op.as_str() {
            "buildBridgedKvsPipeline" => {
                build_bridged_kvs_pipeline(enc(i), s(i, "shmSocketPath")).into()
            }
            "bridgedKvsPipeline" => bridged_kvs_pipeline(s(i, "shmSocketPath"), None).into(),
            "buildDirectKvsPipeline" => {
                build_direct_kvs_pipeline(enc(i), s(i, "streamKey"), s(i, "cameraName")).into()
            }
            "buildRecordEncodeArgs" => serde_json::to_value(build_record_encode_args(
                enc(i),
                i["bitrateKbps"].as_u64().unwrap() as u32,
                i["keyIntMax"].as_u64().unwrap() as u32,
            ))
            .unwrap(),
            "isRecognizedBridgedRender" => {
                is_recognized_bridged_render(s(i, "content"), s(i, "shmSocketPath")).into()
            }
            "isRecognizedDirectRender" => {
                is_recognized_direct_render(s(i, "content"), s(i, "streamKey"), s(i, "cameraName"))
                    .into()
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
    // The TS assertions the fixtures alone do not restate.
    let hw = build_bridged_kvs_pipeline(EncoderToken::HwH264, "/s");
    assert!(!hw.contains("sampleVideoEncoder"));
    let hw_rec = build_record_encode_args(EncoderToken::HwH264, 1000, 30);
    assert_eq!(hw_rec[hw_rec.len() - 2], "!");
    assert!(hw_rec[hw_rec.len() - 1].contains("profile=(string)constrained-baseline"));
    assert_eq!(f.cases.len(), 24);
}
