//! Device -> cloud REPORTED config plane: what the device ACTUALLY holds,
//! published on `xorgate/devices/{id}/config/reported` so the cloud can say
//! applied / adjusted / rejected instead of only "saved". Port of the pure
//! ~800 lines of `src/agent/config-sync.ts` (`:84-1092`): the types, the rev
//! guard, the `config-sync.json` shape + its validating read, namespace
//! snapshots/diffs, the baseline rule, the modem-telemetry sanitizers, the
//! `readModemBias` decision over an already-read snapshot, `buildEffective`,
//! `buildStatus` (with every `detail` / `reason` string VERBATIM — the console
//! renders them, plan README Appendix A) and `buildConfigReport`.
//!
//! Left in the Phase 2 agent: `configSyncPath` / `loadConfigSyncState`'s
//! read / `saveConfigSyncState` (`:192-246`), `readSettingsHash` (`:273`,
//! sha256 of the file), the `readModemTelemetry` read inside `readModemBias`
//! (`:509`), `loadBuildInfo()` (`:639`, injected as [`BuildInfo`]) and the
//! whole `class ConfigSync` (`:1211-1651`: debounce, heartbeat, settle /
//! modem-arrival / APN watches, the settings.json fingerprint watcher and the
//! publish). Two of that class's decisions are pure kernels and live here
//! with line references: [`rebaseline_after_write`] (`:1335-1353`) and
//! [`local_report_changes`] (`:1522-1534`); the class's `persist` merge
//! (`:1277-1280`) is [`ConfigSyncState::merge`].
//!
//! `CONFIG_APPLY_CLASSES` / `CONFIG_NAMESPACE_KEYS` / `ApplyClass` have ONE
//! definition, in [`crate::config_namespaces`]; they are re-exported here.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::boot_config::{CameraMountState, CameraMountVerdict};
use crate::build_info::BuildInfo;
use crate::clock_trust::ClockTrustState;
use crate::cloud_config::CloudConfig;
use crate::js::{self, trim as js_trim};
use crate::js::{date_parse_ms, number_to_string, object_key_order, parsed_key_order};
use crate::settings::{
    AgentSettings, CameraMountSettings, CameraSensorsSettings, CellularSettings, ObdSettings,
    OBD_NAMESPACE_KEYS,
};
use crate::telemetry::JsNumber;
use crate::telemetry_scope::TelemetryScopeReport;
use crate::types::{ApnState, SimRegistration, SimState};
use crate::types::{CameraSensorsState, CameraSensorsVerdict};

pub use crate::config_namespaces::{
    apply_class_of, ApplyClass, CONFIG_APPLY_CLASSES, CONFIG_NAMESPACE_KEYS,
};

/// `ReportOrigin` (`config-sync.ts:93`): why this report is being sent. The
/// cloud only distinguishes `local` (fold `changed` back into the desired
/// doc); `boot` covers the on-connect assertion AND the hourly heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportOrigin {
    Cloud,
    Local,
    Boot,
}

impl ReportOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportOrigin::Cloud => "cloud",
            ReportOrigin::Local => "local",
            ReportOrigin::Boot => "boot",
        }
    }
    /// `ORIGIN_RANK` (`config-sync.ts:1201`): `cloud` outranks `boot`, `local`
    /// outranks both, when several requests coalesce in one debounce window.
    pub fn rank(self) -> u8 {
        match self {
            ReportOrigin::Boot => 0,
            ReportOrigin::Cloud => 1,
            ReportOrigin::Local => 2,
        }
    }
}

/// `NamespaceState` (`config-sync.ts:96`). Cloud-side `pending` is COMPUTED, never reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamespaceState {
    Applied,
    Adjusted,
    Rejected,
}

/// `NamespaceStatus` (`config-sync.ts:98`). Key order = the TS object literals
/// (`{ state, at }`, `{ state, at, detail }`, `{ state, at, reason }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceStatus {
    pub state: NamespaceState,
    /// Epoch ms this verdict was determined (the report's own timestamp).
    pub at: i64,
    /// Why an `adjusted` value differs from what the cloud asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why a namespace was `rejected` outright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl NamespaceStatus {
    fn applied(at: i64) -> Self {
        Self {
            state: NamespaceState::Applied,
            at,
            detail: None,
            reason: None,
        }
    }
    fn adjusted(at: i64, detail: String) -> Self {
        Self {
            state: NamespaceState::Adjusted,
            at,
            detail: Some(detail),
            reason: None,
        }
    }
    fn rejected(at: i64, reason: String) -> Self {
        Self {
            state: NamespaceState::Rejected,
            at,
            detail: None,
            reason: Some(reason),
        }
    }
}

/// `ConfigReport` (`config-sync.ts:108`): the `…/config/reported` payload
/// `{v, deviceId, ts, rev, origin, effective, status, changed?}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReport {
    pub v: u8,
    pub device_id: String,
    pub ts: i64,
    /// Last applied desired rev; 0 = never received one.
    pub rev: u64,
    pub origin: ReportOrigin,
    #[serde(serialize_with = "js::json_map::serialize")]
    pub effective: Map<String, Value>,
    pub status: IndexMap<String, NamespaceStatus>,
    /// `origin: "local"` ONLY: the namespaces that moved since the cloud last
    /// knew, as settings.json now holds them (`null` = removed locally).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "js::json_map_opt::serialize"
    )]
    pub changed: Option<Map<String, Value>>,
}

/// `MAX_REPORT_BYTES` on the backend (`device-config-report.ts:51-90`): a
/// report larger than this is dropped cloud-side.
pub const MAX_REPORT_BYTES: usize = 32_768;

impl ConfigReport {
    /// The exact wire bytes (`JSON.stringify(report)`; every number in
    /// `effective` / `changed` printed like JS — [`js::JsJson`]).
    pub fn to_wire(&self) -> String {
        serde_json::to_string(self).expect("ConfigReport serialises")
    }
    /// Size of the wire form in bytes.
    pub fn wire_bytes(&self) -> usize {
        self.to_wire().len()
    }
    /// True when the backend would accept the report's size.
    pub fn within_cap(&self) -> bool {
        self.wire_bytes() <= MAX_REPORT_BYTES
    }
}

// --- sync state -------------------------------------------------------------

/// `ConfigSyncState` (`config-sync.ts:157`): the `config-sync.json` byte shape
/// `{lastAppliedRev, lastReportTs, settingsHash?, namespaces?, localPending?}`
/// (`atomicWriteJson` = compact `JSON.stringify`). Lives in its OWN file,
/// never inside settings.json (the allowlist rewriter would destroy it).
///
/// Key order is a JS object's: the TS only ever builds this with object
/// literals and `{ ...state, ...patch }` spreads, so an optional key sits
/// where it was FIRST set (`ensureConfigSyncBaseline` writes `namespaces`
/// before `settingsHash`; `loadConfigSyncState` reads them back in the
/// opposite order). `order` records that insertion order so the written bytes
/// match the TS for the same history; [`ConfigSyncState::merge`] is the spread.
/// `localPending: false` is a value, not an absence — the class persists it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConfigSyncState {
    /// Highest desired `rev` this device has applied. 0 = never received one.
    pub last_applied_rev: u64,
    /// Epoch ms of the last report we published (diagnostics only).
    pub last_report_ts: i64,
    /// sha256 of settings.json as of the last time we looked at it (Phase 4).
    pub settings_hash: Option<String>,
    /// The synced namespaces as the CLOUD is believed to know them. Absent =
    /// "never baselined" — must be INITIALISED from the file, never read as `{}`.
    pub namespaces: Option<Map<String, Value>>,
    /// A detected local change whose report has not gone out on a live link yet.
    pub local_pending: Option<bool>,
    /// Insertion order of the optional keys (JS object key order).
    order: Vec<SyncKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncKey {
    SettingsHash,
    Namespaces,
    LocalPending,
}

impl SyncKey {
    const ALL: [SyncKey; 3] = [
        SyncKey::SettingsHash,
        SyncKey::Namespaces,
        SyncKey::LocalPending,
    ];
    fn name(self) -> &'static str {
        match self {
            SyncKey::SettingsHash => "settingsHash",
            SyncKey::Namespaces => "namespaces",
            SyncKey::LocalPending => "localPending",
        }
    }
}

/// One key of a `Partial<ConfigSyncState>` patch (`persist(patch)`,
/// `config-sync.ts:1277`), in the patch literal's key order.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncPatch {
    LastAppliedRev(u64),
    LastReportTs(i64),
    SettingsHash(String),
    Namespaces(Map<String, Value>),
    LocalPending(bool),
}

impl ConfigSyncState {
    fn is_set(&self, k: SyncKey) -> bool {
        match k {
            SyncKey::SettingsHash => self.settings_hash.is_some(),
            SyncKey::Namespaces => self.namespaces.is_some(),
            SyncKey::LocalPending => self.local_pending.is_some(),
        }
    }

    fn touch(&mut self, k: SyncKey) {
        if !self.order.contains(&k) {
            self.order.push(k);
        }
    }

    /// `this.state = { ...this.state, ...patch }` (`config-sync.ts:1278`): an
    /// existing key keeps its position, a new one is appended in patch order.
    pub fn merge(&mut self, patch: impl IntoIterator<Item = SyncPatch>) {
        for p in patch {
            match p {
                SyncPatch::LastAppliedRev(v) => self.last_applied_rev = v,
                SyncPatch::LastReportTs(v) => self.last_report_ts = v,
                SyncPatch::SettingsHash(v) => {
                    self.touch(SyncKey::SettingsHash);
                    self.settings_hash = Some(v);
                }
                SyncPatch::Namespaces(v) => {
                    self.touch(SyncKey::Namespaces);
                    self.namespaces = Some(v);
                }
                SyncPatch::LocalPending(v) => {
                    self.touch(SyncKey::LocalPending);
                    self.local_pending = Some(v);
                }
            }
        }
    }

    /// Optional keys to write: recorded order first, then any field set
    /// directly (bypassing [`Self::merge`]) in the interface's order.
    fn key_order(&self) -> Vec<SyncKey> {
        let mut keys: Vec<SyncKey> = self
            .order
            .iter()
            .copied()
            .filter(|k| self.is_set(*k))
            .collect();
        for k in SyncKey::ALL {
            if self.is_set(k) && !keys.contains(&k) {
                keys.push(k);
            }
        }
        keys
    }

    /// The exact `config-sync.json` bytes `saveConfigSyncState` writes.
    pub fn to_file_string(&self) -> String {
        serde_json::to_string(self).expect("ConfigSyncState serialises")
    }
}

impl Serialize for ConfigSyncState {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let keys = self.key_order();
        let mut m = s.serialize_map(Some(2 + keys.len()))?;
        m.serialize_entry("lastAppliedRev", &self.last_applied_rev)?;
        m.serialize_entry("lastReportTs", &self.last_report_ts)?;
        for k in keys {
            match k {
                SyncKey::SettingsHash => m.serialize_entry(k.name(), &self.settings_hash)?,
                SyncKey::Namespaces => {
                    m.serialize_entry(k.name(), &self.namespaces.as_ref().map(js::JsJsonMap))?
                }
                SyncKey::LocalPending => m.serialize_entry(k.name(), &self.local_pending)?,
            }
        }
        m.end()
    }
}

/// Strict typed read of an in-memory state document (key order recorded).
/// The device's tolerant FILE read is [`parse_config_sync_state`].
impl<'de> Deserialize<'de> for ConfigSyncState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let src = Map::<String, Value>::deserialize(d)?;
        let mut out = ConfigSyncState {
            last_applied_rev: match src.get("lastAppliedRev") {
                None => 0,
                Some(v) => v
                    .as_u64()
                    .ok_or_else(|| D::Error::custom("lastAppliedRev"))?,
            },
            last_report_ts: match src.get("lastReportTs") {
                None => 0,
                Some(v) => v.as_i64().ok_or_else(|| D::Error::custom("lastReportTs"))?,
            },
            ..Default::default()
        };
        for (k, v) in &src {
            let p = match k.as_str() {
                "settingsHash" => SyncPatch::SettingsHash(
                    v.as_str()
                        .ok_or_else(|| D::Error::custom("settingsHash"))?
                        .to_string(),
                ),
                "namespaces" => SyncPatch::Namespaces(
                    v.as_object()
                        .ok_or_else(|| D::Error::custom("namespaces"))?
                        .clone(),
                ),
                "localPending" => SyncPatch::LocalPending(
                    v.as_bool()
                        .ok_or_else(|| D::Error::custom("localPending"))?,
                ),
                _ => continue,
            };
            out.merge([p]);
        }
        Ok(out)
    }
}

/// `Number.isFinite(v) && v > 0 ? Math.floor(v) : 0` (`config-sync.ts:210-211`).
fn positive_floor(v: Option<&Value>) -> f64 {
    match v.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n > 0.0 => n.floor(),
        _ => 0.0,
    }
}

/// The validating half of `loadConfigSyncState` (`config-sync.ts:201-229`)
/// over the file's text: zeros for a missing/corrupt/mistyped file (NEVER an
/// error — a lost file must degrade to "never applied anything"). Pass `None`
/// for an absent/unreadable file.
pub fn parse_config_sync_state(text: Option<&str>) -> ConfigSyncState {
    let Some(text) = text else {
        return ConfigSyncState::default();
    };
    let Ok(Value::Object(src)) = serde_json::from_str::<Value>(text).map(parsed_key_order) else {
        // `obj && typeof obj === "object"` — arrays pass in the TS but every
        // field then reads as undefined, which is the zero state anyway.
        return ConfigSyncState::default();
    };
    let mut out = ConfigSyncState {
        last_applied_rev: positive_floor(src.get("lastAppliedRev")) as u64,
        last_report_ts: positive_floor(src.get("lastReportTs")) as i64,
        ..Default::default()
    };
    // Phase 4 fields, each independently optional, set in the TS's order.
    if let Some(h) = src.get("settingsHash").and_then(Value::as_str) {
        if !h.is_empty() {
            out.merge([SyncPatch::SettingsHash(h.to_string())]);
        }
    }
    if let Some(Value::Object(ns)) = src.get("namespaces") {
        out.merge([SyncPatch::Namespaces(ns.clone())]);
    }
    if src.get("localPending") == Some(&Value::Bool(true)) {
        out.merge([SyncPatch::LocalPending(true)]);
    }
    out
}

/// `shouldApplyRev` (`config-sync.ts:257`): the rev guard. A retained
/// document is applied when NEWER than the last one applied; `rev` absent or
/// 0 = the backend predates Phase 2 → always apply.
pub fn should_apply_rev(rev: Option<u64>, last_applied_rev: u64) -> bool {
    match rev {
        None | Some(0) => true,
        Some(r) => r > last_applied_rev,
    }
}

// --- local-writer detection (Phase 4) ---------------------------------------

/// `namespacesOf` (`config-sync.ts:287`): the synced namespaces out of a
/// parsed settings object, in registry order — post-`loadSettings`, so it is
/// what the device WILL hold. `mode` is excluded (reported-only).
pub fn namespaces_of(settings: &AgentSettings) -> Map<String, Value> {
    let Value::Object(src) = serde_json::to_value(settings).expect("settings serialise") else {
        unreachable!("AgentSettings is an object");
    };
    let mut out = Map::new();
    for key in CONFIG_NAMESPACE_KEYS {
        if let Some(v) = src.get(*key) {
            out.insert((*key).to_string(), v.clone());
        }
    }
    out
}

/// `diffNamespaces` (`config-sync.ts:301`): namespaces that moved from `base`
/// to `next`; a namespace that disappeared maps to `null` (the same "revert
/// to device defaults" erase the cloud PATCH API uses).
pub fn diff_namespaces(base: &Map<String, Value>, next: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for key in CONFIG_NAMESPACE_KEYS {
        let before = base.get(*key);
        let after = next.get(*key);
        match (before, after) {
            (None, None) => {}
            (_, None) => {
                out.insert((*key).to_string(), Value::Null);
            }
            (Some(b), Some(a)) if deep_equal(b, a) => {}
            (_, Some(a)) => {
                out.insert((*key).to_string(), a.clone());
            }
        }
    }
    out
}

/// `unheldNamespaces` (`config-sync.ts:323-347` @ 09da38d): namespaces a
/// desired document carries that this device has NEVER held — absent from
/// settings.json AND absent from the sync baseline. Only an agent UPGRADE
/// produces that combination (the previous build's tolerant parser dropped
/// the key but still stamped the rev as applied). Never fires before a
/// baseline exists; `null` (the cloud's revert marker, `Some(None)`) is not
/// "carried". Registry order.
pub fn unheld_namespaces(
    parsed: &CloudConfig,
    settings: &AgentSettings,
    baseline: Option<&Map<String, Value>>,
) -> Vec<&'static str> {
    let Some(baseline) = baseline else {
        return Vec::new();
    };
    let held = namespaces_of(settings);
    CONFIG_NAMESPACE_KEYS
        .iter()
        .copied()
        .filter(|key| {
            carried_namespace(parsed, key)
                && !held.contains_key(*key)
                && !baseline.contains_key(*key)
        })
        .collect()
}

/// `doc[key] !== undefined && doc[key] !== null` on a parsed `CloudConfig`.
fn carried_namespace(parsed: &CloudConfig, key: &str) -> bool {
    match key {
        "gnssAntBias" => parsed.gnss_ant_bias.is_some(),
        "imuMount" => matches!(parsed.imu_mount, Some(Some(_))),
        "recording" => matches!(parsed.recording, Some(Some(_))),
        "lteRecovery" => matches!(parsed.lte_recovery, Some(Some(_))),
        "timeSync" => matches!(parsed.time_sync, Some(Some(_))),
        "cellular" => matches!(parsed.cellular, Some(Some(_))),
        "cameraMount" => matches!(parsed.camera_mount, Some(Some(_))),
        "simulation" => matches!(parsed.simulation, Some(Some(_))),
        "obd" => matches!(parsed.obd, Some(Some(_))),
        "cameraSensors" => matches!(parsed.camera_sensors, Some(Some(_))),
        _ => false,
    }
}

/// The adoption subset the worker applies under the rev guard's upgrade
/// exception (`worker.ts:948-956` @ 09da38d): `Object.fromEntries(adopt.map(
/// key => [key, parsed[key]]))` — ONLY those keys (no `rev`, no envelope),
/// in `adopt` order. Its JSON is the "cloud config applied:" summary.
pub fn adoption_subset(parsed: &CloudConfig, adopt: &[&str]) -> CloudConfig {
    let mut out = CloudConfig::default();
    for key in adopt {
        match *key {
            "gnssAntBias" => out.gnss_ant_bias = parsed.gnss_ant_bias,
            "imuMount" => out.imu_mount = parsed.imu_mount,
            "recording" => out.recording = parsed.recording.clone(),
            "lteRecovery" => out.lte_recovery = parsed.lte_recovery.clone(),
            "timeSync" => out.time_sync = parsed.time_sync.clone(),
            "cellular" => out.cellular = parsed.cellular.clone(),
            "cameraMount" => out.camera_mount = parsed.camera_mount.clone(),
            "simulation" => out.simulation = parsed.simulation.clone(),
            "obd" => out.obd = parsed.obd.clone(),
            "cameraSensors" => out.camera_sensors = parsed.camera_sensors.clone(),
            _ => {}
        }
    }
    out
}

/// `[agent] cloud config rev <rev> already applied, but it carries
/// namespace(s) this build never held: <keys> — adopting` (`worker.ts:953-955`
/// @ 09da38d). `rev` prints like the TS template (`undefined` cannot occur:
/// the guard only skips a positive rev).
pub fn unheld_adopt_log(rev: Option<u64>, adopt: &[&str]) -> String {
    let rev = rev.map_or_else(|| "undefined".to_string(), |r| r.to_string());
    format!(
        "[agent] cloud config rev {rev} already applied, but it carries namespace(s) this build never held: {} — adopting",
        adopt.join(", ")
    )
}

/// The decision inside `ensureConfigSyncBaseline` (`config-sync.ts:328-341`):
/// a sync state that has no namespace baseline adopts the current file
/// silently (`namespaces` + `settingsHash`); one that has is returned as-is.
/// The bool says whether the caller must persist the result.
pub fn ensure_config_sync_baseline(
    state: ConfigSyncState,
    settings: &AgentSettings,
    settings_hash: &str,
) -> (ConfigSyncState, bool) {
    if state.namespaces.is_some() {
        return (state, false);
    }
    // `{ ...state, namespaces, settingsHash }`
    let mut next = state;
    next.merge([
        SyncPatch::Namespaces(namespaces_of(settings)),
        SyncPatch::SettingsHash(settings_hash.to_string()),
    ]);
    (next, true)
}

/// The re-baseline inside `ConfigSync.noteSettingsWritten`
/// (`config-sync.ts:1335-1353`): after a write the WORKER performed, adopt the
/// file's value for exactly the namespaces that write could have touched,
/// keeping any other namespace's still-unreported local change. The caller
/// has already refused an unreadable/invalid read (`:1341`), so `current` is
/// `namespacesOf(settings)` (or `{}` for an absent file).
pub fn rebaseline_after_write(
    baseline: Option<&Map<String, Value>>,
    current: &Map<String, Value>,
    touched: &[&str],
) -> Map<String, Value> {
    let mut base = baseline.cloned().unwrap_or_else(|| current.clone());
    for key in touched {
        if !CONFIG_NAMESPACE_KEYS.contains(key) {
            continue;
        }
        match current.get(*key) {
            None => {
                base.shift_remove(*key);
            }
            Some(v) => {
                base.insert((*key).to_string(), v.clone());
            }
        }
    }
    base
}

/// The local-diff decision inside `ConfigSync.publishReport`
/// (`config-sync.ts:1522-1534`): computed at PUBLISH time so several edits
/// between two reports collapse into one statement. `None` = the edit was
/// reverted (or overwritten by a cloud apply) before the report went out —
/// the caller then downgrades the origin to `boot` and clears `localPending`.
pub fn local_report_changes(
    baseline: Option<&Map<String, Value>>,
    current: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    let changed = diff_namespaces(baseline.unwrap_or(current), current);
    (!changed.is_empty()).then_some(changed)
}

// --- effective values -------------------------------------------------------

/// The sanitized `sim` block as it reaches the wire (`parseSimTelemetry`
/// output, `config-sync.ts:426-447`): `state` first, then `present` (ALWAYS
/// emitted, `null` = the daemon could not tell), then the optional
/// identifiers in insertion order. The daemon-file INPUT shape is
/// [`crate::types::ModemSimTelemetry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimBlock {
    pub state: SimState,
    #[serde(default)]
    pub present: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iccid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msisdn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<SimRegistration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
}

/// The sanitized `cellular` (APN machinery) block (`parseApnTelemetry`
/// output, `config-sync.ts:472-484`). ABSENT downstream = no rollback net.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApnBlock {
    pub state: ApnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apn_applied: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apn_requested: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fails: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
}

/// The sanitized `uplink` block (`parseUplinkTelemetry` output,
/// `config-sync.ts:494`): both keys always emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UplinkBlock {
    pub primary: Option<String>,
    pub modem_only: u8,
}

/// `ModemBiasSample` (`config-sync.ts:353`): the APPLIED antenna-bias state
/// off the sim7600-lte daemon's telemetry file, plus the reported-only modem
/// facts riding the same snapshot. Key order = the TS object literals in
/// `readModemBias`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModemBiasSample {
    /// Applied bias, or null when the daemon has never reported one.
    pub value: Option<bool>,
    /// False when the telemetry file is missing, unparseable or stale.
    pub fresh: bool,
    /// Human-readable cause when `fresh` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim: Option<SimBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uplink: Option<UplinkBlock>,
    /// The daemon's APN apply machinery; ABSENT = predates the rollback net
    /// (or the snapshot is stale) — the cloud must then refuse to write `cellular`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apn: Option<ApnBlock>,
}

/// `MAX_REPORTED_STRING` (`config-sync.ts:380`): cap on a device-authored
/// string before it enters `reportedConfig`.
pub const MAX_REPORTED_STRING: usize = 64;

/// `reportedString` (`config-sync.ts:383`): a non-empty, length-capped
/// string, or None. Control chars (< 0x20, 0x7f) are dropped per code point,
/// then `trim().slice(0, 64)` (ECMAScript `trim`, see
/// [`crate::js::trim`]) — `slice` counts UTF-16 code units, so the cap
/// is applied on the UTF-16 form (a surrogate pair cut in half decodes to
/// U+FFFD here, where V8 would keep a lone surrogate).
pub fn reported_string(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    let clean: String = s
        .chars()
        .filter(|c| (*c as u32) >= 0x20 && (*c as u32) != 0x7f)
        .collect();
    let units: Vec<u16> = js_trim(&clean)
        .encode_utf16()
        .take(MAX_REPORTED_STRING)
        .collect();
    let out = String::from_utf16_lossy(&units);
    (!out.is_empty()).then_some(out)
}

/// `src.x === 1 || src.x === true` → 1, `=== 0 || === false` → 0, else None
/// (`config-sync.ts:427-431`).
fn one_zero(v: Option<&Value>) -> Option<u8> {
    match v {
        Some(Value::Bool(true)) => Some(1),
        Some(Value::Bool(false)) => Some(0),
        Some(Value::Number(n)) if n.as_f64() == Some(1.0) => Some(1),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => Some(0),
        _ => None,
    }
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `parseSimTelemetry` (`config-sync.ts:419`): sanitize the daemon's `sim`
/// block for the wire. FAIL OPEN: anything not a usable block (or a block
/// with no recognizable `state`) OMITS `sim` — never synthesise `present: 0`.
pub fn parse_sim_telemetry(raw: &Value) -> Option<SimBlock> {
    let src = raw.as_object()?;
    let state = SimState::parse(src.get("state").and_then(Value::as_str)?)?;
    let mut out = SimBlock {
        state,
        present: one_zero(src.get("present")),
        iccid: None,
        msisdn: None,
        carrier: None,
        act: None,
        registration: None,
        checked_at: None,
    };
    // Digits only: an ICCID or a phone number is the one place a rogue value
    // would be rendered verbatim to an operator as an identifier.
    if let Some(iccid) = reported_string(src.get("iccid").unwrap_or(&Value::Null)) {
        // /^[0-9]{10,22}$/
        if (10..=22).contains(&iccid.len()) && all_digits(&iccid) {
            out.iccid = Some(iccid);
        }
    }
    if let Some(msisdn) = reported_string(src.get("msisdn").unwrap_or(&Value::Null)) {
        // /^\+?[0-9]{3,20}$/
        let digits = msisdn.strip_prefix('+').unwrap_or(&msisdn);
        if (3..=20).contains(&digits.len()) && all_digits(digits) {
            out.msisdn = Some(msisdn);
        }
    }
    out.carrier = reported_string(src.get("carrier").unwrap_or(&Value::Null));
    out.act = reported_string(src.get("act").unwrap_or(&Value::Null));
    out.registration = src
        .get("registration")
        .and_then(Value::as_str)
        .and_then(SimRegistration::parse);
    out.checked_at = reported_string(src.get("checked_at").unwrap_or(&Value::Null));
    Some(out)
}

/// `apnField` (`config-sync.ts:458`): full value or nothing — deliberately NOT
/// `reportedString` (a truncated APN is a different APN). `/^[A-Za-z0-9.-]{1,100}$/`.
fn apn_field(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    let n = s.chars().count();
    ((1..=100).contains(&n)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
    .then(|| s.to_string())
}

/// `parseApnTelemetry` (`config-sync.ts:467`): same fail-open posture as the
/// SIM block; an unknown `state` drops the whole block.
pub fn parse_apn_telemetry(raw: &Value) -> Option<ApnBlock> {
    let src = raw.as_object()?;
    let state = ApnState::parse(src.get("state").and_then(Value::as_str)?)?;
    let fails = match src.get("fails").and_then(Value::as_f64) {
        Some(f) if f.is_finite() && f >= 0.0 => Some(f.floor().min(99.0) as u32),
        _ => None,
    };
    Some(ApnBlock {
        state,
        apn_applied: apn_field(src.get("apn_applied")),
        apn_requested: apn_field(src.get("apn_requested")),
        last_error: reported_string(src.get("last_error").unwrap_or(&Value::Null)),
        fails,
        changed_at: reported_string(src.get("changed_at").unwrap_or(&Value::Null)),
    })
}

/// `parseUplinkTelemetry` (`config-sync.ts:488`): None when nothing is usable
/// (`primary` unusable AND `modem_only` key absent).
pub fn parse_uplink_telemetry(raw: &Value) -> Option<UplinkBlock> {
    let src = raw.as_object()?;
    let primary = reported_string(src.get("primary").unwrap_or(&Value::Null));
    let modem_only = match one_zero(src.get("modem_only")) {
        Some(1) => 1,
        _ => 0,
    };
    if primary.is_none() && !src.contains_key("modem_only") {
        return None;
    }
    Some(UplinkBlock {
        primary,
        modem_only,
    })
}

/// `MODEM_TELEMETRY_STALE_MS` (`config-sync.ts:503`): six stable CSQ cycles.
pub const MODEM_TELEMETRY_STALE_MS: i64 = 180_000;

/// The decision inside `readModemBias` (`config-sync.ts:505-538`) over the
/// telemetry snapshot the caller already read (`None` = file absent /
/// unreadable / unparseable, exactly `readModemTelemetry`'s null).
pub fn modem_bias_from_telemetry(telemetry: Option<&Value>, now: i64) -> ModemBiasSample {
    let telemetry = match telemetry {
        None | Some(Value::Null) => {
            return ModemBiasSample {
                value: None,
                fresh: false,
                detail: Some("modem telemetry unavailable".into()),
                ..Default::default()
            }
        }
        Some(t) => t,
    };
    let get = |k: &str| telemetry.get(k);
    let value = one_zero(get("gnss_ant_bias")).map(|v| v == 1);
    // `Date.parse(`${String(telemetry.timestamp ?? "")}`)`
    let stamped_ms = match get("timestamp") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => date_parse_ms(s),
        Some(other) => date_parse_ms(&other.to_string()),
    };
    if stamped_ms.is_none_or(|t| now - t > MODEM_TELEMETRY_STALE_MS) {
        // A stale snapshot takes the reported-only blocks down with it.
        return ModemBiasSample {
            value,
            fresh: false,
            detail: Some("modem telemetry is stale".into()),
            ..Default::default()
        };
    }
    let sim = get("sim").and_then(parse_sim_telemetry);
    let uplink = get("uplink").and_then(parse_uplink_telemetry);
    let apn = get("cellular").and_then(parse_apn_telemetry);
    if value.is_none() {
        // Unfresh for the BIAS only — the snapshot itself is current.
        return ModemBiasSample {
            value,
            fresh: false,
            detail: Some("modem has not applied a bias state yet".into()),
            sim,
            uplink,
            apn,
        };
    }
    ModemBiasSample {
        value,
        fresh: true,
        detail: None,
        sim,
        uplink,
        apn,
    }
}

/// `ReportHints` (`config-sync.ts:546`): read-only hints about latched boot
/// state (each supplied by an object that already publishes it).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportHints {
    /// `RecordingManager.status().bootDefer` — "deferring" / "gating" / …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_defer: Option<String>,
    /// `RecordingManager.status().powerShed` — the current shed rung, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_shed: Option<String>,
    /// `ClockStepper.status().armed` — false once NTP synced this boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_stepper_armed: Option<bool>,
    /// `ClockTrust.status().state` (`config-sync.ts:586-591` @ 09da38d):
    /// "held" while the live publish and the recorder wait for a trusted
    /// wall clock, "fallback" past the 5 min cap, "trusted". `None` in sim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_trust: Option<ClockTrustState>,
}

/// `AGENT_CAPABILITIES` (`config-sync.ts:631-639` @ 09da38d): optional
/// features this build advertises as `effective.agent.capabilities` on
/// EVERY device; the cloud gates commands on them: `sms.read` = `read_sms`
/// and its `…/sms` result, `sms.clear` = `clear_sms`, `obd.scan` and
/// `obd.forget` = the two OBD one-shots (which still need the model's
/// `peripherals.obd`; the cloud gates on that, not here).
///
/// `camera.sensors` (RUST-ONLY, support-other-camera-models Phase 2): this
/// build acts on the `cameraSensors` namespace. The cloud refuses the
/// namespace for an agent that does not advertise it, and WITHDRAWS a
/// stored value on the first report whose capability list lacks it. So the
/// list is this one compile-time constant, emitted whole in every report
/// that has an `agent` block (and by the provisioner in its claim body):
/// nothing may build it conditionally.
pub const AGENT_CAPABILITIES: &[&str] = &[
    "sms.read",
    "sms.clear",
    "obd.scan",
    "obd.forget",
    CAPABILITY_CAMERA_SENSORS,
];

/// The capability that says "this agent manages camera sensors".
pub const CAPABILITY_CAMERA_SENSORS: &str = "camera.sensors";

/// D85 (Phase 8 D9, Q-8.3; `phase-1/evidence/accepted-divergences.md`): the
/// Rust agent's own report marker, `effective.agent.runtime`, the LAST key of
/// the `agent` block (after `capabilities`). The JS agent @ 09da38d sends no
/// such key, so the cloud could not tell the two agents apart from a report
/// (Phase 8 plan-notes X6). The backend mirrors it POSITIVE-ONLY: `"rust"` →
/// `devices.agent = 'rust'`; an absent key leaves the column untouched (it
/// never demotes). An Appendix A amendment: the cloud stores `effective` as an
/// opaque `z.record(z.string(), z.unknown())`, so the extra key is tolerated.
pub const AGENT_RUNTIME: &str = "rust";

/// `buildEffective` (`config-sync.ts:597`): what settings.json actually holds
/// right now, with `gnssAntBias` / `cellular.apn` / `cameraMount` overridden
/// by what is IN FORCE, plus the reported-only blocks (`agent`,
/// `telemetryScope`, `sim`, `uplink`, `apn`). An ABSENT namespace means
/// "device defaults". `build` replaces the TS `loadBuildInfo()` call (`:639`).
/// `obd` is the OBD collector's report (I2: opaque, already in the TS key
/// order), present only with the peripheral (`config-sync.ts:699-709` @ 09da38d).
///
/// `cameras` is the per-port camera report ([`crate::camera_truth`];
/// support-other-camera-models Phase 0, RUST-ONLY, the JS agent sends no
/// such key): reported-only like `sim` / `uplink` / `apn`, appended as the
/// LAST key so the JS key order is kept, and never under a namespace key
/// (the console would then compute "Device differs" for it). Absent without
/// a hardware block or before the first hardware reconcile.
#[allow(clippy::too_many_arguments)]
pub fn build_effective(
    settings: &AgentSettings,
    modem: &ModemBiasSample,
    mode: Option<&str>,
    camera_mount: Option<&CameraMountState>,
    telemetry_scope: Option<&TelemetryScopeReport>,
    obd: Option<&Value>,
    cameras: Option<&Value>,
    camera_sensors: Option<&CameraSensorsState>,
    build: &BuildInfo,
) -> Map<String, Value> {
    let to = |v: &dyn erased::Ser| v.to_json();
    let mut out = Map::new();
    // The APPLIED bias, not the requested one; omitted when the modem has not told us.
    if let Some(v) = modem.value {
        out.insert("gnssAntBias".into(), Value::Bool(v));
    }
    if let Some(v) = &settings.imu_mount {
        out.insert("imuMount".into(), to(v));
    }
    if let Some(v) = &settings.recording {
        out.insert("recording".into(), to(v));
    }
    if let Some(v) = &settings.lte_recovery {
        out.insert("lteRecovery".into(), to(v));
    }
    if let Some(v) = &settings.time_sync {
        out.insert("timeSync".into(), to(v));
    }
    // `simulation` is held exactly as delivered.
    if let Some(v) = &settings.simulation {
        out.insert("simulation".into(), to(v));
    }
    if let Some(c) = effective_cellular(settings.cellular.as_ref(), modem.apn.as_ref()) {
        out.insert("cellular".into(), Value::Object(c));
    }
    if let Some(c) = effective_camera_mount(settings.camera_mount.as_ref(), camera_mount) {
        out.insert("cameraMount".into(), Value::Object(c));
    }
    // `build` rides alongside `mode`: reported-only, never a `status` entry.
    if let Some(mode) = mode {
        let mut agent = Map::new();
        agent.insert("mode".into(), Value::String(mode.to_string()));
        agent.insert("build".into(), to(build));
        agent.insert(
            "capabilities".into(),
            Value::Array(
                AGENT_CAPABILITIES
                    .iter()
                    .map(|c| Value::String((*c).to_string()))
                    .collect(),
            ),
        );
        // D85: Rust-only, after every JS key, so the JS key order is kept.
        agent.insert("runtime".into(), Value::String(AGENT_RUNTIME.to_string()));
        out.insert("agent".into(), Value::Object(agent));
    }
    if let Some(ts) = telemetry_scope {
        out.insert("telemetryScope".into(), to(ts));
    }
    if let Some(sim) = &modem.sim {
        out.insert("sim".into(), to(sim));
    }
    if let Some(uplink) = &modem.uplink {
        out.insert("uplink".into(), to(uplink));
    }
    if let Some(apn) = &modem.apn {
        out.insert("apn".into(), to(apn));
    }
    // `if (obd) out.obd = obd;` — the collector's report; the local-edit
    // fold-back reads settings.json, never this block.
    if let Some(obd) = obd.filter(|v| !v.is_null()) {
        out.insert("obd".into(), obd.clone());
    }
    // Rust-only, after every JS key ([`EFFECTIVE_CAMERAS_KEY`]).
    if let Some(cameras) = cameras.filter(|v| !v.is_null()) {
        out.insert(EFFECTIVE_CAMERAS_KEY.into(), cameras.clone());
    }
    // Rust-only namespace, after every other key; absent while not held.
    if let Some(c) = effective_camera_sensors(settings.camera_sensors.as_ref(), camera_sensors) {
        out.insert("cameraSensors".into(), Value::Object(c));
    }
    out
}

/// `effective.cameraSensors` (RUST-ONLY): absent → None; otherwise the
/// namespace AS HELD (each entry as delivered), plus `reconciled: false`
/// while the camera reconciler has not run, or `rebootRequired` once it
/// has. What is actually on a port is the per-port report's business
/// (`effective.cameras`), not this block's.
pub fn effective_camera_sensors(
    settings: Option<&CameraSensorsSettings>,
    state: Option<&CameraSensorsState>,
) -> Option<Map<String, Value>> {
    let settings = settings?;
    let Value::Object(mut out) = serde_json::to_value(settings).expect("cameraSensors") else {
        unreachable!()
    };
    match camera_sensors_state_for(state, Some(settings)) {
        None => out.insert("reconciled".into(), Value::Bool(false)),
        Some(s) => out.insert("rebootRequired".into(), Value::Bool(s.reboot_required)),
    };
    Some(out)
}

/// The reconciler's state, when it was computed from the value held NOW
/// (`CameraSensorsState::settings`). A pass that looked at another value
/// says nothing about this one: `None`, like "not reconciled yet".
pub fn camera_sensors_state_for<'a>(
    state: Option<&'a CameraSensorsState>,
    held: Option<&CameraSensorsSettings>,
) -> Option<&'a CameraSensorsState> {
    let now = serde_json::to_string(&held).expect("cameraSensors serialises");
    state.filter(|s| s.settings == now)
}

/// THE BACKEND'S BOUND on a status entry's `detail` and `reason`
/// (`z.string().max(500)`, `device-config-report.ts:69-71`; JavaScript
/// string length, i.e. UTF-16 units). A longer value fails the report
/// schema and the backend DROPS THE WHOLE REPORT: the cloud then shows the
/// previous one for ever. No status text of any namespace may exceed it
/// ([`build_status`] ends with [`fit_status_text`] over every entry; the
/// two camera namespaces join their ports with [`join_status_parts`]).
/// RUST-ONLY (D117): the TS has no such bound.
pub const MAX_STATUS_TEXT: usize = 500;

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `text` within `max` UTF-16 units: unchanged when it fits. Otherwise cut
/// at a sentence boundary (after the last `. ` that fits, else after the
/// last `; `), else at a clause boundary (`: `, `, `, ` — `, ` (`), else at
/// a word boundary, never inside a word, and closed with an ellipsis.
pub fn fit_text(text: &str, max: usize) -> String {
    if utf16_len(text) <= max {
        return text.to_string();
    }
    if max < 2 {
        return "…".chars().take(max).collect();
    }
    // The longest prefix that leaves room for " …" (2 units).
    let mut used = 0;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        if used + c.len_utf16() > max - 2 {
            break;
        }
        used += c.len_utf16();
        end = i + c.len_utf8();
    }
    let head = &text[..end];
    // A cut that keeps less than a third of the room is not worth its
    // boundary: fall through to the next kind.
    let worth = |cut: usize| cut >= end / 3;
    // A clause boundary must keep at least half: it leaves a sentence
    // open, which reads better than a cut two words further on only when
    // not much is lost for it.
    let clause = [(": ", 1), (", ", 0), (" — ", 0), (" (", 0)]
        .iter()
        .filter_map(|(mark, keep)| head.rfind(mark).map(|i| i + keep))
        .max()
        .filter(|c| *c >= end / 2);
    let cut = head
        .rfind(". ")
        .map(|i| i + 1)
        .filter(|c| worth(*c))
        .or_else(|| head.rfind("; ").filter(|c| worth(*c)))
        .or(clause)
        .or_else(|| head.rfind(' '))
        .unwrap_or(end);
    format!("{} …", head[..cut].trim_end())
}

/// The widths long quoted parts are clipped to, tried in order, before any
/// sentence is cut ([`clip_long_words`]).
const LONG_WORD_WIDTHS: [usize; 2] = [40, 28];

/// `text` with every word longer than `max` characters clipped to `max`
/// (an ellipsis inside, the closing quote, bracket and punctuation kept):
/// a path, a section name or a token someone made very long is what costs
/// the room, and three quarters of it say nothing the first quarter did
/// not. Words within `max` are untouched.
pub fn clip_long_words(text: &str, max: usize) -> String {
    text.split(' ')
        .map(|w| {
            if w.chars().count() <= max {
                return w.to_string();
            }
            let closing: Vec<char> = w
                .chars()
                .rev()
                .take_while(|c| matches!(c, '"' | '\'' | '`' | ']' | ')' | ',' | '.' | ';' | ':'))
                .collect();
            let keep = max.saturating_sub(1 + closing.len()).max(8);
            let head: String = w.chars().take(keep).collect();
            let tail: String = closing.into_iter().rev().collect();
            format!("{head}…{tail}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`fit_text`] at the backend's bound; long quoted parts are shortened
/// first ([`clip_long_words`]), a sentence is cut only when that is not
/// enough.
pub fn fit_status_text(text: &str) -> String {
    if utf16_len(text) <= MAX_STATUS_TEXT {
        return text.to_string();
    }
    let mut clipped = text.to_string();
    for width in LONG_WORD_WIDTHS {
        clipped = clip_long_words(text, width);
        if utf16_len(&clipped) <= MAX_STATUS_TEXT {
            return clipped;
        }
    }
    fit_text(&clipped, MAX_STATUS_TEXT)
}

/// Room for `lens` within `room`: everyone who fits in an equal share gets
/// what it needs, what they leave is shared among the others.
fn fair_shares(lens: &[usize], room: usize) -> Vec<usize> {
    let mut allot = vec![0; lens.len()];
    let mut open: Vec<usize> = (0..lens.len()).collect();
    let mut left = room;
    while !open.is_empty() {
        let share = left / open.len();
        let (fit, over): (Vec<usize>, Vec<usize>) = open.iter().partition(|i| lens[**i] <= share);
        if fit.is_empty() {
            for i in over {
                allot[i] = share;
            }
            break;
        }
        for i in fit {
            allot[i] = lens[i];
            left -= lens[i];
        }
        open = over;
    }
    allot
}

/// Several ports' sentences as ONE status text within the backend's bound:
/// `a; b; c` exactly as before when it fits. When it does not, in order:
/// 1. long quoted parts (a path, a section name, a token) are clipped
///    ([`clip_long_words`]): nothing else changes, every sentence is whole;
/// 2. the FIRST part is kept whole when that leaves every other part what
///    it needs or 150 characters; otherwise the room is shared fairly
///    ([`fair_shares`]), so the LAST port's sentence is not the one that
///    pays for the others. A part that does not fit its share is cut at a
///    sentence boundary, else at a clause ([`fit_text`]);
/// 3. with so many ports that a share would be under 60 characters, the
///    ports for which no room is left are named in a closing
///    `; and <key>, <key>`.
///
/// `tail` (a closing clause such as "written … waiting for a reboot: …")
/// is kept when it fits and shortened or dropped when it does not.
pub fn join_status_parts(parts: &[(String, String)], tail: Option<&str>) -> String {
    let join = |parts: &[(String, String)], tail: Option<&str>| -> String {
        parts
            .iter()
            .map(|(key, text)| format!("{key}: {text}"))
            .chain(tail.map(str::to_string))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let whole = join(parts, tail);
    if utf16_len(&whole) <= MAX_STATUS_TEXT {
        return whole;
    }
    // 1. The long quoted parts.
    let mut compact: Vec<(String, String)> = parts.to_vec();
    for width in LONG_WORD_WIDTHS {
        compact = parts
            .iter()
            .map(|(k, t)| (k.clone(), clip_long_words(t, width)))
            .collect();
        let text = join(&compact, tail);
        if utf16_len(&text) <= MAX_STATUS_TEXT {
            return text;
        }
    }
    // 2. A share each.
    const MIN_SHARE: usize = 60;
    const FIRST_WHOLE_LEAVES: usize = 150;
    const TAIL_ROOM: usize = 100;
    let tail_len = tail.map_or(0, |t| (utf16_len(t) + 2).min(TAIL_ROOM));
    let budget = MAX_STATUS_TEXT.saturating_sub(tail_len);
    // 3. Too many ports for a readable share each: the first ones speak.
    let spoken = if compact.len() * (MIN_SHARE + 2) <= budget {
        compact.len()
    } else {
        (budget.saturating_sub(48) / (MIN_SHARE + 2)).max(1)
    };
    let (speak, named) = compact.split_at(spoken.min(compact.len()));
    let closing = (!named.is_empty()).then(|| {
        format!(
            "; and {}",
            named
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    let closing_len = closing.as_deref().map_or(0, utf16_len).min(budget / 4);
    let pieces: Vec<String> = speak.iter().map(|(k, t)| format!("{k}: {t}")).collect();
    let lens: Vec<usize> = pieces.iter().map(|p| utf16_len(p)).collect();
    let room = budget
        .saturating_sub(closing_len)
        .saturating_sub(2 * pieces.len().saturating_sub(1));
    let mut allot = fair_shares(&lens, room);
    if lens.len() > 1 && lens[0] <= room {
        let rest = fair_shares(&lens[1..], room - lens[0]);
        if rest
            .iter()
            .zip(&lens[1..])
            .all(|(a, l)| *a >= (*l).min(FIRST_WHOLE_LEAVES))
        {
            allot = std::iter::once(lens[0]).chain(rest).collect();
        }
    }
    let mut out = pieces
        .iter()
        .zip(&allot)
        .map(|(p, a)| fit_text(p, *a))
        .collect::<Vec<_>>()
        .join("; ");
    // A closing clause, when it still fits.
    fn close(out: &mut String, clause: String) {
        if utf16_len(out) + utf16_len(&clause) <= MAX_STATUS_TEXT {
            out.push_str(&clause);
        }
    }
    if let Some(closing) = closing {
        close(&mut out, closing);
    }
    if let Some(tail) = tail {
        let room = MAX_STATUS_TEXT.saturating_sub(utf16_len(&out) + 2);
        if room >= 24 {
            close(&mut out, format!("; {}", fit_text(tail, room)));
        }
    }
    out
}

/// The `cameraSensors` verdict off the camera reconciler's last pass,
/// worst first (like [`camera_mount_status`]): a port that could not be
/// applied makes the namespace `rejected`, with every such port named (and
/// the ports that wait for a reboot named after them); a port whose line
/// differs from the booted one makes it `adjusted`: "saved, not in force
/// yet". Who changed the line is said: "written to the boot config" only
/// for a line the agent wrote in this boot (`PendingReboot`); a line
/// something else changed (`ChangedOutside`: a hand edit, an installer) is
/// not claimed.
pub fn camera_sensors_status(
    state: Option<&CameraSensorsState>,
    held: Option<&CameraSensorsSettings>,
    at: i64,
) -> NamespaceStatus {
    // Only a pass that looked at the value held now may speak for it.
    let Some(state) = camera_sensors_state_for(state, held) else {
        return NamespaceStatus::adjusted(
            at,
            "saved; the device has not reconciled its boot config yet".into(),
        );
    };
    let named = |verdict: CameraSensorsVerdict| -> Vec<String> {
        state
            .cameras
            .iter()
            .filter(|c| c.verdict == verdict)
            .map(|c| match c.sensor.as_deref() {
                Some(s) => format!("{} {s}", c.stream_key),
                None => c.stream_key.clone(),
            })
            .collect()
    };
    let pending = named(CameraSensorsVerdict::PendingReboot);
    let outside = named(CameraSensorsVerdict::ChangedOutside);
    let blocked: Vec<(String, String)> = state
        .cameras
        .iter()
        .filter(|c| {
            matches!(
                c.verdict,
                CameraSensorsVerdict::Rejected | CameraSensorsVerdict::WriteFailed
            )
        })
        .map(|c| {
            (
                c.stream_key.clone(),
                c.detail
                    .clone()
                    .unwrap_or_else(|| c.verdict.as_str().to_string()),
            )
        })
        .collect();
    if !blocked.is_empty() {
        let mut waiting: Vec<String> = Vec::new();
        if !pending.is_empty() {
            waiting.push(format!(
                "written to the boot config and waiting for a reboot: {}",
                pending.join(", ")
            ));
        }
        if !outside.is_empty() {
            waiting.push(format!(
                "changed outside the agent since this boot and waiting for a reboot: {}",
                outside.join(", ")
            ));
        }
        let waiting = (!waiting.is_empty()).then(|| waiting.join("; "));
        return NamespaceStatus::rejected(at, join_status_parts(&blocked, waiting.as_deref()));
    }
    let mut waiting: Vec<String> = Vec::new();
    if !pending.is_empty() {
        waiting.push(format!(
            "written to the boot config — REBOOT REQUIRED to apply: {}",
            pending.join(", ")
        ));
    }
    if !outside.is_empty() {
        waiting.push(format!(
            "the camera line was changed outside the agent since this boot (not written by this setting) — REBOOT REQUIRED to apply: {}",
            outside.join(", ")
        ));
    }
    if !waiting.is_empty() {
        return NamespaceStatus::adjusted(at, fit_status_text(&waiting.join("; ")));
    }
    NamespaceStatus::applied(at)
}

/// The reported-only key of the per-port camera report in `effective`
/// (Rust-only; see [`build_effective`]). It must never be a config
/// namespace key: `config_namespaces::CONFIG_APPLY_CLASSES` has no entry of
/// this name (pinned by a test below).
pub const EFFECTIVE_CAMERAS_KEY: &str = "cameras";

/// Object-safe "serialize to a JSON value" so [`build_effective`] can take
/// every namespace through one closure.
mod erased {
    use serde_json::Value;
    pub trait Ser {
        fn to_json(&self) -> Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn to_json(&self) -> Value {
            serde_json::to_value(self).expect("namespace serialises")
        }
    }
}

/// `effectiveCellular` (`config-sync.ts:668`): absent → None; empty apn →
/// echoed as-is ("don't touch" is trivially in force); non-empty → the
/// modem's APPLIED APN substituted in, or the whole key omitted when the
/// daemon has never reported one.
pub fn effective_cellular(
    settings: Option<&CellularSettings>,
    apn: Option<&ApnBlock>,
) -> Option<Map<String, Value>> {
    let settings = settings?;
    let Value::Object(mut out) = serde_json::to_value(settings).expect("cellular") else {
        unreachable!()
    };
    let desired = settings.apn.clone().unwrap_or_default();
    if desired.is_empty() {
        return Some(out);
    }
    let applied = apn?.apn_applied.clone()?;
    out.insert("apn".into(), Value::String(applied));
    Some(out)
}

/// `effectiveCameraMount` (`config-sync.ts:694`): absent → None; reconciler
/// not run yet → the namespace as held + `reconciled: false`; otherwise
/// `cameras` = the rotation each managed camera is ACTUALLY running (omitted
/// when unreadable — never defaulted to 0), `pending` = written and waiting
/// for a reboot, `rebootRequired`.
pub fn effective_camera_mount(
    settings: Option<&CameraMountSettings>,
    state: Option<&CameraMountState>,
) -> Option<Map<String, Value>> {
    let settings = settings?;
    let Some(state) = state else {
        let Value::Object(mut out) = serde_json::to_value(settings).expect("cameraMount") else {
            unreachable!()
        };
        out.insert("reconciled".into(), Value::Bool(false));
        return Some(out);
    };
    let rot = |deg: f64| {
        let mut m = Map::new();
        m.insert(
            "rotationDeg".into(),
            serde_json::to_value(JsNumber(deg)).unwrap(),
        );
        Value::Object(m)
    };
    // Built as JS object literals (`config-sync.ts:700-707`), so V8 own-key
    // order applies: integer-like stream keys ("9", "10") enumerate first in
    // ascending order, then the rest in insertion (= reconciler) order.
    let mut cameras: IndexMap<String, Value> = IndexMap::new();
    let mut pending: IndexMap<String, Value> = IndexMap::new();
    for cam in &state.cameras {
        let desired = cam.desired_deg.degrees() as f64;
        if let Some(in_force) = cam.in_force_deg {
            cameras.insert(cam.stream_key.clone(), rot(in_force));
        }
        // RUST-ONLY: a port where no camera answers (`NoCamera`) has
        // nothing waiting for a reboot.
        if cam.in_force_deg != Some(desired)
            && cam.boot_deg == Some(desired)
            && cam.verdict != CameraMountVerdict::NoCamera
        {
            pending.insert(cam.stream_key.clone(), rot(desired));
        }
    }
    let cameras: Map<String, Value> = object_key_order(cameras).into_iter().collect();
    let pending: Map<String, Value> = object_key_order(pending).into_iter().collect();
    let mut out = Map::new();
    out.insert("cameras".into(), Value::Object(cameras));
    if !pending.is_empty() {
        out.insert("pending".into(), Value::Object(pending));
    }
    out.insert("rebootRequired".into(), Value::Bool(state.reboot_required));
    Some(out)
}

// --- status -----------------------------------------------------------------

/// `isEmptyObject` (`config-sync.ts:718`): `{}` — the cloud's on-the-wire
/// marker for "revert this namespace".
fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(Map::is_empty)
}

/// `deepEqual` (`config-sync.ts:725`): key-order-insensitive structural
/// equality with JS number semantics (`2.0 === 2`).
pub fn deep_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| deep_equal(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| deep_equal(v, w)))
        }
        _ => a == b,
    }
}

/// `DesiredSnapshot` (`config-sync.ts:747`): the last desired document this
/// session saw on the config topic (in memory only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesiredSnapshot {
    /// The message's `rev` (0 when the backend predates Phase 2).
    pub rev: u64,
    /// The raw namespaces as they arrived, BEFORE the device's tolerant parse.
    #[serde(serialize_with = "js::json_map::serialize")]
    pub raw: Map<String, Value>,
    /// The same message after `parseCloudConfig` — i.e. post-clamp.
    pub parsed: CloudConfig,
}

/// `parsedNamespace` (`config-sync.ts:757`) as the TS triple: `None` =
/// undefined (dropped outright), `Some(None)` = null (parsed to nothing),
/// `Some(Some(v))` = the post-parse value as JSON.
fn parsed_namespace(parsed: &CloudConfig, key: &str) -> Option<Option<Value>> {
    fn triple<T: Serialize>(v: &Option<Option<T>>) -> Option<Option<Value>> {
        v.as_ref()
            .map(|inner| inner.as_ref().map(|x| serde_json::to_value(x).unwrap()))
    }
    match key {
        "gnssAntBias" => parsed.gnss_ant_bias.map(|b| Some(Value::Bool(b))),
        "imuMount" => triple(&parsed.imu_mount),
        "recording" => triple(&parsed.recording),
        "lteRecovery" => triple(&parsed.lte_recovery),
        "timeSync" => triple(&parsed.time_sync),
        "cellular" => triple(&parsed.cellular),
        "cameraMount" => triple(&parsed.camera_mount),
        "simulation" => triple(&parsed.simulation),
        "obd" => triple(&parsed.obd),
        "cameraSensors" => triple(&parsed.camera_sensors),
        _ => None,
    }
}

/// `buildStatus` (`config-sync.ts:777`): per-namespace verdicts for every
/// namespace the cloud actually spoke about. Order of tests:
///   0. `{}` revert marker                         -> applied (or adjusted if still held)
///   1. the tolerant parser dropped it entirely    -> rejected
///   2. the parser changed it (clamp / unknown)    -> adjusted
///   3. cameraMount / cellular                     -> their own machinery's verdict
///   4. what we hold differs from what was asked   -> adjusted
///   5. otherwise                                  -> applied (+ boot-latch detail)
#[allow(clippy::too_many_arguments)]
pub fn build_status(
    desired: Option<&DesiredSnapshot>,
    effective: &Map<String, Value>,
    modem: &ModemBiasSample,
    at: i64,
    hints: &ReportHints,
    camera_mount: Option<&CameraMountState>,
    settings: Option<&AgentSettings>,
    camera_sensors: Option<&CameraSensorsState>,
) -> IndexMap<String, NamespaceStatus> {
    let mut out = IndexMap::new();
    let Some(desired) = desired else {
        return out;
    };
    for key in CONFIG_NAMESPACE_KEYS {
        let Some(raw) = desired.raw.get(*key) else {
            continue;
        };
        let parsed = parsed_namespace(&desired.parsed, key);
        let key_s = (*key).to_string();

        if parsed == Some(None) && is_empty_object(raw) {
            // The cloud's "revert to device defaults" marker: the apply dropped
            // the local key — that is the revert SUCCEEDING. `effective.obd`
            // is the collector's report and exists whether or not the
            // namespace is held, so for obd "still holding" means
            // settings.json (`config-sync.ts:857-860` @ 09da38d; CF74).
            let still_held = if *key == "obd" {
                settings.is_some_and(|s| s.obd.is_some())
            } else if *key == "cameraSensors" {
                // `effective.cameraSensors` is the namespace as held.
                settings.is_some_and(|s| s.camera_sensors.is_some())
            } else {
                effective.get(*key).is_some()
            };
            let st = if !still_held {
                NamespaceStatus::applied(at)
            } else {
                NamespaceStatus::adjusted(
                    at,
                    "asked to revert to defaults, but the device is still holding a value".into(),
                )
            };
            out.insert(key_s, st);
            continue;
        }

        let parsed = match parsed {
            Some(Some(v)) => v,
            // null = arrived as a non-empty object the parser emptied;
            // undefined = arrived as the wrong type outright.
            Some(None) => {
                out.insert(
                    key_s,
                    NamespaceStatus::rejected(
                        at,
                        format!("{key}: no valid fields in the delivered value"),
                    ),
                );
                continue;
            }
            None => {
                let what = if *key == "gnssAntBias" {
                    "boolean"
                } else {
                    "object"
                };
                out.insert(
                    key_s,
                    NamespaceStatus::rejected(at, format!("{key}: not a valid {what}")),
                );
                continue;
            }
        };

        if !deep_equal(raw, &parsed) {
            out.insert(
                key_s,
                NamespaceStatus::adjusted(
                    at,
                    "the device clamped or normalized the delivered value".into(),
                ),
            );
            continue;
        }

        if *key == "cameraMount" {
            out.insert(key_s, camera_mount_status(camera_mount, at));
            continue;
        }

        if *key == "cameraSensors" {
            out.insert(
                key_s,
                camera_sensors_status(
                    camera_sensors,
                    settings.and_then(|s| s.camera_sensors.as_ref()),
                    at,
                ),
            );
            continue;
        }

        if *key == "obd" {
            let desired_obd: ObdSettings = serde_json::from_value(parsed).expect("parsed obd");
            out.insert(
                key_s,
                obd_status(
                    &desired_obd,
                    settings.and_then(|s| s.obd.as_ref()),
                    effective.get("obd"),
                    at,
                ),
            );
            continue;
        }

        if *key == "cellular" {
            let desired_cell: CellularSettings =
                serde_json::from_value(parsed).expect("parsed cellular");
            out.insert(
                key_s,
                cellular_status(&desired_cell, modem.apn.as_ref(), at),
            );
            continue;
        }

        let in_force = effective.get(*key);
        if !in_force.is_some_and(|v| deep_equal(v, &parsed)) {
            out.insert(
                key_s,
                NamespaceStatus::adjusted(at, divergence_detail(key, modem, in_force)),
            );
            continue;
        }

        let mut st = NamespaceStatus::applied(at);
        st.detail = latch_detail(key, hints);
        out.insert(key_s, st);
    }
    // The backend's bound, for every namespace (D117): one text over it
    // would cost the whole report. A text within it is left byte for byte.
    for st in out.values_mut() {
        if let Some(d) = st.detail.as_mut() {
            *d = fit_status_text(d);
        }
        if let Some(r) = st.reason.as_mut() {
            *r = fit_status_text(r);
        }
    }
    out
}

/// `rejected` reason when no OBD collector reports (`config-sync.ts:1024-1027` @ 09da38d).
pub const OBD_NO_PERIPHERAL_REASON: &str = "obd: this device's hardware block declares no OBD peripheral (peripherals.obd) — the namespace is saved but nothing on the device reads it";

/// `obdStatus` (`config-sync.ts:998-1067` @ 09da38d): the `obd` namespace's
/// verdict.
///  - no collector report      -> rejected (no `peripherals.obd`);
///  - settings.json differs    -> adjusted (local edit / not held yet);
///  - collector not caught up  -> adjusted (two texts keyed on `source`);
///  - otherwise                -> applied, with a runtime detail when the
///    setting is in force but not yet doing anything useful.
///
/// `report` is the opaque `effective.obd` (I2); only the six namespace
/// keys, `status` and `source` are read.
pub fn obd_status(
    desired: &ObdSettings,
    held: Option<&ObdSettings>,
    report: Option<&Value>,
    at: i64,
) -> NamespaceStatus {
    let Some(report) = report else {
        return NamespaceStatus::rejected(at, OBD_NO_PERIPHERAL_REASON.into());
    };
    let to = |s: &ObdSettings| serde_json::to_value(s).expect("obd settings serialise");
    // `deepEqual(held, desired)`: `undefined` vs an object is unequal.
    let held_matches = held.is_some_and(|h| deep_equal(&to(h), &to(desired)));
    if !held_matches {
        let detail = if held.is_none() {
            "saved, but the device is not holding this namespace yet"
        } else {
            "the device is holding a different value (changed locally?)"
        };
        return NamespaceStatus::adjusted(at, detail.into());
    }
    let want = desired.with_defaults();
    let want_v = to(&want);
    // `have[key] = report[key]` for the six keys: an absent report key is
    // `undefined`, which `deepEqual` sees as an own key holding undefined —
    // never equal to a defined value (`want` has all six).
    let have_matches = OBD_NAMESPACE_KEYS
        .iter()
        .all(|k| match (report.get(*k), want_v.get(*k)) {
            (Some(h), Some(w)) => deep_equal(h, w),
            _ => false,
        });
    if !have_matches {
        let detail = if report.get("source").and_then(Value::as_str) == Some("env") {
            "saved; the OBD collector is still on the env seam (XORGATE_OBD_*) and picks the namespace up on its next tick"
        } else {
            "saved; the OBD collector applies it on its next tick (about a second)"
        };
        return NamespaceStatus::adjusted(at, detail.into());
    }
    let status = report.get("status").and_then(Value::as_str);
    let enabled = want.enabled == Some(true);
    let mac = want.adapter_mac.clone().flatten();
    let detail = match (enabled, mac) {
        (true, Some(mac)) => match status {
            Some("not_configured") => Some(format!(
                "in force, but the device forgot adapter {mac} (obd_forget) — clear or change the adapter to pair again"
            )),
            Some("adapter_not_found") | Some("backoff") => Some(
                "in force, but the adapter is not reachable right now (unplugged, out of range or not powered)".into(),
            ),
            Some("parked") => Some(
                "in force, but the helper is parked after repeated failures (see the device events)".into(),
            ),
            _ => None,
        },
        (true, None) => Some("in force; no adapter configured, so nothing is polled (link 0)".into()),
        (false, _) => None,
    };
    let mut st = NamespaceStatus::applied(at);
    st.detail = detail;
    st
}

/// `cameraMountStatus` (`config-sync.ts:879`): `adjusted` + `detail` is the
/// only reported state that renders as "saved, but not what you asked for
/// yet" in BOTH frontends. Precedence is worst-first.
pub fn camera_mount_status(state: Option<&CameraMountState>, at: i64) -> NamespaceStatus {
    let Some(state) = state else {
        return NamespaceStatus::adjusted(
            at,
            "saved; the device has not reconciled its boot config yet".into(),
        );
    };
    let blocked: Vec<(String, String)> = state
        .cameras
        .iter()
        .filter(|c| {
            matches!(
                c.verdict,
                CameraMountVerdict::Unsupported
                    | CameraMountVerdict::NoOverlay
                    | CameraMountVerdict::UnknownCamera
                    | CameraMountVerdict::WriteFailed
            )
        })
        .map(|c| {
            (
                c.stream_key.clone(),
                c.detail
                    .clone()
                    .unwrap_or_else(|| c.verdict.as_str().to_string()),
            )
        })
        .collect();
    if !blocked.is_empty() {
        // `blocked.join("; ")` (`config-sync.ts:893`) whenever it fits the
        // backend's 500 characters; else shortened, the first port's
        // sentence whole (D117: the TS sends it as is and the backend then
        // drops the whole report).
        return NamespaceStatus::rejected(at, join_status_parts(&blocked, None));
    }
    // RUST-ONLY (a managed port): no camera answers on the port, so there
    // is no rotation in force to read. Said for what it is: not a write,
    // and not cured by a reboot.
    let no_camera: Vec<&str> = state
        .cameras
        .iter()
        .filter(|c| c.verdict == CameraMountVerdict::NoCamera)
        .map(|c| c.stream_key.as_str())
        .collect();
    let no_camera = (!no_camera.is_empty()).then(|| {
        format!(
            "no camera answers on {}, so the rotation in force cannot be read — look at the camera sensor setting (cameraSensors) and the camera report of the port; nothing here waits for a reboot",
            no_camera.join(", ")
        )
    });
    let with_no_camera = |text: String| match &no_camera {
        Some(n) => format!("{text}; {n}"),
        None => text,
    };
    let pending: Vec<String> = state
        .cameras
        .iter()
        .filter(|c| c.verdict == CameraMountVerdict::PendingReboot)
        .map(|c| match (c.in_force_deg, c.detail.as_deref()) {
            // RUST-ONLY (a managed port whose camera line was changed and
            // not booted, with no camera running there): the reconciler's
            // own words instead of "running ?°". The TS never gives a
            // pending camera a detail.
            (None, Some(why)) => format!("{} {}° ({why})", c.stream_key, c.desired_deg.degrees()),
            (running, _) => format!(
                "{} {}° (running {}°)",
                c.stream_key,
                c.desired_deg.degrees(),
                running.map(number_to_string).unwrap_or_else(|| "?".into())
            ),
        })
        .collect();
    if !pending.is_empty() {
        return NamespaceStatus::adjusted(
            at,
            with_no_camera(format!(
                "written to the boot config — REBOOT REQUIRED to apply: {}",
                pending.join(", ")
            )),
        );
    }
    let unconfirmed: Vec<&str> = state
        .cameras
        .iter()
        .filter(|c| c.verdict == CameraMountVerdict::Unconfirmed)
        .map(|c| c.stream_key.as_str())
        .collect();
    if !unconfirmed.is_empty() {
        return NamespaceStatus::adjusted(
            at,
            with_no_camera(format!(
                "written to the boot config, but the device could not read back the rotation currently in force ({}) — reboot to apply",
                unconfirmed.join(", ")
            )),
        );
    }
    if let Some(text) = no_camera {
        return NamespaceStatus::adjusted(at, text);
    }
    NamespaceStatus::applied(at)
}

/// `cellularStatus` (`config-sync.ts:930`): the verdict off the modem
/// daemon's own apply machinery — a rollback or a rejection must be VISIBLE.
pub fn cellular_status(
    desired: &CellularSettings,
    apn: Option<&ApnBlock>,
    at: i64,
) -> NamespaceStatus {
    let wanted = desired.apn.clone().unwrap_or_default();
    if wanted.is_empty() {
        // "Don't touch the modem" needs no confirmation.
        return NamespaceStatus::applied(at);
    }
    let Some(apn) = apn else {
        return NamespaceStatus::adjusted(
            at,
            "saved, but this device's modem daemon has not reported its APN machinery — it may predate the rollback-capable agent and will never apply this".into(),
        );
    };
    let last_error = apn
        .last_error
        .as_deref()
        .filter(|e| !e.is_empty())
        .map(|e| format!(" ({e})"))
        .unwrap_or_default();
    let requested_is_wanted = apn.apn_requested.as_deref() == Some(wanted.as_str());
    if apn.state == ApnState::Rejected && requested_is_wanted {
        let fails = apn
            .fails
            .map(|f| f.to_string())
            .unwrap_or_else(|| "repeated".into());
        return NamespaceStatus::rejected(
            at,
            format!(
                "the device gave up on \"{wanted}\" after {fails} failed applies{last_error} — change the value to try again"
            ),
        );
    }
    if apn.state == ApnState::Applying && requested_is_wanted {
        return NamespaceStatus::adjusted(
            at,
            "applying — the modem daemon is confirming the new APN with real traffic over the cellular link (allow a few minutes)".into(),
        );
    }
    if apn.apn_applied.as_deref() == Some(wanted.as_str()) {
        return NamespaceStatus::applied(at);
    }
    if apn.state == ApnState::RolledBack && requested_is_wanted {
        let applied = apn
            .apn_applied
            .clone()
            .unwrap_or_else(|| "the previous APN".into());
        return NamespaceStatus::adjusted(
            at,
            format!(
                "the device applied \"{wanted}\", could not pass data, and rolled back to \"{applied}\"{last_error}"
            ),
        );
    }
    NamespaceStatus::adjusted(
        at,
        "saved; the modem daemon has not applied it yet (it re-checks about every 30 s)".into(),
    )
}

/// `divergenceDetail` (`config-sync.ts:989`): why what we hold differs from
/// what the cloud asked for.
pub fn divergence_detail(key: &str, modem: &ModemBiasSample, in_force: Option<&Value>) -> String {
    if key == "gnssAntBias" {
        if !modem.fresh {
            return format!(
                "{} — the bias is saved and will be asserted by the modem daemon",
                modem.detail.as_deref().unwrap_or("modem state unknown")
            );
        }
        return "saved; the modem daemon has not applied it yet (it re-checks about every 30 s)"
            .into();
    }
    if in_force.is_none() {
        return "saved, but the device is not holding this namespace yet".into();
    }
    "the device is holding a different value (changed locally?)".into()
}

/// `latchDetail` (`config-sync.ts:1007`): honest caveats for a namespace that
/// IS persisted but whose effect is latched for this boot.
pub fn latch_detail(key: &str, hints: &ReportHints) -> Option<String> {
    if key == "recording" {
        let mut parts: Vec<String> = Vec::new();
        match hints.boot_defer.as_deref() {
            Some("deferring") => parts.push(
                "video is held by the boot power staging gate and starts when it releases".into(),
            ),
            Some("gating") => {
                parts.push("video is held by the GPS/LTE gate and starts when it releases".into())
            }
            _ => {}
        }
        // `if (hints.powerShed)` — truthy, so "" counts as absent.
        if let Some(shed) = hints.power_shed.as_deref().filter(|s| !s.is_empty()) {
            parts.push(format!("load shedding is active ({shed})"));
        }
        return (!parts.is_empty()).then(|| format!("in force, but {}", parts.join("; ")));
    }
    if key == "timeSync" {
        // `config-sync.ts:1165-1174` @ 09da38d: the clock trust hold first.
        match hints.clock_trust {
            Some(ClockTrustState::Held) => {
                return Some(CLOCK_TRUST_HELD_DETAIL.into());
            }
            Some(ClockTrustState::Fallback) => {
                return Some(CLOCK_TRUST_FALLBACK_DETAIL.into());
            }
            _ => {}
        }
        if hints.clock_stepper_armed == Some(false) {
            return Some(
                "in force, but the clock stepper stood down for this boot (NTP is synchronized)"
                    .into(),
            );
        }
    }
    None
}

/// `timeSync` detail while the clock trust gate holds (`config-sync.ts:1167` @ 09da38d).
pub const CLOCK_TRUST_HELD_DETAIL: &str = "in force, but the wall clock is not trusted yet: telemetry publish and recording are held until NTP/GPS/NITZ lands (5 min cap)";
/// `timeSync` detail in clock trust fallback (`config-sync.ts:1170` @ 09da38d).
pub const CLOCK_TRUST_FALLBACK_DETAIL: &str = "in force, but no NTP/GPS/NITZ sync landed within 5 min: publishing on an untrusted clock (timestamps may be early)";

// --- report builder ---------------------------------------------------------

/// `ReportInput` (`config-sync.ts:1029`). Every input is supplied by the
/// caller; `build` replaces the TS `loadBuildInfo()` read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportInput {
    pub device_id: String,
    pub ts: i64,
    pub origin: ReportOrigin,
    /// Sync state at build time — `lastAppliedRev` becomes the report's `rev`.
    pub last_applied_rev: u64,
    /// settings.json, re-read AFTER any apply completed.
    pub settings: AgentSettings,
    pub modem: ModemBiasSample,
    pub desired: Option<DesiredSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<ReportHints>,
    /// The run mode in force (`config.mode`) — reported-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The boot-config reconciler's last verdict; None = it has not run yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_mount: Option<CameraMountState>,
    /// Where telemetry is actually going; None = a caller that does not track it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_scope: Option<TelemetryScopeReport>,
    /// Locally-changed namespaces; only meaningful on an `origin: "local"` report.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "js::json_map_opt::serialize"
    )]
    pub changed: Option<Map<String, Value>>,
    /// OBD collector report (`ReportInput.obd`, `config-sync.ts:1208` @
    /// 09da38d); reported-only, absent without the peripheral.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "js::json_value_opt::serialize"
    )]
    pub obd: Option<Value>,
    /// The per-port camera report (`effective.cameras`, RUST-ONLY;
    /// [`crate::camera_truth`]); reported-only, absent without a hardware
    /// block. Not part of the TS `ReportInput`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "js::json_value_opt::serialize"
    )]
    pub cameras: Option<Value>,
    /// The camera reconciler's last pass over `cameraSensors` (RUST-ONLY);
    /// None = it has not run yet, or the namespace is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_sensors: Option<CameraSensorsState>,
}

/// `buildConfigReport` (`config-sync.ts:1059`): assemble a complete report.
/// Pure — every input is supplied by the caller.
pub fn build_config_report(input: &ReportInput, build: &BuildInfo) -> ConfigReport {
    let effective = build_effective(
        &input.settings,
        &input.modem,
        input.mode.as_deref(),
        input.camera_mount.as_ref(),
        input.telemetry_scope.as_ref(),
        input.obd.as_ref(),
        input.cameras.as_ref(),
        input.camera_sensors.as_ref(),
        build,
    );
    let status = build_status(
        input.desired.as_ref(),
        &effective,
        &input.modem,
        input.ts,
        input.hints.as_ref().unwrap_or(&ReportHints::default()),
        input.camera_mount.as_ref(),
        Some(&input.settings),
        input.camera_sensors.as_ref(),
    );
    // Only a local report carries `changed`.
    let changed = match (&input.origin, &input.changed) {
        (ReportOrigin::Local, Some(c)) if !c.is_empty() => Some(c.clone()),
        _ => None,
    };
    ConfigReport {
        v: 1,
        device_id: input.device_id.clone(),
        ts: input.ts,
        rev: input.last_applied_rev,
        origin: input.origin,
        effective,
        status,
        changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_parse_matches_v8_for_the_daemon_forms() {
        // `readModemBias` now goes through the shared `js::date_parse` (WP3's
        // V8-checked grammar); the daemon's `date -u +%Y-%m-%dT%H:%M:%SZ` forms:
        assert_eq!(
            date_parse_ms("2026-08-06T12:00:00Z"),
            Some(1_786_017_600_000)
        );
        assert_eq!(
            date_parse_ms("2026-08-06T11:59:40Z"),
            Some(1_786_017_580_000)
        );
        assert_eq!(
            date_parse_ms("2026-08-06T11:00:00Z"),
            Some(1_786_014_000_000)
        );
        assert_eq!(date_parse_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(date_parse_ms("1970-01-01"), Some(0));
        assert_eq!(date_parse_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(date_parse_ms(""), None);
        assert_eq!(date_parse_ms("garbage"), None);
        assert_eq!(date_parse_ms("2026-13-01"), None);
    }

    #[test]
    fn reported_string_cut_inside_a_surrogate_pair_is_fffd() {
        // ACCEPTED DIVERGENCE (review-wp4 #5, evidence/accepted-divergences.md):
        // node 24 `JSON.stringify(("a".repeat(63) + "👍").trim().slice(0, 64))`
        // ends `…aaa\ud83d"` (a lone surrogate escape); Rust keeps valid UTF-8.
        let got = reported_string(&Value::from(format!("{}👍", "a".repeat(63))));
        assert_eq!(got, Some(format!("{}\u{fffd}", "a".repeat(63))));
    }

    #[test]
    fn reported_string_rules() {
        assert_eq!(
            reported_string(&Value::from("  Public\u{1} Mobile  ")),
            Some("Public Mobile".into())
        );
        assert_eq!(reported_string(&Value::from("   ")), None);
        assert_eq!(reported_string(&Value::from(5)), None);
        assert_eq!(
            reported_string(&Value::from("A".repeat(200))).map(|s| s.len()),
            Some(64)
        );
        // 64 UTF-16 units, not chars: 33 emoji = 66 units → 32 emoji kept.
        let emoji = "😀".repeat(33);
        assert_eq!(
            reported_string(&Value::from(emoji)).map(|s| s.chars().count()),
            Some(32)
        );
    }

    #[test]
    fn deep_equal_is_numeric_and_order_insensitive() {
        let a: Value = serde_json::from_str(r#"{"x":2,"y":[1,{"z":true}]}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"y":[1.0,{"z":true}],"x":2.0}"#).unwrap();
        assert!(deep_equal(&a, &b));
        assert!(!deep_equal(&a, &serde_json::json!({"x": 2})));
        assert!(!deep_equal(&Value::Null, &Value::Bool(false)));
    }

    fn desired(payload: &str) -> DesiredSnapshot {
        let m = crate::cloud_config::parse_cloud_message(payload).expect("a v1 message");
        DesiredSnapshot {
            rev: m.config.rev.unwrap_or(0),
            raw: m.raw,
            parsed: m.config,
        }
    }

    fn sensors_state(
        ports: &[(&str, &str, CameraSensorsVerdict, Option<&str>)],
    ) -> CameraSensorsState {
        CameraSensorsState {
            at: 5,
            reboot_required: ports
                .iter()
                .any(|p| p.2 == CameraSensorsVerdict::PendingReboot),
            cameras: ports
                .iter()
                .map(|(k, s, v, d)| crate::types::CameraSensorsPortState {
                    stream_key: (*k).to_string(),
                    sensor: Some((*s).to_string()),
                    verdict: *v,
                    detail: d.map(str::to_string),
                })
                .collect(),
            wrote: false,
            // Computed from the value the status test holds.
            settings: HELD_SENSORS.to_string(),
        }
    }

    /// The `cameraSensors` value the status tests hold, as the reconciler
    /// stamps it on its state.
    const HELD_SENSORS: &str =
        r#"{"cameras":{"cam0":{"sensor":"imx708"},"cam1":{"sensor":"none"}}}"#;

    /// `status.cameraSensors` and `effective.cameraSensors` (RUST-ONLY).
    #[test]
    fn camera_sensors_status_and_effective() {
        use CameraSensorsVerdict::{Applied, PendingReboot, Rejected, WriteFailed};
        let build = BuildInfo {
            version: "abc1234".into(),
            channel: "dev".into(),
            built_at: None,
            package_version: "0.2.0".into(),
        };
        let held: AgentSettings = serde_json::from_str(
            r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx708"},"cam1":{"sensor":"none"}}}}"#,
        )
        .unwrap();
        let msg = r#"{"v":1,"rev":7,"cameraSensors":{"cameras":{"cam0":{"sensor":"imx708"},"cam1":{"sensor":"none"}}}}"#;
        let status = |settings: &AgentSettings,
                      d: Option<&DesiredSnapshot>,
                      st: Option<&CameraSensorsState>| {
            let eff = build_effective(
                settings,
                &ModemBiasSample::default(),
                Some("real"),
                None,
                None,
                None,
                None,
                st,
                &build,
            );
            let s = build_status(
                d,
                &eff,
                &ModemBiasSample::default(),
                9,
                &ReportHints::default(),
                None,
                Some(settings),
                st,
            );
            (
                eff.get("cameraSensors").map(js::to_json_string),
                s.get("cameraSensors")
                    .map(|v| serde_json::to_string(v).unwrap()),
            )
        };
        let d = desired(msg);
        // Saved, the reconciler has not run: not "applied".
        assert_eq!(
            status(&held, Some(&d), None),
            (
                Some(r#"{"cameras":{"cam0":{"sensor":"imx708"},"cam1":{"sensor":"none"}},"reconciled":false}"#.into()),
                Some(r#"{"state":"adjusted","at":9,"detail":"saved; the device has not reconciled its boot config yet"}"#.into())
            )
        );
        // Every port as declared and booted.
        let st = sensors_state(&[
            ("cam0", "imx708", Applied, None),
            ("cam1", "none", Applied, None),
        ]);
        assert_eq!(
            status(&held, Some(&d), Some(&st)),
            (
                Some(r#"{"cameras":{"cam0":{"sensor":"imx708"},"cam1":{"sensor":"none"}},"rebootRequired":false}"#.into()),
                Some(r#"{"state":"applied","at":9}"#.into())
            )
        );
        // Written, not booted: `adjusted` with a detail (what the console
        // reads as a pending reboot, with the port's `reboot-required`).
        let st = sensors_state(&[
            ("cam0", "imx708", PendingReboot, None),
            ("cam1", "none", PendingReboot, None),
        ]);
        assert_eq!(
            status(&held, Some(&d), Some(&st)),
            (
                Some(r#"{"cameras":{"cam0":{"sensor":"imx708"},"cam1":{"sensor":"none"}},"rebootRequired":true}"#.into()),
                Some(r#"{"state":"adjusted","at":9,"detail":"written to the boot config — REBOOT REQUIRED to apply: cam0 imx708, cam1 none"}"#.into())
            )
        );
        // A port that could not be applied: `rejected`, every such port
        // named, and the ports that wait for a reboot after them.
        let st = sensors_state(&[
            (
                "cam0",
                "imx999",
                Rejected,
                Some("this agent build does not know the camera sensor \"imx999\""),
            ),
            ("cam1", "none", PendingReboot, None),
            (
                "cam7",
                "imx290",
                WriteFailed,
                Some("could not edit /boot/firmware/config.txt: sudo: a password is required"),
            ),
        ]);
        assert_eq!(
            status(&held, Some(&d), Some(&st)).1,
            Some(r#"{"state":"rejected","at":9,"reason":"cam0: this agent build does not know the camera sensor \"imx999\"; cam7: could not edit /boot/firmware/config.txt: sudo: a password is required; written to the boot config and waiting for a reboot: cam1 none"}"#.into())
        );
        // The cloud never spoke about it (a local value): no status entry,
        // the namespace still shows in `effective`.
        let quiet = desired(r#"{"v":1,"rev":7,"gnssAntBias":true}"#);
        let (eff, s) = status(&held, Some(&quiet), Some(&st));
        assert!(eff.is_some() && s.is_none());
        // Not held and not sent: neither.
        assert_eq!(
            status(&AgentSettings::default(), Some(&quiet), None),
            (None, None)
        );
        // `{}` = revert: applied once the key is gone, adjusted while held.
        let revert = desired(r#"{"v":1,"rev":8,"cameraSensors":{}}"#);
        assert_eq!(
            status(&AgentSettings::default(), Some(&revert), None).1,
            Some(r#"{"state":"applied","at":9}"#.into())
        );
        assert_eq!(
            status(&held, Some(&revert), None).1,
            Some(r#"{"state":"adjusted","at":9,"detail":"asked to revert to defaults, but the device is still holding a value"}"#.into())
        );
        // A key the parser dropped (not a stream key): normalised.
        let dropped =
            desired(r#"{"v":1,"cameraSensors":{"cameras":{"bad key!":{"sensor":"imx290"}}}}"#);
        assert_eq!(
            status(&held, Some(&dropped), None).1,
            Some(r#"{"state":"adjusted","at":9,"detail":"the device clamped or normalized the delivered value"}"#.into())
        );
        // The pass looked at ANOTHER value than the one held now (a cloud
        // message applied a moment ago, a local edit): it does not speak
        // for this one. Not `applied`, not its old rejection either.
        for verdict in [Applied, Rejected] {
            let mut stale = sensors_state(&[("cam0", "imx290", verdict, Some("old"))]);
            stale.settings = r#"{"cameras":{"cam0":{"sensor":"imx290"}}}"#.to_string();
            assert_eq!(
                status(&held, Some(&d), Some(&stale)),
                (
                    Some(r#"{"cameras":{"cam0":{"sensor":"imx708"},"cam1":{"sensor":"none"}},"reconciled":false}"#.into()),
                    Some(r#"{"state":"adjusted","at":9,"detail":"saved; the device has not reconciled its boot config yet"}"#.into())
                )
            );
        }
        // A sensor this build does not know is NOT "normalized away": the
        // entry is held as delivered and the reconciler's verdict is the status.
        let unknown = desired(
            r#"{"v":1,"cameraSensors":{"cameras":{"cam0":{"sensor":"imx999","future":1}}}}"#,
        );
        let st = sensors_state(&[("cam0", "imx999", Rejected, Some("x"))]);
        assert_eq!(
            status(&held, Some(&unknown), Some(&st)).1,
            Some(r#"{"state":"rejected","at":9,"reason":"cam0: x"}"#.into())
        );
    }

    /// A namespace removed in the cloud (`PATCH {ns: null}`) is published as
    /// `{}`, the revert marker, and stays in the retained message. Every
    /// namespace answers it the same way (`config-sync.ts:777`, rule 0; the
    /// JS goldens "applied for a revert marker on every object namespace"):
    /// `applied` once the local key is gone, with no `effective` block.
    /// `cameraMount` and `cameraSensors` are no exception: the status entry
    /// that stays behind IS the acknowledgement of the revert, for as long
    /// as the marker is published (a bench reading of 2026-11-22 took it for
    /// a leftover).
    #[test]
    fn a_reverted_namespace_is_acknowledged_the_same_way_by_every_namespace() {
        let build = BuildInfo {
            version: "abc1234".into(),
            channel: "dev".into(),
            built_at: None,
            package_version: "0.2.0".into(),
        };
        let settings = AgentSettings::default();
        let eff = build_effective(
            &settings,
            &ModemBiasSample::default(),
            Some("real"),
            None,
            None,
            None,
            None,
            None,
            &build,
        );
        let d = desired(
            r#"{"v":1,"rev":11,"imuMount":{},"recording":{},"lteRecovery":{},"timeSync":{},"cellular":{},"cameraMount":{},"simulation":{},"obd":{},"cameraSensors":{}}"#,
        );
        let status = build_status(
            Some(&d),
            &eff,
            &ModemBiasSample::default(),
            9,
            &ReportHints::default(),
            None,
            Some(&settings),
            None,
        );
        assert_eq!(
            status.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "imuMount",
                "recording",
                "lteRecovery",
                "timeSync",
                "cellular",
                "cameraMount",
                "simulation",
                "obd",
                "cameraSensors"
            ]
        );
        for (key, st) in &status {
            assert_eq!(
                serde_json::to_string(st).unwrap(),
                r#"{"state":"applied","at":9}"#,
                "{key}"
            );
            assert!(!eff.contains_key(key), "{key}");
        }
        // Not published at all: no entry.
        let quiet = desired(r#"{"v":1,"rev":12}"#);
        assert!(build_status(
            Some(&quiet),
            &eff,
            &ModemBiasSample::default(),
            9,
            &ReportHints::default(),
            None,
            Some(&settings),
            None
        )
        .is_empty());
    }

    /// THE BACKEND DROPS A WHOLE REPORT whose status `detail` or `reason`
    /// is longer than 500 characters (`z.string().max(500)`). Two ports,
    /// each with a long path-bearing sentence, were over it. No status text
    /// of either camera namespace, nor of any other, may be: shortened at a
    /// sentence boundary, the first port's sentence whole.
    #[test]
    fn no_status_text_exceeds_the_backends_500_characters() {
        use crate::types::{CameraMountCameraState, CameraRotationDeg};
        let len = |s: &str| s.encode_utf16().count();
        // The real sentences: a port that is not confirmed (Phase 1b), and
        // the helper's refusal with its paths.
        let not_confirmed = "the camera line for cam0 is left as it is, because the camera on this port is not confirmed: csi0 is declared as imx708 by config.txt, but no imx708 answered on this port (the kernel driver did not bind). Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached, then reboot.";
        let helper = "could not edit /boot/firmware/config.txt: a dtparam= line follows the camera line of cam1 and would then apply to another overlay; move it into the dtoverlay line or remove it first (/boot/firmware/config.txt not changed)";
        assert!(
            len(not_confirmed) + len(helper) + 20 > MAX_STATUS_TEXT,
            "the case is over the bound"
        );

        // `cameraMount`, two blocked ports.
        let mount = |cams: &[(&str, &str)]| CameraMountState {
            at: 1,
            cameras: cams
                .iter()
                .map(|(k, d)| CameraMountCameraState {
                    stream_key: (*k).to_string(),
                    desired_deg: CameraRotationDeg::from_f64(180.0).unwrap(),
                    in_force_deg: None,
                    boot_deg: None,
                    verdict: CameraMountVerdict::Unsupported,
                    detail: Some((*d).to_string()),
                })
                .collect(),
            reboot_required: false,
            wrote: false,
        };
        let st = camera_mount_status(
            Some(&mount(&[("cam0", not_confirmed), ("cam1", helper)])),
            9,
        );
        let reason = st.reason.unwrap();
        assert!(len(&reason) <= MAX_STATUS_TEXT, "{}", len(&reason));
        // The first port's sentence is whole; the second is cut at a
        // sentence boundary, not inside a word.
        assert_eq!(
            reason,
            format!("cam0: {not_confirmed}; cam1: could not edit /boot/firmware/config.txt: a dtparam= line follows the camera line of cam1 and would then apply to another overlay …")
        );
        // Within the bound nothing changes: the TS text, byte for byte.
        let short = camera_mount_status(Some(&mount(&[("cam0", "a b."), ("cam1", "c d.")])), 9);
        assert_eq!(short.reason.as_deref(), Some("cam0: a b.; cam1: c d."));

        // `cameraSensors`: three refused ports and a waiting one.
        let sensors = CameraSensorsState {
            at: 1,
            cameras: [("cam0", helper), ("cam1", not_confirmed), ("cam2", helper)]
                .iter()
                .map(|(k, d)| crate::types::CameraSensorsPortState {
                    stream_key: (*k).to_string(),
                    sensor: Some("imx708".into()),
                    verdict: CameraSensorsVerdict::WriteFailed,
                    detail: Some((*d).to_string()),
                })
                .chain(std::iter::once(crate::types::CameraSensorsPortState {
                    stream_key: "cam3".into(),
                    sensor: Some("none".into()),
                    verdict: CameraSensorsVerdict::PendingReboot,
                    detail: None,
                }))
                .collect(),
            reboot_required: true,
            wrote: false,
            settings: "null".into(),
        };
        let st = camera_sensors_status(Some(&sensors), None, 9);
        let reason = st.reason.unwrap();
        assert!(len(&reason) <= MAX_STATUS_TEXT, "{}", len(&reason));
        // Three long sentences do not fit whole: each port keeps its main
        // clause (a fair share each: the LAST port does not pay for the
        // first), cut at a clause boundary, and the waiting port is named.
        assert_eq!(
            reason,
            "cam0: could not edit /boot/firmware/config.txt: a dtparam= line follows the camera line of cam1 and would then apply to another overlay …; cam1: the camera line for cam0 is left as it is, because the camera on this port is not confirmed: csi0 is declared as imx708 by config.txt …; cam2: could not edit /boot/firmware/config.txt: a dtparam= line follows the camera line of cam1 and would then apply to another overlay …; written to the boot config and waiting for a reboot: cam3 none"
        );
        assert!(!reason.contains("  "), "{reason}");

        // The bench case (2026-11-22): a long section name and a long
        // unlisted token pushed the last port's advice off the end ("…it
        // is left as it …"). The long quoted parts are clipped FIRST, so
        // both sentences stay whole, advice included.
        let token = "xgtest-unlisted-parameter-0123456789";
        let unlisted = format!("the camera line for cam0 holds \"{token}\", which the sensor catalog does not list for imx335: rewriting the line would drop it, so the line is left as it is. Edit the line by hand, or remove \"{token}\" from it");
        let section = "a camera line for cam1 in /boot/firmware/config.txt sits under [x-unknown-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789-01234], a section the agent cannot evaluate for this board: it is left as it is; edit it by hand";
        let bench = join_status_parts(
            &[
                ("cam0".into(), unlisted.clone()),
                ("cam1".into(), section.into()),
            ],
            None,
        );
        assert!(len(&format!("cam0: {unlisted}; cam1: {section}")) > MAX_STATUS_TEXT);
        assert_eq!(
            bench,
            format!("cam0: {unlisted}; cam1: a camera line for cam1 in /boot/firmware/config.txt sits under [x-unknown-0123456789-0123456789-0123…], a section the agent cannot evaluate for this board: it is left as it is; edit it by hand")
        );
        assert_eq!(
            clip_long_words("under [abcdefghijklmnopqrstuvwxyz], ok \"0123456789012345678901234567890123456789\".", 20),
            "under [abcdefghijklmnop…], ok \"0123456789012345…\"."
        );
        // So many ports that a share would not hold a sentence: the first
        // ones speak, the others are named.
        let many: Vec<(String, String)> = (0..16)
            .map(|i| (format!("cam{i}"), helper.to_string()))
            .collect();
        let crowded = join_status_parts(&many, None);
        assert!(len(&crowded) <= MAX_STATUS_TEXT, "{}", len(&crowded));
        assert!(crowded.starts_with("cam0: could not edit"), "{crowded}");
        assert!(crowded.ends_with("cam14, cam15"), "{crowded}");
        assert!(crowded.contains("; and cam"), "{crowded}");

        // One sentence over the bound by itself: cut at a sentence end.
        let long = format!("{} {}", "First sentence here.", "word ".repeat(200));
        let cut = fit_status_text(&long);
        assert!(len(&cut) <= MAX_STATUS_TEXT);
        assert!(cut.ends_with("word …"), "{cut}");
        let (a, b) = ("aaaa ".repeat(60), "bbbb ".repeat(60));
        let two = format!("{}. {}", a.trim_end(), b.trim_end());
        assert_eq!(fit_status_text(&two), format!("{}. …", a.trim_end()));
        // A cut inside a sentence falls on a clause boundary when one is
        // near, not two words further on.
        assert_eq!(
            fit_text("one two three four: five six seven eight nine ten", 34),
            "one two three four: …"
        );
        // Astral characters count as two (JavaScript string length).
        let emoji = "😀".repeat(300);
        assert!(len(&fit_status_text(&emoji)) <= MAX_STATUS_TEXT);
        assert_eq!(fit_status_text("short"), "short");

        // The net under every namespace: whatever a namespace's own code
        // produced, `build_status` hands nothing over the bound to the report.
        let build = BuildInfo {
            version: "abc1234".into(),
            channel: "dev".into(),
            built_at: None,
            package_version: "0.2.0".into(),
        };
        let held: AgentSettings = serde_json::from_str(
            r#"{"cameraMount":{"cameras":{"cam0":{"rotationDeg":180},"cam1":{"rotationDeg":180}}}}"#,
        )
        .unwrap();
        let state = mount(&[("cam0", &"x ".repeat(400)), ("cam1", helper)]);
        let report = build_config_report(
            &ReportInput {
                device_id: "d".into(),
                ts: 1,
                origin: ReportOrigin::Cloud,
                last_applied_rev: 3,
                settings: held,
                modem: ModemBiasSample::default(),
                desired: Some(desired(
                    r#"{"v":1,"rev":3,"cameraMount":{"cameras":{"cam0":{"rotationDeg":180},"cam1":{"rotationDeg":180}}}}"#,
                )),
                hints: None,
                mode: Some("real".into()),
                camera_mount: Some(state),
                telemetry_scope: None,
                changed: None,
                obd: None,
                cameras: None,
                camera_sensors: None,
            },
            &build,
        );
        for (key, st) in &report.status {
            for text in [&st.detail, &st.reason].into_iter().flatten() {
                assert!(len(text) <= MAX_STATUS_TEXT, "{key}: {}", len(text));
            }
        }
        assert_eq!(report.status["cameraMount"].state, NamespaceState::Rejected);
    }

    /// A local edit folds back as the WHOLE namespace, or `null` when it
    /// was removed (`changed.cameraSensors`).
    #[test]
    fn camera_sensors_local_changes_fold_back_whole() {
        let settings = |text: &str| -> AgentSettings { serde_json::from_str(text).unwrap() };
        let base = namespaces_of(&settings(
            r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx290"},"cam1":{"sensor":"imx290"}}}}"#,
        ));
        // One port changed: the whole namespace, both ports.
        let next = namespaces_of(&settings(
            r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx708","options":{"linkFrequency":447000000}},"cam1":{"sensor":"imx290"}}}}"#,
        ));
        assert_eq!(
            js::map_to_json_string(&diff_namespaces(&base, &next)),
            r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx708","options":{"linkFrequency":447000000}},"cam1":{"sensor":"imx290"}}}}"#
        );
        // Removed locally: null.
        assert_eq!(
            js::map_to_json_string(&diff_namespaces(&base, &namespaces_of(&settings("{}")))),
            r#"{"cameraSensors":null}"#
        );
        // Unchanged: nothing.
        assert!(diff_namespaces(&base, &base).is_empty());
        // Newly set on a device that had none.
        assert_eq!(
            js::map_to_json_string(&diff_namespaces(&namespaces_of(&settings("{}")), &base)),
            r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx290"},"cam1":{"sensor":"imx290"}}}}"#
        );
    }

    /// THE CAPABILITY PIN (support-other-camera-models Phase 2). The cloud
    /// withdraws a stored `cameraSensors` value on the first report whose
    /// capability list lacks `camera.sensors`, so the list must be the same
    /// in every report that has an `agent` block: one compile-time
    /// constant, emitted whole, built nowhere else and never conditionally.
    #[test]
    fn the_capability_list_is_one_constant_in_every_agent_block() {
        let build = BuildInfo {
            version: "abc1234".into(),
            channel: "dev".into(),
            built_at: None,
            package_version: "0.1.0".into(),
        };
        let caps: Vec<Value> = AGENT_CAPABILITIES
            .iter()
            .map(|c| Value::String((*c).to_string()))
            .collect();
        assert!(AGENT_CAPABILITIES.contains(&CAPABILITY_CAMERA_SENSORS));
        // Whatever else the report holds: every origin, every mode, with
        // and without the namespace, a reconciler state, a hardware report.
        let held: AgentSettings = serde_json::from_str(
            r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx999"}}},"cameraMount":{"cameras":{"cam0":{"rotationDeg":180}}}}"#,
        )
        .unwrap();
        let mut reports = 0;
        for origin in [ReportOrigin::Boot, ReportOrigin::Cloud, ReportOrigin::Local] {
            for mode in ["real", "sim", "replay"] {
                for settings in [AgentSettings::default(), held.clone()] {
                    for sensors in [
                        None,
                        Some(CameraSensorsState {
                            at: 1,
                            cameras: vec![],
                            reboot_required: true,
                            wrote: false,
                            settings: String::new(),
                        }),
                    ] {
                        let report = build_config_report(
                            &ReportInput {
                                device_id: "d".into(),
                                ts: 1,
                                origin,
                                last_applied_rev: 3,
                                settings: settings.clone(),
                                modem: ModemBiasSample::default(),
                                desired: None,
                                hints: None,
                                mode: Some(mode.into()),
                                camera_mount: None,
                                telemetry_scope: None,
                                changed: None,
                                obd: None,
                                cameras: None,
                                camera_sensors: sensors,
                            },
                            &build,
                        );
                        assert_eq!(
                            report.effective["agent"]["capabilities"],
                            Value::Array(caps.clone()),
                            "{origin:?} {mode}"
                        );
                        reports += 1;
                    }
                }
            }
        }
        assert_eq!(reports, 36);
        // The list is built in exactly one place, from the constant, and no
        // other code names the capability string.
        let code = include_str!("config_report.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert_eq!(code.matches("\"capabilities\".into()").count(), 1);
        assert_eq!(
            code.matches("AGENT_CAPABILITIES\n").count(),
            1,
            "one use: the agent block"
        );
        assert_eq!(
            code.matches("\"camera.sensors\"").count(),
            1,
            "one definition"
        );
    }

    #[test]
    fn rank_and_rev_guard() {
        assert!(ReportOrigin::Local.rank() > ReportOrigin::Cloud.rank());
        assert!(ReportOrigin::Cloud.rank() > ReportOrigin::Boot.rank());
        assert!(should_apply_rev(None, 99));
        assert!(should_apply_rev(Some(0), 99));
        assert!(should_apply_rev(Some(43), 42));
        assert!(!should_apply_rev(Some(42), 42));
        // The JS list (`config-sync.ts:631-639`), then the Rust-only
        // `camera.sensors` (D107): the cloud withdraws `cameraSensors` from
        // a device whose report lacks it, so it is pinned here and in
        // `the_capability_list_is_one_constant_in_every_agent_block`.
        assert_eq!(
            AGENT_CAPABILITIES,
            [
                "sms.read",
                "sms.clear",
                "obd.scan",
                "obd.forget",
                "camera.sensors"
            ]
        );
        assert_eq!(CAPABILITY_CAMERA_SENSORS, "camera.sensors");
        assert_eq!(MODEM_TELEMETRY_STALE_MS, 180_000);
    }

    /// D85 (Phase 8 D9, Q-8.3): the Rust agent's report marker. Positive
    /// evidence for the backend's mirror (`runtime === "rust"` →
    /// `devices.agent = 'rust'`), so it must be exactly `"rust"`, ride in
    /// EVERY `agent` block, and come LAST (after the JS keys, whose order the
    /// JS goldens pin in `tests/config_report_fixtures.rs`). No `agent` block
    /// (no mode) → no marker, as JS has no block then either.
    #[test]
    fn d85_effective_agent_reports_runtime_rust_last() {
        assert_eq!(AGENT_RUNTIME, "rust");
        let build = BuildInfo {
            version: "abc1234".into(),
            channel: "dev".into(),
            built_at: None,
            package_version: "0.1.0".into(),
        };
        let settings = AgentSettings::default();
        let modem = ModemBiasSample::default();
        for mode in ["real", "sim", "replay"] {
            let eff = build_effective(
                &settings,
                &modem,
                Some(mode),
                None,
                None,
                None,
                None,
                None,
                &build,
            );
            let agent = eff["agent"].as_object().expect("agent block");
            assert_eq!(
                agent.keys().map(String::as_str).collect::<Vec<_>>(),
                ["mode", "build", "capabilities", "runtime"],
                "{mode}"
            );
            assert_eq!(agent["runtime"], Value::String("rust".into()), "{mode}");
            assert_eq!(
                js::map_to_json_string(&eff),
                format!(
                    r#"{{"agent":{{"mode":"{mode}","build":{{"version":"abc1234","channel":"dev","builtAt":null,"packageVersion":"0.1.0"}},"capabilities":["sms.read","sms.clear","obd.scan","obd.forget","camera.sensors"],"runtime":"rust"}}}}"#
                )
            );
        }
        let eff = build_effective(
            &settings, &modem, None, None, None, None, None, None, &build,
        );
        assert!(!eff.contains_key("agent"));
        assert!(!js::map_to_json_string(&eff).contains("runtime"));
        // ...and on the wire, inside the whole report.
        let input = ReportInput {
            device_id: "dev-1".into(),
            ts: 1_700_000_000_000,
            origin: ReportOrigin::Boot,
            last_applied_rev: 0,
            settings,
            modem,
            desired: None,
            hints: None,
            mode: Some("real".into()),
            camera_mount: None,
            telemetry_scope: None,
            changed: None,
            obd: None,
            cameras: None,
            camera_sensors: None,
        };
        let wire = build_config_report(&input, &build).to_wire();
        let v: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(v["effective"]["agent"]["runtime"], "rust");
        assert_eq!(wire.matches(r#""runtime":"rust""#).count(), 1, "{wire}");
    }

    /// The per-port camera report (support-other-camera-models Phase 0,
    /// Rust-only): `effective.cameras` is the LAST key of `effective`
    /// (after `obd`, the last JS key), is carried verbatim, gets no `status`
    /// entry, is absent when the agent has none, and its key is not a config
    /// namespace (a reported block under a namespace key makes the console
    /// say "Device differs").
    #[test]
    fn effective_cameras_is_reported_only_and_last() {
        assert_eq!(EFFECTIVE_CAMERAS_KEY, "cameras");
        assert!(crate::config_namespaces::CONFIG_APPLY_CLASSES
            .iter()
            .all(|(k, _)| *k != EFFECTIVE_CAMERAS_KEY));
        let build = BuildInfo {
            version: "abc1234".into(),
            channel: "dev".into(),
            built_at: None,
            package_version: "0.1.0".into(),
        };
        let cameras: Value = serde_json::from_str(
            r#"{"cam0":{"connector":"csi0","declared":{"sensor":"imx708","source":"boot-config"},"bootConfig":{"overlay":"imx708","line":"dtoverlay=imx708,cam0","lines":1},"bound":{"driver":true,"node":"imx708@1a","listed":"imx708","path":"/base/soc/i2c0mux/i2c@0/imx708@1a","identity":"chip-id"},"capture":{"state":"streaming","backoffStep":0,"lastError":null},"verdict":"ok","detail":"imx708 on csi0 is detected and streaming."}}"#,
        )
        .unwrap();
        let obd: Value = serde_json::from_str(r#"{"state":"connected"}"#).unwrap();
        let mut input = ReportInput {
            device_id: "dev-1".into(),
            ts: 1_700_000_000_000,
            origin: ReportOrigin::Boot,
            last_applied_rev: 0,
            settings: AgentSettings::default(),
            modem: ModemBiasSample::default(),
            desired: None,
            hints: None,
            mode: Some("real".into()),
            camera_mount: None,
            telemetry_scope: None,
            changed: None,
            obd: Some(obd),
            cameras: Some(cameras.clone()),
            camera_sensors: None,
        };
        let report = build_config_report(&input, &build);
        assert_eq!(
            report
                .effective
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["agent", "obd", "cameras"]
        );
        assert_eq!(report.effective["cameras"], cameras);
        assert!(!report.status.contains_key("cameras"));
        // Byte-exact on the wire: integers stay integers, `null` stays `null`.
        let wire = report.to_wire();
        assert!(
            wire.contains(
                r#""capture":{"state":"streaming","backoffStep":0,"lastError":null},"verdict":"ok""#
            ),
            "{wire}"
        );
        // Without a camera report the key is absent, and so is it for `null`.
        for none in [None, Some(Value::Null)] {
            input.cameras = none;
            let report = build_config_report(&input, &build);
            assert!(!report.effective.contains_key("cameras"));
        }
    }

    #[test]
    fn config_sync_state_shape() {
        let s = r#"{"lastAppliedRev":42,"lastReportTs":1700,"settingsHash":"ab","namespaces":{"gnssAntBias":true},"localPending":true}"#;
        let v: ConfigSyncState = serde_json::from_str(s).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), s);
        assert_eq!(
            serde_json::to_string(&ConfigSyncState::default()).unwrap(),
            r#"{"lastAppliedRev":0,"lastReportTs":0}"#
        );
        assert_eq!(
            parse_config_sync_state(Some("{not json")),
            ConfigSyncState::default()
        );
        assert_eq!(
            parse_config_sync_state(Some(r#"{"lastAppliedRev":"12"}"#)),
            ConfigSyncState::default()
        );
        assert_eq!(parse_config_sync_state(Some(s)), v);
    }

    #[test]
    fn config_sync_state_keeps_js_key_order() {
        // `ensureConfigSyncBaseline` on a Phase-3 file: `{...state, namespaces, settingsHash}`.
        let (mut st, save) = ensure_config_sync_baseline(
            parse_config_sync_state(Some(r#"{"lastAppliedRev":12,"lastReportTs":5}"#)),
            &AgentSettings::default(),
            "h",
        );
        assert!(save);
        assert_eq!(
            st.to_file_string(),
            r#"{"lastAppliedRev":12,"lastReportTs":5,"namespaces":{},"settingsHash":"h"}"#
        );
        // `persist({ localPending: false })` writes the false; an existing key stays put.
        st.merge([
            SyncPatch::LocalPending(false),
            SyncPatch::SettingsHash("h2".into()),
        ]);
        st.merge([SyncPatch::LastReportTs(9)]);
        let wire = st.to_file_string();
        assert_eq!(
            wire,
            r#"{"lastAppliedRev":12,"lastReportTs":9,"namespaces":{},"settingsHash":"h2","localPending":false}"#
        );
        let back: ConfigSyncState = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, st);
        // The tolerant FILE read normalises to the TS read order and drops `false`.
        assert_eq!(
            parse_config_sync_state(Some(&wire)).to_file_string(),
            r#"{"lastAppliedRev":12,"lastReportTs":9,"settingsHash":"h2","namespaces":{}}"#
        );
        // A field set directly (no merge) still serialises, in interface order.
        let direct = ConfigSyncState {
            local_pending: Some(true),
            settings_hash: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(
            direct.to_file_string(),
            r#"{"lastAppliedRev":0,"lastReportTs":0,"settingsHash":"x","localPending":true}"#
        );
    }

    #[test]
    fn rebaseline_and_local_changes() {
        let base: Map<String, Value> =
            serde_json::from_str(r#"{"gnssAntBias":true,"timeSync":{"offlineSources":true}}"#)
                .unwrap();
        let current: Map<String, Value> =
            serde_json::from_str(r#"{"gnssAntBias":false,"timeSync":{"offlineSources":false}}"#)
                .unwrap();
        let re = rebaseline_after_write(Some(&base), &current, &["gnssAntBias", "agent"]);
        assert_eq!(
            serde_json::to_string(&re).unwrap(),
            r#"{"gnssAntBias":false,"timeSync":{"offlineSources":true}}"#
        );
        let changes = local_report_changes(Some(&re), &current).unwrap();
        assert_eq!(
            serde_json::to_string(&changes).unwrap(),
            r#"{"timeSync":{"offlineSources":false}}"#
        );
        assert_eq!(local_report_changes(None, &current), None);
        assert_eq!(local_report_changes(Some(&current), &current), None);
    }
}
