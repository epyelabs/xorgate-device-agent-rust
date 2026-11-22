//! Cloud -> device config channel: the retained JSON document on
//! `xorgate/devices/{deviceId}/config`, parsed tolerantly and merged into
//! settings.json. Port of `src/agent/cloud-config.ts` (413 lines):
//! `CloudConfig`, `CloudConfigMessage`, `parseCloudMessage` (`:187`),
//! `parseCloudConfig` (`:205`), `parseNamespaces` (`:209`), `imuMountEqual`
//! (`:274`) and the merge/diff core of `applyCloudConfig` (`:293-411`) as
//! [`merge`] over an already-performed settings read. The file re-read and the
//! `saveSettings` write (`:307`, `:409`) stay in the Phase 2 agent.
//!
//! Semantics the fixtures pin (plan README Appendix A, agent-anatomy §10
//! invariant 5):
//! - a namespace ABSENT from the message leaves the local key alone; PRESENT
//!   replaces the whole local namespace; `{}` (or an all-invalid object) is
//!   the cloud's "revert to device defaults" marker and REMOVES the local key;
//! - `rev` is metadata (never written to settings.json); `uploadBucket`,
//!   `scope` and `hardware` ride the envelope and are handled by the worker
//!   OUTSIDE the rev guard (kvs.json / telemetry topic / hardware.json) — they
//!   are parsed here and ignored by [`merge`];
//! - an EXISTING but UNREADABLE settings.json REFUSES the merge (the
//!   2026-08-07 data-loss bug: merging onto `{}` would write away `mode` and
//!   every namespace the cloud does not manage). ABSENT and INVALID both merge
//!   onto `{}` — the self-heal that keeps a torn write from wedging delivery.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::hardware_block::{parse_hardware_block, DeviceHardwareBlock};
use crate::orientation::{parse_imu_mount, ImuMount};
use crate::settings::{
    parse_camera_mount_settings, parse_camera_sensors_settings, parse_cellular_settings, parse_lte_recovery_settings,
    parse_obd_settings, parse_recording_settings, parse_simulation_settings,
    parse_time_sync_settings, AgentSettings, CameraMountSettings, CameraSensorsSettings,
    CellularSettings,
    LteRecoverySettings, ObdSettings, RecordingSettings, SettingsReadFailure, SettingsReadResult,
    SimulationSettings, TimeSyncSettings,
};
use crate::telemetry_scope::TenantScope;

/// `Option<Option<T>>` as the TS `T | null | undefined` triple: outer `None` =
/// key absent (leave local alone), `Some(None)` = `null` (clear the local
/// key), `Some(Some(v))` = replace. serde's own `Option` treats a JSON `null`
/// as the outer `None`, so the inner layer is deserialised explicitly.
fn double_option<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `CloudConfig` (`cloud-config.ts:61`). Field order = `parseNamespaces`
/// insertion order (`:209-271`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudConfig {
    /// Monotonic desired-config revision (`devices.config_rev`). ABSENT or 0
    /// = the backend predates it → apply unconditionally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gnss_ant_bias: Option<bool>,
    /// IMU mounting correction; `Some(None)` = carried but nothing valid (the
    /// cloud's revert marker) → remove the local key.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub imu_mount: Option<Option<ImuMount>>,
    /// Segment-upload S3 bucket (persisted into kvs.json by the worker, never settings.json).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_bucket: Option<String>,
    /// The device's tenant (telemetry topic scope); envelope-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<TenantScope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub recording: Option<Option<RecordingSettings>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub lte_recovery: Option<Option<LteRecoverySettings>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub time_sync: Option<Option<TimeSyncSettings>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub cellular: Option<Option<CellularSettings>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub camera_mount: Option<Option<CameraMountSettings>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub simulation: Option<Option<SimulationSettings>>,
    /// Vehicle OBD-II adapter (`cloud-config.ts:135-144` @ 09da38d): same
    /// replace-whole-namespace semantics; `Some(None)` = carried but nothing
    /// valid → remove the local key.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub obd: Option<Option<ObdSettings>>,
    /// Camera sensor per port (RUST-ONLY): same replace-whole-namespace
    /// semantics; `Some(None)` = carried without a `cameras` object (`{}`,
    /// the cloud's clear) → remove the local key.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub camera_sensors: Option<Option<CameraSensorsSettings>>,
    /// Device hardware block; `Some(None)` = carried but failed validation
    /// (log + keep the current block); absent = keep the current block too.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub hardware: Option<Option<DeviceHardwareBlock>>,
}

/// `CloudConfigMessage` (`cloud-config.ts:177`): the parsed message plus the
/// RAW object it came from — the reported plane needs both halves (`raw` is
/// what the cloud asked for, `config` is what the device will hold; the
/// difference is the `adjusted` verdict).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudConfigMessage {
    /// Every top-level key as delivered, before the tolerant parse.
    #[serde(serialize_with = "crate::js::json_map::serialize")]
    pub raw: Map<String, Value>,
    pub config: CloudConfig,
}

/// `parseCloudMessage` (`cloud-config.ts:187`): None when the payload is not
/// JSON, not a plain object, or not `v: 1` (a numeric compare — `1.0` passes,
/// `"1"` does not).
pub fn parse_cloud_message(payload: &str) -> Option<CloudConfigMessage> {
    // `JSON.parse` key order (integer-like keys first at every level).
    let obj: Value = crate::js::parsed_key_order(serde_json::from_str(payload).ok()?);
    let Value::Object(raw) = obj else {
        return None;
    };
    if raw.get("v").and_then(Value::as_f64) != Some(1.0) {
        return None;
    }
    let config = parse_namespaces(&raw);
    Some(CloudConfigMessage { raw, config })
}

/// `parseCloudConfig` (`cloud-config.ts:205`): the normalized half only.
pub fn parse_cloud_config(payload: &str) -> Option<CloudConfig> {
    parse_cloud_message(payload).map(|m| m.config)
}

/// `x && typeof x === "object" && !Array.isArray(x)` — a plain object.
fn plain_object(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| v.is_object())
}

/// `parseNamespaces` (`cloud-config.ts:209`): tolerant — unknown keys are
/// ignored, known keys with the wrong container type are dropped (absent), an
/// object that parses to nothing becomes `Some(None)` (clear).
pub fn parse_namespaces(obj: &Map<String, Value>) -> CloudConfig {
    let mut out = CloudConfig::default();
    // Anything not a positive finite number is treated as absent ("always
    // apply" — the compat path, never a hard failure). `Math.floor`.
    if let Some(rev) = obj.get("rev").and_then(Value::as_f64) {
        if rev.is_finite() && rev > 0.0 {
            out.rev = Some(rev.floor() as u64);
        }
    }
    out.gnss_ant_bias = obj.get("gnssAntBias").and_then(Value::as_bool);
    if let Some(v) = plain_object(obj.get("imuMount")) {
        // An all-invalid/empty object → null (clear), exactly like the
        // namespaces below. Non-object values are still ignored entirely.
        out.imu_mount = Some(parse_imu_mount(v));
    }
    if let Some(b) = obj.get("uploadBucket").and_then(Value::as_str) {
        if !b.is_empty() {
            out.upload_bucket = Some(b.to_string());
        }
    }
    if let Some(scope) = plain_object(obj.get("scope")) {
        // Both halves or neither: a half-populated scope would build a topic
        // with an empty segment, which the broker refuses.
        let org = scope.get("organizationId").and_then(Value::as_str);
        let ws = scope.get("workspaceId").and_then(Value::as_str);
        if let (Some(org), Some(ws)) = (org, ws) {
            if !org.is_empty() && !ws.is_empty() {
                out.scope = Some(TenantScope {
                    organization_id: org.to_string(),
                    workspace_id: ws.to_string(),
                });
            }
        }
    }
    if let Some(v) = plain_object(obj.get("recording")) {
        out.recording = Some(parse_recording_settings(v));
    }
    if let Some(v) = plain_object(obj.get("lteRecovery")) {
        out.lte_recovery = Some(parse_lte_recovery_settings(v));
    }
    if let Some(v) = plain_object(obj.get("timeSync")) {
        out.time_sync = Some(parse_time_sync_settings(v));
    }
    if let Some(v) = plain_object(obj.get("cellular")) {
        out.cellular = Some(parse_cellular_settings(v));
    }
    if let Some(v) = plain_object(obj.get("cameraMount")) {
        out.camera_mount = Some(parse_camera_mount_settings(v));
    }
    if let Some(v) = plain_object(obj.get("simulation")) {
        out.simulation = Some(parse_simulation_settings(v));
    }
    // `obd` (`cloud-config.ts:280-283` @ 09da38d): arrays are refused
    // outright (`!Array.isArray(obd)`), like `simulation`.
    if let Some(v) = plain_object(obj.get("obd")) {
        out.obd = Some(parse_obd_settings(v));
    }
    if let Some(v) = plain_object(obj.get("cameraSensors")) {
        out.camera_sensors = Some(parse_camera_sensors_settings(v));
    }
    if let Some(v) = plain_object(obj.get("hardware")) {
        out.hardware = Some(parse_hardware_block(v));
    }
    out
}

/// `imuMountEqual` (`cloud-config.ts:274`): field-wise `===`.
pub fn imu_mount_equal(a: Option<&ImuMount>, b: Option<&ImuMount>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.enabled == b.enabled
                && a.roll_deg == b.roll_deg
                && a.pitch_deg == b.pitch_deg
                && a.yaw_deg == b.yaw_deg
        }
        _ => false,
    }
}

/// The message `applyCloudConfig` throws for an unreadable file (`cloud-config.ts:309-311`).
pub fn unreadable_refusal(path: &str) -> String {
    format!(
        "settings.json exists but cannot be read — refusing to overwrite it (check owner/permissions on {path})"
    )
}

/// What the merge decided (the pure half of `applyCloudConfig`'s
/// `{ changed }` + the document it would have saved).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MergeOutcome {
    /// The settings to persist, present only when `changed`.
    pub next: Option<AgentSettings>,
    /// The exact document `saveSettings` would write (`JSON.stringify` of
    /// `{...current, ...next}` with the namespace sets/deletes applied):
    /// existing keys keep the parsed file's order, newly-added keys append in
    /// `gnssAntBias, imuMount, recording, lteRecovery, timeSync, cellular,
    /// cameraMount, simulation, obd, cameraSensors` order. Present only when `changed`.
    /// Write it as [`MergeOutcome::document_json`] — NOT a plain
    /// `serde_json::to_string`/`to_vec` of the map, whose number formatter
    /// differs from JS (`0.000005` → `5e-6`; review-integration M1).
    pub document: Option<Map<String, Value>>,
    pub changed: bool,
    /// Set when the merge REFUSED (unreadable file); `changed` is then false.
    pub refuse_reason: Option<String>,
}

impl MergeOutcome {
    /// The exact settings.json bytes `saveSettings` writes
    /// (`JSON.stringify(merged)`, compact, numbers printed like JS); `None`
    /// when nothing changed.
    pub fn document_json(&self) -> Option<String> {
        self.document.as_ref().map(crate::js::map_to_json_string)
    }
}

/// `set` / `clear` / untouched for one replace-whole namespace
/// (`cloud-config.ts:320-381`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Set,
    Clear,
}

fn namespace_change<T: Serialize>(
    cloud: &Option<Option<T>>,
    current: &Option<T>,
) -> Option<Change> {
    let target = cloud.as_ref()?;
    // `JSON.stringify(target) !== JSON.stringify(current)`
    // (`cloud-config.ts:333,341,349,358,368,378`) — a KEY-ORDER-SENSITIVE
    // test: the operator-keyed maps (`recording.video.cameras`,
    // `cameraMount.cameras`) keep their file / delivery order, so the same
    // values in a different order ARE a change (the TS rewrites the file in
    // cloud order). Structural `==` would be wrong here: the derived
    // `PartialEq` on `IndexMap` (and on `serde_json::Map` with
    // `preserve_order`) ignores order. The typed settings serialise exactly
    // like `JSON.stringify` (WP1: parser key order, JS number form), and
    // `None` → `null` on both sides stands in for `undefined === undefined`.
    if serde_json::to_string(target).expect("namespace serialises")
        == serde_json::to_string(current).expect("namespace serialises")
    {
        return None;
    }
    Some(if target.is_some() {
        Change::Set
    } else {
        Change::Clear
    })
}

fn apply_change<T: Serialize + Clone>(
    doc: &mut Map<String, Value>,
    key: &str,
    change: Option<Change>,
    cloud: &Option<Option<T>>,
) {
    match change {
        Some(Change::Set) => {
            let v = cloud
                .as_ref()
                .and_then(|c| c.as_ref())
                .expect("set implies a value");
            doc.insert(key.to_string(), serde_json::to_value(v).unwrap());
        }
        Some(Change::Clear) => {
            doc.shift_remove(key);
        }
        None => {}
    }
}

/// The merge/diff core of `applyCloudConfig` (`cloud-config.ts:293-411`) over
/// a settings read the caller performed immediately beforehand. `path` only
/// feeds the refusal message. Nothing here writes.
pub fn merge(before: &SettingsReadResult, config: &CloudConfig, path: &str) -> MergeOutcome {
    // ONLY `unreadable` refuses. An ABSENT file has nothing to lose, and a
    // CORRUPT one has already lost it — overwriting both is the self-heal.
    let current = match before {
        Ok(s) => s.clone(),
        Err(SettingsReadFailure::Unreadable) => {
            return MergeOutcome {
                refuse_reason: Some(unreadable_refusal(path)),
                ..MergeOutcome::default()
            };
        }
        Err(_) => AgentSettings::default(),
    };

    let gnss_change = match config.gnss_ant_bias {
        Some(b) if Some(b) != current.gnss_ant_bias => Some(b),
        _ => None,
    };
    let imu_change = config.imu_mount.as_ref().and_then(|target| {
        if imu_mount_equal(target.as_ref(), current.imu_mount.as_ref()) {
            None
        } else {
            Some(if target.is_some() {
                Change::Set
            } else {
                Change::Clear
            })
        }
    });
    let recording_change = namespace_change(&config.recording, &current.recording);
    let lte_change = namespace_change(&config.lte_recovery, &current.lte_recovery);
    let time_sync_change = namespace_change(&config.time_sync, &current.time_sync);
    let cellular_change = namespace_change(&config.cellular, &current.cellular);
    let camera_mount_change = namespace_change(&config.camera_mount, &current.camera_mount);
    let simulation_change = namespace_change(&config.simulation, &current.simulation);
    let obd_change = namespace_change(&config.obd, &current.obd);
    let camera_sensors_change = namespace_change(&config.camera_sensors, &current.camera_sensors);

    if gnss_change.is_none()
        && imu_change.is_none()
        && recording_change.is_none()
        && lte_change.is_none()
        && time_sync_change.is_none()
        && cellular_change.is_none()
        && camera_mount_change.is_none()
        && simulation_change.is_none()
        && obd_change.is_none()
        && camera_sensors_change.is_none()
    {
        return MergeOutcome {
            changed: false,
            ..MergeOutcome::default()
        };
    }

    // `{ ...current, ...next }` then the per-namespace set/delete, on a
    // preserve_order map so the written bytes match `JSON.stringify(merged)`.
    let Value::Object(mut doc) = serde_json::to_value(&current).unwrap() else {
        unreachable!("AgentSettings serialises as an object");
    };
    if let Some(b) = gnss_change {
        doc.insert("gnssAntBias".to_string(), Value::Bool(b));
    }
    apply_change(&mut doc, "imuMount", imu_change, &config.imu_mount);
    apply_change(&mut doc, "recording", recording_change, &config.recording);
    apply_change(&mut doc, "lteRecovery", lte_change, &config.lte_recovery);
    apply_change(&mut doc, "timeSync", time_sync_change, &config.time_sync);
    apply_change(&mut doc, "cellular", cellular_change, &config.cellular);
    apply_change(
        &mut doc,
        "cameraMount",
        camera_mount_change,
        &config.camera_mount,
    );
    apply_change(
        &mut doc,
        "simulation",
        simulation_change,
        &config.simulation,
    );
    apply_change(&mut doc, "obd", obd_change, &config.obd);
    apply_change(
        &mut doc,
        "cameraSensors",
        camera_sensors_change,
        &config.camera_sensors,
    );

    let next: AgentSettings =
        serde_json::from_value(Value::Object(doc.clone())).expect("merged document is settings");
    MergeOutcome {
        next: Some(next),
        document: Some(doc),
        changed: true,
        refuse_reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_state_deserialises() {
        let c: CloudConfig =
            serde_json::from_str(r#"{"imuMount":null,"recording":{"video":{"enabled":true}}}"#)
                .unwrap();
        assert_eq!(c.imu_mount, Some(None));
        assert!(matches!(c.recording, Some(Some(_))));
        assert_eq!(c.time_sync, None);
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"imuMount":null,"recording":{"video":{"enabled":true}}}"#
        );
    }

    #[test]
    fn v_must_be_the_number_one() {
        assert!(parse_cloud_message(r#"{"v":1.0}"#).is_some());
        assert!(parse_cloud_message(r#"{"v":"1"}"#).is_none());
        assert!(
            parse_cloud_message(r#"{"v":1,"rev":2.9}"#)
                .unwrap()
                .config
                .rev
                == Some(2)
        );
        assert!(parse_cloud_message(r#"{"v":1,"rev":-1}"#)
            .unwrap()
            .config
            .rev
            .is_none());
    }

    #[test]
    fn unreadable_refuses_and_keeps_the_message() {
        let out = merge(
            &Err(SettingsReadFailure::Unreadable),
            &CloudConfig {
                gnss_ant_bias: Some(true),
                ..Default::default()
            },
            "/var/lib/xorgate/settings.json",
        );
        assert!(!out.changed);
        assert_eq!(
            out.refuse_reason.as_deref(),
            Some("settings.json exists but cannot be read — refusing to overwrite it (check owner/permissions on /var/lib/xorgate/settings.json)")
        );
        let out = merge(
            &Err(SettingsReadFailure::Invalid),
            &CloudConfig {
                gnss_ant_bias: Some(true),
                ..Default::default()
            },
            "p",
        );
        assert!(out.changed);
        assert_eq!(out.document_json().unwrap(), r#"{"gnssAntBias":true}"#);
    }
}
