//! Phase 6 P3a: the `takeover` stream of `xorgate-replay diff` (plan
//! `phase-6/evidence/P3a-takeover.md` §3), present when the player ran a
//! handoff (`play.json` `handoff`, `handoff.rs`). Side A = X then Y on one
//! home (the cross-agent takeover), side B = X then X (the same-agent
//! control); both inherited the state ONE agent kind (X) produced on the same
//! inputs, so every row below is exact — no tolerance row applies:
//!
//! | row | compared |
//! |---|---|
//! | `state` | the classes of the inherited state (`<side>/handoff-state.json`), A vs B, and the tape's declared minima/maxima (`meta.rig.handoff.expect`) — a miss makes the lane **INVALID** (the state was not the one designed), not a verdict |
//! | `edge` | the active fragment's / open telemetry segment's age at the stop is ≥ [`EDGE_MS`] from its rotation — else **INVALID** (a knife edge between the two sides' X runs) |
//! | `fate` | per inherited file, its end fate (`put×N`, `disk`, `gone`), A vs B; per side N ≤ 1 (no double upload) and no non-empty file `gone` (no lost segment) |
//! | `recovery-lines` | the post-handoff stdout/stderr lines in the TS recovery vocabulary ([`VOCAB`]), multiset A vs B |
//! | `disk-end` | the final on-disk files of the inherited (X-era) sessions, A vs B; with `expect.reaped` none may be left |
//! | `inflight` | (`expect.inflight`) the mp4 PUTs the fake S3 held mid-transfer and saw aborted at the cut (`aws.rs` `Hold`), per side = the expected count and A vs B the same keys — else **INVALID** |
//! | `bucket-orphans` | sidecars of the inherited sessions whose mp4 never had a successful PUT at the end, A vs B |
//!
//! One documented re-PUT is not a double upload: a sidecar PUT twice with a
//! FAILED attempt of its own mp4 in between (`takeover/sidecar-reput`,
//! [`sidecar_reput_explained`]): the TS uploads sidecar then mp4 as one unit
//! and "a failure between the two PUTs re-PUTs the sidecar on retry — S3 PUTs
//! are idempotent" (ts:src/recording/uploader.ts:24-28, :168-176 @09da38d).
//! It is cited only when BOTH sides show it (`reput_tol`: the control X→X
//! does it too). Any other double (an mp4, a telemetry segment, a sidecar whose mp4 had
//! not failed in between, a third PUT) stays UNTOLERATED.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::{Mismatch, Report, UNTOLERATED};
use crate::media_diff::MediaCapture;

pub const STREAM: &str = "takeover";
/// The marker a later incarnation's logs start after (`play.rs` `start`).
pub const MARKER: &str = "[rig] handoff:";
/// Knife-edge guard around a rotation.
pub const EDGE_MS: i64 = 2_000;
/// `SEGMENT_MAX_AGE_MS` (ts:src/recording/segments.ts:45 @09da38d).
pub const TELEMETRY_ROTATE_MS: i64 = 60_000;
/// The default fragment length of the P3a tapes (`segmentSeconds: 10`).
pub const DEFAULT_SEG_MS: i64 = 10_000;
/// `at` prefix of the rows that make a lane INVALID (not a verdict).
pub const INVALID: &str = "INVALID";

/// The TS recovery / orphan vocabulary @ 09da38d (every line the agents print
/// when they take over a previous run's recordings).
pub const VOCAB: &[&str] = &[
    // manager.ts:308 (worker.ts:802 prints `[agent] <msg>`)
    "recording: finalized ",
    // manager.ts:314
    "recording: backfilled ",
    // segments.ts:265
    "[recording] orphan finalize failed",
    // video-segments.ts (sweepSessionDir, 0-byte fragment)
    "[recording] dropped empty fragment",
    // video-segments.ts (sweepSessionDir, sidecar write)
    "[recording] sidecar write failed",
    // uploader.ts:148
    "upload: skipping unrecognized segment",
];

/// What the player did at the handoff (`handoff.json`, `play.json` `handoff`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffInfo {
    /// Tape ms of the handoff (tape time is frozen across it).
    pub at_ms: u64,
    /// `term` | `kill`.
    pub signal: String,
    /// Wall ms the first signal went out (both sides).
    pub stop_w: i64,
    /// Per side, wall ms every member was gone.
    pub stopped_w: [i64; 2],
    /// Wall ms both sides were started again.
    pub restart_w: i64,
    /// How long tape time stood still.
    pub pause_ms: i64,
    /// Per side, the kind before / after.
    pub before: [String; 2],
    pub after: [String; 2],
    /// Per side, members signalled at `stop_w` / SIGKILLed at the timeout.
    pub signalled: [usize; 2],
    pub killed_after_timeout: [usize; 2],
    /// Per side, the stopped agent's exit (`code N` / `signal N`).
    pub agent_exit: [String; 2],
    /// `meta.rig.handoff` of the tape (`expect`, `segMs`).
    #[serde(default)]
    pub spec: Value,
}

/// One inherited file, as the snapshot saw it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileState {
    pub size: u64,
    pub birth_ms: Option<i64>,
    /// The ms stamp of a telemetry segment name (`seg-<startTs>-<seq>…`),
    /// read from the RAW name (the normalised one masks it).
    pub name_ms: Option<i64>,
}

/// One side's inherited state: normalised rel (under `recordings/`) → file.
pub type Inherited = BTreeMap<String, FileState>;

/// `seg-<ms>-<seq>.jsonl[.gz]` → `<ms>`.
pub fn seg_name_ms(name: &str) -> Option<i64> {
    let rest = name.strip_prefix("seg-")?;
    let (ms, _) = rest.split_once('-')?;
    (ms.len() == 13).then(|| ms.parse().ok()).flatten()
}

/// `<side>/handoff-state.json` → `(snapshot wall, files)`, normalised with the
/// side's own session labels (`MediaCapture::norm_path`).
pub fn load_inherited(dir: &Path, m: &MediaCapture) -> Option<(i64, Inherited)> {
    let v: Value =
        serde_json::from_slice(&std::fs::read(dir.join("handoff-state.json")).ok()?).ok()?;
    let w = v["w"].as_i64()?;
    let mut out = Inherited::new();
    for (rel, f) in v["files"].as_object()? {
        let name = rel.rsplit('/').next().unwrap_or("");
        out.insert(
            m.norm_path(rel),
            FileState {
                size: f["size"].as_u64().unwrap_or(0),
                birth_ms: f["birthMs"].as_i64().filter(|b| *b > 0),
                name_ms: seg_name_ms(name),
            },
        );
    }
    Some((w, out))
}

/// The class counts of an inherited state (module doc). Detailed classes are
/// compared A vs B; the aggregate ones (`video/<key>: mp4`, `telemetry:
/// segment`) are what `expect` names, independent of the stop signal (a
/// SIGTERM lets X finalize what a SIGKILL leaves open).
pub fn classes(files: &Inherited) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    let mut bump = |k: String| *out.entry(k).or_default() += 1;
    // video/<key>/<sid>/<name>: per session dir, the mp4 seqs + sidecars.
    let mut dirs: BTreeMap<(String, String), (BTreeSet<String>, BTreeSet<String>)> =
        BTreeMap::new();
    for rel in files.keys() {
        let seg: Vec<&str> = rel.split('/').collect();
        match seg.as_slice() {
            ["video", key, sid, name] => {
                let e = dirs.entry((key.to_string(), sid.to_string())).or_default();
                if *name == "session.json" {
                    bump(format!("video/{key}: session.json"));
                } else if let Some(stem) = name.strip_suffix(".mp4") {
                    e.0.insert(stem.to_string());
                } else if let Some(stem) = name.strip_suffix(".json") {
                    e.1.insert(stem.to_string());
                } else {
                    bump(format!("video/{key}: other"));
                }
            }
            ["telemetry", _sid, name] => {
                if name.ends_with(".jsonl") {
                    bump("telemetry: open .jsonl".into());
                    bump("telemetry: segment".into());
                } else if name.ends_with(".jsonl.gz") {
                    bump("telemetry: closed .jsonl.gz".into());
                    bump("telemetry: segment".into());
                } else {
                    bump("telemetry: other".into());
                }
            }
            _ => bump("other".into()),
        }
    }
    for ((key, _), (mp4s, sidecars)) in &dirs {
        let newest = mp4s.iter().max().cloned();
        for stem in mp4s {
            bump(format!("video/{key}: mp4"));
            let has = sidecars.contains(stem);
            let class = match (Some(stem) == newest.as_ref(), has) {
                (true, false) => "active mp4 (newest, no sidecar)",
                (_, true) => "closed mp4 + sidecar",
                (false, false) => "closed mp4 without sidecar",
            };
            bump(format!("video/{key}: {class}"));
        }
        for _ in sidecars.difference(mp4s) {
            bump(format!("video/{key}: sidecar without mp4"));
        }
    }
    out
}

/// `expect.min` / `expect.max` violations of one side's classes.
pub fn expect_misses(cls: &BTreeMap<String, usize>, expect: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let n = |k: &str| cls.get(k).copied().unwrap_or(0);
    for (bound, cmp) in [("min", true), ("max", false)] {
        for (k, v) in expect[bound].as_object().into_iter().flatten() {
            let Some(want) = v.as_u64() else { continue };
            let have = n(k) as u64;
            if (cmp && have < want) || (!cmp && have > want) {
                out.push(format!("{k}: {have} (expect {bound} {want})"));
            }
        }
    }
    out
}

/// Knife-edge rows: every active mp4 (age vs `seg_ms`) and the newest
/// telemetry segment of each session (age vs the 60 s rotation, from its
/// NAME stamp, which is the segment's open wall — a SIGTERM-finalized `.gz`
/// is born at the stop). `first_seen` = the observer's first sighting
/// (`recordings.json`), the fallback when the filesystem keeps no birth.
pub fn edge_misses(
    files: &Inherited,
    stop_w: i64,
    seg_ms: i64,
    first_seen: &dyn Fn(&str) -> Option<i64>,
) -> Vec<String> {
    let cls_of = |rel: &str| -> Option<&'static str> {
        let seg: Vec<&str> = rel.split('/').collect();
        match seg.as_slice() {
            ["video", ..] if rel.ends_with(".mp4") => Some("video"),
            ["telemetry", ..] if rel.ends_with(".jsonl") || rel.ends_with(".jsonl.gz") => {
                Some("telemetry")
            }
            _ => None,
        }
    };
    let dir_of = |rel: &str| {
        rel.rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default()
    };
    // newest per dir (names sort chronologically: zero-padded seqs)
    let mut newest: BTreeMap<String, &String> = BTreeMap::new();
    for rel in files.keys() {
        if cls_of(rel).is_some() {
            let d = dir_of(rel);
            if newest.get(&d).is_none_or(|n| *n < rel) {
                newest.insert(d, rel);
            }
        }
    }
    let mut out = Vec::new();
    for rel in newest.values() {
        let f = &files[*rel];
        let (start, period) = match cls_of(rel) {
            Some("video") => {
                let sidecar = rel.trim_end_matches(".mp4").to_string() + ".json";
                if files.contains_key(&sidecar) {
                    continue; // closed by X (an exit sweep): no active fragment
                }
                (f.birth_ms.or_else(|| first_seen(rel)), seg_ms)
            }
            _ => (f.name_ms, TELEMETRY_ROTATE_MS),
        };
        match start {
            None => out.push(format!("{rel}: no birth / name time")),
            Some(s) => {
                let age = stop_w - s;
                if age < EDGE_MS || age > period - EDGE_MS {
                    out.push(format!("{rel}: {age} ms old at the stop (rotation every {period} ms, guard {EDGE_MS})"));
                }
            }
        }
    }
    out
}

/// The S3 key (normalised, `{id}`) a normalised inherited rel uploads as.
/// An open `.jsonl` uploads as the `.jsonl.gz` it is finalized into.
pub fn s3_key_of(rel: &str) -> Option<String> {
    let seg: Vec<&str> = rel.split('/').collect();
    match seg.as_slice() {
        ["video", _, _, "session.json"] => None,
        ["video", key, sid, name] if name.ends_with(".mp4") || name.ends_with(".json") => {
            Some(format!("media-sessions/v1/{{id}}/{key}/{sid}/{name}"))
        }
        ["telemetry", sid, name] => {
            let stem = name
                .strip_prefix("seg-")?
                .trim_end_matches(".gz")
                .trim_end_matches(".jsonl");
            Some(format!("telemetry/v1/{{id}}/{sid}/{stem}.jsonl.gz"))
        }
        _ => None,
    }
}

/// The rel a file has at the end if it stayed (an open `.jsonl` is finalized
/// into its `.jsonl.gz`).
pub fn end_rel_of(rel: &str) -> String {
    if rel.ends_with(".jsonl") {
        format!("{rel}.gz")
    } else {
        rel.to_string()
    }
}

/// The row id citing the TS sidecar re-PUT (module doc).
pub const SIDECAR_REPUT: &str = "takeover/sidecar-reput";

/// One PUT attempt as the fake S3 saw it, normalised key.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    /// Arrival wall ms (`s3.ndjson` `w`; a held PUT: its `s3-held` record).
    pub w: i64,
    pub key: String,
    pub ok: bool,
}

/// Is a sidecar's second successful PUT the TS re-PUT after a failed unit?
/// Exactly two successes, and an attempt of ITS mp4 that did not succeed
/// strictly between them.
pub fn sidecar_reput_explained(attempts: &[Attempt], sidecar_key: &str) -> bool {
    let Some(stem) = sidecar_key.strip_suffix(".json") else {
        return false;
    };
    let mp4 = format!("{stem}.mp4");
    let mut ok_w: Vec<i64> = attempts
        .iter()
        .filter(|a| a.key == sidecar_key && a.ok)
        .map(|a| a.w)
        .collect();
    ok_w.sort();
    let [first, second] = ok_w.as_slice() else {
        return false;
    };
    attempts
        .iter()
        .any(|a| a.key == mp4 && !a.ok && a.w > *first && a.w < *second)
}

/// Is this inherited rel's `put×2` the TS sidecar re-PUT on its side?
pub fn is_sidecar_reput(rel: &str, fate: &str, attempts: &[Attempt]) -> bool {
    fate.starts_with("put×2")
        && rel.ends_with(".json")
        && !rel.ends_with("/session.json")
        && s3_key_of(rel).is_some_and(|k| sidecar_reput_explained(attempts, &k))
}

/// The row id of a double upload of `rel`: [`SIDECAR_REPUT`] ONLY when BOTH
/// sides show the same TS re-PUT of that sidecar — the control (X→X) does it
/// too, so it is X's own TS-documented behaviour carried over, not something
/// the takeover added. A re-PUT on one side only stays UNTOLERATED.
pub fn reput_tol(rel: &str, explained: &[BTreeSet<String>; 2]) -> &'static str {
    if explained.iter().all(|e| e.contains(rel)) {
        SIDECAR_REPUT
    } else {
        UNTOLERATED
    }
}

/// `(attempts, aborted keys)` of one side: `<side>/s3.ndjson` (every
/// answered PUT) + the run's `aws.ndjson` held-then-aborted PUTs of this
/// side's label (never answered, never stored).
pub fn load_attempts(run: &Path, label: &str, m: &MediaCapture) -> (Vec<Attempt>, Vec<String>) {
    let lines = |p: &Path| -> Vec<Value> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    let mut out: Vec<Attempt> = lines(&run.join(label).join("s3.ndjson"))
        .iter()
        .filter_map(|v| {
            Some(Attempt {
                w: v["w"].as_i64()?,
                key: m.norm_path(v["key"].as_str()?),
                ok: v["status"].as_u64() == Some(200),
            })
        })
        .collect();
    let aws = lines(&run.join("aws.ndjson"));
    let mine = |v: &&Value| v["agent"] == label;
    let aborted: Vec<String> = aws
        .iter()
        .filter(|v| v["k"] == "s3-aborted")
        .filter(mine)
        .filter_map(|v| v["key"].as_str().map(|k| m.norm_path(k)))
        .collect();
    for v in aws.iter().filter(|v| v["k"] == "s3-held").filter(mine) {
        if let (Some(w), Some(k)) = (v["w"].as_i64(), v["key"].as_str()) {
            let key = m.norm_path(k);
            if aborted.contains(&key) {
                out.push(Attempt { w, key, ok: false });
            }
        }
    }
    out.sort_by_key(|a| a.w);
    (out, aborted)
}

/// `put×N[+disk]` / `disk` / `gone`.
pub fn fate(rel: &str, puts: &BTreeMap<String, usize>, end: &BTreeSet<String>) -> String {
    let n = s3_key_of(rel)
        .and_then(|k| puts.get(&k).copied())
        .unwrap_or(0);
    let on_disk = end.contains(&end_rel_of(rel)) || end.contains(rel);
    match (n, on_disk) {
        (0, true) => "disk".into(),
        (0, false) => "gone".into(),
        (n, false) => format!("put×{n}"),
        (n, true) => format!("put×{n}+disk"),
    }
}

/// Per-side absolute fate violations: a double upload, or a non-empty file
/// that is gone without a successful PUT (session manifests are reaped by
/// design, `video-source.ts` reapSession, and never uploaded).
pub fn fate_violations(rel: &str, f: &FileState, fate: &str) -> Option<String> {
    let n: usize = fate
        .strip_prefix("put×")
        .and_then(|r| r.split('+').next())
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    if n >= 2 {
        return Some(format!("double upload ({fate})"));
    }
    let manifest = rel.ends_with("/session.json");
    if fate == "gone" && f.size > 0 && !manifest {
        return Some("lost (gone without a successful PUT)".into());
    }
    None
}

/// The recovery lines of the incarnation after the handoff: every line after
/// the first [`MARKER`] line that carries a [`VOCAB`] phrase, cut to start at
/// the phrase (each agent's own log prefix — `[agent] `, a timestamp — is
/// not compared), normalised by `norm`.
pub fn recovery_lines(text: &str, norm: &dyn Fn(&str) -> String) -> Vec<String> {
    let mut after = false;
    let mut out = Vec::new();
    for l in text.lines() {
        if !after {
            after = l.starts_with(MARKER);
            continue;
        }
        if let Some(i) = VOCAB.iter().filter_map(|v| l.find(v)).min() {
            out.push(norm(l[i..].trim_end()));
        }
    }
    out.sort();
    out
}

fn mm(at: &str, path: &str, a: String, b: String) -> Mismatch {
    mm_tol(at, path, a, b, UNTOLERATED)
}

fn mm_tol(at: &str, path: &str, a: String, b: String, tol: &str) -> Mismatch {
    Mismatch {
        stream: STREAM.into(),
        at: at.into(),
        path: path.into(),
        a,
        b,
        tol: tol.into(),
    }
}

fn count_multiset(v: &[String]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for x in v {
        *m.entry(x.clone()).or_default() += 1;
    }
    m
}

/// The whole stream (module doc). `run` = the run dir; `ma`/`mb` = the two
/// sides' media captures (their session labels normalise every path).
pub fn diff_takeover(
    run: &Path,
    h: &HandoffInfo,
    ma: &MediaCapture,
    mb: &MediaCapture,
    rep: &mut Report,
) {
    let sides = [(run.join("a"), ma), (run.join("b"), mb)];
    let seg_ms = h.spec["segMs"].as_i64().unwrap_or(DEFAULT_SEG_MS);
    let expect = &h.spec["expect"];
    rep.inputs.insert(
        "handoff (P3a)".into(),
        format!(
            "{} at t={} ms; A {}→{}, B {}→{}; stopped A +{} ms B +{} ms; pause {} ms; signalled {:?}, SIGKILLed at the timeout {:?}; exits {:?}",
            h.signal,
            h.at_ms,
            h.before[0],
            h.after[0],
            h.before[1],
            h.after[1],
            h.stopped_w[0] - h.stop_w,
            h.stopped_w[1] - h.stop_w,
            h.pause_ms,
            h.signalled,
            h.killed_after_timeout,
            h.agent_exit
        ),
    );
    let loaded: Vec<Option<(i64, Inherited)>> =
        sides.iter().map(|(d, m)| load_inherited(d, m)).collect();
    let (Some((_, ia)), Some((_, ib))) = (&loaded[0], &loaded[1]) else {
        rep.push(mm(
            &format!("{INVALID} state"),
            "(handoff-state.json)",
            loaded[0].is_some().to_string(),
            loaded[1].is_some().to_string(),
        ));
        return;
    };
    let inh = [ia, ib];

    // (i) state
    let ca = classes(ia);
    let cb = classes(ib);
    let keys: BTreeSet<&String> = ca.keys().chain(cb.keys()).collect();
    for k in &keys {
        let (x, y) = (
            ca.get(*k).copied().unwrap_or(0),
            cb.get(*k).copied().unwrap_or(0),
        );
        if x != y {
            rep.push(mm(
                &format!("{INVALID} state"),
                k,
                x.to_string(),
                y.to_string(),
            ));
        }
    }
    for (side, cls) in [("A", &ca), ("B", &cb)] {
        for miss in expect_misses(cls, expect) {
            rep.push(mm(
                &format!("{INVALID} state {side}"),
                &miss,
                side.into(),
                "expect".into(),
            ));
        }
    }
    rep.inputs.insert(
        "takeover state (A)".into(),
        ca.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", "),
    );

    // (ii) edge
    for (i, (side, (_, m))) in ["A", "B"].iter().zip(&sides).enumerate() {
        let first = |rel: &str| m.first_seen(rel);
        for miss in edge_misses(inh[i], h.stop_w, seg_ms, &first) {
            rep.push(mm(
                &format!("{INVALID} edge {side}"),
                &miss,
                String::new(),
                String::new(),
            ));
        }
    }

    // (iii) fate
    let attempts: Vec<(Vec<Attempt>, Vec<String>)> = ["a", "b"]
        .iter()
        .zip(&sides)
        .map(|(l, (_, m))| load_attempts(run, l, m))
        .collect();
    let mut fates: [BTreeMap<String, String>; 2] = [BTreeMap::new(), BTreeMap::new()];
    let mut violations: [BTreeMap<String, String>; 2] = [BTreeMap::new(), BTreeMap::new()];
    let mut explained: [BTreeSet<String>; 2] = [BTreeSet::new(), BTreeSet::new()];
    for (i, (_, m)) in sides.iter().enumerate() {
        let puts = m.successful_puts();
        let end = m.end_files();
        for (rel, f) in inh[i].iter() {
            let ft = fate(rel, &puts, &end);
            if let Some(v) = fate_violations(rel, f, &ft) {
                if is_sidecar_reput(rel, &ft, &attempts[i].0) {
                    explained[i].insert(rel.clone());
                }
                violations[i].insert(rel.clone(), v);
            }
            fates[i].insert(rel.clone(), ft);
        }
    }
    for (i, viol) in violations.iter().enumerate() {
        for (rel, v) in viol {
            rep.push(mm_tol(
                &format!("fate {}", ["A", "B"][i]),
                rel,
                if i == 0 { v.clone() } else { String::new() },
                if i == 1 { v.clone() } else { String::new() },
                reput_tol(rel, &explained),
            ));
        }
    }
    // (iii-b) in flight at the cut (`expect.inflight`, aws.rs `Hold`).
    let aborted: Vec<BTreeSet<&String>> =
        attempts.iter().map(|(_, ab)| ab.iter().collect()).collect();
    if let Some(want) = expect["inflight"].as_u64() {
        for (i, ab) in aborted.iter().enumerate() {
            if ab.len() as u64 != want {
                rep.push(mm(
                    &format!("{INVALID} inflight {}", ["A", "B"][i]),
                    "(PUTs aborted at the cut)",
                    ab.len().to_string(),
                    want.to_string(),
                ));
            }
        }
        for k in aborted[0].symmetric_difference(&aborted[1]) {
            rep.push(mm(
                &format!("{INVALID} inflight"),
                k,
                aborted[0].contains(*k).to_string(),
                aborted[1].contains(*k).to_string(),
            ));
        }
    }
    rep.inputs.insert(
        "takeover in-flight PUTs aborted at the cut".into(),
        format!("A {:?}, B {:?}", aborted[0], aborted[1]),
    );
    let rels: BTreeSet<&String> = fates[0].keys().chain(fates[1].keys()).collect();
    for rel in &rels {
        let (x, y) = (fates[0].get(*rel), fates[1].get(*rel));
        if x != y {
            rep.push(mm(
                "fate",
                rel,
                x.cloned().unwrap_or_else(|| "∅".into()),
                y.cloned().unwrap_or_else(|| "∅".into()),
            ));
        }
    }
    {
        let st = rep.stats.entry(STREAM.into()).or_default();
        st.compared += rels.len();
    }

    // (iv) recovery lines
    let lines: Vec<BTreeMap<String, usize>> = sides
        .iter()
        .map(|(d, m)| {
            // Each log on its own: the marker opens the new incarnation's
            // part of EACH file.
            let all: Vec<String> = ["stdout.log", "stderr.log"]
                .iter()
                .flat_map(|f| {
                    let text = std::fs::read_to_string(d.join(f)).unwrap_or_default();
                    recovery_lines(&text, &|s| m.norm_path(s))
                })
                .collect();
            count_multiset(&all)
        })
        .collect();
    let lk: BTreeSet<&String> = lines[0].keys().chain(lines[1].keys()).collect();
    for k in &lk {
        let (x, y) = (
            lines[0].get(*k).copied().unwrap_or(0),
            lines[1].get(*k).copied().unwrap_or(0),
        );
        if x != y {
            rep.push(mm("recovery-lines", k, x.to_string(), y.to_string()));
        }
    }
    rep.inputs.insert(
        "takeover recovery lines (A)".into(),
        lines[0]
            .iter()
            .map(|(k, v)| format!("{v}× {k}"))
            .collect::<Vec<_>>()
            .join(" | "),
    );

    // (v) disk-end: the X-era sessions' final files.
    let x_sessions: BTreeSet<String> = ia
        .keys()
        .chain(ib.keys())
        .filter_map(|r| r.rsplit_once('/').map(|(d, _)| d.to_string()))
        .collect();
    let ends: Vec<BTreeSet<String>> = sides
        .iter()
        .map(|(_, m)| {
            m.end_files()
                .into_iter()
                .filter(|r| {
                    r.rsplit_once('/')
                        .is_some_and(|(d, _)| x_sessions.contains(d))
                })
                .collect()
        })
        .collect();
    for r in ends[0].symmetric_difference(&ends[1]) {
        rep.push(mm(
            "disk-end",
            r,
            ends[0].contains(r).to_string(),
            ends[1].contains(r).to_string(),
        ));
    }
    // (vi) bucket orphans: an inherited session's sidecar in the bucket
    // whose mp4 never landed.
    let orphans: Vec<BTreeSet<String>> = sides
        .iter()
        .map(|(_, m)| {
            let puts = m.successful_puts();
            puts.keys()
                .filter(|k| k.starts_with("media-sessions/") && k.ends_with(".json"))
                .filter(|k| {
                    let seg: Vec<&str> = k.split('/').collect();
                    // media-sessions/v1/{id}/<key>/<sid>/<n>.json
                    seg.len() == 6 && x_sessions.contains(&format!("video/{}/{}", seg[3], seg[4]))
                })
                .filter(|k| !puts.contains_key(&format!("{}.mp4", k.trim_end_matches(".json"))))
                .cloned()
                .collect()
        })
        .collect();
    for k in orphans[0].symmetric_difference(&orphans[1]) {
        rep.push(mm(
            "bucket-orphans",
            k,
            orphans[0].contains(k).to_string(),
            orphans[1].contains(k).to_string(),
        ));
    }
    rep.inputs.insert(
        "takeover bucket orphan sidecars (A | B)".into(),
        format!("{} | {}", orphans[0].len(), orphans[1].len()),
    );
    if expect["reaped"] == true {
        for (i, e) in ends.iter().enumerate() {
            for r in e.iter().filter(|r| r.starts_with("video/")) {
                rep.push(mm(
                    &format!("disk-end {} stranded", ["A", "B"][i]),
                    r,
                    (i == 0).to_string(),
                    (i == 1).to_string(),
                ));
            }
        }
    }
}

/// Is a report row one that makes the lane INVALID (rather than FAIL)?
pub fn is_invalid(m: &Mismatch) -> bool {
    m.stream == STREAM && m.at.starts_with(INVALID)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(size: u64, birth: Option<i64>, name_ms: Option<i64>) -> FileState {
        FileState {
            size,
            birth_ms: birth,
            name_ms,
        }
    }

    fn state() -> Inherited {
        let mut s = Inherited::new();
        let v = "video/cam0/<sid:video:cam0#0>";
        s.insert(format!("{v}/session.json"), f(300, Some(1_000), None));
        s.insert(format!("{v}/00000.mp4"), f(1_000_000, Some(2_000), None));
        s.insert(format!("{v}/00000.json"), f(400, Some(12_000), None));
        s.insert(format!("{v}/00001.mp4"), f(500_000, Some(12_000), None));
        let t = "telemetry/<sid:telemetry#0>";
        s.insert(
            format!("{t}/seg-<ms>-00000.jsonl.gz"),
            f(9_000, Some(61_500), Some(1_500)),
        );
        s.insert(
            format!("{t}/seg-<ms>-00001.jsonl"),
            f(20_000, Some(61_500), Some(61_500)),
        );
        s
    }

    #[test]
    fn classes_split_active_closed_and_open() {
        let c = classes(&state());
        assert_eq!(c["video/cam0: mp4"], 2);
        assert_eq!(c["video/cam0: active mp4 (newest, no sidecar)"], 1);
        assert_eq!(c["video/cam0: closed mp4 + sidecar"], 1);
        assert_eq!(c["video/cam0: session.json"], 1);
        assert_eq!(c["telemetry: segment"], 2);
        assert_eq!(c["telemetry: open .jsonl"], 1);
        assert_eq!(c["telemetry: closed .jsonl.gz"], 1);
        assert!(!c.contains_key("video/cam0: closed mp4 without sidecar"));
    }

    #[test]
    fn expect_min_and_max_are_enforced() {
        let c = classes(&state());
        let ok = json!({"min": {"video/cam0: mp4": 2, "telemetry: segment": 1}, "max": {"video/cam0: mp4": 2}});
        assert!(expect_misses(&c, &ok).is_empty());
        // Negative: one more backlog fragment wanted than there is; a max hit.
        let bad = json!({"min": {"video/cam0: mp4": 3}, "max": {"telemetry: segment": 1}});
        let m = expect_misses(&c, &bad);
        assert_eq!(m.len(), 2, "{m:?}");
        // An unknown class counts 0.
        assert_eq!(
            expect_misses(&c, &json!({"min": {"video/cam1: mp4": 1}})).len(),
            1
        );
    }

    #[test]
    fn edge_guard_flags_knife_edges_only() {
        let none = |_: &str| None;
        // Stop at 95 s: cam0's active 00001 born at 90 s → 5 s old; the open
        // telemetry segment 1 opened at 61.5 s → 33.5 s old.
        let mut s = state();
        s.get_mut("video/cam0/<sid:video:cam0#0>/00001.mp4")
            .unwrap()
            .birth_ms = Some(90_000);
        assert!(edge_misses(&s, 95_000, 10_000, &none).is_empty());
        // Negative: the fragment is 1 s old (just rotated) → flagged.
        assert_eq!(edge_misses(&s, 91_000, 10_000, &none).len(), 1);
        // Negative: the open segment is 59 s old (about to rotate) → flagged.
        let m = edge_misses(&s, 61_500 + 59_000, 100_000, &none);
        assert!(m.iter().any(|x| x.contains("telemetry")), "{m:?}");
        // No birth: the observer's first sighting is the fallback.
        s.get_mut("video/cam0/<sid:video:cam0#0>/00001.mp4")
            .unwrap()
            .birth_ms = None;
        assert_eq!(edge_misses(&s, 95_000, 10_000, &none).len(), 1);
        assert!(edge_misses(&s, 95_000, 10_000, &|_| Some(90_000)).is_empty());
    }

    #[test]
    fn keys_and_fates() {
        assert_eq!(
            s3_key_of("video/cam0/<sid:video:cam0#0>/00001.mp4").unwrap(),
            "media-sessions/v1/{id}/cam0/<sid:video:cam0#0>/00001.mp4"
        );
        assert_eq!(
            s3_key_of("video/cam0/<sid:video:cam0#0>/session.json"),
            None
        );
        assert_eq!(
            s3_key_of("telemetry/<sid:telemetry#0>/seg-<ms>-00001.jsonl").unwrap(),
            "telemetry/v1/{id}/<sid:telemetry#0>/<ms>-00001.jsonl.gz"
        );
        let puts = BTreeMap::from([
            (
                "telemetry/v1/{id}/<sid:telemetry#0>/<ms>-00001.jsonl.gz".to_string(),
                1,
            ),
            (
                "media-sessions/v1/{id}/cam0/<sid:video:cam0#0>/00001.mp4".to_string(),
                2,
            ),
        ]);
        let end = BTreeSet::from(["video/cam0/<sid:video:cam0#0>/session.json".to_string()]);
        let open = "telemetry/<sid:telemetry#0>/seg-<ms>-00001.jsonl";
        assert_eq!(fate(open, &puts, &end), "put×1");
        let mp4 = "video/cam0/<sid:video:cam0#0>/00001.mp4";
        assert_eq!(fate(mp4, &puts, &end), "put×2");
        assert_eq!(
            fate("video/cam0/<sid:video:cam0#0>/session.json", &puts, &end),
            "disk"
        );
        assert_eq!(
            fate("video/cam0/<sid:video:cam0#0>/00000.json", &puts, &end),
            "gone"
        );
        // Negatives: a double upload; a lost non-empty segment. A reaped
        // manifest and a dropped 0-byte fragment are not violations.
        assert!(fate_violations(mp4, &f(1, None, None), "put×2")
            .unwrap()
            .contains("double"));
        assert!(fate_violations(open, &f(10, None, None), "gone")
            .unwrap()
            .contains("lost"));
        assert!(fate_violations(
            "video/cam0/<sid:video:cam0#0>/session.json",
            &f(300, None, None),
            "gone"
        )
        .is_none());
        assert!(fate_violations(mp4, &f(0, None, None), "gone").is_none());
        assert!(fate_violations(mp4, &f(1, None, None), "put×1").is_none());
    }

    #[test]
    fn recovery_lines_are_cut_at_the_phrase_after_the_marker() {
        let text = "[agent] recording: finalized 9 orphan segment(s) from previous run\n\
                    [rig] handoff: incarnation 1 (rust) starts at 5\n\
                    [agent] recording: finalized 1 orphan segment(s) from previous run\n\
                    2026-11-07T00:00:00Z WARN [recording] dropped empty fragment /w/home/x.mp4\n\
                    [agent] telemetry recording started (session 0190…, 1 Hz)\n";
        let got = recovery_lines(text, &|s| s.replace("/w/home", "<work>"));
        assert_eq!(
            got,
            vec![
                "[recording] dropped empty fragment <work>/x.mp4".to_string(),
                "recording: finalized 1 orphan segment(s) from previous run".to_string(),
            ]
        );
        // Without a marker (no handoff) nothing is collected.
        assert!(recovery_lines(
            "[agent] recording: finalized 1 orphan segment(s) from previous run",
            &|s| s.into()
        )
        .is_empty());
    }

    #[test]
    fn only_the_ts_sidecar_reput_is_not_a_double_upload() {
        let at = |w: i64, key: &str, ok: bool| Attempt {
            w,
            key: key.into(),
            ok,
        };
        let sc = "media-sessions/v1/{id}/cam1/<sid:video:cam1#0>/00008.json";
        let mp4 = "media-sessions/v1/{id}/cam1/<sid:video:cam1#0>/00008.mp4";
        // X: sidecar 200, mp4 held + aborted at the cut; Y: sidecar 200, mp4 200.
        let ok = vec![
            at(1, sc, true),
            at(2, mp4, false),
            at(10, sc, true),
            at(11, mp4, true),
        ];
        assert!(sidecar_reput_explained(&ok, sc));
        // NEGATIVE: the mp4 had succeeded before the second sidecar PUT.
        let after_ok = vec![at(1, sc, true), at(2, mp4, true), at(10, sc, true)];
        assert!(!sidecar_reput_explained(&after_ok, sc));
        // NEGATIVE: the failed mp4 attempt is not BETWEEN the two.
        let outside = vec![
            at(1, mp4, false),
            at(2, sc, true),
            at(10, sc, true),
            at(11, mp4, true),
        ];
        assert!(!sidecar_reput_explained(&outside, sc));
        // NEGATIVE: a third success; another fragment's failure; an mp4 key.
        let three = vec![
            at(1, sc, true),
            at(2, mp4, false),
            at(5, sc, true),
            at(6, mp4, false),
            at(10, sc, true),
        ];
        assert!(!sidecar_reput_explained(&three, sc));
        let other = vec![
            at(1, sc, true),
            at(2, &mp4.replace("00008", "00009"), false),
            at(10, sc, true),
        ];
        assert!(!sidecar_reput_explained(&other, sc));
        assert!(!sidecar_reput_explained(&ok, mp4));
    }

    #[test]
    fn the_sidecar_reput_is_cited_only_when_both_sides_show_it() {
        let rel = "video/cam1/<sid:video:cam1#0>/00008.json";
        let both = [
            BTreeSet::from([rel.to_string()]),
            BTreeSet::from([rel.to_string()]),
        ];
        assert_eq!(reput_tol(rel, &both), SIDECAR_REPUT);
        // NEGATIVE: only the handoff side (A) re-PUT it; only the control.
        let a_only = [BTreeSet::from([rel.to_string()]), BTreeSet::new()];
        assert_eq!(reput_tol(rel, &a_only), UNTOLERATED);
        let b_only = [BTreeSet::new(), BTreeSet::from([rel.to_string()])];
        assert_eq!(reput_tol(rel, &b_only), UNTOLERATED);
        // NEGATIVE: an mp4 or a manifest is never a sidecar re-PUT.
        let at = |w: i64, key: &str, ok: bool| Attempt {
            w,
            key: key.into(),
            ok,
        };
        let k = "media-sessions/v1/{id}/cam1/<sid:video:cam1#0>/00008";
        let att = vec![
            at(1, &format!("{k}.json"), true),
            at(2, &format!("{k}.mp4"), false),
            at(3, &format!("{k}.json"), true),
        ];
        assert!(is_sidecar_reput(rel, "put×2", &att));
        assert!(!is_sidecar_reput(
            &rel.replace(".json", ".mp4"),
            "put×2",
            &att
        ));
        assert!(!is_sidecar_reput(
            "video/cam1/<sid:video:cam1#0>/session.json",
            "put×2",
            &att
        ));
        assert!(!is_sidecar_reput(rel, "put×1", &att));
    }

    /// review-P3a F1: `diff_takeover` END TO END on a fixture run (no rig).
    /// One X-era video session (cam0: closed 00000 + sidecar, active 00001)
    /// and one open telemetry segment are inherited on both sides; the clean
    /// twin PASSES. Each plant on side A alone must FAIL (an UNTOLERATED
    /// takeover row, verdict FAIL): a lost segment, an mp4 PUT twice, a
    /// one-side sidecar re-PUT, a stranded X-era session dir (expect.reaped),
    /// a missing recovery line, a bucket-orphan sidecar.
    mod e2e {
        use super::super::*;
        use serde_json::json;

        const VSID: &str = "01a1b781-0000-7000-8000-00000000000c";
        const TSID: &str = "01a1b781-0000-7000-8000-00000000000d";
        const MS: &str = "1794080061000";
        /// The fixture's wall origin (13-digit ms, like the rig's).
        const W: i64 = 1_794_080_000_000;

        #[derive(Clone, Copy, Default)]
        struct Plant {
            lost: bool,
            mp4_twice: bool,
            reput_one_side: bool,
            stranded: bool,
            no_recovery_line: bool,
            bucket_orphan: bool,
        }

        fn write(p: &Path, text: &str) {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }

        /// One side `label` of the fixture run under `run`.
        fn side(run: &Path, label: &str, plant: Plant) {
            let dir = run.join(label);
            let dev = format!("dev-{label}");
            let home = format!("/w/{label}/home");
            write(
                &dir.join("meta.json"),
                &json!({"label": label, "kind": "rust", "deviceId": dev, "home": home, "env": {}}).to_string(),
            );
            let v = format!("video/cam0/{VSID}");
            let t = format!("telemetry/{TSID}");
            let inherited = [
                (format!("{v}/session.json"), 300, 1_000),
                (format!("{v}/00000.mp4"), 900_000, 85_000),
                (format!("{v}/00000.json"), 340, 95_000),
                (format!("{v}/00001.mp4"), 65_000, 95_000),
                (format!("{t}/seg-{MS}-00000.jsonl"), 20_000, 61_000),
            ];
            let files: serde_json::Map<String, Value> = inherited
                .iter()
                .map(|(r, size, birth)| (r.clone(), json!({"size": size, "birthMs": W + birth, "mtimeMs": W + 99_000})))
                .collect();
            write(&dir.join("handoff-state.json"), &json!({"w": W + 100_000, "files": files}).to_string());
            let rec: serde_json::Map<String, Value> = inherited
                .iter()
                .enumerate()
                .map(|(i, (r, size, _))| (r.clone(), json!({"first": W + 1_000 + i as i64, "last": W + 99_000, "size": size, "gone": null})))
                .collect();
            write(&dir.join("recordings.json"), &json!({"files": rec}).to_string());
            // The bucket: everything inherited uploaded once by the next agent
            // (the active fragment with the sidecar its recovery backfilled).
            let vk = format!("media-sessions/v1/{dev}/cam0/{VSID}");
            let mut puts: Vec<(i64, String, u64)> = vec![
                (101_000, format!("telemetry/v1/{dev}/{TSID}/{MS}-00000.jsonl.gz"), 200),
                (101_100, format!("{vk}/00000.json"), 200),
                (101_200, format!("{vk}/00000.mp4"), 200),
                (101_300, format!("{vk}/00001.json"), 200),
                (101_400, format!("{vk}/00001.mp4"), 200),
            ];
            if plant.lost {
                puts.retain(|(_, k, _)| !k.ends_with("00000.mp4"));
            }
            if plant.mp4_twice {
                puts.push((101_500, format!("{vk}/00000.mp4"), 200));
            }
            if plant.reput_one_side {
                // The TS unit retry shape, on this side only.
                puts.push((101_050, format!("{vk}/00000.json"), 200));
                puts.push((101_060, format!("{vk}/00000.mp4"), 503));
            }
            if plant.bucket_orphan {
                puts.push((101_600, format!("{vk}/00002.json"), 200));
            }
            puts.sort();
            let s3: String = puts
                .iter()
                .map(|(w, k, st)| format!("{}\n", json!({"w": W + w, "key": k, "status": st, "sig": "ok", "contentType": "x"})))
                .collect();
            write(&dir.join("s3.ndjson"), &s3);
            // End of run: every X-era file uploaded + removed and the
            // drained session reaped, unless the plant strands its manifest.
            // (A lost mp4 is simply gone: never PUT, not on disk.)
            std::fs::create_dir_all(dir.join("final/recordings")).unwrap();
            if plant.stranded {
                write(&dir.join(format!("final/recordings/{v}/session.json")), "{}");
            }
            write(&dir.join("final/recordings.mp4.json"), "{}");
            let mut out = String::from("[agent] recording: finalized 7 orphan segment(s) from previous run\n");
            out.push_str("[rig] handoff: incarnation 1 (rust) starts at 1\n");
            if !plant.no_recovery_line {
                out.push_str("[agent] recording: finalized 1 orphan segment(s) from previous run\n");
            }
            out.push_str("[agent] recording: backfilled 1 video sidecar(s) from previous run\n");
            write(&dir.join("stdout.log"), &out);
            write(&dir.join("stderr.log"), "[rig] handoff: incarnation 1 (rust) starts at 1\n");
        }

        fn run_with(name: &str, plant_a: Plant, reaped: bool) -> Report {
            let run = std::env::temp_dir().join(format!("p3a-e2e-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&run);
            side(&run, "a", plant_a);
            side(&run, "b", Plant::default());
            write(&run.join("aws.ndjson"), "");
            let ma = MediaCapture::load(&run.join("a"), &[]);
            let mb = MediaCapture::load(&run.join("b"), &[]);
            let h = HandoffInfo {
                at_ms: 100_000,
                signal: "kill".into(),
                stop_w: W + 100_000,
                stopped_w: [W + 100_010, W + 100_010],
                restart_w: W + 102_000,
                spec: json!({"segMs": 10_000, "expect": {
                    "min": {"video/cam0: mp4": 2, "telemetry: segment": 1},
                    "reaped": reaped,
                }}),
                ..Default::default()
            };
            let mut rep = Report::default();
            diff_takeover(&run, &h, &ma, &mb, &mut rep);
            let _ = std::fs::remove_dir_all(&run);
            rep
        }

        fn rows<'r>(rep: &'r Report) -> Vec<&'r Mismatch> {
            rep.mismatches.iter().filter(|m| m.tol == UNTOLERATED).collect()
        }

        #[test]
        fn the_clean_twin_passes_and_every_plant_fails() {
            let clean = run_with("clean", Plant::default(), false);
            assert!(rows(&clean).is_empty(), "{:?}", clean.mismatches);
            assert_eq!(crate::diff::verdict(&clean), "PASS");
            assert_eq!(clean.stats[STREAM].compared, 5);
            let fails = |name: &str, p: Plant, reaped: bool, want: &dyn Fn(&Mismatch) -> bool| {
                let rep = run_with(name, p, reaped);
                let r = rows(&rep);
                assert!(r.iter().any(|m| want(m)), "{name}: {:?}", rep.mismatches);
                assert_eq!(crate::diff::verdict(&rep), "FAIL", "{name}: {:?}", rep.mismatches);
                assert!(!rep.mismatches.iter().any(is_invalid), "{name}: not an INVALID");
            };
            fails(
                "lost",
                Plant { lost: true, ..Default::default() },
                false,
                &|m| m.at == "fate A" && m.a.contains("lost") && m.path.ends_with("00000.mp4"),
            );
            fails(
                "mp4-twice",
                Plant { mp4_twice: true, ..Default::default() },
                false,
                &|m| m.at == "fate A" && m.a.contains("double upload") && m.path.ends_with("00000.mp4"),
            );
            fails(
                "reput-one-side",
                Plant { reput_one_side: true, ..Default::default() },
                false,
                &|m| m.at == "fate A" && m.a.contains("double upload") && m.path.ends_with("00000.json"),
            );
            fails(
                "stranded",
                Plant { stranded: true, ..Default::default() },
                true,
                &|m| m.at == "disk-end A stranded",
            );
            fails(
                "no-recovery-line",
                Plant { no_recovery_line: true, ..Default::default() },
                false,
                &|m| m.at == "recovery-lines" && m.path.starts_with("recording: finalized 1 orphan"),
            );
            fails(
                "bucket-orphan",
                Plant { bucket_orphan: true, ..Default::default() },
                false,
                &|m| m.at == "bucket-orphans" && m.path.ends_with("00002.json"),
            );
            // The same sidecar re-PUT on BOTH sides is the cited TS row, no
            // UNTOLERATED row (`reput_tol`).
            let run = std::env::temp_dir().join(format!("p3a-e2e-both-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&run);
            let p = Plant { reput_one_side: true, ..Default::default() };
            side(&run, "a", p);
            side(&run, "b", p);
            write(&run.join("aws.ndjson"), "");
            let (ma, mb) = (MediaCapture::load(&run.join("a"), &[]), MediaCapture::load(&run.join("b"), &[]));
            let h = HandoffInfo {
                stop_w: W + 100_000,
                restart_w: W + 102_000,
                spec: json!({"segMs": 10_000, "expect": {}}),
                ..Default::default()
            };
            let mut rep = Report::default();
            diff_takeover(&run, &h, &ma, &mb, &mut rep);
            let _ = std::fs::remove_dir_all(&run);
            assert!(rows(&rep).is_empty(), "{:?}", rep.mismatches);
            assert_eq!(rep.mismatches.iter().filter(|m| m.tol == SIDECAR_REPUT).count(), 2);
        }
    }

    #[test]
    fn invalid_rows_are_recognised() {
        assert!(is_invalid(&mm(
            &format!("{INVALID} edge A"),
            "x",
            String::new(),
            String::new()
        )));
        assert!(!is_invalid(&mm("fate", "x", String::new(), String::new())));
    }
}
