//! Video capture argv builders. Port of the PURE part of
//! `src/recording/video-capture.ts` (`:84-260`: `CaptureCamera`,
//! `resolveBridgedCameras`, `captureSourceHead`, `CapturePipelineSpec`,
//! `buildBridgeOnlyPipelineArgs`, `buildCapturePipelineArgs`,
//! `bridgedKvsPipeline`) plus the pure helpers of `src/media/pipeline.ts`
//! (`pipelineFileFor`/`cameraFileFor`/`directBackupFor` `:14-30`,
//! `extractCameraName` `:45-48`, and the decisions inside
//! `isBridgedPipeline` `:36-42` / `resolveCameraName` `:55-71`).
//!
//! Out of core: `VideoCaptureSupervisor` (`video-capture.ts:261-1021`:
//! spawn/watch/setPriority/fs.watch/sweep timers/shm reaper) — Phase 3.
//! Everything the supervisor hands to `gst-launch-1.0` is built here, token
//! for token (acceptance B6; the fixtures hold the exact argv the TS built).
//!
//! Shm bridge contract (`video-capture.ts:14-18`): shmsrc does NO caps
//! negotiation, both sides must state [`SHM_BRIDGE_CAPS`] EXACTLY
//! (agent-anatomy invariant 8).

use serde::{Deserialize, Serialize};

use crate::cameras::{js_trim, js_ws};
use crate::hardware_block::EncoderToken;
use crate::pipeline_builder::{build_bridged_kvs_pipeline, build_record_encode_args};
use crate::video_presets::VideoPreset;

pub use crate::pipeline_builder::SHM_BRIDGE_CAPS;

/// `KvsChannel` (`src/media/manifest.ts:19-22`): one KVS signalling channel
/// per camera stream. Declared here (not in WP1's `types`) because only the
/// capture builders need it in core; the manifest loader itself is Phase 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvsChannel {
    pub stream_key: String,
    pub channel_name: String,
}

/// `CaptureCamera` (`video-capture.ts:84-88`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCamera {
    pub stream_key: String,
    /// libcamera camera-name (e.g. `/base/axi/pcie@…/imx290@1a`).
    pub camera_name: String,
}

// ---------------------------------------------------------------------------
// src/media/pipeline.ts — per-stream file names + the bridged-mode helpers
// ---------------------------------------------------------------------------

/// Minimal `path.join(dir, name)` for the shapes the agent produces: an
/// absolute, already-normalised directory plus a bare file name. Node's
/// join also collapses `.`/`..` and duplicate slashes; the agent never
/// hands those in (session dirs come from its own `join` calls), so that
/// normalisation is deliberately NOT reproduced here.
pub fn posix_join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        return name.to_string();
    }
    let base = dir.trim_end_matches('/');
    if base.is_empty() {
        format!("/{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// `pipelineFileFor` (`pipeline.ts:14-16`): `<pipelineDir>/<streamKey>.gst`.
pub fn pipeline_file_for(pipeline_dir: &str, stream_key: &str) -> String {
    posix_join(pipeline_dir, &format!("{stream_key}.gst"))
}

/// `cameraFileFor` (`pipeline.ts:24-26`): `<pipelineDir>/<streamKey>.camera`.
pub fn camera_file_for(pipeline_dir: &str, stream_key: &str) -> String {
    posix_join(pipeline_dir, &format!("{stream_key}.camera"))
}

/// `directBackupFor` (`pipeline.ts:28-30`): `<pipelineDir>/<streamKey>.gst.direct`.
pub fn direct_backup_for(pipeline_dir: &str, stream_key: &str) -> String {
    posix_join(pipeline_dir, &format!("{stream_key}.gst.direct"))
}

/// `isBridgedPipeline` (`pipeline.ts:36-42`) over the `.gst` content: bridged
/// pipelines read from `shmsrc` instead of owning a camera. An unreadable
/// file (`None`) is "not bridged".
pub fn is_bridged_pipeline(gst: Option<&str>) -> bool {
    gst.is_some_and(|s| s.contains("shmsrc"))
}

/// `extractCameraName` (`pipeline.ts:45-48`): pull `camera-name=...` out of a
/// gst-launch string (libcamera names have no spaces). Regex
/// `/camera-name=(\S+)/` — the first occurrence followed by at least one
/// non-whitespace char wins — then `^"(.*)"$` strips one pair of quotes.
pub fn extract_camera_name(gst: &str) -> Option<String> {
    const KEY: &str = "camera-name=";
    let mut from = 0;
    while let Some(i) = gst[from..].find(KEY) {
        let start = from + i + KEY.len();
        let tail = &gst[start..];
        let len = tail.len() - tail.trim_start_matches(|c: char| !js_ws(c)).len();
        if len > 0 {
            let raw = &tail[..len];
            let unquoted = raw
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(raw);
            return Some(unquoted.to_string());
        }
        from = start;
    }
    None
}

/// `resolveCameraName` (`pipeline.ts:55-71`) over the three file contents
/// (`None` = unreadable): the `.camera` file (trimmed, non-empty) wins, else
/// the camera name parsed out of the direct-pipeline backup, else out of the
/// current `.gst` (covers a hand-rolled direct setup).
pub fn resolve_camera_name(
    camera_file: Option<&str>,
    direct_backup: Option<&str>,
    gst: Option<&str>,
) -> Option<String> {
    if let Some(v) = camera_file {
        let v = js_trim(v);
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    [direct_backup, gst]
        .into_iter()
        .flatten()
        .find_map(extract_camera_name)
}

/// The `console.warn` line `resolveBridgedCameras` logs for a bridged stream
/// without a camera identity (`video-capture.ts:105-108`), verbatim.
pub fn bridged_skip_warning(stream_key: &str) -> String {
    format!(
        "[video-capture] {stream_key}: bridged .gst but no camera identity (missing {stream_key}.camera) — skipping"
    )
}

/// `resolveBridgedCameras` (`video-capture.ts:96-114`): the channels the
/// capture daemon may own — bridged `.gst` (the KVS master reads shm, so the
/// camera is free) AND a resolvable camera identity. A channel whose `.gst`
/// is still direct is left alone (rollout safety, plan D1/DoD 8).
///
/// `read(path)` stands in for `readFileSync(path, "utf8")`, returning `None`
/// where the TS would throw. Returns the cameras and, for each skipped
/// bridged stream, the warning text (`bridged_skip_warning`) in order.
pub fn resolve_bridged_cameras(
    pipeline_dir: &str,
    channels: &[KvsChannel],
    read: &dyn Fn(&str) -> Option<String>,
) -> (Vec<CaptureCamera>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for ch in channels {
        let key = ch.stream_key.as_str();
        let gst = read(&pipeline_file_for(pipeline_dir, key));
        if !is_bridged_pipeline(gst.as_deref()) {
            continue;
        }
        let camera_file = read(&camera_file_for(pipeline_dir, key));
        let backup = read(&direct_backup_for(pipeline_dir, key));
        match resolve_camera_name(camera_file.as_deref(), backup.as_deref(), gst.as_deref()) {
            Some(camera_name) => out.push(CaptureCamera {
                stream_key: key.to_string(),
                camera_name,
            }),
            None => warnings.push(bridged_skip_warning(key)),
        }
    }
    (out, warnings)
}

// ---------------------------------------------------------------------------
// src/recording/video-capture.ts:116-260 — argv builders
// ---------------------------------------------------------------------------

/// `captureSourceHead` (`video-capture.ts:126-133`): the element(s) that
/// produce raw frames for the tee. An explicit `head` (replay mode) wins,
/// then the `XORGATE_CAPTURE_SOURCE_HEAD` env override (`env_override`, the
/// caller reads the environment), whitespace-split into argv tokens, else
/// `libcamerasrc camera-name=<cam>` byte-for-byte.
pub fn capture_source_head(
    camera_name: &str,
    head: Option<&[String]>,
    env_override: Option<&str>,
) -> Vec<String> {
    if let Some(h) = head {
        if !h.is_empty() {
            return h.to_vec();
        }
    }
    if let Some(o) = env_override {
        let o = js_trim(o);
        if !o.is_empty() {
            // `override.split(/\s+/)` on a trimmed non-empty string: no empty tokens.
            return o
                .split(js_ws)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    vec![
        "libcamerasrc".to_string(),
        format!("camera-name={camera_name}"),
    ]
}

/// `CapturePipelineSpec` (`video-capture.ts:145-160`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePipelineSpec {
    pub camera_name: String,
    /// Explicit pipeline head (replay mode); absent = libcamerasrc for `camera_name`.
    pub head: Option<Vec<String>>,
    pub preset: VideoPreset,
    /// `BigInt(segmentSeconds)` in the TS — an integer by contract (settings clamp 10..600).
    pub segment_seconds: u64,
    /// Session dir splitmuxsink writes fragments into.
    pub output_dir: String,
    pub shm_socket_path: String,
    /// Encoder token from the device's hardware block (cm4-support D6). Absent = sw.
    pub encoder: Option<EncoderToken>,
}

/// `buildBridgeOnlyPipelineArgs` (`video-capture.ts:168-184`): the daemon
/// still owns the camera and feeds the shm socket so live streaming keeps
/// working — no encode, no disk.
pub fn build_bridge_only_pipeline_args(
    camera_name: &str,
    shm_socket_path: &str,
    head: Option<&[String]>,
    env_override: Option<&str>,
) -> Vec<String> {
    let mut args = capture_source_head(camera_name, head, env_override);
    args.extend([
        "!".to_string(),
        SHM_BRIDGE_CAPS.to_string(),
        "!".to_string(),
        "shmsink".to_string(),
        format!("socket-path={shm_socket_path}"),
        "wait-for-connection=false".to_string(),
        "sync=false".to_string(),
        "shm-size=20971520".to_string(),
    ]);
    args
}

/// `buildCapturePipelineArgs` (`video-capture.ts:190-246`): the capture
/// pipeline as gst-launch argv tokens (argv, not a shell string — no quoting
/// hazards; camera names contain no spaces). `-e` turns SIGINT into EOS so
/// splitmuxsink finalises the active fragment; fragmented MP4 with a moof per
/// second; the bridge branch is a leaky queue → videorate → videoscale →
/// [`SHM_BRIDGE_CAPS`] → shmsink.
pub fn build_capture_pipeline_args(
    spec: &CapturePipelineSpec,
    env_override: Option<&str>,
) -> Vec<String> {
    let preset = &spec.preset;
    let key_int_max = preset.fps * 2; // one keyframe ≈ every 2 s → seekable fragments
    let mut args = vec!["-e".to_string()];
    args.extend(capture_source_head(
        &spec.camera_name,
        spec.head.as_deref(),
        env_override,
    ));
    args.extend([
        "!".to_string(),
        format!(
            "video/x-raw,format=I420,width={},height={},framerate={}/1",
            preset.width, preset.height, preset.fps
        ),
        "!".to_string(),
        "tee".to_string(),
        "name=t".to_string(),
        "t.".to_string(),
        "!".to_string(),
        "queue".to_string(),
        "!".to_string(),
    ]);
    args.extend(build_record_encode_args(
        spec.encoder.unwrap_or(EncoderToken::SwH264),
        preset.bitrate_kbps,
        key_int_max,
    ));
    args.extend([
        "!".to_string(),
        "h264parse".to_string(),
        "!".to_string(),
        "splitmuxsink".to_string(),
        // One argv token: gst-launch rejoins argv with spaces and its parser
        // applies fragment-duration to the mp4mux child.
        "muxer=mp4mux fragment-duration=1000".to_string(),
        format!("location={}", posix_join(&spec.output_dir, "%05d.mp4")),
        format!("max-size-time={}", spec.segment_seconds * 1_000_000_000),
        "t.".to_string(),
        "!".to_string(),
        "queue".to_string(),
        "leaky=downstream".to_string(),
        "max-size-buffers=5".to_string(),
        "!".to_string(),
        "videorate".to_string(),
        "!".to_string(),
        "video/x-raw,framerate=15/1".to_string(),
        "!".to_string(),
        "videoscale".to_string(),
        "!".to_string(),
        SHM_BRIDGE_CAPS.to_string(),
        "!".to_string(),
        "shmsink".to_string(),
        format!("socket-path={}", spec.shm_socket_path),
        "wait-for-connection=false".to_string(),
        "sync=false".to_string(),
        "shm-size=20971520".to_string(),
    ]);
    args
}

/// `bridgedKvsPipeline` (`video-capture.ts:254-259`): content of a BRIDGED
/// per-stream `.gst`; `encoder` defaults to `sw-h264`.
pub fn bridged_kvs_pipeline(shm_socket_path: &str, encoder: Option<EncoderToken>) -> String {
    build_bridged_kvs_pipeline(encoder.unwrap_or(EncoderToken::SwH264), shm_socket_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_camera_name_regex_semantics() {
        assert_eq!(
            extract_camera_name("libcamerasrc camera-name=/base/i2c@88000/imx290@1a ! caps")
                .as_deref(),
            Some("/base/i2c@88000/imx290@1a")
        );
        assert_eq!(extract_camera_name("videotestsrc ! appsink"), None);
        // First occurrence with a non-empty token wins; quotes stripped once.
        assert_eq!(
            extract_camera_name("camera-name= camera-name=\"/q\" camera-name=/z").as_deref(),
            Some("/q")
        );
        assert_eq!(extract_camera_name("camera-name=\"\"").as_deref(), Some(""));
        assert_eq!(extract_camera_name("camera-name=\"").as_deref(), Some("\""));
        assert_eq!(extract_camera_name("camera-name="), None);
    }

    #[test]
    fn source_head_precedence() {
        let explicit = vec!["filesrc".to_string(), "location=/a".to_string()];
        assert_eq!(
            capture_source_head("/c", Some(&explicit), Some("x y")),
            explicit
        );
        assert_eq!(
            capture_source_head("/c", Some(&[]), Some("  videotestsrc   is-live=true ")),
            vec!["videotestsrc", "is-live=true"]
        );
        assert_eq!(
            capture_source_head("/c", None, Some("   ")),
            vec!["libcamerasrc", "camera-name=/c"]
        );
    }

    #[test]
    fn join_shapes() {
        assert_eq!(posix_join("/out", "%05d.mp4"), "/out/%05d.mp4");
        assert_eq!(posix_join("/out/", "x"), "/out/x");
        assert_eq!(posix_join("", "x"), "x");
        assert_eq!(posix_join("/", "x"), "/x");
        assert_eq!(pipeline_file_for("/p", "cam0"), "/p/cam0.gst");
        assert_eq!(camera_file_for("/p", "cam0"), "/p/cam0.camera");
        assert_eq!(direct_backup_for("/p", "cam0"), "/p/cam0.gst.direct");
    }

    #[test]
    fn camera_name_resolution_order() {
        assert_eq!(
            resolve_camera_name(Some(" /cf \n"), Some("libcamerasrc camera-name=/b"), None)
                .as_deref(),
            Some("/cf")
        );
        assert_eq!(
            resolve_camera_name(
                Some("  "),
                Some("libcamerasrc camera-name=/b"),
                Some("camera-name=/g")
            )
            .as_deref(),
            Some("/b")
        );
        assert_eq!(
            resolve_camera_name(None, None, Some("libcamerasrc camera-name=/g ! x")).as_deref(),
            Some("/g")
        );
        assert_eq!(resolve_camera_name(None, None, Some("shmsrc ! x")), None);
        assert!(is_bridged_pipeline(Some("shmsrc socket-path=/x ! appsink")));
        assert!(!is_bridged_pipeline(None));
    }
}
