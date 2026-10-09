//! Video recording presets (plan D7). Port of `src/recording/video-presets.ts`
//! (62 lines, fully pure): a named table, not free-form, so CPU cost stays
//! bounded and the cloud validates `recording.video.preset` against a closed
//! set (`config-contract.json` pins it — see `tests/config_contract.rs`).

use serde::{Deserialize, Serialize};

/// `VideoPreset` (`video-presets.ts:26`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoPreset {
    /// Settings value, e.g. "1080p15-2mbps".
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// x264enc `bitrate` property unit (kbit/s).
    pub bitrate_kbps: u32,
}

/// `VIDEO_PRESETS` (`video-presets.ts:36`), in table order.
pub const VIDEO_PRESETS: &[VideoPreset] = &[
    VideoPreset {
        name: "1080p30-4mbps",
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate_kbps: 4000,
    },
    VideoPreset {
        name: "1080p15-2mbps",
        width: 1920,
        height: 1080,
        fps: 15,
        bitrate_kbps: 2000,
    },
    VideoPreset {
        name: "720p30-2mbps",
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 2000,
    },
    VideoPreset {
        name: "720p15-2mbps",
        width: 1280,
        height: 720,
        fps: 15,
        bitrate_kbps: 2000,
    },
    VideoPreset {
        name: "720p15-1mbps",
        width: 1280,
        height: 720,
        fps: 15,
        bitrate_kbps: 1000,
    },
];

/// `DEFAULT_VIDEO_PRESET` (`video-presets.ts:45`) — must exist in the table
/// (see `RECORDING_DEFAULTS`).
pub const DEFAULT_VIDEO_PRESET: &str = "720p15-1mbps";

/// `findVideoPreset` (`video-presets.ts:47`).
pub fn find_video_preset(name: &str) -> Option<&'static VideoPreset> {
    VIDEO_PRESETS.iter().find(|p| p.name == name)
}

/// `resolveVideoPreset` (`video-presets.ts:52`): an unknown preset degrades to
/// the default rather than disabling recording (settings are forgiving
/// everywhere — parse-and-clamp, never reject).
pub fn resolve_video_preset(name: &str) -> &'static VideoPreset {
    find_video_preset(name)
        .or_else(|| find_video_preset(DEFAULT_VIDEO_PRESET))
        .expect("DEFAULT_VIDEO_PRESET is in VIDEO_PRESETS")
}

/// `videoPresetNames` (`video-presets.ts:60`).
pub fn video_preset_names() -> Vec<&'static str> {
    VIDEO_PRESETS.iter().map(|p| p.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_and_fallback() {
        assert_eq!(video_preset_names().len(), 5);
        assert_eq!(resolve_video_preset("nope").name, DEFAULT_VIDEO_PRESET);
        assert_eq!(resolve_video_preset("1080p30-4mbps").bitrate_kbps, 4000);
        assert!(find_video_preset("").is_none());
    }
}
