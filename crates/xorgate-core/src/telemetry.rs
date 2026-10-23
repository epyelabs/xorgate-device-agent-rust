//! Telemetry payload + status message shapes — byte-compatible with the TS
//! worker (`src/agent/telemetry.ts`, `src/worker.ts` publish tick) and the
//! backend schema (`research/backend-device-contract.md` §2 "Telemetry
//! payload"). Appendix A of the plan freezes this contract.
//!
//! Serialization rules that make the output byte-identical to
//! `JSON.stringify` of the TS objects:
//! - key order = TS insertion order (struct field order here, `IndexMap` for
//!   dynamic groups; `serde_json` is built with `preserve_order`);
//! - optional fields are OMITTED (never `null`) unless the TS writes `null`
//!   explicitly (`sessionId`, `lastUploadAt`);
//! - numbers print like JS: an integral f64 prints without a fractional part
//!   (`0`, `3000`), everything else as the shortest round-trip repr (`5.8`).
//!   See [`JsNumber`] and the [`js_num`] / [`js_num_opt`] serde helpers for
//!   plain `f64` fields.
//!
//! Phase 1 (WP1) completed the type set of `src/agent/telemetry.ts`: the typed
//! metric groups ([`TelemetryMetrics`]), [`TelemetryReading`], [`RunMode`],
//! [`ConnectionState`], [`ActivityEvent`] and [`StateSnapshot`]. The dynamic
//! [`Metrics`] map stays the wire representation (`Partial<TelemetryMetrics>`
//! — only declared groups are present, and the gps collector emits three
//! fields the TS interface does not declare).

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A float that serializes the way `JSON.stringify` prints a JS number —
/// bit-exact with V8 for every double ([`crate::js::number_to_string`];
/// `fixtures/js/number-format.json`, 5 141 doubles dumped from node).
///
/// `serde_json` alone differs from JS in three bands: integral f64s print as
/// `0.0` / `3000.0` (JS `0` / `3000`); integers in [2^53, 1e21) print in
/// exponent form or with a `.0` (`1e20`, `9007199254740992.0`; JS
/// `100000000000000000000`, `9007199254740992`); and fractions in
/// [1e-6, 1e-5) print as `5e-6` (JS `0.000005`) — review-wp6 M2. So safe
/// integers go out as `i64`, and every other finite value as the JS text via
/// `RawValue` (verbatim in `to_string`/`to_writer`; `to_value` re-parses it,
/// so a `Value` built from a `JsNumber` holds the same f64). Non-finite
/// values are not representable in JSON (JS prints `null`); the collectors
/// never produce them, and we serialize them as `null` to match.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct JsNumber(pub f64);

impl From<f64> for JsNumber {
    fn from(v: f64) -> Self {
        JsNumber(v)
    }
}

impl Serialize for JsNumber {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let v = self.0;
        if !v.is_finite() {
            return s.serialize_none();
        }
        if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
            // exact, and `-0` → `0` like JS
            return s.serialize_i64(v as i64);
        }
        serde_json::value::RawValue::from_string(crate::js::number_to_string(v))
            .map_err(serde::ser::Error::custom)?
            .serialize(s)
    }
}

impl<'de> Deserialize<'de> for JsNumber {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        f64::deserialize(d).map(JsNumber)
    }
}

/// `#[serde(with = "js_num")]` on an `f64` field: prints like a JS number.
pub mod js_num {
    use super::JsNumber;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        JsNumber(*v).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        f64::deserialize(d)
    }
}

/// `#[serde(with = "js_num_opt")]` on an `Option<f64>` field: `None` prints
/// as `null` (combine with `skip_serializing_if = "Option::is_none"` where
/// the TS omits the key instead), `Some(v)` like a JS number.
pub mod js_num_opt {
    use super::JsNumber;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => JsNumber(*v).serialize(s),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Option::<f64>::deserialize(d)
    }
}

/// `{ value: number, unit?: string }` — the only leaf shape the cloud accepts
/// (a boolean/string leaf drops the whole payload; agent-anatomy §10.11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricLeaf {
    pub value: JsNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

impl MetricLeaf {
    pub fn new(value: f64) -> Self {
        Self {
            value: JsNumber(value),
            unit: None,
        }
    }
    pub fn with_unit(value: f64, unit: &str) -> Self {
        Self {
            value: JsNumber(value),
            unit: Some(unit.to_string()),
        }
    }
}

/// One metric group (`system`, `gps`, …): ordered field → leaf.
pub type MetricGroup = IndexMap<String, MetricLeaf>;
/// `metrics`: ordered group → fields. Groups with nothing are omitted by the
/// collector registry (`collectors/index.ts:read()`), never published empty.
pub type Metrics = IndexMap<String, MetricGroup>;

/// `TelemetryMetrics.imu` (`telemetry.ts:23-34`): the ten BNO085 fields,
/// always published together (invariant 11). Key order = `collectors/imu.ts:94-105`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImuGroup {
    pub quat_r: MetricLeaf,
    pub quat_i: MetricLeaf,
    pub quat_j: MetricLeaf,
    pub quat_k: MetricLeaf,
    pub euler_roll: MetricLeaf,
    pub euler_pitch: MetricLeaf,
    pub euler_yaw: MetricLeaf,
    pub accel_x: MetricLeaf,
    pub accel_y: MetricLeaf,
    pub accel_z: MetricLeaf,
}

/// `TelemetryMetrics.gps` (`telemetry.ts:35-57`) in WIRE order — the order
/// `collectors/gps.ts:261-298 mapLatest` inserts keys, which differs from the
/// TS interface's declaration order. `sats_heard`, `snr_max` and `sky_state`
/// are emitted by the collector (`gps.ts:273-278`) but NOT declared in the TS
/// interface (it types `mapLatest` as `Record<string, unknown>`); they are
/// part of the wire contract and so of this struct. (review-wp1 #9: the TS
/// simulator, `agent/simulator.ts:100-109`, emits `lat, lon, alt, speed,
/// course, satellites, fix_quality, …`; a Rust sim going through this struct
/// uses the collector order instead — consumers parse `metrics` by key.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpsGroup {
    /// GGA fix quality: 0 = none, 1 = GPS, 2 = DGPS. Always present while NMEA flows.
    pub fix_quality: MetricLeaf,
    /// Satellites used in the fix (GGA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satellites: Option<MetricLeaf>,
    /// Satellites in view across constellations (GSV).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satellites_in_view: Option<MetricLeaf>,
    /// RF evidence: satellites with a reported C/N0 (sky_state Phase 7; undeclared in the TS interface).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sats_heard: Option<MetricLeaf>,
    /// Max C/N0 in dBHz (undeclared in the TS interface).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr_max: Option<MetricLeaf>,
    /// `SKY_STATE_CODES` 0 clear · 1 obstructed · 2 no-rf; omitted while unknown (undeclared in the TS interface).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sky_state: Option<MetricLeaf>,
    /// Horizontal dilution of precision (GSA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdop: Option<MetricLeaf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<MetricLeaf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<MetricLeaf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<MetricLeaf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<MetricLeaf>,
    /// Course over ground in degrees true (RMC); present only while moving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub course: Option<MetricLeaf>,
    /// Time-to-first-fix in seconds, present once a fix follows an observed acquiring period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttff_s: Option<MetricLeaf>,
}

/// `TelemetryMetrics.lte` (`telemetry.ts:58-72`) in wire order (`collectors/lte.ts:44-57`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LteGroup {
    pub quality: MetricLeaf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi: Option<MetricLeaf>,
    /// Modem PMU temperature (°C) from the daemon's AT+CPMUTEMP poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modem_temp: Option<MetricLeaf>,
    /// 0 ok · 1 acquiring · 2 degraded · 3 unhealthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gnss_health: Option<MetricLeaf>,
    /// CRESETs fired in the daemon's rolling 24 h budget window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_24h: Option<MetricLeaf>,
}

/// `TelemetryMetrics.system` (`telemetry.ts:73-84`) in wire order
/// (`collectors/system.ts compute()`, see `system::system_group`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemGroup {
    pub cpu_usage: MetricLeaf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_usage: Option<MetricLeaf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_temp: Option<MetricLeaf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_usage: Option<MetricLeaf>,
    /// Throttle bits 0–3, CURRENT state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttled: Option<MetricLeaf>,
    /// EXT5V input rail voltage (V), Pi 5 family only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rail_5v: Option<MetricLeaf>,
    /// PSU max current negotiated at boot (mA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psu_max_current: Option<MetricLeaf>,
}

/// `Partial<TelemetryMetrics>` (`telemetry.ts:16-85`), typed. Group order =
/// the collector registry order (`collectors/index.ts:124-127`: imu, gps,
/// lte, system). Absent groups are omitted (a collector that returns `{}`
/// contributes no key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TelemetryMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imu: Option<ImuGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps: Option<GpsGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<LteGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemGroup>,
}

impl TelemetryMetrics {
    /// Lower to the dynamic wire map (what the payload carries).
    pub fn into_metrics(self) -> Metrics {
        // Serialising through serde_json keeps the struct field order and
        // the optional-field omission rules in one place.
        let v = serde_json::to_value(&self).expect("TelemetryMetrics serialises");
        serde_json::from_value(v).expect("TelemetryMetrics is a Metrics map")
    }

    /// Lift a wire map back to the typed form (fails on an unknown group or a
    /// malformed leaf).
    pub fn from_metrics(m: &Metrics) -> Result<Self, serde_json::Error> {
        serde_json::from_value(serde_json::to_value(m)?)
    }
}

/// `TelemetryReading` (`telemetry.ts:133`): one reading per tick from the
/// simulator or the collector registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryReading {
    pub metrics: Metrics,
    pub media: TelemetryMedia,
}

/// `{ value: boolean }` — used only inside `media` (schema allows booleans there).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoolLeaf {
    pub value: bool,
}

/// One supervised KVS master (`MediaStream` in telemetry.ts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaStream {
    pub stream_key: String,
    pub channel_name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<bool>,
}

/// The `media` block. When no media supervisor runs the TS publishes the
/// hardcoded idle block `{kvs:{value:false}, recording:{value:false}, streams:[]}`
/// (`collectors/index.ts:read()` fallback + the worker's `recording: {value:
/// video.recording}` overlay, which is `false` with no video capture).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryMedia {
    pub kvs: BoolLeaf,
    pub recording: BoolLeaf,
    pub streams: Vec<MediaStream>,
}

impl TelemetryMedia {
    /// The idle block (no KVS masters, no capture).
    pub fn idle() -> Self {
        Self {
            kvs: BoolLeaf { value: false },
            recording: BoolLeaf { value: false },
            streams: Vec::new(),
        }
    }
}

/// `recording.liveBlock` (power_management_improvements Phase 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveBlock {
    pub reason: String,
    pub detail: String,
    pub since: i64,
}

/// On-device recording status (`RecordingStatus` in telemetry.ts). Key order
/// and null-vs-absent rules follow `recording/manager.ts:status()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStatus {
    pub enabled: bool,
    /// Explicit `null` when no session is open (the TS writes `null`, never omits).
    pub session_id: Option<String>,
    pub backlog_bytes: u64,
    pub backlog_segments: u64,
    pub dropped_segments: u64,
    pub uploading: bool,
    /// Explicit `null` until the first upload (same rule as `session_id`).
    pub last_upload_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_defer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_shed: Option<String>,
    /// `clockHold: true` while telemetry recording is wanted but the wall
    /// clock is untrusted; ABSENT otherwise (never `false`;
    /// `ts:src/recording/manager.ts:442-446` @ 09da38d).
    #[serde(default, skip_serializing_if = "is_false")]
    pub clock_hold: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_block: Option<LiveBlock>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl RecordingStatus {
    /// What `RecordingManager.status()` returns when nothing is enabled: no
    /// recorder, no disk manager stats, no uploader, and no camera ever
    /// consulted the boot policy (so `bootDefer` is absent). This is the block
    /// the TS worker publishes every tick on a device with recording off and
    /// no cameras — exactly Phase 0's situation.
    pub fn all_off() -> Self {
        Self {
            enabled: false,
            session_id: None,
            backlog_bytes: 0,
            backlog_segments: 0,
            dropped_segments: 0,
            uploading: false,
            last_upload_at: None,
            boot_defer: None,
            power_shed: None,
            clock_hold: false,
            live_block: None,
        }
    }
}

/// The §1.3 payload published to `…/telemetry` (`TelemetryPayload` in
/// telemetry.ts; built in `worker.ts` publish tick in this exact key order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryPayload {
    pub v: u8,
    pub device_id: String,
    /// Epoch milliseconds (`Date.now()` at the tick).
    pub ts: i64,
    /// Starts at 0 on process start, +1 per tick (`seq++`), never reset on reconnect.
    pub seq: u64,
    pub metrics: Metrics,
    pub media: TelemetryMedia,
    /// The TS worker includes it every tick; the type is optional because
    /// fixtures/sim payloads may omit it (cloud schema: optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingStatus>,
}

impl TelemetryPayload {
    pub fn new(device_id: &str, ts: i64, seq: u64, metrics: Metrics) -> Self {
        Self {
            v: 1,
            device_id: device_id.to_string(),
            ts,
            seq,
            metrics,
            media: TelemetryMedia::idle(),
            recording: Some(RecordingStatus::all_off()),
        }
    }
}

/// `{v:1, deviceId, ts, online}` on `…/status` — published on connect
/// (`online:true`), on graceful shutdown (`online:false`, shutdown ts) and as
/// the Last-Will (`online:false`, CONNECT-time ts). `research/backend-device-
/// contract.md` §2 "Status": `now - ts > 30 s ⇒ lwt`, else `graceful`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusPayload {
    pub v: u8,
    pub device_id: String,
    pub ts: i64,
    pub online: bool,
}

impl StatusPayload {
    pub fn new(device_id: &str, ts: i64, online: bool) -> Self {
        Self {
            v: 1,
            device_id: device_id.to_string(),
            ts,
            online,
        }
    }
}

/// `RunMode` (`telemetry.ts:201`): "sim" | "real" | "replay". Reported-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Sim,
    Real,
    Replay,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::Sim => "sim",
            RunMode::Real => "real",
            RunMode::Replay => "replay",
        }
    }
    /// `mode === "sim" || mode === "real" || mode === "replay"` (`settings.ts:619`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sim" => Some(RunMode::Sim),
            "real" => Some(RunMode::Real),
            "replay" => Some(RunMode::Replay),
            _ => None,
        }
    }
}

/// `ConnectionState` (`telemetry.ts:203`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Closed,
    Error,
}

impl ConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionState::Connecting => "connecting",
            ConnectionState::Connected => "connected",
            ConnectionState::Reconnecting => "reconnecting",
            ConnectionState::Closed => "closed",
            ConnectionState::Error => "error",
        }
    }
}

/// `ActivityEvent.level` (`telemetry.ts:212`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityLevel {
    Info,
    Warn,
    Error,
}

/// `ActivityEvent` (`telemetry.ts:210`): one line of the state.json ring buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub ts: i64,
    pub level: ActivityLevel,
    pub msg: String,
}

/// `StateSnapshot` (`telemetry.ts:222`): the atomic snapshot the worker
/// writes to `state.json` every tick (throttled). Key order = the TS
/// interface = the worker's initial literal (`worker.ts:356-377`);
/// `recording` is appended by `StateStore.setRecording` and so lands last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub v: u8,
    /// Package version that wrote this snapshot (forward-compat hint).
    pub written_by: String,
    pub device_id: String,
    pub mode: RunMode,
    /// Effective media-runtime enabled state for this run (config.media.enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_enabled: Option<bool>,
    pub rate_hz: JsNumber,
    pub endpoint: String,
    pub offline: bool,
    pub pid: u32,
    pub started_at: i64,
    /// Epoch ms of this snapshot — the freshness anchor the readers check.
    pub last_write_at: i64,
    pub connection: ConnectionState,
    /// Epoch ms the current connection state began (for "connected since").
    pub connection_since: i64,
    pub seq: u64,
    pub published: u64,
    pub publish_errors: u64,
    pub last_publish_at: Option<i64>,
    pub last_error: Option<String>,
    pub latest: Option<TelemetryPayload>,
    pub recent_events: Vec<ActivityEvent>,
    /// On-device recording status; absent on agents predating the recording feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingStatus>,
    /// OBD collector health (`StateStore.setObd`, `ts:src/daemon/state.ts:94-98`
    /// @ 09da38d): absent without the peripheral. The worker's first
    /// `setObd` runs on the first publish tick, after the recording manager's
    /// first `setRecording`, so the key lands after `recording`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obd: Option<ObdStatus>,
}

/// `ObdStatus` (`ts:src/agent/telemetry.ts:166-204` @ 09da38d): the OBD
/// collector's health for `xorgate status`, the monitor and state.json. Key
/// order = the TS interface = the collector's `status()` literal. The four
/// P1 nullable fields are always present (`null` when unknown); the P3 /
/// start-stop fields are present iff the collector set them (`Some`), with
/// `adapter`, `profile` and `lastStop` themselves nullable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObdStatus {
    /// The helper's states plus the supervisor's (`disabled`,
    /// `not_configured`, `starting`, `retrying`, `parked`).
    pub state: String,
    /// 0 no adapter, 1 adapter only, 2 ECU answering.
    pub link: u8,
    #[serde(with = "js_num_opt")]
    pub req_per_sec: Option<f64>,
    pub errors: u64,
    pub forbidden: u64,
    #[serde(with = "js_num_opt")]
    pub voltage: Option<f64>,
    pub detail: Option<String>,
    /// Epoch ms the current state began (a JS number).
    #[serde(with = "js_num")]
    pub since: f64,
    pub helper_version: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub adapter: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub profile: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voltage_low_events: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_seq: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub last_stop: Option<Option<String>>,
}

/// A key that may be absent (`None`), present as `null` (`Some(None)`) or
/// present with a value (`Some(Some(v))`). Used with `#[serde(default)]`.
fn present_nullable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_prints_like_json_stringify() {
        assert_eq!(serde_json::to_string(&JsNumber(0.0)).unwrap(), "0");
        assert_eq!(serde_json::to_string(&JsNumber(3000.0)).unwrap(), "3000");
        assert_eq!(serde_json::to_string(&JsNumber(5.8)).unwrap(), "5.8");
        assert_eq!(serde_json::to_string(&JsNumber(4.939)).unwrap(), "4.939");
        assert_eq!(serde_json::to_string(&JsNumber(-0.008)).unwrap(), "-0.008");
        assert_eq!(
            serde_json::to_string(&JsNumber(0.0042735)).unwrap(),
            "0.0042735"
        );
        assert_eq!(serde_json::to_string(&JsNumber(f64::NAN)).unwrap(), "null");
        assert_eq!(serde_json::to_string(&JsNumber(-0.0)).unwrap(), "0");
        // review-wp6 M2 bands (node: JSON.stringify([2**53,2**60,1e20,1e21,5e-6,1e-7]))
        let p = |v: f64| serde_json::to_string(&JsNumber(v)).unwrap();
        assert_eq!(p(9_007_199_254_740_992.0), "9007199254740992");
        assert_eq!(p(2f64.powi(60)), "1152921504606847000");
        assert_eq!(p(-2f64.powi(60)), "-1152921504606847000");
        assert_eq!(p(1e20), "100000000000000000000");
        assert_eq!(p(1e21), "1e+21");
        assert_eq!(p(5e-6), "0.000005");
        assert_eq!(p(1e-7), "1e-7");
        // through a Value the same f64 survives
        assert_eq!(
            serde_json::to_value(JsNumber(0.1)).unwrap().as_f64(),
            Some(0.1)
        );
        assert_eq!(
            serde_json::to_value(JsNumber(2f64.powi(60)))
                .unwrap()
                .as_f64(),
            Some(2f64.powi(60))
        );
    }

    #[test]
    fn payload_serializes_in_the_ts_key_order() {
        let mut system = MetricGroup::new();
        system.insert("cpu_usage".into(), MetricLeaf::with_unit(5.8, "%"));
        system.insert("ram_usage".into(), MetricLeaf::with_unit(11.2, "%"));
        system.insert("cpu_temp".into(), MetricLeaf::with_unit(52.9, "°C"));
        system.insert("disk_usage".into(), MetricLeaf::with_unit(19.2, "%"));
        system.insert("throttled".into(), MetricLeaf::new(0.0));
        system.insert("rail_5v".into(), MetricLeaf::with_unit(4.939, "V"));
        system.insert(
            "psu_max_current".into(),
            MetricLeaf::with_unit(3000.0, "mA"),
        );
        let mut metrics = Metrics::new();
        metrics.insert("system".into(), system);
        let p = TelemetryPayload::new(
            "58b51737-196e-4c29-a6f6-36f6793a5126",
            1791511485433,
            40913,
            metrics,
        );
        let json = serde_json::to_string(&p).unwrap();
        // The `system` group + envelope below were copied from the live
        // JS agent's state.json `latest` on the bench (2026-10-08).
        assert_eq!(
            json,
            r#"{"v":1,"deviceId":"58b51737-196e-4c29-a6f6-36f6793a5126","ts":1791511485433,"seq":40913,"metrics":{"system":{"cpu_usage":{"value":5.8,"unit":"%"},"ram_usage":{"value":11.2,"unit":"%"},"cpu_temp":{"value":52.9,"unit":"°C"},"disk_usage":{"value":19.2,"unit":"%"},"throttled":{"value":0},"rail_5v":{"value":4.939,"unit":"V"},"psu_max_current":{"value":3000,"unit":"mA"}}},"media":{"kvs":{"value":false},"recording":{"value":false},"streams":[]},"recording":{"enabled":false,"sessionId":null,"backlogBytes":0,"backlogSegments":0,"droppedSegments":0,"uploading":false,"lastUploadAt":null}}"#
        );
        // Round-trips.
        let back: TelemetryPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn status_payload_shape() {
        let s = StatusPayload::new("dev", 1700000000000, false);
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"v":1,"deviceId":"dev","ts":1700000000000,"online":false}"#
        );
    }

    #[test]
    fn media_stream_recording_flag_is_optional() {
        let s = MediaStream {
            stream_key: "cam1".into(),
            channel_name: "x-cam1".into(),
            state: "stopped".into(),
            recording: None,
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"streamKey":"cam1","channelName":"x-cam1","state":"stopped"}"#
        );
    }
}
