//! Phase 3 streams of `xorgate-replay diff` (phase-3-recording-media.md
//! §Record/replay "What is compared, and how"; rows T10–T14 of
//! `tools/replay/tolerances.toml`). Inputs are what the player's media side
//! captured per agent (`media.rs`): `shim.ndjson`, `procs.ndjson`,
//! `recordings.json`, `final/recordings/**`, `final/recordings.mp4.json`,
//! `s3.ndjson`, `s3/**`, and the run's `shm.ndjson`.
//!
//! Normalisation (both sides): the agent's work dir → `<work>`, its deviceId →
//! `{id}`, every session uuid → `<sid:video:<key>#<n>>` / `<sid:telemetry#<n>>` (n = order of first
//! appearance per `video/<key>` / `telemetry` dir), telemetry file-name ms
//! stamps → `<ms>` (T13), the agent's own rig cert/key paths → `<cert>` /
//! `<key>` (kvs env). Telemetry segment bodies are compared decompressed with
//! the same normalisation (header `deviceId`, `sessionId`).
//!
//! | stream | exact | tolerated |
//! |---|---|---|
//! | `children` | per (role, key): spawn count, argv, kvs env, how each child ended (`int` / `eos` / `kill` / `external` / `exit:<code>`) | spawn instant ±1 s (T10); stagger gap ≥ 4 s is a contract check per side |
//! | `session` | `session.json` fields | `sessionId`, `bootId`, `anchorTs`, `anchorMono` (T11) |
//! | `sidecar` | key order; every field but the tolerated ones | `startTs` ±300 ms relative to the agent's own capture PLAYING (`New clock:`) time, `startMono` deltas within the session ±300 ms, `durationMs` ±150 ms, `sizeBytes` any (T12) |
//! | `mp4` | per side: seq contiguity from 0, no 0-byte closed fragment | fragment count per session ±1 (T12, the run's cut) |
//! | `rec-telemetry` | decompressed header lines minus T1/T11 fields; sample key order; per segment the groups present, each group's leaf paths and number SHAPES (T9); per session the deduplicated VALUE sequence of every non-host leaf and per-group sample counts (G1) | segment count per session ±1 and file-name ms stamps (T13; gz bytes never compared: D24) |
//! | `s3` | per side every SigV4 signature valid, sidecar PUT before its mp4, each source's first successful PUTs oldest-first; A vs B the normalised key multiset of successful PUTs + content types, and each source's order of the keys both PUT | a session's LAST fragment / LAST telemetry segment present on one side only (never a middle segment), attempt counts / timing (T14) |
//! | `file:counters.json` | bytes as JSON | — |
//! | `evictions` | the ordered list of fragments/segments deleted without a successful PUT | — |
//! | `shm` | per side: orphan `shmpipe.<pid>.*` (pid of that side's dead gst child) count at the end | — |
//!
//! Sample VALUES of the telemetry segments (review-wp6 G1): per session and
//! leaf (not T3's host-read paths) the run-length-deduplicated value
//! sequence over the segments both sides hold, exactly, plus per group the
//! count of samples carrying it (`rec_values`; rows `T4/rec-sample`, T4).
//! Each recorder ticks on its own timer, ungated by the player, so only a
//! step carried by one sample and missing on the side whose tick raced that
//! input's release (or at a stretch edge) is tolerated. Stats: `rec-values`
//! (leaf sequences), `rec-presence` (group counts).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value};

use crate::diff::{Mismatch, PlayInfo, Report, UNTOLERATED};
use crate::tolerances::Table;

/// The TS default `spawnStaggerSecs` (config.ts:58); the synth tapes keep it.
const STAGGER_MS: i64 = 4_000;

/// How a child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    pub w: i64,
    pub role: String,
    pub key: String,
    pub pid: u32,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Wall ms of `New clock:` (capture PLAYING).
    pub playing: Option<i64>,
    pub end: Option<String>,
    /// Wall ms of the child's exit (kvs: the shim's `exit` line).
    pub end_w: Option<i64>,
    pub sid: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub size: u64,
    pub gone: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct S3Obs {
    pub key: String,
    pub ctype: String,
    pub status: u64,
    pub sig: String,
}

/// One agent's media capture, normalised.
#[derive(Debug, Clone, Default)]
pub struct MediaCapture {
    pub kind: String,
    pub spawns: Vec<Spawn>,
    pub rec: BTreeMap<String, Seen>,
    pub finals: BTreeMap<String, Vec<u8>>,
    pub mp4_final: BTreeMap<String, u64>,
    pub s3: Vec<S3Obs>,
    pub s3_bodies: BTreeMap<String, Vec<u8>>,
    /// Orphan shmpipe names at the end (pid = a dead gst child of this side).
    pub shm_orphans: usize,
    /// Wall ms (publish `w`) of every telemetry tick whose
    /// `recording.powerShed` / `recording.liveBlock` differs from the
    /// previous tick's (`mqtt.ndjson`; T7/D30 `d30_shed_tick_session_end` (d)).
    pub shed_changes: Vec<i64>,
    /// `recording.backlogSegments` of the agent's LAST telemetry tick and
    /// of its final state.json (written at shutdown) — the end-snapshot
    /// per-side bound (review-wp6 FLAG 3).
    pub last_tick_backlog: Option<i64>,
    /// `(ts, "<level>: <msg>")` of every `power shed →` event seen in the
    /// state.json snapshots (`files.ndjson`; T8/d30-js-reference).
    pub shed_events: Vec<(i64, String)>,
    pub end_backlog: Option<i64>,
    /// Anything media-related captured at all.
    pub present: bool,
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Every uuid substring of `s` with its position.
fn uuids_in(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i + 36 <= b.len() {
        if s.is_char_boundary(i) && s.is_char_boundary(i + 36) && is_uuid(&s[i..i + 36]) {
            out.push(&s[i..i + 36]);
            i += 36;
        } else {
            i += 1;
        }
    }
    out
}

/// The session dir key of a path / S3 key containing `sid`.
fn dir_of(path: &str, sid: &str) -> String {
    let before = path.split(sid).next().unwrap_or("");
    let segs: Vec<&str> = before.split('/').filter(|s| !s.is_empty()).collect();
    if segs.contains(&"telemetry") {
        "telemetry".into()
    } else {
        // …/video/<key>/<sid> or media-sessions/v1/<id>/<key>/<sid>
        format!("video/{}", segs.last().copied().unwrap_or("?"))
    }
}

/// Mask 13-digit ms stamps (T13) in telemetry file names.
fn mask_ms(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let j = (i..b.len())
                .find(|&j| !b[j].is_ascii_digit())
                .unwrap_or(b.len());
            if j - i == 13 {
                out.push_str("<ms>");
            } else {
                out.push_str(&s[i..j]);
            }
            i = j;
        } else {
            let c = s[i..].chars().next().unwrap_or(' ');
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

struct Norm {
    work: String,
    id: String,
    sids: BTreeMap<String, String>,
    /// The agent's own mTLS cert/key paths (`/rig/certs/<label>.crt|.key`,
    /// meta env `CERT_PATH`/`KEY_PATH`) → `<cert>`/`<key>`: the kvs env
    /// keys `AWS_IOT_CORE_CERT`/`_PRIVATE_KEY` name them per agent.
    files: Vec<(String, &'static str)>,
}

impl Norm {
    fn s(&self, s: &str) -> String {
        let mut t = s.to_string();
        if !self.work.is_empty() {
            t = t.replace(&self.work, "<work>");
        }
        if !self.id.is_empty() {
            t = t.replace(&self.id, "{id}");
        }
        for (f, l) in &self.files {
            if !f.is_empty() {
                t = t.replace(f.as_str(), l);
            }
        }
        for (u, l) in &self.sids {
            if t.contains(u.as_str()) {
                t = t.replace(u.as_str(), l);
            }
        }
        t
    }
    fn path(&self, p: &str) -> String {
        let t = self.s(p);
        if t.contains("telemetry") {
            mask_ms(&t)
        } else {
            t
        }
    }
}

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn walk(root: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, base, out);
        } else if let (Ok(rel), Ok(b)) = (p.strip_prefix(base), std::fs::read(&p)) {
            out.insert(rel.display().to_string(), b);
        }
    }
}

impl MediaCapture {
    /// Load `<run>/<label>/` (see the module doc); `shm` = the run's
    /// `shm.ndjson` records.
    pub fn load(dir: &Path, shm: &[Value]) -> Self {
        let meta: Value = std::fs::read(dir.join("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let id = meta["deviceId"].as_str().unwrap_or("").to_string();
        let home = meta["home"].as_str().unwrap_or("");
        let work = Path::new(home)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let shim = lines(&dir.join("shim.ndjson"));
        let procs = lines(&dir.join("procs.ndjson"));
        let rec_json: Value = std::fs::read(dir.join("recordings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let s3l = lines(&dir.join("s3.ndjson"));
        let mut finals_raw = BTreeMap::new();
        walk(
            &dir.join("final/recordings"),
            &dir.join("final/recordings"),
            &mut finals_raw,
        );
        let mut s3_raw = BTreeMap::new();
        walk(&dir.join("s3"), &dir.join("s3"), &mut s3_raw);
        let mp4_raw: Map<String, Value> = std::fs::read(dir.join("final/recordings.mp4.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();

        // Session uuids by first appearance per dir.
        let mut first: BTreeMap<String, (i64, String)> = BTreeMap::new();
        let mut note = |path: &str, w: i64| {
            for u in uuids_in(path) {
                if u == id {
                    continue;
                }
                let d = dir_of(path, u);
                let e = first.entry(u.to_string()).or_insert((w, d));
                if w < e.0 {
                    e.0 = w;
                }
            }
        };
        if let Some(files) = rec_json["files"].as_object() {
            for (rel, s) in files {
                note(
                    &format!("recordings/{rel}"),
                    s["first"].as_i64().unwrap_or(i64::MAX),
                );
            }
        }
        for v in &shim {
            if v["k"] == "spawn" {
                for a in v["argv"].as_array().into_iter().flatten() {
                    if let Some(a) = a.as_str().filter(|a| a.starts_with("location=")) {
                        note(a, v["w"].as_i64().unwrap_or(i64::MAX));
                    }
                }
            }
        }
        for v in &s3l {
            note(
                v["key"].as_str().unwrap_or(""),
                v["w"].as_i64().unwrap_or(i64::MAX),
            );
        }
        let mut per_dir: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
        for (u, (w, d)) in first {
            per_dir.entry(d).or_default().push((w, u));
        }
        let mut sids = BTreeMap::new();
        for (d, mut v) in per_dir {
            v.sort();
            for (i, (_, u)) in v.into_iter().enumerate() {
                // No `/` inside the label: it sits in paths.
                sids.insert(u, format!("<sid:{}#{i}>", d.replace('/', ":")));
            }
        }
        let files = vec![
            (
                meta["env"]["CERT_PATH"].as_str().unwrap_or("").to_string(),
                "<cert>",
            ),
            (
                meta["env"]["KEY_PATH"].as_str().unwrap_or("").to_string(),
                "<key>",
            ),
        ];
        let n = Norm {
            work,
            id,
            sids,
            files,
        };

        // Children.
        let mut spawns: Vec<Spawn> = Vec::new();
        let mut by_pid: BTreeMap<u32, usize> = BTreeMap::new();
        let mut lines_of: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        let mut kvs_end: BTreeMap<u32, String> = BTreeMap::new();
        let mut exit_w: BTreeMap<u32, i64> = BTreeMap::new();
        for v in &shim {
            let pid = v["pid"].as_u64().unwrap_or(0) as u32;
            match v["k"].as_str() {
                Some("spawn") => {
                    let tool = v["tool"].as_str().unwrap_or("");
                    let argv: Vec<String> = v["argv"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| a.as_str().map(str::to_string))
                        .collect();
                    let (role, key) = if tool == "xorgate-kvs-webrtc" {
                        ("kvs", argv.first().cloned().unwrap_or_default())
                    } else {
                        (
                            "capture",
                            crate::media::capture_key(&argv).unwrap_or_else(|| "?".into()),
                        )
                    };
                    let sid = argv
                        .iter()
                        .find(|a| a.starts_with("location="))
                        .and_then(|a| uuids_in(a).first().map(|u| u.to_string()))
                        .map(|u| n.s(&u));
                    let env: BTreeMap<String, String> = v["env"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(k, x)| (k.clone(), n.s(x.as_str().unwrap_or(""))))
                        .collect();
                    by_pid.insert(pid, spawns.len());
                    spawns.push(Spawn {
                        w: v["w"].as_i64().unwrap_or(0),
                        role: role.into(),
                        key: n.s(&key),
                        pid,
                        argv: argv.iter().map(|a| n.s(a)).collect(),
                        env,
                        playing: None,
                        end: None,
                        end_w: None,
                        sid,
                    });
                }
                Some("line") => {
                    let line = v["line"].as_str().unwrap_or("").to_string();
                    if line.contains("New clock:") {
                        if let Some(&i) = by_pid.get(&pid) {
                            spawns[i]
                                .playing
                                .get_or_insert(v["w"].as_i64().unwrap_or(0));
                        }
                    }
                    lines_of.entry(pid).or_default().push(line);
                }
                Some("signal") if v["sig"] == 2 => {
                    kvs_end.insert(pid, "int".into());
                }
                Some("exit") => {
                    exit_w.insert(pid, v["w"].as_i64().unwrap_or(0));
                    let code = v["code"].as_i64().unwrap_or(-1);
                    let k = if v["external"] == true {
                        "external".to_string()
                    } else if code == 0 && kvs_end.get(&pid).map(String::as_str) == Some("int") {
                        "int".to_string()
                    } else {
                        format!("exit:{code}")
                    };
                    kvs_end.insert(pid, k);
                }
                _ => {}
            }
        }
        let gone: BTreeSet<u32> = procs
            .iter()
            .filter(|v| v["k"] == "gone")
            .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
            .collect();
        let external_cap: BTreeSet<u32> = procs
            .iter()
            .filter(|v| v["k"] == "external" && v["role"] == "capture")
            .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
            .collect();
        for s in spawns.iter_mut() {
            if s.role == "kvs" {
                s.end_w = exit_w.get(&s.pid).copied();
                s.end = kvs_end
                    .get(&s.pid)
                    .cloned()
                    .or_else(|| gone.contains(&s.pid).then(|| "kill".to_string()));
            } else if gone.contains(&s.pid) {
                let ls = lines_of.get(&s.pid).cloned().unwrap_or_default();
                let int = ls.iter().any(|l| l.contains("handling interrupt"));
                let eos = ls.iter().any(|l| l.contains("Got EOS"));
                s.end = Some(
                    if external_cap.contains(&s.pid) {
                        "external"
                    } else if int {
                        "int"
                    } else if eos {
                        "eos"
                    } else {
                        "kill"
                    }
                    .into(),
                );
            }
        }

        let rec: BTreeMap<String, Seen> = rec_json["files"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(rel, s)| {
                (
                    n.path(rel),
                    Seen {
                        size: s["size"].as_u64().unwrap_or(0),
                        gone: s["gone"].as_i64(),
                    },
                )
            })
            .collect();
        // Telemetry segments are kept DECOMPRESSED + normalised (deviceId →
        // `{id}`, session uuid → its label, like the sidecars): the header
        // line carries both, and gz bytes are never compared (D24/T13). A
        // body that does not gunzip is kept as is (`segments` skips it).
        let norm_body = |rel: &str, b: Vec<u8>| -> Vec<u8> {
            if rel.ends_with(".json") {
                n.s(&String::from_utf8_lossy(&b)).into_bytes()
            } else if rel.ends_with(".jsonl.gz") {
                match gunzip(&b) {
                    Some(t) => n.s(&t).into_bytes(),
                    None => b,
                }
            } else {
                b
            }
        };
        let finals: BTreeMap<String, Vec<u8>> = finals_raw
            .into_iter()
            .map(|(rel, b)| {
                let k = n.path(&rel);
                let b = norm_body(&rel, b);
                (k, b)
            })
            .collect();
        let s3_bodies: BTreeMap<String, Vec<u8>> = s3_raw
            .into_iter()
            .map(|(rel, b)| {
                let k = n.path(&rel);
                let b = norm_body(&rel, b);
                (k, b)
            })
            .collect();
        let s3: Vec<S3Obs> = s3l
            .iter()
            .map(|v| S3Obs {
                key: n.path(v["key"].as_str().unwrap_or("")),
                ctype: v["contentType"].as_str().unwrap_or("").to_string(),
                status: v["status"].as_u64().unwrap_or(0),
                sig: v["sig"].as_str().unwrap_or("").to_string(),
            })
            .collect();
        let mp4_final: BTreeMap<String, u64> = mp4_raw
            .iter()
            .map(|(k, v)| (n.path(k), v["size"].as_u64().unwrap_or(0)))
            .collect();
        // Orphan shm segments at the end: shmpipe.<pid>.* whose pid is one of
        // this side's gst children that is gone.
        let last_names: Vec<String> = shm
            .last()
            .and_then(|v| v["names"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        let mine: BTreeSet<u32> = spawns
            .iter()
            .filter(|s| s.role == "capture")
            .map(|s| s.pid)
            .collect();
        let shm_orphans = last_names
            .iter()
            .filter_map(|nm| {
                nm.strip_prefix("shmpipe.")?
                    .split('.')
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .filter(|p| mine.contains(p) && gone.contains(p))
            .count();
        let present = !spawns.is_empty() || !rec.is_empty() || !s3.is_empty();
        let mut shed_changes = Vec::new();
        let mut shed_events: Vec<(i64, String)> = Vec::new();
        for v in lines(&dir.join("files.ndjson")) {
            if v["name"] != "state.json" {
                continue;
            }
            let Some(d) = v["text"]
                .as_str()
                .and_then(|t| serde_json::from_str::<Value>(t).ok())
            else {
                continue;
            };
            for e in d["recentEvents"].as_array().into_iter().flatten() {
                let msg = e["msg"].as_str().unwrap_or("");
                if msg.starts_with("power shed ") {
                    let k = (
                        e["ts"].as_i64().unwrap_or(0),
                        format!("{}: {msg}", e["level"].as_str().unwrap_or("")),
                    );
                    if !shed_events.contains(&k) {
                        shed_events.push(k);
                    }
                }
            }
        }
        let mut last_tick_backlog = None;
        let end_backlog = std::fs::read(dir.join("final/state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["recording"]["backlogSegments"].as_i64());
        let mut prev: Option<(Value, Value)> = None;
        for v in lines(&dir.join("mqtt.ndjson")) {
            if !v["topic"]
                .as_str()
                .is_some_and(|t| t.ends_with("/telemetry"))
            {
                continue;
            }
            let Some(p) = v["payload"]
                .as_str()
                .and_then(|t| serde_json::from_str::<Value>(t).ok())
            else {
                continue;
            };
            let cur = (
                p["recording"]["powerShed"].clone(),
                p["recording"]["liveBlock"].clone(),
            );
            last_tick_backlog = p["recording"]["backlogSegments"].as_i64();
            if prev.as_ref().is_some_and(|q| *q != cur) {
                shed_changes.push(v["w"].as_i64().unwrap_or(0));
            }
            prev = Some(cur);
        }
        Self {
            kind: meta["kind"].as_str().unwrap_or("").to_string(),
            spawns,
            rec,
            finals,
            mp4_final,
            s3,
            s3_bodies,
            shm_orphans,
            shed_changes,
            last_tick_backlog,
            end_backlog,
            shed_events,
            present,
        }
    }
}

fn mm(stream: &str, at: &str, path: &str, a: String, b: String, tol: &str) -> Mismatch {
    Mismatch {
        stream: stream.into(),
        at: at.into(),
        path: path.into(),
        a,
        b,
        tol: tol.into(),
    }
}

fn row_tol(table: &Table, id: &str, key: &str, default: f64) -> f64 {
    table
        .rules
        .iter()
        .find(|r| r.id == id)
        .and_then(|r| r.tolerance.get(key).copied())
        .unwrap_or(default)
}

fn has_row(table: &Table, id: &str) -> bool {
    table.rules.iter().any(|r| r.id == id)
}

fn stat<'a>(rep: &'a mut Report, s: &str) -> &'a mut crate::diff::StreamStats {
    rep.stats.entry(s.to_string()).or_default()
}

fn show(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 300 {
        format!("{}…", s.chars().take(300).collect::<String>())
    } else {
        s
    }
}

/// Leaf-by-leaf JSON compare; `skip(path)` = excluded leaf (cited `tol`).
fn json_cmp(
    stream: &str,
    at: &str,
    path: &str,
    a: &Value,
    b: &Value,
    skip: &dyn Fn(&str) -> Option<String>,
    rep: &mut Report,
) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let kx: Vec<&String> = x.keys().collect();
            let ky: Vec<&String> = y.keys().collect();
            if kx != ky && kx.iter().collect::<BTreeSet<_>>() == ky.iter().collect::<BTreeSet<_>>()
            {
                rep.push(mm(
                    stream,
                    at,
                    &format!("{path}(key order)"),
                    format!("{kx:?}"),
                    format!("{ky:?}"),
                    UNTOLERATED,
                ));
            }
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                match (x.get(k), y.get(k)) {
                    (Some(va), Some(vb)) => json_cmp(stream, at, &p, va, vb, skip, rep),
                    (va, vb) => {
                        let tol = skip(&p).unwrap_or_else(|| UNTOLERATED.into());
                        rep.push(mm(
                            stream,
                            at,
                            &p,
                            va.map(show).unwrap_or_else(|| "∅".into()),
                            vb.map(show).unwrap_or_else(|| "∅".into()),
                            &tol,
                        ))
                    }
                }
            }
        }
        _ => {
            // Numbers compared as their JSON text (T9).
            // serde_json keeps 1 (int) and 1.0 (float) apart, as T9 wants.
            if a != b {
                let tol = skip(path).unwrap_or_else(|| UNTOLERATED.into());
                rep.push(mm(stream, at, path, show(a), show(b), &tol));
            }
        }
    }
}

fn gunzip(b: &[u8]) -> Option<String> {
    let mut d = flate2::read::MultiGzDecoder::new(b);
    let mut s = String::new();
    d.read_to_string(&mut s).ok()?;
    Some(s)
}

/// `(session dir, seq)` of a sidecar / mp4 path: `…/<sid>/NNNNN.(json|mp4)`.
fn frag_of(path: &str) -> Option<(String, u64, &'static str)> {
    let (dir, file) = path.rsplit_once('/')?;
    let (stem, ext) = file.rsplit_once('.')?;
    if stem.len() != 5 || !stem.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let ext = match ext {
        "json" => "json",
        "mp4" => "mp4",
        _ => return None,
    };
    let sid = dir.rsplit('/').next()?.to_string();
    Some((sid, stem.parse().ok()?, ext))
}

/// Every sidecar of a side: local finals ∪ S3 bodies, keyed `<sid>/<seq>`.
fn sidecars(m: &MediaCapture) -> BTreeMap<(String, u64), Value> {
    let mut out = BTreeMap::new();
    for (k, b) in m.s3_bodies.iter().chain(m.finals.iter()) {
        if let Some((sid, seq, "json")) = frag_of(k) {
            if let Ok(v) = serde_json::from_slice::<Value>(b) {
                out.entry((sid, seq)).or_insert(v);
            }
        }
    }
    out
}

/// `(session label, seq)` of a telemetry segment path / S3 key:
/// `…telemetry/<sid>/seg-<ms>-<seq>.jsonl[.gz]` locally,
/// `telemetry/v1/{id}/<sid>/<ms>-<seq>.jsonl.gz` in S3 (ms masked or not).
fn tele_seg_of(path: &str) -> Option<(String, u64)> {
    if !path.contains("telemetry") {
        return None;
    }
    let (dir, file) = path.rsplit_once('/')?;
    let stem = file
        .strip_suffix(".jsonl.gz")
        .or_else(|| file.strip_suffix(".jsonl"))?;
    let seq = stem.rsplit('-').next()?.parse().ok()?;
    Some((dir.rsplit('/').next()?.to_string(), seq))
}

/// Every telemetry segment seq a side ever had, per session: the
/// recordings tree as polled (incl. the open `.jsonl`), the final tree and
/// every S3 PUT attempt.
fn tele_segs(m: &MediaCapture) -> BTreeMap<String, BTreeSet<u64>> {
    let mut out: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    let keys = m
        .rec
        .keys()
        .chain(m.finals.keys())
        .chain(m.s3.iter().map(|o| &o.key));
    for k in keys {
        if let Some((sid, seq)) = tele_seg_of(k) {
            out.entry(sid).or_default().insert(seq);
        }
    }
    out
}

/// Every telemetry segment of a side (decompressed), keyed `<sid>/<n>`
/// (n = order of the file name).
fn segments(m: &MediaCapture) -> BTreeMap<String, Vec<String>> {
    segments_by_seq(m)
        .into_iter()
        .map(|(sid, segs)| (sid, segs.into_values().collect()))
        .collect()
}

/// [`segments`] keyed by the segment seq (file name), per session.
fn segments_by_seq(m: &MediaCapture) -> BTreeMap<String, BTreeMap<u64, String>> {
    let mut per: BTreeMap<String, BTreeMap<u64, String>> = BTreeMap::new();
    for (k, b) in m.s3_bodies.iter().chain(m.finals.iter()) {
        if !k.ends_with(".jsonl.gz") {
            continue;
        }
        // `seg-<ms>-<seq>` locally, `<ms>-<seq>` in S3: key by the seq.
        let Some((sid, seq)) = tele_seg_of(k) else {
            continue;
        };
        // Decompressed + normalised at load (`norm_body`).
        if let Ok(text) = std::str::from_utf8(b) {
            per.entry(sid)
                .or_default()
                .entry(seq)
                .or_insert_with(|| text.to_string());
        }
    }
    per
}

/// Leaf paths + number shapes of a JSON value: `path → shape` where shape is
/// `int` / `float` / `str` / `bool` / `null` / `[]`.
fn shapes(v: &Value, path: &str, out: &mut BTreeMap<String, BTreeSet<&'static str>>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                shapes(x, &format!("{path}.{k}"), out);
            }
        }
        Value::Array(_) => {
            out.entry(path.to_string()).or_default().insert("[]");
        }
        Value::Number(n) => {
            let s = n.to_string();
            // `float-integral` (review-wp6): an f64 printed with a zero
            // fraction (`5.0`) — JSON.stringify never writes one, so it is
            // never a live-value int/float flip (always UNTOLERATED).
            let integral = s.contains('.')
                && s.split('.')
                    .nth(1)
                    .is_some_and(|f| f.bytes().all(|c| c == b'0'));
            out.entry(path.to_string())
                .or_default()
                .insert(if integral {
                    "float-integral"
                } else if s.contains(['.', 'e', 'E']) {
                    "float"
                } else {
                    "int"
                });
        }
        Value::String(_) => {
            out.entry(path.to_string()).or_default().insert("str");
        }
        Value::Bool(_) => {
            out.entry(path.to_string()).or_default().insert("bool");
        }
        Value::Null => {
            out.entry(path.to_string()).or_default().insert("null");
        }
    }
}

const T1_LIKE: &[&str] = &["ts", "mono", "startTs", "endTs", "createdAt", "updatedAt"];

/// The source of a fragment's session label: `<sid:video:cam1#0>` → `cam1`.
fn dir_label(sid: &str) -> String {
    sid.trim_start_matches("<sid:")
        .trim_start_matches("video:")
        .split('#')
        .next()
        .unwrap_or(sid)
        .to_string()
}

/// The camera a kvs master serves: the stem of its `KVS_GST_PIPELINE`
/// (`<work>/etc/pipelines/cam1.gst` → `cam1`).
fn kvs_camera(s: &Spawn) -> Option<String> {
    let p = s.env.get("KVS_GST_PIPELINE")?;
    let f = p.rsplit('/').next()?;
    Some(f.split('.').next()?.to_string())
}

/// The `(samples without system)` verdict of one telemetry session, `None` =
/// nothing to report. `ka`/`kb` = each side's implementation (`js`/`rust`).
/// - JS-vs-JS: `T13/system-presence` `perSession` (the TS's own spread).
/// - JS-vs-Rust: `T7/D30` (`d30_system_presence`) iff the Rust count is 0
///   and the JS count ≥ 1; a Rust count > 0 is UNTOLERATED even when the
///   counts agree (D30: Rust never drops — a drop is evidence against it).
/// - Rust-vs-Rust (and anything else): exact.
fn system_presence_tol(table: &Table, ka: &str, kb: &str, la: usize, lb: usize) -> Option<String> {
    match (ka, kb) {
        ("js", "js") => (la != lb).then(|| {
            let id = "T13/system-presence";
            let max = row_tol(table, id, "perSession", 0.0) as usize;
            if has_row(table, id) && la.abs_diff(lb) <= max {
                id.to_string()
            } else {
                UNTOLERATED.to_string()
            }
        }),
        ("js", "rust") | ("rust", "js") => {
            let (js, rs) = if ka == "js" { (la, lb) } else { (lb, la) };
            (la != lb || rs > 0).then(|| {
                let d30 = table.matcher_rows("d30_system_presence", "rec-telemetry");
                match d30.first() {
                    Some(id) if rs == 0 && js >= 1 => id.clone(),
                    _ => UNTOLERATED.to_string(),
                }
            })
        }
        _ => (la != lb).then(|| UNTOLERATED.to_string()),
    }
}

/// Flatten `metrics` into `path → JSON text` leaves (`metrics.gps.lat.value`).
fn leaf_texts(v: &Value, path: &str, out: &mut BTreeMap<String, String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                leaf_texts(x, &format!("{path}.{k}"), out);
            }
        }
        _ => {
            out.insert(path.to_string(), v.to_string());
        }
    }
}

/// One run-length-deduplicated value: `(JSON text, samples carrying it,
/// ts of its first sample)`.
type Step = (String, usize, i64);

/// Edit script of two step sequences by LCS on the values: `(hunk start in
/// x, only-in-x steps, only-in-y steps)` for every maximal non-equal stretch.
fn step_hunks(x: &[Step], y: &[Step]) -> Vec<(usize, Vec<Step>, Vec<Step>)> {
    let (n, m) = (x.len(), y.len());
    let mut l = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if x[i].0 == y[j].0 {
                l[i + 1][j + 1] + 1
            } else {
                l[i + 1][j].max(l[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out: Vec<(usize, Vec<Step>, Vec<Step>)> = Vec::new();
    let mut cur: Option<(usize, Vec<Step>, Vec<Step>)> = None;
    while i < n || j < m {
        if i < n && j < m && x[i].0 == y[j].0 {
            out.extend(cur.take());
            i += 1;
            j += 1;
        } else if j >= m || (i < n && l[i + 1][j] >= l[i][j + 1]) {
            cur.get_or_insert_with(|| (i, vec![], vec![]))
                .1
                .push(x[i].clone());
            i += 1;
        } else {
            cur.get_or_insert_with(|| (i, vec![], vec![]))
                .2
                .push(y[j].clone());
            j += 1;
        }
    }
    out.extend(cur);
    out
}

/// The tape record kinds that feed a telemetry group (T4's per-group list).
fn group_inputs(group: &str) -> &'static [&'static str] {
    match group {
        "gps" => &["nmea"],
        "imu" => &["imu", "imu_spawn", "imu_exit", "i2cdetect"],
        "lte" => &["lte_file"],
        "system" => &["exec", "dt"],
        _ => &[],
    }
}

/// Recorded telemetry VALUES (review-wp6 G1, phase file "telemetry segments …
/// sample `metrics` per T3/T4"), per telemetry session over each stretch of
/// consecutive segment seqs present on BOTH sides:
/// - every leaf but T3's host-read paths: the run-length-deduplicated value
///   sequence, exactly. Tolerated (`T4/rec-sample`) only a step carried by ONE
///   sample on one side and absent on the other, at the stretch's start/end
///   (±1 sample at a segment boundary), or where the side without it sampled
///   within `collisionMs` of a release of that group's input (its recorder
///   tick raced the input — the next tick shows the following value). A
///   stretch where both sides hold a step the other lacks (a different value)
///   is UNTOLERATED.
/// - per group (`system`: the `(samples without system)` check above), the
///   count of samples carrying it: |Δ| ≤ that group's input transitions in
///   the stretch ±1.5 s (T4, `InputFacts::groups`).
#[allow(clippy::too_many_arguments)]
fn rec_values(
    sid: &str,
    a: &BTreeMap<u64, String>,
    b: &BTreeMap<u64, String>,
    play: &PlayInfo,
    facts: &crate::diff::InputFacts,
    table: &Table,
    rep: &mut Report,
) {
    let id = "T4/rec-sample";
    let collision = row_tol(table, id, "collisionMs", 0.0) as i64;
    let has = has_row(table, id);
    let common: Vec<u64> = a.keys().filter(|k| b.contains_key(k)).copied().collect();
    let mut stretches: Vec<Vec<u64>> = Vec::new();
    for q in common {
        match stretches.last_mut() {
            Some(s) if s.last().is_some_and(|l| l + 1 == q) => s.push(q),
            _ => stretches.push(vec![q]),
        }
    }
    let samples = |segs: &BTreeMap<u64, String>, st: &[u64]| -> Vec<Value> {
        st.iter()
            .flat_map(|q| segs[q].lines())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|x| x["type"] != "header" && x["metrics"].is_object())
            .collect()
    };
    let walls = |group: &str| -> Vec<i64> {
        let ks = group_inputs(group);
        play.releases
            .iter()
            .filter(|r| ks.contains(&r.k.as_str()))
            .map(|r| r.w)
            .collect()
    };
    for st in stretches {
        let at = format!("{sid}/segs{}-{}", st[0], st[st.len() - 1]);
        let (xa, xb) = (samples(a, &st), samples(b, &st));
        let leaves = |xs: &[Value]| -> Vec<(i64, BTreeMap<String, String>)> {
            xs.iter()
                .map(|x| {
                    let mut o = BTreeMap::new();
                    leaf_texts(&x["metrics"], "metrics", &mut o);
                    (x["ts"].as_i64().unwrap_or(0), o)
                })
                .collect()
        };
        let (la, lb) = (leaves(&xa), leaves(&xb));
        let paths: BTreeSet<&String> = la
            .iter()
            .chain(lb.iter())
            .flat_map(|(_, o)| o.keys())
            .collect();
        let steps = |l: &[(i64, BTreeMap<String, String>)], p: &str| -> Vec<Step> {
            let mut v: Vec<Step> = Vec::new();
            for (ts, o) in l {
                if let Some(t) = o.get(p) {
                    match v.last_mut() {
                        Some(s) if s.0 == *t => s.1 += 1,
                        _ => v.push((t.clone(), 1, *ts)),
                    }
                }
            }
            v
        };
        for p in paths {
            if table
                .numeric("telemetry", p)
                .is_some_and(|(r, _)| r == "T3")
            {
                continue; // host-read (T3): the live stream compares them
            }
            stat(rep, "rec-values").compared += 1;
            let (sa, sb) = (steps(&la, p), steps(&lb, p));
            if sa.iter().map(|s| &s.0).eq(sb.iter().map(|s| &s.0)) {
                continue;
            }
            let group = p.split('.').nth(1).unwrap_or("");
            let rel = walls(group);
            let raced = |l: &[(i64, BTreeMap<String, String>)], ts: i64| {
                l.iter()
                    .filter(|(t, _)| (t - ts).abs() <= 1_500)
                    .any(|(t, _)| rel.iter().any(|w| (t - w).abs() <= collision))
            };
            for (start, only_a, only_b) in step_hunks(&sa, &sb) {
                let edge = start == 0 || start + only_a.len() >= sa.len();
                let one_sided = only_a.is_empty() != only_b.is_empty();
                let ok = has
                    && one_sided
                    && only_a.iter().chain(only_b.iter()).all(|s| s.1 == 1)
                    && (if edge {
                        only_a.len() + only_b.len() == 1
                    } else if only_b.is_empty() {
                        only_a.iter().all(|s| raced(&lb, s.2))
                    } else {
                        only_b.iter().all(|s| raced(&la, s.2))
                    });
                let fmt = |v: &[Step]| {
                    v.iter()
                        .map(|s| format!("{}×{}@{}", s.0, s.1, s.2))
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                rep.push(mm(
                    "rec-telemetry",
                    &at,
                    &format!("{p}(values)"),
                    fmt(&only_a),
                    fmt(&only_b),
                    if ok { id } else { UNTOLERATED },
                ));
            }
        }
        // Per-group presence (not `system`: judged per sample above).
        let (lo, hi) = (
            la.first()
                .map(|x| x.0)
                .unwrap_or(0)
                .min(lb.first().map(|x| x.0).unwrap_or(0))
                - 1_500,
            la.last()
                .map(|x| x.0)
                .unwrap_or(0)
                .max(lb.last().map(|x| x.0).unwrap_or(0))
                + 1_500,
        );
        let groups: BTreeSet<String> = xa
            .iter()
            .chain(xb.iter())
            .filter_map(|x| x["metrics"].as_object())
            .flat_map(|m| m.keys().cloned())
            .filter(|g| g != "system")
            .collect();
        for g in groups {
            stat(rep, "rec-presence").compared += 1;
            let n = |xs: &[Value]| xs.iter().filter(|x| x["metrics"].get(&g).is_some()).count();
            let (na, nb) = (n(&xa), n(&xb));
            if na == nb {
                continue;
            }
            let trans = facts
                .groups
                .get(&g)
                .map_or(0, |v| v.iter().filter(|w| **w >= lo && **w <= hi).count());
            rep.push(mm(
                "rec-telemetry",
                &at,
                &format!("metrics.{g}(samples carrying)"),
                na.to_string(),
                nb.to_string(),
                if na.abs_diff(nb) <= trans {
                    "T4"
                } else {
                    UNTOLERATED
                },
            ));
        }
    }
}

/// T7/D30 `d30_shed_tick_session_end` (review-wp6 §3) for the sidecar `key`
/// (`(session, seq)`), every condition but (e) (the caller's):
/// (a) `seq` is the LAST fragment of the session on BOTH sides, and on each
///     side the session's capture child ended at the agent's stop (`int` /
///     `eos`, no later capture spawn of that camera);
/// (b) δ = first-fragment `startTs` JS − Rust in [850, 1150] ms (JS LATER;
///     JS-vs-JS |δ| in that band; Rust-vs-Rust never);
/// (c) the end instants (`startTs + durationMs` of this fragment) agree
///     within `end_ms` (T12's durationMs bound: one SIGTERM cuts both);
/// (d) on each side the session's capture spawn lags that side's last
///     `recording.powerShed`/`liveBlock` change (telemetry, publish `w`, at
///     most one tick after the spawn) by the same ±300 ms.
#[allow(clippy::too_many_arguments)]
fn d30_session_end(
    a: &MediaCapture,
    b: &MediaCapture,
    ca: &BTreeMap<(String, u64), Value>,
    cb: &BTreeMap<(String, u64), Value>,
    fa: &BTreeMap<String, BTreeMap<u64, u64>>,
    fb: &BTreeMap<String, BTreeMap<u64, u64>>,
    key: &(String, u64),
    end_ms: f64,
) -> bool {
    const DELTA: (f64, f64) = (850.0, 1150.0);
    const LAG_MS: i64 = 300;
    /// A telemetry tick publishes a shed change at most ~1 tick after it.
    const TICK_MS: i64 = 1_100;
    let (sid, seq) = (&key.0, key.1);
    let last = |f: &BTreeMap<String, BTreeMap<u64, u64>>, c: &BTreeMap<(String, u64), Value>| {
        let fl = f.get(sid).and_then(|m| m.keys().next_back().copied());
        let cl = c
            .range((sid.clone(), 0)..=(sid.clone(), u64::MAX))
            .next_back()
            .map(|((_, q), _)| *q);
        fl.max(cl) == Some(seq)
    };
    if !last(fa, ca) || !last(fb, cb) {
        return false;
    }
    // The session's capture spawn, ended at the stop, never respawned.
    let spawn_at = |m: &MediaCapture| -> Option<i64> {
        let s = m
            .spawns
            .iter()
            .find(|s| s.role == "capture" && s.sid.as_deref() == Some(sid.as_str()))?;
        let at_stop = matches!(s.end.as_deref(), Some("int") | Some("eos"));
        let respawned = m
            .spawns
            .iter()
            .any(|o| o.role == "capture" && o.key == s.key && o.w > s.w);
        (at_stop && !respawned).then_some(s.w)
    };
    let (Some(wa), Some(wb)) = (spawn_at(a), spawn_at(b)) else {
        return false;
    };
    let f64_at = |c: &BTreeMap<(String, u64), Value>, q: u64, k: &str| {
        c.get(&(sid.clone(), q)).and_then(|v| v[k].as_f64())
    };
    let (Some(s0a), Some(s0b)) = (f64_at(ca, 0, "startTs"), f64_at(cb, 0, "startTs")) else {
        return false;
    };
    let delta = match (a.kind.as_str(), b.kind.as_str()) {
        ("js", "rust") => s0a - s0b,
        ("rust", "js") => s0b - s0a,
        ("js", "js") => (s0a - s0b).abs(),
        _ => return false,
    };
    if !(DELTA.0..=DELTA.1).contains(&delta) {
        return false;
    }
    let end = |c: &BTreeMap<(String, u64), Value>| {
        Some(f64_at(c, seq, "startTs")? + f64_at(c, seq, "durationMs")?)
    };
    let (Some(ea), Some(eb)) = (end(ca), end(cb)) else {
        return false;
    };
    if (ea - eb).abs() > end_ms {
        return false;
    }
    let lag = |m: &MediaCapture, w: i64| {
        m.shed_changes
            .iter()
            .rfind(|c| **c <= w + TICK_MS)
            .map(|c| w - c)
    };
    matches!((lag(a, wa), lag(b, wb)), (Some(x), Some(y)) if (x - y).abs() <= LAG_MS)
}

/// The Phase 3 comparison. `a`/`b` absent of any media artifact on BOTH
/// sides = nothing to compare (a Phase 2 tape): the streams are listed with 0.
pub fn diff_media(
    a: &MediaCapture,
    b: &MediaCapture,
    play: &PlayInfo,
    table: &Table,
    rep: &mut Report,
) {
    for k in [
        "children",
        "session",
        "sidecar",
        "mp4",
        "rec-telemetry",
        "s3",
        "file:counters.json",
        "evictions",
        "shm",
    ] {
        stat(rep, k);
    }
    if !a.present && !b.present {
        return;
    }
    let t10 = if has_row(table, "T10") {
        "T10"
    } else {
        UNTOLERATED
    };
    let t12 = if has_row(table, "T12") {
        "T12"
    } else {
        UNTOLERATED
    };
    let t13 = if has_row(table, "T13") {
        "T13"
    } else {
        UNTOLERATED
    };
    let t14 = if has_row(table, "T14") {
        "T14"
    } else {
        UNTOLERATED
    };

    // ---- children
    let spawn_ms = row_tol(table, "T10", "spawnMs", 1000.0) as i64;
    let flash_ms = row_tol(table, "T10", "flashMs", 0.0) as i64;
    let groups = |m: &MediaCapture| {
        let mut g: BTreeMap<(String, String), Vec<Spawn>> = BTreeMap::new();
        for s in &m.spawns {
            g.entry((s.role.clone(), s.key.clone()))
                .or_default()
                .push(s.clone());
        }
        g
    };
    let (ga, gb) = (groups(a), groups(b));
    let keys: BTreeSet<&(String, String)> = ga.keys().chain(gb.keys()).collect();
    for key in keys {
        let at = format!("{}/{}", key.0, key.1);
        let (mut xa, mut xb) = (
            ga.get(key).cloned().unwrap_or_default(),
            gb.get(key).cloned().unwrap_or_default(),
        );
        if xa.len() != xb.len() {
            // T10 `flash`: a kvs master SIGINT-stopped within `flashMs` of
            // its own spawn is the capture-spawn bounce (worker.ts:455
            // onCaptureSpawned → media.ts:51-59 restartChannel) landing in
            // the same instant as the master's (re)start — whether the
            // release or the capture spawn runs first is a race inside ONE
            // agent (JS-vs-JS rec-cameras: 4 vs 3). Dropped from BOTH sides
            // only when that makes the counts equal; the rest compare as usual.
            // NARROWED (review-wp6): a flash counts only when a capture of
            // the SAME camera (the master's KVS_GST_PIPELINE `camN.gst`)
            // spawned within ±flashMs on that side, one flash per capture
            // spawn — a master double-bounced for any other reason is not one.
            let flashes = |m: &MediaCapture, xs: &[Spawn]| -> BTreeSet<u32> {
                let mut used: BTreeSet<u32> = BTreeSet::new();
                let mut out = BTreeSet::new();
                for s in xs {
                    let quick = s.role == "kvs"
                        && s.end.as_deref() == Some("int")
                        && s.end_w.is_some_and(|e| e - s.w <= flash_ms);
                    let Some(cam) = quick.then(|| kvs_camera(s)).flatten() else {
                        continue;
                    };
                    if let Some(c) = m.spawns.iter().find(|c| {
                        c.role == "capture"
                            && c.key == cam
                            && (c.w - s.w).abs() <= flash_ms
                            && !used.contains(&c.pid)
                    }) {
                        used.insert(c.pid);
                        out.insert(s.pid);
                    }
                }
                out
            };
            let (pa_f, pb_f) = (flashes(a, &xa), flashes(b, &xb));
            let (fa, fb): (Vec<Spawn>, Vec<Spawn>) = (
                xa.iter()
                    .filter(|s| !pa_f.contains(&s.pid))
                    .cloned()
                    .collect(),
                xb.iter()
                    .filter(|s| !pb_f.contains(&s.pid))
                    .cloned()
                    .collect(),
            );
            let tol = if fa.len() == fb.len() {
                xa = fa;
                xb = fb;
                t10
            } else {
                UNTOLERATED
            };
            rep.push(mm(
                "children",
                &at,
                "(spawn count)",
                ga.get(key).map_or(0, Vec::len).to_string(),
                gb.get(key).map_or(0, Vec::len).to_string(),
                tol,
            ));
        }
        for (i, (sa, sb)) in xa.iter().zip(xb.iter()).enumerate() {
            stat(rep, "children").compared += 1;
            let at = format!("{at}#{i}");
            if sa.argv != sb.argv {
                rep.push(mm(
                    "children",
                    &at,
                    "argv",
                    sa.argv.join(" "),
                    sb.argv.join(" "),
                    UNTOLERATED,
                ));
            }
            if sa.env != sb.env {
                rep.push(mm(
                    "children",
                    &at,
                    "env",
                    format!("{:?}", sa.env),
                    format!("{:?}", sb.env),
                    UNTOLERATED,
                ));
            }
            if sa.end != sb.end {
                rep.push(mm(
                    "children",
                    &at,
                    "(ended)",
                    format!("{:?}", sa.end),
                    format!("{:?}", sb.end),
                    UNTOLERATED,
                ));
            }
            let (da, db) = (sa.w - play.start_wall, sb.w - play.start_wall);
            if da != db {
                let tol = if (da - db).abs() <= spawn_ms {
                    t10
                } else {
                    UNTOLERATED
                };
                rep.push(mm(
                    "children",
                    &at,
                    "(spawn ms after start)",
                    da.to_string(),
                    db.to_string(),
                    tol,
                ));
            }
        }
    }
    for (label, m) in [("A", a), ("B", b)] {
        let firsts: Vec<i64> = ["cam0", "cam1"]
            .iter()
            .filter_map(|k| {
                m.spawns
                    .iter()
                    .find(|s| s.role == "capture" && s.key == *k)
                    .map(|s| s.w)
            })
            .collect();
        if let [x, y] = firsts[..] {
            let gap = (y - x).abs();
            rep.inputs.insert(
                format!("stagger {label}"),
                format!("{gap} ms between cam0/cam1 first spawns"),
            );
            if gap < STAGGER_MS - 50 {
                rep.push(mm(
                    "children",
                    label,
                    "(stagger)",
                    format!("{gap} ms"),
                    format!("≥ {STAGGER_MS} ms"),
                    UNTOLERATED,
                ));
            }
        }
    }

    // ---- session.json
    let t11_paths: Vec<String> = table
        .rules
        .iter()
        .filter(|r| r.id == "T11")
        .flat_map(|r| r.paths.clone())
        .collect();
    let sess = |m: &MediaCapture| -> BTreeMap<String, Value> {
        m.finals
            .iter()
            .filter(|(k, _)| k.ends_with("/session.json"))
            .filter_map(|(k, b)| Some((k.clone(), serde_json::from_slice(b).ok()?)))
            .collect()
    };
    let (sa, sb) = (sess(a), sess(b));
    for k in sa.keys().chain(sb.keys()).collect::<BTreeSet<_>>() {
        match (sa.get(k), sb.get(k)) {
            (Some(x), Some(y)) => {
                stat(rep, "session").compared += 1;
                let skip = |p: &str| t11_paths.iter().any(|q| q == p).then(|| "T11".to_string());
                json_cmp("session", k, "", x, y, &skip, rep);
            }
            (x, y) => rep.push(mm(
                "session",
                k,
                "(file)",
                x.is_some().to_string(),
                y.is_some().to_string(),
                UNTOLERATED,
            )),
        }
    }

    // Fragments per session (mp4 stream below; the D30 last-seq test).
    let frags = |m: &MediaCapture| -> BTreeMap<String, BTreeMap<u64, u64>> {
        let mut out: BTreeMap<String, BTreeMap<u64, u64>> = BTreeMap::new();
        for (k, s) in &m.rec {
            if let Some((sid, seq, "mp4")) = frag_of(k) {
                out.entry(sid).or_default().insert(seq, s.size);
            }
        }
        for o in &m.s3 {
            if let Some((sid, seq, "mp4")) = frag_of(&o.key) {
                out.entry(sid).or_default().entry(seq).or_insert(0);
            }
        }
        for (k, size) in &m.mp4_final {
            if let Some((sid, seq, "mp4")) = frag_of(k) {
                out.entry(sid).or_default().insert(seq, *size);
            }
        }
        out
    };
    let (fa, fb) = (frags(a), frags(b));

    // ---- sidecars (T12) + D8 lag figures
    let (ca, cb) = (sidecars(a), sidecars(b));
    let start_ms = row_tol(table, "T12", "startTs", 300.0);
    let mono_ms = row_tol(table, "T12", "startMono", 300.0);
    let dur_ms = row_tol(table, "T12", "durationMs", 150.0);
    let playing = |m: &MediaCapture, sid: &str| {
        m.spawns
            .iter()
            .find(|s| s.sid.as_deref() == Some(sid))
            .and_then(|s| s.playing)
    };
    let mut lag: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    for (side, (m, c)) in [(a, &ca), (b, &cb)].into_iter().enumerate() {
        for ((sid, seq), v) in c {
            if *seq == 0 {
                if let (Some(p), Some(st)) = (playing(m, sid), v["startTs"].as_f64()) {
                    lag[side].push(st - p as f64);
                }
            }
        }
    }
    let med = |v: &Vec<f64>| {
        let mut v = v.clone();
        v.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        v.get(v.len() / 2).copied()
    };
    rep.inputs.insert(
        "D8 rig: fragment-0 startTs − PLAYING (median ms)".into(),
        format!(
            "A {:?} ({} sessions), B {:?} ({} sessions)",
            med(&lag[0]),
            lag[0].len(),
            med(&lag[1]),
            lag[1].len()
        ),
    );
    let base_mono = |c: &BTreeMap<(String, u64), Value>, sid: &str| {
        c.get(&(sid.to_string(), 0))
            .and_then(|v| v["startMono"].as_f64())
    };
    // T7/D30 `d30_shed_tick_session_end` (review-wp6 §3): the sessions
    // whose earlier fragments left an UNTOLERATED sidecar row (condition (e)).
    let d30_end = table
        .matcher_rows("d30_shed_tick_session_end", "sidecar")
        .into_iter()
        .next();
    let mut sid_failed: BTreeSet<String> = BTreeSet::new();
    for key in ca.keys().chain(cb.keys()).collect::<BTreeSet<_>>() {
        let at = format!("{}/{:05}", key.0, key.1);
        let (Some(x), Some(y)) = (ca.get(key), cb.get(key)) else {
            continue; // presence: the mp4/s3 streams judge it (±1 at the cut)
        };
        stat(rep, "sidecar").compared += 1;
        let (pa, pb) = (playing(a, &key.0), playing(b, &key.0));
        let d30_ok = d30_end.is_some()
            && !sid_failed.contains(&key.0)
            && d30_session_end(a, b, &ca, &cb, &fa, &fb, key, dur_ms);
        let skip = |p: &str| -> Option<String> {
            match p {
                "sizeBytes" => Some(t12.to_string()),
                "startTs" => {
                    let (sx, sy) = (x["startTs"].as_f64()?, y["startTs"].as_f64()?);
                    let (rx, ry) = (sx - pa? as f64, sy - pb? as f64);
                    ((rx - ry).abs() <= start_ms).then(|| t12.to_string())
                }
                "startMono" => {
                    let rx = x["startMono"].as_f64()? - base_mono(&ca, &key.0)?;
                    let ry = y["startMono"].as_f64()? - base_mono(&cb, &key.0)?;
                    ((rx - ry).abs() <= mono_ms).then(|| t12.to_string())
                }
                "durationMs" => {
                    let d = (x["durationMs"].as_f64()? - y["durationMs"].as_f64()?).abs();
                    if d <= dur_ms {
                        Some(t12.to_string())
                    } else if d30_ok {
                        d30_end.clone()
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        let before = rep.untolerated();
        json_cmp("sidecar", &at, "", x, y, &skip, rep);
        if rep.untolerated() > before {
            sid_failed.insert(key.0.clone());
        }
    }

    // ---- mp4: contiguity + zero-byte per side, count ±1 (T12)
    for (label, f, m) in [("A", &fa, a), ("B", &fb, b)] {
        for (sid, seqs) in f {
            let n = seqs.len() as u64;
            let last = seqs.keys().next_back().copied().unwrap_or(0);
            if last + 1 != n {
                rep.push(mm(
                    "mp4",
                    &format!("{label} {sid}"),
                    "(contiguity)",
                    format!("{:?}", seqs.keys().collect::<Vec<_>>()),
                    "0..n".into(),
                    UNTOLERATED,
                ));
            }
            // 0-byte CLOSED fragment (not the newest): only from sizes we saw
            // locally (an uploaded-and-deleted one is judged by its PUT size).
            for (seq, size) in seqs {
                let put_ok = m.s3.iter().any(|o| {
                    o.status == 200 && frag_of(&o.key) == Some((sid.clone(), *seq, "mp4"))
                });
                if *seq != last && *size == 0 && !put_ok {
                    rep.push(mm(
                        "mp4",
                        &format!("{label} {sid}/{seq:05}"),
                        "(0-byte closed fragment)",
                        "0".into(),
                        "> 0".into(),
                        UNTOLERATED,
                    ));
                }
            }
        }
    }
    for sid in fa.keys().chain(fb.keys()).collect::<BTreeSet<_>>() {
        stat(rep, "mp4").compared += 1;
        let (na, nb) = (
            fa.get(sid).map_or(0, BTreeMap::len) as i64,
            fb.get(sid).map_or(0, BTreeMap::len) as i64,
        );
        if na != nb {
            let tol = if (na - nb).abs() <= 1 && na > 0 && nb > 0 {
                t12
            } else {
                UNTOLERATED
            };
            rep.push(mm(
                "mp4",
                sid,
                "(fragment count)",
                na.to_string(),
                nb.to_string(),
                tol,
            ));
        }
    }

    // ---- telemetry segments (T13; gz bytes never compared — D24)
    let (ta, tb) = (segments(a), segments(b));
    let t1: BTreeSet<&str> = T1_LIKE.iter().copied().collect();
    for sid in ta.keys().chain(tb.keys()).collect::<BTreeSet<_>>() {
        let (xa, xb) = (
            ta.get(sid).cloned().unwrap_or_default(),
            tb.get(sid).cloned().unwrap_or_default(),
        );
        if xa.len() != xb.len() {
            let tol = if (xa.len() as i64 - xb.len() as i64).abs() <= 1 {
                t13
            } else {
                UNTOLERATED
            };
            rep.push(mm(
                "rec-telemetry",
                sid,
                "(segment count)",
                xa.len().to_string(),
                xb.len().to_string(),
                tol,
            ));
        }
        let mut lack = (0usize, 0usize);
        for (i, (sa, sb)) in xa.iter().zip(xb.iter()).enumerate() {
            stat(rep, "rec-telemetry").compared += 1;
            let at = format!("{sid}/seg#{i}");
            let parse = |t: &str| -> Vec<Value> {
                t.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            };
            let (la, lb) = (parse(sa), parse(sb));
            let headers = |v: &[Value]| -> Vec<Value> {
                v.iter()
                    .filter(|x| x["type"] == "header")
                    .cloned()
                    .collect()
            };
            let (ha, hb) = (headers(&la), headers(&lb));
            if ha.len() != hb.len() {
                rep.push(mm(
                    "rec-telemetry",
                    &at,
                    "(header lines)",
                    ha.len().to_string(),
                    hb.len().to_string(),
                    UNTOLERATED,
                ));
            }
            for (x, y) in ha.iter().zip(hb.iter()) {
                let skip = |p: &str| -> Option<String> {
                    // json_cmp paths start at `header.`; T11 names the field.
                    let p = p.strip_prefix("header.").unwrap_or(p);
                    if t11_paths.iter().any(|q| q == p) {
                        Some("T11".into())
                    } else if t1.contains(p) || p.ends_with("At") || p.ends_with("Ts") {
                        Some("T1".into())
                    } else {
                        None
                    }
                };
                json_cmp("rec-telemetry", &at, "header", x, y, &skip, rep);
            }
            let samples = |v: &[Value]| -> Vec<Value> {
                v.iter()
                    .filter(|x| x["type"] != "header")
                    .cloned()
                    .collect()
            };
            let (pa, pb) = (samples(&la), samples(&lb));
            let order = |v: &[Value]| -> BTreeSet<Vec<String>> {
                v.iter()
                    .filter_map(|x| x.as_object().map(|o| o.keys().cloned().collect()))
                    .collect()
            };
            if order(&pa) != order(&pb) {
                rep.push(mm(
                    "rec-telemetry",
                    &at,
                    "(sample key order)",
                    format!("{:?}", order(&pa)),
                    format!("{:?}", order(&pb)),
                    UNTOLERATED,
                ));
            }
            let sh = |v: &[Value]| {
                let mut out = BTreeMap::new();
                for x in v {
                    shapes(&x["metrics"], "metrics", &mut out);
                }
                out
            };
            let (sha, shb) = (sh(&pa), sh(&pb));
            for p in sha.keys().chain(shb.keys()).collect::<BTreeSet<_>>() {
                if sha.get(p) != shb.get(p) {
                    // A value read LIVE from the rig host (a T3 numeric path:
                    // cpu/ram/disk/temp) is integral on some ticks — both
                    // runtimes then print it as an int (JsNumber = JS) — so
                    // int-vs-float there follows the value, not the format.
                    // Tape-fed numbers keep the strict T9 shape check.
                    let num_only = |o: Option<&BTreeSet<&'static str>>| {
                        o.is_some_and(|s| s.iter().all(|k| *k == "int" || *k == "float"))
                    };
                    let tol = match table.numeric("telemetry", p) {
                        Some((id, _)) if num_only(sha.get(p)) && num_only(shb.get(p)) => id,
                        _ => UNTOLERATED.to_string(),
                    };
                    rep.push(mm(
                        "rec-telemetry",
                        &at,
                        &format!("{p}(shape)"),
                        format!("{:?}", sha.get(p)),
                        format!("{:?}", shb.get(p)),
                        &tol,
                    ));
                }
            }
            // Per-SAMPLE `system` presence, summed over the session below.
            let lacking = |v: &[Value]| {
                v.iter()
                    .filter(|x| x["metrics"].is_object() && x["metrics"].get("system").is_none())
                    .count()
            };
            let carries = |v: &[Value]| v.iter().any(|x| x["metrics"].get("system").is_some());
            if carries(&pa) || carries(&pb) {
                lack.0 += lacking(&pa);
                lack.1 += lacking(&pb);
            }
            rep.inputs
                .entry(format!("rec-telemetry samples {sid}"))
                .or_default()
                .push_str(&format!("[{}:{}]", pa.len(), pb.len()));
        }
        // Per-SAMPLE `system` presence over the session's paired segments
        // (rig fix 2026-10-19). The per-segment group union above let a
        // recorder that drops `system` on ~8 % of ticks pass unseen.
        // `system` is sampled by the agent itself (system.ts:104-113: a 1 s
        // recompute gate shared by the recorder and the shed tick;
        // telemetry-recorder.ts:102 skips a group not refreshed since its
        // last written sample), so the count is the agent's timer
        // behaviour, not a tape input. Judged per implementation pair
        // (review-wp6 §3): JS-vs-JS `T13/system-presence` `perSession`
        // (calibrations: max |Δ| 2); JS-vs-Rust the D30 matcher only (Rust
        // never drops: any Rust drop is UNTOLERATED, even when the counts
        // agree); Rust-vs-Rust exact.
        if let Some(tol) = system_presence_tol(table, &a.kind, &b.kind, lack.0, lack.1) {
            rep.push(mm(
                "rec-telemetry",
                sid,
                "(samples without system)",
                lack.0.to_string(),
                lack.1.to_string(),
                &tol,
            ));
        }
    }

    // ---- recorded telemetry VALUES + per-group presence (G1)
    let facts = crate::diff::InputFacts::for_play(play);
    let (va, vb) = (segments_by_seq(a), segments_by_seq(b));
    stat(rep, "rec-values");
    stat(rep, "rec-presence");
    for sid in va.keys().filter(|k| vb.contains_key(*k)) {
        rec_values(sid, &va[sid], &vb[sid], play, &facts, table, rep);
    }

    // ---- S3 (T14)
    for (label, m) in [("A", a), ("B", b)] {
        for o in &m.s3 {
            if o.sig != "ok" {
                rep.push(mm(
                    "s3",
                    &format!("{label} {}", o.key),
                    "(SigV4)",
                    o.sig.clone(),
                    "ok".into(),
                    UNTOLERATED,
                ));
            }
        }
        // Per fragment: the sidecar's successful PUT precedes the mp4's.
        let ok: Vec<&S3Obs> = m.s3.iter().filter(|o| o.status == 200).collect();
        for (i, o) in ok.iter().enumerate() {
            if let Some((sid, seq, "mp4")) = frag_of(&o.key) {
                let json_before = ok[..i]
                    .iter()
                    .any(|p| frag_of(&p.key) == Some((sid.clone(), seq, "json")));
                if !json_before {
                    rep.push(mm(
                        "s3",
                        &format!("{label} {}", o.key),
                        "(sidecar before mp4)",
                        "mp4 first".into(),
                        "json first".into(),
                        UNTOLERATED,
                    ));
                }
            }
        }
    }
    let okset = |m: &MediaCapture| -> BTreeMap<String, String> {
        m.s3.iter()
            .filter(|o| o.status == 200)
            .map(|o| (o.key.clone(), o.ctype.clone()))
            .collect()
    };
    let (oa, ob) = (okset(a), okset(b));
    let last_frag = |f: &BTreeMap<String, BTreeMap<u64, u64>>, sid: &str, seq: u64| {
        f.get(sid)
            .and_then(|s| s.keys().next_back())
            .is_some_and(|l| *l == seq)
    };
    let (sa_tel, sb_tel) = (tele_segs(a), tele_segs(b));
    let last_seg = |t: &BTreeMap<String, BTreeSet<u64>>, sid: &str, seq: u64| {
        t.get(sid)
            .and_then(|s| s.iter().next_back())
            .is_some_and(|l| *l == seq)
    };
    for k in oa.keys().chain(ob.keys()).collect::<BTreeSet<_>>() {
        stat(rep, "s3").compared += 1;
        match (oa.get(k), ob.get(k)) {
            (Some(x), Some(y)) if x != y => rep.push(mm(
                "s3",
                k,
                "contentType",
                x.clone(),
                y.clone(),
                UNTOLERATED,
            )),
            (Some(_), Some(_)) => {}
            (x, y) => {
                // T14: only a session's LAST piece may be one-sided — the
                // newest fragment, or the newest telemetry segment (rotated at
                // or after the cut), of that session on EITHER side. A middle
                // segment PUT on one side only is a lost upload (review-wp6
                // must-fix 1).
                let at_cut = frag_of(k).is_some_and(|(sid, seq, _)| {
                    last_frag(&fa, &sid, seq) || last_frag(&fb, &sid, seq)
                }) || tele_seg_of(k).is_some_and(|(sid, seq)| {
                    // …and the side WITHOUT the PUT holds that segment (its
                    // upload cut short) or never got that far: a side that
                    // skipped it and wrote a newer one lost it.
                    let missing = if x.is_some() { &sb_tel } else { &sa_tel };
                    let reached = missing.get(&sid).is_some_and(|s| {
                        s.contains(&seq) || s.iter().next_back().is_some_and(|l| *l < seq)
                    }) || !missing.contains_key(&sid);
                    (last_seg(&sa_tel, &sid, seq) || last_seg(&sb_tel, &sid, seq)) && reached
                });
                let tol = if at_cut { t14 } else { UNTOLERATED };
                rep.push(mm(
                    "s3",
                    k,
                    "(PUT)",
                    x.is_some().to_string(),
                    y.is_some().to_string(),
                    tol,
                ));
            }
        }
    }
    let attempts = |m: &MediaCapture| m.s3.iter().filter(|o| o.status != 200).count();
    rep.inputs.insert(
        "s3 PUTs (ok / failed)".into(),
        format!(
            "A {}/{}, B {}/{}",
            oa.len(),
            attempts(a),
            ob.len(),
            attempts(b)
        ),
    );
    if attempts(a) != attempts(b) {
        // NARROWED (review-wp6): |Δ| ≤ 1 per `meta.rig.s3Faults` window edge
        // (a PUT straddling the open/close); no fault window → exact.
        let edges = xorgate_hal::tape::Tape::read(Path::new(&play.tape))
            .map(|t| 2 * crate::aws::faults_of(t.meta.rig.as_ref()).len())
            .unwrap_or(0);
        rep.push(mm(
            "s3",
            "run",
            "(failed attempts)",
            attempts(a).to_string(),
            attempts(b).to_string(),
            if attempts(a).abs_diff(attempts(b)) <= edges {
                t14
            } else {
                UNTOLERATED
            },
        ));
    }
    // S3 ORDER (review-wp6, TS uploader.ts:7-8,39,91: by source priority,
    // then oldest-first within a source): per side each source's first
    // successful PUTs run monotonic in (session, seq); across sides the
    // per-source order of the keys BOTH sides PUT is equal.
    let order = |m: &MediaCapture| -> BTreeMap<String, Vec<(String, u64)>> {
        let mut out: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for o in m.s3.iter().filter(|o| o.status == 200) {
            let (src, sid, seq) = if let Some((sid, seq, _)) = frag_of(&o.key) {
                (dir_label(&sid), sid, seq)
            } else if let Some((sid, seq)) = tele_seg_of(&o.key) {
                ("telemetry".to_string(), sid, seq)
            } else {
                continue;
            };
            if seen.insert((sid.clone(), seq)) {
                out.entry(src).or_default().push((sid, seq));
            }
        }
        out
    };
    let (qa, qb) = (order(a), order(b));
    for (label, q) in [("A", &qa), ("B", &qb)] {
        for (src, v) in q {
            if let Some(w) = v.windows(2).find(|w| w[1] < w[0]) {
                rep.push(mm(
                    "s3",
                    &format!("{label} {src}"),
                    "(PUT order)",
                    format!("{:?} after {:?}", w[1], w[0]),
                    "oldest first".into(),
                    UNTOLERATED,
                ));
            }
        }
    }
    for src in qa.keys().filter(|k| qb.contains_key(*k)) {
        let both = |v: &Vec<(String, u64)>, o: &Vec<(String, u64)>| -> Vec<(String, u64)> {
            v.iter().filter(|x| o.contains(x)).cloned().collect()
        };
        let (x, y) = (both(&qa[src], &qb[src]), both(&qb[src], &qa[src]));
        if x != y {
            rep.push(mm(
                "s3",
                src,
                "(PUT order A vs B)",
                format!("{x:?}"),
                format!("{y:?}"),
                UNTOLERATED,
            ));
        }
    }

    // ---- counters.json
    let counters = |m: &MediaCapture| m.finals.get("counters.json").cloned();
    match (counters(a), counters(b)) {
        (None, None) => stat(rep, "file:counters.json").absent_both += 1,
        (Some(x), Some(y)) => {
            stat(rep, "file:counters.json").compared += 1;
            let (vx, vy) = (
                serde_json::from_slice::<Value>(&x).unwrap_or(Value::Null),
                serde_json::from_slice::<Value>(&y).unwrap_or(Value::Null),
            );
            json_cmp("file:counters.json", "end", "", &vx, &vy, &|_| None, rep);
        }
        (x, y) => rep.push(mm(
            "file:counters.json",
            "end",
            "(file)",
            x.is_some().to_string(),
            y.is_some().to_string(),
            UNTOLERATED,
        )),
    }

    // ---- evictions: fragments/segments gone without a successful PUT
    let evicted = |m: &MediaCapture| -> Vec<String> {
        let put: BTreeSet<String> =
            m.s3.iter()
                .filter(|o| o.status == 200)
                .filter_map(|o| frag_of(&o.key).map(|(s, q, e)| format!("{s}/{q:05}.{e}")))
                .collect();
        let mut v: Vec<(i64, String)> = m
            .rec
            .iter()
            .filter_map(|(k, s)| {
                let (sid, seq, ext) = frag_of(k)?;
                let id = format!("{sid}/{seq:05}.{ext}");
                (s.gone.is_some() && !put.contains(&id)).then(|| (s.gone.unwrap_or(0), id))
            })
            .collect();
        v.sort();
        v.into_iter().map(|(_, k)| k).collect()
    };
    let (ea, eb) = (evicted(a), evicted(b));
    stat(rep, "evictions").compared += 1;
    rep.inputs.insert(
        "evictions".into(),
        format!("A {} files, B {} files", ea.len(), eb.len()),
    );
    let set = |v: &[String]| v.iter().cloned().collect::<BTreeSet<_>>();
    if set(&ea) != set(&eb) {
        rep.push(mm(
            "evictions",
            "run",
            "(evicted set)",
            format!("{ea:?}"),
            format!("{eb:?}"),
            UNTOLERATED,
        ));
    } else if ea != eb {
        // Same set, different order within one 1 s poll is invisible; a real
        // order difference across polls is a FAIL (oldest-first is the rule).
        rep.push(mm(
            "evictions",
            "run",
            "(order)",
            format!("{ea:?}"),
            format!("{eb:?}"),
            UNTOLERATED,
        ));
    }

    // ---- end-snapshot `recording.backlogSegments` (T8/end-snapshot,
    // NARROWED per review-wp6 FLAG 3): no cross-side bound. Each side's
    // shutdown value must lie in [its own last telemetry tick − 1, the
    // unuploaded pieces left on its disk (mp4 fragments + telemetry
    // segments of the final tree)]: the shutdown write may land before or
    // after the last fragments' EOS + finalize (TS worker.ts shutdown →
    // recording.stop(); JS-vs-JS 1/3), never below what the last tick saw
    // nor above what exists. (A camera can close TWO fragments at the stop —
    // the open one + an EOS-born one, replay-20261019T151944Z cam1 #9/#10 on
    // both sides — so "live sessions + 1" would undercount.) The cross-side
    // row is tolerated only when BOTH sides pass.
    let mut in_bound = true;
    for (label, m) in [("A", a), ("B", b)] {
        let Some(end) = m.end_backlog else {
            continue;
        };
        let disk = m.mp4_final.len() + m.finals.keys().filter(|k| tele_seg_of(k).is_some()).count();
        let lo = m.last_tick_backlog.unwrap_or(0) - 1;
        let hi = disk as i64;
        rep.inputs.insert(
            format!("end backlogSegments {label}"),
            format!("{end} (bound [{lo}, {hi}]: last tick − 1, pieces on disk)"),
        );
        if end < lo || end > hi {
            in_bound = false;
            rep.push(mm(
                "state",
                &format!("end {label}"),
                "recording.backlogSegments (own bound)",
                end.to_string(),
                format!("[{lo}, {hi}]"),
                UNTOLERATED,
            ));
        }
    }
    if let Some((rid, _)) = table.decision_row("end-snapshot") {
        let id = format!("{rid}/end-snapshot");
        let Report {
            mismatches, stats, ..
        } = rep;
        for x in mismatches.iter_mut().filter(|x| {
            x.stream == "state" && x.path == "recording.backlogSegments" && x.tol == UNTOLERATED
        }) {
            if in_bound && a.end_backlog.is_some() && b.end_backlog.is_some() {
                x.tol = id.clone();
                let st = stats.entry(x.stream.clone()).or_default();
                st.untolerated = st.untolerated.saturating_sub(1);
                st.tolerated += 1;
            }
        }
    }

    // ---- shm orphans
    stat(rep, "shm").compared += 1;
    rep.inputs.insert(
        "shm orphans at end".into(),
        format!("A {}, B {}", a.shm_orphans, b.shm_orphans),
    );
    if a.shm_orphans != b.shm_orphans {
        rep.push(mm(
            "shm",
            "end",
            "(orphan shmpipe segments)",
            a.shm_orphans.to_string(),
            b.shm_orphans.to_string(),
            UNTOLERATED,
        ));
    }
    d30_js_reference(a, b, &pmic_series(play), table, rep);
}

/// The rail value of a TS shed event and its text with it masked: `…(rail
/// 5.10V < 5.2V for 5s, triggered 100% of the last 20s past no-live): …` →
/// (`5.10`, `…(rail <V>V < 5.2V …`). `None` unless it is that template.
fn shed_text(k: &str) -> Option<(String, String)> {
    let (_, msg) = k.split_once(": ")?;
    msg.strip_prefix("power shed → ")?;
    let (head, rest) = k.split_once("(rail ")?;
    let (v, tail) = rest.split_once("V < ")?;
    v.parse::<f64>().ok()?;
    let shape = [
        "V for ",
        "s, triggered ",
        "% of the last ",
        "s past ",
        "): ",
    ];
    let mut t = tail;
    for p in shape {
        t = t.split_once(p)?.1;
    }
    Some((v.to_string(), format!("{head}(rail <V>V < {tail}")))
}

/// Row `T8/d30-js-reference` (review-wp6 re-check, ORCHESTRATOR DECISION):
/// D30 INSIDE the reference — a JS shed tick deciding on its recorder's ≤ 1 s
/// old cached power reading. CALIBRATION ONLY: between JS and Rust the same
/// rows are a Rust bug (Rust is never stale), so both matchers return false
/// unless BOTH sides are `js` — in code; no toml field can widen it.
/// Re-judges UNTOLERATED rows already in `rep`:
/// - `d30_js_shed_text` (events multiset): exactly one A+1 and one B+1
///   message, both the TS shed template and identical with the rail value
///   masked; the two values are CONSECUTIVE distinct values of the tape's
///   released `vcgencmd pmic_read_adc` series; the stale side's event lies in
///   [r, r + 1100] ms, r = the release of the fresher value; one pair per
///   masked template.
/// - `d30_js_spawn_tick` (children `(spawn ms after start)`): kvs only;
///   1000 < |Δ| ≤ 1150; each side's spawn lags its own last powerShed/
///   liveBlock change by the same ±300 ms; every kvs spawn referencing that
///   same restore shifts by the same Δ ±150; ≤ channels × shed changes rows.
fn d30_js_reference(
    a: &MediaCapture,
    b: &MediaCapture,
    pmic: &[(i64, String)],
    table: &Table,
    rep: &mut Report,
) {
    let Some((rid, _)) = table.decision_row("d30-js-reference") else {
        return;
    };
    let id = format!("{rid}/d30-js-reference");
    let shed_ok = d30_js_shed_text(a, b, pmic, rep);
    let spawn_ok = d30_js_spawn_tick(a, b, rep);
    let Report {
        mismatches, stats, ..
    } = rep;
    for (i, x) in mismatches.iter_mut().enumerate() {
        if x.tol == UNTOLERATED && (shed_ok.contains(&i) || spawn_ok.contains(&i)) {
            x.tol = id.clone();
            let st = stats.entry(x.stream.clone()).or_default();
            st.untolerated = st.untolerated.saturating_sub(1);
            st.tolerated += 1;
        }
    }
}

/// The tape's released pmic rail values, deduplicated to their changes:
/// `(release wall ms, "%.2f")`.
fn pmic_series(play: &PlayInfo) -> Vec<(i64, String)> {
    use xorgate_hal::tape::TapeEvent;
    let Ok(t) = xorgate_hal::tape::Tape::read(Path::new(&play.tape)) else {
        return Vec::new();
    };
    let walls = crate::diff::release_walls(&t.records, &play.releases);
    let mut out: Vec<(i64, String)> = Vec::new();
    for (r, w) in t.records.iter().zip(walls) {
        let (TapeEvent::Exec { argv, stdout, .. }, Some(w)) = (&r.ev, w) else {
            continue;
        };
        if argv.get(1).map(String::as_str) != Some("pmic_read_adc") {
            continue;
        }
        let Some(v) = xorgate_core::system::parse_pmic_adc(stdout)
            .and_then(|adc| xorgate_core::system::rail_5v_from(&adc))
        else {
            continue;
        };
        let v = format!("{v:.2}");
        if out.last().map(|l| &l.1) != Some(&v) {
            out.push((w, v));
        }
    }
    out
}

/// `d30_js_shed_text`: indices of the events rows it covers.
fn d30_js_shed_text(
    a: &MediaCapture,
    b: &MediaCapture,
    pmic: &[(i64, String)],
    rep: &Report,
) -> BTreeSet<usize> {
    let mut ok = BTreeSet::new();
    if !(a.kind == "js" && b.kind == "js") {
        return ok;
    }
    // masked template → [(row index, value, a count, b count)]
    let mut by: BTreeMap<String, Vec<(usize, String, i64, i64)>> = BTreeMap::new();
    for (i, m) in rep.mismatches.iter().enumerate() {
        if m.stream != "events" || m.at != "multiset" || m.tol != UNTOLERATED {
            continue;
        }
        if let (Some((v, masked)), Ok(x), Ok(y)) =
            (shed_text(&m.path), m.a.parse::<i64>(), m.b.parse::<i64>())
        {
            by.entry(masked).or_default().push((i, v, x, y));
        }
    }
    for rows in by.values() {
        let [(ia, va, xa, ya), (ib, vb, xb, yb)] = rows.as_slice() else {
            continue;
        };
        // One A+1/B+0 and one A+0/B+1 (either listing order).
        let (a_only, b_only) = match (xa - ya, xb - yb) {
            (1, -1) => ((ia, va), (ib, vb)),
            (-1, 1) => ((ib, vb), (ia, va)),
            _ => continue,
        };
        // Consecutive distinct released values (the fresher one later); the
        // stale side decided within 1.1 s of the fresher value's release.
        let fits = pmic.windows(2).any(|w| {
            let (stale, fresh, r) = (&w[0].1, &w[1].1, w[1].0);
            let stale_side = if stale == a_only.1 && fresh == b_only.1 {
                a
            } else if stale == b_only.1 && fresh == a_only.1 {
                b
            } else {
                return false;
            };
            stale_side.shed_events.iter().any(|(ts, msg)| {
                shed_text(msg).is_some_and(|(v, _)| v == *stale) && *ts >= r && *ts <= r + 1100
            })
        });
        if fits {
            ok.insert(*ia);
            ok.insert(*ib);
        }
    }
    ok
}

/// `d30_js_spawn_tick`: indices of the children rows it covers.
fn d30_js_spawn_tick(a: &MediaCapture, b: &MediaCapture, rep: &Report) -> BTreeSet<usize> {
    let mut ok = BTreeSet::new();
    if !(a.kind == "js" && b.kind == "js") {
        return ok;
    }
    const LAG_MS: i64 = 300;
    const TICK_MS: i64 = 1_100;
    let last_change =
        |m: &MediaCapture, w: i64| m.shed_changes.iter().copied().rfind(|c| *c <= w + TICK_MS);
    let kvs = |m: &MediaCapture| -> BTreeMap<String, Vec<i64>> {
        let mut g: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for s in m.spawns.iter().filter(|s| s.role == "kvs") {
            g.entry(s.key.clone()).or_default().push(s.w);
        }
        g
    };
    let (ka, kb) = (kvs(a), kvs(b));
    // Every paired kvs spawn: (key#i, w_a, w_b, A's reference change).
    let mut pairs: Vec<(String, i64, i64, Option<i64>)> = Vec::new();
    for (key, wa) in &ka {
        let Some(wb) = kb.get(key).filter(|wb| wb.len() == wa.len()) else {
            continue;
        };
        for (i, (x, y)) in wa.iter().zip(wb).enumerate() {
            pairs.push((format!("kvs/{key}#{i}"), *x, *y, last_change(a, *x)));
        }
    }
    let channels = ka.len().max(kb.len());
    let cap = channels * a.shed_changes.len().max(b.shed_changes.len());
    let mut cand = Vec::new();
    for (i, m) in rep.mismatches.iter().enumerate() {
        if m.stream != "children" || m.path != "(spawn ms after start)" || m.tol != UNTOLERATED {
            continue;
        }
        let Some(p) = pairs.iter().find(|p| p.0 == m.at) else {
            continue;
        };
        let d = p.2 - p.1;
        if !(d.abs() > 1000 && d.abs() <= 1150) {
            continue;
        }
        let (Some(ca), Some(cb)) = (p.3, last_change(b, p.2)) else {
            continue;
        };
        if ((p.1 - ca) - (p.2 - cb)).abs() > LAG_MS {
            continue;
        }
        // Every kvs spawn of that same restore shifts by the same Δ ±150.
        let same = pairs
            .iter()
            .filter(|q| q.3 == Some(ca))
            .all(|q| ((q.2 - q.1) - d).abs() <= 150);
        if same {
            cand.push(i);
        }
    }
    if cand.len() <= cap {
        ok.extend(cand);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T10 flash: a kvs master stopped by the capture-spawn bounce in the
    /// same instant on one side only is tolerated — only next to a capture
    /// spawn of the SAME camera on that side, one flash per capture spawn;
    /// any other extra spawn is not.
    #[test]
    fn t10_flash_spawn_equalises_counts_or_fails() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let kvs_cam = |w: i64, end_after: i64, cam: &str| Spawn {
            w,
            role: "kvs".into(),
            key: "xorgate-{id}-cam1".into(),
            pid: w as u32,
            argv: vec!["xorgate-{id}-cam1".into(), "testsrc".into()],
            env: BTreeMap::from([(
                "KVS_GST_PIPELINE".to_string(),
                format!("<work>/etc/pipelines/{cam}.gst"),
            )]),
            playing: None,
            end: Some("int".into()),
            end_w: Some(w + end_after),
            sid: None,
        };
        let kvs = |w: i64, end_after: i64| kvs_cam(w, end_after, "cam1");
        let capture = |w: i64, cam: &str| Spawn {
            w,
            role: "capture".into(),
            key: cam.into(),
            pid: w as u32 + 1,
            argv: vec!["gst-launch-1.0".into()],
            env: BTreeMap::new(),
            playing: None,
            end: Some("int".into()),
            end_w: None,
            sid: None,
        };
        let cap = |spawns: Vec<Spawn>| MediaCapture {
            kind: "js".into(),
            spawns,
            present: true,
            ..Default::default()
        };
        let play = PlayInfo::default();
        // `extra` = A's additional masters; A's/B's camera spawns `ca`/`cb`.
        let run = |extra: Vec<Spawn>, ca: &str, cb: &str| {
            let mut sa = vec![kvs(6_500, 44_000)];
            sa.extend(extra);
            sa.extend([kvs(102_900, 60_000), capture(100_905, ca)]);
            let a = cap(sa);
            let b = cap(vec![
                kvs(6_500, 44_000),
                kvs(102_920, 60_000),
                capture(100_925, cb),
            ]);
            let mut rep = Report::default();
            diff_media(&a, &b, &play, &table, &mut rep);
            rep.mismatches
                .into_iter()
                .filter(|m| m.stream == "children" && m.at.starts_with("kvs/"))
                .map(|m| (m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let unt = |r: Vec<(String, String)>| {
            r.iter()
                .any(|(p, t)| p == "(spawn count)" && t == UNTOLERATED)
        };
        assert_eq!(
            run(vec![kvs(100_900, 0)], "cam1", "cam1"),
            vec![
                ("(spawn count)".to_string(), "T10".to_string()),
                ("(spawn ms after start)".to_string(), "T10".to_string()),
            ]
        );
        // A master that ran for 2 s is a real extra start, not a bounce flash.
        assert!(unt(run(vec![kvs(100_900, 2_000)], "cam1", "cam1")));
        // No capture of THAT camera next to it on that side: not a flash.
        assert!(unt(run(vec![kvs(100_900, 0)], "cam0", "cam0")));
        assert!(
            unt(run(vec![kvs(100_700, 0)], "cam1", "cam1")),
            "capture 205 ms away"
        );
        // Two quick masters around ONE capture spawn: only one is a flash.
        assert!(unt(run(
            vec![kvs(100_880, 0), kvs(100_890, 0)],
            "cam1",
            "cam1"
        )));
    }

    #[test]
    fn rec_telemetry_system_presence_per_sample() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let seg = |missing: usize| {
            let mut t = String::from("{\"type\":\"header\",\"v\":1}\n");
            for i in 0..10 {
                let sys = if i < missing {
                    String::new()
                } else {
                    ",\"system\":{\"throttled\":{\"value\":0}}".to_string()
                };
                t.push_str(&format!(
                    "{{\"ts\":{i},\"mono\":{i},\"seq\":{i},\"metrics\":{{\"gps\":{{\"hdop\":{{\"value\":1.3}}}}{sys}}}}}\n"
                ));
            }
            t
        };
        let cap = |kind: &str, missing: usize| {
            let mut finals = BTreeMap::new();
            finals.insert(
                "recordings/telemetry/sid-1/seg-1-00000.jsonl.gz".to_string(),
                seg(missing).into_bytes(),
            );
            MediaCapture {
                kind: kind.into(),
                finals,
                present: true,
                ..Default::default()
            }
        };
        let run_k = |ka: &str, ma: usize, kb: &str, mb: usize| {
            let mut rep = Report::default();
            diff_media(
                &cap(ka, ma),
                &cap(kb, mb),
                &PlayInfo::default(),
                &table,
                &mut rep,
            );
            rep.mismatches
                .into_iter()
                .filter(|m| m.stream == "rec-telemetry" && m.path == "(samples without system)")
                .map(|m| m.tol)
                .collect::<Vec<_>>()
        };
        let run = |ma: usize, mb: usize| run_k("js", ma, "js", mb);
        let unt = vec![UNTOLERATED.to_string()];
        let d30 = vec!["T7/D30".to_string()];
        // JS-vs-JS: the calibrated spread.
        assert!(run(2, 2).is_empty());
        assert_eq!(run(0, 2), vec!["T13/system-presence".to_string()]);
        assert_eq!(run(0, 3), unt);
        // JS-vs-Rust: D30 only — Rust 0 and JS ≥ 1, either column order, no
        // JS cap; T13/system-presence never applies.
        assert_eq!(run_k("js", 5, "rust", 0), d30);
        assert_eq!(run_k("rust", 0, "js", 1), d30);
        assert!(run_k("js", 0, "rust", 0).is_empty());
        // Any Rust drop fails — also when the counts agree, and also when it
        // is within the JS-vs-JS spread.
        assert_eq!(run_k("js", 0, "rust", 1), unt);
        assert_eq!(run_k("js", 2, "rust", 1), unt);
        assert_eq!(run_k("rust", 2, "js", 2), unt);
        // Rust-vs-Rust: exact.
        assert!(run_k("rust", 1, "rust", 1).is_empty());
        assert_eq!(run_k("rust", 0, "rust", 1), unt);
    }

    /// T7/D30 `d30_shed_tick_session_end`: the shape of
    /// replay-20261019T152309Z cam1#1 (JS restore one tick late → the last
    /// fragment 3451 vs 4350 ms); every condition of review-wp6 §3 flips it.
    #[test]
    fn d30_shed_tick_session_end_only_for_a_late_js_restore() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let sid = "<sid:video:cam1#1>";
        // (kind, spawn w, first startTs, last durationMs, shed change w)
        let side = |kind: &str, w: i64, st0: i64, last_dur: i64, change: i64| {
            let mut finals = BTreeMap::new();
            for (seq, st, dur) in [(0u64, st0, 10_000), (1, st0 + 10_000, last_dur)] {
                finals.insert(
                    format!("recordings/video/cam1/{sid}/{seq:05}.json"),
                    serde_json::to_vec(&serde_json::json!({
                        "seq": seq, "startTs": st, "startMono": (st - st0) + 5,
                        "durationMs": dur, "sizeBytes": 100,
                    }))
                    .unwrap(),
                );
            }
            MediaCapture {
                kind: kind.into(),
                spawns: vec![Spawn {
                    w,
                    role: "capture".into(),
                    key: "cam1".into(),
                    pid: 7,
                    argv: vec!["gst".into()],
                    env: BTreeMap::new(),
                    playing: Some(st0 - 2_000),
                    end: Some("int".into()),
                    end_w: None,
                    sid: Some(sid.into()),
                }],
                finals,
                shed_changes: vec![change],
                present: true,
                ..Default::default()
            }
        };
        let js = || side("js", 526_868, 528_886, 3_451, 522_939);
        let rs = || side("rust", 525_962, 527_978, 4_350, 521_949);
        let run = |a: MediaCapture, b: MediaCapture| {
            let mut rep = Report::default();
            diff_media(&a, &b, &PlayInfo::default(), &table, &mut rep);
            rep.mismatches
                .into_iter()
                // startTs differs by δ everywhere (T12, own PLAYING basis).
                .filter(|m| m.stream == "sidecar" && m.path != "startTs")
                .map(|m| format!("{} {} {}", m.at, m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let row = |tol: &str| vec![format!("{sid}/00001 durationMs {tol}")];
        assert_eq!(run(js(), rs()), row("T7/D30"));
        assert_eq!(run(rs(), js()), row("T7/D30"), "either column order");
        // (b) Rust LATER (a P-WP6-1 regression shape): never.
        let rs_late = side("rust", 527_774, 529_790, 2_540, 523_845);
        assert_eq!(run(js(), rs_late), row(UNTOLERATED));
        // Rust-vs-Rust: never.
        let mut r2 = js();
        r2.kind = "rust".into();
        assert_eq!(run(r2, rs()), row(UNTOLERATED));
        // (c) the ends disagree by > 150 ms.
        assert_eq!(
            run(js(), side("rust", 525_962, 527_978, 4_600, 521_949)),
            row(UNTOLERATED)
        );
        // (a) a capture respawn of that camera after the session.
        let mut j = js();
        let mut again = j.spawns[0].clone();
        again.w += 20_000;
        again.sid = None;
        j.spawns.push(again);
        assert!(run(j, rs()).contains(&format!("{sid}/00001 durationMs {UNTOLERATED}")));
        // (a) the child did not end at the stop (an external kill).
        let mut j = js();
        j.spawns[0].end = Some("external".into());
        assert_eq!(run(j, rs()), row(UNTOLERATED));
        // (d) the spawn-to-restore lags differ by > 300 ms.
        assert_eq!(
            run(side("js", 526_868, 528_886, 3_451, 522_400), rs()),
            row(UNTOLERATED)
        );
        // (e) an earlier fragment of the session failed T12.
        let mut j = js();
        j.finals.insert(
            format!("recordings/video/cam1/{sid}/00000.json"),
            serde_json::to_vec(&serde_json::json!({
                "seq": 0, "startTs": 528_886, "startMono": 5, "durationMs": 9_000, "sizeBytes": 100,
            }))
            .unwrap(),
        );
        assert_eq!(
            run(j, rs()),
            vec![
                format!("{sid}/00000 durationMs {UNTOLERATED}"),
                format!("{sid}/00001 durationMs {UNTOLERATED}"),
            ]
        );
    }

    /// G1: recorded VALUES per leaf (deduplicated), per-group presence.
    #[test]
    fn rec_values_compare_steps_exactly_but_a_raced_tick() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        // One sample per second at `t0 + i*1000 + phase`; lat[i] (None = no gps).
        let seg = |phase: i64, lat: &[Option<f64>]| {
            let mut t = String::from("{\"type\":\"header\",\"v\":1}\n");
            for (i, l) in lat.iter().enumerate() {
                let gps = l
                    .map(|v| format!("\"gps\":{{\"lat\":{{\"value\":{v}}}}},"))
                    .unwrap_or_default();
                t.push_str(&format!(
                    "{{\"ts\":{},\"seq\":{i},\"metrics\":{{{gps}\"system\":{{\"cpu_usage\":{{\"value\":{i}}},\"throttled\":{{\"value\":0}}}}}}}}\n",
                    100_000 + i as i64 * 1000 + phase
                ));
            }
            t
        };
        let cap = |phase: i64, lat: &[Option<f64>]| {
            let mut finals = BTreeMap::new();
            finals.insert(
                "recordings/telemetry/<sid:telemetry#0>/seg-1-00000.jsonl.gz".to_string(),
                seg(phase, lat).into_bytes(),
            );
            MediaCapture {
                kind: "js".into(),
                finals,
                present: true,
                ..Default::default()
            }
        };
        // NMEA bursts released at 100 650 + k*1000.
        let play = PlayInfo {
            releases: (0..8)
                .map(|k| crate::diff::Release {
                    w: 100_650 + k * 1000,
                    due: 0,
                    t: 0,
                    k: "nmea".into(),
                })
                .collect(),
            ..Default::default()
        };
        let run = |a: MediaCapture, b: MediaCapture| {
            let mut rep = Report::default();
            diff_media(&a, &b, &play, &table, &mut rep);
            assert!(
                rep.stats["rec-values"].compared >= 2,
                "lat + throttled compared"
            );
            rep.mismatches
                .into_iter()
                .filter(|m| m.stream == "rec-telemetry" && m.path != "(sample key order)")
                .map(|m| format!("{} {}", m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let v = |x: f64| Some(x + 0.25); // always a float (T9 shapes stay equal)
        let full = [v(1.0), v(2.0), v(3.0), v(4.0), v(5.0), v(6.0)];
        // Equal steps at different tick phases (cpu_usage = T3 host, never compared).
        assert!(run(cap(200, &full), cap(600, &full)).is_empty());
        // B ticks 1 ms after each burst and once misses it: value 3 skipped.
        let skip = [v(1.0), v(2.0), v(2.0), v(4.0), v(5.0), v(6.0)];
        assert_eq!(
            run(cap(200, &full), cap(651, &skip)),
            vec!["metrics.gps.lat.value(values) T4/rec-sample".to_string()]
        );
        // The same skip by a tick far from any release: untolerated.
        assert_eq!(
            run(cap(200, &full), cap(300, &skip)),
            vec![format!("metrics.gps.lat.value(values) {UNTOLERATED}")]
        );
        // A DIFFERENT value (both sides hold a step the other lacks): never.
        let other = [v(1.0), v(2.0), v(3.5), v(4.0), v(5.0), v(6.0)];
        assert_eq!(
            run(cap(200, &full), cap(651, &other)),
            vec![format!("metrics.gps.lat.value(values) {UNTOLERATED}")]
        );
        // A step held by two samples on one side only: not a raced tick.
        let two = [v(1.0), v(2.0), v(9.0), v(9.0), v(5.0), v(6.0)];
        let one = [v(1.0), v(2.0), v(2.0), v(2.0), v(5.0), v(6.0)];
        assert!(run(cap(651, &one), cap(200, &two))
            .contains(&format!("metrics.gps.lat.value(values) {UNTOLERATED}")));
        // ±1 sample at the stretch edge (the cut): one step, one sample.
        let short = [v(1.0), v(2.0), v(3.0), v(4.0), v(5.0), None];
        assert_eq!(
            run(cap(200, &full), cap(300, &short)),
            vec![
                "metrics.gps.lat.value(values) T4/rec-sample".to_string(),
                // no tape here: 0 gps transitions to explain the count
                format!("metrics.gps(samples carrying) {UNTOLERATED}"),
            ]
        );
    }

    /// End-snapshot `recording.backlogSegments` (review-wp6 FLAG 3): no
    /// cross-side bound; each side within [its last tick − 1, pieces on disk].
    #[test]
    fn end_backlog_is_a_per_side_bound() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let side = |end: i64, last: i64, mp4s: usize| MediaCapture {
            kind: "js".into(),
            end_backlog: Some(end),
            last_tick_backlog: Some(last),
            mp4_final: (0..mp4s).map(|i| (format!("v/s/{i:05}.mp4"), 1)).collect(),
            finals: BTreeMap::from([(
                "recordings/telemetry/<sid:telemetry#0>/seg-<ms>-00002.jsonl.gz".to_string(),
                b"{}".to_vec(),
            )]),
            present: true,
            ..Default::default()
        };
        let run = |a: MediaCapture, b: MediaCapture| {
            let mut rep = Report::default();
            // The cross-side row diff.rs leaves (no numeric bound any more).
            rep.push(mm(
                "state",
                "end",
                "recording.backlogSegments",
                a.end_backlog.unwrap().to_string(),
                b.end_backlog.unwrap().to_string(),
                UNTOLERATED,
            ));
            diff_media(&a, &b, &PlayInfo::default(), &table, &mut rep);
            rep.mismatches
                .into_iter()
                .filter(|m| m.stream == "state")
                .map(|m| format!("{} {} {}", m.at, m.path, m.tol))
                .collect::<Vec<_>>()
        };
        // 1 vs 4 (3 fragments + 1 segment on B's disk): both in bound.
        assert_eq!(
            run(side(1, 0, 3), side(4, 0, 3)),
            vec!["end recording.backlogSegments T8/end-snapshot".to_string()]
        );
        // B claims more than exists on its disk: its own row + the cross row fail.
        assert_eq!(
            run(side(1, 0, 3), side(5, 0, 3)),
            vec![
                format!("end recording.backlogSegments {UNTOLERATED}"),
                format!("end B recording.backlogSegments (own bound) {UNTOLERATED}"),
            ]
        );
        // A below its own last tick − 1.
        assert!(run(side(70, 78, 80), side(79, 78, 80)).contains(&format!(
            "end A recording.backlogSegments (own bound) {UNTOLERATED}"
        )));
    }

    /// T8/d30-js-reference: the replay-20261019T173537Z shape (JS-vs-JS
    /// shed-uv) is tolerated ONLY js/js; js/rust, rust/js, rust/rust fail.
    #[test]
    fn d30_js_reference_only_for_js_vs_js() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let text = |v: &str| {
            format!("warn: power shed → bridge-only (rail {v}V < 5.2V for 5s, triggered 100% of the last 20s past no-live): video recording off, telemetry recording continues")
        };
        let kvs = |key: &str, w: i64| Spawn {
            w,
            role: "kvs".into(),
            key: key.into(),
            pid: w as u32 + key.len() as u32,
            argv: vec![],
            env: BTreeMap::new(),
            playing: None,
            end: Some("int".into()),
            end_w: None,
            sid: None,
        };
        // A fresh (5.11 at 367.128), B stale (5.10 at 367.109, its recorder's
        // 366.321 reading); 5.11 released 366.880. Restore: A change 522.0 →
        // kvs 523.0, B change 523.004 → kvs 524.004.
        let side = |kind: &str, ev: (i64, &str), change: i64, spawn: i64| MediaCapture {
            kind: kind.into(),
            shed_events: vec![(ev.0, text(ev.1))],
            shed_changes: vec![398_000, change],
            spawns: vec![
                kvs("xorgate-{id}-cam0", spawn),
                kvs("xorgate-{id}-cam1", spawn),
            ],
            present: true,
            ..Default::default()
        };
        let pmic = vec![
            (360_880, "5.13".to_string()),
            (364_880, "5.10".to_string()),
            (366_880, "5.11".to_string()),
            (368_880, "5.12".to_string()),
        ];
        let run = |ka: &str, kb: &str, stale_at: i64, d: i64| {
            let a = side(ka, (367_128, "5.11"), 522_000, 523_000);
            let b = side(kb, (stale_at, "5.10"), 522_000 + d, 523_000 + d);
            let mut rep = Report::default();
            for (k, x, y) in [(text("5.10"), 0, 1), (text("5.11"), 1, 0)] {
                rep.push(mm(
                    "events",
                    "multiset",
                    &k,
                    x.to_string(),
                    y.to_string(),
                    UNTOLERATED,
                ));
            }
            for key in ["xorgate-{id}-cam0", "xorgate-{id}-cam1"] {
                rep.push(mm(
                    "children",
                    &format!("kvs/{key}#0"),
                    "(spawn ms after start)",
                    "523000".into(),
                    (523_000 + d).to_string(),
                    UNTOLERATED,
                ));
            }
            d30_js_reference(&a, &b, &pmic, &table, &mut rep);
            rep.mismatches
                .into_iter()
                .map(|m| m.tol)
                .collect::<Vec<_>>()
        };
        let all = |t: &str| vec![t.to_string(); 4];
        assert_eq!(run("js", "js", 367_109, 1_004), all("T8/d30-js-reference"));
        // The guard: the SAME rows for any pair with a Rust side.
        for (ka, kb) in [("js", "rust"), ("rust", "js"), ("rust", "rust")] {
            assert_eq!(run(ka, kb, 367_109, 1_004), all(UNTOLERATED), "{ka}/{kb}");
        }
        // Stale decision more than 1.1 s after the fresher release: text fails.
        let r = run("js", "js", 368_100, 1_004);
        assert_eq!(r[..2], [UNTOLERATED.to_string(), UNTOLERATED.to_string()]);
        // A shift beyond one shed period + 150: spawns fail.
        let r = run("js", "js", 367_109, 1_200);
        assert_eq!(r[2..], [UNTOLERATED.to_string(), UNTOLERATED.to_string()]);
    }

    /// T14: a telemetry object PUT on one side only is tolerated only when it
    /// is the newest segment of its session (on either side), never a middle one.
    #[test]
    fn t14_one_sided_telemetry_put_only_for_the_last_segment() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let key =
            |seq: u64| format!("telemetry/v1/{{id}}/<sid:telemetry#0>/<ms>-{seq:05}.jsonl.gz");
        let put = |seq: u64| S3Obs {
            key: key(seq),
            ctype: "application/gzip".into(),
            status: 200,
            sig: "ok".into(),
        };
        let cap = |seqs: &[u64]| MediaCapture {
            kind: "js".into(),
            s3: seqs.iter().map(|q| put(*q)).collect(),
            present: true,
            ..Default::default()
        };
        let run = |a: &[u64], b: &[u64]| {
            let mut rep = Report::default();
            diff_media(&cap(a), &cap(b), &PlayInfo::default(), &table, &mut rep);
            rep.mismatches
                .into_iter()
                .filter(|m| m.stream == "s3" && m.path == "(PUT)")
                .map(|m| (m.at, m.tol))
                .collect::<Vec<_>>()
        };
        // The newest segment uploaded by one side only (the cut): T14.
        assert_eq!(run(&[0, 1, 2], &[0, 1]), vec![(key(2), "T14".to_string())]);
        // …also when the other side holds it locally, its PUT cut short.
        let mut b = cap(&[0, 1]);
        b.rec.insert(
            "recordings/telemetry/<sid:telemetry#0>/seg-<ms>-00002.jsonl.gz".into(),
            Seen::default(),
        );
        let mut rep = Report::default();
        diff_media(&cap(&[0, 1, 2]), &b, &PlayInfo::default(), &table, &mut rep);
        assert!(rep
            .mismatches
            .iter()
            .any(|m| m.path == "(PUT)" && m.at == key(2) && m.tol == "T14"));
        // A MIDDLE segment lost on one side: untolerated.
        assert_eq!(
            run(&[0, 1, 2], &[0, 2]),
            vec![(key(1), UNTOLERATED.to_string())]
        );
        // S3 ORDER: oldest-first within a source, per side and A vs B.
        let order_rows = |a: &[u64], b: &[u64]| {
            let mut rep = Report::default();
            diff_media(&cap(a), &cap(b), &PlayInfo::default(), &table, &mut rep);
            rep.mismatches
                .into_iter()
                .filter(|m| m.path.starts_with("(PUT order"))
                .map(|m| format!("{} {} {}", m.at, m.path, m.tol))
                .collect::<Vec<_>>()
        };
        assert!(order_rows(&[0, 1, 2], &[0, 1, 2]).is_empty());
        assert_eq!(
            order_rows(&[0, 2, 1], &[0, 1, 2]),
            vec![
                format!("A telemetry (PUT order) {UNTOLERATED}"),
                format!("telemetry (PUT order A vs B) {UNTOLERATED}"),
            ]
        );
        // The side without the PUT skipped that segment and wrote a newer
        // one: lost, not cut short.
        let mut b = cap(&[0, 1]);
        b.rec.insert(
            "recordings/telemetry/<sid:telemetry#0>/seg-<ms>-00003.jsonl".into(),
            Seen::default(),
        );
        let mut rep = Report::default();
        diff_media(&cap(&[0, 1, 2]), &b, &PlayInfo::default(), &table, &mut rep);
        assert!(rep
            .mismatches
            .iter()
            .any(|m| m.path == "(PUT)" && m.at == key(2) && m.tol == UNTOLERATED));
    }

    #[test]
    fn helpers() {
        assert_eq!(
            uuids_in("location=/w/home/recordings/video/cam0/0198a6a1-2b3c-7d4e-8f00-112233445566/%05d.mp4"),
            vec!["0198a6a1-2b3c-7d4e-8f00-112233445566"]
        );
        assert_eq!(
            dir_of(
                "recordings/video/cam1/0198a6a1-2b3c-7d4e-8f00-112233445566/00001.json",
                "0198a6a1-2b3c-7d4e-8f00-112233445566"
            ),
            "video/cam1"
        );
        assert_eq!(
            dir_of(
                "telemetry/v1/{id}/0198a6a1-2b3c-7d4e-8f00-112233445566/1-2.jsonl.gz",
                "0198a6a1-2b3c-7d4e-8f00-112233445566"
            ),
            "telemetry"
        );
        assert_eq!(
            mask_ms("seg-1785353400123-3.jsonl.gz"),
            "seg-<ms>-3.jsonl.gz"
        );
        assert_eq!(
            frag_of("<sid:video:cam0#0>/00003.json"),
            Some(("<sid:video:cam0#0>".into(), 3, "json"))
        );
        assert_eq!(frag_of("x/session.json"), None);
        let mut s = BTreeMap::new();
        shapes(
            &serde_json::json!({"a": 1, "b": 1.5, "c": {"d": "x"}}),
            "m",
            &mut s,
        );
        assert_eq!(s["m.a"], BTreeSet::from(["int"]));
        assert_eq!(s["m.b"], BTreeSet::from(["float"]));
        let mut s = BTreeMap::new();
        shapes(
            &serde_json::from_str::<Value>(r#"{"a": 5.0, "b": 5.25, "c": 5}"#).unwrap(),
            "m",
            &mut s,
        );
        assert_eq!(s["m.a"], BTreeSet::from(["float-integral"]));
        assert_eq!(s["m.b"], BTreeSet::from(["float"]));
        assert_eq!(s["m.c"], BTreeSet::from(["int"]));
        let n = Norm {
            work: "/tmp/rig-work/a".into(),
            id: "00000000-0000-4000-8000-00000000000a".into(),
            sids: BTreeMap::from([(
                "0198a6a1-2b3c-7d4e-8f00-112233445566".to_string(),
                "<sid:telemetry#0>".to_string(),
            )]),
            files: vec![
                ("/rig/certs/a.crt".into(), "<cert>"),
                ("/rig/certs/a.key".into(), "<key>"),
            ],
        };
        assert_eq!(
            n.s(r#"{"AWS_IOT_CORE_CERT":"/rig/certs/a.crt","AWS_IOT_CORE_PRIVATE_KEY":"/rig/certs/a.key","KVS_GST_PIPELINE":"/tmp/rig-work/a/etc/pipelines/cam0.gst"}"#),
            r#"{"AWS_IOT_CORE_CERT":"<cert>","AWS_IOT_CORE_PRIVATE_KEY":"<key>","KVS_GST_PIPELINE":"<work>/etc/pipelines/cam0.gst"}"#
        );
        assert_eq!(
            n.s(r#"{"type":"header","deviceId":"00000000-0000-4000-8000-00000000000a","sessionId":"0198a6a1-2b3c-7d4e-8f00-112233445566"}"#),
            r#"{"type":"header","deviceId":"{id}","sessionId":"<sid:telemetry#0>"}"#
        );
    }
}
