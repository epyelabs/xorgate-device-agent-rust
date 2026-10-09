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
//!   See [`JsNumber`].

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A float that serializes the way `JSON.stringify` prints a JS number.
///
/// `serde_json` would print `0.0` / `3000.0` for integral f64s; JS prints `0`
/// / `3000`. Both parse back to the same number, but Appendix A asks for
/// byte-identical payloads, so the integral case is emitted as an integer.
/// Non-finite values are not representable in JSON (JS prints `null`); the
/// collectors never produce them, and we serialize them as `null` to match.
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
        // 2^53: above this an f64 cannot hold a non-integer anyway, and i64
        // formatting would differ from JS's exponent form; stay on the float
        // path (ryu and JS agree on the shortest repr there).
        if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
            s.serialize_i64(v as i64)
        } else {
            s.serialize_f64(v)
        }
    }
}

impl<'de> Deserialize<'de> for JsNumber {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        f64::deserialize(d).map(JsNumber)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_block: Option<LiveBlock>,
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
