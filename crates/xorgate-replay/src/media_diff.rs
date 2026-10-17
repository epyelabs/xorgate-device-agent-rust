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
//! | `rec-telemetry` | decompressed header lines minus T1/T11 fields; sample key order; per segment the groups present, each group's leaf paths and number SHAPES (T9) | segment count per session ±1 and file-name ms stamps (T13; gz bytes never compared: D24) |
//! | `s3` | per side every SigV4 signature valid, sidecar PUT before its mp4; A vs B the normalised key multiset of successful PUTs + content types | a session's LAST fragment present on one side only, attempt counts / timing (T14) |
//! | `file:counters.json` | bytes as JSON | — |
//! | `evictions` | the ordered list of fragments/segments deleted without a successful PUT | — |
//! | `shm` | per side: orphan `shmpipe.<pid>.*` (pid of that side's dead gst child) count at the end | — |
//!
//! Sample VALUES of the telemetry segments are not compared pairwise: each
//! recorder ticks on its own timer, ungated by the player, so a stream-fed
//! value differs by one input step between the two sides at almost every
//! tick; the live `telemetry` stream (hold-gated pairs) compares the same
//! collector values exactly, and the shapes here catch a formatting
//! divergence (CF42). Reviewer to confirm (wp6.md ambiguity A3).

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
        Self {
            kind: meta["kind"].as_str().unwrap_or("").to_string(),
            spawns,
            rec,
            finals,
            mp4_final,
            s3,
            s3_bodies,
            shm_orphans,
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

/// Every telemetry segment of a side (decompressed), keyed `<sid>/<n>`
/// (n = order of the file name).
fn segments(m: &MediaCapture) -> BTreeMap<String, Vec<String>> {
    let mut per: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (k, b) in m.s3_bodies.iter().chain(m.finals.iter()) {
        if !k.contains("telemetry") || !k.ends_with(".jsonl.gz") {
            continue;
        }
        let Some((dir, file)) = k.rsplit_once('/') else {
            continue;
        };
        let sid = dir.rsplit('/').next().unwrap_or("").to_string();
        // `seg-<ms>-<seq>` locally, `<ms>-<seq>` in S3: key by the seq.
        let seq = file
            .trim_end_matches(".jsonl.gz")
            .rsplit('-')
            .next()
            .unwrap_or("")
            .to_string();
        // Decompressed + normalised at load (`norm_body`).
        if let Ok(text) = std::str::from_utf8(b) {
            per.entry(sid)
                .or_default()
                .entry(seq)
                .or_insert_with(|| text.to_string());
        }
    }
    let mut out = BTreeMap::new();
    for (sid, segs) in per {
        let mut v: Vec<(u64, String)> = segs
            .into_iter()
            .map(|(seq, t)| (seq.parse().unwrap_or(u64::MAX), t))
            .collect();
        v.sort();
        out.insert(sid, v.into_iter().map(|(_, t)| t).collect());
    }
    out
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
            out.entry(path.to_string())
                .or_default()
                .insert(if s.contains(['.', 'e', 'E']) {
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
        let (xa, xb) = (
            ga.get(key).cloned().unwrap_or_default(),
            gb.get(key).cloned().unwrap_or_default(),
        );
        if xa.len() != xb.len() {
            rep.push(mm(
                "children",
                &at,
                "(spawn count)",
                xa.len().to_string(),
                xb.len().to_string(),
                UNTOLERATED,
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
    for key in ca.keys().chain(cb.keys()).collect::<BTreeSet<_>>() {
        let at = format!("{}/{:05}", key.0, key.1);
        let (Some(x), Some(y)) = (ca.get(key), cb.get(key)) else {
            continue; // presence: the mp4/s3 streams judge it (±1 at the cut)
        };
        stat(rep, "sidecar").compared += 1;
        let (pa, pb) = (playing(a, &key.0), playing(b, &key.0));
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
                    (d <= dur_ms).then(|| t12.to_string())
                }
                _ => None,
            }
        };
        json_cmp("sidecar", &at, "", x, y, &skip, rep);
    }

    // ---- mp4: contiguity + zero-byte per side, count ±1 (T12)
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
            rep.inputs
                .entry(format!("rec-telemetry samples {sid}"))
                .or_default()
                .push_str(&format!("[{}:{}]", pa.len(), pb.len()));
        }
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
                let at_cut = frag_of(k).is_some_and(|(sid, seq, _)| {
                    last_frag(&fa, &sid, seq) || last_frag(&fb, &sid, seq)
                }) || k.contains("telemetry");
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
        rep.push(mm(
            "s3",
            "run",
            "(failed attempts)",
            attempts(a).to_string(),
            attempts(b).to_string(),
            t14,
        ));
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
    let _ = (&a.kind, &b.kind);
}

#[cfg(test)]
mod tests {
    use super::*;

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
