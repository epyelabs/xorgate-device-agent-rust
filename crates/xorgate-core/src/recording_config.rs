//! Fully-resolved recording config: `RecordingSettings` (all-optional,
//! persisted) merged over the plan's defaults. Port of
//! `src/recording/config.ts:12-126`. The path helpers
//! (`recordingsRoot/telemetryRoot/videoRoot/shmDir/shmSocketPathFor`,
//! `config.ts:129-155`) read `XORGATE_HOME` and stay in the HAL/agent.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::settings::{BootDeferMode, RecordingSettings, UploadMode};
use crate::types::ShedConfig;

/// `ResolvedRecordingConfig.telemetry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedTelemetry {
    pub enabled: bool,
    pub rate_hz: u32,
}

/// `ResolvedRecordingConfig.video` (`config.ts:14-39`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedVideo {
    pub enabled: bool,
    pub preset: String,
    pub segment_seconds: u32,
    /// Boot power staging knobs (Phase 2).
    pub boot_defer: BootDeferMode,
    pub boot_defer_max_secs: u32,
    pub boot_defer_min_budget_ma: u32,
    /// GPS/LTE video gate (Phase 7) — stage 2 of the boot policy.
    pub gps_gate: BootDeferMode,
    /// Gate give-up cap, seconds; 0 = no time cap (Q8).
    pub gps_gate_max_secs: u32,
    /// Gate no-progress escape hatch, seconds; 0 = off (Phase 10 iter 2).
    pub gps_gate_no_progress_secs: u32,
    pub spawn_stagger_secs: u32,
    /// Per-camera enable keyed by streamKey. ABSENT KEY = ENABLED, so `{}`
    /// (the default) means "every detected camera".
    pub cameras: IndexMap<String, bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedUpload {
    pub mode: UploadMode,
}

/// Bytes are `f64` like the JS numbers they come from (the parser rounds but
/// never caps `maxBytes`, so `u64` could not hold every value the TS keeps).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRetention {
    #[serde(with = "crate::telemetry::js_num")]
    pub max_bytes: f64,
    #[serde(with = "crate::telemetry::js_num")]
    pub min_free_bytes: f64,
}

/// `ResolvedRecordingConfig` (`config.ts:12`). Key order = the TS interface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRecordingConfig {
    pub telemetry: ResolvedTelemetry,
    pub video: ResolvedVideo,
    pub upload: ResolvedUpload,
    pub retention: ResolvedRetention,
    /// Runtime undervoltage load shedding (Phase 3).
    pub power_shedding: ShedConfig,
}

/// `RECORDING_DEFAULTS` (`config.ts:46-82`). Checked against the cloud's
/// `config-contract.json` defaults in `tests/config_contract.rs`.
pub fn recording_defaults() -> ResolvedRecordingConfig {
    ResolvedRecordingConfig {
        telemetry: ResolvedTelemetry {
            enabled: false,
            rate_hz: 1,
        },
        video: ResolvedVideo {
            enabled: false,
            preset: "720p15-1mbps".to_string(),
            segment_seconds: 60,
            boot_defer: BootDeferMode::Auto,
            boot_defer_max_secs: 90,
            boot_defer_min_budget_ma: 5000,
            gps_gate: BootDeferMode::Auto,
            gps_gate_max_secs: 600,
            gps_gate_no_progress_secs: 120,
            spawn_stagger_secs: 4,
            cameras: IndexMap::new(),
        },
        upload: ResolvedUpload {
            mode: UploadMode::Auto,
        },
        retention: ResolvedRetention {
            max_bytes: 10.0 * 1024f64.powi(3),
            min_free_bytes: 2.0 * 1024f64.powi(3),
        },
        // Q4 STARTING values (2026-07-28) — tune from the field via settings, not code.
        power_shedding: ShedConfig::DEFAULTS,
    }
}

/// `resolveRecordingConfig` (`config.ts:84`): every leaf is
/// `settings value ?? default`, so "no `recording` key at all" and
/// "explicitly all defaults" behave identically.
pub fn resolve_recording_config(s: Option<&RecordingSettings>) -> ResolvedRecordingConfig {
    let d = recording_defaults();
    let tel = s.and_then(|s| s.telemetry.as_ref());
    let vid = s.and_then(|s| s.video.as_ref());
    let up = s.and_then(|s| s.upload.as_ref());
    let ret = s.and_then(|s| s.retention.as_ref());
    let shed = s.and_then(|s| s.power_shedding.as_ref());
    let dp = &d.power_shedding;
    ResolvedRecordingConfig {
        telemetry: ResolvedTelemetry {
            enabled: tel.and_then(|t| t.enabled).unwrap_or(d.telemetry.enabled),
            rate_hz: tel.and_then(|t| t.rate_hz).unwrap_or(d.telemetry.rate_hz),
        },
        video: ResolvedVideo {
            enabled: vid.and_then(|v| v.enabled).unwrap_or(d.video.enabled),
            preset: vid
                .and_then(|v| v.preset.clone())
                .unwrap_or_else(|| d.video.preset.clone()),
            segment_seconds: vid
                .and_then(|v| v.segment_seconds)
                .unwrap_or(d.video.segment_seconds),
            boot_defer: vid.and_then(|v| v.boot_defer).unwrap_or(d.video.boot_defer),
            boot_defer_max_secs: vid
                .and_then(|v| v.boot_defer_max_secs)
                .unwrap_or(d.video.boot_defer_max_secs),
            boot_defer_min_budget_ma: vid
                .and_then(|v| v.boot_defer_min_budget_ma)
                .unwrap_or(d.video.boot_defer_min_budget_ma),
            gps_gate: vid.and_then(|v| v.gps_gate).unwrap_or(d.video.gps_gate),
            gps_gate_max_secs: vid
                .and_then(|v| v.gps_gate_max_secs)
                .unwrap_or(d.video.gps_gate_max_secs),
            gps_gate_no_progress_secs: vid
                .and_then(|v| v.gps_gate_no_progress_secs)
                .unwrap_or(d.video.gps_gate_no_progress_secs),
            spawn_stagger_secs: vid
                .and_then(|v| v.spawn_stagger_secs)
                .unwrap_or(d.video.spawn_stagger_secs),
            cameras: vid
                .and_then(|v| v.cameras.clone())
                .unwrap_or_else(|| d.video.cameras.clone()),
        },
        upload: ResolvedUpload {
            mode: up.and_then(|u| u.mode).unwrap_or(d.upload.mode),
        },
        retention: ResolvedRetention {
            max_bytes: ret
                .and_then(|r| r.max_bytes)
                .unwrap_or(d.retention.max_bytes),
            min_free_bytes: ret
                .and_then(|r| r.min_free_bytes)
                .unwrap_or(d.retention.min_free_bytes),
        },
        power_shedding: ShedConfig {
            enabled: shed.and_then(|p| p.enabled).unwrap_or(dp.enabled),
            uv_samples: shed.and_then(|p| p.uv_samples).unwrap_or(dp.uv_samples),
            uv_window_secs: shed
                .and_then(|p| p.uv_window_secs)
                .unwrap_or(dp.uv_window_secs),
            rail_low_volts: shed
                .and_then(|p| p.rail_low_volts)
                .unwrap_or(dp.rail_low_volts),
            rail_low_secs: shed
                .and_then(|p| p.rail_low_secs)
                .unwrap_or(dp.rail_low_secs),
            escalate_dwell_secs: shed
                .and_then(|p| p.escalate_dwell_secs)
                .unwrap_or(dp.escalate_dwell_secs),
            escalate_duty_pct: shed
                .and_then(|p| p.escalate_duty_pct)
                .unwrap_or(dp.escalate_duty_pct),
            cpu_temp_c: shed.and_then(|p| p.cpu_temp_c).unwrap_or(dp.cpu_temp_c),
            cpu_temp_restore_c: shed
                .and_then(|p| p.cpu_temp_restore_c)
                .unwrap_or(dp.cpu_temp_restore_c),
            cpu_temp_secs: shed
                .and_then(|p| p.cpu_temp_secs)
                .unwrap_or(dp.cpu_temp_secs),
            restore_secs: shed.and_then(|p| p.restore_secs).unwrap_or(dp.restore_secs),
            rail_restore_volts: shed
                .and_then(|p| p.rail_restore_volts)
                .unwrap_or(dp.rail_restore_volts),
            max_sheds_per_hour: shed
                .and_then(|p| p.max_sheds_per_hour)
                .unwrap_or(dp.max_sheds_per_hour),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_with_standard_retention() {
        let d = recording_defaults();
        assert!(!d.telemetry.enabled);
        assert_eq!(d.retention.max_bytes, 10_737_418_240.0);
        assert_eq!(d.retention.min_free_bytes, 2_147_483_648.0);
        assert_eq!(resolve_recording_config(None), d);
        assert_eq!(d.video.preset, crate::video_presets::DEFAULT_VIDEO_PRESET);
    }
}
