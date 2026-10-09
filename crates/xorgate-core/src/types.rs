//! Shared types referenced across core modules (inventory §2 "shared-types"):
//! owned here so the WP2–WP6 modules have no inbound edges on each other.
//!
//! Every struct that reaches a file or the wire keeps the TS key order and
//! optional-field omission (plan Phase 1 rule 5).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::telemetry::{js_num, js_num_opt};

// ---------------------------------------------------------------------------
// sky_state — `src/lib/sky-state.ts:25-35, 109-112`
// ---------------------------------------------------------------------------

/// `SkyState` (`sky-state.ts:25`): the hysteresis classifier's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkyState {
    Clear,
    Obstructed,
    NoRf,
    Unknown,
}

impl SkyState {
    pub fn as_str(self) -> &'static str {
        match self {
            SkyState::Clear => "clear",
            SkyState::Obstructed => "obstructed",
            SkyState::NoRf => "no-rf",
            SkyState::Unknown => "unknown",
        }
    }
}

/// `GnssView` (`sky-state.ts:109`): what the Phase 7 video gate and the LED
/// ladder see of the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GnssView {
    pub fix: bool,
    pub sky: SkyState,
}

// ---------------------------------------------------------------------------
// boot_policy — `src/recording/boot-policy.ts:421-485`
// ---------------------------------------------------------------------------

/// `ShedConfig` (`boot-policy.ts:421`): the resolved `recording.powerShedding`
/// knobs. Key order = the TS interface (= `RECORDING_DEFAULTS.powerShedding`
/// literal order in `recording/config.ts:64-81`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShedConfig {
    pub enabled: bool,
    /// Shed when this many undervoltage samples land within uvWindowSecs.
    pub uv_samples: u32,
    pub uv_window_secs: u32,
    /// ... OR the rail stays below this for railLowSecs straight.
    #[serde(with = "js_num")]
    pub rail_low_volts: f64,
    pub rail_low_secs: u32,
    /// Trailing window evaluated for escalation to the next rung.
    pub escalate_dwell_secs: u32,
    /// Escalate when the supply was bad for at least this % of the dwell window.
    pub escalate_duty_pct: u32,
    /// ... OR the SoC runs at/above this for cpuTempSecs straight (°C).
    #[serde(with = "js_num")]
    pub cpu_temp_c: f64,
    /// Restore needs the SoC at/below this — the anti-flap margin (°C).
    #[serde(with = "js_num")]
    pub cpu_temp_restore_c: f64,
    pub cpu_temp_secs: u32,
    /// Clean interval (no UV, rail at/above railRestoreVolts) per restored rung.
    pub restore_secs: u32,
    #[serde(with = "js_num")]
    pub rail_restore_volts: f64,
    /// Shed cycles per rolling hour before parking at the shed level.
    pub max_sheds_per_hour: u32,
}

impl ShedConfig {
    /// `RECORDING_DEFAULTS.powerShedding` (`recording/config.ts:64-81`) — the
    /// Q4 starting values (2026-07-28) + the thermal arm (power_management
    /// Phase 1). Tuned from the field via settings, never here.
    pub const DEFAULTS: ShedConfig = ShedConfig {
        enabled: true,
        uv_samples: 3,
        uv_window_secs: 30,
        rail_low_volts: 4.85,
        rail_low_secs: 15,
        escalate_dwell_secs: 60,
        escalate_duty_pct: 70,
        cpu_temp_c: 80.0,
        cpu_temp_restore_c: 70.0,
        cpu_temp_secs: 15,
        restore_secs: 120,
        rail_restore_volts: 4.95,
        max_sheds_per_hour: 4,
    };
}

impl Default for ShedConfig {
    fn default() -> Self {
        Self::DEFAULTS
    }
}

/// `PowerSampleReading` (`boot-policy.ts:470`): one 1 Hz power sample as the
/// system collector hands it to the shed policy
/// (`collectors/index.ts:44-64 powerReadingFromFields`, ported by WP2).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerSampleReading {
    /// CURRENT undervoltage bit (0x1) of the throttle mask.
    pub uv: bool,
    /// EXT5V rail voltage; explicit `null` when the PMIC ADC is unavailable (Pi 4).
    #[serde(with = "js_num_opt")]
    pub rail_v: Option<f64>,
    /// SoC temperature in °C; null/absent when /sys/class/thermal is unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub temp_c: Option<f64>,
    /// CURRENT soft-temperature-limit bit (0x8) of the throttle mask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_temp: Option<bool>,
}

// ---------------------------------------------------------------------------
// session — `src/recording/session.ts:67, 76`
// ---------------------------------------------------------------------------

/// `TimeSource` (`session.ts:67`): provenance rank unsynced < rtc < nitz < gps < ntp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeSource {
    Ntp,
    Gps,
    Nitz,
    Rtc,
    Unsynced,
}

impl TimeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TimeSource::Ntp => "ntp",
            TimeSource::Gps => "gps",
            TimeSource::Nitz => "nitz",
            TimeSource::Rtc => "rtc",
            TimeSource::Unsynced => "unsynced",
        }
    }
}

/// `SteppedClockSource` (`session.ts:76`): where an offline clock step got its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SteppedClockSource {
    Gps,
    Nitz,
}

impl From<SteppedClockSource> for TimeSource {
    fn from(s: SteppedClockSource) -> Self {
        match s {
            SteppedClockSource::Gps => TimeSource::Gps,
            SteppedClockSource::Nitz => TimeSource::Nitz,
        }
    }
}

// ---------------------------------------------------------------------------
// gps collector — `src/agent/collectors/gps.ts:62-69`
// ---------------------------------------------------------------------------

/// `GpsClockReading` (`gps.ts:62`): the latest RMC UTC time for the clock stepper.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpsClockReading {
    /// UTC epoch ms as reported by RMC, at the instant it arrived.
    pub utc_ms: i64,
    /// Monotonic age of that reading, ms.
    pub age_ms: u64,
    /// RMC status was "A" (valid fix) on the sentence that carried this time.
    pub valid: bool,
}

// ---------------------------------------------------------------------------
// boot-config — `src/hardware/boot-config.ts:226-253`
// ---------------------------------------------------------------------------

/// `CameraRotationDeg` (`daemon/settings.ts:253`): the only values the camera
/// overlay's `rotation` parameter accepts. Serialises as the number 0 / 180.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraRotationDeg {
    Deg0,
    Deg180,
}

impl CameraRotationDeg {
    pub fn degrees(self) -> u32 {
        match self {
            CameraRotationDeg::Deg0 => 0,
            CameraRotationDeg::Deg180 => 180,
        }
    }
    /// `deg !== 0 && deg !== 180` → None (`settings.ts:275`); 180.0 is 180.
    pub fn from_f64(v: f64) -> Option<Self> {
        if v == 0.0 {
            Some(CameraRotationDeg::Deg0)
        } else if v == 180.0 {
            Some(CameraRotationDeg::Deg180)
        } else {
            None
        }
    }
}

impl Serialize for CameraRotationDeg {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(self.degrees())
    }
}

impl<'de> Deserialize<'de> for CameraRotationDeg {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = f64::deserialize(d)?;
        CameraRotationDeg::from_f64(v).ok_or_else(|| {
            serde::de::Error::custom(format!("rotationDeg must be 0 or 180, got {v}"))
        })
    }
}

/// `CameraMountVerdict` (`boot-config.ts:216-231`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraMountVerdict {
    /// In force right now: the running kernel booted with the desired rotation.
    Applied,
    /// config.txt now holds the desired rotation; it applies on the next boot.
    PendingReboot,
    /// config.txt holds it, but the rotation in force could not be read back.
    Unconfirmed,
    /// The device has no camera by that stream key.
    UnknownCamera,
    /// No `dtoverlay=` line configures that camera — nothing to edit.
    NoOverlay,
    /// That sensor's overlay has no `rotation` parameter (or the probe failed).
    Unsupported,
    /// The privileged write did not happen (helper/sudoers missing, or it failed).
    WriteFailed,
}

/// `CameraMountCameraState` (`boot-config.ts:233`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraMountCameraState {
    pub stream_key: String,
    pub desired_deg: CameraRotationDeg,
    /// Rotation the running kernel booted with; null = could not read it.
    #[serde(with = "js_num_opt")]
    pub in_force_deg: Option<f64>,
    /// Rotation config.txt now holds; null = no line (or lines disagree).
    #[serde(with = "js_num_opt")]
    pub boot_deg: Option<f64>,
    pub verdict: CameraMountVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `CameraMountState` (`boot-config.ts:246`): the reconciler's last pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraMountState {
    pub at: i64,
    pub cameras: Vec<CameraMountCameraState>,
    /// Any camera whose desired rotation is written but not yet in force.
    pub reboot_required: bool,
    /// True when the boot config changed on THIS pass (the write actually ran).
    pub wrote: bool,
}

// ---------------------------------------------------------------------------
// modem telemetry — `src/hardware/modem.ts:21-196` (the daemon's telemetry.json)
// ---------------------------------------------------------------------------
//
// The agent only READS this file. `readModemTelemetry` is a bare `JSON.parse`
// with no validation (`modem.ts:203-212`), and each consumer `typeof`-checks
// the fields it uses one at a time (`clock-step.ts:144-146`,
// `collectors/lte.ts:43-57`, `config-sync.ts:513-525`, `led-state.ts:183`).
// One wrong-typed or null field must therefore cost THAT field only, never the
// snapshot. So these structs are built by hand from a `serde_json::Value`
// ([`ModemTelemetry::from_json`] / [`ModemTelemetry::from_value`]), field by
// field, instead of by a derived `Deserialize` (which fails as a whole):
//
// - a string field is `Some` only for a JSON string (`typeof x === "string"`);
// - a number field is `Some` for ANY JSON number, as `f64`
//   (`typeof x === "number"`, so `1.0` is `1` and `1.5` stays `1.5`);
// - a nested block is `Some` only for a JSON object, and the same rules apply
//   inside it, so `{"ts": null, "path": "ladder"}` keeps `path`;
// - null, a missing key or any other type is `None` for that field alone;
// - unknown keys are ignored, and enum-valued fields stay `String` (an
//   unknown label is classified downstream: [`GnssHealth::parse`], WP4's
//   `parse*Telemetry`).
//
// `Deserialize` is implemented through the same builder, so a struct nested
// in a fixture or a status document reads identically. On output a `None` is
// written as `null` where the TS type is `X | null` without `?`, and omitted
// otherwise (a TS-required field is only `None` on malformed input).

/// `ModemPresence` (`modem.ts:21`): lsusb + tty enumeration for the monitor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModemPresence {
    pub present: bool,
    /// USB vendor:product from lsusb, e.g. "1e0e:9011".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Enumerated /dev/ttyUSB* nodes.
    pub tty_ports: Vec<String>,
}

/// `GnssHealth` (`modem.ts:34`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GnssHealth {
    Ok,
    Acquiring,
    Degraded,
    Unhealthy,
}

impl GnssHealth {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(GnssHealth::Ok),
            "acquiring" => Some(GnssHealth::Acquiring),
            "degraded" => Some(GnssHealth::Degraded),
            "unhealthy" => Some(GnssHealth::Unhealthy),
            _ => None,
        }
    }
    /// `GNSS_HEALTH_CODES` (`collectors/lte.ts:10`): 0 ok · 1 acquiring · 2 degraded · 3 unhealthy.
    pub fn code(self) -> u8 {
        match self {
            GnssHealth::Ok => 0,
            GnssHealth::Acquiring => 1,
            GnssHealth::Degraded => 2,
            GnssHealth::Unhealthy => 3,
        }
    }
}

/// A JSON object (`typeof v === "object"`, not null, not an array).
type JsonMap = Map<String, Value>;

fn field_str(o: &JsonMap, k: &str) -> Option<String> {
    o.get(k)?.as_str().map(str::to_owned)
}

fn field_num(o: &JsonMap, k: &str) -> Option<f64> {
    o.get(k)?.as_f64()
}

fn field_obj<'a>(o: &'a JsonMap, k: &str) -> Option<&'a JsonMap> {
    o.get(k)?.as_object()
}

/// `Deserialize` for the hand-built modem structs: decode a `Value`, require
/// an object, then run the lenient `from_map` builder.
macro_rules! deserialize_via_from_map {
    ($($t:ident),* $(,)?) => {$(
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let v = Value::deserialize(d)?;
                v.as_object().map($t::from_map).ok_or_else(|| {
                    serde::de::Error::custom(concat!(stringify!($t), ": expected a JSON object"))
                })
            }
        }
    )*};
}

/// `last_engine_restart` (`modem.ts:49`). `ts` is `null` when the daemon's
/// epoch was 0/empty (`iso_or_null`, `sim7600-lte.sh:311-318,509`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct GnssEngineRestart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    /// "off" | "wedge" | "post-reset".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl GnssEngineRestart {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            ts: field_str(o, "ts"),
            reason: field_str(o, "reason"),
        }
    }
}

/// `ModemGnssTelemetry` (`modem.ts:42`): all LAST-KNOWLEDGE values.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ModemGnssTelemetry {
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub engine_on: Option<f64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub has_fix: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_checked_at: Option<String>,
    /// Coarse daemon-side TTFF (engine start → first fix-probe success), secs.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub ttff_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_engine_start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_engine_restart: Option<GnssEngineRestart>,
    /// 1 while the engine runs the plain-standalone fallback (SUPL start failed).
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub standalone_fallback: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_fix_since: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub wedge_restarts_1h: Option<f64>,
    /// A `GnssHealth` label, or anything the daemon wrote (validated downstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
}

impl ModemGnssTelemetry {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            engine_on: field_num(o, "engine_on"),
            has_fix: field_num(o, "has_fix"),
            fix_checked_at: field_str(o, "fix_checked_at"),
            ttff_s: field_num(o, "ttff_s"),
            last_engine_start: field_str(o, "last_engine_start"),
            last_engine_restart: field_obj(o, "last_engine_restart")
                .map(GnssEngineRestart::from_map),
            standalone_fallback: field_num(o, "standalone_fallback"),
            no_fix_since: field_str(o, "no_fix_since"),
            wedge_restarts_1h: field_num(o, "wedge_restarts_1h"),
            health: field_str(o, "health"),
        }
    }
}

/// `last_reset` (`modem.ts:62`). `ts` may be `null` (`sim7600-lte.sh:513`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ModemLastReset {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    /// "ladder" | "deadzone" | "config-drift".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl ModemLastReset {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            ts: field_str(o, "ts"),
            path: field_str(o, "path"),
        }
    }
}

/// `last_gate_skip` (`modem.ts:63`). `ts` may be `null` (`sim7600-lte.sh:517`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ModemGateSkip {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ModemGateSkip {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            ts: field_str(o, "ts"),
            context: field_str(o, "context"),
            reason: field_str(o, "reason"),
        }
    }
}

/// `ModemRecoveryTelemetry` (`modem.ts:58`).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ModemRecoveryTelemetry {
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub connect_fails: Option<f64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub radio_cycles_since_ok: Option<f64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub resets_24h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reset: Option<ModemLastReset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_gate_skip: Option<ModemGateSkip>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_lte_ok: Option<String>,
}

impl ModemRecoveryTelemetry {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            connect_fails: field_num(o, "connect_fails"),
            radio_cycles_since_ok: field_num(o, "radio_cycles_since_ok"),
            resets_24h: field_num(o, "resets_24h"),
            last_reset: field_obj(o, "last_reset").map(ModemLastReset::from_map),
            last_gate_skip: field_obj(o, "last_gate_skip").map(ModemGateSkip::from_map),
            last_lte_ok: field_str(o, "last_lte_ok"),
        }
    }
}

/// `SimState` (`modem.ts:68`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SimState {
    Ready,
    Locked,
    NotDetected,
    Error,
    Unknown,
}

impl SimState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ready" => Some(SimState::Ready),
            "locked" => Some(SimState::Locked),
            "not-detected" => Some(SimState::NotDetected),
            "error" => Some(SimState::Error),
            "unknown" => Some(SimState::Unknown),
            _ => None,
        }
    }
}

/// `SimRegistration` (`modem.ts:71`): 3GPP <stat> as a label the daemon resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SimRegistration {
    Home,
    Roaming,
    Searching,
    /// The network REJECTED this SIM — a deprovisioned or unpaid account.
    Denied,
    NotSearching,
    Unknown,
}

impl SimRegistration {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "home" => Some(SimRegistration::Home),
            "roaming" => Some(SimRegistration::Roaming),
            "searching" => Some(SimRegistration::Searching),
            "denied" => Some(SimRegistration::Denied),
            "not-searching" => Some(SimRegistration::NotSearching),
            "unknown" => Some(SimRegistration::Unknown),
            _ => None,
        }
    }
}

/// `ModemSimTelemetry` (`modem.ts:92`): ABSENT until the first probe reaches
/// the modem. WP4's `parseSimTelemetry` port sanitises the RAW block (it also
/// accepts `present: true`), so this typed view is informational.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ModemSimTelemetry {
    /// 1 present, 0 the modem says no card is inserted, null we cannot tell.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub present: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iccid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msisdn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub act: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
}

impl ModemSimTelemetry {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            present: field_num(o, "present"),
            state: field_str(o, "state"),
            iccid: field_str(o, "iccid"),
            msisdn: field_str(o, "msisdn"),
            carrier: field_str(o, "carrier"),
            act: field_str(o, "act"),
            registration: field_str(o, "registration"),
            checked_at: field_str(o, "checked_at"),
        }
    }
}

/// `ApnState` (`modem.ts:116`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApnState {
    InForce,
    Applying,
    RolledBack,
    Rejected,
}

impl ApnState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "in-force" => Some(ApnState::InForce),
            "applying" => Some(ApnState::Applying),
            "rolled-back" => Some(ApnState::RolledBack),
            "rejected" => Some(ApnState::Rejected),
            _ => None,
        }
    }
}

/// `ModemCellularTelemetry` (`modem.ts:127`): ABSENT = no rollback safety net.
/// WP4's `parseApnTelemetry` port sanitises the RAW block.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ModemCellularTelemetry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The APN in the modem's data context. Absent while never read / NV empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apn_applied: Option<String>,
    /// Last non-empty desired value seen. Absent when nothing was ever asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apn_requested: Option<String>,
    /// Why the last apply rolled back / was rejected. Absent after a clean apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Failed applies of apn_requested — capped, keyed on the value.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub fails: Option<f64>,
    /// ISO ts of the last state transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
}

impl ModemCellularTelemetry {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            state: field_str(o, "state"),
            apn_applied: field_str(o, "apn_applied"),
            apn_requested: field_str(o, "apn_requested"),
            last_error: field_str(o, "last_error"),
            fails: field_num(o, "fails"),
            changed_at: field_str(o, "changed_at"),
        }
    }
}

/// `ModemUplinkTelemetry` (`modem.ts:146`): which interface carries the
/// default route. WP4's `parseUplinkTelemetry` port sanitises the RAW block.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ModemUplinkTelemetry {
    /// Interface from the kernel's own route lookup, or null when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    /// 1 when the modem interface is the ONLY default route.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub modem_only: Option<f64>,
}

impl ModemUplinkTelemetry {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            primary: field_str(o, "primary"),
            modem_only: field_num(o, "modem_only"),
        }
    }
}

/// `modem_clock` (`modem.ts:185`): raw AT+CCLK? + the daemon-clock epoch
/// seconds at the instant it was read, + registration <stat> at that instant.
/// Each field is independent: [`crate::clock_step::read_modem_clock`] needs
/// `cclk` and `read_at`, and falls back to the top-level `reg` when this
/// `reg` is not a number (`clock-step.ts:144-145`).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ModemClock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cclk: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub read_at: Option<f64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub reg: Option<f64>,
}

impl ModemClock {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            cclk: field_str(o, "cclk"),
            read_at: field_num(o, "read_at"),
            reg: field_num(o, "reg"),
        }
    }
}

/// `ModemTelemetry` (`modem.ts:153`): the daemon's atomic telemetry snapshot
/// (`write_telemetry`, `sim7600-lte.sh:473-621`). Every field is independent;
/// see the section comment for the per-field typing rules.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ModemTelemetry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// AT+CSQ value 0–31, or 99 when unknown. Doubles as ASU.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub csq: Option<f64>,
    /// Derived dBm (-113 + csq*2), or null when csq is unknown.
    #[serde(serialize_with = "js_num_opt::serialize")]
    pub rssi_dbm: Option<f64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub csq_interval: Option<f64>,
    /// "connected" | "connecting" | "disconnected" | "dead_zone".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Last seen 3GPP registration <stat>: 1 home, 5 roaming, 2 searching,
    /// 3 denied, 0 none, 99 unknown.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub reg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iface: Option<String>,
    pub ip: Option<String>,
    /// Minutes of XTRA assistance validity left (written by the daemon,
    /// `sim7600-lte.sh:587`; no agent consumer).
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub xtra_valid_min: Option<f64>,
    /// The APPLIED antenna-bias state, RAW (`sim7600-lte.sh:588`): the daemon
    /// writes 1/0/null, and `readModemBias` (`config-sync.ts:513-514`) maps
    /// `1 | true` → true, `0 | false` → false, anything else → null. Kept as
    /// the raw value so WP4's port applies that rule itself.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::js::json_value_opt::serialize"
    )]
    pub gnss_ant_bias: Option<Value>,
    /// Modem PMU temperature in °C (AT+CPMUTEMP), or null/absent when never read.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_num_opt::serialize"
    )]
    pub modem_temp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modem_clock: Option<ModemClock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gnss: Option<ModemGnssTelemetry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ModemRecoveryTelemetry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sim: Option<ModemSimTelemetry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cellular: Option<ModemCellularTelemetry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uplink: Option<ModemUplinkTelemetry>,
}

impl ModemTelemetry {
    pub fn from_map(o: &JsonMap) -> Self {
        Self {
            timestamp: field_str(o, "timestamp"),
            csq: field_num(o, "csq"),
            rssi_dbm: field_num(o, "rssi_dbm"),
            csq_interval: field_num(o, "csq_interval"),
            status: field_str(o, "status"),
            reg: field_num(o, "reg"),
            iface: field_str(o, "iface"),
            ip: field_str(o, "ip"),
            xtra_valid_min: field_num(o, "xtra_valid_min"),
            // `null` is a real daemon value (never applied); keep it raw.
            gnss_ant_bias: o.get("gnss_ant_bias").cloned(),
            modem_temp: field_num(o, "modem_temp"),
            modem_clock: field_obj(o, "modem_clock").map(ModemClock::from_map),
            gnss: field_obj(o, "gnss").map(ModemGnssTelemetry::from_map),
            recovery: field_obj(o, "recovery").map(ModemRecoveryTelemetry::from_map),
            sim: field_obj(o, "sim").map(ModemSimTelemetry::from_map),
            cellular: field_obj(o, "cellular").map(ModemCellularTelemetry::from_map),
            uplink: field_obj(o, "uplink").map(ModemUplinkTelemetry::from_map),
        }
    }

    /// The snapshot as `readModemTelemetry` hands it on, from an already
    /// `JSON.parse`d value. `None` exactly when the TS value is falsy (`null`,
    /// `false`, `0`, `""`): the case every consumer treats as "no telemetry"
    /// (`if (!t)`, `lte.ts:65`). Any other non-object (a number, a string, an
    /// array, `true`) is a snapshot whose every field is undefined, which is
    /// what a field lookup on it yields in JS.
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Null | Value::Bool(false) => None,
            Value::Number(n) if n.as_f64() == Some(0.0) => None,
            Value::String(s) if s.is_empty() => None,
            Value::Object(o) => Some(Self::from_map(o)),
            _ => Some(Self::default()),
        }
    }

    /// `readModemTelemetry`'s parse half (`modem.ts:207-208`) over the file
    /// text the HAL read: unparseable JSON is `None`, like the TS `catch`.
    pub fn from_json(raw: &str) -> Option<Self> {
        Self::from_value(&serde_json::from_str::<Value>(raw).ok()?)
    }
}

deserialize_via_from_map!(
    GnssEngineRestart,
    ModemGnssTelemetry,
    ModemLastReset,
    ModemGateSkip,
    ModemRecoveryTelemetry,
    ModemSimTelemetry,
    ModemCellularTelemetry,
    ModemUplinkTelemetry,
    ModemClock,
    ModemTelemetry,
);
#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        json: &str,
    ) -> String {
        let v: T = serde_json::from_str(json).unwrap_or_else(|e| panic!("{json}: {e}"));
        let out = serde_json::to_string(&v).unwrap();
        let back: T = serde_json::from_str(&out).unwrap();
        assert_eq!(back, v);
        out
    }

    #[test]
    fn modem_telemetry_sample_from_modem_test_ts() {
        // test/modem.test.ts:7-15 SAMPLE
        let s = r#"{"timestamp":"2026-06-13T08:21:45Z","csq":18,"rssi_dbm":-77,"csq_interval":30,"status":"connected","iface":"usb0","ip":"10.164.20.3"}"#;
        assert_eq!(round_trip::<ModemTelemetry>(s), s);
    }

    #[test]
    fn modem_telemetry_full_daemon_snapshot() {
        // Shape of `write_telemetry` in modem/sim7600-lte.sh:473-600 (every
        // block present), normalised to JSON.stringify spacing.
        let s = r#"{"timestamp":"2026-08-09T18:00:00Z","csq":99,"rssi_dbm":null,"csq_interval":30,"status":"dead_zone","reg":3,"iface":"usb0","ip":null,"modem_temp":41,"modem_clock":{"cclk":"26/08/09,18:00:00+32","read_at":1786384800,"reg":1},"gnss":{"engine_on":1,"has_fix":0,"fix_checked_at":"2026-08-09T17:50:00Z","ttff_s":null,"last_engine_start":"2026-08-09T17:00:00Z","last_engine_restart":{"ts":"2026-08-09T17:00:00Z","reason":"wedge"},"standalone_fallback":0,"no_fix_since":null,"wedge_restarts_1h":1,"health":"acquiring"},"recovery":{"connect_fails":2,"radio_cycles_since_ok":1,"resets_24h":1,"last_reset":{"ts":"2026-08-09T16:00:00Z","path":"ladder"},"last_gate_skip":{"ts":"2026-08-09T16:30:00Z","context":"deadzone","reason":"gps-fix"},"last_lte_ok":"2026-08-09T15:00:00Z"},"sim":{"present":1,"state":"ready","iccid":"8912","msisdn":"+1555","carrier":"Public Mobile","act":"4G","registration":"denied","checked_at":"2026-08-09T17:59:00Z"},"cellular":{"state":"rolled-back","apn_applied":"sp.mb.com","apn_requested":"wrong.apn","last_error":"no data session within 300s (ping and tcp both failed)","fails":2,"changed_at":"2026-08-09T18:00:00Z"},"uplink":{"primary":"usb0","modem_only":1}}"#;
        let out = round_trip::<ModemTelemetry>(s);
        // Explicit nulls on non-optional-but-nullable fields (`rssi_dbm`, `ip`)
        // survive; `ttff_s: null` / `no_fix_since: null` collapse to absent on
        // re-serialisation (documented in the module comment).
        let expected = s
            .replace(r#""ttff_s":null,"#, "")
            .replace(r#""no_fix_since":null,"#, "");
        assert_eq!(out, expected);
        let t: ModemTelemetry = serde_json::from_str(s).unwrap();
        assert_eq!(
            GnssHealth::parse(t.gnss.as_ref().unwrap().health.as_deref().unwrap()),
            Some(GnssHealth::Acquiring)
        );
        assert_eq!(SimState::parse("not-detected"), Some(SimState::NotDetected));
        assert_eq!(
            SimRegistration::parse("not-searching"),
            Some(SimRegistration::NotSearching)
        );
        assert_eq!(ApnState::parse("in-force"), Some(ApnState::InForce));
        assert_eq!(ApnState::parse("confused"), None);
    }

    #[test]
    fn modem_telemetry_tolerates_unknown_labels() {
        let s = r#"{"timestamp":"t","csq":5,"rssi_dbm":-103,"csq_interval":30,"status":"weird","iface":"usb0","ip":null,"sim":{"state":"martian"},"cellular":{"state":"confused"}}"#;
        let t: ModemTelemetry = serde_json::from_str(s).unwrap();
        assert_eq!(t.sim.unwrap().state.as_deref(), Some("martian"));
        assert_eq!(SimState::parse("martian"), None);
    }

    #[test]
    fn shed_config_and_power_sample_shapes() {
        let d = serde_json::to_string(&ShedConfig::DEFAULTS).unwrap();
        assert_eq!(
            d,
            r#"{"enabled":true,"uvSamples":3,"uvWindowSecs":30,"railLowVolts":4.85,"railLowSecs":15,"escalateDwellSecs":60,"escalateDutyPct":70,"cpuTempC":80,"cpuTempRestoreC":70,"cpuTempSecs":15,"restoreSecs":120,"railRestoreVolts":4.95,"maxShedsPerHour":4}"#
        );
        let p = r#"{"uv":true,"railV":4.66,"tempC":71.5,"softTemp":false}"#;
        assert_eq!(round_trip::<PowerSampleReading>(p), p);
        let p = r#"{"uv":false,"railV":null}"#;
        assert_eq!(round_trip::<PowerSampleReading>(p), p);
        let p = r#"{"uv":false,"railV":5,"tempC":null}"#;
        // `tempC: null` is legal input (`number | null`) and reads as None.
        let v: PowerSampleReading = serde_json::from_str(p).unwrap();
        assert_eq!(v.temp_c, None);
        assert_eq!(v.rail_v, Some(5.0));
    }

    #[test]
    fn gnss_view_time_source_gps_clock_shapes() {
        let g = r#"{"fix":true,"sky":"no-rf"}"#;
        assert_eq!(round_trip::<GnssView>(g), g);
        assert_eq!(
            serde_json::to_string(&TimeSource::Unsynced).unwrap(),
            r#""unsynced""#
        );
        assert_eq!(
            serde_json::to_string(&SteppedClockSource::Nitz).unwrap(),
            r#""nitz""#
        );
        assert_eq!(TimeSource::from(SteppedClockSource::Gps), TimeSource::Gps);
        let r = r#"{"utcMs":1791511485433,"ageMs":120,"valid":true}"#;
        assert_eq!(round_trip::<GpsClockReading>(r), r);
    }

    #[test]
    fn camera_mount_state_shape() {
        // Mirrors the reconciler's report block (boot-config.ts:233-253).
        let s = r#"{"at":1791511485433,"cameras":[{"streamKey":"cam0","desiredDeg":180,"inForceDeg":0,"bootDeg":180,"verdict":"pending-reboot","detail":"written; reboot to apply"},{"streamKey":"cam1","desiredDeg":0,"inForceDeg":null,"bootDeg":null,"verdict":"write-failed"}],"rebootRequired":true,"wrote":true}"#;
        assert_eq!(round_trip::<CameraMountState>(s), s);
        assert!(serde_json::from_str::<CameraRotationDeg>("90").is_err());
        assert_eq!(
            serde_json::from_str::<CameraRotationDeg>("180.0").unwrap(),
            CameraRotationDeg::Deg180
        );
    }

    #[test]
    fn camera_mount_verdict_wire_strings() {
        // Exactly the TS union (boot-config.ts:216-231): config-sync.ts
        // `cameraMountStatus` keys its wording off these strings.
        const ALL: [(CameraMountVerdict, &str); 7] = [
            (CameraMountVerdict::Applied, "applied"),
            (CameraMountVerdict::PendingReboot, "pending-reboot"),
            (CameraMountVerdict::Unconfirmed, "unconfirmed"),
            (CameraMountVerdict::UnknownCamera, "unknown-camera"),
            (CameraMountVerdict::NoOverlay, "no-overlay"),
            (CameraMountVerdict::Unsupported, "unsupported"),
            (CameraMountVerdict::WriteFailed, "write-failed"),
        ];
        for (v, s) in ALL {
            let json = format!("\"{s}\"");
            assert_eq!(serde_json::to_string(&v).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<CameraMountVerdict>(&json).unwrap(),
                v
            );
        }
        assert!(serde_json::from_str::<CameraMountVerdict>(r#""pending""#).is_err());
    }

    #[test]
    fn modem_presence_shape() {
        let s = r#"{"present":true,"usbId":"1e0e:9011","description":"SimTech","ttyPorts":["/dev/ttyUSB0"]}"#;
        assert_eq!(round_trip::<ModemPresence>(s), s);
        let s = r#"{"present":false,"ttyPorts":[]}"#;
        assert_eq!(round_trip::<ModemPresence>(s), s);
    }
}
