//! Fixture-driven tests for `xorgate_core::upload_keys`, from the real
//! `s3KeyFor` of `src/recording/telemetry-source.ts:45-51` and
//! `src/recording/video-source.ts:51-56,174-178` (+ the sidecar key of `:162`)
//! over 54 valid / stray names (upper-case uuid, 4-digit seq, `\n` tails,
//! non-ASCII digits, extra path parts), dumped with tsx
//! (`phase-3/evidence/wp3-tools/dump-upload-keys.mts`).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::upload_keys::{mp4_to_json, telemetry_s3_key, video_s3_key};

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Input,
    expected: Value,
}

#[derive(Deserialize)]
struct Input {
    #[serde(rename = "deviceId")]
    device_id: String,
    name: String,
}

#[test]
fn upload_keys_fixture() {
    let f = common::fixture::<Case>("upload_keys/upload-keys.json");
    let (mut tel, mut vid) = (0, 0);
    for c in &f.cases {
        match c.op.as_str() {
            "telemetry" => {
                tel += 1;
                let got = telemetry_s3_key(&c.input.device_id, &c.input.name);
                assert_eq!(got.as_deref(), c.expected.as_str(), "{}", c.name);
            }
            "video" => {
                vid += 1;
                let got = video_s3_key(&c.input.device_id, &c.input.name);
                assert_eq!(got.as_deref(), c.expected["key"].as_str(), "{}", c.name);
                let side = got.as_deref().map(mp4_to_json);
                assert_eq!(side.as_deref(), c.expected["sidecarKey"].as_str(), "{}", c.name);
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert!(tel >= 20 && vid >= 20, "tel={tel} vid={vid}");
}
