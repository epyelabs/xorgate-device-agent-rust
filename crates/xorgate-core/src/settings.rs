//! Persisted agent settings (settings.json under XORGATE_HOME) — the PURE
//! half of `src/daemon/settings.ts` (642 lines): the namespace interfaces,
//! the constants/regexes, the clamp parsers and the allowlist rewriter
//! (`parseSettingsObject`, `settings.ts:616`), plus the absent / unreadable /
//! invalid / ok classification of `readSettingsFile` (`:589-607`) over an
//! already-performed read, so the Phase 2 HAL wrapper is a thin `readFileSync`
//! plus [`settings_from_read`]. `settingsPath`, `readSettingsFile`'s I/O,
//! `loadSettings`' default path and `saveSettings` (`atomicWriteJson`) stay
//! out of core.
//!
//! ⚠️ This is an ALLOWLIST rewriter: every key the parser does not know is
//! dropped, so the next save by ANY writer destroys it. The sim7600-lte daemon
//! greps settings.json by exact path, so namespace names are load-bearing and
//! never renamed (agent-anatomy invariant 10).
//!
//! Serde: every struct below serialises in the key order the TS PARSER
//! inserts keys (`parseRecordingSettings` etc.), which is the order the
//! file carries after any `loadSettings` → `saveSettings` round trip and after
//! every cloud apply (`applyCloudConfig` writes parser output). Note this is
//! NOT always the TS interface's declaration order: `RecordingSettings` is
//! `telemetry, video, upload, powerShedding, retention` (`settings.ts:417-549`)
//! and `powerShedding` is `enabled`, the eight integer knobs, then the four
//! decimal knobs (`:497-533`). Optional fields are omitted, never `null`.
//! Numbers print like JS (`4` not `4.0`).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::js::object_key_order;
use crate::orientation::{parse_imu_mount, ImuMount};
use crate::round::js_math_round;
use crate::telemetry::{js_num_opt, RunMode};
use crate::types::CameraRotationDeg;

// ---------------------------------------------------------------------------
// constants / regexes (hand-coded char classes; patterns kept verbatim in docs)
// ---------------------------------------------------------------------------

/// `RECORDING_RATE_HZ_MIN` (`settings.ts:395`): the IMU caps at 20 Hz.
pub const RECORDING_RATE_HZ_MIN: f64 = 1.0;
/// `RECORDING_RATE_HZ_MAX` (`settings.ts:396`).
pub const RECORDING_RATE_HZ_MAX: f64 = 20.0;

/// `SIMULATION_GAP_SECONDS_MIN` (`settings.ts:302`).
pub const SIMULATION_GAP_SECONDS_MIN: f64 = 0.0;
/// `SIMULATION_GAP_SECONDS_MAX` (`settings.ts:303`).
pub const SIMULATION_GAP_SECONDS_MAX: f64 = 3600.0;

/// `APN_RE` (`settings.ts:215`): `/^[A-Za-z0-9.-]{0,100}$/` — the one charset
/// this stack will ever interpolate into an AT command.
pub const APN_RE: &str = "^[A-Za-z0-9.-]{0,100}$";
/// `SCENARIO_ID_RE` (`settings.ts:300`): `/^[a-z0-9][a-z0-9-]{0,63}$/`.
pub const SCENARIO_ID_RE: &str = "^[a-z0-9][a-z0-9-]{0,63}$";
/// `STREAM_KEY_RE` (`settings.ts:403`): `/^[A-Za-z0-9_-]{1,32}$/`.
pub const STREAM_KEY_RE: &str = "^[A-Za-z0-9_-]{1,32}$";

/// `APN_RE.test(s)`.
pub fn apn_re_test(s: &str) -> bool {
    s.chars().count() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// `SCENARIO_ID_RE.test(s)`.
pub fn scenario_id_re_test(s: &str) -> bool {
    let mut it = s.chars();
    let Some(first) = it.next() else {
        return false;
    };
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    ok(first) && s.chars().count() <= 64 && it.all(|c| ok(c) || c == '-')
}

/// A camera-map key the TS parsers can never keep: `__proto__` passes
/// `STREAM_KEY_RE` and `JSON.parse` keeps it as an own property, but both
/// camera maps are built by plain assignment into an object literal
/// (`c[key] = value`, `settings.ts:481`; `out[key] = { rotationDeg }`, `:276`),
/// which runs the `Object.prototype.__proto__` setter instead of defining a
/// key — a boolean is ignored, an entry object becomes the map's prototype —
/// so the key never reaches `Object.keys` / `JSON.stringify`. Only these two
/// maps take untrusted keys; every other namespace reads fixed field names.
pub fn js_assign_drops_key(key: &str) -> bool {
    key == "__proto__"
}

/// `STREAM_KEY_RE.test(s)`.
pub fn stream_key_re_test(s: &str) -> bool {
    let n = s.chars().count();
    (1..=32).contains(&n)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// ---------------------------------------------------------------------------
// JS semantics helpers
// ---------------------------------------------------------------------------

/// `Math.min(max, Math.max(min, Math.round(v)))` for the integer knobs.
fn clamp_round(v: f64, min: f64, max: f64) -> f64 {
    js_math_round(v).max(min).min(max)
}

/// `Math.min(max, Math.max(min, v))` for the decimal knobs.
fn clamp(v: f64, min: f64, max: f64) -> f64 {
    v.max(min).min(max)
}

/// A JSON number as the TS sees it (`typeof v === "number" && Number.isFinite(v)`;
/// every JSON number is finite).
fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|f| f.is_finite())
}

fn boolean(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

fn string(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

/// `v && typeof v === "object"` — the TS check lets arrays through, but every
/// parser then reads named properties off them (always `undefined`), so an
/// array behaves exactly like `{}`. Returning an empty map for arrays keeps
/// that equivalence without special-casing each call site.
fn object(v: &Value) -> Option<std::borrow::Cow<'_, serde_json::Map<String, Value>>> {
    match v {
        Value::Object(m) => Some(std::borrow::Cow::Borrowed(m)),
        Value::Array(_) => Some(std::borrow::Cow::Owned(serde_json::Map::new())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// recording — `settings.ts:36-121`, parser `:412-552`
// ---------------------------------------------------------------------------

/// `"auto" | "always" | "off"` — shared by `bootDefer` and `gpsGate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootDeferMode {
    Auto,
    Always,
    Off,
}

impl BootDeferMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BootDeferMode::Auto => "auto",
            BootDeferMode::Always => "always",
            BootDeferMode::Off => "off",
        }
    }
    fn parse(v: Option<&Value>) -> Option<Self> {
        match string(v)? {
            "auto" => Some(BootDeferMode::Auto),
            "always" => Some(BootDeferMode::Always),
            "off" => Some(BootDeferMode::Off),
            _ => None,
        }
    }
}

/// `upload.mode`: `"auto" | "off"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadMode {
    Auto,
    Off,
}

impl UploadMode {
    pub fn as_str(self) -> &'static str {
        match self {
            UploadMode::Auto => "auto",
            UploadMode::Off => "off",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_hz: Option<u32>,
}

/// `RecordingSettings.video` (`settings.ts:38-79`), parser order (`:429-486`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VideoSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_seconds: Option<u32>,
    /// Boot power staging: "auto" (default) defers heavy encoders at boot only
    /// on a constrained PSU budget; "always" every boot; "off" start-everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_defer: Option<BootDeferMode>,
    /// Hard cap on the boot defer, seconds (default 90; 0 = feature off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_defer_max_secs: Option<u32>,
    /// `auto` skips the defer at/above this negotiated budget in mA (default 5000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_defer_min_budget_ma: Option<u32>,
    /// GPS/LTE video gate (Phase 7 / Q8) — stage 2 of the boot policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_gate: Option<BootDeferMode>,
    /// Gate give-up cap, seconds (default 600; 0 = wait indefinitely, clamp ≤3600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_gate_max_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_gate_no_progress_secs: Option<u32>,
    /// Inter-camera spawn delay, seconds (default 4; 0 = simultaneous).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_stagger_secs: Option<u32>,
    /// Per-camera enable keyed by streamKey; an ABSENT key means enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cameras: Option<IndexMap<String, bool>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UploadSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<UploadMode>,
}

/// `RecordingSettings.retention` (`settings.ts:81`). Bytes stay `f64`: the
/// parser rounds but never caps `maxBytes` (`:541-543`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RetentionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub max_bytes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub min_free_bytes: Option<f64>,
}

/// `RecordingSettings.powerShedding` (`settings.ts:89-120`) in PARSER order
/// (`:497-533`): `enabled`, the integer knobs, then the decimal knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PowerSheddingSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Shed when this many CURRENT-undervoltage samples land within uvWindowSecs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uv_samples: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uv_window_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rail_low_secs: Option<u32>,
    /// Trailing window (seconds) evaluated for escalation to the next rung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate_dwell_secs: Option<u32>,
    /// Escalate when the supply was bad for at least this % of the dwell (clamp 50–100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate_duty_pct: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_temp_secs: Option<u32>,
    /// Clean interval per restored rung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_secs: Option<u32>,
    /// Shed cycles per rolling hour before parking at the shed level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sheds_per_hour: Option<u32>,
    /// ... OR the 5 V rail stays below this for railLowSecs straight.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub rail_low_volts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub rail_restore_volts: Option<f64>,
    /// Thermal arm: shed live streaming at/above this for cpuTempSecs straight.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub cpu_temp_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub cpu_temp_restore_c: Option<f64>,
}

/// `RecordingSettings` (`settings.ts:36`): the `recording` namespace. Every
/// field optional; defaults live in [`crate::recording_config`]. Parser order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RecordingSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetrySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<UploadSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_shedding: Option<PowerSheddingSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionSettings>,
}

/// `parseRecordingSettings` (`settings.ts:412`): validate an untrusted
/// `recording` value, dropping anything mistyped (a partially-valid object
/// still applies). Numbers are clamped, not rejected. None when nothing
/// valid remains.
pub fn parse_recording_settings(v: &Value) -> Option<RecordingSettings> {
    let src = object(v)?;
    let mut out = RecordingSettings::default();

    if let Some(tel) = src.get("telemetry").and_then(object) {
        let mut t = TelemetrySettings {
            enabled: boolean(tel.get("enabled")),
            rate_hz: None,
        };
        if let Some(r) = num(tel.get("rateHz")) {
            t.rate_hz = Some(clamp_round(r, RECORDING_RATE_HZ_MIN, RECORDING_RATE_HZ_MAX) as u32);
        }
        if t != TelemetrySettings::default() {
            out.telemetry = Some(t);
        }
    }

    if let Some(vid) = src.get("video").and_then(object) {
        let mut o = VideoSettings {
            enabled: boolean(vid.get("enabled")),
            preset: string(vid.get("preset"))
                .filter(|p| !p.is_empty())
                .map(str::to_string),
            ..VideoSettings::default()
        };
        if let Some(n) = num(vid.get("segmentSeconds")) {
            o.segment_seconds = Some(clamp_round(n, 10.0, 600.0) as u32);
        }
        o.boot_defer = BootDeferMode::parse(vid.get("bootDefer"));
        if let Some(n) = num(vid.get("bootDeferMaxSecs")) {
            // 0 stays meaningful (feature off); no defer past 10 minutes.
            o.boot_defer_max_secs = Some(clamp_round(n, 0.0, 600.0) as u32);
        }
        if let Some(n) = num(vid.get("bootDeferMinBudgetMa")) {
            o.boot_defer_min_budget_ma = Some(clamp_round(n, 0.0, 10_000.0) as u32);
        }
        o.gps_gate = BootDeferMode::parse(vid.get("gpsGate"));
        if let Some(n) = num(vid.get("gpsGateMaxSecs")) {
            // 0 stays meaningful (Q8: no time cap); 3600 is the sanity ceiling.
            o.gps_gate_max_secs = Some(clamp_round(n, 0.0, 3600.0) as u32);
        }
        if let Some(n) = num(vid.get("gpsGateNoProgressSecs")) {
            // 0 = disable the escape hatch (back to cap-only behavior).
            o.gps_gate_no_progress_secs = Some(clamp_round(n, 0.0, 3600.0) as u32);
        }
        if let Some(n) = num(vid.get("spawnStaggerSecs")) {
            o.spawn_stagger_secs = Some(clamp_round(n, 0.0, 30.0) as u32);
        }
        // Per-camera enables (Phase 6): non-boolean values and implausible
        // keys are dropped rather than rejected; arrays are not maps.
        if let Some(Value::Object(cams)) = vid.get("cameras") {
            let c: IndexMap<String, bool> = cams
                .iter()
                .filter_map(|(k, v)| match v {
                    Value::Bool(b) if stream_key_re_test(k) && !js_assign_drops_key(k) => {
                        Some((k.clone(), *b))
                    }
                    _ => None,
                })
                .collect();
            if !c.is_empty() {
                o.cameras = Some(object_key_order(c));
            }
        }
        if o != VideoSettings::default() {
            out.video = Some(o);
        }
    }

    if let Some(up) = src.get("upload").and_then(object) {
        match string(up.get("mode")) {
            Some("auto") => {
                out.upload = Some(UploadSettings {
                    mode: Some(UploadMode::Auto),
                })
            }
            Some("off") => {
                out.upload = Some(UploadSettings {
                    mode: Some(UploadMode::Off),
                })
            }
            _ => {}
        }
    }

    if let Some(shed) = src.get("powerShedding").and_then(object) {
        let mut p = PowerSheddingSettings {
            enabled: boolean(shed.get("enabled")),
            ..PowerSheddingSettings::default()
        };
        // Integer knobs, clamped (`settings.ts:501-518`).
        let int = |key: &str, min: f64, max: f64| {
            num(shed.get(key)).map(|v| clamp_round(v, min, max) as u32)
        };
        p.uv_samples = int("uvSamples", 1.0, 60.0);
        p.uv_window_secs = int("uvWindowSecs", 5.0, 300.0);
        p.rail_low_secs = int("railLowSecs", 3.0, 120.0);
        p.escalate_dwell_secs = int("escalateDwellSecs", 5.0, 600.0);
        // 50 % floor: below "more triggered than not" the ladder would
        // escalate off noise; 100 = the pre-Phase-6 continuous-dwell rule.
        p.escalate_duty_pct = int("escalateDutyPct", 50.0, 100.0);
        p.cpu_temp_secs = int("cpuTempSecs", 5.0, 120.0);
        p.restore_secs = int("restoreSecs", 10.0, 3600.0);
        p.max_sheds_per_hour = int("maxShedsPerHour", 0.0, 60.0);
        // Voltage/temperature thresholds keep their decimals (`:522-533`).
        let dec = |key: &str, min: f64, max: f64| num(shed.get(key)).map(|v| clamp(v, min, max));
        p.rail_low_volts = dec("railLowVolts", 4.0, 5.2);
        p.rail_restore_volts = dec("railRestoreVolts", 4.0, 5.4);
        p.cpu_temp_c = dec("cpuTempC", 50.0, 90.0);
        p.cpu_temp_restore_c = dec("cpuTempRestoreC", 40.0, 85.0);
        if p != PowerSheddingSettings::default() {
            out.power_shedding = Some(p);
        }
    }

    if let Some(ret) = src.get("retention").and_then(object) {
        let r = RetentionSettings {
            max_bytes: num(ret.get("maxBytes"))
                .filter(|b| *b > 0.0)
                .map(js_math_round),
            min_free_bytes: num(ret.get("minFreeBytes"))
                .filter(|b| *b >= 0.0)
                .map(js_math_round),
        };
        if r != RetentionSettings::default() {
            out.retention = Some(r);
        }
    }

    (out != RecordingSettings::default()).then_some(out)
}

// ---------------------------------------------------------------------------
// lteRecovery — `settings.ts:132-155`
// ---------------------------------------------------------------------------

/// What a live GNSS fix means for a modem reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpsFixResetPolicy {
    Defer,
    VetoDeadzone,
}

/// Dead-zone nudge pacing preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeadZoneProfile {
    Conservative,
    Balanced,
    Aggressive,
}

/// `LteRecoverySettings` (`settings.ts:132`): read LIVE by the sim7600-lte
/// daemon; the daemon resolves anything missing to defer / balanced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LteRecoverySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_fix_reset_policy: Option<GpsFixResetPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_zone_profile: Option<DeadZoneProfile>,
}

/// `parseLteRecoverySettings` (`settings.ts:144`).
pub fn parse_lte_recovery_settings(v: &Value) -> Option<LteRecoverySettings> {
    let src = object(v)?;
    let out = LteRecoverySettings {
        gps_fix_reset_policy: match string(src.get("gpsFixResetPolicy")) {
            Some("defer") => Some(GpsFixResetPolicy::Defer),
            Some("veto-deadzone") => Some(GpsFixResetPolicy::VetoDeadzone),
            _ => None,
        },
        dead_zone_profile: match string(src.get("deadZoneProfile")) {
            Some("conservative") => Some(DeadZoneProfile::Conservative),
            Some("balanced") => Some(DeadZoneProfile::Balanced),
            Some("aggressive") => Some(DeadZoneProfile::Aggressive),
            _ => None,
        },
    };
    (out != LteRecoverySettings::default()).then_some(out)
}

// ---------------------------------------------------------------------------
// timeSync — `settings.ts:167-196` (+ `TIME_SYNC_DEFAULTS`, `clock-step.ts:47-66`)
// ---------------------------------------------------------------------------

/// `TimeSyncSettings` (`settings.ts:167`): read LIVE by the clock stepper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TimeSyncSettings {
    /// Master enable for GPS/NITZ clock stepping while NTP is unsynced (default on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline_sources: Option<bool>,
    /// Step only when |clock offset| exceeds this many seconds (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_threshold_secs: Option<u32>,
    /// Hard cap on clock steps per boot (default 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps_per_boot: Option<u32>,
}

/// `parseTimeSyncSettings` (`settings.ts:181`).
pub fn parse_time_sync_settings(v: &Value) -> Option<TimeSyncSettings> {
    let src = object(v)?;
    let out = TimeSyncSettings {
        offline_sources: boolean(src.get("offlineSources")),
        // Floor 2 s: below NITZ/NMEA-arrival granularity a threshold would flap.
        step_threshold_secs: num(src.get("stepThresholdSecs"))
            .map(|t| clamp_round(t, 2.0, 600.0) as u32),
        max_steps_per_boot: num(src.get("maxStepsPerBoot"))
            .map(|m| clamp_round(m, 1.0, 20.0) as u32),
    };
    (out != TimeSyncSettings::default()).then_some(out)
}

/// `TimeSyncConfig` (`src/agent/clock-step.ts:47`): the resolved knobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSyncConfig {
    pub offline_sources: bool,
    pub step_threshold_secs: u32,
    pub max_steps_per_boot: u32,
}

/// `TIME_SYNC_DEFAULTS` (`clock-step.ts:53`). Parked here until WP2 ports
/// `clock_step`; the contract test pins it against the cloud defaults.
pub const TIME_SYNC_DEFAULTS: TimeSyncConfig = TimeSyncConfig {
    offline_sources: true,
    step_threshold_secs: 10,
    max_steps_per_boot: 4,
};

/// `resolveTimeSyncConfig` (`clock-step.ts:59`).
pub fn resolve_time_sync_config(s: Option<&TimeSyncSettings>) -> TimeSyncConfig {
    let d = TIME_SYNC_DEFAULTS;
    TimeSyncConfig {
        offline_sources: s
            .and_then(|s| s.offline_sources)
            .unwrap_or(d.offline_sources),
        step_threshold_secs: s
            .and_then(|s| s.step_threshold_secs)
            .unwrap_or(d.step_threshold_secs),
        max_steps_per_boot: s
            .and_then(|s| s.max_steps_per_boot)
            .unwrap_or(d.max_steps_per_boot),
    }
}

// ---------------------------------------------------------------------------
// cellular — `settings.ts:205-229`
// ---------------------------------------------------------------------------

/// `CellularSettings` (`settings.ts:205`): `""` or absent = don't touch modem NV.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CellularSettings {
    /// APN for PDP context 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apn: Option<String>,
}

/// `parseCellularSettings` (`settings.ts:224`): STRICTER than its siblings —
/// an invalid apn is DROPPED (never repaired), which resolves to "don't touch
/// the modem", the only safe fallback.
pub fn parse_cellular_settings(v: &Value) -> Option<CellularSettings> {
    let src = object(v)?;
    let apn = string(src.get("apn")).filter(|a| apn_re_test(a))?;
    Some(CellularSettings {
        apn: Some(apn.to_string()),
    })
}

// ---------------------------------------------------------------------------
// cameraMount — `settings.ts:248-279`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraMountEntry {
    pub rotation_deg: CameraRotationDeg,
}

/// `CameraMountSettings` (`settings.ts:248`): per-camera 0/180 keyed by KVS
/// stream key. An ABSENT camera key is UNMANAGED.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CameraMountSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cameras: Option<IndexMap<String, CameraMountEntry>>,
}

/// `parseCameraMountSettings` (`settings.ts:266`): forgiving like its
/// siblings, except a rotation is never rounded to the nearest legal value.
/// `{cameras: {}}` parses to itself; no usable `cameras` object → None
/// (the cloud's "revert this namespace" marker).
pub fn parse_camera_mount_settings(v: &Value) -> Option<CameraMountSettings> {
    let Value::Object(src) = v else {
        return None;
    };
    let Some(Value::Object(cameras)) = src.get("cameras") else {
        return None;
    };
    let mut out: IndexMap<String, CameraMountEntry> = IndexMap::new();
    for (key, entry) in cameras {
        if !stream_key_re_test(key) {
            continue;
        }
        let Value::Object(entry) = entry else {
            continue;
        };
        let Some(deg) = num(entry.get("rotationDeg")).and_then(CameraRotationDeg::from_f64) else {
            continue;
        };
        if js_assign_drops_key(key) {
            continue; // `out[key] = …` sets the prototype (`settings.ts:276`)
        }
        out.insert(key.clone(), CameraMountEntry { rotation_deg: deg });
    }
    Some(CameraMountSettings {
        cameras: Some(object_key_order(out)),
    })
}

// ---------------------------------------------------------------------------
// cameraSensors — RUST-ONLY (support-other-camera-models plan, Phase 2; the
// JS agent has no such namespace and ignores the key)
// ---------------------------------------------------------------------------

/// `cameraSensors`: which image sensor is attached to each camera port,
/// keyed by stream key. An ABSENT port key is UNMANAGED: the agent leaves
/// that port's camera line in config.txt exactly as it found it.
///
/// Each entry is kept AS DELIVERED (a JSON value), not normalised: the
/// agent has to be able to say "this entry was refused, and why" for a
/// sensor id or an option its compiled-in catalog lacks and for an entry
/// of the wrong type, and it must hand the value back unchanged in a local
/// fold-back (`changed.cameraSensors`). What an entry means is decided by
/// [`camera_sensor_choice`], and against the catalog by the reconciler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CameraSensorsSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cameras: Option<IndexMap<String, Value>>,
}

/// The most port keys `cameraSensors.cameras` holds (the backend's bound);
/// keys beyond it are dropped.
pub const MAX_CAMERA_SENSOR_PORTS: usize = 16;

/// The largest entry (its JSON text, in bytes) `cameraSensors.cameras`
/// holds as delivered.
pub const MAX_CAMERA_SENSOR_ENTRY_BYTES: usize = 512;

/// The `sensor` value of a port declared empty.
pub const CAMERA_SENSOR_NONE: &str = "none";

/// Forgiving like `parse_camera_mount_settings`: `{cameras: {}}` parses to
/// itself; no `cameras` object → None (the cloud's "revert this namespace"
/// marker). A key that is not a stream key is dropped; every other entry is
/// kept whatever it holds.
pub fn parse_camera_sensors_settings(v: &Value) -> Option<CameraSensorsSettings> {
    let Value::Object(src) = v else {
        return None;
    };
    let Some(Value::Object(cameras)) = src.get("cameras") else {
        return None;
    };
    let mut out: IndexMap<String, Value> = IndexMap::new();
    for (key, entry) in cameras {
        if !stream_key_re_test(key) || js_assign_drops_key(key) {
            continue;
        }
        // The backend's bound on ports; only a hand-edited file exceeds it.
        if out.len() >= MAX_CAMERA_SENSOR_PORTS {
            break;
        }
        // An entry is held as delivered, so its size is what the report
        // carries (twice, with a local fold-back), and the backend drops a
        // report over 32 KB. A real entry is some 60 bytes; one over the
        // bound (a hand-edited file) is replaced by a marker the reconciler
        // refuses, so nothing is written for the port.
        let bytes = serde_json::to_string(entry).map_or(0, |s| s.len());
        if bytes > MAX_CAMERA_SENSOR_ENTRY_BYTES {
            let mut marker = serde_json::Map::new();
            marker.insert("oversized".to_string(), Value::from(bytes));
            out.insert(key.clone(), Value::Object(marker));
            continue;
        }
        out.insert(key.clone(), entry.clone());
    }
    Some(CameraSensorsSettings {
        cameras: Some(object_key_order(out)),
    })
}

/// What one `cameraSensors.cameras.<key>` entry asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum CameraSensorChoice {
    /// `{"sensor": "none"}`: the port is declared empty.
    Empty,
    /// A sensor id (not checked against the catalog here) and its options
    /// in the entry's order.
    Sensor {
        id: String,
        options: Vec<(String, f64)>,
    },
}

/// Read one entry. `Err` is a sentence fragment that completes "the camera
/// sensor setting of <key> …" (the port is then refused, config.txt is left
/// alone). Unknown keys of the entry are ignored.
pub fn camera_sensor_choice(entry: &Value) -> Result<CameraSensorChoice, String> {
    let Value::Object(e) = entry else {
        return Err("is not an object".to_string());
    };
    let Some(Value::String(id)) = e.get("sensor") else {
        if let Some(bytes) = e.get("oversized").and_then(Value::as_u64) {
            return Err(format!(
                "is larger than the agent keeps ({bytes} bytes; at most {MAX_CAMERA_SENSOR_ENTRY_BYTES})"
            ));
        }
        return Err("has no \"sensor\" name".to_string());
    };
    let options = match e.get("options") {
        None => Vec::new(),
        Some(Value::Object(o)) => {
            let mut out = Vec::new();
            for (k, v) in o {
                let Some(n) = v.as_f64().filter(|n| n.is_finite()) else {
                    return Err(format!(
                        "has an option \"{}\" that is not a number",
                        clip(k)
                    ));
                };
                out.push((k.clone(), n));
            }
            out
        }
        Some(_) => return Err("has \"options\" that is not an object".to_string()),
    };
    if id == CAMERA_SENSOR_NONE {
        if !options.is_empty() {
            return Err("declares the port empty (\"none\") and still carries options".to_string());
        }
        return Ok(CameraSensorChoice::Empty);
    }
    Ok(CameraSensorChoice::Sensor {
        id: id.clone(),
        options,
    })
}

/// A name out of a setting, as it may appear in a sentence: at most 32
/// characters, and only `[A-Za-z0-9_.-]` (anything else becomes `?`).
pub fn clip(s: &str) -> String {
    s.chars()
        .take(32)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '?'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// simulation — `settings.ts:290-327`
// ---------------------------------------------------------------------------

/// `SimulationSettings` (`settings.ts:290`): cloud-delivered to simulated devices only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SimulationSettings {
    /// Catalog id of the scenario to replay (`demo-assets/v1/<scenarioId>/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario_id: Option<String>,
    /// Replay again after the parked gap (default true).
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "loop")]
    pub loop_: Option<bool>,
    /// Parked pause between laps, seconds 0..3600 (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_seconds: Option<u32>,
}

/// `parseSimulationSettings` (`settings.ts:312`).
pub fn parse_simulation_settings(v: &Value) -> Option<SimulationSettings> {
    let Value::Object(src) = v else {
        return None;
    };
    let out = SimulationSettings {
        scenario_id: string(src.get("scenarioId"))
            .filter(|s| scenario_id_re_test(s))
            .map(str::to_string),
        loop_: boolean(src.get("loop")),
        gap_seconds: num(src.get("gapSeconds"))
            .map(|g| clamp_round(g, SIMULATION_GAP_SECONDS_MIN, SIMULATION_GAP_SECONDS_MAX) as u32),
    };
    (out != SimulationSettings::default()).then_some(out)
}

// ---------------------------------------------------------------------------
// obd — `settings.ts:329-412` @ 09da38d
// ---------------------------------------------------------------------------

/// `OBD_POLL_HZ_MIN` (`settings.ts:375` @ 09da38d).
pub const OBD_POLL_HZ_MIN: f64 = 0.2;
/// `OBD_POLL_HZ_MAX` (`settings.ts:376` @ 09da38d).
pub const OBD_POLL_HZ_MAX: f64 = 2.0;
/// `OBD_MAC_RE` (`settings.ts:373`): `/^([0-9A-F]{2}:){5}[0-9A-F]{2}$/`.
pub const OBD_MAC_RE: &str = "^([0-9A-F]{2}:){5}[0-9A-F]{2}$";
/// `OBD_PIN_RE` (`settings.ts:374`): `/^[0-9]{4,6}$/`.
pub const OBD_PIN_RE: &str = "^[0-9]{4,6}$";
/// `OBD_FIELD_RE` (`settings.ts:378`): `/^[a-z_][a-z0-9_]*$/`.
pub const OBD_FIELD_RE: &str = "^[a-z_][a-z0-9_]*$";

/// `OBD_MAC_RE.test(s)` — uppercase hex pairs, colon-separated, 17 chars.
pub fn obd_mac_re_test(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 17
        && b.iter().enumerate().all(|(i, c)| {
            if i % 3 == 2 {
                *c == b':'
            } else {
                c.is_ascii_digit() || (b'A'..=b'F').contains(c)
            }
        })
}

/// `OBD_PIN_RE.test(s)` — 4 to 6 ASCII digits.
pub fn obd_pin_re_test(s: &str) -> bool {
    (4..=6).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_digit())
}

/// `OBD_FIELD_RE.test(s)` — a helper identifier.
pub fn obd_field_re_test(s: &str) -> bool {
    let mut it = s.bytes();
    match it.next() {
        Some(c) if c.is_ascii_lowercase() || c == b'_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

/// `ObdPowerSource` (`settings.ts:359`): ignition-switched 12 V (default)
/// or constant/hardwired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObdPowerSource {
    Switched,
    Constant,
}

impl ObdPowerSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ObdPowerSource::Switched => "switched",
            ObdPowerSource::Constant => "constant",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "switched" => Some(ObdPowerSource::Switched),
            "constant" => Some(ObdPowerSource::Constant),
            _ => None,
        }
    }
}

/// `ObdSettings` (`settings.ts:344-357` @ 09da38d): the `obd` namespace.
/// Every field optional; key order = `parseObdSettings`' insertion order
/// (enabled, adapterMac, pin, pollHz, fields, powerSource). `adapter_mac`
/// is the TS `string | null | undefined` triple: `Some(None)` = an explicit
/// "no adapter" (`null`), kept as `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ObdSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "nullable_present"
    )]
    pub adapter_mac: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub poll_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_source: Option<ObdPowerSource>,
}

/// A present JSON `null` → `Some(None)` (with `#[serde(default)]` an absent
/// key stays `None`).
fn nullable_present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `OBD_DEFAULTS` (`settings.ts:362-369` @ 09da38d) — the cloud contract's
/// device defaults (`Required<ObdSettings>`), pinned by `tests/config_contract.rs`.
pub fn obd_defaults() -> ObdSettings {
    ObdSettings {
        enabled: Some(false),
        adapter_mac: Some(None),
        pin: Some("1234".to_string()),
        poll_hz: Some(1.0),
        fields: Some(Vec::new()),
        power_source: Some(ObdPowerSource::Switched),
    }
}

/// The six namespace keys as the report and the contract spell them
/// (`OBD_NAMESPACE_KEYS = Object.keys(OBD_DEFAULTS)`, `config-sync.ts:999`).
pub const OBD_NAMESPACE_KEYS: [&str; 6] = [
    "enabled",
    "adapterMac",
    "pin",
    "pollHz",
    "fields",
    "powerSource",
];

impl ObdSettings {
    /// `{ ...OBD_DEFAULTS, ...self }` (`config-sync.ts:1043`): every field
    /// filled, the delivered ones winning.
    pub fn with_defaults(&self) -> ObdSettings {
        let d = obd_defaults();
        ObdSettings {
            enabled: self.enabled.or(d.enabled),
            adapter_mac: self.adapter_mac.clone().or(d.adapter_mac),
            pin: self.pin.clone().or(d.pin),
            poll_hz: self.poll_hz.or(d.poll_hz),
            fields: self.fields.clone().or(d.fields),
            power_source: self.power_source.or(d.power_source),
        }
    }
}

/// `parseObdSettings` (`settings.ts:388-412` @ 09da38d): forgiving on the
/// numbers (`pollHz` clamped to [0.2, 2]) and STRICT on the two strings
/// that reach bluetoothctl — a malformed MAC or PIN is DROPPED, never
/// repaired. `adapterMac: null` is kept. Arrays are refused outright (the
/// TS checks `Array.isArray`). `None` when nothing valid remains.
pub fn parse_obd_settings(v: &Value) -> Option<ObdSettings> {
    let Value::Object(src) = v else {
        return None;
    };
    let mut out = ObdSettings {
        enabled: boolean(src.get("enabled")),
        ..ObdSettings::default()
    };
    match src.get("adapterMac") {
        Some(Value::Null) => out.adapter_mac = Some(None),
        Some(Value::String(raw)) => {
            let mac = crate::js::trim(raw).to_uppercase();
            if obd_mac_re_test(&mac) {
                out.adapter_mac = Some(Some(mac));
            }
        }
        _ => {}
    }
    out.pin = string(src.get("pin"))
        .filter(|p| obd_pin_re_test(p))
        .map(str::to_string);
    out.poll_hz = num(src.get("pollHz")).map(|h| clamp(h, OBD_POLL_HZ_MIN, OBD_POLL_HZ_MAX));
    if let Some(Value::Array(list)) = src.get("fields") {
        let mut fields: Vec<String> = Vec::new();
        for f in list {
            if let Value::String(f) = f {
                if obd_field_re_test(f) && !fields.contains(f) {
                    fields.push(f.clone());
                }
            }
        }
        out.fields = Some(fields);
    }
    out.power_source = string(src.get("powerSource")).and_then(ObdPowerSource::parse);
    (out != ObdSettings::default()).then_some(out)
}

// ---------------------------------------------------------------------------
// AgentSettings — `settings.ts:329-392`, allowlist `:616-637`
// ---------------------------------------------------------------------------

/// `AgentSettings` (`settings.ts:329`): the whole settings.json document.
/// Key order = `parseSettingsObject` insertion order (`:618-635`), which the
/// TS interface also follows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RunMode>,
    /// GNSS antenna bias: true powers an active antenna's LNA over the coax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gnss_ant_bias: Option<bool>,
    /// IMU mounting correction; absent = publish sensor-frame euler as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imu_mount: Option<ImuMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte_recovery: Option<LteRecoverySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_sync: Option<TimeSyncSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cellular: Option<CellularSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_mount: Option<CameraMountSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation: Option<SimulationSettings>,
    /// Vehicle OBD-II adapter (`settings.ts:477-482` @ 09da38d).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obd: Option<ObdSettings>,
    /// Camera sensor per port (RUST-ONLY, after every JS key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_sensors: Option<CameraSensorsSettings>,
}

/// `parseSettingsObject` (`settings.ts:616`): the allowlist rewriter — every
/// key this agent knows, and nothing else (the retired `mediaEnabled` is
/// dropped like any unknown key; see [`crate::settings_migrate`]).
pub fn parse_settings_object(obj: &serde_json::Map<String, Value>) -> AgentSettings {
    let get = |k: &str| obj.get(k).unwrap_or(&Value::Null);
    AgentSettings {
        mode: string(obj.get("mode")).and_then(RunMode::parse),
        gnss_ant_bias: boolean(obj.get("gnssAntBias")),
        imu_mount: parse_imu_mount(get("imuMount")),
        recording: parse_recording_settings(get("recording")),
        lte_recovery: parse_lte_recovery_settings(get("lteRecovery")),
        time_sync: parse_time_sync_settings(get("timeSync")),
        cellular: parse_cellular_settings(get("cellular")),
        camera_mount: parse_camera_mount_settings(get("cameraMount")),
        simulation: parse_simulation_settings(get("simulation")),
        obd: parse_obd_settings(get("obd")),
        camera_sensors: parse_camera_sensors_settings(get("cameraSensors")),
    }
}

/// `SettingsReadFailure` (`settings.ts:579`): why a settings read produced
/// nothing. `loadSettings` collapses all of them into `{}`; the config-sync
/// watcher and `applyCloudConfig` refuse to act on `Unreadable` / `Invalid`
/// (an unreadable file is NOT "the operator deleted every namespace").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingsReadFailure {
    Absent,
    Unreadable,
    Invalid,
}

impl SettingsReadFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            SettingsReadFailure::Absent => "absent",
            SettingsReadFailure::Unreadable => "unreadable",
            SettingsReadFailure::Invalid => "invalid",
        }
    }
}

/// `SettingsReadResult` (`settings.ts:581`).
pub type SettingsReadResult = Result<AgentSettings, SettingsReadFailure>;

/// What the HAL's `readFileSync` produced, before any parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRead<'a> {
    /// ENOENT.
    Absent,
    /// Any other read error (EACCES after `sudo xorgate set …`, EISDIR, …).
    Unreadable,
    /// The file's text.
    Text(&'a str),
}

/// The decision inside `readSettingsFile` (`settings.ts:589-607`) over an
/// already-performed read: absent / unreadable pass through; text that is
/// not JSON, or JSON that is not a plain object (`null`, arrays, scalars),
/// is `Invalid`; otherwise the allowlisted document.
pub fn settings_from_read(read: SettingsRead<'_>) -> SettingsReadResult {
    match read {
        SettingsRead::Absent => Err(SettingsReadFailure::Absent),
        SettingsRead::Unreadable => Err(SettingsReadFailure::Unreadable),
        SettingsRead::Text(text) => parse_settings_text(text),
    }
}

/// The `JSON.parse` + plain-object gate of `readSettingsFile` (`:597-606`).
pub fn parse_settings_text(text: &str) -> SettingsReadResult {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(obj)) => Ok(parse_settings_object(&obj)),
        _ => Err(SettingsReadFailure::Invalid),
    }
}

/// `loadSettings` (`settings.ts:610`) over a performed read: `{}` on any failure.
pub fn load_settings_from(read: SettingsRead<'_>) -> AgentSettings {
    settings_from_read(read).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_json_is_stricter_than_json_parse() {
        // ACCEPTED (review-wp1 #7, evidence/accepted-divergences.md): JSON.parse
        // reads `1e400` as Infinity (the leaf is then dropped by `isFinite`);
        // serde_json rejects the whole text → `invalid` (both refuse to write).
        assert_eq!(
            parse_settings_text(r#"{"recording":{"retention":{"maxBytes":1e400}}}"#),
            Err(SettingsReadFailure::Invalid)
        );
    }

    #[test]
    fn regex_ports() {
        assert!(apn_re_test(""));
        assert!(apn_re_test("sp.mb.com"));
        assert!(!apn_re_test("a b"));
        assert!(!apn_re_test(&"a".repeat(101)));
        assert!(apn_re_test(&"a".repeat(100)));
        assert!(!apn_re_test("sp.mb.com\n"));
        assert!(scenario_id_re_test("demo-01"));
        assert!(!scenario_id_re_test("-demo"));
        assert!(!scenario_id_re_test("Demo"));
        assert!(!scenario_id_re_test(""));
        assert!(stream_key_re_test("cam0"));
        assert!(stream_key_re_test("a_b-C9"));
        assert!(!stream_key_re_test(""));
        assert!(!stream_key_re_test("bad key!"));
        assert!(!stream_key_re_test(&"x".repeat(33)));
    }

    #[test]
    fn js_key_order_puts_array_indices_first() {
        let m: IndexMap<String, u8> = [
            ("cam0", 1),
            ("10", 2),
            ("2", 3),
            ("01", 4),
            ("4294967295", 5),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let ordered = object_key_order(m);
        let keys: Vec<&str> = ordered.keys().map(String::as_str).collect();
        assert_eq!(keys, ["2", "10", "cam0", "01", "4294967295"]);
    }

    #[test]
    fn read_classification() {
        assert_eq!(
            settings_from_read(SettingsRead::Absent),
            Err(SettingsReadFailure::Absent)
        );
        assert_eq!(
            settings_from_read(SettingsRead::Unreadable),
            Err(SettingsReadFailure::Unreadable)
        );
        assert_eq!(
            settings_from_read(SettingsRead::Text("not json")),
            Err(SettingsReadFailure::Invalid)
        );
        assert_eq!(
            settings_from_read(SettingsRead::Text("[]")),
            Err(SettingsReadFailure::Invalid)
        );
        assert_eq!(
            settings_from_read(SettingsRead::Text("{\"mode\":\"bogus\"}")),
            Ok(AgentSettings::default())
        );
        assert_eq!(
            load_settings_from(SettingsRead::Unreadable),
            AgentSettings::default()
        );
    }

    #[test]
    fn time_sync_resolve() {
        assert_eq!(resolve_time_sync_config(None), TIME_SYNC_DEFAULTS);
        let s = TimeSyncSettings {
            offline_sources: Some(false),
            ..Default::default()
        };
        let r = resolve_time_sync_config(Some(&s));
        assert!(!r.offline_sources);
        assert_eq!(r.step_threshold_secs, 10);
        assert_eq!(
            serde_json::to_string(&TIME_SYNC_DEFAULTS).unwrap(),
            r#"{"offlineSources":true,"stepThresholdSecs":10,"maxStepsPerBoot":4}"#
        );
    }
}
