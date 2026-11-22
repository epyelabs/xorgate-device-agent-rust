//! `xorgate set <key> <value>` — the PURE half of `src/commands/set.ts`
//! (`:94-671`): argument split, key routing, every validation message
//! (verbatim), and the settings.json mutation each key performs, expressed
//! over the loaded document as a JSON object so the bytes the CLI writes are
//! the bytes `JSON.stringify` writes.
//!
//! Why a JSON object and not [`AgentSettings`]: the TS mutates the object
//! `loadSettings()` returned with object spread / property assignment
//! (`{ ...loadSettings() }`, `next.mode = value`, `{ ...current, [section]: … }`)
//! and saves it WITHOUT re-parsing. A namespace or field that was absent is
//! therefore APPENDED after the existing keys (`{"cellular":{…},"mode":"real"}`),
//! while the typed struct would always serialise in parser order. The loaded
//! document itself is in parser order (`parseSettingsObject` inserts keys in a
//! fixed order and so does [`AgentSettings`]' serde), so `to_value(loaded)` is
//! exactly `loadSettings()`; the mutations below reproduce JS insertion order
//! (an existing key keeps its slot, a new key goes last; V8 enumerates
//! array-index keys first, see [`crate::js::object_key_order`]).
//!
//! The I/O (settings read/write, the sudo facts, `inspect()` for `mode`, the
//! modem telemetry reads and the clock of the APN watch) lives in
//! `xorgate-agent::cli::set`, which calls these in the TS order.

use serde_json::{Map, Value};

use crate::config_report::{parse_apn_telemetry, ApnBlock};
use crate::js::{number_to_string, object_key_order, to_json_string};
use crate::settings::{
    apn_re_test, js_assign_drops_key, parse_camera_mount_settings, parse_obd_settings,
    parse_recording_settings, parse_time_sync_settings, stream_key_re_test, AgentSettings,
    OBD_POLL_HZ_MAX, OBD_POLL_HZ_MIN, RECORDING_RATE_HZ_MAX, RECORDING_RATE_HZ_MIN,
};
use crate::system::js_number;
use crate::types::ApnState;
use crate::video_presets::{find_video_preset, video_preset_names};

// ---------------------------------------------------------------------------
// verbatim texts
// ---------------------------------------------------------------------------

/// `USAGE` (`set.ts:107-108` @ 09da38d).
pub const USAGE: &str = "usage: xorgate set <key> <value>   (supported: mode <sim|real>, gps-antenna <active|passive>, gps-fix-reset-policy <defer|veto-deadzone>, deadzone-profile <conservative|balanced|aggressive>, apn <value|\"\"> [--no-wait], wifi <on|off> [--force], recording.<group>.<field> <value>, timeSync.<field> <value>, cameraMount.<streamKey>.rotationDeg <0|180>, obd.<enabled|adapter|pin|poll-hz|fields|power-source> <value>)";

/// The unknown-key message of `setCommand` (`set.ts:601-603` @ 09da38d).
pub const UNKNOWN_SETTING_SUPPORTED: &str = "(supported: mode, gps-antenna, gps-fix-reset-policy, deadzone-profile, apn, wifi, recording.*, timeSync.*, cameraMount.*, obd.*)";

const RECORDING_LIVE: &str =
    "  the agent applies recording changes live within ~5 s (no restart needed)";
const MODEM_LIVE_MINUTE: &str =
    "  the modem daemon applies it live within ~a minute (journalctl -u sim7600-lte)";

/// `set.ts:511` — `--no-restart` after `mode`.
pub const MODE_NO_RESTART: &str = "  --no-restart: applies on the next daemon start";
/// `set.ts:517` — `mode` with no running daemon.
pub const MODE_NOT_RUNNING: &str = "  daemon not running — applies on next start (xorgate start)";
/// `mode` while an agent IS running. The TS restarts its self-managed daemon
/// (`set.ts:523-530`: `restart(status.record.argv)`); the Rust agent has no
/// self-managed daemon (README §3.3: systemd only), so it says how to apply
/// and exits 1 — the shape of the TS's own "could not restart" branch
/// (`:527-529`). Deliberate deviation, WP6 evidence ambiguity A3.
pub const MODE_RESTART_UNAVAILABLE: [&str; 2] = [
    "  could not restart automatically: xorgate-rs runs the agent under systemd only",
    "  on systemd installs, run: sudo systemctl restart xorgate-agent-rs",
];

// ---------------------------------------------------------------------------
// output lines
// ---------------------------------------------------------------------------

/// `console.log` (stdout) or `console.error` (stderr).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

/// One `console.*` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub stream: Stream,
    pub text: String,
}

impl Line {
    pub fn out(t: impl Into<String>) -> Self {
        Line {
            stream: Stream::Out,
            text: t.into(),
        }
    }
    pub fn err(t: impl Into<String>) -> Self {
        Line {
            stream: Stream::Err,
            text: t.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// arguments + routing (set.ts:391-505)
// ---------------------------------------------------------------------------

/// The parsed `set` argument list (`set.ts:392-396`, `:456`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetArgs {
    pub no_restart: bool,
    pub force: bool,
    pub no_wait: bool,
    /// `positionals[0]` — the first argument not starting with `-`.
    pub key: Option<String>,
    /// `positionals[1]` (so a negative number like `-3` is NOT a value).
    pub value: Option<String>,
}

/// `setCommand`'s argument split (`set.ts:392-396`, `--no-wait` from `:456`).
pub fn parse_set_args(rest: &[String]) -> SetArgs {
    let has = |f: &str| rest.iter().any(|a| a == f);
    let mut positionals = rest.iter().filter(|a| !a.starts_with('-'));
    SetArgs {
        no_restart: has("--no-restart"),
        force: has("--force"),
        no_wait: has("--no-wait"),
        key: positionals.next().cloned(),
        value: positionals.next().cloned(),
    }
}

/// Where `setCommand` sends a key (`set.ts:405-505`), in its test order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `:405` — handled BEFORE the sudo note.
    Wifi,
    Recording,
    TimeSync,
    CameraMount,
    /// `cameraSensors.*` (RUST-ONLY: the JS CLI has no such key).
    CameraSensors,
    /// `obd.*` (`set.ts:528-532` @ 09da38d).
    Obd,
    Mode,
    Media,
    Apn,
    GpsAntenna,
    GpsFixResetPolicy,
    DeadZoneProfile,
    Unknown,
}

/// `set.ts:405-505`.
pub fn route(key: &str) -> Route {
    if key == "wifi" {
        Route::Wifi
    } else if key.starts_with("recording.") {
        Route::Recording
    } else if key.starts_with("timeSync.") {
        Route::TimeSync
    } else if key.starts_with("cameraMount.") {
        Route::CameraMount
    } else if key.starts_with("cameraSensors.") {
        Route::CameraSensors
    } else if key.starts_with("obd.") {
        Route::Obd
    } else {
        match key {
            "mode" => Route::Mode,
            "media" => Route::Media,
            "apn" => Route::Apn,
            "gps-antenna" => Route::GpsAntenna,
            "gps-fix-reset-policy" => Route::GpsFixResetPolicy,
            "deadzone-profile" => Route::DeadZoneProfile,
            _ => Route::Unknown,
        }
    }
}

/// `parseBoolValue` (`set.ts:98-102`).
pub fn parse_bool_value(v: Option<&str>) -> Option<bool> {
    match v {
        Some("on" | "true" | "1") => Some(true),
        Some("off" | "false" | "0") => Some(false),
        _ => None,
    }
}

/// `warnIfNeedlessSudo`'s decision (`set.ts:382-389`) over the facts the
/// caller gathered: `is_root`, the uid owning the first of
/// `agent.pid` / `XORGATE_HOME` / `settings.json` that can be stat'd, and that
/// uid's login name from /etc/passwd. Empty when no note is due.
pub fn sudo_note(is_root: bool, owner_uid: Option<u32>, owner_name: Option<&str>) -> Vec<Line> {
    if !is_root {
        return Vec::new();
    }
    let Some(uid) = owner_uid.filter(|u| *u != 0) else {
        return Vec::new();
    };
    let who = owner_name
        .map(str::to_string)
        .unwrap_or_else(|| format!("uid {uid}"));
    vec![
        Line::err(format!(
            "note: the xorgate agent runs as '{who}' — you don't need sudo for settings."
        )),
        Line::err(format!(
            "  writing anyway; settings.json keeps its '{who}' ownership."
        )),
    ]
}

/// `usernameForUid` (`privilege.ts`): the login name of `uid` in an
/// `/etc/passwd` text (`name:x:uid:…`, an empty uid field never matches).
pub fn username_for_uid(passwd: &str, uid: u32) -> Option<String> {
    passwd.split('\n').find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() >= 3 && !f[0].is_empty() && !f[2].is_empty() && js_number(f[2]) == uid as f64)
            .then(|| f[0].to_string())
    })
}

// ---------------------------------------------------------------------------
// the document (`{ ...loadSettings() }`) and JS object spread
// ---------------------------------------------------------------------------

/// `{ ...loadSettings() }`: the loaded settings as the JS object the TS
/// mutates (parser key order, optional fields omitted).
pub fn document(loaded: &AgentSettings) -> Map<String, Value> {
    match serde_json::to_value(loaded) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// `{ ...base, ...overlay }` — own enumerable keys of `base` (objects only;
/// `undefined`/primitives spread nothing), then `overlay`'s, an existing key
/// keeping its slot; V8 own-key order (array-index keys first).
fn spread(base: Option<&Value>, overlay: Map<String, Value>) -> Map<String, Value> {
    let mut out: Map<String, Value> = match base {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    for (k, v) in overlay {
        out.insert(k, v);
    }
    object_key_order(out.into_iter().collect())
        .into_iter()
        .collect()
}

fn one(k: &str, v: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(k.to_string(), v);
    m
}

/// `obj.key = value` on the top-level document (a present key keeps its slot,
/// a new one is appended — every top-level key is a namespace name, so V8's
/// index-key rule never applies).
fn assign(doc: &mut Map<String, Value>, key: &str, value: Value) {
    doc.insert(key.to_string(), value);
}

fn num_value(n: f64) -> Value {
    serde_json::Number::from_f64(n)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// `value ?? ""` in the messages.
fn or_empty(v: Option<&str>) -> &str {
    v.unwrap_or("")
}

/// `value !== undefined && value !== "" ? Number(value) : NaN` (`set.ts:130`, `:335`).
fn cli_number(v: Option<&str>) -> f64 {
    match v {
        Some(s) if !s.is_empty() => js_number(s),
        _ => f64::NAN,
    }
}

// ---------------------------------------------------------------------------
// key handlers
// ---------------------------------------------------------------------------

/// What a handler decided.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// Print these on stderr, exit 1, write nothing.
    Fail(Vec<String>),
    /// Write `doc` as settings.json; on success print `stdout`, then `then`.
    Save {
        doc: Map<String, Value>,
        stdout: Vec<String>,
        then: Then,
    },
}

/// What follows a successful save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Then {
    Done,
    /// `mode` without `--no-restart` (`set.ts:515-530`): inspect the daemon.
    ModeRestart,
}

fn fail(msg: impl Into<String>) -> KeyOutcome {
    KeyOutcome::Fail(vec![msg.into()])
}

/// Every key except `wifi` (Phase 4) and `apn` (see [`apn_validate`]):
/// `setRecordingCommand` (`set.ts:109-219`), `setCameraEnable` (`:227-254`),
/// `setTimeSyncCommand` (`:316-365`), `setCameraMountCommand` (`:266-308`)
/// and the `mode` / `media` / `gps-antenna` / `gps-fix-reset-policy` /
/// `deadzone-profile` / unknown branches of `setCommand` (`:433-509`).
/// `loaded` is `loadSettings()`.
pub fn apply(args: &SetArgs, loaded: &AgentSettings) -> KeyOutcome {
    let key = args.key.as_deref().unwrap_or("");
    let value = args.value.as_deref();
    match route(key) {
        Route::Recording => set_recording(key, value, loaded),
        Route::TimeSync => set_time_sync(key, value, loaded),
        Route::CameraMount => set_camera_mount(key, value, loaded),
        Route::CameraSensors => {
            set_camera_sensors(key, value, loaded, crate::camera_catalog::catalog())
        }
        Route::Obd => set_obd(key, value, loaded),
        Route::Mode => set_mode(value, args.no_restart, loaded),
        Route::Media => fail(
            "'media' was removed: turn cameras off individually instead, e.g. xorgate set recording.video.cameras.cam0 off",
        ),
        Route::GpsAntenna => {
            let v = match value {
                Some(v @ ("active" | "passive")) => v,
                _ => {
                    return fail(format!(
                        "gps-antenna must be 'active' or 'passive', got '{}'",
                        or_empty(value)
                    ))
                }
            };
            let mut doc = document(loaded);
            assign(&mut doc, "gnssAntBias", Value::Bool(v == "active"));
            KeyOutcome::Save {
                doc,
                stdout: vec![
                    format!(
                        "✓ GPS antenna set to {v} (bias {}, persisted to settings.json)",
                        if v == "active" { "on" } else { "off" }
                    ),
                    MODEM_LIVE_MINUTE.to_string(),
                ],
                then: Then::Done,
            }
        }
        Route::GpsFixResetPolicy => {
            let v = match value {
                Some(v @ ("defer" | "veto-deadzone")) => v,
                _ => {
                    return fail(format!(
                        "gps-fix-reset-policy must be 'defer' or 'veto-deadzone', got '{}'",
                        or_empty(value)
                    ))
                }
            };
            lte_recovery(
                loaded,
                "gpsFixResetPolicy",
                v,
                vec![
                    format!("✓ GPS fix reset policy set to {v} (persisted to settings.json)"),
                    "  the modem daemon reads it live at its next recovery decision (journalctl -u sim7600-lte)".to_string(),
                ],
            )
        }
        Route::DeadZoneProfile => {
            let v = match value {
                Some(v @ ("conservative" | "balanced" | "aggressive")) => v,
                _ => {
                    return fail(format!(
                        "deadzone-profile must be 'conservative', 'balanced' or 'aggressive', got '{}'",
                        or_empty(value)
                    ))
                }
            };
            lte_recovery(
                loaded,
                "deadZoneProfile",
                v,
                vec![
                    format!("✓ Dead-zone profile set to {v} (persisted to settings.json)"),
                    "  the modem daemon reads it live at its next dead-zone nudge (journalctl -u sim7600-lte)".to_string(),
                ],
            )
        }
        Route::Unknown | Route::Wifi | Route::Apn => fail(format!(
            "unknown setting '{key}' {UNKNOWN_SETTING_SUPPORTED}"
        )),
    }
}

/// `next.lteRecovery = { ...next.lteRecovery, <field>: value }` (`set.ts:479`, `:494`).
fn lte_recovery(loaded: &AgentSettings, field: &str, v: &str, stdout: Vec<String>) -> KeyOutcome {
    let mut doc = document(loaded);
    let merged = spread(doc.get("lteRecovery"), one(field, Value::String(v.into())));
    assign(&mut doc, "lteRecovery", Value::Object(merged));
    KeyOutcome::Save {
        doc,
        stdout,
        then: Then::Done,
    }
}

/// `set.ts:437-444` + `:507-513`.
fn set_mode(value: Option<&str>, no_restart: bool, loaded: &AgentSettings) -> KeyOutcome {
    let v = match value {
        Some(v @ ("sim" | "real")) => v,
        _ => {
            return fail(format!(
                "mode must be 'sim' or 'real', got '{}'",
                or_empty(value)
            ))
        }
    };
    let mut doc = document(loaded);
    assign(&mut doc, "mode", Value::String(v.into()));
    let mut stdout = vec![format!("✓ mode set to {v} (persisted to settings.json)")];
    let then = if no_restart {
        stdout.push(MODE_NO_RESTART.to_string());
        Then::Done
    } else {
        Then::ModeRestart
    };
    KeyOutcome::Save { doc, stdout, then }
}

/// `setRecordingCommand` (`set.ts:109-219`).
fn set_recording(key: &str, value: Option<&str>, loaded: &AgentSettings) -> KeyOutcome {
    let parts: Vec<&str> = key.split('.').collect();
    let section = parts.get(1).copied().unwrap_or("");
    let field = parts.get(2).copied().unwrap_or("");
    if parts.len() == 4 && section == "video" && field == "cameras" {
        return set_camera_enable(key, parts[3], value, loaded);
    }
    if parts.len() != 3 || section.is_empty() || field.is_empty() {
        return fail(
            "recording keys are dotted paths, e.g. xorgate set recording.telemetry.enabled on",
        );
    }
    let shown = or_empty(value);
    let bool_v = parse_bool_value(value);
    let num = cli_number(value);
    let raw: Value = match field {
        "enabled" => match bool_v {
            Some(b) => Value::Bool(b),
            None => return fail(format!("{key} must be on|off, got '{shown}'")),
        },
        "rateHz" => {
            if !num.is_finite() || num < RECORDING_RATE_HZ_MIN || num > RECORDING_RATE_HZ_MAX {
                return fail(format!(
                    "{key} must be a number in [{}, {}], got '{shown}'",
                    number_to_string(RECORDING_RATE_HZ_MIN),
                    number_to_string(RECORDING_RATE_HZ_MAX)
                ));
            }
            num_value(num)
        }
        "segmentSeconds"
        | "maxBytes"
        | "minFreeBytes"
        | "bootDeferMaxSecs"
        | "bootDeferMinBudgetMa"
        | "gpsGateMaxSecs"
        | "gpsGateNoProgressSecs"
        | "spawnStaggerSecs"
        | "uvSamples"
        | "uvWindowSecs"
        | "railLowVolts"
        | "railLowSecs"
        | "escalateDwellSecs"
        | "escalateDutyPct"
        | "restoreSecs"
        | "railRestoreVolts"
        | "maxShedsPerHour"
        | "cpuTempC"
        | "cpuTempRestoreC"
        | "cpuTempSecs" => {
            if !num.is_finite() || num < 0.0 {
                return fail(format!(
                    "{key} must be a non-negative number, got '{shown}'"
                ));
            }
            num_value(num)
        }
        "bootDefer" | "gpsGate" => match value {
            Some(v @ ("auto" | "always" | "off")) => Value::String(v.into()),
            _ => return fail(format!("{key} must be auto|always|off, got '{shown}'")),
        },
        "preset" => match value {
            Some(v) if !v.is_empty() && find_video_preset(v).is_some() => Value::String(v.into()),
            _ => {
                return fail(format!(
                    "{key} must be one of: {} (got '{shown}')",
                    video_preset_names().join(", ")
                ))
            }
        },
        // string fields (upload.mode) — validated by the parser below
        _ => value.map_or(Value::Null, |v| Value::String(v.into())),
    };

    // parseRecordingSettings({ [section]: { [field]: raw } }) — `raw` is
    // `undefined` (absent) when no value was given for a string field.
    let mut inner = Map::new();
    if value.is_some() || !raw.is_null() {
        inner.insert(field.to_string(), raw);
    }
    let patch_src = Value::Object(one(section, Value::Object(inner)));
    let patched_section = parse_recording_settings(&patch_src)
        .and_then(|p| serde_json::to_value(p).ok())
        .and_then(|v| v.get(section).cloned());
    let Some(Value::Object(patched_section)) = patched_section else {
        return fail(format!(
            "unknown or invalid recording setting '{key}' = '{shown}'"
        ));
    };
    let Some(applied) = patched_section.get(field).cloned() else {
        return fail(format!(
            "unknown or invalid recording setting '{key}' = '{shown}'"
        ));
    };

    let mut doc = document(loaded);
    let current = doc.get("recording").cloned();
    let merged_section = spread(
        current.as_ref().and_then(|c| c.get(section)),
        patched_section,
    );
    let recording = spread(
        current.as_ref(),
        one(section, Value::Object(merged_section)),
    );
    assign(&mut doc, "recording", Value::Object(recording));
    KeyOutcome::Save {
        doc,
        stdout: vec![
            format!(
                "✓ {key} = {} (persisted to settings.json)",
                to_json_string(&applied)
            ),
            RECORDING_LIVE.to_string(),
        ],
        then: Then::Done,
    }
}

/// `setCameraEnable` (`set.ts:227-254`).
fn set_camera_enable(
    key: &str,
    stream_key: &str,
    value: Option<&str>,
    loaded: &AgentSettings,
) -> KeyOutcome {
    let b = parse_bool_value(value);
    if !stream_key_re_test(stream_key) {
        return fail(format!(
            "'{stream_key}' is not a valid stream key (see kvs.json channels)"
        ));
    }
    let Some(b) = b else {
        return fail(format!("{key} must be on|off, got '{}'", or_empty(value)));
    };
    let mut doc = document(loaded);
    let recording = doc.get("recording").cloned();
    let video = recording.as_ref().and_then(|r| r.get("video")).cloned();
    let cameras = spread(
        video.as_ref().and_then(|v| v.get("cameras")),
        one(stream_key, Value::Bool(b)),
    );
    let video = spread(video.as_ref(), one("cameras", Value::Object(cameras)));
    let recording = spread(recording.as_ref(), one("video", Value::Object(video)));
    assign(&mut doc, "recording", Value::Object(recording));
    KeyOutcome::Save {
        doc,
        stdout: vec![
            format!("✓ {key} = {b} (persisted to settings.json)"),
            if b {
                RECORDING_LIVE.to_string()
            } else {
                "  camera disabled: no capture, no live stream, no recording (live within ~5 s)"
                    .to_string()
            },
        ],
        then: Then::Done,
    }
}

/// `setCameraMountCommand` (`set.ts:266-308`).
fn set_camera_mount(key: &str, value: Option<&str>, loaded: &AgentSettings) -> KeyOutcome {
    let parts: Vec<&str> = key.split('.').collect();
    let stream_key = parts.get(1).copied().unwrap_or("");
    if parts.len() != 3 || stream_key.is_empty() || parts[2] != "rotationDeg" {
        return fail(
            "cameraMount keys are dotted paths, e.g. xorgate set cameraMount.cam0.rotationDeg 180",
        );
    }
    if !stream_key_re_test(stream_key) {
        return fail(format!(
            "'{stream_key}' is not a valid stream key (see kvs.json channels)"
        ));
    }
    let deg: u32 = match value {
        Some("180") => 180,
        Some("0") => 0,
        _ => return fail(format!("{key} must be 0 or 180, got '{}'", or_empty(value))),
    };
    let entry = Value::Object(one("rotationDeg", Value::from(deg)));
    let patch_src = Value::Object(one(
        "cameras",
        Value::Object(one(stream_key, entry.clone())),
    ));
    // `patch?.cameras?.[streamKey]` (`set.ts:291`): for `__proto__` the read
    // returns the map's (inherited) prototype, always truthy, though the
    // parser dropped the key (`settings::js_assign_drops_key`); the write
    // below is a computed-key literal (`:299`), so it lands as an own key.
    let ok = js_assign_drops_key(stream_key)
        || parse_camera_mount_settings(&patch_src)
            .and_then(|p| p.cameras)
            .is_some_and(|c| c.contains_key(stream_key));
    if !ok {
        return fail(format!(
            "unknown or invalid cameraMount setting '{key}' = '{}'",
            or_empty(value)
        ));
    }
    let mut doc = document(loaded);
    let current = doc
        .get("cameraMount")
        .and_then(|c| c.get("cameras"))
        .cloned();
    let cameras = spread(current.as_ref(), one(stream_key, entry));
    assign(
        &mut doc,
        "cameraMount",
        Value::Object(one("cameras", Value::Object(cameras))),
    );
    KeyOutcome::Save {
        doc,
        stdout: vec![
            format!("✓ {key} = {deg} (persisted to settings.json)"),
            "  the agent updates the camera overlay in /boot/firmware/config.txt within ~a minute,"
                .to_string(),
            "  then a REBOOT is required before the image changes (the agent never reboots itself)."
                .to_string(),
        ],
        then: Then::Done,
    }
}

/// The usage of the `cameraSensors.*` keys (RUST-ONLY; the JS `USAGE` text
/// above is kept word for word, so these keys are named here).
pub const CAMERA_SENSORS_USAGE: &str = "cameraSensors keys are dotted paths: xorgate-rs set cameraSensors.<streamKey>.sensor <sensor id|none|unmanaged>, or xorgate-rs set cameraSensors.<streamKey>.options.<option> <number|default>, e.g. xorgate-rs set cameraSensors.cam0.sensor imx708";

/// The value that takes a port (or an option) out of the setting.
pub const CAMERA_SENSORS_UNMANAGED: &str = "unmanaged";

/// `xorgate-rs set cameraSensors.<streamKey>.sensor <id|none|unmanaged>` and
/// `xorgate-rs set cameraSensors.<streamKey>.options.<option> <number|default>`
/// (RUST-ONLY, support-other-camera-models Phase 2).
///
/// - `sensor <id>`: the port's entry becomes that sensor. Its options are
///   kept when the sensor is the one already set, dropped otherwise (they
///   are the other sensor's).
/// - `sensor none`: the port is declared empty.
/// - `sensor unmanaged`: the port's key is removed (the agent leaves its
///   camera line as found); with the last port gone the namespace itself is
///   removed, which a local fold-back sends as `null`.
/// - `options.<option> <number>`: one option of the sensor already set;
///   `default` removes it.
///
/// A sensor id and an option are checked against this build's catalog (the
/// cloud checks them again when the change is folded back). The stream key
/// is not checked against the device's ports here: the agent reports a
/// port the device does not have.
fn set_camera_sensors(
    key: &str,
    value: Option<&str>,
    loaded: &AgentSettings,
    catalog: &crate::camera_catalog::Catalog,
) -> KeyOutcome {
    let parts: Vec<&str> = key.split('.').collect();
    let stream_key = parts.get(1).copied().unwrap_or("");
    let shape_ok = match parts.as_slice() {
        [_, k, "sensor"] => !k.is_empty(),
        [_, k, "options", o] => !k.is_empty() && !o.is_empty(),
        _ => false,
    };
    if !shape_ok {
        return fail(CAMERA_SENSORS_USAGE);
    }
    if !stream_key_re_test(stream_key) || js_assign_drops_key(stream_key) {
        return fail(format!(
            "'{stream_key}' is not a valid stream key (see kvs.json channels)"
        ));
    }
    let value = or_empty(value);
    let mut doc = document(loaded);
    let mut cameras: Map<String, Value> =
        match doc.get("cameraSensors").and_then(|c| c.get("cameras")) {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
    let current = cameras.get(stream_key).cloned();
    let current_sensor = current
        .as_ref()
        .and_then(|e| e.get("sensor"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let known = || {
        catalog
            .sensors
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut stdout: Vec<String>;
    if parts.len() == 3 {
        if value == CAMERA_SENSORS_UNMANAGED {
            cameras.shift_remove(stream_key);
            stdout = vec![format!(
                "✓ {key} is no longer managed (persisted to settings.json)"
            )];
            stdout.push(
                "  the agent leaves that port's camera line in /boot/firmware/config.txt as it is."
                    .to_string(),
            );
            if cameras.is_empty() {
                doc.shift_remove("cameraSensors");
            } else {
                assign(
                    &mut doc,
                    "cameraSensors",
                    Value::Object(one("cameras", Value::Object(cameras))),
                );
            }
            return KeyOutcome::Save {
                doc,
                stdout,
                then: Then::Done,
            };
        }
        if value != crate::settings::CAMERA_SENSOR_NONE && catalog.by_id(value).is_none() {
            return fail(format!(
                "{key}: '{value}' is not a camera sensor this agent build knows (known: {}; or 'none' for a port with no camera, 'unmanaged' to leave the port alone)",
                known()
            ));
        }
        let mut entry = Map::new();
        entry.insert("sensor".to_string(), Value::String(value.to_string()));
        if current_sensor.as_deref() == Some(value) {
            if let Some(o) = current.as_ref().and_then(|e| e.get("options")) {
                entry.insert("options".to_string(), o.clone());
            }
        }
        cameras = spread(
            Some(&Value::Object(cameras)),
            one(stream_key, Value::Object(entry)),
        );
        stdout = vec![format!("✓ {key} = {value} (persisted to settings.json)")];
    } else {
        let option = parts[3];
        let Some(sensor) = current_sensor.as_deref().and_then(|id| catalog.by_id(id)) else {
            return fail(format!(
                "{key}: set the sensor of {stream_key} first (xorgate-rs set cameraSensors.{stream_key}.sensor <{}>)",
                known().replace(", ", "|")
            ));
        };
        let Some(opt) = sensor.options.iter().find(|o| o.key == option) else {
            let has = if sensor.options.is_empty() {
                "it has no options".to_string()
            } else {
                format!(
                    "its options: {}",
                    sensor
                        .options
                        .iter()
                        .map(|o| o.key.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return fail(format!(
                "{key}: the {} sensor has no option '{option}' ({has})",
                sensor.id
            ));
        };
        let mut entry = match current {
            Some(Value::Object(e)) => e,
            _ => Map::new(),
        };
        let mut options = match entry.get("options") {
            Some(Value::Object(o)) => o.clone(),
            _ => Map::new(),
        };
        if value == "default" {
            options.shift_remove(option);
            stdout = vec![format!(
                "✓ {key} = default ({} {}; persisted to settings.json)",
                opt.default, opt.param
            )];
        } else {
            let n = cli_number(Some(value));
            if !(n.fract() == 0.0 && opt.values.contains(&(n as i64))) {
                return fail(format!(
                    "{key} must be one of {} (or 'default'), got '{value}'",
                    opt.values
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            options.insert(option.to_string(), Value::from(n as i64));
            stdout = vec![format!(
                "✓ {key} = {} (persisted to settings.json)",
                n as i64
            )];
        }
        if options.is_empty() {
            entry.shift_remove("options");
        } else {
            entry.insert("options".to_string(), Value::Object(options));
        }
        cameras.insert(stream_key.to_string(), Value::Object(entry));
    }
    assign(
        &mut doc,
        "cameraSensors",
        Value::Object(one("cameras", Value::Object(cameras))),
    );
    stdout.push(
        "  the agent rewrites the camera line in /boot/firmware/config.txt within ~a minute,"
            .to_string(),
    );
    stdout.push(
        "  then a REBOOT is required before the camera changes (the agent never reboots itself)."
            .to_string(),
    );
    KeyOutcome::Save {
        doc,
        stdout,
        then: Then::Done,
    }
}

/// `setTimeSyncCommand` (`set.ts:316-365`).
fn set_time_sync(key: &str, value: Option<&str>, loaded: &AgentSettings) -> KeyOutcome {
    let parts: Vec<&str> = key.split('.').collect();
    let field = parts.get(1).copied().unwrap_or("");
    if parts.len() != 2 || field.is_empty() {
        return fail(
            "timeSync keys are dotted paths, e.g. xorgate set timeSync.offlineSources off",
        );
    }
    let shown = or_empty(value);
    let raw = match field {
        "offlineSources" => match parse_bool_value(value) {
            Some(b) => Value::Bool(b),
            None => return fail(format!("{key} must be on|off, got '{shown}'")),
        },
        "stepThresholdSecs" | "maxStepsPerBoot" => {
            let num = cli_number(value);
            if !num.is_finite() || num < 0.0 {
                return fail(format!("{key} must be a non-negative number, got '{shown}'"));
            }
            num_value(num)
        }
        _ => {
            return fail(format!(
                "unknown timeSync setting '{key}' (supported: offlineSources, stepThresholdSecs, maxStepsPerBoot)"
            ))
        }
    };
    let patch = parse_time_sync_settings(&Value::Object(one(field, raw)))
        .and_then(|p| serde_json::to_value(p).ok());
    let Some(Value::Object(patch)) = patch else {
        return fail(format!(
            "unknown or invalid timeSync setting '{key}' = '{shown}'"
        ));
    };
    let Some(applied) = patch.get(field).cloned() else {
        return fail(format!(
            "unknown or invalid timeSync setting '{key}' = '{shown}'"
        ));
    };
    let mut doc = document(loaded);
    let merged = spread(doc.get("timeSync"), patch);
    assign(&mut doc, "timeSync", Value::Object(merged));
    KeyOutcome::Save {
        doc,
        stdout: vec![
            format!(
                "✓ {key} = {} (persisted to settings.json)",
                to_json_string(&applied)
            ),
            "  the agent's clock stepper reads it live within ~30 s (no restart needed)"
                .to_string(),
        ],
        then: Then::Done,
    }
}

// ---------------------------------------------------------------------------
// obd (set.ts:380-460 @ 09da38d)
// ---------------------------------------------------------------------------

/// The CLI spelling of each `obd` namespace field (`OBD_CLI_KEYS`,
/// `set.ts:381-391` @ 09da38d).
const OBD_CLI_KEYS: &[(&str, &str)] = &[
    ("enabled", "enabled"),
    ("adapter", "adapterMac"),
    ("adapterMac", "adapterMac"),
    ("pin", "pin"),
    ("poll-hz", "pollHz"),
    ("pollHz", "pollHz"),
    ("fields", "fields"),
    ("power-source", "powerSource"),
    ("powerSource", "powerSource"),
];

/// Own and inherited properties of a plain JS object literal: a lookup
/// `OBD_CLI_KEYS[name]` on one of these is TRUTHY (a function / the
/// prototype), so the TS goes on to `parseObdSettings({[field]: raw})` with
/// a key the parser ignores and reports "invalid", never "unknown".
const JS_OBJECT_PROTO_KEYS: &[&str] = &[
    "__proto__",
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
];

/// `setObdCommand` (`set.ts:400-460` @ 09da38d): validate one field through
/// the SAME parser `loadSettings` uses ([`parse_obd_settings`]), merge it
/// into the persisted `obd` namespace (`{ ...settings.obd, ...patch }`).
fn set_obd(key: &str, value: Option<&str>, loaded: &AgentSettings) -> KeyOutcome {
    let parts: Vec<&str> = key.split('.').collect();
    let name = if parts.len() == 2 { parts[1] } else { "" };
    let field = OBD_CLI_KEYS
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, f)| *f)
        .or_else(|| (parts.len() == 2 && JS_OBJECT_PROTO_KEYS.contains(&name)).then_some(""));
    let Some(field) = field else {
        return fail(format!(
            "unknown obd setting '{key}' (supported: obd.enabled, obd.adapter, obd.pin, obd.poll-hz, obd.fields, obd.power-source)"
        ));
    };
    let shown = or_empty(value);
    let raw: Option<Value> = match field {
        "enabled" => match parse_bool_value(value) {
            Some(b) => Some(Value::Bool(b)),
            None => return fail(format!("{key} must be on|off, got '{shown}'")),
        },
        "adapterMac" => match value {
            None | Some("" | "none" | "null") => Some(Value::Null),
            Some(v) => Some(Value::String(v.to_string())),
        },
        "pollHz" => {
            let num = cli_number(value);
            if !num.is_finite() || !(OBD_POLL_HZ_MIN..=OBD_POLL_HZ_MAX).contains(&num) {
                return fail(format!(
                    "{key} must be a number in [{}, {}], got '{shown}'",
                    number_to_string(OBD_POLL_HZ_MIN),
                    number_to_string(OBD_POLL_HZ_MAX)
                ));
            }
            Some(num_value(num))
        }
        "fields" => match value {
            None | Some("" | "all") => Some(Value::Array(Vec::new())),
            Some(v) => Some(Value::Array(
                v.split(',')
                    .map(|f| Value::String(crate::js::trim(f).to_string()))
                    .collect(),
            )),
        },
        // pin, powerSource: validated by the parser below (`raw = value`,
        // `undefined` when no value was given).
        _ => value.map(|v| Value::String(v.to_string())),
    };
    // `parseObdSettings({ [field]: raw })`; a prototype-key lookup (field
    // "") lands on a key the parser ignores.
    let mut src = Map::new();
    if let Some(raw) = raw {
        if !field.is_empty() {
            src.insert(field.to_string(), raw);
        }
    }
    let patch = parse_obd_settings(&Value::Object(src)).and_then(|p| serde_json::to_value(p).ok());
    let applied = match &patch {
        Some(Value::Object(p)) if !field.is_empty() => p.get(field).cloned(),
        _ => None,
    };
    let (Some(Value::Object(patch)), Some(applied)) = (patch, applied) else {
        let hint = match field {
            "adapterMac" => " (expected AA:BB:CC:DD:EE:FF or none)",
            "pin" => " (expected 4 to 6 digits)",
            "powerSource" => " (expected switched|constant)",
            "fields" => " (expected a csv of field names, or all)",
            _ => "",
        };
        return fail(format!("invalid obd setting '{key}' = '{shown}'{hint}"));
    };
    let mut doc = document(loaded);
    let merged = spread(doc.get("obd"), patch);
    let enabled_on = merged.get("enabled") == Some(&Value::Bool(true));
    assign(&mut doc, "obd", Value::Object(merged));
    let mut stdout = vec![
        format!(
            "✓ {key} = {} (persisted to settings.json)",
            to_json_string(&applied)
        ),
        "  the agent's OBD collector reads it live within ~1 s (no restart needed; the helper restarts itself when the adapter changes)"
            .to_string(),
    ];
    // `field === "adapterMac" && patch.adapterMac && settings.obd.enabled !== true`
    if field == "adapterMac" && applied.is_string() && !enabled_on {
        stdout.push(
            "  note: obd.enabled is off — nothing is polled until: xorgate set obd.enabled on"
                .to_string(),
        );
    }
    KeyOutcome::Save {
        doc,
        stdout,
        then: Then::Done,
    }
}

// ---------------------------------------------------------------------------
// apn (set.ts:533-671)
// ---------------------------------------------------------------------------

/// `APN_WATCH_POLL_MS` (`set.ts:534`).
pub const APN_WATCH_POLL_MS: i64 = 3000;
/// `APN_WATCH_MAX_MS` (`set.ts:535`).
pub const APN_WATCH_MAX_MS: i64 = 8 * 60_000;
/// `APN_WATCH_OLD_DAEMON_MS` (`set.ts:537`).
pub const APN_WATCH_OLD_DAEMON_MS: i64 = 90_000;

/// `setApnCommand`'s input checks (`set.ts:568-579`): the value to write, or
/// the stderr lines (exit 1, nothing read or written).
pub fn apn_validate(value: Option<&str>) -> Result<&str, Vec<String>> {
    let Some(v) = value else {
        return Err(vec![
            "usage: xorgate set apn <value>   (or \"\" to leave the modem's stored APN alone)"
                .to_string(),
        ]);
    };
    if !apn_re_test(v) {
        return Err(vec![format!(
            "apn must be at most 100 letters, digits, dots or hyphens, got '{v}'"
        )]);
    }
    Ok(v)
}

/// `readApnBlock` (`set.ts:541-544`) over a `readModemTelemetry()` result:
/// `parseApnTelemetry(tel?.cellular)`.
pub fn apn_block(telemetry: Option<&Value>) -> Option<ApnBlock> {
    telemetry
        .and_then(|t| t.get("cellular"))
        .and_then(parse_apn_telemetry)
}

/// `next.cellular = { ...next.cellular, apn: value }` (`set.ts:583-584`).
pub fn apn_document(loaded: &AgentSettings, value: &str) -> Map<String, Value> {
    let mut doc = document(loaded);
    let merged = spread(doc.get("cellular"), one("apn", Value::String(value.into())));
    assign(&mut doc, "cellular", Value::Object(merged));
    doc
}

/// What follows the APN save (`set.ts:587-617`).
#[derive(Debug, Clone, PartialEq)]
pub struct ApnAfterSave {
    pub lines: Vec<Line>,
    pub exit_code: i32,
    /// Enter the watch loop ([`ApnWatch`]).
    pub watch: bool,
}

fn opt_num(n: Option<u32>, fallback: &str) -> String {
    n.map_or_else(|| fallback.to_string(), |n| n.to_string())
}

/// `set.ts:587-617`: the lines after a successful save, given the block read
/// BEFORE the save.
pub fn apn_after_save(value: &str, before: Option<&ApnBlock>, no_wait: bool) -> ApnAfterSave {
    let done = |lines: Vec<Line>, exit_code: i32| ApnAfterSave {
        lines,
        exit_code,
        watch: false,
    };
    if value.is_empty() {
        return done(
            vec![
                Line::out("✓ apn cleared (persisted to settings.json)"),
                Line::out(
                    "  the modem's stored APN is left untouched — nothing will be written to it",
                ),
            ],
            0,
        );
    }
    let mut lines = vec![Line::out(format!(
        "✓ apn = \"{value}\" (persisted to settings.json)"
    ))];
    if let Some(b) = before {
        if b.apn_applied.as_deref() == Some(value) && b.state == ApnState::InForce {
            lines.push(Line::out(
                "  already in force — the modem's data context holds this APN.",
            ));
            return done(lines, 0);
        }
        if b.state == ApnState::Rejected && b.apn_requested.as_deref() == Some(value) {
            lines.push(Line::err(format!(
                "✗ this exact value has already failed {} times and is rejected — the daemon will not retry it. Change the value to get another attempt.",
                opt_num(b.fails, "several")
            )));
            return done(lines, 1);
        }
    }
    if no_wait {
        lines.push(Line::out(
            "  the modem daemon applies it with confirm-and-rollback within a few minutes (journalctl -u sim7600-lte)",
        ));
        return done(lines, 0);
    }
    lines.push(Line::out(
        "  waiting for the modem daemon to apply and confirm (cellular will drop briefly; Ctrl-C is safe — the apply continues on its own)...",
    ));
    ApnAfterSave {
        lines,
        exit_code: 0,
        watch: true,
    }
}

/// One poll's verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchStep {
    /// Keep polling (after printing these).
    Continue(Vec<Line>),
    /// Stop with these lines and exit code.
    Finish(Vec<Line>, i32),
}

/// The APN outcome watch (`set.ts:618-670`) as a state machine: the caller
/// sleeps [`APN_WATCH_POLL_MS`], reads the block, and calls [`ApnWatch::poll`]
/// while `elapsed < APN_WATCH_MAX_MS`; then [`ApnWatch::timeout_line`].
#[derive(Debug, Clone, PartialEq)]
pub struct ApnWatch {
    value: String,
    /// `JSON.stringify(before ?? null)` — compared structurally (the block's
    /// serde order is the TS insertion order, so equal JSON ⇔ equal value).
    baseline: Option<ApnBlock>,
    saw_applying: bool,
}

impl ApnWatch {
    pub fn new(value: &str, before: Option<ApnBlock>) -> Self {
        ApnWatch {
            value: value.to_string(),
            baseline: before,
            saw_applying: false,
        }
    }

    /// One iteration after the sleep + read (`set.ts:623-665`); `elapsed_ms` is
    /// `Date.now() - started` at the time of the check.
    pub fn poll(&mut self, elapsed_ms: i64, cur: Option<ApnBlock>) -> WatchStep {
        let value = self.value.as_str();
        let Some(cur) = cur else {
            if elapsed_ms > APN_WATCH_OLD_DAEMON_MS {
                return WatchStep::Finish(
                    vec![Line::err(
                        "✗ the modem daemon is not reporting any APN machinery — it likely predates this feature and will NEVER apply the change. Update the agent (xorgate update).",
                    )],
                    1,
                );
            }
            return WatchStep::Continue(Vec::new());
        };
        if self.baseline.as_ref() == Some(&cur) {
            return WatchStep::Continue(Vec::new());
        }
        let requested = cur.apn_requested.as_deref() == Some(value);
        match cur.state {
            ApnState::Applying if requested => {
                let mut lines = Vec::new();
                if !self.saw_applying {
                    lines.push(Line::out(
                        "  applying — confirming with real traffic over the cellular link...",
                    ));
                    self.saw_applying = true;
                }
                WatchStep::Continue(lines)
            }
            ApnState::InForce if cur.apn_applied.as_deref() == Some(value) => WatchStep::Finish(
                vec![Line::out(format!(
                    "✓ confirmed: \"{value}\" is in force (data verified end-to-end)."
                ))],
                0,
            ),
            ApnState::RolledBack if requested => WatchStep::Finish(
                vec![Line::err(format!(
                    "✗ rolled back: \"{value}\" passed no traffic{}. The device restored \"{}\" and is recovering. Attempt {}/3; the daemon retries the value until it is rejected.",
                    paren(cur.last_error.as_deref()),
                    cur.apn_applied.as_deref().unwrap_or("the previous APN"),
                    opt_num(cur.fails, "?"),
                ))],
                1,
            ),
            ApnState::Rejected if requested => WatchStep::Finish(
                vec![Line::err(format!(
                    "✗ rejected: \"{value}\" failed {} times{}. Change the value to get another attempt.",
                    opt_num(cur.fails, "too many"),
                    paren(cur.last_error.as_deref()),
                ))],
                1,
            ),
            _ => WatchStep::Continue(Vec::new()),
        }
    }

    /// `set.ts:667-670`.
    pub fn timeout_line() -> Line {
        Line::out(
            "  no verdict after 8 minutes — the apply may be deferred (device unregistered?). Watch journalctl -u sim7600-lte, or re-run `xorgate set apn` later.",
        )
    }
}

/// `${x ? ` (${x})` : ""}` — an empty string is falsy.
fn paren(x: Option<&str>) -> String {
    match x {
        Some(s) if !s.is_empty() => format!(" ({s})"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> SetArgs {
        parse_set_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn positionals_skip_every_dash_token() {
        let a = args(&["--force", "timeSync.stepThresholdSecs", "-3", "--no-wait"]);
        assert_eq!(a.key.as_deref(), Some("timeSync.stepThresholdSecs"));
        assert_eq!(a.value, None);
        assert!(a.force && a.no_wait && !a.no_restart);
        assert_eq!(args(&["apn", ""]).value.as_deref(), Some(""));
    }

    #[test]
    fn routing_follows_set_command_order() {
        assert_eq!(route("wifi"), Route::Wifi);
        assert_eq!(route("recording.x"), Route::Recording);
        assert_eq!(route("recording"), Route::Unknown);
        assert_eq!(route("timeSync."), Route::TimeSync);
        assert_eq!(route("cameraMount.cam0"), Route::CameraMount);
        assert_eq!(route("apn"), Route::Apn);
    }

    /// `xorgate-rs set cameraSensors.*` (RUST-ONLY): the document it
    /// writes, its lines, its refusals.
    #[test]
    fn camera_sensors_keys() {
        fn run(loaded: &str, key: &str, value: &str) -> Result<(String, Vec<String>), Vec<String>> {
            let loaded: AgentSettings = serde_json::from_str(loaded).unwrap();
            match apply(&args(&[key, value]), &loaded) {
                KeyOutcome::Save { doc, stdout, then } => {
                    assert_eq!(then, Then::Done);
                    Ok((to_json_string(&Value::Object(doc)), stdout))
                }
                KeyOutcome::Fail(lines) => Err(lines),
            }
        }
        let doc = |loaded: &str, key: &str, value: &str| run(loaded, key, value).unwrap().0;
        let err =
            |loaded: &str, key: &str, value: &str| run(loaded, key, value).unwrap_err().join("\n");
        assert_eq!(route("cameraSensors.cam0.sensor"), Route::CameraSensors);

        // A sensor on a device that holds nothing: the namespace is created
        // after every other key; the lines say what happens next.
        let (d, out) = run(
            r#"{"gnssAntBias":true}"#,
            "cameraSensors.cam0.sensor",
            "imx708",
        )
        .unwrap();
        assert_eq!(
            d,
            r#"{"gnssAntBias":true,"cameraSensors":{"cameras":{"cam0":{"sensor":"imx708"}}}}"#
        );
        assert_eq!(
            out,
            [
                "✓ cameraSensors.cam0.sensor = imx708 (persisted to settings.json)",
                "  the agent rewrites the camera line in /boot/firmware/config.txt within ~a minute,",
                "  then a REBOOT is required before the camera changes (the agent never reboots itself).",
            ]
        );
        // A second port keeps the first; `none` declares a port empty.
        let two = doc(&d, "cameraSensors.cam1.sensor", "none");
        assert_eq!(
            two,
            r#"{"gnssAntBias":true,"cameraSensors":{"cameras":{"cam0":{"sensor":"imx708"},"cam1":{"sensor":"none"}}}}"#
        );
        // An option of the sensor already set; setting the same sensor
        // again keeps it; another sensor drops it (it is not that sensor's).
        let opt = doc(
            &two,
            "cameraSensors.cam0.options.linkFrequency",
            "447000000",
        );
        assert_eq!(
            opt,
            r#"{"gnssAntBias":true,"cameraSensors":{"cameras":{"cam0":{"sensor":"imx708","options":{"linkFrequency":447000000}},"cam1":{"sensor":"none"}}}}"#
        );
        assert_eq!(doc(&opt, "cameraSensors.cam0.sensor", "imx708"), opt);
        assert_eq!(
            doc(&opt, "cameraSensors.cam0.sensor", "imx290"),
            r#"{"gnssAntBias":true,"cameraSensors":{"cameras":{"cam0":{"sensor":"imx290"},"cam1":{"sensor":"none"}}}}"#
        );
        // `default` removes the option again.
        let (d2, out) = run(&opt, "cameraSensors.cam0.options.linkFrequency", "default").unwrap();
        assert_eq!(d2, two);
        assert_eq!(
            out[0],
            "✓ cameraSensors.cam0.options.linkFrequency = default (450000000 link-frequency; persisted to settings.json)"
        );
        // `unmanaged` removes the port; the last port removes the namespace
        // (a local fold-back then sends `cameraSensors: null`).
        let (one, out) = run(&two, "cameraSensors.cam1.sensor", "unmanaged").unwrap();
        assert_eq!(one, d);
        assert_eq!(
            out,
            [
                "✓ cameraSensors.cam1.sensor is no longer managed (persisted to settings.json)",
                "  the agent leaves that port's camera line in /boot/firmware/config.txt as it is.",
            ]
        );
        assert_eq!(
            doc(&one, "cameraSensors.cam0.sensor", "unmanaged"),
            r#"{"gnssAntBias":true}"#
        );
        // An entry this build cannot read (a newer backend's sensor) is
        // replaced by a sensor it knows; the other entries are kept as held.
        assert_eq!(
            doc(
                r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx999","future":1},"cam1":7}}}"#,
                "cameraSensors.cam0.sensor",
                "imx219"
            ),
            r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx219"},"cam1":7}}}"#
        );

        // Refusals: nothing is written.
        assert_eq!(
            err("{}", "cameraSensors.cam0.sensor", "imx999"),
            "cameraSensors.cam0.sensor: 'imx999' is not a camera sensor this agent build knows (known: imx290, imx219, imx708, imx335; or 'none' for a port with no camera, 'unmanaged' to leave the port alone)"
        );
        assert_eq!(
            err("{}", "cameraSensors.cam0.options.clockFrequency", "74250000"),
            "cameraSensors.cam0.options.clockFrequency: set the sensor of cam0 first (xorgate-rs set cameraSensors.cam0.sensor <imx290|imx219|imx708|imx335>)"
        );
        assert_eq!(
            err(&two, "cameraSensors.cam1.options.clockFrequency", "74250000"),
            "cameraSensors.cam1.options.clockFrequency: set the sensor of cam1 first (xorgate-rs set cameraSensors.cam1.sensor <imx290|imx219|imx708|imx335>)"
        );
        assert_eq!(
            err(&two, "cameraSensors.cam0.options.lanes", "4"),
            "cameraSensors.cam0.options.lanes: the imx708 sensor has no option 'lanes' (its options: linkFrequency)"
        );
        assert_eq!(
            err(&two, "cameraSensors.cam0.options.linkFrequency", "1"),
            "cameraSensors.cam0.options.linkFrequency must be one of 450000000, 447000000, 453000000 (or 'default'), got '1'"
        );
        assert_eq!(
            err(&two, "cameraSensors.cam0.options.linkFrequency", ""),
            "cameraSensors.cam0.options.linkFrequency must be one of 450000000, 447000000, 453000000 (or 'default'), got ''"
        );
        for key in [
            "cameraSensors.cam0",
            "cameraSensors..sensor",
            "cameraSensors.cam0.rotationDeg",
            "cameraSensors.cam0.options",
            "cameraSensors.cam0.options.",
            "cameraSensors.cam0.sensor.x",
        ] {
            assert_eq!(err("{}", key, "imx290"), CAMERA_SENSORS_USAGE, "{key}");
        }
        assert_eq!(
            err("{}", "cameraSensors.bad key!.sensor", "imx290"),
            "'bad key!' is not a valid stream key (see kvs.json channels)"
        );
        assert_eq!(
            err("{}", "cameraSensors.__proto__.sensor", "imx290"),
            "'__proto__' is not a valid stream key (see kvs.json channels)"
        );
        // The JS usage text is untouched (its goldens still hold).
        assert!(!USAGE.contains("cameraSensors"));
    }

    /// set-command.test.ts:81/:91/:100 — the pure half of the sudo note.
    #[test]
    fn sudo_note_only_when_root_and_agent_is_someone_else() {
        let n = sudo_note(true, Some(1000), Some("pi"));
        assert_eq!(n.len(), 2);
        assert!(n[0].text.contains("agent runs as 'pi'") && n[0].text.contains("don't need sudo"));
        assert_eq!(n[0].stream, Stream::Err);
        assert!(sudo_note(true, Some(0), Some("root")).is_empty());
        assert!(sudo_note(false, Some(1000), Some("pi")).is_empty());
        assert!(sudo_note(true, None, None).is_empty());
        assert_eq!(
            sudo_note(true, Some(1234), None)[1].text,
            "  writing anyway; settings.json keeps its 'uid 1234' ownership."
        );
    }

    #[test]
    fn passwd_lookup_guards_empty_uid() {
        let p = "root:x:0:0::/root:/bin/sh\nbad::::\npi:x:1000:1000::/home/pi:/bin/bash\n";
        assert_eq!(username_for_uid(p, 1000).as_deref(), Some("pi"));
        assert_eq!(username_for_uid(p, 0).as_deref(), Some("root"));
        assert_eq!(username_for_uid(p, 7), None);
    }

    #[test]
    fn spread_appends_new_keys_and_orders_index_keys_first() {
        let base: Value = serde_json::json!({"cam0": false, "cam1": true});
        let m = spread(Some(&base), one("7", Value::Bool(false)));
        assert_eq!(
            to_json_string(&Value::Object(m)),
            r#"{"7":false,"cam0":false,"cam1":true}"#
        );
        let m = spread(Some(&base), one("cam0", Value::Bool(true)));
        assert_eq!(
            to_json_string(&Value::Object(m)),
            r#"{"cam0":true,"cam1":true}"#
        );
    }
}
