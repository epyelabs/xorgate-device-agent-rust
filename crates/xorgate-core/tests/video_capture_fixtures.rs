//! Fixture-driven tests for `xorgate_core::video_capture`, from
//! `test/recording-video-capture.test.ts` › "pipeline builders" (3) and
//! "resolveBridgedCameras" (2, F: the tmp pipeline dir as a name → content
//! map), the builder-level half of › replay hooks › "sourceHead replaces…",
//! `test/hardware-block.test.ts` › "buildCapturePipelineArgs swaps only the
//! encoder stage on hw-h264", `test/pipeline.test.ts` › "bridged mode
//! helpers" (3) + TS-oracle extras (every preset × encoder, the env head).

mod common;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::hardware_block::EncoderToken;
use xorgate_core::video_capture::*;
use xorgate_core::video_presets::find_video_preset;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
    /// `resolveBridgedCameras` cases: the `console.warn` lines the TS logged.
    #[serde(default)]
    warnings: Option<Vec<String>>,
}

fn opt_str<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn head(v: &Value) -> Option<Vec<String>> {
    v.get("head")
        .map(|h| serde_json::from_value(h.clone()).unwrap())
}

#[test]
fn video_capture_builders_and_pipeline_helpers() {
    let f = common::fixture::<Case>("video_capture/builders.json");
    for c in &f.cases {
        let i = &c.input;
        let got: Value = match c.op.as_str() {
            "buildCapturePipelineArgs" => {
                let spec = CapturePipelineSpec {
                    camera_name: i["cameraName"].as_str().unwrap().into(),
                    head: head(i),
                    preset: find_video_preset(i["preset"].as_str().unwrap())
                        .unwrap()
                        .clone(),
                    segment_seconds: i["segmentSeconds"].as_u64().unwrap(),
                    output_dir: i["outputDir"].as_str().unwrap().into(),
                    shm_socket_path: i["shmSocketPath"].as_str().unwrap().into(),
                    encoder: opt_str(i, "encoder").map(|e| EncoderToken::parse(e).unwrap()),
                };
                serde_json::to_value(build_capture_pipeline_args(&spec, None)).unwrap()
            }
            "buildBridgeOnlyPipelineArgs" => {
                let h = head(i);
                serde_json::to_value(build_bridge_only_pipeline_args(
                    i["cameraName"].as_str().unwrap(),
                    i["shmSocketPath"].as_str().unwrap(),
                    h.as_deref(),
                    opt_str(i, "envOverride"),
                ))
                .unwrap()
            }
            "bridgedKvsPipeline" => {
                bridged_kvs_pipeline(i["shmSocketPath"].as_str().unwrap(), None).into()
            }
            "resolveBridgedCameras" => {
                let dir = i["pipelineDir"].as_str().unwrap();
                let files: BTreeMap<String, String> =
                    serde_json::from_value(i["files"].clone()).unwrap();
                let channels: Vec<KvsChannel> =
                    serde_json::from_value(i["channels"].clone()).unwrap();
                let read = |p: &str| {
                    p.strip_prefix(dir)
                        .and_then(|n| n.strip_prefix('/'))
                        .and_then(|n| files.get(n).cloned())
                };
                let (cams, warnings) = resolve_bridged_cameras(dir, &channels, &read);
                // The TS's console.warn lines, verbatim and in order.
                assert_eq!(Some(&warnings), c.warnings.as_ref(), "{}", c.name);
                serde_json::to_value(cams).unwrap()
            }
            "extractCameraName" => extract_camera_name(i["gst"].as_str().unwrap()).into(),
            "isBridgedPipeline" => is_bridged_pipeline(opt_str(i, "gst")).into(),
            "resolveCameraName" => resolve_camera_name(
                opt_str(i, "cameraFile"),
                opt_str(i, "directBackup"),
                opt_str(i, "gst"),
            )
            .into(),
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 34);
}

#[test]
fn skipped_bridged_stream_warning_is_verbatim() {
    // `video-capture.ts:105-108`; the dumper's stderr printed exactly this.
    assert_eq!(
        bridged_skip_warning("cam2"),
        "[video-capture] cam2: bridged .gst but no camera identity (missing cam2.camera) — skipping"
    );
    let channels = vec![KvsChannel {
        stream_key: "cam2".into(),
        channel_name: "d-cam2".into(),
    }];
    let read =
        |p: &str| (p == "/p/cam2.gst").then(|| "shmsrc socket-path=/y ! appsink".to_string());
    let (cams, warnings) = resolve_bridged_cameras("/p", &channels, &read);
    assert!(cams.is_empty());
    assert_eq!(warnings, vec![bridged_skip_warning("cam2")]);
}
