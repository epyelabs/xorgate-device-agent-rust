//! `xorgate-replay diff` — compare two rig captures under `tolerances.toml`
//! (phase file §Record/replay "Capture and diff").
//!
//! Capture layout (written by `play`; see `tools/replay/README.md`):
//! ```text
//! <run>/play.json                    run facts: start/end wall, shift, closed spans, transitions
//! <run>/pairs.ndjson                 {"a":tsA,"b":tsB} telemetry pairs the player saw per hold window
//! <run>/<label>/meta.json            {"label","kind","deviceId",…}
//! <run>/<label>/mqtt.ndjson          {"t","w","topic","retain","qos","payload":"<raw>"} every publish of that agent
//! <run>/<label>/files.ndjson         {"w","name","text"|null} file snapshots on change (1 s poll)
//! <run>/<label>/final/<name>         the files at the end
//! <run>/<label>/sudo.log             "<epoch ms> <argv…>" per sudo shim call
//! ```
//!
//! Streams compared: telemetry (paired by `ts`), report (quiet periods), ack,
//! status, sms (in order), files at end, state.json at end, the state.json
//! `recentEvents` msg multiset, the sudo argv sequence, the pidfile shape.
//! Every mismatch is listed with both values and the tolerance id that covers
//! it, or `UNTOLERATED`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    payload: String,
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub w: i64,
    /// Topic with the agent's deviceId replaced by `{id}`.
    pub topic: String,
    /// Kept for reports (a retained publish from an agent would be a bug).
    #[allow(dead_code)]
    pub retain: bool,
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
                    payload: parse_payload(&norm_id(&m.payload, &id)),
                });
            }
        }
        let mut finals = BTreeMap::new();
        for name in FILES {
            let p = dir.join("final").join(name);
            finals.insert(
                name.to_string(),
                std::fs::read(&p)
                    .ok()
                    .map(|b| norm_id(&String::from_utf8_lossy(&b), &id)),
            );
        }
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
                let at = e["at"].as_i64().unwrap_or(0);
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
    pub unpaired_a: usize,
    pub unpaired_b: usize,
    pub tolerated: usize,
    pub untolerated: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub stats: BTreeMap<String, StreamStats>,
    pub mismatches: Vec<Mismatch>,
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
    /// T2 bases: path → (base A, base B).
    deltas: &'a BTreeMap<String, (f64, f64)>,
    first_wall: i64,
}

impl Cx<'_> {
    fn near_transition(&self) -> Option<String> {
        let w = self.when?;
        let r = self.table.of_kind("presence_window", &self.stream).next()?;
        let window = r.window_ms.unwrap_or(1500) as i64;
        let startup = r.startup_ms.unwrap_or(5000) as i64;
        if w - self.first_wall <= startup
            || self
                .play
                .transitions
                .iter()
                .any(|t| (w - t).abs() <= window)
        {
            return Some(r.id.clone());
        }
        None
    }

    fn near_closed(&self) -> Option<String> {
        let w = self.when?;
        let (rid, row) = self.table.decision_row("cadence")?;
        if !crate::tolerances::stream_matches(&row.streams, &self.stream) {
            return None;
        }
        let pad = row.around_closed_ms.unwrap_or(5000) as i64;
        self.play
            .closed_spans
            .iter()
            .any(|(a, b)| w >= a - pad && w <= b + pad)
            .then(|| format!("{rid}/cadence"))
    }

    fn near_wall_step(&self) -> Option<String> {
        let w = self.when?;
        let (rid, _) = self.table.decision_row("O2")?;
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

/// Recursive compare. Objects compare key ORDER too (wire bytes, serde exactness).
fn cmp(cx: &Cx, path: &str, a: Option<&Value>, b: Option<&Value>, out: &mut Report) {
    if a == b && num_strict_eq(a, b) {
        return;
    }
    if let Some(id) = cx.table.excluded(&cx.stream, path) {
        out.push(cx.mismatch(path, a, b, id));
        return;
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
            // T2 deltas.
            if let (Some(id), Some(p), Some(q)) =
                (cx.table.is_delta(&cx.stream, path), x.as_f64(), y.as_f64())
            {
                let (ba, bb) = cx.deltas.get(path).copied().unwrap_or((p, q));
                if p - ba == q - bb {
                    return;
                }
                out.push(cx.mismatch(
                    path,
                    a,
                    b,
                    cx.near_closed().unwrap_or_else(|| {
                        let _ = id;
                        UNTOLERATED.into()
                    }),
                ));
                return;
            }
            // T3 numeric.
            if let (Some((id, tol)), Some(p), Some(q)) =
                (cx.table.numeric(&cx.stream, path), x.as_f64(), y.as_f64())
            {
                let t = if (p - q).abs() <= tol + 1e-9 {
                    id
                } else {
                    UNTOLERATED.into()
                };
                out.push(cx.mismatch(path, a, b, t));
                return;
            }
            // T7 FFFD vs lone surrogate (D1/D2).
            if let (Some(p), Some(q)) = (x.as_str(), y.as_str()) {
                let rows = cx.table.matcher_rows("fffd_vs_lone_surrogate", &cx.stream);
                if !rows.is_empty() && fffd_equiv(p, q) {
                    out.push(cx.mismatch(path, a, b, rows[0].clone()));
                    return;
                }
            }
            let tol = cx.near_wall_step().unwrap_or_else(|| UNTOLERATED.into());
            out.push(cx.mismatch(path, a, b, tol));
        }
        _ => {
            // Presence (or array length / type) mismatch.
            let tol = cx
                .near_transition()
                .or_else(|| cx.near_closed())
                .or_else(|| cx.near_wall_step())
                .unwrap_or_else(|| UNTOLERATED.into());
            out.push(cx.mismatch(path, a, b, tol));
        }
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
    // Fallback: greedy nearest for whatever the player did not pair.
    let mut j0 = 0;
    for (i, ma) in a.iter().enumerate() {
        if used_a[i] {
            continue;
        }
        let Some(ta) = ts_of(&ma.payload) else {
            continue;
        };
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

/// Run the whole comparison.
pub fn diff(
    a: &Capture,
    b: &Capture,
    play: &PlayInfo,
    pairs: &[(i64, i64)],
    table: &Table,
) -> Report {
    let mut rep = Report::default();
    let empty = BTreeMap::new();
    let first_wall = play.start_wall;

    // ---- telemetry
    let ta: Vec<&Msg> = a
        .msgs
        .iter()
        .filter(|m| m.topic.ends_with("/telemetry"))
        .collect();
    let tb: Vec<&Msg> = b
        .msgs
        .iter()
        .filter(|m| m.topic.ends_with("/telemetry"))
        .collect();
    let (paired, ua, ub) = pair_telemetry(&ta, &tb, pairs);
    let bases = first_values(&paired, table, "telemetry");
    {
        let st = rep.stats.entry("telemetry".into()).or_default();
        st.compared = paired.len();
        st.unpaired_a = ua.len();
        st.unpaired_b = ub.len();
    }
    for (x, y) in &paired {
        let w = ts_of(&x.payload);
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
            deltas: &bases,
            first_wall,
        };
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
    for (side, list) in [("A", &ua), ("B", &ub)] {
        for m in list.iter() {
            let w = ts_of(&m.payload);
            let cx = Cx {
                table,
                play,
                stream: "telemetry".into(),
                at: format!("unpaired {side} ts {}", w.unwrap_or(m.w)),
                when: w.or(Some(m.w)),
                deltas: &empty,
                first_wall,
            };
            let tol = cx
                .near_closed()
                .or_else(|| {
                    // The first/last tick of a run may have no partner.
                    let w = w.unwrap_or(m.w);
                    (w - first_wall <= 5_000 || play.end_wall - w <= 3_000)
                        .then(|| "T4".to_string())
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

    // ---- reports: quiet periods
    diff_reports(a, b, play, table, &mut rep);

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
                deltas: &empty,
                first_wall,
            };
            match (x, y) {
                (Some(x), Some(y)) => cmp(&cx, "", Some(&x.payload), Some(&y.payload), &mut rep),
                _ => {
                    // A status message around a cut connection (online/LWT
                    // timing) is cadence; anything else is a real difference.
                    let tol = if stream == "status" {
                        cx.near_closed()
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

    // ---- files at end
    for name in FILES {
        let stream = match *name {
            "state.json" => "state".to_string(),
            "agent.pid" => "pidfile".to_string(),
            n => format!("file:{n}"),
        };
        let (fa, fb) = (
            a.finals.get(*name).cloned().flatten(),
            b.finals.get(*name).cloned().flatten(),
        );
        rep.stats.entry(stream.clone()).or_default().compared += 1;
        let cx = Cx {
            table,
            play,
            stream: stream.clone(),
            at: "end".into(),
            when: None,
            deltas: &empty,
            first_wall,
        };
        match (&fa, &fb) {
            (None, None) => {}
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => {
                let (vx, vy) = (parse_payload(x), parse_payload(y));
                let (mut vx, mut vy) = (vx, vy);
                if *name == "state.json" {
                    // recentEvents are compared as the `events` multiset.
                    for v in [&mut vx, &mut vy] {
                        if let Some(o) = v.as_object_mut() {
                            o.remove("recentEvents");
                        }
                    }
                }
                let before = rep.mismatches.len();
                cmp(&cx, "", Some(&vx), Some(&vy), &mut rep);
                if rep.mismatches.len() == before && *name == "settings.json" {
                    // Same JSON, different bytes (whitespace) — a FAIL: the
                    // writer is byte-exact (atomicWriteJson layout).
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
    diff_events(a, b, table, &mut rep);

    // ---- sudo argv sequence
    diff_sudo(a, b, table, &mut rep);

    rep
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

fn diff_reports(a: &Capture, b: &Capture, play: &PlayInfo, table: &Table, rep: &mut Report) {
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
            deltas: &empty,
            first_wall: play.start_wall,
        };
        if pa.len() != pb.len() {
            let tol =
                if pa.len().abs_diff(pb.len()) <= max_delta && !pa.is_empty() && !pb.is_empty() {
                    t5.clone()
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
                    ..cx
                };
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
                    deltas: &empty,
                    first_wall: play.start_wall,
                };
                cmp(&cx, "", Some(&x.payload), Some(&y.payload), rep);
            }
        }
    }
    rep.stats.entry("report".into()).or_default().compared = compared;
}

fn diff_events(a: &Capture, b: &Capture, table: &Table, rep: &mut Report) {
    let count = |c: &Capture| {
        let mut m: BTreeMap<String, usize> = BTreeMap::new();
        for (_, level, msg) in &c.events {
            *m.entry(format!("{level}: {msg}")).or_default() += 1;
        }
        m
    };
    let (ca, cb) = (count(a), count(b));
    let keys: BTreeSet<&String> = ca.keys().chain(cb.keys()).collect();
    let t6 = table.of_kind("phase_owned", "events").next();
    let st = rep.stats.entry("events".into()).or_default();
    st.compared = keys.len();
    for k in keys {
        let (x, y) = (
            ca.get(k).copied().unwrap_or(0),
            cb.get(k).copied().unwrap_or(0),
        );
        if x == y {
            continue;
        }
        let msg = k.split_once(": ").map(|(_, m)| m).unwrap_or(k);
        let tol = t6
            .filter(|r| r.event_prefixes.iter().any(|p| msg.starts_with(p.as_str())))
            .map(|r| r.id.clone())
            .unwrap_or_else(|| UNTOLERATED.into());
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
    let _ = writeln!(o, "## Per stream\n");
    let _ = writeln!(
        o,
        "| stream | compared | unpaired A | unpaired B | tolerated | UNTOLERATED |"
    );
    let _ = writeln!(o, "|---|---|---|---|---|---|");
    for (s, st) in &rep.stats {
        let _ = writeln!(
            o,
            "| {s} | {} | {} | {} | {} | {} |",
            st.compared, st.unpaired_a, st.unpaired_b, st.tolerated, st.untolerated
        );
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

/// Load a run dir (`play.json`, `pairs.ndjson`, `a/`, `b/`).
/// A loaded run: both captures, the run facts and the player's pairs.
pub type Run = (Capture, Capture, PlayInfo, Vec<(i64, i64)>);

pub fn load_run(run: &Path, a: Option<PathBuf>, b: Option<PathBuf>) -> Result<Run> {
    let play: PlayInfo = std::fs::read(run.join("play.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let pairs = std::fs::read_to_string(run.join("pairs.ndjson"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| Some((v["a"].as_i64()?, v["b"].as_i64()?)))
        .collect();
    let ca = Capture::load(&a.unwrap_or_else(|| run.join("a")))?;
    let cb = Capture::load(&b.unwrap_or_else(|| run.join("b")))?;
    Ok((ca, cb, play, pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let rep = diff(&a, &b, &play, &[], &t);
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
        assert!(tols.contains(&("metrics.gps", "T4")), "{tols:?}");
        assert!(
            tols.contains(&("metrics.gps.lat.value", UNTOLERATED)),
            "{tols:?}"
        );
        // seq deltas match (5→6→15→25 vs 105→106→115→125).
        assert!(!tols.iter().any(|(p, _)| *p == "seq"), "{tols:?}");
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
        let rep = diff(&a, &b, &play, &[], &t);
        let tols: Vec<(&str, &str, &str)> = rep
            .mismatches
            .iter()
            .map(|m| (m.stream.as_str(), m.path.as_str(), m.tol.as_str()))
            .collect();
        assert!(tols.contains(&("report", "(count)", "T5")), "{tols:?}");
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
    fn fffd_equivalence() {
        assert!(fffd_equiv("a\u{FFFD}", "a\\ud83d"));
        assert!(!fffd_equiv("ab", "ab"));
        assert!(!fffd_equiv("a\u{FFFD}", "b"));
    }
}
