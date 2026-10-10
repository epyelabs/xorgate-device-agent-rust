//! `xorgate-replay diff` — compare two rig captures under `tolerances.toml`
//! (phase file §Record/replay "Capture and diff").
//!
//! Capture layout (written by `play`; see `tools/replay/README.md`):
//! ```text
//! <run>/play.json                    run facts: start/end wall, shift, closed spans, transitions
//! <run>/pairs.ndjson                 {"a":tsA,"b":tsB} telemetry pairs the player saw per hold window
//! <run>/releases.ndjson              {"w","due","t","k"} every record's ACTUAL release (required)
//! <run>/<label>/meta.json            {"label","kind","deviceId",…}
//! <run>/<label>/mqtt.ndjson          {"t","w","topic","retain","qos","payload":"<raw>"} every publish of that agent
//! <run>/<label>/files.ndjson         {"w","name","text"|null} file snapshots on change (1 s poll)
//! <run>/<label>/final.json          every snapshotted file at the end: {"files":{key:{"present","b64",…}}}
//! <run>/<label>/final/<name>         the files at the end (legacy captures: only these)
//! <run>/<label>/sudo.log             "<epoch ms> <argv…>" per sudo shim call
//! ```
//!
//! Streams compared: telemetry (paired by `ts`), report (quiet periods), ack,
//! status, sms (in order), each publish's QoS + retain (pairwise and against
//! Appendix A), every end file (presence, JSON, byte layout), state.json at end, the state.json
//! `recentEvents` msg multiset, the sudo argv sequence, the pidfile shape.
//! Every mismatch is listed with both values and the tolerance id that covers
//! it, or `UNTOLERATED`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use xorgate_hal::tape::{TapeEvent, TapeRecord};

use crate::tolerances::Table;

pub const UNTOLERATED: &str = "UNTOLERATED";
/// Files compared at the end of a run.
pub const FILES: &[&str] = &[
    "settings.json",
    "config-sync.json",
    "commands.json",
    "gnss-sky.json",
    "state.json",
    "agent.pid",
];

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayInfo {
    #[serde(default)]
    pub tape: String,
    #[serde(default)]
    pub start_wall: i64,
    #[serde(default)]
    pub end_wall: i64,
    #[serde(default)]
    pub shift_s: i64,
    /// Wall-ms spans during which the broker path was cut.
    #[serde(default)]
    pub closed_spans: Vec<(i64, i64)>,
    /// Wall ms of every input transition (T4).
    #[serde(default)]
    pub transitions: Vec<i64>,
    /// `(wall ms, step ms)` of wall steps seen on the tape (O2).
    #[serde(default)]
    pub wall_steps: Vec<(i64, i64)>,
    #[serde(default)]
    pub calibration: bool,
    /// Wall ms where NMEA resumed after a silence ≥ REOPEN_SILENT_MS (both
    /// agents have reopened their port by then; T7/D23).
    #[serde(default)]
    pub nmea_resumes: Vec<i64>,
    /// Host reference samples `(wall ms, series → value)` from the player's
    /// `host.ndjson` (T3 host attribution); loaded by `load_run`.
    #[serde(skip)]
    pub host: Vec<(i64, BTreeMap<String, f64>)>,
    /// `(side "a"|"b", estimated tick wall ms)` of every lead tick the hold
    /// gate logged as `skip` at a drift flip (`pairs.ndjson`); loaded by `load_run`.
    #[serde(skip)]
    pub skips: Vec<(String, i64)>,
    /// Every record the player released, with its ACTUAL release wall ms
    /// (`releases.ndjson`, review-wp7 R1); loaded by `load_run`. The input
    /// facts are built from these, never from tape `t` + `start_wall`.
    #[serde(skip)]
    pub releases: Vec<Release>,
}

/// One `releases.ndjson` record: tape record `(t, k)` released at wall `w`
/// (`due` = when it fell due; the gates only delay).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Release {
    pub w: i64,
    #[serde(default)]
    pub due: i64,
    pub t: u64,
    pub k: String,
}

impl PlayInfo {
    /// Host CPU busy % over `[to − window, to]` from the player's `/proc/stat`
    /// samples (`cpu_busy`/`cpu_total` jiffies), nearest samples at both ends.
    pub fn host_cpu(&self, to: i64, window: i64) -> Option<f64> {
        let at = |w: i64| {
            self.host
                .iter()
                .filter(|(_, m)| m.contains_key("cpu_busy") && m.contains_key("cpu_total"))
                .min_by_key(|(t, _)| (t - w).abs())
                .filter(|(t, _)| (t - w).abs() <= 200)
                .map(|(_, m)| (m["cpu_busy"], m["cpu_total"]))
        };
        let ((b0, t0), (b1, t1)) = (at(to - window)?, at(to)?);
        (t1 > t0).then(|| (b1 - b0) / (t1 - t0) * 100.0)
    }

    /// Closed spans extended to the moment BOTH agents publish telemetry again
    /// (T8 `reconnect-cadence`: Rust backs off 1→60 s with jitter, mqtt.js
    /// retries every 2 s, so one side resumes later than the span's end).
    pub fn with_reconnects(&self, a: &[&Msg], b: &[&Msg]) -> PlayInfo {
        let mut out = self.clone();
        for span in out.closed_spans.iter_mut() {
            let end = span.1;
            let back = |ms: &[&Msg]| ms.iter().map(|m| m.w).filter(|w| *w > end).min();
            if let (Some(x), Some(y)) = (back(a), back(b)) {
                span.1 = end.max(x.max(y));
            }
        }
        out
    }

    /// The host series `name` at wall `w` (nearest sample within `within` ms).
    pub fn host_at(&self, name: &str, w: i64, within: i64) -> Option<f64> {
        self.host
            .iter()
            .filter(|(t, m)| (t - w).abs() <= within && m.contains_key(name))
            .min_by_key(|(t, _)| (t - w).abs())
            .map(|(_, m)| m[name])
    }
}

/// `REOPEN_SILENT_MS` (`gps.ts`): NMEA silence after which an agent reopens its port.
const REOPEN_SILENT_MS: u64 = 30_000;
/// play.rs `PREFILL_MS`: state-like records before this are applied before
/// the agents start (not transitions).
const PREFILL_MS: u64 = 1_000;
/// `NMEA_STALE_MS` (`gps.ts:15`): a gps group older than this is dropped.
const NMEA_STALE_MS: i64 = 10_000;
/// play.rs: an NMEA / IMU line stream with a gap above this stopped.
const LINE_GAP_MS: u64 = 2_000;
/// Lines released within this many tape ms of a chunk's first line are one
/// read chunk (the recorder stamps the lines of one tty read alike).
const CHUNK_MS: u64 = 2;

/// One NMEA resume after a reopen-length silence (T7/D23): the sentences of
/// the FIRST chunk after the resume — the chunk the TS loses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NmeaReopen {
    /// Wall ms the first resumed line was written (play.json `nmeaResumes`).
    pub w: i64,
    /// Sentence types (`GGA`, `RMC`, `GSA`, `GSV`, …) of that chunk's lines.
    pub sentences: Vec<String>,
    /// The chunk holds a GGA quality 0 or a void RMC (clears the position keys).
    pub clears_position: bool,
}

/// Input facts the diff derives from the TAPE itself: play.json's
/// `transitions` is one unclassified list, so the per-input rows classify the
/// tape's records, each at the wall ms the player ACTUALLY released it
/// (`releases.ndjson`; review-wp7 R1 — the watch, report-pair and post-cut
/// holds delay releases by up to ~10 s, so tape `t` + start is no estimate).
#[derive(Debug, Clone, Default)]
pub struct InputFacts {
    /// The tape the facts come from; `None` = not read (`tape_error` says why).
    pub tape: Option<String>,
    pub tape_error: Option<String>,
    pub reopens: Vec<NmeaReopen>,
    /// The tape's `meta.env.MODE` (`sim` → telemetry not comparable, O7).
    pub mode: Option<String>,
    /// T4: wall ms of the input transitions per telemetry GROUP (gps ← NMEA
    /// start/stop/stale/resume + GGA quality; imu ← helper spawn/exit, line
    /// gaps, i2cdetect; lte ← LTE-file changes; system ← exec/dt changes).
    /// Never mqtt_in / cli / conn.
    pub groups: BTreeMap<String, Vec<i64>>,
    /// Wall ms (`start_wall + t`) of every IMU line the tape released
    /// (T8 `rig-cut-ungated` IMU residual).
    pub imu_lines: Vec<i64>,
    /// Wall ms where a GSV talker's satellites-in-view changed (T8 `input-edge`).
    pub gsv_changes: Vec<i64>,
    /// `(wall ms, kind)` of the released non-line inputs: `config`, `command`,
    /// `cli`, `lte_file`, `exec`, `dt`, `i2cdetect`, `conn`, `imu_spawn`,
    /// `imu_exit` (T5 triggers, reconnect-cadence report values).
    pub inputs: Vec<(i64, &'static str)>,
    /// Every `rev` the tape's config publishes carry (reconnect-cadence
    /// report count after a cut).
    pub config_revs: BTreeSet<i64>,
    /// The tape's `meta.synthetic` tracks (e.g. `lte-apn`: the APN re-report
    /// path must be exercised).
    pub synthetic: Vec<String>,
    /// Per record kind: (records released, max ms the actual release lagged
    /// `start_wall + t`) — diff.md evidence for R1.
    pub release_lag: BTreeMap<&'static str, (usize, i64)>,
}

/// `$GPGGA,…` → `GGA`.
fn nmea_type(line: &str) -> Option<&str> {
    let head = line.trim().strip_prefix('$')?.split([',', '*']).next()?;
    head.get(2..).filter(|t| !t.is_empty())
}

/// A state-like record before `PREFILL_MS`: applied before the agents start
/// (play.rs prefill), never released.
fn prefilled(r: &TapeRecord) -> bool {
    r.t < PREFILL_MS
        && matches!(
            r.ev,
            TapeEvent::LteFile { .. }
                | TapeEvent::Exec { .. }
                | TapeEvent::I2cdetect { .. }
                | TapeEvent::Dt { .. }
        )
}

/// Per tape record, the wall ms the player ACTUALLY released it
/// (`releases.ndjson`; review-wp7 R1), `None` = prefilled or never released
/// (past the run's end / `--max-s`). The player writes one line per release
/// with the record's `(t, kind)`; records sharing both keep their order
/// (the pending queue keeps the order of every kind it holds).
pub fn release_walls(records: &[TapeRecord], releases: &[Release]) -> Vec<Option<i64>> {
    let mut q: BTreeMap<(u64, &str), std::collections::VecDeque<i64>> = BTreeMap::new();
    for r in releases {
        q.entry((r.t, r.k.as_str())).or_default().push_back(r.w);
    }
    records
        .iter()
        .map(|r| {
            if prefilled(r) {
                return None;
            }
            q.get_mut(&(r.t, r.ev.kind())).and_then(|d| d.pop_front())
        })
        .collect()
}

impl InputFacts {
    /// Read `play.tape` (load_run resolves it) and derive the facts.
    pub fn for_play(play: &PlayInfo) -> Self {
        if play.tape.is_empty() {
            return Self {
                tape_error: Some("play.json names no tape".into()),
                ..Self::default()
            };
        }
        if play.releases.is_empty() {
            return Self {
                tape_error: Some(
                    "the run has no releases.ndjson: the input instants are the player's actual \
                     releases, never tape t + start (review-wp7 R1)"
                        .into(),
                ),
                ..Self::default()
            };
        }
        match xorgate_hal::tape::Tape::read(Path::new(&play.tape)) {
            Ok(t) => {
                let mut f = Self::from_records(&t.records, play);
                f.tape = Some(play.tape.clone());
                f.synthetic = t.meta.synthetic.clone();
                f.mode = t
                    .meta
                    .env
                    .get("MODE")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                f
            }
            Err(e) => Self {
                tape_error: Some(format!("{}: {e}", play.tape)),
                ..Self::default()
            },
        }
    }

    /// The facts from the tape's records at the player's ACTUAL release
    /// instants (`play.releases`); a record without a release is not an input.
    pub fn from_records(records: &[TapeRecord], play: &PlayInfo) -> Self {
        let mut out = Self::default();
        let walls = release_walls(records, &play.releases);
        let rel: Vec<(&TapeRecord, i64)> = records
            .iter()
            .zip(&walls)
            .filter_map(|(r, w)| Some((r, (*w)?)))
            .collect();
        // (tape t, wall, line): the chunk is a TAPE notion (one tty read).
        let nmea: Vec<(u64, i64, &str)> = rel
            .iter()
            .filter_map(|(r, w)| match &r.ev {
                TapeEvent::Nmea { line } => Some((r.t, *w, line.as_str())),
                _ => None,
            })
            .collect();
        for (i, (t, w, _)) in nmea.iter().enumerate() {
            let Some(prev) = i.checked_sub(1).map(|j| nmea[j].0) else {
                continue;
            };
            if t - prev < REOPEN_SILENT_MS {
                continue;
            }
            let chunk: Vec<&str> = nmea[i..]
                .iter()
                .take_while(|(u, _, _)| u - t <= CHUNK_MS)
                .map(|(_, _, l)| *l)
                .collect();
            let mut r = NmeaReopen {
                w: *w,
                ..NmeaReopen::default()
            };
            for line in chunk {
                let Some(ty) = nmea_type(line) else { continue };
                let f: Vec<&str> = line
                    .trim()
                    .split('*')
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .collect();
                if (ty == "GGA" && f.get(6).is_some_and(|q| *q == "0"))
                    || (ty == "RMC" && f.get(2).is_some_and(|s| *s == "V"))
                {
                    r.clears_position = true;
                }
                if !r.sentences.iter().any(|s| s == ty) {
                    r.sentences.push(ty.to_string());
                }
            }
            out.reopens.push(r);
        }
        out.groups = group_transitions(&rel);
        let mut in_view: BTreeMap<String, String> = BTreeMap::new();
        for &(r, w) in &rel {
            let kind = match &r.ev {
                TapeEvent::Nmea { line } => {
                    if nmea_type(line) == Some("GSV") {
                        let talker = line.trim().get(1..3).unwrap_or("").to_string();
                        let n = line.split(',').nth(3).unwrap_or("").to_string();
                        if in_view.insert(talker, n.clone()).is_some_and(|o| o != n) {
                            out.gsv_changes.push(w);
                        }
                    }
                    continue;
                }
                TapeEvent::MqttIn { topic, .. } if topic.ends_with("/config") => {
                    if let Some(rev) =
                        r.ev.mqtt_payload()
                            .and_then(|p| serde_json::from_slice::<Value>(&p).ok())
                            .and_then(|v| v["rev"].as_i64())
                    {
                        out.config_revs.insert(rev);
                    }
                    "config"
                }
                TapeEvent::MqttIn { topic, .. } if topic.ends_with("/command") => "command",
                TapeEvent::Cli { .. } => "cli",
                TapeEvent::LteFile { .. } => "lte_file",
                TapeEvent::Exec { .. } => "exec",
                TapeEvent::Dt { .. } => "dt",
                TapeEvent::I2cdetect { .. } => "i2cdetect",
                TapeEvent::Conn { .. } => "conn",
                TapeEvent::ImuSpawn { .. } => "imu_spawn",
                TapeEvent::ImuExit { .. } => "imu_exit",
                _ => continue,
            };
            out.inputs.push((w, kind));
        }
        out.inputs.sort_by_key(|(w, _)| *w);
        out.imu_lines = rel
            .iter()
            .filter(|(r, _)| matches!(r.ev, TapeEvent::Imu { .. }))
            .map(|(_, w)| *w)
            .collect();
        out.imu_lines.sort();
        // Evidence: how far the actual releases lag the tape schedule.
        for (r, w) in &rel {
            let lag = w - (play.start_wall + r.t as i64);
            let e = out.release_lag.entry(r.ev.kind()).or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(lag);
        }
        out
    }

    /// Per group, the share (%) of the comparable run time (after the T4
    /// startup window) that lies within ±`window` of a transition.
    pub fn coverage(&self, play: &PlayInfo, startup: i64, window: i64) -> BTreeMap<String, f64> {
        let (lo, hi) = (play.start_wall + startup, play.end_wall);
        let mut out = BTreeMap::new();
        for g in ["gps", "imu", "lte", "system"] {
            let mut iv: Vec<(i64, i64)> = self
                .groups
                .get(g)
                .into_iter()
                .flatten()
                .map(|t| ((t - window).max(lo), (t + window).min(hi)))
                .filter(|(a, b)| a < b)
                .collect();
            iv.sort();
            let (mut covered, mut end) = (0i64, i64::MIN);
            for (a, b) in iv {
                let a = a.max(end);
                if b > a {
                    covered += b - a;
                    end = b;
                }
            }
            let span = (hi - lo).max(1);
            out.insert(g.to_string(), covered as f64 * 100.0 / span as f64);
        }
        out
    }
}

/// T4 per-group transitions from the released records (play.rs's release
/// loop, classified) at their ACTUAL release instants (review-wp7 R1).
fn group_transitions(rel: &[(&TapeRecord, i64)]) -> BTreeMap<String, Vec<i64>> {
    let mut g: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut push = |k: &str, w: i64| g.entry(k.to_string()).or_default().push(w);
    // `(tape t, wall)` of the last line of each stream.
    let mut last_nmea: Option<(u64, i64)> = None;
    let mut last_imu: Option<(u64, i64)> = None;
    let mut quality: Option<String> = None;
    for &(r, now) in rel {
        match &r.ev {
            TapeEvent::Nmea { line } => {
                match last_nmea {
                    Some((lt, lw)) if r.t - lt > LINE_GAP_MS => {
                        push("gps", lw);
                        if r.t - lt > NMEA_STALE_MS as u64 {
                            push("gps", lw + NMEA_STALE_MS);
                        }
                        push("gps", now);
                    }
                    None => push("gps", now),
                    _ => {}
                }
                if nmea_type(line) == Some("GGA") {
                    let q = line.split(',').nth(6).map(str::to_string);
                    if q.is_some() && q != quality {
                        push("gps", now);
                        quality = q;
                    }
                }
                last_nmea = Some((r.t, now));
            }
            TapeEvent::Imu { .. } => {
                match last_imu {
                    Some((lt, lw)) if r.t - lt > LINE_GAP_MS => {
                        push("imu", lw);
                        push("imu", now);
                    }
                    None => push("imu", now),
                    _ => {}
                }
                last_imu = Some((r.t, now));
            }
            TapeEvent::ImuSpawn { .. } | TapeEvent::ImuExit { .. } => push("imu", now),
            TapeEvent::I2cdetect { .. } => push("imu", now),
            TapeEvent::LteFile { .. } => push("lte", now),
            TapeEvent::Exec { .. } | TapeEvent::Dt { .. } => push("system", now),
            _ => {}
        }
    }
    if let Some((_, lw)) = last_nmea {
        push("gps", lw + NMEA_STALE_MS);
    }
    if let Some((_, lw)) = last_imu {
        push("imu", lw);
    }
    for v in g.values_mut() {
        v.sort();
        v.dedup();
    }
    g
}

/// The metrics.gps fields (`gps.ts` mapLatest) the sentences of a lost
/// chunk would have set (T7/D23).
fn d23_fields(r: &NmeaReopen) -> BTreeSet<&'static str> {
    let mut out = BTreeSet::new();
    for s in &r.sentences {
        let f: &[&'static str] = match s.as_str() {
            "GGA" => &["fix_quality", "satellites", "alt", "lat", "lon", "ttff_s"],
            "RMC" => &["lat", "lon", "speed", "course"],
            "GSA" => &["hdop"],
            "GSV" => &["satellites_in_view", "sats_heard", "snr_max", "sky_state"],
            _ => &[],
        };
        out.extend(f);
    }
    if r.clears_position {
        out.extend(["lat", "lon", "alt", "speed", "course", "hdop"]);
    }
    out
}

/// `metrics.gps.<field>[.…]` → `<field>`.
fn gps_field(path: &str) -> Option<&str> {
    path.strip_prefix("metrics.gps.")?.split('.').next()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMeta {
    pub label: String,
    pub kind: String,
    pub device_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawMsg {
    t: u64,
    w: i64,
    topic: String,
    #[serde(default)]
    retain: bool,
    #[serde(default)]
    qos: Option<u8>,
    payload: String,
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub w: i64,
    /// Topic with the agent's deviceId replaced by `{id}`.
    pub topic: String,
    /// The publisher's retain bit (the capture subscribes with
    /// Retain-As-Published and no retained replay).
    pub retain: bool,
    /// The publisher's QoS (the capture subscribes at QoS 2); `None` in old
    /// captures.
    pub qos: Option<u8>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct Capture {
    pub meta: AgentMeta,
    pub msgs: Vec<Msg>,
    pub finals: BTreeMap<String, Option<String>>,
    /// Every distinct state.json event `(at, level, msg)` seen in snapshots.
    pub events: Vec<(i64, String, String)>,
    pub sudo: Vec<String>,
}

/// Replace the agent's deviceId by `{id}` (T2).
fn norm_id(s: &str, id: &str) -> String {
    if id.is_empty() {
        s.to_string()
    } else {
        s.replace(id, "{id}")
    }
}

fn parse_payload(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// The end-of-run files: `final.json` (every key the player snapshots, raw
/// bytes as base64, `{"present":false}` when absent) when the capture has
/// one, else the legacy `final/<name>` copies of [`FILES`].
fn load_finals(dir: &Path, id: &str) -> BTreeMap<String, Option<String>> {
    let text = |b: &[u8]| norm_id(&String::from_utf8_lossy(b), id);
    let mut finals = BTreeMap::new();
    let snap: Option<Value> = std::fs::read(dir.join("final.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if let Some(files) = snap.as_ref().and_then(|v| v["files"].as_object()) {
        for (k, f) in files {
            let bytes = (f["present"] == true)
                .then(|| {
                    f["b64"]
                        .as_str()
                        .and_then(xorgate_hal::tape::b64_decode)
                        .or_else(|| f["text"].as_str().map(|t| t.as_bytes().to_vec()))
                })
                .flatten();
            finals.insert(k.clone(), bytes.map(|b| text(&b)));
        }
        return finals;
    }
    for name in FILES {
        let p = dir.join("final").join(name);
        finals.insert(name.to_string(), std::fs::read(&p).ok().map(|b| text(&b)));
    }
    finals
}

/// The whitespace/indent skeleton of a JSON text: every string (keys too) is
/// `s`, every other scalar `v`; structure and whitespace kept verbatim.
fn json_skeleton(text: &str) -> String {
    let delim = |c: char| matches!(c, '{' | '}' | '[' | ']' | ',' | ':' | '"') || c.is_whitespace();
    let mut out = String::with_capacity(text.len() / 4);
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if c == '"' {
            while let Some(d) = it.next() {
                if d == '\\' {
                    it.next();
                } else if d == '"' {
                    break;
                }
            }
            out.push('s');
        } else if delim(c) {
            out.push(c);
        } else {
            while it.peek().is_some_and(|d| !delim(*d)) {
                it.next();
            }
            out.push('v');
        }
    }
    out
}

/// The writer's layout style of a JSON text, for documents whose shapes
/// differ (so their skeletons cannot be equal): multi-line or not, the
/// indent unit, `": "` vs `":"`, `", "` inline, trailing newline.
fn json_style(text: &str) -> String {
    let sk = json_skeleton(text);
    let indent = sk
        .lines()
        .skip(1)
        .map(|l| l.len() - l.trim_start().len())
        .find(|n| *n > 0)
        .unwrap_or(0);
    format!(
        "multiline={} indent={indent} colon_space={} comma_space={} trailing_nl={}",
        sk.contains('\n'),
        sk.contains(": "),
        sk.contains(", "),
        text.ends_with('\n')
    )
}

impl Capture {
    pub fn load(dir: &Path) -> Result<Self> {
        let meta: AgentMeta = serde_json::from_slice(
            &std::fs::read(dir.join("meta.json"))
                .with_context(|| format!("{}/meta.json", dir.display()))?,
        )?;
        let id = meta.device_id.clone();
        let mut msgs = Vec::new();
        if let Ok(text) = std::fs::read_to_string(dir.join("mqtt.ndjson")) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let m: RawMsg = serde_json::from_str(line)
                    .with_context(|| format!("{}/mqtt.ndjson: {line}", dir.display()))?;
                let _ = m.t;
                msgs.push(Msg {
                    w: m.w,
                    topic: norm_id(&m.topic, &id),
                    retain: m.retain,
                    qos: m.qos,
                    payload: parse_payload(&norm_id(&m.payload, &id)),
                });
            }
        }
        let finals = load_finals(dir, &id);
        let mut seen = BTreeSet::new();
        let mut events = Vec::new();
        let snap_texts: Vec<String> = std::fs::read_to_string(dir.join("files.ndjson"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["name"] == "state.json")
            .filter_map(|v| v["text"].as_str().map(str::to_string))
            .chain(finals.get("state.json").cloned().flatten())
            .collect();
        for text in snap_texts {
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            for e in v["recentEvents"].as_array().into_iter().flatten() {
                // ActivityEvent `{ts, level, msg}` (telemetry.ts); `at` kept for
                // older captures. Keyed by it, so a repeated text is counted
                // once per occurrence, not once per run.
                let at = e["ts"].as_i64().or_else(|| e["at"].as_i64()).unwrap_or(0);
                let level = e["level"].as_str().unwrap_or("").to_string();
                let msg = norm_id(e["msg"].as_str().unwrap_or(""), &id);
                if seen.insert((at, level.clone(), msg.clone())) {
                    events.push((at, level, msg));
                }
            }
        }
        events.sort();
        let sudo = std::fs::read_to_string(dir.join("sudo.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once(' ').map(|(_, argv)| argv.to_string()))
            .collect();
        Ok(Self {
            meta,
            msgs,
            finals,
            events,
            sudo,
        })
    }

    fn stream(&self, suffix: &str) -> Vec<&Msg> {
        self.msgs
            .iter()
            .filter(|m| m.topic.ends_with(&format!("{{id}}/{suffix}")))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Mismatch {
    pub stream: String,
    pub at: String,
    pub path: String,
    pub a: String,
    pub b: String,
    pub tol: String,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct StreamStats {
    pub compared: usize,
    /// Files only: absent on BOTH sides (nothing to compare).
    pub absent_both: usize,
    /// Leaves compared (the larger document of each compared pair).
    pub leaves: usize,
    pub unpaired_a: usize,
    pub unpaired_b: usize,
    pub tolerated: usize,
    pub untolerated: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub stats: BTreeMap<String, StreamStats>,
    pub mismatches: Vec<Mismatch>,
    /// What the diff derived from the tape (source, reopens, coverage …),
    /// printed in diff.md so an empty input set is visible.
    pub inputs: BTreeMap<String, String>,
    /// Hits per tolerance row, every row of the table (0 included).
    pub hits: Vec<(String, usize)>,
}

impl Report {
    pub fn untolerated(&self) -> usize {
        self.mismatches
            .iter()
            .filter(|m| m.tol == UNTOLERATED)
            .count()
    }

    fn push(&mut self, m: Mismatch) {
        let st = self.stats.entry(m.stream.clone()).or_default();
        if m.tol == UNTOLERATED {
            st.untolerated += 1;
        } else {
            st.tolerated += 1;
        }
        self.mismatches.push(m);
    }
}

fn show(v: Option<&Value>) -> String {
    match v {
        None => "∅ (absent)".to_string(),
        Some(v) => {
            let s = v.to_string();
            if s.len() > 300 {
                format!(
                    "{}…",
                    &s[..s.char_indices().nth(300).map(|(i, _)| i).unwrap_or(s.len())]
                )
            } else {
                s
            }
        }
    }
}

/// Comparison context for one pair of documents.
struct Cx<'a> {
    table: &'a Table,
    play: &'a PlayInfo,
    stream: String,
    at: String,
    /// Wall ms the documents describe (telemetry `ts`), for T4 / O2 windows.
    when: Option<i64>,
    /// The B document's `ts` when it differs from A's (paired telemetry): the
    /// T3 host window is `[when, when_b]`.
    when_b: Option<i64>,
    /// T2 bases: path → (base A, base B).
    deltas: &'a BTreeMap<String, (f64, f64)>,
    first_wall: i64,
    /// Paired telemetry only: the neighbouring pairs' payloads
    /// `[prev A, next A, prev B, next B]` (T8 `input-edge`).
    neighbors: [Option<&'a Value>; 4],
    /// Paired telemetry only: this pair holds the JS side's first tick after
    /// an NMEA reopen — the gps fields the lost chunk would have set (T7/D23).
    d23: Option<&'a BTreeSet<&'static str>>,
    facts: &'a InputFacts,
    /// `Some(true)`: A is the js agent and B the rust one; `Some(false)`: the
    /// reverse; `None`: same kinds (the one-directional rows never apply).
    js_a: Option<bool>,
}

impl Cx<'_> {
    fn near_transition(&self, path: &str) -> Option<String> {
        // T7/D23: at the JS side's first tick after an NMEA reopen the gps
        // fields are judged by D23 alone (review-wp7 finding 1).
        if self.d23.is_some() && gps_field(path).is_some() {
            return None;
        }
        let w = self.when?;
        let r = self.table.of_kind("presence_window", &self.stream).next()?;
        let window = r.window_ms.unwrap_or(1500) as i64;
        let startup = r.startup_ms.unwrap_or(5000) as i64;
        if w - self.first_wall <= startup {
            return Some(r.id.clone());
        }
        // Only the transitions of the group the path belongs to
        // (`metrics.<group>[.…]`); any other path has only the startup window.
        let group = path.strip_prefix("metrics.")?.split('.').next()?;
        self.facts
            .groups
            .get(group)?
            .iter()
            .any(|t| (w - t).abs() <= window)
            .then(|| format!("{}/{group}", r.id))
    }

    /// T8 `rig-cut-ungated`, narrowed: inside a closed span (after the
    /// re-pair) a VALUE may differ only when the tape released an input of
    /// that value's group BETWEEN the two agents' reads (the gate cannot hold
    /// releases while an agent's ticks are unobserved): imu ← an IMU line
    /// (−1 s: the helper's sample is read at the next tick), lte ← an LTE-file
    /// change, system ← an exec/dt change. Never gps (NMEA flows every second;
    /// the re-pair aligns it) and never a presence difference.
    ///
    /// lte/system are further RESTRICTED to the fields that input event
    /// changed: one agent read the old version and the other the new one, so
    /// one side's value must be the other side's value at the ADJACENT tick
    /// (A_k = B_k±1 or B_k = A_k±1 at this path). A field the change did not
    /// move (equal in both versions) can never satisfy that and stays
    /// UNTOLERATED.
    fn cut_input(&self, path: &str, x: &Value, y: &Value) -> Option<String> {
        let group = path.strip_prefix("metrics.")?.split('.').next()?;
        if !matches!(group, "imu" | "lte" | "system") {
            return None;
        }
        let id = self.near_closed_as("rig-cut-ungated")?;
        let (a, b) = (self.when?, self.when_b?);
        let (lo, hi) = (a.min(b), a.max(b));
        let between = |w: i64| w > lo && w <= hi;
        let released = match group {
            "imu" => self
                .facts
                .imu_lines
                .iter()
                .any(|w| *w >= lo - 1_000 && *w <= hi),
            "lte" => self
                .facts
                .inputs
                .iter()
                .any(|(w, k)| *k == "lte_file" && between(*w)),
            _ => self
                .facts
                .inputs
                .iter()
                .any(|(w, k)| matches!(*k, "exec" | "dt") && between(*w)),
        };
        if !released {
            return None;
        }
        if group != "imu" {
            let at = |v: Option<&Value>| v.and_then(|v| lookup(v, path)).cloned();
            let [pa, na, pb, nb] = self.neighbors;
            let moved = [(pb, x), (nb, x), (pa, y), (na, y)]
                .iter()
                .any(|(n, v)| at(*n).as_ref() == Some(*v));
            if !moved {
                return None;
            }
        }
        Some(id)
    }

    /// `when` inside a closed span (extended to both agents' reconnect) padded
    /// by the row's `around_closed_ms`, for the T8 row `row_id`.
    fn near_closed_as(&self, row_id: &str) -> Option<String> {
        let w = self.when?;
        let (rid, row) = self.table.decision_row(row_id)?;
        if !crate::tolerances::stream_matches(&row.streams, &self.stream) {
            return None;
        }
        let pad = row.around_closed_ms.unwrap_or(5000) as i64;
        self.play
            .closed_spans
            .iter()
            .any(|(a, b)| w >= a - pad && w <= b + pad)
            .then(|| format!("{rid}/{row_id}"))
    }

    /// T3 host attribution: the excess `over` the agent tolerance is covered
    /// only if the host's own reference reading (sampled by the player every
    /// 50 ms) swung by at least as much between the two agents' `ts` (±250 ms
    /// for the read-to-stamp latency).
    fn host_attributed(&self, path: &str, p: f64, q: f64, tol: f64) -> Option<String> {
        let (id, series) = self.table.host_reference(&self.stream, path)?;
        let (a, b) = (self.when?, self.when_b?);
        // NARROWED (review-wp7): the HOST's own difference between the two
        // agents' instants must explain the agents' difference with the same
        // sign, within the agents' tolerance: |(A−B) − (host(tsA) − host(tsB))| ≤ tol.
        let hd = if series == "cpu" {
            // Each agent's cpu_usage is the busy share since its previous tick
            // (1 s at RATE_HZ=1) ending at its read (≈ `ts`).
            self.play.host_cpu(a, 1000)? - self.play.host_cpu(b, 1000)?
        } else {
            self.play.host_at(&series, a, 250)? - self.play.host_at(&series, b, 250)?
        };
        let d = p - q;
        (d.signum() == hd.signum() && hd != 0.0 && (d - hd).abs() <= tol + 1e-9)
            .then(|| format!("{id}/host"))
    }

    /// T8 `input-edge`, NARROWED (review-wp7): JS-LAG only — within the
    /// row's window of an NMEA transition (gps group) or a GSV
    /// satellites-in-view change, a metrics.gps value where the JS side reads
    /// Rust's PREVIOUS sample and the JS NEXT sample is Rust's value (Node
    /// handles a pty chunk late). A Rust value one tick late is not covered.
    fn input_edge(&self, path: &str, x: &Value, y: &Value) -> Option<String> {
        let (rid, row) = self.table.decision_row("input-edge")?;
        if !crate::tolerances::stream_matches(&row.streams, &self.stream)
            || !path.starts_with("metrics.gps.")
        {
            return None;
        }
        let w = self.when?;
        let win = row.around_closed_ms.unwrap_or(1500) as i64;
        let near = self
            .facts
            .groups
            .get("gps")
            .into_iter()
            .flatten()
            .chain(&self.facts.gsv_changes)
            .any(|t| (w - t).abs() <= win);
        if !near {
            return None;
        }
        let at = |v: Option<&Value>| v.and_then(|v| lookup(v, path)).cloned();
        let [pa, na, pb, nb] = self.neighbors;
        let lag = match self.js_a? {
            true => at(pb).as_ref() == Some(x) && at(na).as_ref() == Some(y),
            false => at(pa).as_ref() == Some(y) && at(nb).as_ref() == Some(x),
        };
        lag.then(|| format!("{rid}/input-edge"))
    }

    /// T7/D23: a gps field the TS's lost first chunk after a reopen would
    /// have set, at the JS side's first tick after the resume.
    fn d23(&self, path: &str) -> Option<String> {
        let field = gps_field(path)?;
        if !self.d23?.contains(field) {
            return None;
        }
        self.table
            .matcher_rows("nmea_reopen_first_chunk", &self.stream)
            .into_iter()
            .next()
    }

    /// T8 reconnect-cadence for REPORT values: after a cut each agent's
    /// reconnect triggers a `cloud` report at its own reconnect time (the Rust
    /// backoff resumes later), so an input that changed BETWEEN the two
    /// reports' publish times is read differently. NARROWED (review-wp7):
    /// only a report pair inside a cut's window, and only the paths that
    /// input feeds (an LTE-file change → effective.sim / uplink / apn,
    /// status.cellular).
    fn report_input_between(&self, path: &str) -> Option<String> {
        if self.stream != "report" {
            return None;
        }
        let (a, b) = (self.when?, self.when_b?);
        let (lo, hi) = (a.min(b), a.max(b));
        let (rid, row) = self.table.decision_row("reconnect-cadence")?;
        let pad = row.around_closed_ms.unwrap_or(5000) as i64;
        let near_cut = self
            .play
            .closed_spans
            .iter()
            .any(|(cs, ce)| lo <= ce + pad && hi >= cs - pad);
        let fed = |kind: &str| match kind {
            "lte_file" => [
                "effective.sim",
                "effective.uplink",
                "effective.apn",
                "status.cellular",
            ]
            .iter()
            .any(|p| path == *p || path.starts_with(&format!("{p}."))),
            _ => false,
        };
        let input_between = self
            .facts
            .inputs
            .iter()
            .any(|(t, k)| *t > lo && *t <= hi && fed(k));
        (near_cut && input_between).then(|| format!("{rid}/reconnect-cadence"))
    }

    /// T8/O2, NARROWED (review-wp7): only the row's `observables` (the six
    /// mono-derived ones) within ±(step + 15 s) of a wall step.
    fn near_wall_step(&self, path: &str) -> Option<String> {
        let w = self.when?;
        let (rid, row) = self.table.decision_row("O2")?;
        let observed = row.observables.iter().any(|o| {
            o.rsplit_once(':').is_some_and(|(st, p)| {
                st == self.stream && crate::tolerances::path_matches(p, path)
            })
        });
        if !observed {
            return None;
        }
        self.play
            .wall_steps
            .iter()
            .any(|(t, step)| (w - t).abs() <= step.abs() + 15_000)
            .then(|| format!("{rid}/O2"))
    }

    fn mismatch(&self, path: &str, a: Option<&Value>, b: Option<&Value>, tol: String) -> Mismatch {
        Mismatch {
            stream: self.stream.clone(),
            at: self.at.clone(),
            path: path.to_string(),
            a: show(a),
            b: show(b),
            tol,
        }
    }
}

fn num_text(v: &Value) -> Option<String> {
    v.as_number().map(|n| n.to_string())
}

fn join(path: &str, seg: &str) -> String {
    if path.is_empty() {
        seg.to_string()
    } else {
        format!("{path}.{seg}")
    }
}

/// Appendix A publish flags `(QoS, retain)` by topic suffix (TS worker.ts:
/// telemetry/status QoS 0, report/ack/sms QoS 1; nothing retained).
fn contract_flags(topic: &str) -> Option<(&'static str, u8, bool)> {
    [
        ("/telemetry", "telemetry", 0, false),
        ("/status", "status", 0, false),
        ("/config/reported", "report", 1, false),
        ("/command/response", "ack", 1, false),
        ("/sms", "sms", 1, false),
    ]
    .into_iter()
    .find(|(suf, ..)| topic.ends_with(suf))
    .map(|(_, s, q, r)| (s, q, r))
}

/// Every publish of one capture against the Appendix A flags.
fn flags_contract(c: &Capture, side: &str, rep: &mut Report) {
    for m in &c.msgs {
        let Some((stream, q, r)) = contract_flags(&m.topic) else {
            continue;
        };
        rep.stats.entry("mqtt-flags".into()).or_default().compared += 1;
        let mut bad = |path: &str, got: String, want: String| {
            rep.push(Mismatch {
                stream: stream.into(),
                at: format!("{side} w {}", m.w),
                path: path.into(),
                a: if side == "A" {
                    got.clone()
                } else {
                    want.clone()
                },
                b: if side == "B" { got } else { want },
                tol: UNTOLERATED.into(),
            })
        };
        if let Some(got) = m.qos.filter(|g| *g != q) {
            bad("(qos contract)", got.to_string(), q.to_string());
        }
        if m.retain != r {
            bad("(retain contract)", m.retain.to_string(), r.to_string());
        }
    }
}

/// One paired publish: both agents' QoS and retain must be equal.
fn flags_pair(cx: &Cx, x: &Msg, y: &Msg, rep: &mut Report) {
    rep.stats.entry("mqtt-flags".into()).or_default().compared += 1;
    if x.qos != y.qos {
        let q = |m: &Msg| m.qos.map_or(Value::Null, Value::from);
        rep.push(cx.mismatch("(qos)", Some(&q(x)), Some(&q(y)), UNTOLERATED.into()));
    }
    if x.retain != y.retain {
        rep.push(cx.mismatch(
            "(retain)",
            Some(&Value::Bool(x.retain)),
            Some(&Value::Bool(y.retain)),
            UNTOLERATED.into(),
        ));
    }
}

fn leaf_count(v: &Value) -> usize {
    match v {
        Value::Object(m) => m.values().map(leaf_count).sum(),
        Value::Array(a) => a.iter().map(leaf_count).sum(),
        _ => 1,
    }
}

/// Recursive compare. Objects compare key ORDER too (wire bytes, serde exactness).
fn cmp(cx: &Cx, path: &str, a: Option<&Value>, b: Option<&Value>, out: &mut Report) {
    if path.is_empty() {
        // Per-kind comparison counts: the leaves of the larger document.
        let n = a.map_or(0, leaf_count).max(b.map_or(0, leaf_count));
        out.stats.entry(cx.stream.clone()).or_default().leaves += n;
    }
    if a == b && num_strict_eq(a, b) {
        return;
    }
    if let Some(id) = cx.table.excluded(&cx.stream, path) {
        let values_only = id
            .split_once('/')
            .and_then(|(_, row)| cx.table.decision_row(row))
            .is_some_and(|(_, r)| r.values_only);
        if !values_only || (a.is_some() && b.is_some()) {
            out.push(cx.mismatch(path, a, b, id));
            return;
        }
    }
    match (a, b) {
        (Some(Value::Object(x)), Some(Value::Object(y))) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                cmp(cx, &join(path, k), x.get(k), y.get(k), out);
            }
            let ox: Vec<&String> = x.keys().filter(|k| y.contains_key(*k)).collect();
            let oy: Vec<&String> = y.keys().filter(|k| x.contains_key(*k)).collect();
            if ox != oy {
                let rows = cx
                    .table
                    .matcher_rows("same_json_different_key_order", &cx.stream);
                let tol = rows.first().cloned().unwrap_or_else(|| UNTOLERATED.into());
                out.push(Mismatch {
                    stream: cx.stream.clone(),
                    at: cx.at.clone(),
                    path: format!("{path} (key order)"),
                    a: format!("{ox:?}"),
                    b: format!("{oy:?}"),
                    tol,
                });
            }
        }
        (Some(Value::Array(x)), Some(Value::Array(y))) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                cmp(cx, &join(path, &i.to_string()), Some(p), Some(q), out);
            }
        }
        (Some(x), Some(y)) => {
            // T2: `seq` is compared as per-agent increments (seq_increments);
            // the offset between two agents' counters is not a property.
            if cx.table.is_delta(&cx.stream, path).is_some() && x.is_number() && y.is_number() {
                let _ = cx.deltas;
                return;
            }
            // T3 numeric.
            if let (Some((id, tol)), Some(p), Some(q)) =
                (cx.table.numeric(&cx.stream, path), x.as_f64(), y.as_f64())
            {
                let t = if (p - q).abs() <= tol + 1e-9 {
                    id
                } else if let Some(h) = cx.host_attributed(path, p, q, tol) {
                    h
                } else {
                    UNTOLERATED.into()
                };
                out.push(cx.mismatch(path, a, b, t));
                return;
            }
            // T8 reconnect-cadence: `published` / `publishErrors` may differ by
            // one per closed span (QoS 0 racing a drop) + one (the SIGTERM race).
            if let (Some(p), Some(q), Some((rid, row))) = (
                x.as_f64(),
                y.as_f64(),
                cx.table.decision_row("reconnect-cadence"),
            ) {
                if crate::tolerances::stream_matches(&row.streams, &cx.stream)
                    && row.count_paths.iter().any(|c| c == path)
                {
                    let slack = 1.0 + cx.play.closed_spans.len() as f64;
                    let t = if (p - q).abs() <= slack {
                        format!("{rid}/reconnect-cadence")
                    } else {
                        UNTOLERATED.into()
                    };
                    out.push(cx.mismatch(path, a, b, t));
                    return;
                }
            }
            // T8 mqtt-error-text: state.json `lastError` from the client library.
            if let (Some(p), Some(q), Some((rid, row))) = (
                x.as_str(),
                y.as_str(),
                cx.table.decision_row("mqtt-error-text"),
            ) {
                let foreign = |m: &str| {
                    !row.exact_texts.iter().any(|e| e == m)
                        && !row.own_prefixes.iter().any(|o| m.starts_with(o.as_str()))
                };
                if cx.stream == "state" && path == "lastError" && foreign(p) && foreign(q) {
                    out.push(cx.mismatch(path, a, b, format!("{rid}/mqtt-error-text")));
                    return;
                }
            }
            // T7 FFFD vs lone surrogate (D1/D2).
            if let (Some(p), Some(q)) = (x.as_str(), y.as_str()) {
                let rows = cx.table.matcher_rows("fffd_vs_lone_surrogate", &cx.stream);
                if !rows.is_empty() && fffd_equiv(p, q) {
                    out.push(cx.mismatch(path, a, b, rows[0].clone()));
                    return;
                }
            }
            let tol = cx
                .d23(path)
                .or_else(|| cx.cut_input(path, x, y))
                .or_else(|| cx.input_edge(path, x, y))
                .or_else(|| cx.report_input_between(path))
                .or_else(|| cx.near_wall_step(path))
                .unwrap_or_else(|| UNTOLERATED.into());
            out.push(cx.mismatch(path, a, b, tol));
        }
        _ => {
            if let Some(id) = cx.d23(path) {
                out.push(cx.mismatch(path, a, b, id));
                return;
            }
            // T7/D22: the JS group carries a `null` leaf (a non-finite number),
            // the Rust side omitted the whole group.
            if let (Some(x), None) = (a, b) {
                let rows = cx.table.matcher_rows("null_leaf_group", &cx.stream);
                if !rows.is_empty() && path.matches('.').count() == 1 && has_null_leaf(x) {
                    out.push(cx.mismatch(path, a, b, rows[0].clone()));
                    return;
                }
            }
            // T6, NARROWED (review-wp7): a hint-only latch detail present on
            // the JS side and absent on the Rust side, with exactly the TS text.
            let js_only = match cx.js_a {
                Some(true) => a.and_then(Value::as_str).filter(|_| b.is_none()),
                Some(false) => b.and_then(Value::as_str).filter(|_| a.is_none()),
                None => None,
            };
            if let Some(text) = js_only {
                if let Some(r) = cx.table.of_kind("phase_owned", &cx.stream).find(|r| {
                    r.latch_detail_paths
                        .iter()
                        .any(|p| crate::tolerances::path_matches(p, path))
                        && latch_text(r, text)
                }) {
                    out.push(cx.mismatch(path, a, b, r.id.clone()));
                    return;
                }
            }
            // Presence (or array length / type) mismatch.
            let tol = cx
                .near_transition(path)
                .or_else(|| cx.near_wall_step(path))
                .unwrap_or_else(|| UNTOLERATED.into());
            out.push(cx.mismatch(path, a, b, tol));
        }
    }
}

/// A latchDetail text (config-sync.ts:1007-1024) only a Phase 3/4 hint
/// produces: an exact one, or the prefix + `; `-joined known parts.
fn latch_text(r: &crate::tolerances::Rule, text: &str) -> bool {
    if r.latch_detail_exact.iter().any(|e| e == text) {
        return true;
    }
    let Some(rest) = r
        .latch_detail_prefix
        .as_deref()
        .and_then(|p| text.strip_prefix(p))
    else {
        return false;
    };
    !rest.is_empty()
        && rest.split("; ").all(|part| {
            r.latch_detail_parts
                .iter()
                .any(|known| match known.strip_suffix("*)") {
                    Some(head) => {
                        part.starts_with(head) && part.ends_with(')') && part.len() > head.len() + 1
                    }
                    None => part == known,
                })
        })
}

/// Scalars → `true` (not `null`: that would read as a D22 non-finite leaf),
/// structure (keys, order, array lengths) kept.
fn mask_scalars(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), mask_scalars(x)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(mask_scalars).collect()),
        _ => Value::Bool(true),
    }
}

/// T8 `end-snapshot`, NARROWED (review-wp7): state.json `latest` must EQUAL
/// each agent's own last published telemetry payload; across the agents
/// only its key structure is compared (under the telemetry rules, unless the
/// telemetry is not comparable at all — sim mode), and a VALUE difference is
/// this row. Removes `latest` from both documents afterwards.
fn end_snapshot(
    vx: &mut Value,
    vy: &mut Value,
    a: &Capture,
    b: &Capture,
    cx: &Cx,
    structure: bool,
    rep: &mut Report,
) {
    let take = |v: &mut Value| v.as_object_mut().and_then(|o| o.remove("latest"));
    let (la, lb) = (take(vx), take(vy));
    for (side, l, c) in [("A", &la, a), ("B", &lb, b)] {
        let last = c
            .msgs
            .iter()
            .filter(|m| m.topic.ends_with("/telemetry"))
            .max_by_key(|m| m.w)
            .map(|m| &m.payload);
        if let (Some(l), Some(last)) = (l, last) {
            if l != last {
                rep.push(Mismatch {
                    stream: "state".into(),
                    at: format!("end {side}"),
                    path: "latest (≠ its own last telemetry)".into(),
                    a: if side == "A" {
                        show(Some(l))
                    } else {
                        show(Some(last))
                    },
                    b: if side == "B" {
                        show(Some(l))
                    } else {
                        show(Some(last))
                    },
                    tol: UNTOLERATED.into(),
                });
            }
        }
    }
    match (&la, &lb) {
        (Some(x), Some(y)) => {
            if structure {
                let tcx = Cx {
                    stream: "telemetry".into(),
                    at: "state.json latest (structure)".into(),
                    when: ts_of(x),
                    when_b: ts_of(y),
                    ..*cx
                };
                cmp(
                    &tcx,
                    "",
                    Some(&mask_scalars(x)),
                    Some(&mask_scalars(y)),
                    rep,
                );
            }
            if x != y {
                let id = cx
                    .table
                    .decision_row("end-snapshot")
                    .map(|(r, _)| format!("{r}/end-snapshot"))
                    .unwrap_or_else(|| UNTOLERATED.into());
                rep.push(cx.mismatch("latest (values)", Some(x), Some(y), id));
            }
        }
        (None, None) => {}
        _ => rep.push(cx.mismatch("latest", la.as_ref(), lb.as_ref(), UNTOLERATED.into())),
    }
}

/// T8 `sky-file-end`: gnss-sky.json at the end is each agent's LAST 15 s
/// write, at its own instant (`timestamp`); `hold_s` is the state's age at
/// that write. A `hold_s` difference within the write instants' distance
/// (+1 s for whole-second truncation) is the same state seen at different
/// times: cited, then equalised so the remaining keys are still compared.
fn sky_hold_at_write(a: &mut Value, b: &mut Value, cx: &Cx, rep: &mut Report) {
    let Some((rid, _)) = cx.table.decision_row("sky-file-end") else {
        return;
    };
    // `timestamp` is an ISO string (gps.ts:247 `new Date().toISOString()`).
    let num = |v: &Value, k: &str| {
        v.get(k).and_then(|x| {
            x.as_f64().or_else(|| {
                x.as_str()
                    .and_then(xorgate_core::js::date_parse_ms)
                    .map(|ms| ms as f64)
            })
        })
    };
    let (Some(ha), Some(hb), Some(ta), Some(tb)) = (
        num(a, "hold_s"),
        num(b, "hold_s"),
        num(a, "timestamp"),
        num(b, "timestamp"),
    ) else {
        return;
    };
    if ha == hb || a.get("state") != b.get("state") {
        return;
    }
    if (ha - hb).abs() <= ((ta - tb).abs() / 1000.0).ceil() + 1.0 {
        rep.push(cx.mismatch(
            "hold_s",
            a.get("hold_s"),
            b.get("hold_s"),
            format!("{rid}/sky-file-end"),
        ));
        if let (Some(o), Some(h)) = (b.as_object_mut(), a.get("hold_s").cloned()) {
            o.insert("hold_s".into(), h);
        }
    }
}

fn has_null_leaf(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Object(m) => m.values().any(has_null_leaf),
        Value::Array(a) => a.iter().any(has_null_leaf),
        _ => false,
    }
}

/// `serde_json` equality treats 1 and 1.0 as different already (u64 vs f64),
/// but compare the printed form too so T9 is explicit.
fn num_strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) if x.is_number() && y.is_number() => num_text(x) == num_text(y),
        _ => true,
    }
}

/// One side has U+FFFD where the other kept a lone surrogate (as JSON text the
/// TS writes `\ud83d`, which serde_json turns into U+FFFD on read — so compare
/// via the raw escapes when present, else equal-length with FFFD positions).
fn fffd_equiv(a: &str, b: &str) -> bool {
    let strip = |s: &str| s.replace('\u{FFFD}', "").replace("\\ud83d", "");
    a != b && (a.contains('\u{FFFD}') || b.contains('\u{FFFD}')) && strip(a) == strip(b)
}

fn ts_of(v: &Value) -> Option<i64> {
    v.get("ts").and_then(Value::as_i64)
}

/// Pair telemetry by the player's hold-window pairs, else nearest `ts` within
/// ±500 ms (monotone).
fn pair_telemetry<'a>(
    a: &[&'a Msg],
    b: &[&'a Msg],
    pairs: &[(i64, i64)],
) -> (Vec<(&'a Msg, &'a Msg)>, Vec<&'a Msg>, Vec<&'a Msg>) {
    let mut out = Vec::new();
    let mut used_a = vec![false; a.len()];
    let mut used_b = vec![false; b.len()];
    let idx_a: BTreeMap<i64, usize> = a
        .iter()
        .enumerate()
        .filter_map(|(i, m)| Some((ts_of(&m.payload)?, i)))
        .collect();
    let idx_b: BTreeMap<i64, usize> = b
        .iter()
        .enumerate()
        .filter_map(|(i, m)| Some((ts_of(&m.payload)?, i)))
        .collect();
    for (ta, tb) in pairs {
        if let (Some(&i), Some(&j)) = (idx_a.get(ta), idx_b.get(tb)) {
            if !used_a[i] && !used_b[j] {
                used_a[i] = true;
                used_b[j] = true;
                out.push((a[i], b[j]));
            }
        }
    }
    // Fallback: greedy nearest for whatever the player did not pair, aligned
    // like the nearest player pair (its B − A offset, if one lies within 30 s):
    // raw nearest-ts would re-align across a phase offset near ±500 ms and
    // orphan a tick at the seam.
    let offsets: Vec<(i64, i64)> = out
        .iter()
        .filter_map(|(x, y)| Some((ts_of(&x.payload)?, ts_of(&y.payload)? - ts_of(&x.payload)?)))
        .collect();
    let mut j0 = 0;
    for (i, ma) in a.iter().enumerate() {
        if used_a[i] {
            continue;
        }
        let Some(ta_raw) = ts_of(&ma.payload) else {
            continue;
        };
        let off = offsets
            .iter()
            .min_by_key(|(t, _)| (t - ta_raw).abs())
            .filter(|(t, _)| (t - ta_raw).abs() <= 30_000)
            .map(|(_, o)| *o)
            .unwrap_or(0);
        let ta = ta_raw + off;
        let mut best: Option<(i64, usize)> = None;
        for (j, mb) in b.iter().enumerate().skip(j0) {
            if used_b[j] {
                continue;
            }
            let Some(tb) = ts_of(&mb.payload) else {
                continue;
            };
            let d = (ta - tb).abs();
            if tb > ta + 500 {
                break;
            }
            if d <= 500 && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, j));
            }
        }
        if let Some((_, j)) = best {
            used_a[i] = true;
            used_b[j] = true;
            out.push((ma, b[j]));
            j0 = j0.max(j.saturating_sub(2));
        }
    }
    out.sort_by_key(|(x, _)| ts_of(&x.payload));
    let ua = a
        .iter()
        .enumerate()
        .filter(|(i, _)| !used_a[*i])
        .map(|(_, m)| *m)
        .collect();
    let ub = b
        .iter()
        .enumerate()
        .filter(|(j, _)| !used_b[*j])
        .map(|(_, m)| *m)
        .collect();
    (out, ua, ub)
}

/// Leaf differences between two documents that no exclusion covers (the
/// re-pair score).
fn leaf_diffs(table: &Table, path: &str, a: Option<&Value>, b: Option<&Value>) -> usize {
    if a == b || table.excluded("telemetry", path).is_some() {
        return 0;
    }
    match (a, b) {
        (Some(Value::Object(x)), Some(Value::Object(y))) => x
            .keys()
            .chain(y.keys().filter(|k| !x.contains_key(*k)))
            .map(|k| leaf_diffs(table, &join(path, k), x.get(k), y.get(k)))
            .sum(),
        _ if table.is_delta("telemetry", path).is_some() => 0,
        _ => 1,
    }
}

type Pairs<'a> = Vec<(&'a Msg, &'a Msg)>;
/// `((span start, end), ticks moved, pairs in the run, ticks orphaned)` per run.
type Shifts = Vec<((i64, i64), usize, usize, usize)>;
/// `(side "A"|"B", ts)` of a tick the re-pair left without a partner.
type Orphans = Vec<(&'static str, i64)>;
/// At most this many ticks a re-pair run may orphan (one seam per side).
const REPAIR_MAX_ORPHANS: usize = 2;
/// An orphaned tick costs this many leaf differences in the re-pair score.
const REPAIR_ORPHAN_COST: u64 = 3;

/// T8 `rig-cut-ungated` (review-wp7 finding 4): inside each closed span
/// (± the row's pad) the hold gate could not pair ticks, so the diff's
/// fallback pairing (pre-cut offset) may sit one tick off. Every contiguous
/// run of pairs there — fallback pairs, and player pairs from the cut's start
/// on — is re-paired tick by tick with the current partner or the B tick one
/// before / after it (never one another pair holds, B order kept), minimising
/// the leaf differences (ties keep the current one). Ticks left without a
/// partner become unpaired (judged as unpaired messages).
fn repair_cuts<'a>(
    paired: Pairs<'a>,
    mut ua: Vec<&'a Msg>,
    mut ub: Vec<&'a Msg>,
    pairs: &[(i64, i64)],
    play: &PlayInfo,
    table: &Table,
) -> (Pairs<'a>, Vec<&'a Msg>, Vec<&'a Msg>, Shifts, Orphans) {
    let mut shifts = Vec::new();
    let mut orphans = Vec::new();
    let Some((_, row)) = table.decision_row("rig-cut-ungated") else {
        return (paired, ua, ub, shifts, orphans);
    };
    let pad = row.around_closed_ms.unwrap_or(5000) as i64;
    let player: BTreeSet<(i64, i64)> = pairs.iter().copied().collect();
    let is_player = |p: &(&Msg, &Msg)| matches!((ts_of(&p.0.payload), ts_of(&p.1.payload)), (Some(x), Some(y)) if player.contains(&(x, y)));
    // Every B message in ts order (paired or not).
    let mut bs: Vec<&'a Msg> = paired
        .iter()
        .map(|p| p.1)
        .chain(ub.iter().copied())
        .collect();
    bs.sort_by_key(|m| ts_of(&m.payload));
    let pos = |m: &Msg| bs.iter().position(|x| std::ptr::eq(*x, m));
    let mut out = paired;
    out.sort_by_key(|(x, _)| ts_of(&x.payload));
    for &(s, e) in &play.closed_spans {
        let inside =
            |p: &(&Msg, &Msg)| ts_of(&p.0.payload).is_some_and(|w| w >= s - pad && w <= e + pad);
        // Contiguous runs (A ts order) of fallback pairs inside the window.
        let mut runs: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        for (i, p) in out.iter().enumerate() {
            // Player pairs before the cut are gated and stay; from the cut on
            // (through the reconnect + pad) the gate's pairs are as unreliable
            // as the fallback's (publishes were queued, then flushed).
            let movable = !is_player(p) || ts_of(&p.0.payload).is_some_and(|w| w >= s);
            if inside(p) && movable {
                cur.push(i);
            } else if !cur.is_empty() {
                runs.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            runs.push(cur);
        }
        let mut dropped: BTreeSet<usize> = BTreeSet::new();
        for run in runs {
            let taken: BTreeSet<usize> = out
                .iter()
                .enumerate()
                .filter(|(i, _)| !run.contains(i) && !dropped.contains(i))
                .filter_map(|(_, p)| pos(p.1))
                .collect();
            // Per tick: the current partner j0, or j0 ± 1 (never a B another
            // pair holds), B order kept strictly increasing; minimise the leaf
            // differences (a move costs 1/1000
            // of a difference so ties keep the current partner).
            let opts = |i: usize| -> Vec<Option<usize>> {
                let mut v: Vec<Option<usize>> = vec![None];
                if let Some(j0) = pos(out[i].1) {
                    for j in [j0.checked_sub(1), Some(j0), Some(j0 + 1)]
                        .into_iter()
                        .flatten()
                    {
                        if j < bs.len() && !taken.contains(&j) {
                            v.push(Some(j));
                        }
                    }
                }
                v
            };
            let cost = |i: usize, o: Option<usize>| -> u64 {
                match o {
                    None => REPAIR_ORPHAN_COST * 1_000,
                    Some(j) => {
                        let d =
                            leaf_diffs(table, "", Some(&out[i].0.payload), Some(&bs[j].payload));
                        let moved = pos(out[i].1) != Some(j);
                        d as u64 * 1_000 + u64::from(moved)
                    }
                }
            };
            // State: (last chosen B index, ticks orphaned so far) → (cost,
            // choices). A tick is orphaned when it loses its partner (A) or is
            // skipped over (B); at most REPAIR_MAX_ORPHANS per run, each worth
            // REPAIR_ORPHAN_COST leaf differences — a re-pair may close a seam,
            // never drop content wholesale.
            type Dp = BTreeMap<(Option<usize>, usize), (u64, Vec<Option<usize>>)>;
            let mut states: Dp = BTreeMap::from([((None, 0), (0, Vec::new()))]);
            for &i in &run {
                let mut next: Dp = BTreeMap::new();
                for ((last, orphans), (c, ch)) in &states {
                    for o in opts(i) {
                        let skipped = match (o, last) {
                            (Some(j), Some(l)) if j <= *l => continue,
                            (Some(j), Some(l)) => (l + 1..j).filter(|k| !taken.contains(k)).count(),
                            (None, _) => 1,
                            _ => 0,
                        };
                        let orph = orphans + skipped;
                        if orph > REPAIR_MAX_ORPHANS {
                            continue;
                        }
                        let nc = c + cost(i, o) + skipped as u64 * REPAIR_ORPHAN_COST * 1_000;
                        let key = (o.or(*last), orph);
                        if next.get(&key).is_none_or(|(bc, _)| nc < *bc) {
                            let mut v = ch.clone();
                            v.push(o);
                            next.insert(key, (nc, v));
                        }
                    }
                }
                states = next;
            }
            let Some((_, choice)) = states.into_values().min_by_key(|(c, _)| *c) else {
                continue;
            };
            let best: Vec<(usize, Option<&'a Msg>)> = run
                .iter()
                .zip(&choice)
                .map(|(&i, o)| (i, o.map(|j| bs[j])))
                .collect();
            let moved = best
                .iter()
                .filter(|(i, b)| !b.is_some_and(|b| std::ptr::eq(b, out[*i].1)))
                .count();
            if moved == 0 {
                continue;
            }
            let old_b: Vec<&'a Msg> = run.iter().map(|&i| out[i].1).collect();
            let new_b: Vec<&'a Msg> = best.iter().filter_map(|(_, b)| *b).collect();
            let mut orphaned = 0;
            for b in old_b {
                if !new_b.iter().any(|y| std::ptr::eq(*y, b)) {
                    ub.push(b);
                    orphans.push(("B", ts_of(&b.payload).unwrap_or(b.w)));
                    orphaned += 1;
                }
            }
            ub.retain(|m| !new_b.iter().any(|y| std::ptr::eq(*y, *m)));
            for (i, b) in &best {
                match b {
                    Some(b) => out[*i].1 = b,
                    None => {
                        let m = out[*i].0;
                        ua.push(m);
                        orphans.push(("A", ts_of(&m.payload).unwrap_or(m.w)));
                        orphaned += 1;
                    }
                }
            }
            dropped.extend(best.iter().filter(|(_, b)| b.is_none()).map(|(i, _)| *i));
            shifts.push(((s, e), moved, run.len(), orphaned));
        }
        let mut i = 0;
        out.retain(|_| {
            i += 1;
            !dropped.contains(&(i - 1))
        });
    }
    out.sort_by_key(|(x, _)| ts_of(&x.payload));
    ua.sort_by_key(|m| ts_of(&m.payload));
    ub.sort_by_key(|m| ts_of(&m.payload));
    (out, ua, ub, shifts, orphans)
}

fn first_values(
    msgs: &[(&Msg, &Msg)],
    table: &Table,
    stream: &str,
) -> BTreeMap<String, (f64, f64)> {
    let mut out = BTreeMap::new();
    if let Some((x, y)) = msgs.first() {
        for r in table.of_kind("deltas", stream) {
            for p in &r.paths {
                if let (Some(p1), Some(p2)) = (lookup(&x.payload, p), lookup(&y.payload, p)) {
                    if let (Some(u), Some(v)) = (p1.as_f64(), p2.as_f64()) {
                        out.insert(p.clone(), (u, v));
                    }
                }
            }
        }
    }
    out
}

fn lookup<'v>(v: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(v, |cur, seg| cur.get(seg))
}

/// Which side is the js agent when the other is rust (one-directional rows).
fn js_a_of(a: &Capture, b: &Capture) -> Option<bool> {
    match (a.meta.kind.as_str(), b.meta.kind.as_str()) {
        ("js", "rust") => Some(true),
        ("rust", "js") => Some(false),
        _ => None,
    }
}

/// The TS config-report debounce (config-sync.ts): two triggers closer than
/// this are one report on one agent and two on the other (T5).
const DEBOUNCE_MS: i64 = 2_000;

/// Run the whole comparison; the input facts come from `play.tape`.
pub fn diff(
    a: &Capture,
    b: &Capture,
    play: &PlayInfo,
    pairs: &[(i64, i64)],
    table: &Table,
) -> Report {
    let facts = InputFacts::for_play(play);
    let mut rep = diff_with(a, b, play, &facts, pairs, table);
    if play.releases.is_empty() {
        // R1: no actual release instants → no tape-derived row may apply, and
        // the run itself is not judgeable.
        rep.push(Mismatch {
            stream: "run".into(),
            at: "run".into(),
            path: "(releases.ndjson missing)".into(),
            a: "∅".into(),
            b: "∅".into(),
            tol: UNTOLERATED.into(),
        });
    }
    require_apn_rereport(&mut rep, a, b, &facts);
    require_core_kinds(&mut rep, &facts, table);
    rep
}

/// `info: modem APN state changed — re-reporting` (TS config-sync.ts:1502,
/// Rust `APN_CHANGED_EVENT`).
const APN_EVENT: &str = "modem APN state changed — re-reporting";

/// Re-review R2: a tape with the synth `lte-apn` track changes the LTE file's
/// `cellular` block in a stretch with no other report trigger, so the 30 s APN
/// watch is the only path to a report. Each side must then log the APN event
/// AND publish a `boot` report carrying the changed APN (else UNTOLERATED:
/// "0 = 0" is no evidence); the counts themselves are compared by the events
/// multiset and the report stream as usual.
pub fn require_apn_rereport(rep: &mut Report, a: &Capture, b: &Capture, facts: &InputFacts) {
    if !facts.synthetic.iter().any(|t| t == "lte-apn") {
        return;
    }
    let events = |c: &Capture| c.events.iter().filter(|(_, _, m)| m == APN_EVENT).count();
    let reports = |c: &Capture| {
        c.msgs
            .iter()
            .filter(|m| m.topic.ends_with("/config/reported"))
            .filter(|m| {
                m.payload["origin"] == "boot"
                    && m.payload["effective"]["apn"]["apn_applied"] == crate::synth::LTE_APN_ALT
            })
            .count()
    };
    let (ea, eb, ra, rb) = (events(a), events(b), reports(a), reports(b));
    rep.inputs.insert(
        "APN re-report (lte-apn track)".into(),
        format!(
            "events A {ea} / B {eb}; boot reports with apn_applied {} A {ra} / B {rb}",
            crate::synth::LTE_APN_ALT
        ),
    );
    for (what, x, y) in [("events", ea, eb), ("report", ra, rb)] {
        if x == 0 || y == 0 {
            rep.push(Mismatch {
                stream: what.into(),
                at: "run".into(),
                path: "(APN re-report not exercised: lte-apn track)".into(),
                a: x.to_string(),
                b: y.to_string(),
                tol: UNTOLERATED.into(),
            });
        }
    }
}

/// A whole run must compare every CORE kind at least once: "0 untolerated"
/// must never come from an empty comparison (review-wp7). (Unit tests with
/// partial fixtures call [`diff_with`], which does not require it.)
pub fn require_core_kinds(rep: &mut Report, facts: &InputFacts, table: &Table) {
    let sim =
        facts.mode.as_deref() == Some("sim") && table.decision_row("sim-mode-telemetry").is_some();
    for k in CORE_KINDS {
        if *k == "telemetry" && sim {
            continue;
        }
        if rep.stats.get(*k).is_none_or(|st| st.compared == 0) {
            rep.push(Mismatch {
                stream: k.to_string(),
                at: "run".into(),
                path: "(no comparisons)".into(),
                a: "0".into(),
                b: "0".into(),
                tol: UNTOLERATED.into(),
            });
        }
    }
    rep.hits = row_hits(rep, table);
}

/// [`diff`] with the input facts given (tests build them by hand).
pub fn diff_with(
    a: &Capture,
    b: &Capture,
    play: &PlayInfo,
    facts: &InputFacts,
    pairs: &[(i64, i64)],
    table: &Table,
) -> Report {
    let mut rep = Report::default();
    let js_a = js_a_of(a, b);
    rep.inputs.insert(
        "tape".into(),
        match (&facts.tape, &facts.tape_error) {
            (Some(t), _) => t.clone(),
            (None, Some(e)) => format!("NOT READ ({e}): no tape-derived tolerance applies"),
            (None, None) => "NOT READ: no tape-derived tolerance applies".into(),
        },
    );
    if !facts.release_lag.is_empty() {
        rep.inputs.insert(
            "input instants".into(),
            format!(
                "releases.ndjson (actual release per record; max lag behind start + tape t: {})",
                facts
                    .release_lag
                    .iter()
                    .map(|(k, (n, lag))| format!("{k} {n}× ≤{lag} ms"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    rep.inputs.insert(
        "nmea reopens (T7/D23)".into(),
        facts.reopens.len().to_string(),
    );
    if let Some(r) = table.of_kind("presence_window", "telemetry").next() {
        let (startup, window) = (
            r.startup_ms.unwrap_or(5000) as i64,
            r.window_ms.unwrap_or(1500) as i64,
        );
        for (g, pct) in facts.coverage(play, startup, window) {
            let n = facts.groups.get(&g).map_or(0, Vec::len);
            rep.inputs.insert(
                format!("{} window coverage {g}", r.id),
                format!("{pct:.1} % of the run ({n} transitions, ±{window} ms)"),
            );
        }
    }
    let empty = BTreeMap::new();
    let first_wall = play.start_wall;

    // ---- telemetry
    flags_contract(a, "A", &mut rep);
    flags_contract(b, "B", &mut rep);
    // T8/sim-mode-telemetry (O7): with MODE=sim on the tape each agent's
    // telemetry is its own simulator (TS generator vs Rust empty SimSource),
    // so the telemetry stream is not compared at all; everything else is.
    let sim =
        facts.mode.as_deref() == Some("sim") && table.decision_row("sim-mode-telemetry").is_some();
    if sim {
        rep.inputs.insert(
            "telemetry".into(),
            "NOT COMPARED: tape MODE=sim (T8/sim-mode-telemetry, O7)".into(),
        );
    }
    let ta: Vec<&Msg> = a
        .msgs
        .iter()
        .filter(|m| !sim && m.topic.ends_with("/telemetry"))
        .collect();
    let tb: Vec<&Msg> = b
        .msgs
        .iter()
        .filter(|m| !sim && m.topic.ends_with("/telemetry"))
        .collect();
    let extended = play.with_reconnects(&ta, &tb);
    let play = &extended;
    let (paired, ua, ub) = pair_telemetry(&ta, &tb, pairs);
    let (paired, ua, ub, shifts, orphans) = repair_cuts(paired, ua, ub, pairs, play, table);
    for (k, (span, moved, n, orphaned)) in shifts.iter().enumerate() {
        rep.inputs.insert(
            format!("rig-cut re-pair {k}"),
            format!(
                "span [{}..{}]: {moved} of {n} pairs moved by ±1 tick, {orphaned} tick(s) orphaned",
                span.0, span.1
            ),
        );
    }
    let bases = first_values(&paired, table, "telemetry");
    {
        let st = rep.stats.entry("telemetry".into()).or_default();
        st.compared = paired.len();
        st.unpaired_a = ua.len();
        st.unpaired_b = ub.len();
    }
    // T7/D23: the JS side's first tick at/after each NMEA reopen (from the
    // tape), with the fields its lost first chunk would have set.
    let js_side = match (a.meta.kind == "js", b.meta.kind == "js") {
        (true, false) => Some(0),
        (false, true) => Some(1),
        _ => None,
    };
    let mut d23_at: BTreeMap<i64, BTreeSet<&'static str>> = BTreeMap::new();
    if let Some(side) = js_side {
        let js = if side == 0 { &ta } else { &tb };
        for r in &facts.reopens {
            if let Some(t) = js
                .iter()
                .filter_map(|m| ts_of(&m.payload))
                .filter(|t| *t >= r.w)
                .min()
            {
                d23_at.entry(t).or_default().extend(d23_fields(r));
            }
        }
    }
    for (i, (x, y)) in paired.iter().enumerate() {
        let w = ts_of(&x.payload);
        let js_ts = match js_side {
            Some(0) => w,
            Some(_) => ts_of(&y.payload),
            None => None,
        };
        let nb = |j: Option<usize>, side: usize| {
            j.and_then(|j| paired.get(j)).map(|p| {
                if side == 0 {
                    &p.0.payload
                } else {
                    &p.1.payload
                }
            })
        };
        let (prev, next) = (i.checked_sub(1), Some(i + 1));
        let cx = Cx {
            table,
            play,
            stream: "telemetry".into(),
            at: format!(
                "ts A {} / B {}",
                w.unwrap_or(0),
                ts_of(&y.payload).unwrap_or(0)
            ),
            when: w,
            when_b: ts_of(&y.payload),
            deltas: &bases,
            first_wall,
            neighbors: [nb(prev, 0), nb(next, 0), nb(prev, 1), nb(next, 1)],
            d23: js_ts.and_then(|t| d23_at.get(&t)),
            facts,
            js_a,
        };
        flags_pair(&cx, x, y, &mut rep);
        if x.topic != y.topic {
            rep.push(cx.mismatch(
                "(topic)",
                Some(&Value::String(x.topic.clone())),
                Some(&Value::String(y.topic.clone())),
                UNTOLERATED.into(),
            ));
        }
        cmp(&cx, "", Some(&x.payload), Some(&y.payload), &mut rep);
    }
    // Unpaired ticks: a re-pair seam (rig-cut-ungated, bounded by the
    // re-pair), ONE QoS 0 tick racing each drop per side and span
    // (reconnect-cadence; WP1 follow-up amb. 3), the run's first/last tick,
    // ONE lead tick per logged gate skip (tick-drift).
    let mut cadence_used: BTreeSet<(&str, usize)> = BTreeSet::new();
    let mut skips_used: BTreeSet<usize> = BTreeSet::new();
    for (side, list) in [("A", &ua), ("B", &ub)] {
        for m in list.iter() {
            let w = ts_of(&m.payload);
            let cx = Cx {
                table,
                play,
                stream: "telemetry".into(),
                at: format!("unpaired {side} ts {}", w.unwrap_or(m.w)),
                when: w.or(Some(m.w)),
                when_b: None,
                deltas: &empty,
                first_wall,
                neighbors: [None; 4],
                d23: None,
                facts,
                js_a,
            };
            let wv = w.unwrap_or(m.w);
            let seam = orphans
                .iter()
                .any(|(s, t)| *s == side && *t == wv)
                .then(|| {
                    table
                        .decision_row("rig-cut-ungated")
                        .map(|(r, _)| format!("{r}/rig-cut-ungated"))
                })
                .flatten();
            let tol = seam
                .or_else(|| {
                    let id = cx.near_closed_as("reconnect-cadence")?;
                    let (_, row) = table.decision_row("reconnect-cadence")?;
                    let pad = row.around_closed_ms.unwrap_or(5000) as i64;
                    let k = play
                        .closed_spans
                        .iter()
                        .position(|(a, b)| wv >= a - pad && wv <= b + pad)?;
                    cadence_used.insert((side, k)).then_some(id)
                })
                .or_else(|| {
                    // The first/last tick of a run may have no partner.
                    (wv - first_wall <= 5_000 || play.end_wall - wv <= 3_000)
                        .then(|| "T4".to_string())
                })
                .or_else(|| {
                    // A lead tick the hold gate skipped at a drift flip.
                    let (rid, _) = table.decision_row("tick-drift")?;
                    let k = play
                        .skips
                        .iter()
                        .position(|(s, t)| s.eq_ignore_ascii_case(side) && (wv - t).abs() <= 400)?;
                    skips_used.insert(k).then(|| format!("{rid}/tick-drift"))
                })
                .unwrap_or_else(|| UNTOLERATED.into());
            let (pa, pb) = if side == "A" {
                (Some(&m.payload), None)
            } else {
                (None, Some(&m.payload))
            };
            rep.push(cx.mismatch(
                "(message)",
                pa.map(|_| &Value::Bool(true)),
                pb.map(|_| &Value::Bool(true)),
                tol,
            ));
        }
    }

    // ---- T2: seq increments per agent
    seq_increments(&ta, "A", play, facts, table, &mut rep);
    seq_increments(&tb, "B", play, facts, table, &mut rep);

    // ---- reports: quiet periods
    diff_reports(a, b, play, facts, table, &mut rep);

    // ---- in-order streams
    for (suffix, stream) in [
        ("command/response", "ack"),
        ("status", "status"),
        ("sms", "sms"),
    ] {
        let xa = a.stream(suffix);
        let xb = b.stream(suffix);
        let st = rep.stats.entry(stream.into()).or_default();
        st.compared = xa.len().min(xb.len());
        for i in 0..xa.len().max(xb.len()) {
            let (x, y) = (xa.get(i), xb.get(i));
            let cx = Cx {
                table,
                play,
                stream: stream.into(),
                at: format!("#{i}"),
                when: x.or(y).map(|m| m.w),
                when_b: None,
                deltas: &empty,
                first_wall,
                neighbors: [None; 4],
                d23: None,
                facts,
                js_a,
            };
            match (x, y) {
                (Some(x), Some(y)) => {
                    flags_pair(&cx, x, y, &mut rep);
                    if stream == "sms" {
                        // T8/O4 + T8/sms-duration judge their fields here;
                        // `cmp` compares everything else.
                        let (px, py, n) = sms_decisions(&cx, &x.payload, &y.payload, &mut rep);
                        cmp(&cx, "", Some(&px), Some(&py), &mut rep);
                        rep.stats.entry(stream.into()).or_default().leaves += n;
                    } else {
                        cmp(&cx, "", Some(&x.payload), Some(&y.payload), &mut rep)
                    }
                }
                _ => {
                    // A status message around a cut connection (online/LWT
                    // timing) is cadence; anything else is a real difference.
                    let tol = if stream == "status" {
                        cx.near_closed_as("reconnect-cadence")
                    } else {
                        None
                    }
                    .unwrap_or_else(|| UNTOLERATED.into());
                    rep.push(cx.mismatch(
                        "(message)",
                        x.map(|m| &m.payload),
                        y.map(|m| &m.payload),
                        tol,
                    ));
                }
            }
        }
    }

    // ---- files at end: every key either capture has (final.json: the
    // player's full set incl. refresh-modem.flag, etc/kvs.json,
    // etc/hardware.json, etc/pipelines/*), present on one side only = FAIL.
    let names: BTreeSet<&String> = a.finals.keys().chain(b.finals.keys()).collect();
    for name in names {
        let stream = match name.as_str() {
            "state.json" => "state".to_string(),
            "agent.pid" => "pidfile".to_string(),
            n => format!("file:{n}"),
        };
        let (fa, fb) = (
            a.finals.get(name).cloned().flatten(),
            b.finals.get(name).cloned().flatten(),
        );
        let st = rep.stats.entry(stream.clone()).or_default();
        if fa.is_none() && fb.is_none() {
            st.absent_both += 1;
            continue;
        }
        st.compared += 1;
        let cx = Cx {
            table,
            play,
            stream: stream.clone(),
            at: "end".into(),
            when: None,
            when_b: None,
            deltas: &empty,
            first_wall,
            neighbors: [None; 4],
            d23: None,
            facts,
            js_a,
        };
        match (&fa, &fb) {
            (Some(x), Some(y)) if x == y => {
                let v = parse_payload(x);
                rep.stats.entry(stream.clone()).or_default().leaves += leaf_count(&v);
            }
            (Some(x), Some(y)) if name == "refresh-modem.flag" => {
                // `String(now)` (worker.ts:1058): each agent's own wall ms (T1);
                // the content must still be a bare decimal ms.
                let ms = |t: &str| !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit());
                let tol = if ms(x) && ms(y) {
                    "T1".to_string()
                } else {
                    UNTOLERATED.into()
                };
                rep.push(cx.mismatch(
                    "(content)",
                    Some(&Value::String(x.clone())),
                    Some(&Value::String(y.clone())),
                    tol,
                ));
            }
            (Some(x), Some(y)) => {
                let (vx, vy) = (parse_payload(x), parse_payload(y));
                let json = !vx.is_string() && !vy.is_string();
                let (mut vx, mut vy) = (vx, vy);
                if name == "state.json" {
                    // recentEvents are compared as the `events` multiset.
                    for v in [&mut vx, &mut vy] {
                        if let Some(o) = v.as_object_mut() {
                            o.remove("recentEvents");
                        }
                    }
                    end_snapshot(&mut vx, &mut vy, a, b, &cx, !sim, &mut rep);
                }
                if name == "gnss-sky.json" {
                    sky_hold_at_write(&mut vx, &mut vy, &cx, &mut rep);
                }
                let before = rep.mismatches.len();
                cmp(&cx, "", Some(&vx), Some(&vy), &mut rep);
                if !json {
                    continue;
                }
                // Byte layout (review-wp7 finding 9): equal shapes must have
                // the same whitespace/indent skeleton; differing shapes the
                // same writer style.
                let (kx, ky) = (json_skeleton(x), json_skeleton(y));
                let shape = |k: &str| k.split_whitespace().collect::<String>();
                let layout = if shape(&kx) == shape(&ky) {
                    (kx != ky).then(|| (kx.clone(), ky.clone()))
                } else {
                    let (sx, sy) = (json_style(x), json_style(y));
                    (sx != sy).then_some((sx, sy))
                };
                if let Some((lx, ly)) = layout {
                    rep.push(cx.mismatch(
                        "(layout)",
                        Some(&Value::String(lx)),
                        Some(&Value::String(ly)),
                        UNTOLERATED.into(),
                    ));
                } else if rep.mismatches.len() == before && name == "settings.json" {
                    // Same JSON, same skeleton, different bytes (escapes) — a
                    // FAIL: the writer is byte-exact (atomicWriteJson).
                    rep.push(cx.mismatch(
                        "(bytes)",
                        Some(&Value::String(x.clone())),
                        Some(&Value::String(y.clone())),
                        UNTOLERATED.into(),
                    ));
                }
            }
            _ => rep.push(cx.mismatch(
                "(file)",
                fa.as_ref().map(|_| &Value::Bool(true)),
                fb.as_ref().map(|_| &Value::Bool(true)),
                UNTOLERATED.into(),
            )),
        }
    }

    // ---- recentEvents msg multiset
    diff_events(a, b, play, facts, table, &mut rep);

    // ---- sudo argv sequence
    diff_sudo(a, b, table, &mut rep);

    cap_row_hits(&mut rep, table, play, "input-edge");

    // Per-kind comparison counts (review-wp7): every kind is listed.
    for k in EXPECTED_KINDS {
        rep.stats.entry(k.to_string()).or_default();
    }
    rep.hits = row_hits(&rep, table);

    rep
}

/// Every comparison kind diff.md lists, exercised or not.
const EXPECTED_KINDS: &[&str] = &[
    "telemetry",
    "report",
    "ack",
    "status",
    "sms",
    "events",
    "sudo",
    "mqtt-flags",
    "state",
    "pidfile",
    "file:settings.json",
    "file:config-sync.json",
    "file:commands.json",
    "file:gnss-sky.json",
    "file:refresh-modem.flag",
    "file:etc/kvs.json",
    "file:etc/hardware.json",
];
/// Kinds every run must compare at least once.
const CORE_KINDS: &[&str] = &["telemetry", "report", "events", "state", "mqtt-flags"];

/// A row with `max_hits_per_10min`: above that many distinct ticks per 10 min
/// of run every hit of the row becomes UNTOLERATED (a systematic shift is a
/// bug, not timing); the count is printed either way.
fn cap_row_hits(rep: &mut Report, table: &Table, play: &PlayInfo, row_id: &str) {
    let Some((rid, row)) = table.decision_row(row_id) else {
        return;
    };
    let Some(per) = row.max_hits_per_10min else {
        return;
    };
    let id = format!("{rid}/{row_id}");
    let dur = (play.end_wall - play.start_wall).max(0);
    let allowed = per * ((dur + 599_999) / 600_000).max(1) as usize;
    let Report {
        mismatches,
        stats,
        inputs,
        ..
    } = rep;
    let ticks: BTreeSet<(&str, &str)> = mismatches
        .iter()
        .filter(|m| m.tol == id)
        .map(|m| (m.stream.as_str(), m.at.as_str()))
        .collect();
    let n = ticks.len();
    inputs.insert(
        format!("{id} ticks"),
        format!("{n} (cap {allowed} for this run)"),
    );
    if n > allowed {
        for m in mismatches.iter_mut().filter(|m| m.tol == id) {
            m.tol = UNTOLERATED.into();
            let st = stats.entry(m.stream.clone()).or_default();
            st.tolerated = st.tolerated.saturating_sub(1);
            st.untolerated += 1;
        }
    }
}

/// T2 "seq compared as deltas": within each agent, consecutive telemetry
/// (by `ts`) must advance `seq` by exactly 1; a jump is tolerated only around
/// a closed span (T8 `cadence`: Rust skips publishing while disconnected).
fn seq_increments(
    msgs: &[&Msg],
    side: &str,
    play: &PlayInfo,
    facts: &InputFacts,
    table: &Table,
    rep: &mut Report,
) {
    let js_a = None;
    let mut v: Vec<(i64, i64)> = msgs
        .iter()
        .filter_map(|m| Some((ts_of(&m.payload)?, m.payload.get("seq")?.as_i64()?)))
        .collect();
    v.sort();
    let empty = BTreeMap::new();
    let bound = table.decision_row("offline-queue-bound");
    let pad = bound
        .as_ref()
        .and_then(|(_, r)| r.around_closed_ms)
        .unwrap_or(5000) as i64;
    let min_span = bound.as_ref().and_then(|(_, r)| r.min_span_ms).unwrap_or(0) as i64;
    for w in v.windows(2) {
        let ((t0, s0), (t1, s1)) = (w[0], w[1]);
        if s1 - s0 == 1 {
            continue;
        }
        let cx = Cx {
            table,
            play,
            stream: "telemetry".into(),
            at: format!("{side} ts {t1}"),
            when: Some(t1),
            when_b: None,
            deltas: &empty,
            first_wall: play.start_wall,
            neighbors: [None; 4],
            d23: None,
            facts,
            js_a,
        };
        // A jump across an outage longer than the offline queue holds (1000
        // payloads ≈ 16.7 min at 1 Hz): Rust dropped the oldest queued ticks
        // (WP1 follow-up amb. 1). Shorter outages must flush every tick.
        let tol = bound
            .as_ref()
            .and_then(|(rid, _)| {
                play.closed_spans
                    .iter()
                    .any(|(a, b)| b - a >= min_span && t0 <= b + pad && t1 >= a - pad)
                    .then(|| format!("{rid}/offline-queue-bound"))
            })
            .unwrap_or_else(|| UNTOLERATED.into());
        rep.push(cx.mismatch(
            "seq (increment)",
            Some(&Value::from(s0)),
            Some(&Value::from(s1)),
            tol,
        ));
    }
}

fn quiet_periods(a: &[&Msg], b: &[&Msg], quiet_ms: i64) -> Vec<(i64, i64)> {
    let mut ws: Vec<i64> = a.iter().chain(b.iter()).map(|m| m.w).collect();
    ws.sort();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for w in ws {
        match out.last_mut() {
            Some(last) if w - last.1 < quiet_ms => last.1 = w,
            _ => out.push((w, w)),
        }
    }
    out
}

fn diff_reports(
    a: &Capture,
    b: &Capture,
    play: &PlayInfo,
    facts: &InputFacts,
    table: &Table,
    rep: &mut Report,
) {
    let js_a = js_a_of(a, b);
    let ra = a.stream("config/reported");
    let rb = b.stream("config/reported");
    let r5 = table.of_kind("report_count", "report").next();
    let quiet = r5.and_then(|r| r.quiet_ms).unwrap_or(10_000) as i64;
    let max_delta = r5.and_then(|r| r.max_count_delta).unwrap_or(1);
    let t5 = r5.map(|r| r.id.clone()).unwrap_or_else(|| "T5".into());
    let empty = BTreeMap::new();
    let mut compared = 0;
    for (k, (s, e)) in quiet_periods(&ra, &rb, quiet).into_iter().enumerate() {
        let pa: Vec<&Msg> = ra
            .iter()
            .copied()
            .filter(|m| m.w >= s && m.w <= e)
            .collect();
        let pb: Vec<&Msg> = rb
            .iter()
            .copied()
            .filter(|m| m.w >= s && m.w <= e)
            .collect();
        let at = format!("period {k} [{s}..{e}]");
        let cx = Cx {
            table,
            play,
            stream: "report".into(),
            at: at.clone(),
            when: Some(s),
            when_b: None,
            deltas: &empty,
            first_wall: play.start_wall,
            neighbors: [None; 4],
            d23: None,
            facts,
            js_a,
        };
        if pa.len() != pb.len() {
            // T5, NARROWED (review-wp7): only where two report triggers
            // (config / command / cli / LTE-file inputs on the tape) fall
            // within the 2 s debounce of each other inside the period.
            let mut trig: Vec<i64> = facts
                .inputs
                .iter()
                .filter(|(t, k)| {
                    matches!(*k, "config" | "command" | "cli" | "lte_file")
                        && *t >= s - DEBOUNCE_MS
                        && *t <= e
                })
                .map(|(t, _)| *t)
                .collect();
            trig.sort();
            let straddle = trig.windows(2).any(|w| w[1] - w[0] <= DEBOUNCE_MS);
            // T8 reconnect-cadence, "report count after a cut" (modelled, not
            // T5): each reconnect's retained redelivery triggers a `cloud`
            // report, and mqtt.js reconnecting sooner (2 s) can see an
            // intermediate retained rev the Rust agent never sees (calibration
            // period 3: JS saw rev 1 then rev 2). So in a period overlapping a
            // (extended) span: |Δ| ≤ the spans it overlaps, and every report
            // the longer side has beyond the shorter's (by origin + rev) is
            // origin `cloud` with a rev the tape's config publishes carry.
            let after_cut = table
                .decision_row("reconnect-cadence")
                .and_then(|(rid, row)| {
                    let pad = row.around_closed_ms.unwrap_or(5000) as i64;
                    let spans = play
                        .closed_spans
                        .iter()
                        .filter(|(cs, ce)| s <= ce + pad && e >= cs - pad)
                        .count();
                    let key = |m: &&Msg| {
                        (
                            m.payload["origin"].as_str().unwrap_or("").to_string(),
                            m.payload["rev"].as_i64(),
                        )
                    };
                    let (long, short) = if pa.len() > pb.len() {
                        (&pa, &pb)
                    } else {
                        (&pb, &pa)
                    };
                    let mut rest: Vec<(String, Option<i64>)> = short.iter().map(key).collect();
                    let extra: Vec<(String, Option<i64>)> = long
                        .iter()
                        .map(key)
                        .filter(|k| match rest.iter().position(|r| r == k) {
                            Some(i) => {
                                rest.remove(i);
                                false
                            }
                            None => true,
                        })
                        .collect();
                    let ok = spans > 0
                        && pa.len().abs_diff(pb.len()) <= spans
                        && extra.iter().all(|(o, r)| {
                            o == "cloud" && r.is_some_and(|r| facts.config_revs.contains(&r))
                        });
                    ok.then(|| format!("{rid}/reconnect-cadence"))
                });
            let tol = if straddle
                && pa.len().abs_diff(pb.len()) <= max_delta
                && !pa.is_empty()
                && !pb.is_empty()
            {
                t5.clone()
            } else if let Some(id) = after_cut {
                id
            } else {
                UNTOLERATED.into()
            };
            rep.push(cx.mismatch(
                "(count)",
                Some(&Value::from(pa.len())),
                Some(&Value::from(pb.len())),
                tol,
            ));
            // Only the last of each side is binding.
            if let (Some(x), Some(y)) = (pa.last(), pb.last()) {
                compared += 1;
                let cx = Cx {
                    at: format!("{at} last"),
                    when: Some(x.w),
                    when_b: Some(y.w),
                    ..cx
                };
                flags_pair(&cx, x, y, rep);
                cmp(&cx, "", Some(&x.payload), Some(&y.payload), rep);
            }
        } else {
            for (i, (x, y)) in pa.iter().zip(&pb).enumerate() {
                compared += 1;
                let cx = Cx {
                    table,
                    play,
                    stream: "report".into(),
                    at: format!(
                        "{at} #{i}{}",
                        if i + 1 == pa.len() { " (last)" } else { "" }
                    ),
                    when: Some(x.w),
                    when_b: Some(y.w),
                    deltas: &empty,
                    first_wall: play.start_wall,
                    neighbors: [None; 4],
                    d23: None,
                    facts,
                    js_a,
                };
                flags_pair(&cx, x, y, rep);
                cmp(&cx, "", Some(&x.payload), Some(&y.payload), rep);
            }
        }
    }
    rep.stats.entry("report".into()).or_default().compared = compared;
}

/// The `level: msg` key of a state.json event after the T8 normalisations:
/// the Rust `<task>: ` segment of an unhandledRejection (`task-prefix`), a
/// transport-library error text (`mqtt-error-text`), and the suffix of an
/// upload retry (`s3-error-text`, pending Phase 3). Returns the key and the
/// row that changed it.
fn event_key(level: &str, msg: &str, kind: &str, table: &Table) -> (String, Option<String>) {
    let mut msg = msg.to_string();
    let mut hit = None;
    if kind == "rust" {
        if let Some((rid, _)) = table.decision_row("task-prefix") {
            if let Some(rest) = msg.strip_prefix("unhandledRejection: ") {
                if let Some((task, tail)) = rest.split_once(": ") {
                    if !task.is_empty()
                        && task
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    {
                        msg = format!("unhandledRejection: {tail}");
                        hit = Some(format!("{rid}/task-prefix"));
                    }
                }
            }
        }
    }
    if let Some((rid, row)) = table.decision_row("s3-error-text") {
        for p in &row.event_prefixes {
            if let Some(rest) = msg.strip_prefix(p.as_str()) {
                if let Some((secs, _)) = rest.split_once(": ") {
                    msg = format!("{p}{secs}: <transport error>");
                    hit = Some(format!("{rid}/s3-error-text"));
                }
            }
        }
    }
    if let Some((rid, row)) = table.decision_row("fs-error-text") {
        for p in &row.event_prefixes {
            if let Some(rest) = msg.strip_prefix(p.as_str()) {
                let norm = normalise_fs_error(rest);
                if norm != rest {
                    msg = format!("{p}{norm}");
                    hit = Some(format!("{rid}/fs-error-text"));
                }
            }
        }
    }
    if level == "error" {
        if let Some((rid, row)) = table.decision_row("mqtt-error-text") {
            if !row.own_prefixes.iter().any(|p| msg.starts_with(p.as_str()))
                && !row.exact_texts.contains(&msg)
            {
                msg = "<transport error>".into();
                hit = Some(format!("{rid}/mqtt-error-text"));
            }
        }
    }
    (format!("{level}: {msg}"), hit)
}

/// Failed atomic-write text after phase-2/integration: both agents name Node's
/// step and the temp file (`open '<dir>/.settings.json.tmp.<pid>.<n>'`,
/// `rename '<tmp>' -> '<dest>'`, pathless write/fsync); only the temp file's
/// `<pid>.<n>` is per-process (T8 `fs-error-text`). Anything else unchanged.
fn normalise_fs_error(rest: &str) -> String {
    let mut out = String::with_capacity(rest.len());
    let mut i = 0;
    let b = rest.as_bytes();
    while i < b.len() {
        if rest[i..].starts_with(".tmp.") {
            let start = i + 5;
            let mut j = start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > start && j < b.len() && b[j] == b'.' {
                let mut k = j + 1;
                while k < b.len() && b[k].is_ascii_digit() {
                    k += 1;
                }
                if k > j + 1 {
                    out.push_str(".tmp.<pid>.<n>");
                    i = k;
                    continue;
                }
            }
        }
        let ch = rest[i..].chars().next().unwrap_or_default();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The TS real-mode sweep result when no interface-05 tty exists
/// (`sms-modem.ts:524`): T8/O4's JS half.
const TS_SMS_NO_MODEM: (&str, &str) = ("no_modem", "no SIMCom interface 05 tty");
/// The Rust real-mode sweep until Phase 4 (`command_flow.rs:511-516`): O4.
const RS_SMS_UNAVAILABLE: (&str, &str) =
    ("error", xorgate_core::command_flow::SMS_READER_UNAVAILABLE);

/// T8/O4 applies: a real-mode tape (`meta.env.MODE = real`, worker.ts:930)
/// and one JS + one Rust agent; `Some(js is A)`.
fn o4_applies(facts: &InputFacts, js_a: Option<bool>) -> Option<bool> {
    js_a.filter(|_| facts.mode.as_deref() == Some("real"))
}

/// T8/O4 and T8/sms-duration on one paired `…/sms` result. Each row judges
/// exactly its own leaves (O4: `status` + `error`, only when the JS document
/// holds the TS no-modem pair AND the Rust one the O4 pair; sms-duration:
/// `durationMs`, only when both are non-negative integers ≤ `max_delta_ms`
/// apart); those leaves are removed from the returned documents, which `cmp`
/// compares as usual. The third value = the leaves removed (stats).
fn sms_decisions(cx: &Cx, x: &Value, y: &Value, rep: &mut Report) -> (Value, Value, usize) {
    let (mut x, mut y) = (x.clone(), y.clone());
    let mut removed = 0;
    let pair_of = |v: &Value| {
        (
            v.get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            v.get("error")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    };
    let o4 = cx.table.matcher_rows("read_sms_unavailable", "sms");
    if let (Some(js_a), Some(id)) = (o4_applies(cx.facts, cx.js_a), o4.first()) {
        let (js, rs) = if js_a { (&x, &y) } else { (&y, &x) };
        let (pj, pr) = (pair_of(js), pair_of(rs));
        if (pj.0.as_str(), pj.1.as_str()) == TS_SMS_NO_MODEM
            && (pr.0.as_str(), pr.1.as_str()) == RS_SMS_UNAVAILABLE
        {
            for k in ["status", "error"] {
                rep.push(cx.mismatch(k, x.get(k), y.get(k), id.clone()));
                for d in [&mut x, &mut y] {
                    if let Some(o) = d.as_object_mut() {
                        o.remove(k);
                    }
                }
                removed += 1;
            }
        }
    }
    if let Some((rid, row)) = cx
        .table
        .decision_row("sms-duration")
        .filter(|(_, r)| crate::tolerances::stream_matches(&r.streams, "sms"))
    {
        let max = row.max_delta_ms.unwrap_or(0);
        let (p, q) = (
            x.get("durationMs").and_then(Value::as_u64),
            y.get("durationMs").and_then(Value::as_u64),
        );
        if let (Some(p), Some(q)) = (p, q) {
            if p.abs_diff(q) <= max {
                if p != q {
                    let tol = format!("{rid}/sms-duration");
                    rep.push(cx.mismatch(
                        "durationMs",
                        x.get("durationMs"),
                        y.get("durationMs"),
                        tol,
                    ));
                }
                for d in [&mut x, &mut y] {
                    if let Some(o) = d.as_object_mut() {
                        o.remove("durationMs");
                    }
                }
                removed += 1;
            }
        }
    }
    (x, y, removed)
}

/// A `read_sms <id>: status=… durationMs=<n>[ error=…]` event line
/// (worker.ts:931-935, command_flow.rs `read_sms_line`) →
/// `(line with durationMs=<ms>, request id, n)`; with `o4` the Rust O4 words
/// are rewritten to the TS no-modem words (the second value says so).
/// Anything else → `None`.
fn sms_event_norm(msg: &str, o4: bool) -> Option<(String, bool, String, u64)> {
    let rest = msg.strip_prefix("read_sms ")?;
    let (id, body) = rest.split_once(": ")?;
    if id.is_empty() || id.contains(' ') || !body.starts_with("status=") {
        return None;
    }
    let (head, tail) = body.split_once(" durationMs=")?;
    let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
    let n: u64 = tail.get(..digits)?.parse().ok()?;
    let after = &tail[digits..];
    if !(after.is_empty() || after.starts_with(" error=")) {
        return None;
    }
    let mut head = head.to_string();
    let mut after = after.to_string();
    let mut hit = false;
    let rs_status = format!("status={} ", RS_SMS_UNAVAILABLE.0);
    let rs_error = format!(" error={}", RS_SMS_UNAVAILABLE.1);
    if o4 && head.starts_with(&rs_status) && after == rs_error {
        head = format!("status={} {}", TS_SMS_NO_MODEM.0, &head[rs_status.len()..]);
        after = format!(" error={}", TS_SMS_NO_MODEM.1);
        hit = true;
    }
    Some((
        format!("read_sms {id}: {head} durationMs=<ms>{after}"),
        hit,
        id.to_string(),
        n,
    ))
}

fn diff_events(
    a: &Capture,
    b: &Capture,
    play: &PlayInfo,
    facts: &InputFacts,
    table: &Table,
    rep: &mut Report,
) {
    let js_a = js_a_of(a, b);
    let o4 = table
        .matcher_rows("read_sms_unavailable", "events")
        .into_iter()
        .next()
        .filter(|_| o4_applies(facts, js_a).is_some());
    let timing = table
        .decision_row("sms-duration")
        .filter(|(_, r)| crate::tolerances::stream_matches(&r.streams, "events"));
    // Per side: request id → the `durationMs` of each of its read_sms lines.
    type Durations = BTreeMap<String, Vec<u64>>;
    let count = |c: &Capture| {
        let mut m: BTreeMap<String, (usize, Option<String>)> = BTreeMap::new();
        let mut durs: Durations = BTreeMap::new();
        for (_, level, msg) in &c.events {
            let (mut k, mut hit) = event_key(level, msg, &c.meta.kind, table);
            if timing.is_some() {
                let rust = c.meta.kind == "rust";
                if let Some((line, o4_hit, id, n)) = sms_event_norm(msg, o4.is_some() && rust) {
                    k = format!("{level}: {line}");
                    if o4_hit {
                        hit = o4.clone();
                    }
                    durs.entry(id).or_default().push(n);
                }
            }
            let e = m.entry(k).or_insert((0, None));
            e.0 += 1;
            if hit.is_some() {
                e.1 = hit;
            }
        }
        (m, durs)
    };
    let ((ca, da), (cb, db)) = (count(a), count(b));
    // T8/sms-duration on the event lines: paired per request id, in order.
    if let Some((rid, row)) = &timing {
        let max = row.max_delta_ms.unwrap_or(0);
        for (id, xs) in &da {
            let Some(ys) = db.get(id) else { continue };
            for (p, q) in xs.iter().zip(ys) {
                rep.stats.entry("events".into()).or_default().compared += 1;
                if p == q {
                    continue;
                }
                rep.push(Mismatch {
                    stream: "events".into(),
                    at: format!("read_sms {id}"),
                    path: "durationMs".into(),
                    a: p.to_string(),
                    b: q.to_string(),
                    tol: if p.abs_diff(*q) <= max {
                        format!("{rid}/sms-duration")
                    } else {
                        UNTOLERATED.into()
                    },
                });
            }
        }
    }
    let keys: BTreeSet<&String> = ca.keys().chain(cb.keys()).collect();
    let t6 = table.of_kind("phase_owned", "events").next();
    let cadence = table.decision_row("reconnect-cadence");
    let had_cut = !play.closed_spans.is_empty();
    // Each agent connects once at start and once after every cut: a
    // different number of `connected to` is a reconnect loop or a lost
    // reconnect (review-wp7: `connected to` = #spans + 1 on both sides).
    let connects = |c: &BTreeMap<String, (usize, Option<String>)>| -> usize {
        c.iter()
            .filter(|(k, _)| k.starts_with("info: connected to "))
            .map(|(_, (n, _))| n)
            .sum()
    };
    let want = play.closed_spans.len() + 1;
    // (Skipped when neither capture journaled any event: a hand-built fixture.)
    let journaled = !(a.events.is_empty() && b.events.is_empty());
    for (side, n) in [("A", connects(&ca)), ("B", connects(&cb))]
        .into_iter()
        .filter(|_| journaled)
    {
        rep.stats.entry("events".into()).or_default().compared += 1;
        if n != want {
            rep.push(Mismatch {
                stream: "events".into(),
                at: format!("{side} count"),
                path: "info: connected to … (count = closed spans + 1)".into(),
                a: if side == "A" {
                    n.to_string()
                } else {
                    want.to_string()
                },
                b: if side == "B" {
                    n.to_string()
                } else {
                    want.to_string()
                },
                tol: UNTOLERATED.into(),
            });
        }
    }
    // `(count A, count B)` → the JS side has it and the Rust side does not.
    let js_only = |x: usize, y: usize| match (a.meta.kind.as_str(), b.meta.kind.as_str()) {
        ("js", "rust") => x > 0 && y == 0,
        ("rust", "js") => y > 0 && x == 0,
        _ => false,
    };
    rep.stats.entry("events".into()).or_default().compared += keys.len();
    for k in keys {
        let (x, hx) = ca.get(k).cloned().unwrap_or((0, None));
        let (y, hy) = cb.get(k).cloned().unwrap_or((0, None));
        let norm = hx.or(hy);
        if x == y {
            if let Some(id) = norm {
                // Equal only after a normalisation: record which row did it.
                if k.contains("<transport error>")
                    || k.contains("unhandledRejection")
                    || o4.as_ref() == Some(&id)
                {
                    rep.push(Mismatch {
                        stream: "events".into(),
                        at: "multiset (normalised)".into(),
                        path: k.clone(),
                        a: x.to_string(),
                        b: y.to_string(),
                        tol: id,
                    });
                }
            }
            continue;
        }
        let msg = k.split_once(": ").map(|(_, m)| m).unwrap_or(k);
        let rust_only = table.decision_row("rust-only-events").filter(|(_, row)| {
            x == 0
                && b.meta.kind == "rust"
                && row
                    .event_prefixes
                    .iter()
                    .any(|p| msg.starts_with(p.as_str()))
        });
        let tol = if let Some((rid, _)) = rust_only {
            format!("{rid}/rust-only-events")
        } else if let Some(r) = t6.filter(|r| {
            // Phase 3/4 texts: JS has it, Rust has none of it (one-directional).
            js_only(x, y) && r.event_prefixes.iter().any(|p| msg.starts_with(p.as_str()))
        }) {
            r.id.clone()
        } else if let Some((rid, _)) = cadence.as_ref().filter(|(_, row)| {
            // NARROWED (review-wp7), only in a run with a closed span:
            // `reconnecting…` / `connection closed` — the Rust agent at most
            // as many as mqtt.js (its backoff is slower, never faster);
            // the transport-error class — either way (detection differs).
            had_cut
                && ((row.event_exact.iter().any(|e| e == k)
                    && match js_a {
                        Some(true) => y <= x,
                        Some(false) => x <= y,
                        None => true,
                    })
                    || row.event_prefixes.iter().any(|p| k.starts_with(p.as_str())))
        }) {
            format!("{rid}/reconnect-cadence")
        } else {
            UNTOLERATED.into()
        };
        rep.push(Mismatch {
            stream: "events".into(),
            at: "multiset".into(),
            path: k.clone(),
            a: x.to_string(),
            b: y.to_string(),
            tol,
        });
    }
}

fn diff_sudo(a: &Capture, b: &Capture, table: &Table, rep: &mut Report) {
    let t6 = table.of_kind("phase_owned", "sudo").next();
    let owned = |argv: &str| {
        t6.is_some_and(|r| {
            r.sudo_argv_contains
                .iter()
                .any(|p| argv.contains(p.as_str()))
        })
    };
    let o3 = table.decision_row("O3");
    let norm = |argv: &str| -> (String, bool) {
        let mut s = argv.to_string();
        let mut hit = false;
        if let Some((_, row)) = &o3 {
            for (from, to) in &row.normalize {
                if s.contains(from.as_str()) {
                    s = s.replace(from.as_str(), to);
                    hit = true;
                }
            }
        }
        (s, hit)
    };
    let xa: Vec<&String> = a.sudo.iter().filter(|s| !owned(s)).collect();
    let xb: Vec<&String> = b.sudo.iter().filter(|s| !owned(s)).collect();
    for (side, all, kept) in [("A", &a.sudo, xa.len()), ("B", &b.sudo, xb.len())] {
        if all.len() != kept {
            rep.push(Mismatch {
                stream: "sudo".into(),
                at: format!("{side} phase-owned calls"),
                path: "(argv)".into(),
                a: if side == "A" {
                    (all.len() - kept).to_string()
                } else {
                    String::new()
                },
                b: if side == "B" {
                    (all.len() - kept).to_string()
                } else {
                    String::new()
                },
                tol: t6
                    .map(|r| r.id.clone())
                    .unwrap_or_else(|| UNTOLERATED.into()),
            });
        }
    }
    let st = rep.stats.entry("sudo".into()).or_default();
    st.compared = xa.len().min(xb.len());
    for i in 0..xa.len().max(xb.len()) {
        let (x, y) = (xa.get(i), xb.get(i));
        let tol = match (x, y) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => {
                let ((nx, hx), (ny, hy)) = (norm(x), norm(y));
                if nx == ny && (hx || hy) {
                    o3.as_ref()
                        .map(|(r, _)| format!("{r}/O3"))
                        .unwrap_or_else(|| UNTOLERATED.into())
                } else {
                    UNTOLERATED.into()
                }
            }
            _ => UNTOLERATED.into(),
        };
        rep.push(Mismatch {
            stream: "sudo".into(),
            at: format!("#{i}"),
            path: "(argv)".into(),
            a: x.map(|s| s.to_string()).unwrap_or_else(|| "∅".into()),
            b: y.map(|s| s.to_string()).unwrap_or_else(|| "∅".into()),
            tol,
        });
    }
}

/// Every row of the table (a rule without rows, else each of its rows) with
/// the number of mismatches citing it (`<id>` or `<id>/<sub>`), 0 included.
pub fn row_hits(rep: &Report, table: &Table) -> Vec<(String, usize)> {
    let mut ids: Vec<String> = Vec::new();
    for r in &table.rules {
        if r.rows.is_empty() {
            if !ids.contains(&r.id) {
                ids.push(r.id.clone());
            }
        } else {
            ids.extend(r.rows.iter().map(|w| format!("{}/{}", r.id, w.id)));
        }
    }
    ids.into_iter()
        .map(|id| {
            let n = rep
                .mismatches
                .iter()
                .filter(|m| m.tol == id || m.tol.starts_with(&format!("{id}/")))
                .count();
            (id, n)
        })
        .collect()
}

/// `diff.md`.
pub fn render(
    rep: &Report,
    a: &Capture,
    b: &Capture,
    play: &PlayInfo,
    table: &Table,
    run: &Path,
) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "# Replay diff — A ({}) vs B ({})\n",
        a.meta.kind, b.meta.kind
    );
    let _ = writeln!(o, "- run: `{}`", run.display());
    let _ = writeln!(o, "- tape: `{}`", play.tape);
    let _ = writeln!(
        o,
        "- wall {} → {} ({:.0} s), shift {} s{}",
        play.start_wall,
        play.end_wall,
        (play.end_wall - play.start_wall) as f64 / 1000.0,
        play.shift_s,
        if play.calibration {
            ", CALIBRATION (C3a)"
        } else {
            ""
        }
    );
    let _ = writeln!(
        o,
        "- deviceIds: A `{}`, B `{}` (normalised to `{{id}}`, T2)",
        a.meta.device_id, b.meta.device_id
    );
    let unt = rep.untolerated();
    let _ = writeln!(
        o,
        "- verdict: **{}** ({} untolerated, {} tolerated)\n",
        if unt == 0 { "PASS" } else { "FAIL" },
        unt,
        rep.mismatches.len() - unt
    );
    let _ = writeln!(o, "## Inputs the diff derived\n");
    for (k, v) in &rep.inputs {
        let _ = writeln!(o, "- {k}: {v}");
    }
    let _ = writeln!(o);
    let _ = writeln!(o, "## Comparisons per kind\n");
    let _ = writeln!(
        o,
        "| kind | compared | leaves | absent on both | unpaired A | unpaired B | tolerated | UNTOLERATED |"
    );
    let _ = writeln!(o, "|---|---|---|---|---|---|---|---|");
    for (s, st) in &rep.stats {
        let compared = if st.compared == 0 {
            "**0 — not exercised**".to_string()
        } else {
            st.compared.to_string()
        };
        let _ = writeln!(
            o,
            "| {s} | {compared} | {} | {} | {} | {} | {} | {} |",
            st.leaves, st.absent_both, st.unpaired_a, st.unpaired_b, st.tolerated, st.untolerated
        );
    }
    let _ = writeln!(o, "\n## Hits per tolerance row (every row of the table)\n");
    let _ = writeln!(o, "| row | hits |");
    let _ = writeln!(o, "|---|---|");
    for (id, n) in row_hits(rep, table) {
        let _ = writeln!(o, "| {id} | {n} |");
    }
    let mut by_tol: BTreeMap<&str, Vec<&Mismatch>> = BTreeMap::new();
    for m in &rep.mismatches {
        by_tol.entry(m.tol.as_str()).or_default().push(m);
    }
    let _ = writeln!(o, "\n## By tolerance id\n");
    let _ = writeln!(o, "| id | count | rule |");
    let _ = writeln!(o, "|---|---|---|");
    for (id, list) in &by_tol {
        let base = id.split('/').next().unwrap_or(id);
        let title = table.rule(base).map(|r| r.title.as_str()).unwrap_or("");
        let _ = writeln!(o, "| {id} | {} | {} |", list.len(), title);
    }
    if let Some(list) = by_tol.get(UNTOLERATED) {
        let _ = writeln!(o, "\n## UNTOLERATED ({})\n", list.len());
        for m in list.iter().take(500) {
            let _ = writeln!(
                o,
                "- **{}** {} `{}`: A=`{}` B=`{}`",
                m.stream, m.at, m.path, m.a, m.b
            );
        }
        if list.len() > 500 {
            let _ = writeln!(o, "- … {} more in diff.json", list.len() - 500);
        }
    }
    let _ = writeln!(o, "\n## Tolerated (first 20 per id; all in diff.json)\n");
    for (id, list) in &by_tol {
        if *id == UNTOLERATED {
            continue;
        }
        let _ = writeln!(o, "### {id} ({})\n", list.len());
        for m in list.iter().take(20) {
            let _ = writeln!(
                o,
                "- {} {} `{}`: A=`{}` B=`{}`",
                m.stream, m.at, m.path, m.a, m.b
            );
        }
        let _ = writeln!(o);
    }
    o
}

/// The tape play.json names, as found from where the diff runs: the path
/// itself (inside the rig), `<run>/<name>`, or `tools/replay/.cache/tapes/<name>`
/// (from the repo root). Unchanged when none exists (the facts report it).
fn resolve_tape(run: &Path, tape: &str) -> String {
    if tape.is_empty() || Path::new(tape).exists() {
        return tape.to_string();
    }
    let Some(name) = Path::new(tape).file_name() else {
        return tape.to_string();
    };
    [
        run.join(name),
        PathBuf::from("tools/replay/.cache/tapes").join(name),
    ]
    .into_iter()
    .find(|p| p.exists())
    .map(|p| p.display().to_string())
    .unwrap_or_else(|| tape.to_string())
}

/// Load a run dir (`play.json`, `pairs.ndjson`, `a/`, `b/`).
/// A loaded run: both captures, the run facts and the player's pairs.
pub type Run = (Capture, Capture, PlayInfo, Vec<(i64, i64)>);

pub fn load_run(run: &Path, a: Option<PathBuf>, b: Option<PathBuf>) -> Result<Run> {
    let mut play: PlayInfo = std::fs::read(run.join("play.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    play.host = std::fs::read_to_string(run.join("host.ndjson"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            let w = v["w"].as_i64()?;
            let m: BTreeMap<String, f64> = v
                .as_object()?
                .iter()
                .filter(|(k, _)| k.as_str() != "w")
                .filter_map(|(k, x)| Some((k.clone(), x.as_f64()?)))
                .collect();
            Some((w, m))
        })
        .collect();
    play.skips = std::fs::read_to_string(run.join("pairs.ndjson"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| Some((v["skip"].as_str()?.to_string(), v["tick"].as_i64()?)))
        .collect();
    let pairs = std::fs::read_to_string(run.join("pairs.ndjson"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| Some((v["a"].as_i64()?, v["b"].as_i64()?)))
        .collect();
    // R1: the input instants are the player's ACTUAL releases. A run without
    // them (a player older than d1831f4) cannot be judged: fail loudly.
    let rel_path = run.join("releases.ndjson");
    let text = std::fs::read_to_string(&rel_path).with_context(|| {
        format!(
            "{}: missing — the diff needs the player's actual release instants (review-wp7 R1); \
             re-run the tape on a player that writes releases.ndjson",
            rel_path.display()
        )
    })?;
    play.releases = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str::<Release>)
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parsing {}", rel_path.display()))?;
    play.tape = resolve_tape(run, &play.tape);
    let ca = Capture::load(&a.unwrap_or_else(|| run.join("a")))?;
    let cb = Capture::load(&b.unwrap_or_else(|| run.join("b")))?;
    Ok((ca, cb, play, pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Partial fixtures: the comparison without the whole-run core-kind
    /// requirement (that is tested on its own).
    fn diff(a: &Capture, b: &Capture, play: &PlayInfo, pairs: &[(i64, i64)], t: &Table) -> Report {
        diff_with(a, b, play, &InputFacts::for_play(play), pairs, t)
    }

    /// `releases.ndjson` for `recs`: every non-prefilled record released at
    /// `start_wall + t`, except the `(t, wall)` overrides (a gate hold).
    fn released(play: &PlayInfo, recs: &[TapeRecord], late: &[(u64, i64)]) -> Vec<Release> {
        recs.iter()
            .filter(|r| !prefilled(r))
            .map(|r| {
                let w = late
                    .iter()
                    .find(|(t, _)| *t == r.t)
                    .map_or(play.start_wall + r.t as i64, |(_, w)| *w);
                Release {
                    w,
                    due: play.start_wall + r.t as i64,
                    t: r.t,
                    k: r.ev.kind().into(),
                }
            })
            .collect()
    }

    #[test]
    fn lte_apn_track_requires_the_apn_rereport_on_both_sides() {
        let boot = |apn: &str| serde_json::json!({"origin":"boot","effective":{"apn":{"state":"in-force","apn_applied":apn}}});
        let side = |kind: &str, n_ev: usize, apn: &str| {
            let mut c = cap(kind, vec![msg(1, "config/reported", boot(apn))]);
            c.events = (0..n_ev)
                .map(|i| (i as i64, "info".to_string(), APN_EVENT.to_string()))
                .collect();
            c
        };
        let facts = InputFacts {
            synthetic: vec!["lte-apn".into()],
            ..Default::default()
        };
        let unt = |a: &Capture, b: &Capture, f: &InputFacts| {
            let mut rep = Report::default();
            require_apn_rereport(&mut rep, a, b, f);
            rep.mismatches
                .iter()
                .filter(|m| m.tol == UNTOLERATED)
                .map(|m| m.stream.clone())
                .collect::<Vec<_>>()
        };
        let alt = crate::synth::LTE_APN_ALT;
        assert!(unt(&side("js", 1, alt), &side("rust", 1, alt), &facts).is_empty());
        // 0 = 0 is no evidence: both the event and the report are required.
        assert_eq!(
            unt(&side("js", 0, alt), &side("rust", 0, "synth.apn"), &facts),
            vec!["events", "report"]
        );
        assert_eq!(
            unt(&side("js", 1, alt), &side("rust", 0, alt), &facts),
            vec!["events"]
        );
        // Without the track nothing is required.
        assert!(unt(
            &side("js", 0, alt),
            &side("rust", 0, alt),
            &InputFacts::default()
        )
        .is_empty());
    }

    #[test]
    fn input_facts_use_the_actual_release_not_the_tape_schedule() {
        let rec = |t: u64, ev: TapeEvent| TapeRecord { t, w: t as i64, ev };
        let mut play = PlayInfo {
            start_wall: 100_000,
            end_wall: 200_000,
            ..Default::default()
        };
        let recs = vec![
            rec(500, TapeEvent::lte_file(Some(b"a"))),
            rec(10_000, TapeEvent::lte_file(Some(b"b"))),
            rec(20_000, TapeEvent::lte_file(Some(b"c"))),
            rec(
                30_000,
                TapeEvent::Cli {
                    argv: vec!["status".into()],
                },
            ),
        ];
        // A 10 s watch hold on the first change (far beyond the old −0.3/+2 s
        // snap window); the second is never released (past the end).
        play.releases = released(&play, &recs[..2], &[(10_000, 120_000)]);
        play.releases.push(Release {
            w: 131_000,
            due: 130_000,
            t: 30_000,
            k: "cli".into(),
        });
        let f = InputFacts::from_records(&recs, &play);
        assert_eq!(f.groups["lte"], vec![120_000]);
        assert_eq!(f.inputs, vec![(120_000, "lte_file"), (131_000, "cli")]);
        assert_eq!(f.release_lag["lte_file"], (1, 10_000));
        // Without releases nothing is an input, and for_play says why.
        let none = PlayInfo {
            tape: "x.ndjson".into(),
            ..play.clone()
        };
        let none = PlayInfo {
            releases: vec![],
            ..none
        };
        let f = InputFacts::for_play(&none);
        assert!(f.tape.is_none());
        assert!(f.tape_error.unwrap().contains("releases.ndjson"));
    }

    #[test]
    fn a_run_without_comparisons_of_a_core_kind_fails_and_rows_count_hits() {
        let t = table();
        let tel = |ts: i64| serde_json::json!({"v":1,"ts":ts,"seq":1,"metrics":{}});
        let a = cap("js", vec![msg(1, "telemetry", tel(1))]);
        let b = cap("rust", vec![msg(2, "telemetry", tel(2))]);
        let rep = super::diff(&a, &b, &PlayInfo::default(), &[], &t);
        let empty: Vec<&str> = rep
            .mismatches
            .iter()
            .filter(|m| m.path == "(no comparisons)" && m.tol == UNTOLERATED)
            .map(|m| m.stream.as_str())
            .collect();
        assert_eq!(
            empty,
            vec!["report", "events", "state"],
            "{:?}",
            rep.mismatches
        );
        assert_eq!(rep.stats["telemetry"].compared, 1);
        assert_eq!(rep.stats["telemetry"].leaves, 3);
        assert_eq!(rep.stats["sms"].compared, 0, "listed even when unexercised");
        // Every row of the table is listed, with its hit count.
        assert!(rep.hits.iter().any(|(id, n)| id == "T7/D23" && *n == 0));
        assert!(
            rep.hits.iter().any(|(id, n)| id == "T1" && *n == 1),
            "{:?}",
            rep.hits
        );
        let md = render(&rep, &a, &b, &PlayInfo::default(), &t, Path::new("run"));
        assert!(md.contains("| sms | **0 — not exercised** |"), "{md}");
        assert!(md.contains("| T8/input-edge | 0 |"), "{md}");
    }

    fn table() -> Table {
        Table::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/replay/tolerances.toml"),
        )
        .unwrap()
    }

    fn msg(w: i64, topic: &str, payload: Value) -> Msg {
        Msg {
            w,
            topic: format!("xorgate/devices/{{id}}/{topic}"),
            retain: false,
            qos: None,
            payload,
        }
    }

    fn cap(kind: &str, msgs: Vec<Msg>) -> Capture {
        Capture {
            meta: AgentMeta {
                label: kind.into(),
                kind: kind.into(),
                device_id: "x".into(),
            },
            msgs,
            finals: BTreeMap::new(),
            events: vec![],
            sudo: vec![],
        }
    }

    fn tel(ts: i64, seq: i64, cpu: f64, lat: Option<f64>) -> Value {
        let mut m = serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":seq,"metrics":{"system":{"cpu_usage":{"value":cpu,"unit":"%"}}}});
        if let Some(l) = lat {
            m["metrics"]["gps"] = serde_json::json!({"lat":{"value":l,"unit":"°"}});
        }
        m
    }

    #[test]
    fn fs_error_text_normalises_only_pid_and_counter() {
        assert_eq!(
            normalise_fs_error("EACCES: permission denied, open '/h/.settings.json.tmp.12.0'"),
            "EACCES: permission denied, open '/h/.settings.json.tmp.<pid>.<n>'"
        );
        assert_eq!(
            normalise_fs_error(
                "EXDEV: cross-device link not permitted, rename '/h/.k.tmp.7.3' -> '/h/k'"
            ),
            "EXDEV: cross-device link not permitted, rename '/h/.k.tmp.<pid>.<n>' -> '/h/k'"
        );
        assert_eq!(normalise_fs_error("unexpected token"), "unexpected token");
        let t = table();
        let (ka, ha) = event_key(
            "warn",
            "cloud config apply failed: EACCES: permission denied, open '/h/.settings.json.tmp.1.0'",
            "js",
            &t,
        );
        let (kb, _) = event_key(
            "warn",
            "cloud config apply failed: EACCES: permission denied, open '/h/.settings.json.tmp.88.0'",
            "rust",
            &t,
        );
        assert_eq!(ka, kb);
        assert_eq!(ha.as_deref(), Some("T8/fs-error-text"));
        // A different step or path is still a difference.
        let (kc, _) = event_key(
            "warn",
            "cloud config apply failed: EACCES: permission denied, open '/h/settings.json'",
            "rust",
            &t,
        );
        assert_ne!(ka, kc);
    }

    #[test]
    fn outage_seq_jump_reconnect_tail_and_gate_skip() {
        let t = table();
        let tel = |ts: i64, seq: i64| serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":seq,"metrics":{}});
        // A (JS) resumes 1 s after the cut; B (Rust, backoff) 8 s after and,
        // past the 1000-payload offline queue, dropped the oldest ticks.
        let run = |span_end: i64, extra: bool| {
            let mut play = PlayInfo {
                start_wall: 0,
                end_wall: span_end + 100_000,
                closed_spans: vec![(50_000, span_end)],
                ..Default::default()
            };
            play.skips = vec![("b".into(), 30_000)];
            let a = cap(
                "js",
                vec![
                    msg(29_000, "telemetry", tel(29_000, 29)),
                    msg(31_000, "telemetry", tel(31_000, 30)),
                    msg(49_000, "telemetry", tel(49_000, 31)),
                    msg(span_end + 1_000, "telemetry", tel(span_end + 1_000, 32)),
                    msg(span_end + 8_000, "telemetry", tel(span_end + 8_000, 33)),
                ]
                .into_iter()
                .chain(extra.then(|| msg(span_end + 6_000, "telemetry", tel(span_end + 6_000, 40))))
                .collect(),
            );
            let b = cap(
                "rust",
                vec![
                    msg(29_000, "telemetry", tel(29_000, 1)),
                    msg(30_010, "telemetry", tel(30_010, 2)),
                    msg(31_000, "telemetry", tel(31_000, 3)),
                    msg(49_000, "telemetry", tel(49_000, 4)),
                    msg(span_end + 8_000, "telemetry", tel(span_end + 8_000, 33)),
                ],
            );
            diff(&a, &b, &play, &[], &t)
        };
        let tol_of = |rep: &Report, p: &str| -> Vec<String> {
            rep.mismatches
                .iter()
                .filter(|m| m.path == p)
                .map(|m| m.tol.clone())
                .collect()
        };
        // A 20 s outage must flush every tick now (WP1 follow-up parity): the
        // seq jump is a FAIL; the reconnect tail and the gate skip are not.
        let short = run(70_000, false);
        assert_eq!(
            tol_of(&short, "seq (increment)"),
            vec![UNTOLERATED.to_string()]
        );
        let msgs = tol_of(&short, "(message)");
        assert!(msgs.contains(&"T8/tick-drift".to_string()), "{msgs:?}");
        assert!(
            msgs.contains(&"T8/reconnect-cadence".to_string()),
            "{msgs:?}"
        );
        assert!(!msgs.contains(&UNTOLERATED.to_string()), "{msgs:?}");
        // ONE racing tick per side and span: a second unpaired tick there is
        // not cadence (it hid a missing flush).
        let two = run(70_000, true);
        let msgs = tol_of(&two, "(message)");
        assert_eq!(
            msgs.iter().filter(|t| *t == UNTOLERATED).count(),
            1,
            "{msgs:?}"
        );
        // Past the queue bound (span ≥ 1000 s) the jump is the bound.
        let long = run(1_100_000, false);
        assert_eq!(
            tol_of(&long, "seq (increment)"),
            vec!["T8/offline-queue-bound".to_string()]
        );
        assert_eq!(long.untolerated(), 0, "{:?}", long.mismatches);
    }

    #[test]
    fn latch_detail_only_when_hint_text_is_one_sided() {
        let t = table();
        let rep_doc = |detail: Option<&str>| {
            let mut ts = serde_json::json!({"state": "applied"});
            if let Some(d) = detail {
                ts["detail"] = Value::from(d);
            }
            serde_json::json!({"v":1,"origin":"boot","status":{"timeSync": ts}})
        };
        let run = |da: Option<&str>, db: Option<&str>| {
            let a = cap("js", vec![msg(10_000, "config/reported", rep_doc(da))]);
            let b = cap("rust", vec![msg(10_000, "config/reported", rep_doc(db))]);
            let rep = diff(&a, &b, &PlayInfo::default(), &[], &t);
            rep.mismatches
                .iter()
                .filter(|m| m.path == "status.timeSync.detail")
                .map(|m| m.tol.clone())
                .collect::<Vec<_>>()
        };
        let latch =
            "in force, but the clock stepper stood down for this boot (NTP is synchronized)";
        assert_eq!(run(Some(latch), None), vec!["T6".to_string()]);
        // Any other detail is compared.
        assert_eq!(
            run(
                Some("the device is holding a different value (changed locally?)"),
                None
            ),
            vec![UNTOLERATED.to_string()]
        );
        // Rust carrying the latch text (and JS not) is a bug.
        assert_eq!(run(None, Some(latch)), vec![UNTOLERATED.to_string()]);
        // Only the exact TS texts: a made-up "in force, but …" fails.
        assert_eq!(
            run(Some("in force, but something else"), None),
            vec![UNTOLERATED.to_string()]
        );
        // recording: the prefix + `; `-joined known parts.
        let rec = "in force, but video is held by the GPS/LTE gate and starts when it releases; \
                   load shedding is active (bridge-only)";
        assert_eq!(run(Some(rec), None), vec!["T6".to_string()]);
        assert_eq!(
            run(Some("in force, but load shedding is active ()"), None),
            vec![UNTOLERATED.to_string()]
        );
    }

    #[test]
    fn report_count_around_a_cut_is_reconnect_cadence() {
        let t = table();
        let r = |rev: i64, origin: &str| serde_json::json!({"v":1,"origin":origin,"rev":rev});
        // JS reconnected sooner and saw the intermediate retained rev 1.
        let a = cap(
            "js",
            vec![
                msg(10_000, "config/reported", r(1, "cloud")),
                msg(14_000, "config/reported", r(2, "cloud")),
            ],
        );
        let b = cap("rust", vec![msg(15_000, "config/reported", r(2, "cloud"))]);
        let count_tol = |a: &Capture, play: &PlayInfo, revs: &[i64]| {
            let f = InputFacts {
                config_revs: revs.iter().copied().collect(),
                ..Default::default()
            };
            diff_with(a, &b, play, &f, &[], &t)
                .mismatches
                .into_iter()
                .filter(|m| m.path == "(count)")
                .map(|m| m.tol)
                .collect::<Vec<_>>()
        };
        let unt = vec![UNTOLERATED.to_string()];
        let cut = PlayInfo {
            closed_spans: vec![(5_000, 12_000)],
            ..Default::default()
        };
        // No cut, no debounce straddle: a real difference (no T5 by default).
        assert_eq!(count_tol(&a, &PlayInfo::default(), &[1, 2]), unt);
        assert_eq!(
            count_tol(&a, &cut, &[1, 2]),
            vec!["T8/reconnect-cadence".to_string()]
        );
        // The extra report's rev was never published by the tape.
        assert_eq!(count_tol(&a, &cut, &[2]), unt);
        // The extra report is not a `cloud` one (a wrong trigger).
        let a2 = cap(
            "js",
            vec![
                msg(10_000, "config/reported", r(2, "boot")),
                msg(14_000, "config/reported", r(2, "cloud")),
            ],
        );
        assert_eq!(count_tol(&a2, &cut, &[1, 2]), unt);
        // More extras than spans.
        let a3 = cap(
            "js",
            vec![
                msg(9_000, "config/reported", r(1, "cloud")),
                msg(10_000, "config/reported", r(1, "cloud")),
                msg(14_000, "config/reported", r(2, "cloud")),
            ],
        );
        assert_eq!(count_tol(&a3, &cut, &[1, 2]), unt);
    }

    #[test]
    fn d22_null_leaf_group_vs_omitted_group() {
        let t = table();
        let tel = |imu: Option<Value>| {
            let mut m =
                serde_json::json!({"v":1,"deviceId":"{id}","ts":10_000,"seq":1,"metrics":{}});
            if let Some(i) = imu {
                m["metrics"]["imu"] = i;
            }
            m
        };
        let run = |imu: Value| {
            let a = cap("js", vec![msg(10_000, "telemetry", tel(Some(imu)))]);
            let b = cap("rust", vec![msg(10_000, "telemetry", tel(None))]);
            let play = PlayInfo {
                start_wall: -100_000,
                end_wall: 100_000,
                ..Default::default()
            };
            diff(&a, &b, &play, &[], &t)
                .mismatches
                .into_iter()
                .filter(|m| m.path == "metrics.imu")
                .map(|m| m.tol)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            run(serde_json::json!({"quat_r":{"value":null}})),
            vec!["T7/D22".to_string()]
        );
        assert_eq!(
            run(serde_json::json!({"quat_r":{"value":1}})),
            vec![UNTOLERATED.to_string()]
        );
    }

    #[test]
    fn report_value_read_between_two_reconnect_reports() {
        let t = table();
        let r = |c: &str| {
            serde_json::json!({"v":1,"origin":"cloud","rev":1,
            "effective":{"sim":{"checked_at":c},"timeSync":{"x":c}}})
        };
        let a = cap("js", vec![msg(59_000, "config/reported", r("10"))]);
        let b = cap("rust", vec![msg(62_000, "config/reported", r("20"))]);
        let play = PlayInfo {
            closed_spans: vec![(37_000, 57_000)],
            ..Default::default()
        };
        let tol = |inputs: Vec<(i64, &'static str)>, path: &str| {
            let f = InputFacts {
                inputs,
                ..Default::default()
            };
            diff_with(&a, &b, &play, &f, &[], &t)
                .mismatches
                .into_iter()
                .filter(|m| m.path == path)
                .map(|m| m.tol)
                .collect::<Vec<_>>()
        };
        let sim = "effective.sim.checked_at";
        // The LTE file changed between the two reports: the sim path it feeds.
        assert_eq!(
            tol(vec![(61_900, "lte_file")], sim),
            vec!["T8/reconnect-cadence".to_string()]
        );
        // ... but not a path the LTE file does not feed.
        assert_eq!(
            tol(vec![(61_900, "lte_file")], "effective.timeSync.x"),
            vec![UNTOLERATED.to_string()]
        );
        // Another input kind between them, or none: a real difference.
        assert_eq!(
            tol(vec![(61_900, "config")], sim),
            vec![UNTOLERATED.to_string()]
        );
        assert_eq!(
            tol(vec![(70_000, "lte_file")], sim),
            vec![UNTOLERATED.to_string()]
        );
    }

    #[test]
    fn input_edge_js_lag_only_near_nmea_and_capped() {
        let t = table();
        let tel = |ts: i64, seq: i64, q: i64| {
            serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":seq,
                "metrics":{"gps":{"fix_quality":{"value":q}}}})
        };
        let side = |kind: &str, vals: &[i64], off: i64| {
            cap(
                kind,
                vals.iter()
                    .enumerate()
                    .map(|(k, v)| {
                        let ts = 20_000 + off + k as i64 * 1000;
                        msg(ts, "telemetry", tel(ts, k as i64 + 1, *v))
                    })
                    .collect(),
            )
        };
        let play = PlayInfo {
            start_wall: 0,
            end_wall: 100_000,
            ..Default::default()
        };
        let gps_at = |w: Vec<i64>| {
            let mut f = InputFacts::default();
            f.groups.insert("gps".into(), w);
            f
        };
        let run = |a: &Capture, b: &Capture, f: &InputFacts| {
            diff_with(a, b, &play, f, &[], &t)
                .mismatches
                .into_iter()
                .filter(|m| m.path == "metrics.gps.fix_quality.value")
                .map(|m| m.tol)
                .collect::<Vec<_>>()
        };
        let near = gps_at(vec![20_700]);
        let edge = vec!["T8/input-edge".to_string()];
        let unt = vec![UNTOLERATED.to_string()];
        // JS (A) one tick late: JS = Rust's previous, JS next = Rust's.
        let (js, rs) = (side("js", &[0, 0, 1], 0), side("rust", &[0, 1, 1], 10));
        assert_eq!(run(&js, &rs, &near), edge);
        // The same with JS as B.
        assert_eq!(
            run(
                &side("rust", &[0, 1, 1], 0),
                &side("js", &[0, 0, 1], 10),
                &near
            ),
            edge
        );
        // RUST one tick late: not covered.
        assert_eq!(
            run(
                &side("js", &[0, 1, 1], 0),
                &side("rust", &[0, 0, 1], 10),
                &near
            ),
            unt
        );
        // Far from an NMEA transition, or next to a non-NMEA one: not covered.
        assert_eq!(run(&js, &rs, &InputFacts::default()), unt);
        let mut lte = InputFacts::default();
        lte.groups.insert("lte".into(), vec![20_700]);
        assert_eq!(run(&js, &rs, &lte), unt);
        // A GSV in-view change counts as an NMEA transition.
        let gsv = InputFacts {
            gsv_changes: vec![21_400],
            ..Default::default()
        };
        assert_eq!(run(&js, &rs, &gsv), edge);
        // A value neither neighbour had: a real difference.
        assert_eq!(run(&js, &side("rust", &[0, 4, 1], 10), &near), unt);
        // More than 2 ticks in a (≤ 10 min) run: every hit is UNTOLERATED.
        let js3 = side("js", &[0, 0, 1, 1, 2, 2, 3], 0);
        let rs3 = side("rust", &[0, 1, 1, 2, 2, 3, 3], 10);
        let many = gps_at(vec![21_000, 23_000, 25_000]);
        let got = run(&js3, &rs3, &many);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got.iter().all(|t| t == UNTOLERATED), "{got:?}");
    }

    fn nmea(t: u64, line: &str) -> TapeRecord {
        TapeRecord {
            t,
            w: t as i64,
            ev: TapeEvent::Nmea { line: line.into() },
        }
    }

    #[test]
    fn d23_facts_from_the_tape_first_chunk_after_a_reopen() {
        let play = PlayInfo {
            start_wall: 1_000_000,
            end_wall: 2_000_000,
            nmea_resumes: vec![1_050_120],
            ..Default::default()
        };
        let recs = vec![
            nmea(
                10_000,
                "$GPGGA,000000,4807.038,N,01131.000,E,1,08,0.9,545.4,M,,,,*47",
            ),
            // 40 s silence, then GGA (q0) + GSV in ONE chunk, RMC 8 ms later.
            nmea(50_000, "$GPGGA,000040,,,,,0,00,,,M,,,,*66"),
            nmea(50_001, "$GPGSV,1,1,01,01,40,083,46*4B"),
            nmea(50_008, "$GPRMC,000040,V,,,,,,,010125,,,N*48"),
            nmea(51_000, "$GPGGA,000041,,,,,0,00,,,M,,,,*67"),
        ];
        let play = PlayInfo {
            releases: released(&play, &recs, &[(50_000, 1_050_120)]),
            ..play
        };
        let f = InputFacts::from_records(&recs, &play);
        assert_eq!(f.reopens.len(), 1, "{f:?}");
        let r = &f.reopens[0];
        assert_eq!(r.w, 1_050_120, "the resumed line's actual release");
        assert_eq!(r.sentences, vec!["GGA".to_string(), "GSV".to_string()]);
        assert!(r.clears_position);
        let fields = d23_fields(r);
        for k in [
            "fix_quality",
            "satellites",
            "alt",
            "satellites_in_view",
            "hdop",
        ] {
            assert!(fields.contains(k), "{k}");
        }
        // A shorter gap is not a reopen.
        let short = vec![nmea(1_000, "$GPGGA,1"), nmea(20_000, "$GPGGA,2")];
        let play = PlayInfo {
            releases: released(&play, &short, &[]),
            ..play
        };
        assert!(InputFacts::from_records(&short, &play).reopens.is_empty());
    }

    #[test]
    fn d23_only_js_first_tick_after_reopen_and_only_the_lost_fields() {
        let t = table();
        let gps = |q: i64, sats: Option<i64>, alt: Option<f64>| {
            let mut g = serde_json::json!({"fix_quality":{"value":q}});
            if let Some(s) = sats {
                g["satellites"] = serde_json::json!({"value":s});
            }
            if let Some(a) = alt {
                g["alt"] = serde_json::json!({"value":a,"unit":"m"});
            }
            g
        };
        let tel = |ts: i64, seq: i64, g: Value| serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":seq,"metrics":{"gps":g}});
        // JS lost the GGA: fix_quality 0, no satellites/alt on its first tick.
        let js = cap(
            "js",
            vec![
                msg(
                    50_000,
                    "telemetry",
                    tel(50_000, 1, gps(1, Some(8), Some(545.4))),
                ),
                msg(51_000, "telemetry", tel(51_000, 2, gps(0, None, None))),
                msg(
                    52_000,
                    "telemetry",
                    tel(52_000, 3, gps(1, Some(8), Some(545.5))),
                ),
            ],
        );
        let rust = |second: Value| {
            cap(
                "rust",
                vec![
                    msg(
                        50_010,
                        "telemetry",
                        tel(50_010, 1, gps(1, Some(8), Some(545.4))),
                    ),
                    msg(51_010, "telemetry", tel(51_010, 2, second)),
                    msg(
                        52_010,
                        "telemetry",
                        tel(52_010, 3, gps(1, Some(8), Some(545.5))),
                    ),
                ],
            )
        };
        let play = PlayInfo {
            start_wall: 0,
            end_wall: 100_000,
            // An input transition next to the tick: T4 must NOT cover the
            // gps fields of the D23 tick.
            transitions: vec![50_700],
            ..Default::default()
        };
        let gga = InputFacts {
            reopens: vec![NmeaReopen {
                w: 50_700,
                sentences: vec!["GGA".into()],
                clears_position: false,
            }],
            ..Default::default()
        };
        let tols = |a: &Capture, b: &Capture, facts: &InputFacts| {
            diff_with(a, b, &play, facts, &[], &t)
                .mismatches
                .into_iter()
                .filter(|m| m.path.starts_with("metrics.gps"))
                .map(|m| (m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let b = rust(gps(1, Some(8), Some(545.5)));
        let got = tols(&js, &b, &gga);
        assert!(!got.is_empty());
        assert!(got.iter().all(|(_, t)| t == "T7/D23"), "{got:?}");
        // Without the reopen fact: the value difference is UNTOLERATED.
        let got = tols(&js, &b, &InputFacts::default());
        assert!(got.iter().any(|(_, t)| t == UNTOLERATED), "{got:?}");
        // The lost chunk was an RMC: fix_quality / satellites are NOT its
        // fields → UNTOLERATED (and T4 does not step in).
        let rmc = InputFacts {
            reopens: vec![NmeaReopen {
                w: 50_700,
                sentences: vec!["RMC".into()],
                clears_position: false,
            }],
            ..Default::default()
        };
        let got = tols(&js, &b, &rmc);
        assert!(
            got.iter()
                .any(|(p, t)| p == "metrics.gps.fix_quality.value" && t == UNTOLERATED),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|(p, t)| p == "metrics.gps.satellites" && t == UNTOLERATED),
            "{got:?}"
        );
        // Rust-vs-Rust (no JS side): D23 never applies.
        let mut r2 = js.clone();
        r2.meta.kind = "rust".into();
        let got = tols(&r2, &b, &gga);
        assert!(got.iter().all(|(_, t)| t != "T7/D23"), "{got:?}");
        // Only the FIRST JS tick after the resume: a difference on the
        // second tick stays UNTOLERATED.
        let late = InputFacts {
            reopens: vec![NmeaReopen {
                w: 49_000,
                sentences: vec!["GGA".into()],
                clears_position: false,
            }],
            ..Default::default()
        };
        let got = tols(&js, &b, &late);
        assert!(got.iter().any(|(_, t)| t == UNTOLERATED), "{got:?}");
    }

    #[test]
    fn sky_hold_at_write() {
        let t = table();
        // gnss-sky.json: same state, hold_s apart by the write distance.
        let mut ca = cap("js", vec![]);
        let mut cb = cap("rust", vec![]);
        let sky = |ts: &str, hold: i64| {
            format!(
                r#"{{"v":1,"timestamp":"{ts}","state":"clear","heard":3,"usable":1,"in_view":8,"max_snr":30,"hold_s":{hold}}}"#
            )
        };
        ca.finals.insert(
            "gnss-sky.json".into(),
            Some(sky("2026-10-10T02:41:56.492Z", 183)),
        );
        cb.finals.insert(
            "gnss-sky.json".into(),
            Some(sky("2026-10-10T02:41:55.840Z", 181)),
        );
        let tols: Vec<_> = diff(&ca, &cb, &PlayInfo::default(), &[], &t)
            .mismatches
            .into_iter()
            .filter(|m| m.stream == "file:gnss-sky.json")
            .map(|m| (m.path, m.tol))
            .collect();
        assert!(
            tols.contains(&("hold_s".to_string(), "T8/sky-file-end".to_string())),
            "{tols:?}"
        );
        assert!(!tols.iter().any(|(_, t)| t == UNTOLERATED), "{tols:?}");
        cb.finals.insert(
            "gnss-sky.json".into(),
            Some(sky("2026-10-10T02:41:56.400Z", 170)),
        );
        let rep = diff(&ca, &cb, &PlayInfo::default(), &[], &t);
        assert!(rep.untolerated() > 0);
    }

    #[test]
    fn ram_beyond_t3_is_host_only_where_the_host_moved() {
        let t = table();
        let ram = |ts: i64, seq: i64, v: f64| {
            serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":seq,
                "metrics":{"system":{"ram_usage":{"value":v,"unit":"%"}}}})
        };
        let host = |w: i64, v: f64| (w, BTreeMap::from([("ram".to_string(), v)]));
        let play = PlayInfo {
            start_wall: 0,
            end_wall: 100_000,
            host: vec![
                host(10_000, 20.0),
                host(10_100, 23.0),
                host(10_200, 25.5),
                host(20_000, 20.0),
                host(20_100, 20.1),
                host(20_200, 20.0),
            ],
            ..Default::default()
        };
        let a = cap(
            "js",
            vec![
                msg(10_000, "telemetry", ram(10_000, 1, 20.0)),
                msg(20_000, "telemetry", ram(20_000, 2, 20.0)),
            ],
        );
        let b = cap(
            "rust",
            vec![
                msg(10_200, "telemetry", ram(10_200, 1, 25.0)),
                msg(20_200, "telemetry", ram(20_200, 2, 25.0)),
            ],
        );
        let rep = diff(&a, &b, &play, &[], &t);
        let got: Vec<(&str, &str)> = rep
            .mismatches
            .iter()
            .filter(|m| m.path == "metrics.system.ram_usage.value")
            .map(|m| (m.at.as_str(), m.tol.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("ts A 10000 / B 10200", "T3/host"),
                ("ts A 20000 / B 20200", UNTOLERATED),
            ],
            "{:?}",
            rep.mismatches
        );
    }

    #[test]
    fn should_narrowings_host_sign_build_o2_end_snapshot() {
        let t = table();
        // T3/host: the host's own difference must explain A − B WITH its sign.
        let ram = |ts: i64, v: f64| {
            serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":1,
                "metrics":{"system":{"ram_usage":{"value":v,"unit":"%"}}}})
        };
        let host = |w: i64, v: f64| (w, BTreeMap::from([("ram".to_string(), v)]));
        let play = PlayInfo {
            start_wall: 0,
            end_wall: 100_000,
            host: vec![host(10_000, 20.0), host(10_200, 25.5)],
            ..Default::default()
        };
        let tol_ram = |bv: f64| {
            let a = cap("js", vec![msg(10_000, "telemetry", ram(10_000, 20.0))]);
            let b = cap("rust", vec![msg(10_200, "telemetry", ram(10_200, bv))]);
            diff(&a, &b, &play, &[], &t)
                .mismatches
                .into_iter()
                .find(|m| m.path == "metrics.system.ram_usage.value")
                .map(|m| m.tol)
        };
        assert_eq!(tol_ram(25.0).as_deref(), Some("T3/host"));
        // The host rose 5.5 points, Rust reports 5 points LOWER: a formula bug.
        assert_eq!(tol_ram(15.0).as_deref(), Some(UNTOLERATED));

        // build: the four leaf values are each agent's own; the key set is not.
        let rep_b = |build: Value| serde_json::json!({"v":1,"origin":"boot","rev":1,"effective":{"agent":{"build":build}}});
        let build_tols = |bb: Value| {
            let a = cap(
                "js",
                vec![msg(
                    1_000,
                    "config/reported",
                    rep_b(
                        serde_json::json!({"version":"0.2.0","channel":"local","builtAt":null,"packageVersion":"0.2.0"}),
                    ),
                )],
            );
            let b = cap("rust", vec![msg(1_000, "config/reported", rep_b(bb))]);
            diff(&a, &b, &PlayInfo::default(), &[], &t)
                .mismatches
                .into_iter()
                .filter(|m| m.path.starts_with("effective.agent.build"))
                .map(|m| (m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let got = build_tols(
            serde_json::json!({"version":"unknown","channel":"dev-local","builtAt":null,"packageVersion":"0.0.1"}),
        );
        assert!(
            got.iter().all(|(_, t)| t == "T8/build") && got.len() == 3,
            "{got:?}"
        );
        let got = build_tols(
            serde_json::json!({"version":"unknown","channel":"dev-local","builtAt":null}),
        );
        assert!(
            got.iter()
                .any(|(p, t)| p == "effective.agent.build.packageVersion" && t == UNTOLERATED),
            "{got:?}"
        );

        // O2: only the mono-derived observables near a wall step.
        let step = PlayInfo {
            start_wall: 0,
            end_wall: 100_000,
            wall_steps: vec![(50_000, 5_000)],
            ..Default::default()
        };
        let lte = |ts: i64, v: i64| serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":1,"metrics":{"lte":{"rssi":{"value":v}}}});
        let a = cap(
            "js",
            vec![
                msg(51_000, "telemetry", lte(51_000, 1)),
                msg(
                    51_000,
                    "config/reported",
                    serde_json::json!({"rev":1,"x":1}),
                ),
            ],
        );
        let b = cap(
            "rust",
            vec![
                msg(51_010, "telemetry", lte(51_010, 2)),
                msg(
                    51_010,
                    "config/reported",
                    serde_json::json!({"rev":1,"x":2}),
                ),
            ],
        );
        let rep = diff(&a, &b, &step, &[], &t);
        let tol = |p: &str| {
            rep.mismatches
                .iter()
                .find(|m| m.path == p)
                .map(|m| m.tol.clone())
        };
        assert_eq!(tol("metrics.lte.rssi.value").as_deref(), Some("T8/O2"));
        assert_eq!(tol("x").as_deref(), Some(UNTOLERATED));

        // end-snapshot: `latest` = own last telemetry; values may differ
        // across agents, the structure may not.
        let tel_l = |ts: i64, v: i64, gps: bool| {
            let mut m = lte(ts, v);
            if gps {
                m["metrics"]["gps"] = serde_json::json!({"fix_quality":{"value":1}});
            }
            m
        };
        let state = |latest: &Value| Some(format!(r#"{{"v":1,"latest":{latest}}}"#));
        let run = |la: Value, lb: Value, lb_own: Value| {
            let mut a = cap("js", vec![msg(90_000, "telemetry", la.clone())]);
            let mut b = cap("rust", vec![msg(90_010, "telemetry", lb_own)]);
            a.finals.insert("state.json".into(), state(&la));
            b.finals.insert("state.json".into(), state(&lb));
            diff(&a, &b, &PlayInfo::default(), &[], &t)
                .mismatches
                .into_iter()
                .filter(|m| m.at.contains("latest") || m.path.starts_with("latest"))
                .map(|m| (m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let got = run(
            tel_l(90_000, 1, true),
            tel_l(90_010, 2, true),
            tel_l(90_010, 2, true),
        );
        assert_eq!(
            got,
            vec![("latest (values)".to_string(), "T8/end-snapshot".to_string())]
        );
        let got = run(
            tel_l(90_000, 1, true),
            tel_l(90_010, 2, true),
            tel_l(90_010, 3, true),
        );
        assert!(
            got.iter()
                .any(|(p, t)| p.starts_with("latest (≠") && t == UNTOLERATED),
            "{got:?}"
        );
        let got = run(
            tel_l(90_000, 1, true),
            tel_l(90_010, 2, false),
            tel_l(90_010, 2, false),
        );
        assert!(
            got.iter()
                .any(|(p, t)| p == "metrics.gps" && t == UNTOLERATED),
            "{got:?}"
        );
    }

    #[test]
    fn t4_windows_are_per_group_from_the_tape() {
        let rec = |t: u64, ev: TapeEvent| TapeRecord { t, w: t as i64, ev };
        let nmea = |t: u64, l: &str| rec(t, TapeEvent::Nmea { line: l.into() });
        let play = PlayInfo {
            start_wall: 100_000,
            end_wall: 200_000,
            // The lte change was released 120 ms late (releases.ndjson).
            ..Default::default()
        };
        let recs = vec![
            // Prefilled (t < 1000): not a transition.
            rec(500, TapeEvent::lte_file(Some(b"a"))),
            nmea(10_000, "$GPGGA,1,,,,,0,00,,,M"),
            nmea(11_000, "$GPGGA,2,,,,,0,00,,,M"),
            nmea(12_000, "$GPGGA,3,,,,,1,08,,,M"),
            rec(50_000, TapeEvent::lte_file(Some(b"b"))),
            rec(
                60_000,
                TapeEvent::Exec {
                    argv: vec!["vcgencmd".into()],
                    stdout: "x".into(),
                    code: Some(0),
                },
            ),
            rec(
                61_000,
                TapeEvent::MqttIn {
                    topic: "t/config".into(),
                    payload: Some("{}".into()),
                    payload_b64: None,
                    retain: true,
                    qos: 1,
                },
            ),
            rec(
                62_000,
                TapeEvent::Cli {
                    argv: vec!["status".into()],
                },
            ),
            rec(63_000, TapeEvent::ImuSpawn { argv: vec![] }),
            // NMEA resumes after 20 s: stop, stale, resume.
            nmea(32_000, "$GPGGA,4,,,,,1,08,,,M"),
        ];
        let mut recs = recs;
        recs.sort_by_key(|r| r.t);
        let play = PlayInfo {
            releases: released(&play, &recs, &[(50_000, 150_120)]),
            ..play
        };
        let f = InputFacts::from_records(&recs, &play);
        assert_eq!(
            f.groups["lte"],
            vec![150_120],
            "actual release, prefill skipped"
        );
        assert_eq!(f.groups["system"], vec![160_000]);
        assert_eq!(f.groups["imu"], vec![163_000]);
        // first line, quality q0, q1, stop 112000, stale 122000, resume 132000, end stale 142000
        assert_eq!(
            f.groups["gps"],
            vec![110_000, 112_000, 122_000, 132_000, 142_000]
        );
        // mqtt_in / cli add nothing anywhere.
        assert!(!f
            .groups
            .values()
            .flatten()
            .any(|w| *w == 161_000 || *w == 162_000));
        let cov = f.coverage(&play, 5_000, 1_500);
        assert!(
            (cov["lte"] - 3_000.0 * 100.0 / 95_000.0).abs() < 1e-6,
            "{cov:?}"
        );

        // A presence difference in the LTE group is tolerated only near an
        // lte transition — not near a gps one, and never for another group.
        let t = table();
        let tel = |ts: i64, seq: i64, lte: bool| {
            let mut m = serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":seq,"metrics":{"gps":{"fix_quality":{"value":1}}}});
            if lte {
                m["metrics"]["lte"] = serde_json::json!({"rssi":{"value":-70}});
            }
            m
        };
        let a = cap("js", vec![msg(150_000, "telemetry", tel(150_000, 1, true))]);
        let b = cap(
            "rust",
            vec![msg(150_010, "telemetry", tel(150_010, 1, false))],
        );
        let play2 = PlayInfo {
            start_wall: 0,
            end_wall: 300_000,
            ..Default::default()
        };
        let tol = |facts: &InputFacts| {
            diff_with(&a, &b, &play2, facts, &[], &t)
                .mismatches
                .into_iter()
                .find(|m| m.path == "metrics.lte")
                .map(|m| m.tol)
        };
        let mut g = InputFacts::default();
        g.groups.insert("gps".into(), vec![150_000]);
        assert_eq!(tol(&g).as_deref(), Some(UNTOLERATED));
        g.groups.insert("lte".into(), vec![149_000]);
        assert_eq!(tol(&g).as_deref(), Some("T4/lte"));
        // The summary carries the coverage per group.
        let rep = diff_with(&a, &b, &play2, &g, &[], &t);
        assert!(
            rep.inputs.contains_key("T4 window coverage lte"),
            "{:?}",
            rep.inputs
        );
        assert!(rep.inputs["tape"].starts_with("NOT READ"));
    }

    /// A tick `k` carrying lte rssi `k` (+ optional imu ax / extra lte field).
    fn cut_tel(ts: i64, k: i64, imu: f64, extra: bool) -> Value {
        let mut m = serde_json::json!({"v":1,"deviceId":"{id}","ts":ts,"seq":k,
            "metrics":{"lte":{"rssi":{"value":k}},"imu":{"ax":{"value":imu}}}});
        if extra {
            m["metrics"]["lte"]["quality"] = serde_json::json!({"value":1});
        }
        m
    }

    /// A ticks at k s (value k); B on player pairs at k s + 300 ms, but inside
    /// the cut its ticks sit 650 ms EARLY, so the ts fallback pairs A_k with
    /// B's tick k+1 — the review's A_k = B_{k+1} rows.
    fn cut_run(tweak: impl Fn(i64, &mut Value)) -> (Capture, Capture, Vec<(i64, i64)>) {
        let mut am = Vec::new();
        let mut bm = Vec::new();
        let mut pairs = Vec::new();
        for k in 40..=70 {
            am.push(msg(k * 1000, "telemetry", cut_tel(k * 1000, k, 0.0, false)));
            let tb = if (50..=60).contains(&k) {
                k * 1000 - 650
            } else {
                k * 1000 + 300
            };
            let mut p = cut_tel(tb, k, 0.0, false);
            tweak(k, &mut p);
            bm.push(msg(tb, "telemetry", p));
            if !(50..=60).contains(&k) {
                pairs.push((k * 1000, tb));
            }
        }
        (cap("js", am), cap("rust", bm), pairs)
    }

    #[test]
    fn rig_cut_repairs_fallback_pairs_then_compares_strictly() {
        let t = table();
        let play = PlayInfo {
            start_wall: 0,
            end_wall: 100_000,
            closed_spans: vec![(50_000, 60_000)],
            ..Default::default()
        };
        let unt = |a: &Capture, b: &Capture, pairs: &[(i64, i64)], f: &InputFacts| {
            let rep = diff_with(a, b, &play, f, pairs, &t);
            let u: Vec<(String, String, String)> = rep
                .mismatches
                .iter()
                .filter(|m| m.tol == UNTOLERATED)
                .map(|m| (m.at.clone(), m.path.clone(), m.tol.clone()))
                .collect();
            (u, rep)
        };
        // Pure alignment: after the re-pair nothing differs.
        let (a, b, pairs) = cut_run(|_, _| {});
        let (u, rep) = unt(&a, &b, &pairs, &InputFacts::default());
        assert!(u.is_empty(), "{u:?}");
        assert!(
            rep.inputs
                .get("rig-cut re-pair 0")
                .is_some_and(|v| v.contains(": 10 of 16 ")),
            "{:?}",
            rep.inputs
        );
        assert!(!rep
            .mismatches
            .iter()
            .any(|m| m.path.starts_with("metrics.lte")));
        // A real lte value difference inside the cut: UNTOLERATED (no
        // blanket value tolerance any more).
        let (a, b, pairs) = cut_run(|k, p| {
            if k == 55 {
                p["metrics"]["lte"]["rssi"]["value"] = serde_json::json!(-1);
            }
        });
        let (u, _) = unt(&a, &b, &pairs, &InputFacts::default());
        assert!(
            u.iter().any(|(_, p, _)| p == "metrics.lte.rssi.value"),
            "{u:?}"
        );
        // A presence difference inside the cut: UNTOLERATED.
        let (a, b, pairs) = cut_run(|k, p| {
            if k == 55 {
                p["metrics"]["lte"]["quality"] = serde_json::json!({"value":1});
            }
        });
        let (u, _) = unt(&a, &b, &pairs, &InputFacts::default());
        assert!(
            u.iter().any(|(_, p, _)| p == "metrics.lte.quality"),
            "{u:?}"
        );
        // An IMU value inside the cut: tolerated only when the tape released
        // an IMU line between the two ticks.
        let (a, b, pairs) = cut_run(|k, p| {
            if k == 55 {
                p["metrics"]["imu"]["ax"]["value"] = serde_json::json!(0.5);
            }
        });
        let (u, _) = unt(&a, &b, &pairs, &InputFacts::default());
        assert!(
            u.iter().any(|(_, p, _)| p == "metrics.imu.ax.value"),
            "{u:?}"
        );
        let f = InputFacts {
            imu_lines: vec![54_600],
            ..Default::default()
        };
        let (u, rep) = unt(&a, &b, &pairs, &f);
        assert!(u.is_empty(), "{u:?}");
        assert!(rep
            .mismatches
            .iter()
            .any(|m| m.path == "metrics.imu.ax.value" && m.tol == "T8/rig-cut-ungated"));
        // An lte value inside the cut: only with an LTE-file change released
        // between the two reads (A 55 000, B 54 350 after the re-pair), and
        // only a field that change moved: B read the OLD version (= A's
        // previous sample, 54).
        let (a, b, pairs) = cut_run(|k, p| {
            if k == 55 {
                p["metrics"]["lte"]["rssi"]["value"] = serde_json::json!(54);
            }
        });
        let at = |w: i64| InputFacts {
            inputs: vec![(w, "lte_file")],
            ..Default::default()
        };
        let (u, rep) = unt(&a, &b, &pairs, &at(54_700));
        assert!(u.is_empty(), "{u:?}");
        assert!(rep
            .mismatches
            .iter()
            .any(|m| m.path == "metrics.lte.rssi.value" && m.tol == "T8/rig-cut-ungated"));
        let (u, _) = unt(&a, &b, &pairs, &at(56_200));
        assert!(
            u.iter().any(|(_, p, _)| p == "metrics.lte.rssi.value"),
            "{u:?}"
        );
        // A value neither version carried (no adjacent sample has it): the
        // change did not produce it, so UNTOLERATED even with the release.
        let (a, b, pairs) = cut_run(|k, p| {
            if k == 55 {
                p["metrics"]["lte"]["rssi"]["value"] = serde_json::json!(-1);
            }
        });
        let (u, _) = unt(&a, &b, &pairs, &at(54_700));
        assert!(
            u.iter().any(|(_, p, _)| p == "metrics.lte.rssi.value"),
            "{u:?}"
        );
    }

    fn final_dir(tag: &str, kind: &str, files: &[(&str, Option<&str>)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xr-{}-{tag}-{kind}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("meta.json"),
            format!(r#"{{"label":"{kind}","kind":"{kind}","deviceId":"dev-{kind}"}}"#),
        )
        .unwrap();
        let mut m = serde_json::Map::new();
        for (k, v) in files {
            m.insert(
                k.to_string(),
                match v {
                    Some(t) => serde_json::json!({"present":true,"len":t.len(),
                        "b64":xorgate_hal::tape::b64_encode(t.as_bytes()),"text":t}),
                    None => serde_json::json!({"present":false}),
                },
            );
        }
        std::fs::write(
            d.join("final.json"),
            serde_json::to_vec(&serde_json::json!({"w":1,"files":m})).unwrap(),
        )
        .unwrap();
        d
    }

    #[test]
    fn files_from_final_json_presence_layout_and_flag() {
        let t = table();
        let pretty = "{\n  \"channels\": [],\n  \"uploadBucket\": \"b\"\n}\n";
        let compact = "{\"channels\":[],\"uploadBucket\":\"b\"}";
        let a = Capture::load(&final_dir(
            "f",
            "js",
            &[
                ("etc/kvs.json", Some(pretty)),
                ("etc/hardware.json", Some(pretty)),
                ("config-sync.json", Some("{\n  \"savedAt\": 1\n}\n")),
                ("refresh-modem.flag", Some("1791599514639")),
                ("etc/pipelines/cam0.gst", None),
                ("commands.json", Some("{\"a\":1}")),
            ],
        ))
        .unwrap();
        let b = Capture::load(&final_dir(
            "f",
            "rust",
            &[
                ("etc/kvs.json", None),
                ("etc/hardware.json", Some(compact)),
                ("config-sync.json", Some("{\n  \"savedAt\": 2\n}\n")),
                ("refresh-modem.flag", Some("1791599514001")),
                ("etc/pipelines/cam0.gst", None),
                ("commands.json", Some("{\"a\":1}")),
            ],
        ))
        .unwrap();
        let rep = diff_with(
            &a,
            &b,
            &PlayInfo::default(),
            &InputFacts::default(),
            &[],
            &t,
        );
        let got = |stream: &str, path: &str| {
            rep.mismatches
                .iter()
                .find(|m| m.stream == stream && m.path == path)
                .map(|m| m.tol.clone())
        };
        // One agent wrote kvs.json, the other did not: FAIL, not skipped.
        assert_eq!(
            got("file:etc/kvs.json", "(file)").as_deref(),
            Some(UNTOLERATED)
        );
        // Same JSON, different layout: FAIL.
        assert_eq!(
            got("file:etc/hardware.json", "(layout)").as_deref(),
            Some(UNTOLERATED)
        );
        // A T1 value with the same layout: no layout row.
        assert_eq!(
            got("file:config-sync.json", "savedAt").as_deref(),
            Some("T1")
        );
        assert_eq!(got("file:config-sync.json", "(layout)"), None);
        assert_eq!(
            got("file:refresh-modem.flag", "(content)").as_deref(),
            Some("T1")
        );
        assert_eq!(rep.stats["file:etc/pipelines/cam0.gst"].absent_both, 1);
        assert_eq!(rep.stats["file:etc/pipelines/cam0.gst"].compared, 0);
        assert_eq!(rep.stats["file:commands.json"].compared, 1);
        // A flag that is not bare decimal ms: FAIL.
        let b2 = Capture::load(&final_dir(
            "g",
            "rust",
            &[("refresh-modem.flag", Some("1791599514001\n"))],
        ))
        .unwrap();
        let rep = diff_with(
            &a,
            &b2,
            &PlayInfo::default(),
            &InputFacts::default(),
            &[],
            &t,
        );
        assert!(rep
            .mismatches
            .iter()
            .any(|m| m.stream == "file:refresh-modem.flag" && m.tol == UNTOLERATED));
        assert_eq!(json_skeleton(pretty), "{\n  s: [],\n  s: s\n}\n");
    }

    #[test]
    fn events_count_each_occurrence_by_ts() {
        let ev = |ts: i64, m: &str| serde_json::json!({"ts":ts,"level":"warn","msg":m});
        let st = serde_json::json!({"v":1,"recentEvents":[ev(1,"reconnecting…"), ev(2,"reconnecting…")]});
        let d = final_dir("ev", "js", &[("state.json", Some(&st.to_string()))]);
        // A snapshot repeating the same ring does not double count.
        std::fs::write(
            d.join("files.ndjson"),
            format!(
                "{}\n",
                serde_json::json!({"w":1,"name":"state.json","text":st.to_string()})
            ),
        )
        .unwrap();
        let c = Capture::load(&d).unwrap();
        assert_eq!(c.events.len(), 2, "{:?}", c.events);
    }

    #[test]
    fn mqtt_qos_and_retain_per_publish_and_contract() {
        let t = table();
        let with = |mut m: Msg, q: u8, r: bool| {
            m.qos = Some(q);
            m.retain = r;
            m
        };
        let a = cap(
            "js",
            vec![
                with(
                    msg(10_000, "telemetry", tel(10_000, 1, 1.0, None)),
                    0,
                    false,
                ),
                with(
                    msg(20_000, "config/reported", serde_json::json!({"rev":1})),
                    1,
                    false,
                ),
            ],
        );
        let b = cap(
            "rust",
            vec![
                with(
                    msg(10_010, "telemetry", tel(10_010, 1, 1.0, None)),
                    1,
                    false,
                ),
                with(
                    msg(20_010, "config/reported", serde_json::json!({"rev":1})),
                    1,
                    true,
                ),
            ],
        );
        let rep = diff_with(
            &a,
            &b,
            &PlayInfo::default(),
            &InputFacts::default(),
            &[],
            &t,
        );
        let has = |stream: &str, path: &str| {
            rep.mismatches
                .iter()
                .any(|m| m.stream == stream && m.path == path && m.tol == UNTOLERATED)
        };
        assert!(has("telemetry", "(qos)"));
        assert!(has("telemetry", "(qos contract)"));
        assert!(has("report", "(retain)"));
        assert!(has("report", "(retain contract)"));
        assert!(!has("report", "(qos)"));
        // 4 publishes against the contract + 2 pairs.
        assert_eq!(rep.stats["mqtt-flags"].compared, 6);
    }

    #[test]
    fn o4_and_sms_duration_rows_judge_only_their_fields() {
        let t = table();
        let sms = |status: &str, error: &str, dur: Value, total: i64| {
            serde_json::json!({"v":1,"deviceId":"{id}","requestId":"r1","ts":5_000,
                "status":status,"error":error,"durationMs":dur,"storage":{},
                "total":total,"skipped":0,"partial":false,"messages":[]})
        };
        let line = |status: &str, dur: &str, error: &str| {
            format!("read_sms r1: status={status} total=0 sent=0 unread=0 skipped=0 partial=false durationMs={dur} error={error}")
        };
        let js = |dur: i64, total: i64| {
            let mut c = cap(
                "js",
                vec![msg(
                    5_000,
                    "sms",
                    sms("no_modem", "no SIMCom interface 05 tty", dur.into(), total),
                )],
            );
            c.events = vec![
                (
                    5_000,
                    "warn".into(),
                    line("no_modem", &dur.to_string(), "no SIMCom interface 05 tty"),
                ),
                (0, "info".into(), "connected to broker".into()),
            ];
            c
        };
        let rs = |dur: Value| {
            let mut c = cap(
                "rust",
                vec![msg(
                    5_001,
                    "sms",
                    sms(
                        "error",
                        "sms reader not available in this build",
                        dur.clone(),
                        0,
                    ),
                )],
            );
            c.events = vec![
                (
                    5_001,
                    "warn".into(),
                    line(
                        "error",
                        &dur.to_string(),
                        "sms reader not available in this build",
                    ),
                ),
                (0, "info".into(), "connected to broker".into()),
            ];
            c
        };
        let real = InputFacts {
            mode: Some("real".into()),
            ..Default::default()
        };
        let run = |a: &Capture, b: &Capture, f: &InputFacts| {
            diff_with(a, b, &PlayInfo::default(), f, &[], &t)
        };
        let hits = |rep: &Report, id: &str| rep.mismatches.iter().filter(|m| m.tol == id).count();
        // Real mode, JS vs Rust, 1 ms vs 0 ms: O4 (2 sms leaves + the event
        // line) and sms-duration (sms + event line), nothing untolerated.
        let rep = run(&js(1, 0), &rs(0.into()), &real);
        assert_eq!(rep.untolerated(), 0, "{:?}", rep.mismatches);
        assert_eq!(hits(&rep, "T8/O4"), 3, "{:?}", rep.mismatches);
        assert_eq!(hits(&rep, "T8/sms-duration"), 2, "{:?}", rep.mismatches);
        assert_eq!(
            rep.stats["sms"].leaves,
            leaf_count(&js(1, 0).msgs[0].payload)
        );
        // A/B swapped: the same.
        assert_eq!(run(&rs(0.into()), &js(1, 0), &real).untolerated(), 0);
        // Another field of that result still counts.
        let rep = run(&js(0, 3), &rs(0.into()), &real);
        assert!(rep
            .mismatches
            .iter()
            .any(|m| m.path == "total" && m.tol == UNTOLERATED));
        // Not real mode (sim/replay answer from the fixture): O4 never applies.
        let sim = InputFacts {
            mode: Some("sim".into()),
            ..Default::default()
        };
        let rep = run(&js(0, 0), &rs(0.into()), &sim);
        assert_eq!(hits(&rep, "T8/O4"), 0);
        assert!(rep.untolerated() >= 3, "{:?}", rep.mismatches);
        // JS vs JS: O4 never applies, even with the Rust words on one side.
        let mut b = rs(0.into());
        b.meta.kind = "js".into();
        assert_eq!(hits(&run(&js(0, 0), &b, &real), "T8/O4"), 0);
        // Durations > 50 ms apart, negative, or non-integer: UNTOLERATED.
        let rep = run(&js(51, 0), &rs(0.into()), &real);
        let unt: Vec<_> = rep
            .mismatches
            .iter()
            .filter(|m| m.tol == UNTOLERATED)
            .map(|m| (m.stream.clone(), m.path.clone()))
            .collect();
        assert!(
            unt.contains(&("sms".into(), "durationMs".into())),
            "{unt:?}"
        );
        assert!(
            unt.contains(&("events".into(), "durationMs".into())),
            "{unt:?}"
        );
        assert!(run(&js(50, 0), &rs(0.into()), &real).untolerated() == 0);
        assert!(run(&js(1, 0), &rs((-1).into()), &real).untolerated() > 0);
        assert!(run(&js(1, 0), &rs(serde_json::json!(1.5)), &real).untolerated() > 0);
        // JS vs JS timing spread (2 vs 0): sms-duration only.
        let rep = run(&js(2, 0), &js(0, 0), &InputFacts::default());
        assert_eq!(rep.untolerated(), 0, "{:?}", rep.mismatches);
        assert_eq!(hits(&rep, "T8/sms-duration"), 2);
    }

    #[test]
    fn sim_mode_tape_leaves_telemetry_uncompared() {
        let t = table();
        let a = cap(
            "js",
            vec![msg(10_000, "telemetry", tel(10_000, 1, 1.0, Some(1.0)))],
        );
        let b = cap(
            "rust",
            vec![msg(10_010, "telemetry", tel(10_010, 1, 50.0, None))],
        );
        let facts = InputFacts {
            mode: Some("sim".into()),
            ..Default::default()
        };
        let rep = diff_with(&a, &b, &PlayInfo::default(), &facts, &[], &t);
        assert!(!rep.mismatches.iter().any(|m| m.stream == "telemetry"));
        assert_eq!(rep.stats["telemetry"].compared, 0);
        assert!(rep.inputs["telemetry"].starts_with("NOT COMPARED"));
        let rep = diff_with(
            &a,
            &b,
            &PlayInfo::default(),
            &InputFacts::default(),
            &[],
            &t,
        );
        assert!(rep.untolerated() > 0);
    }

    #[test]
    fn identical_runs_pass_and_tolerances_are_cited() {
        let t = table();
        let play = PlayInfo {
            start_wall: 0,
            end_wall: 100_000,
            transitions: vec![20_000],
            ..Default::default()
        };
        let a = cap(
            "js",
            vec![
                msg(10_000, "telemetry", tel(10_000, 5, 10.0, Some(14.5))),
                msg(11_000, "telemetry", tel(11_000, 6, 10.0, Some(14.5))),
                msg(20_000, "telemetry", tel(20_000, 15, 10.0, Some(14.5))),
                msg(30_000, "telemetry", tel(30_000, 25, 10.0, Some(14.5))),
            ],
        );
        let b = cap(
            "rust",
            vec![
                msg(10_200, "telemetry", tel(10_200, 105, 15.0, Some(14.5))),
                msg(11_200, "telemetry", tel(11_200, 106, 30.0, Some(14.5))),
                msg(20_200, "telemetry", tel(20_200, 115, 10.0, None)),
                msg(30_200, "telemetry", tel(30_200, 125, 10.0, Some(14.6))),
            ],
        );
        let mut facts = InputFacts::default();
        facts.groups.insert("gps".into(), vec![20_000]);
        let rep = diff_with(&a, &b, &play, &facts, &[], &t);
        let tols: Vec<(&str, &str)> = rep
            .mismatches
            .iter()
            .map(|m| (m.path.as_str(), m.tol.as_str()))
            .collect();
        assert!(tols.contains(&("ts", "T1")), "{tols:?}");
        assert!(
            tols.contains(&("metrics.system.cpu_usage.value", "T3")),
            "{tols:?}"
        );
        // 10 vs 30 is beyond ±10.
        assert!(
            tols.contains(&("metrics.system.cpu_usage.value", UNTOLERATED)),
            "{tols:?}"
        );
        // gps absent at a transition → T4; a value change far from one → FAIL.
        assert!(tols.contains(&("metrics.gps", "T4/gps")), "{tols:?}");
        assert!(
            tols.contains(&("metrics.gps.lat.value", UNTOLERATED)),
            "{tols:?}"
        );
        // seq deltas match (5→6→15→25 vs 105→106→115→125).
        assert!(!tols.iter().any(|(p, _)| *p == "seq"), "{tols:?}");
        // …but each agent's own counter jumps 6→15→25: T2 increments fail.
        assert!(tols.contains(&("seq (increment)", UNTOLERATED)), "{tols:?}");
        assert_eq!(rep.stats["telemetry"].compared, 4);
        let md = render(&rep, &a, &b, &play, &t, Path::new("/run"));
        assert!(md.contains("FAIL"));
    }

    #[test]
    fn numbers_compare_as_text_t9_and_key_order_matters() {
        let t = table();
        let play = PlayInfo::default();
        let a = cap(
            "js",
            vec![msg(
                1,
                "command/response",
                serde_json::json!({"requestId":"r","status":"accepted","n":1}),
            )],
        );
        let b = cap(
            "rust",
            vec![msg(
                1,
                "command/response",
                serde_json::from_str(r#"{"status":"accepted","requestId":"r","n":1.0}"#).unwrap(),
            )],
        );
        let rep = diff(&a, &b, &play, &[], &t);
        let paths: Vec<_> = rep
            .mismatches
            .iter()
            .map(|m| (m.path.clone(), m.tol.clone()))
            .collect();
        assert!(
            paths.contains(&("n".into(), UNTOLERATED.into())),
            "{paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|(p, tol)| p.contains("key order") && tol == UNTOLERATED),
            "{paths:?}"
        );
    }

    #[test]
    fn report_periods_t5_and_sudo_o3() {
        let t = table();
        let play = PlayInfo::default();
        let r = |w: i64, origin: &str| {
            msg(
                w,
                "config/reported",
                serde_json::json!({"v":1,"ts":w,"rev":2,"origin":origin}),
            )
        };
        let mut a = cap(
            "js",
            vec![r(1_000, "boot"), r(3_500, "cloud"), r(60_000, "local")],
        );
        let mut b = cap("rust", vec![r(1_100, "cloud"), r(60_100, "local")]);
        a.sudo = vec!["-n systemctl start --no-block xorgate-updater.service".into()];
        b.sudo = vec!["-n systemctl start --no-block xorgate-updater-rs.service".into()];
        // Two triggers 1.6 s apart inside the period straddle the debounce.
        let facts = InputFacts {
            inputs: vec![(900, "config"), (2_500, "cli")],
            ..Default::default()
        };
        let rep = diff_with(&a, &b, &play, &facts, &[], &t);
        let tols: Vec<(&str, &str, &str)> = rep
            .mismatches
            .iter()
            .map(|m| (m.stream.as_str(), m.path.as_str(), m.tol.as_str()))
            .collect();
        assert!(tols.contains(&("report", "(count)", "T5")), "{tols:?}");
        // Triggers 3 s apart (or none): the count difference is real.
        for inputs in [vec![(500, "config"), (3_500, "cli")], vec![]] {
            let f = InputFacts {
                inputs,
                ..Default::default()
            };
            let rep = diff_with(&a, &b, &play, &f, &[], &t);
            assert!(rep
                .mismatches
                .iter()
                .any(|m| m.path == "(count)" && m.tol == UNTOLERATED));
        }
        assert!(tols.contains(&("sudo", "(argv)", "T8/O3")), "{tols:?}");
        assert_eq!(rep.untolerated(), 0, "{tols:?}");
    }

    #[test]
    fn events_multiset_with_t6_prefixes() {
        let t = table();
        let mut a = cap("js", vec![]);
        let mut b = cap("rust", vec![]);
        a.events = vec![
            (1, "info".into(), "connected".into()),
            (2, "info".into(), "hardware: cam0 ok".into()),
        ];
        b.events = vec![
            (1, "info".into(), "connected".into()),
            (3, "warn".into(), "something new".into()),
        ];
        let rep = diff(&a, &b, &PlayInfo::default(), &[], &t);
        let tols: Vec<(&str, &str)> = rep
            .mismatches
            .iter()
            .map(|m| (m.path.as_str(), m.tol.as_str()))
            .collect();
        assert!(
            tols.contains(&("info: hardware: cam0 ok", "T6")),
            "{tols:?}"
        );
        assert!(
            tols.contains(&("warn: something new", UNTOLERATED)),
            "{tols:?}"
        );
    }

    #[test]
    fn t6_events_exact_phase_prefixes_js_only() {
        let t = table();
        let ev = |l: &str, m: &str| (1, l.to_string(), m.to_string());
        let tol_of = |a: &Capture, b: &Capture, key: &str| {
            diff(a, b, &PlayInfo::default(), &[], &t)
                .mismatches
                .into_iter()
                .find(|m| m.stream == "events" && m.path == key)
                .map(|m| m.tol)
        };
        let mut js = cap("js", vec![]);
        let rust = cap("rust", vec![]);
        // Phase 2 uploadBucket events are NOT phase-owned (worker.ts:800/806).
        js.events = vec![
            ev("info", "uploadBucket backfilled: my-bucket"),
            ev("warn", "uploadBucket persist failed: EACCES"),
            ev("info", "upload paused (offline)"),
            ev(
                "info",
                "[clock] NTP synchronized — offline clock stepping stood down for this boot",
            ),
            ev("info", "hardware: cam0 ok"),
            ev("info", "hardware block updated: cm5/v4l2"),
        ];
        for (k, want) in [
            ("info: uploadBucket backfilled: my-bucket", UNTOLERATED),
            ("warn: uploadBucket persist failed: EACCES", UNTOLERATED),
            ("info: upload paused (offline)", "T6"),
            (
                "info: [clock] NTP synchronized — offline clock stepping stood down for this boot",
                "T6",
            ),
            ("info: hardware: cam0 ok", "T6"),
            ("info: hardware block updated: cm5/v4l2", UNTOLERATED),
        ] {
            assert_eq!(tol_of(&js, &rust, k).as_deref(), Some(want), "{k}");
        }
        // Rust emitting a Phase 3/4 event is a bug: never tolerated.
        let mut r2 = cap("rust", vec![]);
        r2.events = vec![ev("info", "[clock] tick failed: x")];
        assert_eq!(
            tol_of(&cap("js", vec![]), &r2, "info: [clock] tick failed: x").as_deref(),
            Some(UNTOLERATED)
        );
        // Both have it, counts differ: not "absent on Rust" → UNTOLERATED.
        let mut j3 = cap("js", vec![]);
        j3.events = vec![
            ev("info", "hardware: a"),
            (2, "info".into(), "hardware: a".into()),
        ];
        let mut r3 = cap("rust", vec![]);
        r3.events = vec![ev("info", "hardware: a")];
        assert_eq!(
            tol_of(&j3, &r3, "info: hardware: a").as_deref(),
            Some(UNTOLERATED)
        );
        // B = js, A = rust: the direction follows the kinds, not the labels.
        assert_eq!(
            tol_of(&rust, &js, "info: hardware: cam0 ok").as_deref(),
            Some("T6")
        );
        // JS vs JS: never tolerated by T6.
        assert_eq!(
            tol_of(&js, &cap("js", vec![]), "info: hardware: cam0 ok").as_deref(),
            Some(UNTOLERATED)
        );
    }

    #[test]
    fn events_wp1_rows_error_text_cadence_task_prefix() {
        let t = table();
        let mut a = cap("js", vec![]);
        let mut b = cap("rust", vec![]);
        a.events = vec![
            (1, "error".into(), "read ECONNRESET".into()),
            (2, "warn".into(), "reconnecting…".into()),
            (3, "warn".into(), "reconnecting…".into()),
            (4, "error".into(), "unhandledRejection: boom".into()),
            (0, "info".into(), "connected to broker".into()),
            (11, "info".into(), "connected to broker".into()),
        ];
        b.events = vec![
            (0, "info".into(), "connected to broker".into()),
            (12, "info".into(), "connected to broker".into()),
            (
                1,
                "error".into(),
                "I/O: Connection reset by peer (os error 104)".into(),
            ),
            (2, "warn".into(), "reconnecting…".into()),
            (
                4,
                "error".into(),
                "unhandledRejection: config_plane: boom".into(),
            ),
        ];
        let tols_of = |rep: &Report| -> Vec<(String, String)> {
            rep.mismatches
                .iter()
                .map(|m| (m.path.clone(), m.tol.clone()))
                .collect()
        };
        let has =
            |v: &[(String, String)], p: &str, t: &str| v.iter().any(|(a, b)| a == p && b == t);
        // Without a closed span the reconnect count difference is a FAIL …
        let rep = diff(&a, &b, &PlayInfo::default(), &[], &t);
        let tols = tols_of(&rep);
        assert!(has(&tols, "warn: reconnecting…", UNTOLERATED), "{tols:?}");
        assert!(
            has(&tols, "error: <transport error>", "T8/mqtt-error-text"),
            "{tols:?}"
        );
        assert!(
            has(&tols, "error: unhandledRejection: boom", "T8/task-prefix"),
            "{tols:?}"
        );
        // … with one it is the backoff cadence.
        let play = PlayInfo {
            closed_spans: vec![(0, 10)],
            ..Default::default()
        };
        let rep = diff(&a, &b, &play, &[], &t);
        let tols = tols_of(&rep);
        assert!(
            has(&tols, "warn: reconnecting…", "T8/reconnect-cadence"),
            "{tols:?}"
        );
        assert_eq!(rep.untolerated(), 0, "{tols:?}");
        // Rust reconnecting MORE often than mqtt.js: not the backoff.
        let (mut a2, mut b2) = (b.clone(), a.clone());
        a2.meta.kind = "js".into();
        b2.meta.kind = "rust".into();
        let rep = diff(&a2, &b2, &play, &[], &t);
        assert!(has(&tols_of(&rep), "warn: reconnecting…", UNTOLERATED));
        // A transport-error COUNT difference only in a run with a cut.
        let mut b3 = b.clone();
        b3.events
            .push((5, "error".into(), "I/O: broken pipe".into()));
        let rep = diff(&a, &b3, &PlayInfo::default(), &[], &t);
        assert!(has(&tols_of(&rep), "error: <transport error>", UNTOLERATED));
        let rep = diff(&a, &b3, &play, &[], &t);
        assert!(has(
            &tols_of(&rep),
            "error: <transport error>",
            "T8/reconnect-cadence"
        ));
        // `connected to` must be #closed spans + 1 on each side.
        let mut a4 = a.clone();
        a4.events
            .push((13, "info".into(), "connected to broker".into()));
        let rep = diff(&a4, &b, &play, &[], &t);
        assert!(rep
            .mismatches
            .iter()
            .any(|m| m.at == "A count" && m.tol == UNTOLERATED));
    }

    #[test]
    fn fffd_equivalence() {
        assert!(fffd_equiv("a\u{FFFD}", "a\\ud83d"));
        assert!(!fffd_equiv("ab", "ab"));
        assert!(!fffd_equiv("a\u{FFFD}", "b"));
    }
}
