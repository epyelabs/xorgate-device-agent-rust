//! Phase 7a WP0: the argv matrix the capture-host equivalence harness runs
//! (`tools/capture-host/`, K3 in `phase-7a-inprocess-gstreamer.md`). Every
//! entry is produced by the REAL core builders (`build_capture_pipeline_args`
//! / `build_bridge_only_pipeline_args`), so the rig compares `gst-launch-1.0`
//! and `xorgate-capture` on exactly the tokens the agent emits:
//!
//! - bridge-only, without and with an `XORGATE_CAPTURE_SOURCE_HEAD`;
//! - record × every preset × `sw-h264`/`hw-h264`, without and with a head;
//! - `record-finite`: the 7e-style finite head (`num-buffers`) that ends in
//!   a natural EOS (WP0 transcript scenario 7).
//!
//! Paths are fixed (`/work/...`): the harness runs inside a fresh rig
//! container, so the same path serves both programs and needs no rewriting.
//! The camera source `libcamerasrc camera-name=<cam>` is swapped for
//! `videotestsrc is-live=true pattern=ball` by the harness, exactly as the
//! rig's `gst-launch-1.0` shim does (`tools/replay/shims/gst-launch-1.0`).
//!
//! Golden: `XG_BLESS=1 cargo test -p xorgate-core --test
//! capture_host_argv_matrix` rewrites `fixtures/capture-host/argv-matrix.json`;
//! otherwise the test fails when a builder change is not reflected there.

mod common;

use serde_json::{json, Value};
use xorgate_core::hardware_block::EncoderToken;
use xorgate_core::video_capture::*;
use xorgate_core::video_presets::VIDEO_PRESETS;

const CAMERA: &str = "/base/axi/pcie@1000120000/rp1/i2c@88000/imx708@1a";
const OUT_DIR: &str = "/work/out";
const SOCK: &str = "/work/shm/cam0.sock";
const SEGMENT_SECONDS: u64 = 10;
/// A multi-token head, so the splice of a head containing `!` is exercised.
const HEAD: &str = "videotestsrc is-live=true pattern=smpte ! videoconvert";
/// 3 s at 15 fps, then a natural EOS (the replay-head path of 7e).
const FINITE_HEAD: &str = "videotestsrc is-live=true num-buffers=45 pattern=ball";

fn spec(
    preset: &xorgate_core::video_presets::VideoPreset,
    enc: EncoderToken,
) -> CapturePipelineSpec {
    CapturePipelineSpec {
        camera_name: CAMERA.into(),
        head: None,
        preset: preset.clone(),
        segment_seconds: SEGMENT_SECONDS,
        output_dir: OUT_DIR.into(),
        shm_socket_path: SOCK.into(),
        encoder: Some(enc),
    }
}

fn matrix() -> Value {
    let mut cases = Vec::<Value>::new();
    for (head_tag, head) in [("", None), ("/head", Some(HEAD))] {
        cases.push(json!({
            "name": format!("bridge{head_tag}"),
            "mode": "bridge",
            "argv": build_bridge_only_pipeline_args(CAMERA, SOCK, None, head),
        }));
    }
    for (head_tag, head) in [("", None), ("/head", Some(HEAD))] {
        for p in VIDEO_PRESETS {
            for enc in [EncoderToken::SwH264, EncoderToken::HwH264] {
                cases.push(json!({
                    "name": format!("record/{}/{}{head_tag}", p.name, enc.as_str()),
                    "mode": "record",
                    "argv": build_capture_pipeline_args(&spec(p, enc), head),
                }));
            }
        }
    }
    let p = xorgate_core::video_presets::resolve_video_preset("720p15-1mbps");
    cases.push(json!({
        "name": "record-finite/720p15-1mbps/sw-h264",
        "mode": "record",
        "argv": build_capture_pipeline_args(&spec(p, EncoderToken::SwH264), Some(FINITE_HEAD)),
    }));
    json!({
        "generatedBy": "crates/xorgate-core/tests/capture_host_argv_matrix.rs (XG_BLESS=1 to rewrite)",
        "camera": CAMERA,
        "sourceSwap": {
            "from": ["libcamerasrc", format!("camera-name={CAMERA}")],
            "to": ["videotestsrc", "is-live=true", "pattern=ball"],
        },
        "cases": cases,
    })
}

#[test]
fn capture_host_argv_matrix_is_current() {
    let want = serde_json::to_string_pretty(&matrix()).unwrap() + "\n";
    let path = common::fixture_path("capture-host/argv-matrix.json");
    if std::env::var_os("XG_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, &want).unwrap();
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == want,
        "{path} is stale: rerun with XG_BLESS=1 (a builder changed the argv the harness compares)"
    );
}
