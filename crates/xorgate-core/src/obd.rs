//! OBD2 peripheral — the PURE half of `src/hardware/obd.ts` (558 lines), the
//! pure helpers of `src/agent/collectors/obd.ts` (788), `src/commands/obd.ts`
//! (117) and `obdStatusLine` (`src/commands/status.ts:124-137`), JS @ 09da38d.
//! The supervisor state machine itself is [`crate::obd_supervisor`].
//!
//! | TS @ 09da38d | here |
//! |---|---|
//! | `OBD_HELPER_STATES`, `OBD_EXIT` (`hardware/obd.ts:25-46`) | [`OBD_HELPER_STATES`], [`obd_exit`] |
//! | `parseObdLine` (`:112-199`, contract 1 + finite-number hygiene) | [`parse_obd_line`] → [`ObdMessage`] |
//! | `readObdEnvConfig`, `isObdConfigured` (`:228-249`) | [`read_obd_env_config`], [`ObdHelperConfig::is_configured`] |
//! | `resolveObdConfig` (`:281-324`) | [`resolve_obd_config`] |
//! | `normalizeScanCandidates` (`:347-366`) | [`normalize_scan_candidates`] |
//! | `buildObdHelperArgs` (`:417-425`) | [`build_obd_helper_args`] |
//! | `OBD_FIELD_UNITS` (`collectors/obd.ts:83-104`) | [`obd_field_unit`] |
//! | `boundInventory`, `obdReportDue`, `profileSummary`, `auditLine`, `requestRate` (`:203-278`) | same names |
//! | `parseObdArgs`, `formatScanTable`, `USAGE` (`commands/obd.ts`) | [`parse_obd_args`], [`format_scan_table`], [`OBD_USAGE`] |
//! | `obdStatusLine` (`commands/status.ts:132-137`) | [`obd_status_line`] |
//!
//! Every object this module builds from helper JSON keeps the key order
//! `JSON.parse` / `Object.fromEntries` give it in V8 (array-index keys first,
//! [`crate::js::object_key_order`]), so `effective.obd.profile`/`audit` print
//! byte-equal. Golden vectors: `fixtures/obd/pure.json` (dumped from the real
//! TS by `tools/replay/js/zz-p5b-dump-obd.test.ts`).

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::js::{number_to_string, object_key_order, parsed_key_order, to_fixed, trim};
use crate::round::js_math_round;
use crate::system::js_number;
use crate::telemetry::{js_num, js_num_opt, ObdStatus};

// ---------------------------------------------------------------------------
// contract 1 (hardware/obd.ts)
// ---------------------------------------------------------------------------

/// Helper states, verbatim from the contract's `status` line (`hardware/obd.ts:25-34`).
pub const OBD_HELPER_STATES: [&str; 8] = [
    "connecting",
    "pairing",
    "probing",
    "live",
    "ecu_silent",
    "backoff",
    "stopped",
    "adapter_not_found",
];

/// Helper exit codes (contract 1, `hardware/obd.ts:40-46`). 0 = clean exit on SIGINT/SIGTERM.
pub mod obd_exit {
    pub const NO_BLUETOOTH: i64 = 2;
    pub const ADAPTER_UNREACHABLE: i64 = 3;
    pub const PROTOCOL_REFUSED: i64 = 4;
    pub const STOP_CONDITION: i64 = 5;
    pub const SOFTWARE_BUG: i64 = 6;
}

/// Discovery window the scan helper runs (`OBD_SCAN_SECONDS`, `:336`).
pub const OBD_SCAN_SECONDS: u32 = 25;
/// Cap on candidates in the report (`OBD_SCAN_MAX_CANDIDATES`, `:338`).
pub const OBD_SCAN_MAX_CANDIDATES: usize = 20;
/// The helper's hard request cap (`MAX_POLL_HZ`, `:218`).
pub const MAX_POLL_HZ: f64 = 20.0;
/// Bound on `effective.obd.audit.inventory` (`MAX_INVENTORY_KEYS`, `collectors/obd.ts:73`).
pub const MAX_INVENTORY_KEYS: usize = 200;
/// Re-publish the report every Nth audit line (`REPORT_EVERY_AUDITS`, `:80`).
pub const REPORT_EVERY_AUDITS: u64 = 10;

/// `OBD_MAC_RE` (`daemon/settings.ts`): `^([0-9A-F]{2}:){5}[0-9A-F]{2}$` —
/// UPPERCASE only. The same closed alphabet guards every MAC that reaches a
/// subprocess argv (`removeObdBond` re-validates).
pub fn is_obd_mac(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 17
        && b.iter().enumerate().all(|(i, &c)| {
            if i % 3 == 2 {
                c == b':'
            } else {
                c.is_ascii_digit() || (b'A'..=b'F').contains(&c)
            }
        })
}

/// `/^[a-z_][a-z0-9_]*$/` — a helper field identifier.
pub fn is_obd_field_name(s: &str) -> bool {
    let mut it = s.bytes();
    match it.next() {
        Some(c) if c.is_ascii_lowercase() || c == b'_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

/// `last_stop` of an audit line: `{ ts?, reason? }` (key order ts, reason).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LastStop {
    pub ts: Option<f64>,
    pub reason: Option<String>,
}

impl LastStop {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(ts) = self.ts {
            m.insert("ts".into(), num(ts));
        }
        if let Some(r) = &self.reason {
            m.insert("reason".into(), Value::String(r.clone()));
        }
        Value::Object(m)
    }
}

/// A line emitted by the helper, typed per contract 1 (`ObdMessage`, `:79-91`).
#[derive(Debug, Clone, PartialEq)]
pub enum ObdMessage {
    Ready {
        helper: Option<String>,
        version: Option<String>,
        allowlist_sha256: Option<String>,
    },
    Status {
        /// One of [`OBD_HELPER_STATES`].
        state: String,
        link: u8,
        detail: Option<String>,
        ts: Option<f64>,
    },
    /// `{ type: "profile", ...rest }`: everything but `type`, in V8 key order.
    Profile(Map<String, Value>),
    Sample {
        /// Finite numbers only (C1), V8 key order; `link` still inside.
        fields: IndexMap<String, f64>,
        ts: Option<f64>,
    },
    Audit {
        counters: IndexMap<String, f64>,
        inventory: IndexMap<String, f64>,
        /// `null` when absent or not an object.
        last_stop: Option<LastStop>,
        ts: Option<f64>,
    },
    Error {
        code: f64,
        error: String,
    },
}

fn num(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn num_map(m: &IndexMap<String, f64>) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), num(*v))).collect())
}

impl ObdMessage {
    /// The TS object `parseObdLine` returns, as JSON (key order = the TS
    /// literals; fixtures compare it via `JSON.stringify`).
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        match self {
            ObdMessage::Ready {
                helper,
                version,
                allowlist_sha256,
            } => {
                m.insert("type".into(), "ready".into());
                if let Some(v) = helper {
                    m.insert("helper".into(), v.clone().into());
                }
                if let Some(v) = version {
                    m.insert("version".into(), v.clone().into());
                }
                if let Some(v) = allowlist_sha256 {
                    m.insert("allowlist_sha256".into(), v.clone().into());
                }
            }
            ObdMessage::Status {
                state,
                link,
                detail,
                ts,
            } => {
                m.insert("type".into(), "status".into());
                m.insert("state".into(), state.clone().into());
                m.insert("link".into(), (*link).into());
                if let Some(d) = detail {
                    m.insert("detail".into(), d.clone().into());
                }
                if let Some(t) = ts {
                    m.insert("ts".into(), num(*t));
                }
            }
            ObdMessage::Profile(rest) => {
                m.insert("type".into(), "profile".into());
                for (k, v) in rest {
                    m.insert(k.clone(), v.clone());
                }
                // `{ type, ...rest }` is a fresh object: V8 enumerates its
                // array-index keys first.
                return Value::Object(object_key_order(m.into_iter().collect()).into_iter().collect());
            }
            ObdMessage::Sample { fields, ts } => {
                m.insert("type".into(), "sample".into());
                m.insert("fields".into(), num_map(fields));
                if let Some(t) = ts {
                    m.insert("ts".into(), num(*t));
                }
            }
            ObdMessage::Audit {
                counters,
                inventory,
                last_stop,
                ts,
            } => {
                m.insert("type".into(), "audit".into());
                m.insert("counters".into(), num_map(counters));
                m.insert("inventory".into(), num_map(inventory));
                m.insert(
                    "last_stop".into(),
                    last_stop.as_ref().map_or(Value::Null, LastStop::to_json),
                );
                if let Some(t) = ts {
                    m.insert("ts".into(), num(*t));
                }
            }
            ObdMessage::Error { code, error } => {
                m.insert("type".into(), "error".into());
                m.insert("code".into(), num(*code));
                m.insert("error".into(), error.clone().into());
            }
        }
        Value::Object(m)
    }
}

fn str_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

fn number_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// `{k: v}` of the finite-number entries of a JSON object (V8 order already
/// applied by the caller); `None` when `v` is not an object.
fn finite_entries(v: Option<&Value>) -> Option<IndexMap<String, f64>> {
    let o = v?.as_object()?;
    Some(
        o.iter()
            .filter_map(|(k, v)| v.as_f64().filter(|f| f.is_finite()).map(|f| (k.clone(), f)))
            .collect(),
    )
}

/// `parseObdLine(line)` (`hardware/obd.ts:112-199`): one stdout line → a
/// typed message, or `None` for anything that is not a well-formed contract
/// line. `sample` fields and `audit` counters/inventory keep finite numbers
/// only (cloud constraint C1).
///
/// One divergence by construction: a JSON number literal beyond f64 range
/// (`1e400`) is `Infinity` in `JSON.parse` but a parse error for serde_json,
/// so such a line is dropped whole here. The helper never prints one
/// (`json.dumps` emits `Infinity` — not JSON — and the helper rounds every
/// value), so the difference is unreachable.
pub fn parse_obd_line(line: &str) -> Option<ObdMessage> {
    let t = trim(line);
    if !t.starts_with('{') {
        return None;
    }
    let obj = match serde_json::from_str::<Value>(t) {
        Ok(v) => parsed_key_order(v),
        Err(_) => return None,
    };
    let obj = obj.as_object()?;
    let ty = obj.get("type")?.as_str()?;
    match ty {
        "ready" => Some(ObdMessage::Ready {
            helper: str_of(obj.get("helper")),
            version: str_of(obj.get("version")),
            allowlist_sha256: str_of(obj.get("allowlist_sha256")),
        }),
        "status" => {
            let state = obj.get("state")?.as_str()?;
            if !OBD_HELPER_STATES.contains(&state) {
                return None;
            }
            let link = obj.get("link")?.as_f64()?;
            let link = match link {
                l if l == 0.0 => 0,
                l if l == 1.0 => 1,
                l if l == 2.0 => 2,
                _ => return None,
            };
            Some(ObdMessage::Status {
                state: state.to_string(),
                link,
                detail: str_of(obj.get("detail")),
                ts: number_of(obj.get("ts")),
            })
        }
        "profile" => Some(ObdMessage::Profile(
            obj.iter()
                .filter(|(k, _)| k.as_str() != "type")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )),
        "sample" => Some(ObdMessage::Sample {
            fields: finite_entries(obj.get("fields"))?,
            ts: number_of(obj.get("ts")),
        }),
        "audit" => {
            let counters = finite_entries(obj.get("counters"))?;
            let inventory = finite_entries(obj.get("inventory")).unwrap_or_default();
            let last_stop = obj.get("last_stop").and_then(Value::as_object).map(|ls| LastStop {
                ts: number_of(ls.get("ts")),
                reason: str_of(ls.get("reason")),
            });
            Some(ObdMessage::Audit {
                counters,
                inventory,
                last_stop,
                ts: number_of(obj.get("ts")),
            })
        }
        "error" => Some(ObdMessage::Error {
            code: obj.get("code")?.as_f64()?,
            error: str_of(obj.get("error")).unwrap_or_default(),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// helper config: settings namespace over the env seam
// ---------------------------------------------------------------------------

/// The `XORGATE_OBD_*` environment (the P1 seam, `readObdEnvConfig`): the raw
/// values, `None` = unset. Read once by the agent (`ObdEnv::from_lookup`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ObdEnv {
    #[serde(rename = "XORGATE_OBD_ADAPTER")]
    pub adapter: Option<String>,
    #[serde(rename = "XORGATE_OBD_SERIAL")]
    pub serial: Option<String>,
    #[serde(rename = "XORGATE_OBD_PIN")]
    pub pin: Option<String>,
    #[serde(rename = "XORGATE_OBD_POLL_HZ")]
    pub poll_hz: Option<String>,
    #[serde(rename = "XORGATE_OBD_FIELDS")]
    pub fields: Option<String>,
    #[serde(rename = "XORGATE_OBD_POWER_SOURCE")]
    pub power_source: Option<String>,
}

impl ObdEnv {
    /// The six `XORGATE_OBD_*` names, in TS doc order.
    pub const NAMES: [&'static str; 6] = [
        "XORGATE_OBD_ADAPTER",
        "XORGATE_OBD_SERIAL",
        "XORGATE_OBD_PIN",
        "XORGATE_OBD_POLL_HZ",
        "XORGATE_OBD_FIELDS",
        "XORGATE_OBD_POWER_SOURCE",
    ];

    /// Read the six names through `get` (the agent passes `std::env::var`).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            adapter: get("XORGATE_OBD_ADAPTER"),
            serial: get("XORGATE_OBD_SERIAL"),
            pin: get("XORGATE_OBD_PIN"),
            poll_hz: get("XORGATE_OBD_POLL_HZ"),
            fields: get("XORGATE_OBD_FIELDS"),
            power_source: get("XORGATE_OBD_POWER_SOURCE"),
        }
    }

    /// `(env.XORGATE_OBD_SERIAL ?? "").trim()` → `null` when empty.
    fn serial(&self) -> Option<String> {
        let s = trim(self.serial.as_deref().unwrap_or(""));
        (!s.is_empty()).then(|| s.to_string())
    }
}

/// `ObdHelperConfig` (`hardware/obd.ts:205-215`). Key order = the TS literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObdHelperConfig {
    /// Adapter MAC (uppercase); required unless `serial`.
    pub adapter: Option<String>,
    /// Emulator pty: overrides `adapter`.
    pub serial: Option<String>,
    pub pin: String,
    #[serde(with = "js_num")]
    pub poll_hz: f64,
    /// `null` = the helper's default (all fields).
    pub fields: Option<Vec<String>>,
    /// `"switched"` | `"constant"`.
    pub power_source: String,
}

impl ObdHelperConfig {
    /// `isObdConfigured(cfg)` (`:247-249`).
    pub fn is_configured(&self) -> bool {
        self.serial.is_some() || self.adapter.is_some()
    }
}

/// `readObdEnvConfig(env)` (`hardware/obd.ts:228-244`).
pub fn read_obd_env_config(env: &ObdEnv) -> ObdHelperConfig {
    let raw_mac = trim(env.adapter.as_deref().unwrap_or("")).to_uppercase();
    let adapter = is_obd_mac(&raw_mac).then_some(raw_mac);
    let pin = trim(env.pin.as_deref().unwrap_or(""));
    let pin = if pin.is_empty() { "1234" } else { pin }.to_string();
    let hz = js_number(env.poll_hz.as_deref().unwrap_or(""));
    let poll_hz = if hz.is_finite() && hz > 0.0 && hz <= MAX_POLL_HZ {
        hz
    } else {
        1.0
    };
    let fields: Vec<String> = env
        .fields
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(trim)
        .filter(|f| is_obd_field_name(f))
        .map(str::to_string)
        .collect();
    let ps = trim(env.power_source.as_deref().unwrap_or(""));
    ObdHelperConfig {
        adapter,
        serial: env.serial(),
        pin,
        poll_hz,
        fields: (!fields.is_empty()).then_some(fields),
        power_source: if ps == "constant" { "constant" } else { "switched" }.to_string(),
    }
}

/// The `obd` settings namespace as `parseObdSettings` leaves it (every field
/// optional; `adapterMac: null` kept as an explicit "no adapter"). The parser
/// itself (validation, clamps) is the settings loader's (WP1); this is the
/// shape [`resolve_obd_config`] reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObdSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// `None` = absent, `Some(None)` = `null`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub adapter_mac: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_source: Option<String>,
}

fn present_nullable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `Required<ObdSettings>` — the namespace as resolved (defaults filled), the
/// first six keys of `effective.obd`. Key order = `OBD_DEFAULTS`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObdSettingsResolved {
    pub enabled: bool,
    pub adapter_mac: Option<String>,
    pub pin: String,
    #[serde(with = "js_num")]
    pub poll_hz: f64,
    pub fields: Vec<String>,
    pub power_source: String,
}

/// `OBD_DEFAULTS` (`daemon/settings.ts`): the cloud contract's defaults.
pub fn obd_defaults() -> ObdSettingsResolved {
    ObdSettingsResolved {
        enabled: false,
        adapter_mac: None,
        pin: "1234".into(),
        poll_hz: 1.0,
        fields: vec![],
        power_source: "switched".into(),
    }
}

/// Why the helper must not run (`off`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObdOff {
    Disabled,
    NotConfigured,
}

impl ObdOff {
    pub fn as_str(self) -> &'static str {
        match self {
            ObdOff::Disabled => "disabled",
            ObdOff::NotConfigured => "not_configured",
        }
    }
}

/// Where the six namespace fields came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObdSource {
    Settings,
    Env,
}

impl ObdSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ObdSource::Settings => "settings",
            ObdSource::Env => "env",
        }
    }
}

/// `ObdResolvedConfig` (`hardware/obd.ts:273-279`). Key order = the TS literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObdResolvedConfig {
    pub run: bool,
    pub off: Option<ObdOff>,
    pub source: ObdSource,
    pub helper: ObdHelperConfig,
    pub settings: ObdSettingsResolved,
}

/// `resolveObdConfig(settings, env)` (`hardware/obd.ts:281-324`): the
/// namespace when present (env ignored, defaults fill the gaps, `enabled` is
/// the kill switch), else the env seam; `XORGATE_OBD_SERIAL` overrides the
/// adapter in both cases.
pub fn resolve_obd_config(settings: Option<&ObdSettings>, env: &ObdEnv) -> ObdResolvedConfig {
    let serial = env.serial();
    let Some(s) = settings else {
        let helper = read_obd_env_config(env);
        let run = helper.is_configured();
        let settings = ObdSettingsResolved {
            enabled: run,
            adapter_mac: helper.adapter.clone(),
            pin: helper.pin.clone(),
            poll_hz: helper.poll_hz,
            fields: helper.fields.clone().unwrap_or_default(),
            power_source: helper.power_source.clone(),
        };
        return ObdResolvedConfig {
            run,
            off: (!run).then_some(ObdOff::NotConfigured),
            source: ObdSource::Env,
            helper,
            settings,
        };
    };
    let d = obd_defaults();
    let resolved = ObdSettingsResolved {
        enabled: s.enabled.unwrap_or(d.enabled),
        adapter_mac: match &s.adapter_mac {
            None => d.adapter_mac,
            Some(v) => v.clone(),
        },
        pin: s.pin.clone().unwrap_or(d.pin),
        poll_hz: s.poll_hz.unwrap_or(d.poll_hz),
        fields: s.fields.clone().unwrap_or(d.fields),
        power_source: s.power_source.clone().unwrap_or(d.power_source),
    };
    let helper = ObdHelperConfig {
        adapter: resolved.adapter_mac.clone(),
        serial,
        pin: resolved.pin.clone(),
        poll_hz: resolved.poll_hz,
        fields: (!resolved.fields.is_empty()).then(|| resolved.fields.clone()),
        power_source: resolved.power_source.clone(),
    };
    let off = if !resolved.enabled {
        Some(ObdOff::Disabled)
    } else if helper.is_configured() {
        None
    } else {
        Some(ObdOff::NotConfigured)
    };
    ObdResolvedConfig {
        run: off.is_none(),
        off,
        source: ObdSource::Settings,
        helper,
        settings: resolved,
    }
}

/// `buildObdHelperArgs(cfg)` (`hardware/obd.ts:417-425`): the contract-1
/// flags. The PIN rides on argv, as in the TS.
pub fn build_obd_helper_args(cfg: &ObdHelperConfig) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(serial) = cfg.serial.as_deref().filter(|s| !s.is_empty()) {
        args.extend(["--serial".to_string(), serial.to_string()]);
    } else if let Some(adapter) = cfg.adapter.as_deref().filter(|s| !s.is_empty()) {
        args.extend([
            "--adapter".to_string(),
            adapter.to_string(),
            "--pin".to_string(),
            cfg.pin.clone(),
        ]);
    }
    args.extend(["--poll-hz".to_string(), number_to_string(cfg.poll_hz)]);
    if let Some(fields) = &cfg.fields {
        args.extend(["--fields".to_string(), fields.join(",")]);
    }
    args.extend(["--power-source".to_string(), cfg.power_source.clone()]);
    args
}

// ---------------------------------------------------------------------------
// scan (obd_scan)
// ---------------------------------------------------------------------------

pub use scan_types::{ObdScanCandidate, ObdScanResult};

mod scan_types {
    use super::*;

    /// One discovered device (`ObdScanCandidate`, `ts:src/hardware/obd.ts:329-333`
    /// @ 09da38d), as `effective.obd.scan.candidates[]` carries it. Key order =
    /// the TS literal `{ mac, name, rssi }`; `rssi` is `null` when unknown.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ObdScanCandidate {
        pub mac: String,
        pub name: String,
        /// `Math.round(rssi)` (a JS number) or `null`.
        #[serde(with = "js_num_opt")]
        pub rssi: Option<f64>,
    }

    /// Result of an `obd_scan` (`ObdScanResult`,
    /// `ts:src/agent/collectors/obd.ts:122-129` @ 09da38d), as
    /// `effective.obd.scan` carries it. `error` is present only when the scan
    /// helper failed (candidates then empty).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ObdScanResult {
        /// Epoch ms the scan finished (a JS number).
        #[serde(with = "js_num")]
        pub ts: f64,
        pub candidates: Vec<ObdScanCandidate>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<String>,
    }
}

/// `str.slice(0, n)` on UTF-16 code units (a split surrogate pair becomes
/// U+FFFD here, a lone surrogate in JS — unreachable for BlueZ names).
fn js_slice_utf16(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// `normalizeScanCandidates(raw)` (`hardware/obd.ts:347-366`): uppercase MAC,
/// dedup (first named occurrence wins), unnamed / MAC-named dropped, rssi
/// desc with `null` last (ties by MAC), cap 20. Garbage → `[]`.
pub fn normalize_scan_candidates(raw: &Value) -> Vec<ObdScanCandidate> {
    let Some(items) = raw.as_array() else {
        return vec![];
    };
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<ObdScanCandidate> = Vec::new();
    for item in items {
        let Some(o) = item.as_object() else { continue };
        let Some(mac) = o.get("mac").and_then(Value::as_str) else {
            continue;
        };
        let mac = trim(mac).to_uppercase();
        if !is_obd_mac(&mac) || seen.contains(&mac) {
            continue;
        }
        let name = o
            .get("name")
            .and_then(Value::as_str)
            .map(|n| js_slice_utf16(trim(n), 64))
            .unwrap_or_default();
        let upper = name.to_uppercase();
        if name.is_empty() || upper == mac || upper == mac.replace(':', "-") {
            continue;
        }
        let rssi = o
            .get("rssi")
            .and_then(Value::as_f64)
            .filter(|r| r.is_finite())
            .map(js_math_round);
        seen.insert(mac.clone());
        out.push(ObdScanCandidate { mac, name, rssi });
    }
    // `(b.rssi ?? -Infinity) - (a.rssi ?? -Infinity) || (a.mac < b.mac ? -1 : 1)`:
    // two nulls give NaN (falsy) → the MAC decides, like any other tie.
    out.sort_by(|a, b| {
        let ra = a.rssi.unwrap_or(f64::NEG_INFINITY);
        let rb = b.rssi.unwrap_or(f64::NEG_INFINITY);
        rb.partial_cmp(&ra)
            .filter(|o| o.is_ne())
            .unwrap_or_else(|| a.mac.cmp(&b.mac))
    });
    out.truncate(OBD_SCAN_MAX_CANDIDATES);
    out
}

// ---------------------------------------------------------------------------
// collector helpers (collectors/obd.ts)
// ---------------------------------------------------------------------------

/// `OBD_FIELD_UNITS[key]` (`collectors/obd.ts:83-104`): fields without a unit
/// are plain counts/codes.
pub fn obd_field_unit(key: &str) -> Option<&'static str> {
    Some(match key {
        "rpm" => "rpm",
        "speed" => "km/h",
        "coolant_temp" => "°C",
        "engine_load" => "%",
        "throttle" => "%",
        "pedal" => "%",
        "intake_temp" => "°C",
        "ambient_temp" => "°C",
        "intake_map" => "kPa",
        "baro" => "kPa",
        "fuel_level" => "%",
        "fuel_rate" => "L/h",
        "module_voltage" => "V",
        "adapter_voltage" => "V",
        "run_time" => "s",
        "odometer" => "km",
        "distance_since_clear" => "km",
        "engine_torque_pct" => "%",
        "reference_torque" => "Nm",
        "engine_torque" => "Nm",
        _ => return None,
    })
}

/// `AuditSnapshot` (`collectors/obd.ts:194-200`): the latest audit line as
/// the collector keeps it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AuditSnapshot {
    pub counters: IndexMap<String, f64>,
    /// Already bounded ([`bound_inventory`]).
    pub inventory: IndexMap<String, f64>,
    pub last_stop: Option<LastStop>,
    /// The helper's own stamp (epoch s), else the receive time in s.
    pub ts: f64,
}

impl AuditSnapshot {
    fn counter(&self, k: &str) -> f64 {
        self.counters.get(k).copied().unwrap_or(0.0)
    }

    /// `err_adapter + err_protocol + err_identity + err_voltage`.
    pub fn errors(&self) -> f64 {
        self.counter("err_adapter")
            + self.counter("err_protocol")
            + self.counter("err_identity")
            + self.counter("err_voltage")
    }

    /// `forbidden_attempts + budget_violations`.
    pub fn forbidden(&self) -> f64 {
        self.counter("forbidden_attempts") + self.counter("budget_violations")
    }
}

/// `boundInventory(inventory, max)` (`collectors/obd.ts:203-211`): keep the
/// `max` most-used commands (ties by key). The result is a fresh JS object, so
/// its keys enumerate in V8 order.
pub fn bound_inventory(inventory: &IndexMap<String, f64>, max: usize) -> IndexMap<String, f64> {
    if inventory.len() <= max {
        return object_key_order(inventory.clone());
    }
    let mut entries: Vec<(&String, &f64)> = inventory.iter().collect();
    entries.sort_by(|a, b| {
        b.1.partial_cmp(a.1)
            .filter(|o| o.is_ne())
            .unwrap_or_else(|| js_str_cmp(a.0, b.0))
    });
    object_key_order(
        entries
            .into_iter()
            .take(max)
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
    )
}

/// JS `a < b ? -1 : 1` on strings (UTF-16 code-unit order; equal keys cannot
/// occur in one object).
fn js_str_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    if a.encode_utf16().lt(b.encode_utf16()) {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    }
}

/// `obdReportDue(prev, next)` (`collectors/obd.ts:227-238`): does this status
/// change warrant a fresh config report?
pub fn obd_report_due(prev: Option<&ObdStatus>, next: Option<&ObdStatus>) -> bool {
    let (Some(p), Some(n)) = (prev, next) else {
        return prev.is_some() != next.is_some();
    };
    if p.link != n.link {
        return true;
    }
    let terminal = |s: &str| s == "parked" || s == "exited";
    if terminal(&p.state) != terminal(&n.state) {
        return true;
    }
    let pa = p.audit_seq.unwrap_or(0);
    let na = n.audit_seq.unwrap_or(0);
    if na != pa && (pa == 0 || na % REPORT_EVERY_AUDITS == 0) {
        return true;
    }
    if p.errors != n.errors || p.forbidden != n.forbidden {
        return true;
    }
    if p.voltage_low_events.unwrap_or(0) != n.voltage_low_events.unwrap_or(0) {
        return true;
    }
    p.last_stop.clone().flatten() != n.last_stop.clone().flatten()
}

/// `String(v)` / `${v}` of a JSON value.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Array(a) => js_join(a, ","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `arr.join(sep)`: `null`/`undefined` elements print as "".
fn js_join(a: &[Value], sep: &str) -> String {
    a.iter()
        .map(|v| if v.is_null() { String::new() } else { js_string(v) })
        .collect::<Vec<_>>()
        .join(sep)
}

/// JS truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// `x ?? fallback` for a JSON member (`null`/absent → fallback).
fn present<'a>(v: Option<&'a Value>) -> Option<&'a Value> {
    v.filter(|v| !v.is_null())
}

/// `.length` of an array member, 0 when absent (the helper always sends arrays).
fn arr_len(v: Option<&Value>) -> usize {
    present(v).and_then(Value::as_array).map_or(0, Vec::len)
}

/// `profileSummary(profile)` (`collectors/obd.ts:241-253`): one line for
/// `xorgate obd status`: protocol · responders · supported/total fields ·
/// powertrain [· ati]. Non-array list members (never sent by the helper)
/// count as empty.
pub fn profile_summary(profile: Option<&Map<String, Value>>) -> Option<String> {
    let p = profile?;
    let supported = arr_len(p.get("supported_fields"));
    let total = supported + arr_len(p.get("unsupported_fields"));
    let protocol = present(p.get("protocol"));
    let proto = protocol
        .and_then(|pr| present(pr.get("dp")).or_else(|| present(pr.get("dpn"))))
        .map_or_else(|| "protocol ?".to_string(), js_string);
    let responders = present(p.get("responders"))
        .and_then(Value::as_array)
        .map(|a| js_join(a, ","))
        .unwrap_or_default();
    let mut parts = vec![
        proto,
        format!(
            "ECUs {}",
            if responders.is_empty() {
                "none"
            } else {
                &responders
            }
        ),
        format!("{supported}/{total} fields"),
        present(p.get("powertrain")).map_or_else(|| "unknown".to_string(), js_string),
    ];
    if let Some(ati) = present(p.get("adapter"))
        .and_then(|a| a.get("ati"))
        .filter(|v| truthy(v))
    {
        parts.push(js_string(ati));
    }
    Some(parts.join(" · "))
}

/// `auditLine(seq, audit, rate)` (`collectors/obd.ts:256-267`): the journal's
/// 60 s line (without the `[obd] ` prefix).
pub fn audit_line(seq: u64, audit: &AuditSnapshot, rate: Option<f64>) -> String {
    let n = |k: &str| number_to_string(audit.counters.get(k).copied().unwrap_or(0.0));
    format!(
        "audit #{seq}: req {} ({} req/s) ok {} no_data {} timeouts {} err {} forbidden {} reconnects {} voltage_low {} inventory {} last_stop {}",
        n("requests"),
        rate.map_or_else(|| "—".to_string(), |r| to_fixed(r, 1)),
        n("ok"),
        n("no_data"),
        n("timeouts"),
        number_to_string(audit.errors()),
        number_to_string(audit.forbidden()),
        n("reconnects"),
        n("voltage_low_events"),
        audit.inventory.len(),
        audit
            .last_stop
            .as_ref()
            .and_then(|l| l.reason.as_deref())
            .unwrap_or("—"),
    )
}

/// `requestRate(prev, cur)` (`collectors/obd.ts:270-278`): requests per second
/// between two audit snapshots, `None` until two exist / on a counter reset.
pub fn request_rate(prev: Option<&AuditSnapshot>, cur: Option<&AuditSnapshot>) -> Option<f64> {
    let cur_req = *cur?.counters.get("requests")?;
    let prev_req = *prev?.counters.get("requests")?;
    let dt = cur?.ts - prev?.ts;
    if !(dt > 0.0) {
        return None;
    }
    let dr = cur_req - prev_req;
    if dr < 0.0 {
        return None;
    }
    Some(js_math_round((dr / dt) * 10.0) / 10.0)
}

/// `obdStatusLine(obd)` (`commands/status.ts:132-137`): the `obd` status line,
/// e.g. `obd        live  link 2  10.0 req/s  err 0  forbidden 0  14.1 V  adapter …`.
pub fn obd_status_line(obd: &ObdStatus) -> String {
    let rate = obd
        .req_per_sec
        .map_or_else(|| "— req/s".to_string(), |r| format!("{} req/s", to_fixed(r, 1)));
    let volts = obd
        .voltage
        .map_or_else(|| "— V".to_string(), |v| format!("{} V", to_fixed(v, 1)));
    let adapter = match obd.adapter.as_ref().and_then(Option::as_deref) {
        Some(a) if !a.is_empty() => format!("  adapter {a}"),
        _ => String::new(),
    };
    format!(
        "obd        {}  link {}  {rate}  err {}  forbidden {}  {volts}{adapter}",
        obd.state, obd.link, obd.errors, obd.forbidden
    )
}

// ---------------------------------------------------------------------------
// `xorgate obd …` (commands/obd.ts)
// ---------------------------------------------------------------------------

/// `USAGE` (`commands/obd.ts:19-26`). The trailing `xorgate` names are the
/// local CLI's (D34: `xorgate-rs` in the Rust binary's help).
pub fn obd_usage(bin: &str) -> String {
    format!(
        "Usage: {bin} obd scan [--seconds N] [--json]
       {bin} obd status

scan    Bluetooth discovery for N seconds (default {OBD_SCAN_SECONDS}, 1..120), strongest
        signal first, unnamed devices dropped. Never pairs. --json prints the
        exact candidate list the agent publishes in effective.obd.scan.
status  The agent's obd line (state, link, req/s, errors, forbidden, voltage,
        adapter) and the last vehicle profile the helper reported."
    )
}

/// `parseObdArgs(rest)` result.
#[derive(Debug, Clone, PartialEq)]
pub enum ObdArgs {
    Scan { seconds: f64, json: bool },
    Status,
    Help,
    Error(String),
}

impl ObdArgs {
    /// The TS object, as JSON (fixtures).
    pub fn to_json(&self) -> Value {
        match self {
            ObdArgs::Scan { seconds, json } => {
                serde_json::json!({"cmd": "scan", "seconds": num(*seconds), "json": json})
            }
            ObdArgs::Status => serde_json::json!({"cmd": "status"}),
            ObdArgs::Help => serde_json::json!({"cmd": "help"}),
            ObdArgs::Error(m) => serde_json::json!({"cmd": "error", "message": m}),
        }
    }
}

/// `parseObdArgs(rest)` (`commands/obd.ts:29-51`).
pub fn parse_obd_args(rest: &[String]) -> ObdArgs {
    let Some(sub) = rest.first().map(String::as_str) else {
        return ObdArgs::Help;
    };
    if sub.is_empty() || sub == "--help" || sub == "-h" || sub == "help" {
        return ObdArgs::Help;
    }
    if sub == "status" {
        return ObdArgs::Status;
    }
    if sub != "scan" {
        return ObdArgs::Error(format!("unknown obd subcommand '{sub}'"));
    }
    let args = &rest[1..];
    let json = args.iter().any(|a| a == "--json");
    let i = args.iter().position(|a| a == "--seconds");
    let eq = args.iter().find(|a| a.starts_with("--seconds="));
    let raw: Option<&str> = match i {
        Some(i) => args.get(i + 1).map(String::as_str),
        None => eq.map(|a| &a["--seconds=".len()..]),
    };
    let mut seconds = f64::from(OBD_SCAN_SECONDS);
    if i.is_some() || eq.is_some() {
        // `Number(undefined)` is NaN (a trailing `--seconds`).
        let n = raw.map_or(f64::NAN, js_number);
        if !n.is_finite() || !(1.0..=120.0).contains(&n) {
            return ObdArgs::Error(format!(
                "--seconds must be 1..120, got '{}'",
                raw.unwrap_or("")
            ));
        }
        seconds = n;
    }
    ObdArgs::Scan { seconds, json }
}

/// `${n}`.padStart(4) for an rssi.
fn pad_start(s: &str, width: usize) -> String {
    let len = s.encode_utf16().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - len))
    }
}

/// `formatScanTable(candidates)` (`commands/obd.ts:54-63`). `bin` names the
/// local CLI in the trailing hint (D34).
pub fn format_scan_table(candidates: &[ObdScanCandidate], bin: &str) -> String {
    if candidates.is_empty() {
        return "no named Bluetooth devices found (is the adapter powered? ELM327 dongles need the car's OBD port live)".into();
    }
    let mut lines = vec!["MAC                RSSI   NAME".to_string()];
    for c in candidates {
        let rssi = match c.rssi {
            None => "   —".to_string(),
            Some(r) => pad_start(&number_to_string(r), 4),
        };
        lines.push(format!("{}  {rssi}   {}", c.mac, c.name));
    }
    lines.push(String::new());
    lines.push(format!(
        "pick the adapter by its label, then: {bin} set obd.adapter <MAC>   (never pairs by name)"
    ));
    lines.join("\n")
}

/// `xorgate obd status` when state.json has no `obd` (`commands/obd.ts:86-88`).
pub const OBD_STATUS_ABSENT: &str =
    "obd        (no obd collector: agent not running, or the hardware block declares no peripherals.obd)";

/// The lines after the status line of `xorgate obd status`
/// (`commands/obd.ts:90-101`), `since` rendered by the caller's
/// `toISOString`.
pub fn obd_status_detail_lines(obd: &ObdStatus, since_iso: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(d) = obd.detail.as_deref().filter(|d| !d.is_empty()) {
        out.push(format!("  detail     {d}"));
    }
    out.push(format!("  since      {since_iso}"));
    out.push(format!(
        "  helper     {}",
        obd.helper_version.as_deref().unwrap_or("—")
    ));
    match obd.profile.as_ref().and_then(Option::as_deref) {
        Some(p) if !p.is_empty() => out.push(format!("  profile    {p}")),
        _ => out.push("  profile    — (no vehicle probed yet)".to_string()),
    }
    if let Some(seq) = obd.audit_seq {
        out.push(format!(
            "  audit      #{seq}  voltage_low {}  last_stop {}",
            obd.voltage_low_events.unwrap_or(0),
            obd.last_stop.clone().flatten().unwrap_or_else(|| "—".into())
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_result_key_order_and_optional_error() {
        let r = ObdScanResult {
            ts: 1_700_000_000_000.0,
            candidates: vec![ObdScanCandidate {
                mac: "AA:BB:CC:DD:EE:FF".into(),
                name: "OBDII".into(),
                rssi: None,
            }],
            error: None,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"ts":1700000000000,"candidates":[{"mac":"AA:BB:CC:DD:EE:FF","name":"OBDII","rssi":null}]}"#
        );
        let e = ObdScanResult {
            ts: 1.0,
            candidates: vec![],
            error: Some("boom".into()),
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"ts":1,"candidates":[],"error":"boom"}"#
        );
    }

    #[test]
    fn mac_alphabet_is_closed_and_uppercase() {
        assert!(is_obd_mac("10:21:3E:4F:33:65"));
        assert!(!is_obd_mac("10:21:3e:4f:33:65"));
        assert!(!is_obd_mac("10-21-3E-4F-33-65"));
        assert!(!is_obd_mac("10:21:3E:4F:33:6"));
        assert!(!is_obd_mac("10:21:3E:4F:33:65 "));
        assert!(!is_obd_mac("10:21:3E:4F:33:65;rm"));
        assert!(!is_obd_mac(""));
    }
}
