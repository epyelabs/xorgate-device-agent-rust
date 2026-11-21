//! The single GStreamer pipeline-builder. Port of `src/media/pipeline-builder.ts`
//! (186 lines, fully pure; cm4-support plan Phase 2): every rendered artifact —
//! the bridged KVS `.gst`, the direct `.gst.direct` backup and the recording
//! pipeline's encode stage — comes from ONE template per encoder token.
//!
//! Every string here is BYTE-IDENTICAL to the TS (acceptance B6): the
//! fixtures in `fixtures/pipeline_builder/` carry the exact strings the TS
//! produced, dumped with `npx tsx`, never hand-typed. Do not "tidy" them.
//!
//! Two load-bearing details copied from the TS doc comment: a convert element
//! is MANDATORY before `v4l2h264enc` (without it the pipeline goes PLAYING and
//! produces zero bytes forever) and `profile=(string)constrained-baseline`
//! caps are MANDATORY for WebRTC (browser renders black otherwise). The HW
//! encoder is deliberately NOT named `sampleVideoEncoder` (TWCC skip, D5).

use crate::hardware_block::EncoderToken;
use crate::js::trim as js_trim;

/// `SHM_BRIDGE_CAPS` (`pipeline-builder.ts:27`): fixed low-res bridge caps —
/// MUST match the capture daemon's shmsink caps (agent-anatomy invariant 8:
/// `I420 1280x720 15/1`, shmsrc does no caps negotiation).
pub const SHM_BRIDGE_CAPS: &str = "video/x-raw,format=I420,width=1280,height=720,framerate=15/1";

/// `SW_H264_OUT_CAPS` (`:30`).
const SW_H264_OUT_CAPS: &str =
    "video/x-h264,stream-format=byte-stream,alignment=au,profile=baseline";
/// `HW_H264_OUT_CAPS` (`:31-33`).
const HW_H264_OUT_CAPS: &str = "video/x-h264,profile=(string)constrained-baseline,level=(string)4,stream-format=(string)byte-stream,alignment=(string)au";
/// `SW_ENCODE` (`:36-38`): the x264enc stage of the BRIDGED CM5 pipeline (TWCC-named).
const SW_ENCODE: &str = "x264enc name=sampleVideoEncoder bframes=0 speed-preset=ultrafast bitrate=1000 byte-stream=TRUE tune=zerolatency threads=2";
/// `SW_ENCODE_DIRECT` (`:45-47`): the install script's historical direct
/// variant — identical but for the missing `name=sampleVideoEncoder` (kept
/// byte-for-byte rather than "fixed", D4).
const SW_ENCODE_DIRECT: &str =
    "x264enc bframes=0 speed-preset=ultrafast bitrate=1000 byte-stream=TRUE tune=zerolatency threads=2";
/// `HW_RECORD_OUT_CAPS` (`:59`): the caps `v4l2h264enc` needs to negotiate at all.
const HW_RECORD_OUT_CAPS: &str =
    "video/x-h264,profile=(string)constrained-baseline,level=(string)4";
/// `HW_ENCODE` (`:62-63`): Appendix A's v4l2h264enc stage.
const HW_ENCODE: &str = "v4l2h264enc extra-controls=controls,video_bitrate=1000000,repeat_sequence_header=1,h264_i_frame_period=30";
/// `APPSINK` (`:65`).
const APPSINK: &str = "appsink sync=TRUE emit-signals=TRUE name=appsink-video";

/// `buildBridgedKvsPipeline` (`pipeline-builder.ts:72-81`): the shmsrc
/// consumer the KVS master loads while the capture daemon owns the camera.
pub fn build_bridged_kvs_pipeline(encoder: EncoderToken, shm_socket_path: &str) -> String {
    let src = format!("shmsrc socket-path={shm_socket_path} is-live=true do-timestamp=true");
    match encoder {
        EncoderToken::HwH264 => format!(
            "{src} ! {SHM_BRIDGE_CAPS} ! videoconvert ! {HW_ENCODE} ! {HW_H264_OUT_CAPS} ! {APPSINK}"
        ),
        EncoderToken::SwH264 => {
            format!("{src} ! {SHM_BRIDGE_CAPS} ! {SW_ENCODE} ! {SW_H264_OUT_CAPS} ! {APPSINK}")
        }
    }
}

/// `buildDirectKvsPipeline` (`pipeline-builder.ts:88-98`): the KVS master
/// owns the camera itself (the `.gst.direct` backup).
pub fn build_direct_kvs_pipeline(
    encoder: EncoderToken,
    stream_key: &str,
    camera_name: &str,
) -> String {
    let src = format!("libcamerasrc name={stream_key} camera-name={camera_name}");
    match encoder {
        EncoderToken::HwH264 => format!(
            "{src} ! {SHM_BRIDGE_CAPS} ! videoconvert ! {HW_ENCODE} ! {HW_H264_OUT_CAPS} ! {APPSINK}"
        ),
        EncoderToken::SwH264 => format!(
            "{src} ! {SHM_BRIDGE_CAPS} ! {SW_ENCODE_DIRECT} ! {SW_H264_OUT_CAPS} ! {APPSINK}"
        ),
    }
}

/// `buildRecordEncodeArgs` (`pipeline-builder.ts:116-147`): the recording
/// pipeline's encode stage as gst-launch argv tokens; slots between the tee'd
/// `queue` and `h264parse` in [`crate::video_capture::build_capture_pipeline_args`].
///
/// `bitrateKbps * 1000` is JS number arithmetic on what the preset table
/// holds (integers), so `u32` is exact here; `keyIntMax` is `preset.fps * 2`.
pub fn build_record_encode_args(
    encoder: EncoderToken,
    bitrate_kbps: u32,
    key_int_max: u32,
) -> Vec<String> {
    match encoder {
        EncoderToken::HwH264 => vec![
            "videoconvert".into(),
            "!".into(),
            "v4l2h264enc".into(),
            format!(
                "extra-controls=controls,video_bitrate={},repeat_sequence_header=1,h264_i_frame_period={key_int_max}",
                u64::from(bitrate_kbps) * 1000
            ),
            "!".into(),
            HW_RECORD_OUT_CAPS.into(),
        ],
        EncoderToken::SwH264 => vec![
            "x264enc".into(),
            // ultrafast, not veryfast (65% vs 98% of a core per camera, spike).
            "speed-preset=ultrafast".into(),
            "tune=zerolatency".into(),
            // Pinned like the KVS side (threads=2) — see the TS comment.
            "threads=2".into(),
            format!("bitrate={bitrate_kbps}"),
            format!("key-int-max={key_int_max}"),
        ],
    }
}

/// `isRecognizedBridgedRender` (`pipeline-builder.ts:155-161`): does this
/// `.gst` content look like something this builder rendered? The reconciler
/// only overwrites files it recognises, never an operator's hand-tuned pipeline.
///
/// `content.trim()` is JS `trim` ([`crate::js::trim`]: it also strips
/// a BOM and leaves U+0085, unlike Rust `str::trim`).
pub fn is_recognized_bridged_render(content: &str, shm_socket_path: &str) -> bool {
    let t = js_trim(content);
    t == build_bridged_kvs_pipeline(EncoderToken::SwH264, shm_socket_path)
        || t == build_bridged_kvs_pipeline(EncoderToken::HwH264, shm_socket_path)
}

/// `isRecognizedDirectRender` (`pipeline-builder.ts:163-186`): either of the
/// two direct renders, or — so legacy installs adopt cleanly — any
/// single-line `libcamerasrc`→`x264enc`→`appsink` render for THIS camera.
pub fn is_recognized_direct_render(content: &str, stream_key: &str, camera_name: &str) -> bool {
    let t = js_trim(content);
    if t == build_direct_kvs_pipeline(EncoderToken::SwH264, stream_key, camera_name)
        || t == build_direct_kvs_pipeline(EncoderToken::HwH264, stream_key, camera_name)
    {
        return true;
    }
    !t.contains('\n')
        && t.starts_with("libcamerasrc")
        && t.contains(&format!("camera-name={camera_name}"))
        && t.contains("x264enc")
        && t.contains("appsink")
}

/// B6 (support-other-camera-models Phase 0, RUST-ONLY): is this
/// `.gst.direct` content one of OUR renders, for whatever camera it names?
/// Returns the camera name the file itself carries.
///
/// [`is_recognized_direct_render`] (the JS rule) ties recognition to the
/// camera enumerated NOW, so after a sensor change the previous sensor's
/// render is taken for an operator's pipeline and left behind with its dead
/// `camera-name=`. Here the name is read out of the file, and the content
/// must then be, for THAT name:
///
/// - exactly the software or the hardware direct render of this stream, or
/// - a legacy install's render: one line, `libcamerasrc` first, an H.264
///   encoder element (`x264enc`, or `v4l2h264enc` as a CM4 file has it: the
///   JS clause asks for `x264enc` only) and `appsink`.
///
/// Anything else (no `camera-name=`, several lines, another source or sink)
/// is an operator's pipeline: `None`, never overwritten.
pub fn own_direct_render_camera(content: &str, stream_key: &str) -> Option<String> {
    let t = js_trim(content);
    let name = crate::video_capture::extract_camera_name(t).filter(|n| !n.is_empty())?;
    if t == build_direct_kvs_pipeline(EncoderToken::SwH264, stream_key, &name)
        || t == build_direct_kvs_pipeline(EncoderToken::HwH264, stream_key, &name)
    {
        return Some(name);
    }
    let legacy = !t.contains('\n')
        && t.starts_with("libcamerasrc")
        && (t.contains("x264enc") || t.contains("v4l2h264enc"))
        && t.contains("appsink");
    legacy.then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B6: our own render is recognised by the camera name it carries, in
    /// every template variant; an operator's pipeline never is.
    #[test]
    fn b6_own_direct_render_is_recognised_by_the_name_it_carries() {
        let old = "/base/soc/i2c0mux/i2c@0/imx290@1a";
        let new = "/base/soc/i2c0mux/i2c@0/imx708@1a";
        for enc in [EncoderToken::SwH264, EncoderToken::HwH264] {
            let file = format!("{}\n", build_direct_kvs_pipeline(enc, "cam0", old));
            // The JS rule, asked about the NEW camera, does not know it …
            assert!(!is_recognized_direct_render(&file, "cam0", new));
            // … this one does, and says whose it is.
            assert_eq!(
                own_direct_render_camera(&file, "cam0").as_deref(),
                Some(old)
            );
            // Another stream's exact render is not this stream's.
            assert_eq!(
                own_direct_render_camera(&file, "cam1"),
                // (the sw/hw templates carry `name=<key>`; the legacy clause
                // still adopts a single-line libcamerasrc → encoder → appsink)
                Some(old.to_string())
            );
        }
        // Legacy single-line renders: x264enc (CM5) and v4l2h264enc (CM4).
        let legacy_sw = format!("libcamerasrc camera-name={old} ! video/x-raw ! x264enc ! appsink");
        let legacy_hw = format!(
            "libcamerasrc camera-name={old} ! video/x-raw ! videoconvert ! v4l2h264enc ! appsink"
        );
        assert_eq!(
            own_direct_render_camera(&legacy_sw, "cam0").as_deref(),
            Some(old)
        );
        assert_eq!(
            own_direct_render_camera(&legacy_hw, "cam0").as_deref(),
            Some(old)
        );
        // The JS legacy clause never took the v4l2h264enc file.
        assert!(!is_recognized_direct_render(&legacy_hw, "cam0", old));
        // A quoted name is read without its quotes.
        assert_eq!(
            own_direct_render_camera(
                "libcamerasrc camera-name=\"/c\" ! x264enc ! appsink",
                "cam0"
            )
            .as_deref(),
            Some("/c")
        );
        // Operator pipelines: never ours.
        for custom in [
            // the golden fixture's custom file: no camera name
            "libcamerasrc ! my direct ! appsink\n",
            // several lines
            "libcamerasrc camera-name=/c\n! x264enc ! appsink",
            // another source
            "videotestsrc camera-name=/c ! x264enc ! appsink",
            // no H.264 encoder / no appsink
            "libcamerasrc camera-name=/c ! jpegenc ! appsink",
            "libcamerasrc camera-name=/c ! x264enc ! filesink location=/tmp/x",
            // an empty name
            "libcamerasrc camera-name=\"\" ! x264enc ! appsink",
            "",
        ] {
            assert_eq!(own_direct_render_camera(custom, "cam0"), None, "{custom}");
        }
    }

    #[test]
    fn shm_caps_are_the_fixed_contract() {
        // Invariant 8: `I420 1280x720 15/1`, stated identically on both sides.
        assert_eq!(
            SHM_BRIDGE_CAPS,
            "video/x-raw,format=I420,width=1280,height=720,framerate=15/1"
        );
    }

    #[test]
    fn hw_templates_never_name_the_twcc_encoder() {
        assert!(
            !build_bridged_kvs_pipeline(EncoderToken::HwH264, "/s").contains("sampleVideoEncoder")
        );
        assert!(
            !build_direct_kvs_pipeline(EncoderToken::HwH264, "cam0", "/cam/path")
                .contains("sampleVideoEncoder")
        );
        assert!(
            build_bridged_kvs_pipeline(EncoderToken::SwH264, "/s").contains("sampleVideoEncoder")
        );
    }

    #[test]
    fn legacy_direct_render_is_recognised_only_for_this_camera() {
        let legacy = "libcamerasrc camera-name=/c ! video/x-raw ! x264enc ! appsink";
        assert!(is_recognized_direct_render(legacy, "cam0", "/c"));
        assert!(!is_recognized_direct_render(legacy, "cam0", "/d"));
        assert!(!is_recognized_direct_render(
            "libcamerasrc camera-name=/c\n! x264enc ! appsink",
            "cam0",
            "/c"
        ));
        assert!(!is_recognized_direct_render(
            "videotestsrc ! x264enc ! appsink",
            "cam0",
            "/c"
        ));
    }
}
