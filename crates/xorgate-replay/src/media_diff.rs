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
//! | `sidecar` | key order; every field but the tolerated ones | `startTs` ±300 ms relative to the agent's own capture PLAYING (`New clock:`) time, `startMono` deltas within the session ±300 ms, `durationMs` ±150 ms (an INTERIOR fragment: its end boundary like its start, `T12/interior-boundaries`; the session's last: `T12/cut-fragment`), `sizeBytes` any (T12) |
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
    /// The same orphans by name (`shm_orphans` = its length), sorted.
    pub shm_orphan_names: Vec<String>,
    /// Per orphan (same order): the wall ms its owner child was seen gone.
    pub shm_orphan_gone: Vec<Option<i64>>,
    /// review-close F2: this side's capture children that OUTLIVED its agent
    /// (`procs.ndjson` `alive_at_stop`: alive, or a live member of their own
    /// process group, at the observer's stop), sorted. Their segments count
    /// as orphans; on a Rust side each is UNTOLERATED.
    pub capture_alive_at_stop: Vec<u32>,
    /// The agent did NOT exit within 30 s of the player's SIGTERM and was
    /// SIGKILLed (the player's `[rig] agent <x> did NOT exit in 30 s` line in
    /// `<side>/stderr.log`) — the JS SIGTERM hang (js-bug-triage bug 3).
    pub agent_hung: bool,
    /// Wall ms (publish `w`) of every telemetry tick whose
    /// `recording.powerShed` / `recording.liveBlock` differs from the
    /// previous tick's (`mqtt.ndjson`; T7/D30 `d30_shed_tick_session_end` (d)).
    pub shed_changes: Vec<i64>,
    /// `recording.backlogSegments` of the agent's LAST telemetry tick and
    /// of its final state.json (written at shutdown) — the end-snapshot
    /// per-side bound (review-wp6 FLAG 3).
    pub last_tick_backlog: Option<i64>,
    /// Phase 5b WP4: every live telemetry tick's `metrics.system.health_flags`
    /// value TEXT `(ts, value)` (the agent's own LED state over time).
    pub live_flags: Vec<(i64, String)>,
    /// P3a D59: the same for `metrics.system.led_state` (value TEXT).
    pub live_led: Vec<(i64, String)>,
    /// `(ts, "<level>: <msg>")` of every `power shed →` event seen in the
    /// state.json snapshots (`files.ndjson`; T8/d30-js-reference).
    pub shed_events: Vec<(i64, String)>,
    pub end_backlog: Option<i64>,
    /// `(read wall ms, line)` of every NMEA line this agent READ, from its
    /// own tape (`<run>/<label>/agent-tape.ndjson.gz`, Rust only; empty for
    /// a JS side): the T4/rec-sample epoch anchor (review-wp6 F4).
    pub nmea_reads: Vec<(i64, String)>,
    /// Phase 5b (R-P3G-2): the side's `imuMount` at the end (final
    /// settings.json), for the recorded-IMU input oracle's euler leaves.
    pub imu_mount: Option<xorgate_core::orientation::ImuMount>,
    /// Anything media-related captured at all.
    pub present: bool,
    /// Phase 6 P3a: `(spawn wall, kind)` per incarnation (`meta.json`
    /// `incarnations`; empty before P3a = one incarnation of `kind`).
    pub incarnations: Vec<(i64, String)>,
    /// P3a: each session label's first appearance (wall ms).
    pub session_first: BTreeMap<String, i64>,
    /// P3a: each normalised `recordings/` path's first sighting (wall ms).
    pub rec_first: BTreeMap<String, i64>,
    /// The normaliser of this side (P3a `takeover.rs` normalises with it).
    norm: Norm,
}

impl MediaCapture {
    /// P3a (R2): the kind of the incarnation alive at wall `w`.
    pub fn kind_at(&self, w: i64) -> &str {
        self.incarnations
            .iter()
            .rev()
            .find(|(from, _)| *from <= w)
            .or(self.incarnations.first())
            .map_or(self.kind.as_str(), |(_, k)| k.as_str())
    }

    /// P3a (R2): the kind of the incarnation that started session `label`.
    pub fn kind_of_session(&self, label: &str) -> &str {
        match self.session_first.get(label) {
            Some(w) => self.kind_at(*w),
            None => self.kind.as_str(),
        }
    }

    /// This side's path normalisation (work dir, deviceId, session labels,
    /// telemetry ms stamps).
    pub fn norm_path(&self, p: &str) -> String {
        self.norm.path(p)
    }

    /// When the observer first saw a normalised `recordings/` path.
    pub fn first_seen(&self, rel: &str) -> Option<i64> {
        self.rec_first.get(rel).copied()
    }

    /// Normalised S3 key → number of successful (200) PUTs.
    pub fn successful_puts(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for o in self.s3.iter().filter(|o| o.status == 200) {
            *m.entry(o.key.clone()).or_default() += 1;
        }
        m
    }

    /// Every normalised `recordings/` file present at the end.
    pub fn end_files(&self) -> BTreeSet<String> {
        self.finals
            .keys()
            .chain(self.mp4_final.keys())
            .cloned()
            .collect()
    }
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

#[derive(Debug, Clone, Default)]
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
        // The agent's own tape (Rust only): its NMEA reads and the capture
        // children IT SIGINTed — counted only from a record that proves the
        // SIGINT was DELIVERED to a live child (review-wp6 M5): written after
        // the kill(2) returned (`errno` present) with sig 2 and errno 0;
        // stamped before the child died (the shim's `eof`, else the
        // player's `gone`); and the agent never reaped that child
        // (`child_exit`): it died with its agent, the A3 race. Anything else
        // falls back to gst's own `handling interrupt.` line.
        // P3a: one tape per Rust incarnation (`play::agent_tape_name`).
        let tape: Vec<Value> = (0..16)
            .flat_map(|n| tape_recs(&dir.join(crate::play::agent_tape_name(n))))
            .collect();
        let death: BTreeMap<u32, i64> = shim
            .iter()
            .filter(|v| v["k"] == "eof")
            .chain(procs.iter().filter(|v| v["k"] == "gone"))
            .filter_map(|v| Some((v["pid"].as_u64()? as u32, v["w"].as_i64()?)))
            .fold(BTreeMap::new(), |mut m, (p, w)| {
                m.entry(p)
                    .and_modify(|x: &mut i64| *x = (*x).min(w))
                    .or_insert(w);
                m
            });
        let reaped: BTreeSet<u32> = tape
            .iter()
            .filter(|v| v["k"] == "child_exit")
            .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
            .collect();
        let agent_int: BTreeSet<u32> = tape
            .iter()
            .filter(|v| {
                v["k"] == "child_signal"
                    && v["role"] == "capture"
                    && v["sig"] == 2
                    && v["errno"] == 0
            })
            .filter_map(|v| Some((v["pid"].as_u64()? as u32, v["w"].as_i64()?)))
            .filter(|(p, w)| death.get(p).is_some_and(|d| w < d) && !reaped.contains(p))
            .map(|(p, _)| p)
            .collect();
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
        let mut session_first = BTreeMap::new();
        for (d, mut v) in per_dir {
            v.sort();
            for (i, (w, u)) in v.into_iter().enumerate() {
                // No `/` inside the label: it sits in paths.
                let label = format!("<sid:{}#{i}>", d.replace('/', ":"));
                session_first.insert(label.clone(), w);
                sids.insert(u, label);
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
            // `ok: false` = a planned per-attempt exit found its pid already
            // gone (media.rs `deliver_exit`): that child ended on its own.
            .filter(|v| v["k"] == "external" && v["role"] == "capture" && v["ok"] != false)
            .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
            .collect();
        // P3a (R1): a child the rig's handoff stop signalled while it was
        // alive ended by the RIG's doing (`handoff.rs`), like `external`.
        let handoff_sig: BTreeMap<u32, String> = procs
            .iter()
            .filter(|v| v["k"] == "handoff")
            .filter_map(|v| Some((v["pid"].as_u64()? as u32, v["sig"].as_str()?.to_string())))
            .fold(BTreeMap::new(), |mut m, (p, s)| {
                m.entry(p).or_insert(s);
                m
            });
        for s in spawns.iter_mut() {
            if let Some(sig) = handoff_sig.get(&s.pid) {
                s.end_w = exit_w.get(&s.pid).copied();
                s.end = Some(format!("handoff:{sig}"));
                continue;
            }
            if s.role == "kvs" {
                s.end_w = exit_w.get(&s.pid).copied();
                s.end = kvs_end
                    .get(&s.pid)
                    .cloned()
                    .or_else(|| gone.contains(&s.pid).then(|| "kill".to_string()));
            } else if gone.contains(&s.pid) {
                let ls = lines_of.get(&s.pid).cloned().unwrap_or_default();
                // SIGINTed: gst's own `handling interrupt.` line, or (review-wp6
                // gate run 032624Z) the agent's tape record of sending it. At
                // the agent's stop the TS SIGINTs (video-capture.ts:493-507,
                // worker.ts:1307) and exits tens of ms later, killing the
                // child (execa cleanup; Rust kill_on_drop, A3): under CPU
                // starvation gst can die before its main loop prints the
                // line (A: SIGINT 183368, killed 183389, no line). The line
                // stays the only evidence for a JS side (no tape).
                let int = agent_int.contains(&s.pid)
                    || ls.iter().any(|l| l.contains("handling interrupt"));
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

        let rec_first: BTreeMap<String, i64> = rec_json["files"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(rel, s)| Some((n.path(rel), s["first"].as_i64()?)))
            .collect();
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
        // this side's gst children that is gone. The owner pid is shmsink's
        // SPACE-padded "%5d" field (`shmpipe. 1185.    0`, gst shmpipe.c:307):
        // parsed by the agent's own `shm_owner_pid`. Before Phase 5b WP5 this
        // parsed `" 1185"` as a u32, which fails, so every rig child (4-digit
        // pids) counted 0 and this row could never fail (js-bug-triage §4).
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
        // review-close F2: a capture child that outlived its agent (alive, or
        // its process group, at the stop) strands its segments as surely as
        // a dead one whose segments were never unlinked.
        let capture_alive_at_stop: Vec<u32> = procs
            .iter()
            .filter(|v| v["k"] == "alive_at_stop")
            .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
            .filter(|p| mine.contains(p))
            .collect::<BTreeSet<u32>>()
            .into_iter()
            .collect();
        let shm_orphan_names: Vec<String> = last_names
            .iter()
            .filter(|nm| {
                xorgate_core::capture_ladder::shm_owner_pid(nm).is_some_and(|p| {
                    mine.contains(&p) && (gone.contains(&p) || capture_alive_at_stop.contains(&p))
                })
            })
            .cloned()
            .collect();
        let shm_orphans = shm_orphan_names.len();
        let gone_w: BTreeMap<u32, i64> = procs
            .iter()
            .filter(|v| v["k"] == "gone")
            .filter_map(|v| Some((v["pid"].as_u64()? as u32, v["w"].as_i64()?)))
            .fold(BTreeMap::new(), |mut m, (p, w)| {
                m.entry(p).or_insert(w);
                m
            });
        let shm_orphan_gone: Vec<Option<i64>> = shm_orphan_names
            .iter()
            .map(|nm| {
                xorgate_core::capture_ladder::shm_owner_pid(nm)
                    .and_then(|p| gone_w.get(&p).copied())
            })
            .collect();
        let agent_hung = std::fs::read_to_string(dir.join("stderr.log"))
            .map(|t| {
                t.lines()
                    .any(|l| l.starts_with("[rig] agent ") && l.contains(" did NOT exit in 30 s"))
            })
            .unwrap_or(false);
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
        let mut live_flags = Vec::new();
        let mut live_led = Vec::new();
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
            if let (Some(ts), Some(f)) = (
                p["ts"].as_i64(),
                p.pointer("/metrics/system/health_flags/value"),
            ) {
                live_flags.push((ts, f.to_string()));
            }
            if let (Some(ts), Some(f)) = (
                p["ts"].as_i64(),
                p.pointer("/metrics/system/led_state/value"),
            ) {
                live_led.push((ts, f.to_string()));
            }
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
            capture_alive_at_stop,
            shm_orphan_names,
            shm_orphan_gone,
            agent_hung,
            shed_changes,
            last_tick_backlog,
            live_flags,
            live_led,
            end_backlog,
            shed_events,
            nmea_reads: nmea_lines(&tape, None),
            imu_mount: std::fs::read(dir.join("final.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|f| {
                    f["files"]["settings.json"]["text"]
                        .as_str()
                        .and_then(|t| serde_json::from_str::<Value>(t).ok())
                })
                .and_then(|st| xorgate_core::orientation::parse_imu_mount(&st["imuMount"])),
            present,
            incarnations: meta["incarnations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| Some((v["fromW"].as_i64()?, v["kind"].as_str()?.to_string())))
                .collect(),
            session_first,
            rec_first,
            norm: n,
        }
    }
}

/// Every record of a tape file (gz or plain); empty when absent.
fn tape_recs(path: &Path) -> Vec<Value> {
    let Ok(raw) = std::fs::read(path) else {
        return Vec::new();
    };
    let text = if raw.starts_with(&[0x1f, 0x8b]) {
        gunzip(&raw).unwrap_or_default()
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// `(wall ms, line)` of a tape's `nmea` records: an agent tape's `w` is
/// the agent's own read instant. With `releases` (a player tape), each
/// record's instant is instead its actual release (`releases.ndjson`,
/// matched by tape `t` in order) — never reordered.
fn nmea_lines(recs: &[Value], releases: Option<&[crate::diff::Release]>) -> Vec<(i64, String)> {
    let recs = recs.iter().filter(|v| v["k"] == "nmea");
    let Some(rel) = releases else {
        return recs
            .filter_map(|v| Some((v["w"].as_i64()?, v["line"].as_str()?.to_string())))
            .collect();
    };
    let mut at: BTreeMap<u64, std::collections::VecDeque<i64>> = BTreeMap::new();
    for r in rel.iter().filter(|r| r.k == "nmea") {
        at.entry(r.t).or_default().push_back(r.w);
    }
    recs.filter_map(|v| {
        let w = at.get_mut(&v["t"].as_u64()?)?.pop_front()?;
        Some((w, v["line"].as_str()?.to_string()))
    })
    .collect()
}

/// Per gps leaf path (`metrics.gps.<leaf>.value`), the `(instant, value)`
/// CHANGES one side's collector state went through, replayed from that
/// side's NMEA lines at their own instants through the agent's own core
/// (`FixAggregator::update` + `SkyTracker::note` → `map_latest`, the
/// `collectors/gps.rs` run_stream/sample path; ttff not replayed). The
/// T4/rec-sample value anchor (review-wp6 F4): when each value became
/// current on THAT side — position at its GGA/RMC, `snr_max`/`sats_heard`
/// at the talker's last GSV of the cycle.
pub(crate) type Timeline = BTreeMap<String, Changes>;
/// One leaf's `(instant, value, anchored)` changes on one side; `anchored`
/// = the read that made it current is bounded by the player's release
/// (S-F4, [`bound_reads`]).
pub(crate) type Changes = Vec<(i64, f64, bool)>;

/// review-wp6 S-F4: each self-reported read `(w, line)` is matched to the
/// player's LATEST release of the same line at or before it (`feed`, the
/// release instants; an NMEA epoch's GSV/GSA lines repeat verbatim every
/// second, GGA/RMC carry the UTC time and are unique), looking back at most
/// 2 s; `true` when `read − release ≤ max_lag`. Unmatched (no player tape,
/// a line never released before the read) → `false`. Also returns the
/// matched lags. A repeated line can only match TOO EARLY (a larger lag →
/// unanchored), never too late.
fn bound_reads(
    reads: &[(i64, String)],
    feed: &[(i64, String)],
    max_lag: i64,
) -> (Vec<(i64, String, bool)>, Vec<i64>) {
    let mut lags = Vec::new();
    let out = reads
        .iter()
        .map(|(w, line)| {
            let l = line.trim_end();
            let upto = feed.partition_point(|(r, _)| r <= w);
            let lag = feed[..upto]
                .iter()
                .rev()
                .take_while(|(r, _)| w - r <= 2_000)
                .find(|(_, f)| f.trim_end() == l)
                .map(|(r, _)| w - r);
            if let Some(g) = lag {
                lags.push(g);
            }
            (*w, line.clone(), lag.is_some_and(|g| g <= max_lag))
        })
        .collect();
    (out, lags)
}

fn gps_timeline(reads: &[(i64, String, bool)]) -> Timeline {
    let mut out: Timeline = BTreeMap::new();
    let Some(&(t0, _, _)) = reads.first() else {
        return out;
    };
    let mut agg = xorgate_core::nmea::FixAggregator::new();
    let mut sky = xorgate_core::sky_state::SkyTracker::with_defaults(t0.max(0) as u64);
    let mut latest: Option<xorgate_core::nmea::GpsFix> = None;
    for (w, line, bounded) in reads {
        let now = (*w).max(0) as u64;
        if agg.update(line) {
            latest = Some(agg.fix.clone());
        }
        if line.contains("GSV") {
            if let Some(g) = xorgate_core::nmea::parse_gsv(line) {
                sky.note(&g, now);
            }
        }
        let Some(fix) = &latest else { continue };
        let snap = sky.snapshot(now);
        let group = xorgate_core::gps_group::map_latest(fix, &snap, None);
        let mut leaves = BTreeMap::new();
        leaf_texts(
            &serde_json::to_value(&group).unwrap_or(Value::Null),
            "metrics.gps",
            &mut leaves,
        );
        for (path, text) in leaves {
            let Ok(v) = text.parse::<f64>() else { continue };
            let tl = out.entry(path).or_default();
            if tl.last().is_none_or(|&(_, last, _)| last != v) {
                tl.push((*w, v, *bounded));
            }
        }
    }
    out
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

/// review-wp6 M2 (F3): the system-less samples of one segment that carry the
/// D30 STARVATION signature — isolated (both neighbours carry `system`) and
/// taken 900–999 ms after the previous sample (`mono`). On a steady host the
/// recorder's fires are ≥ 1000 ms apart (timer.rs re-arm from fire), so a gap
/// < 1000 ms proves the PREVIOUS sample was taken late; its gate stamp
/// (`collectors/system.rs` `rec.last_at = mono_ms()` at the call, = TS
/// `system.ts:104-113` `recLastAt` before the compute) then made this call
/// land < 1000 ms after it, and the freshness gate (`recorder_gate.rs:85-89`
/// = `telemetry-recorder.ts:102`) wrote no duplicate `system`. `mono` units
/// follow the segment's own median step (ns/µs/ms).
fn d30_starved_drops(v: &[Value]) -> usize {
    let mono: Vec<Option<f64>> = v.iter().map(|x| x["mono"].as_f64()).collect();
    let mut steps: Vec<f64> = mono
        .windows(2)
        .filter_map(|w| Some(w[1]? - w[0]?))
        .filter(|d| *d > 0.0)
        .collect();
    if steps.is_empty() {
        return 0;
    }
    steps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = steps[steps.len() / 2];
    // One sample per ~1 s (rateHz 1): the median step is ~1 s in the unit.
    let per_ms = if median > 1e8 {
        1e6
    } else if median > 1e5 {
        1e3
    } else {
        1.0
    };
    let has = |i: usize| v[i]["metrics"].get("system").is_some();
    (1..v.len().saturating_sub(1))
        .filter(|&i| v[i]["metrics"].is_object() && !has(i) && has(i - 1) && has(i + 1))
        .filter(|&i| match (mono[i - 1], mono[i]) {
            (Some(p), Some(c)) => {
                let gap = (c - p) / per_ms;
                (900.0..1000.0).contains(&gap)
            }
            _ => false,
        })
        .count()
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

/// review-wp6 F4: a two-sided 1×1 gps hunk — A holds X (its sample at
/// `ta`) where B holds Y (its sample at `tb`) — is two MISSES, not a
/// different value, iff on BOTH sides' own replayed collector state
/// ([`gps_timeline`]) X and Y are CONSECUTIVE values of that leaf (same
/// order on both), so each side's sequence is the input's minus one value;
/// each holder's value became current on its side ≤ 1.1 s before its sample;
/// and each miss passes the one-sided race test on its own: the lacking
/// side's sample nearest the instant the missing value became current ON
/// THAT SIDE lies within `collision` of it (that tick raced that input;
/// `ts` is stamped after the collectors are read, so a tick that read just
/// before can stamp just after). Mechanism: the recorder's 1 Hz tick is
/// ungated by the player (README "Limits") and drifts (`setInterval`
/// re-armed from each fire; mean 1003 ms under load); while its phase
/// crosses the input's phase every sample is a coin flip per side, and two
/// adjacent opposite flips (A: P X · N, B: P · Y N) are what LCS presents as
/// the substitution X/Y. Anything else two-sided stays UNTOLERATED.
fn adjacent_races(
    (la, lb): (&Leaves, &Leaves),
    (ea, eb): (Option<&Changes>, Option<&Changes>),
    ((ta, x), (tb, y)): ((i64, &String), (i64, &String)),
    collision: i64,
) -> bool {
    let (Some(ea), Some(eb), Ok(x), Ok(y)) = (ea, eb, x.parse::<f64>(), y.parse::<f64>()) else {
        return false;
    };
    // The holder's change to `v`: the last at or before its sample, ≤ 1.1 s.
    let held = |e: &[(i64, f64, bool)], v: f64, ts: i64| -> Option<usize> {
        let i = e.iter().rposition(|&(w, u, _)| w <= ts && u == v)?;
        (ts - e[i].0 <= 1_100).then_some(i)
    };
    // The same change on the other side: to `v`, within 500 ms.
    let peer = |e: &[(i64, f64, bool)], v: f64, at: i64| -> Option<usize> {
        e.iter()
            .enumerate()
            .filter(|(_, &(w, u, _))| u == v && (w - at).abs() <= 500)
            .min_by_key(|(_, &(w, _, _))| (w - at).abs())
            .map(|(i, _)| i)
    };
    let raced = |l: &Leaves, at: i64| {
        l.iter()
            .map(|(t, _)| (t - at).abs())
            .min()
            .is_some_and(|o| o <= collision)
    };
    let (Some(iax), Some(iby)) = (held(ea, x, ta), held(eb, y, tb)) else {
        return false;
    };
    let (Some(iay), Some(ibx)) = (peer(ea, y, eb[iby].0), peer(eb, x, ea[iax].0)) else {
        return false;
    };
    let (da, db) = (iay as i64 - iax as i64, iby as i64 - ibx as i64);
    // S-F4: every instant used is a release-bounded read.
    let anchored = [ea[iax].2, ea[iay].2, eb[iby].2, eb[ibx].2]
        .iter()
        .all(|b| *b);
    anchored && da.abs() == 1 && da == db && raced(la, ea[iay].0) && raced(lb, eb[ibx].0)
}

type Leaves = [(i64, BTreeMap<String, String>)];

/// review-wp6 F5 (final-check NIT): on ONE side, how far an interior
/// fragment's end (`startTs + durationMs` = its mtime, the finalize's last
/// write) may sit from its own successor's `startTs` (the next file's
/// birthtime, opened at the rotation). Measured over 240 seams of 12 gate
/// runs under 15 busy loops: −42 … 0 ms, never positive (175 exactly 0).
/// 50 keeps that with headroom while a late finalize — the end drifting
/// past the next start, which T12's ±300 boundary bound alone would admit —
/// fails.
const FRAGMENT_SEAM_MS: f64 = 50.0;

/// [`FRAGMENT_SEAM_MS`] for one fragment and its successor on the same side.
fn seam_ok(v: &Value, next: &Value) -> bool {
    let end = v["startTs"]
        .as_f64()
        .zip(v["durationMs"].as_f64())
        .map(|(s, d)| s + d);
    matches!((end, next["startTs"].as_f64()), (Some(e), Some(n)) if (e - n).abs() <= FRAGMENT_SEAM_MS)
}

/// review-wp6 F5: the fragments' END instants (`startTs + durationMs`),
/// each relative to its own side's capture PLAYING, agree within `bound`.
fn ends_agree(x: &Value, y: &Value, (pa, pb): (Option<i64>, Option<i64>), bound: f64) -> bool {
    let end = |v: &Value, p: Option<i64>| {
        Some(v["startTs"].as_f64()? + v["durationMs"].as_f64()? - p? as f64)
    };
    matches!((end(x, pa), end(y, pb)), (Some(rx), Some(ry)) if (rx - ry).abs() <= bound)
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

/// One recorded sample as `(ts, leaf path → JSON text)` (`rec_values`).
type LeafSample = (i64, BTreeMap<String, String>);

/// T7/D30 `d30_rec_value_gap` (F-E2-1, Phase 4 gate rec-disk run 1,
/// replay-20261021T074334Z): the rail value a JS recorder never wrote
/// because of D30 itself. The tape releases one pmic value `v` between `u`
/// and `w` (consecutive in the released series, ~2 s apart); the JS
/// recorder's sample taken < 1 s after `v`'s release still carries `u` (the
/// shared 1 s `system.sample()` cache was computed before the release —
/// system.ts:104-113), and its next sample carries NO `system` at all (D30's
/// system-less sample: it arrived during the other consumer's in-flight
/// compute, telemetry-recorder.ts:102), so `v` is absent on the JS side
/// only. Covered ONLY when all hold:
/// - `metrics.system.rail_5v.value`; the LACKING side is `js`, the HOLDING
///   side `rust` (D30 is the TS cache; never Rust-lacking, never JS-vs-JS
///   or Rust-vs-Rust);
/// - the hunk is ONE step on the holder (one-sided), whose value is a
///   released pmic value `v` (`%.2f`) with a released predecessor `u` and
///   successor `w`, and every holder sample of that step lies in
///   [release(v), release(w) + 1000 ms) — a value the replay never released
///   at that time (an extra or wrong rail value) is never covered;
/// - on the lacking side, the samples in that window are, in order, one or
///   more carrying `u` taken < 1000 ms after release(v) (stale cache), then
///   one or more WITHOUT `system` (the D30 drop, counted in that session's
///   `(samples without system)` row), then only `w` taken at/after
///   release(w);
/// - every value compared exactly as a 2-dp number (`5.1` = `5.10`; a
///   `5.111` is never the released `5.11`).
fn d30_rec_value_gap(
    p: &str,
    (lack_kind, lack): (&str, &[LeafSample]),
    (hold_kind, hold): (&str, &[LeafSample]),
    only: &[Step],
    pmic: &[(i64, String)],
) -> bool {
    if lack_kind != "js" || hold_kind != "rust" {
        return false;
    }
    d30_gap_signature(p, lack, hold, only, pmic, true)
}

/// The D30 rail-value-gap signature shared by `d30_rec_value_gap` (Rust
/// holding: `need_n` — the lacking JS's system-less sample is required) and
/// the PROPOSED JS-vs-JS arm (`need_n` false: the other JS's fresher cache may
/// jump `u` → `w` directly).
fn d30_gap_signature(
    p: &str,
    lack: &[LeafSample],
    hold: &[LeafSample],
    only: &[Step],
    pmic: &[(i64, String)],
    need_n: bool,
) -> bool {
    const RAIL: &str = "metrics.system.rail_5v.value";
    if p != RAIL {
        return false;
    }
    let [(v, n, ts0)] = only else {
        return false;
    };
    // The JSON number text as the series' `%.2f` — only when it IS a 2-dp
    // value (`5.1` → `5.10`; a `5.111` is no released value: `None`).
    let norm = |t: &str| {
        let x = t.parse::<f64>().ok()?;
        let f = format!("{x:.2}");
        (f.parse::<f64>().ok()? == x).then_some(f)
    };
    let Some(v) = norm(v) else {
        return false;
    };
    // `v`'s release, its predecessor and successor in the released series.
    let Some(k) = (1..pmic.len().saturating_sub(1))
        .rev()
        .find(|&k| pmic[k].1 == v && pmic[k].0 <= *ts0)
    else {
        return false;
    };
    let (u, (rv, _), (rw, w)) = (&pmic[k - 1].1, &pmic[k], &pmic[k + 1]);
    let (lo, hi) = (*rv, *rw + 1000);
    // The holder's samples of that step (the n samples from ts0 on).
    let held: Vec<i64> = hold
        .iter()
        .filter(|(t, o)| *t >= *ts0 && o.get(p).and_then(|x| norm(x)).as_ref() == Some(&v))
        .take(*n)
        .map(|(t, _)| *t)
        .collect();
    if held.len() != *n || held.iter().any(|t| *t < lo || *t >= hi) {
        return false;
    }
    let in_window: Vec<&LeafSample> = lack.iter().filter(|(t, _)| *t >= lo && *t < hi).collect();
    let system_less =
        |o: &BTreeMap<String, String>| !o.keys().any(|k| k.starts_with("metrics.system."));
    // Each JS sample in the window: S = the stale repeat of `u` (< 1 s after
    // v's release: the cache predates it), N = no `system` (D30's drop),
    // W = `w` at/after its release. The D30 signature, IN THAT ORDER: one or
    // more stale repeats, then the system-less sample(s), then only `w`
    // (review-gate-fixes F-R3).
    let class: Option<String> = in_window
        .iter()
        .map(|(t, o)| {
            if system_less(o) {
                return Some('N');
            }
            match o.get(p).and_then(|x| norm(x)) {
                Some(x) if x == *u && *t < *rv + 1000 => Some('S'),
                Some(x) if x == *w && *t >= *rw => Some('W'),
                _ => None,
            }
        })
        .collect();
    let Some(class) = class else {
        return false;
    };
    let rest = class.trim_start_matches('S');
    let after_n = rest.trim_start_matches('N');
    rest.len() < class.len()
        // The reviewed Rust-holding row: unchanged (S+ N+ W*). The proposed
        // JS-vs-JS arm: S+ N* W+ (it must reach `w`).
        && (if need_n { after_n.len() < rest.len() } else { !after_n.is_empty() })
        && after_n.chars().all(|c| c == 'W')
}

/// PROPOSED (Phase 5b WP4, rec-disk `.cal09r2`; inert without its row):
/// `d30_rec_value_gap`'s mechanism INSIDE the reference. Both recorders are
/// JS and both read the shared 1 s `system.sample()` cache
/// (ts:src/agent/collectors/system.ts:104-113), whose compute instant is
/// whichever consumer asks first (D30). So one JS can record a released pmic
/// value `v` (its cache computed between release(v) and release(w)) that the
/// other never records: its samples in [release(v), release(w) + 1000) are,
/// IN ORDER, one or more stale `u` (< 1000 ms after release(v)), then zero or
/// more WITHOUT `system` (telemetry-recorder.ts:102), then only `w` (at/after
/// release(w)). Same guards as the Rust-holding row otherwise: rail_5v only,
/// ONE step on the holder, a released `v` with released neighbours, every
/// holder sample in the window, values exact as 2-dp numbers. The caller
/// requires both sides `js`.
fn d30_js_rec_value_gap(
    p: &str,
    lack: &[LeafSample],
    hold: &[LeafSample],
    only: &[Step],
    pmic: &[(i64, String)],
) -> bool {
    d30_gap_signature(p, lack, hold, only, pmic, false)
}

/// PROPOSED (Phase 5b WP4, rec-disk `.cal09`; inert without its row): a
/// `metrics.system.*` value one JS recorded for ONE sample that the other JS
/// could not record because ITS sample at that instant carried no `system`
/// at all — the D30 system-less sample (ts:src/recording/telemetry-recorder.ts:102,
/// the shared cache's compute in flight, system.ts:104-113; JS-vs-JS T13
/// already accepts the missing group, this is the value it hid). Judged on
/// the whole value sequence (an LCS hunk can pair the transient's neighbours
/// instead): the step is ONE sample, not the first or last step; the lacking
/// side's sample nearest it (≤ 500 ms,
/// half a recorder period) carries no `metrics.system.*` leaf; and the lacking
/// side's samples just before and after that one carry `system`; and with
/// every such transient removed, both sides' value sequences are EQUAL. A
/// value held two samples, a lacking sample that HAS `system`, two adjacent
/// system-less samples, or any other difference stay UNTOLERATED. Both sides
/// `js` only.
/// The indices of `steps` (one side) that are such one-sample transients
/// against the other side's samples `other`.
fn system_less_transients(p: &str, steps: &[Step], other: &[LeafSample]) -> Vec<usize> {
    (1..steps.len().saturating_sub(1))
        .filter(|&i| d30_js_system_less_value(p, other, &steps[i]))
        .collect()
}

/// The LED/health leaves D59 may apply to (agent-computed on the LED
/// heartbeat, ts:src/worker.ts:703-705, ts:src/lib/led-state.ts:223-253).
const D59_LEAVES: &[&str] = &[
    "metrics.system.health_flags.value",
    "metrics.system.led_state.value",
];

/// P3a D59: how long after a boot (the run's start or a handoff restart,
/// `PlayInfo::boot_wall`) a recorded LED/health blip counts as the boot
/// transient — the startup window of the diff's boot rules (5 s).
const D59_BOOT_MS: i64 = 5_000;
/// `led_state_both_held`'s window: one LED heartbeat.
const D59_LIVE_MS: i64 = 1_000;

/// P3a D59 (P3a-takeover.md, D-row text there): a BOOT-STARTED recording
/// session (recording on in settings.json at boot — every handoff, and a
/// fielded cold boot) samples before the agent's first LED evaluation, so
/// its first sample is the pre-LED `(health_flags 0, led_state starting)`,
/// and the one-sample LED states of the boot transient that follow land
/// MID-sequence (a split run), where `led_state_both_held` (the edge-only
/// `fix_edge` arm) cannot reach them. `Some(drops)` when the two value
/// sequences of an LED/health leaf differ ONLY by such blips: a single
/// sample of value v between two samples of the same value u, at most
/// [`D59_BOOT_MS`] after its boot, in a session whose first sample is too on
/// BOTH sides, and that BOTH sides' live telemetry carried within
/// ±[`D59_LIVE_MS`] (both LEDs held v; which recorder caught it is each
/// one's own 1 s phase). Any kind pair (a timing race, not a kind
/// difference). Anything else: None.
fn d59_boot_led_blips(
    p: &str,
    play: &PlayInfo,
    kinds: (&str, &str),
    (la, lb): (&[LeafSample], &[LeafSample]),
    (live_a, live_b): (LiveSeries, LiveSeries),
) -> Option<Vec<(bool, Step)>> {
    if !D59_LEAVES.contains(&p) {
        return None;
    }
    let vals = |l: &[LeafSample]| -> Vec<(i64, String)> {
        l.iter()
            .filter_map(|(ts, o)| o.get(p).map(|v| (*ts, v.clone())))
            .collect()
    };
    let (va, vb) = (vals(la), vals(lb));
    let after_boot = |ts: i64| ts - play.boot_wall(ts) <= D59_BOOT_MS;
    let boot_started = |v: &[(i64, String)]| v.first().is_some_and(|(ts, _)| after_boot(*ts));
    if !boot_started(&va) || !boot_started(&vb) {
        return None;
    }
    let near = |l: LiveSeries, ts: i64, v: &str| {
        l.iter()
            .any(|(t, x)| (t - ts).abs() <= D59_LIVE_MS && x == v)
    };
    let blips = |v: &[(i64, String)]| -> Vec<usize> {
        (1..v.len().saturating_sub(1))
            .filter(|&i| {
                let (ts, x) = (v[i].0, &v[i].1);
                v[i - 1].1 != *x
                    && v[i - 1].1 == v[i + 1].1
                    && after_boot(ts)
                    && near(live_a, ts, x)
                    && near(live_b, ts, x)
            })
            .collect()
    };
    // review-P3a F2: the side LACKING a blip must be a JS incarnation (the
    // TS post-await LED read, worker.ts:544, races the heartbeat armed a few
    // ms after the recorder, :597 vs :703; Rust arms its heartbeat after the
    // connect, worker.rs:828, so it reads the previous tick deterministically
    // and never lacks one). A Rust side lacking a blip stays judged.
    let da = if kinds.1 == "js" {
        blips(&va)
    } else {
        Vec::new()
    };
    let db = if kinds.0 == "js" {
        blips(&vb)
    } else {
        Vec::new()
    };
    if da.is_empty() && db.is_empty() {
        return None;
    }
    let dedup = |v: &[(i64, String)], d: &[usize]| -> Vec<String> {
        let mut o: Vec<String> = Vec::new();
        for (i, (_, x)) in v.iter().enumerate() {
            if !d.contains(&i) && o.last() != Some(x) {
                o.push(x.clone());
            }
        }
        o
    };
    (dedup(&va, &da) == dedup(&vb, &db)).then(|| {
        da.iter()
            .map(|&i| (true, (va[i].1.clone(), 1, va[i].0)))
            .chain(db.iter().map(|&i| (false, (vb[i].1.clone(), 1, vb[i].0))))
            .collect()
    })
}

fn d30_js_system_less_value(p: &str, lack: &[LeafSample], step: &Step) -> bool {
    if !p.starts_with("metrics.system.") {
        return false;
    }
    let (_, 1, ts) = step else {
        return false;
    };
    let has_sys = |o: &BTreeMap<String, String>| o.keys().any(|k| k.starts_with("metrics.system."));
    let Some(i) = (0..lack.len()).min_by_key(|&i| (lack[i].0 - ts).abs()) else {
        return false;
    };
    (lack[i].0 - ts).abs() <= 500
        && !has_sys(&lack[i].1)
        && i > 0
        && i + 1 < lack.len()
        && has_sys(&lack[i - 1].1)
        && has_sys(&lack[i + 1].1)
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
///   is UNTOLERATED — except (review-wp6 F4) a gps 1×1 pair proven to be two
///   adjacent misses, each raced at its own side's read (`adjacent_races`).
/// - per group (`system`: the `(samples without system)` check above), the
///   count of samples carrying it: |Δ| ≤ that group's input transitions in
///   the stretch ±1.5 s (T4, `InputFacts::groups`).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn rec_values(
    sid: &str,
    (a, b): (&BTreeMap<u64, String>, &BTreeMap<u64, String>),
    ep: &(Timeline, Timeline),
    play: &PlayInfo,
    facts: &crate::diff::InputFacts,
    kinds: (&str, &str),
    pmic: &[(i64, String)],
    table: &Table,
    rep: &mut Report,
) {
    rec_values_m(
        sid,
        (a, b),
        ep,
        play,
        facts,
        kinds,
        pmic,
        table,
        rep,
        (None, None),
        (&[], &[]),
        (&[], &[]),
    )
}

#[allow(clippy::too_many_arguments)]
fn rec_values_m(
    sid: &str,
    (a, b): (&BTreeMap<u64, String>, &BTreeMap<u64, String>),
    (ea, eb): &(Timeline, Timeline),
    play: &PlayInfo,
    facts: &crate::diff::InputFacts,
    kinds: (&str, &str),
    pmic: &[(i64, String)],
    table: &Table,
    rep: &mut Report,
    mounts: (
        Option<&xorgate_core::orientation::ImuMount>,
        Option<&xorgate_core::orientation::ImuMount>,
    ),
    live: (LiveSeries, LiveSeries),
    live_led: (LiveSeries, LiveSeries),
) {
    let id = "T4/rec-sample";
    let d30_gap_row = table
        .matcher_rows("d30_rec_value_gap", "rec-telemetry")
        .first()
        .cloned();
    // PROPOSED (Phase 5b WP4, for the reviewer; inert until its row exists in
    // tolerances.toml — see phase-5b/evidence/wp4.md "rec-disk proposals"):
    // the two D30 recorded-value gaps INSIDE the reference (JS-vs-JS only).
    let d30_js_gap_row = table
        .matcher_rows("d30_js_rec_value_gap", "rec-telemetry")
        .first()
        .cloned();
    let d30_js_nosys_row = table
        .matcher_rows("d30_js_system_less_value", "rec-telemetry")
        .first()
        .cloned();
    // P3a D59 (inert until its row exists in tolerances.toml): the boot
    // transient's one-sample LED/health states of a boot-started session.
    let d59_row = table
        .matcher_rows("d59_boot_led_blip", "rec-telemetry")
        .first()
        .cloned();
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
        // R-P3G-2, high-rate IMU: each side's recorded IMU sample is checked
        // against the TAPE: its quat/accel (and euler, with that side's
        // imuMount or none) must be exactly `map_latest` of one of the IMU
        // lines released shortly before its read instant. Those samples are
        // proven genuine reads of the input; pairs of two such samples are
        // then not cross-compared (they read different 16 Hz lines).
        let (va_imu, vb_imu) = if has_row(table, REC_INPUT_BETWEEN) && !facts.imu_samples.is_empty()
        {
            (
                imu_oracle(&la, &facts.imu_samples, mounts.0),
                imu_oracle(&lb, &facts.imu_samples, mounts.1),
            )
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        // … and gps the same way, for its FIX leaves (the tape's NMEA through
        // the fix aggregator); the sky leaves keep the timing rule.
        let (va_gps, vb_gps) = if has_row(table, REC_INPUT_BETWEEN) && !facts.gps_snaps.is_empty() {
            (
                snap_oracle(&la, "gps", &facts.gps_snaps),
                snap_oracle(&lb, "gps", &facts.gps_snaps),
            )
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        let imu_ws: Vec<i64> = facts.imu_samples.iter().map(|(w, _)| *w).collect();
        let gps_ws: Vec<i64> = facts.gps_snaps.iter().map(|(w, _)| *w).collect();
        for (g, va, vb, ws) in [
            ("imu", &va_imu, &vb_imu, &imu_ws),
            ("gps", &va_gps, &vb_gps, &gps_ws),
        ] {
            if let Some((summary, failed)) = oracle_lag_check(&la, &lb, va, vb, ws) {
                rep.inputs
                    .insert(format!("T4 oracle {g} {at}"), summary.clone());
                if failed {
                    rep.push(mm(
                        "rec-telemetry",
                        &at,
                        &format!("metrics.{g}(oracle input lag)"),
                        summary,
                        format!("≤ {ORACLE_STALE_MS} ms on each side"),
                        UNTOLERATED,
                    ));
                }
            }
        }
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
            let group = p.split('.').nth(1).unwrap_or("");
            let rel = walls(group);
            // R-P3G-2 (phase-3 gate, Phase 5b WP4): pair the two sides'
            // recorder ticks by time and drop, for THIS leaf only, each pair
            // whose two reads straddle a release of the leaf's group input
            // (they read different inputs: a real 16 Hz IMU or a 1 Hz NMEA
            // epoch between two ticks ~250 ms apart) and whose values differ.
            // Every other pair keeps its exact comparison below.
            let gps_fix_leaf = group == "gps"
                && p.strip_prefix("metrics.gps.")
                    .and_then(|r| r.strip_suffix(".value"))
                    .is_some_and(|l| GPS_FIX_LEAVES.contains(&l));
            let (la_p, lb_p, dropped) = if group == "imu" {
                // IMU: only the tape oracle removes pairs (never timing alone).
                oracle_filter(&la, &lb, p, &va_imu, &vb_imu)
            } else if gps_fix_leaf && !(va_gps.is_empty() && vb_gps.is_empty()) {
                oracle_filter(&la, &lb, p, &va_gps, &vb_gps)
            } else if has_row(table, REC_INPUT_BETWEEN) && !rel.is_empty() {
                straddle_filter(&la, &lb, p, &rel, collision)
            } else {
                (la.clone(), lb.clone(), 0)
            };
            if dropped > 0 {
                rep.push(mm(
                    "rec-telemetry",
                    &at,
                    &format!("{p}(paired reads straddling an input)"),
                    format!("{dropped} pair(s)"),
                    format!("{dropped} pair(s)"),
                    REC_INPUT_BETWEEN,
                ));
            }
            let (la, lb) = (la_p, lb_p);
            let (sa, sb) = (steps(&la, p), steps(&lb, p));
            if sa.iter().map(|s| &s.0).eq(sb.iter().map(|s| &s.0)) {
                continue;
            }
            // PROPOSED (inert without its row): one-sample values a JS
            // recorded where the other JS's sample carried no `system`.
            if let Some(rid) = d30_js_nosys_row
                .as_ref()
                .filter(|_| kinds.0 == "js" && kinds.1 == "js")
            {
                let (ka, kb) = (
                    system_less_transients(p, &sa, &lb),
                    system_less_transients(p, &sb, &la),
                );
                let keep = |st: &[Step], drop: &[usize]| -> Vec<String> {
                    let mut v: Vec<String> = Vec::new();
                    for (i, s) in st.iter().enumerate() {
                        if !drop.contains(&i) && v.last() != Some(&s.0) {
                            v.push(s.0.clone());
                        }
                    }
                    v
                };
                if !(ka.is_empty() && kb.is_empty()) && keep(&sa, &ka) == keep(&sb, &kb) {
                    for (st, drop, side_a) in [(&sa, &ka, true), (&sb, &kb, false)] {
                        for &i in drop {
                            let f = format!("{}×{}@{}", st[i].0, st[i].1, st[i].2);
                            let (x, y) = if side_a {
                                (f, String::new())
                            } else {
                                (String::new(), f)
                            };
                            rep.push(mm("rec-telemetry", &at, &format!("{p}(values)"), x, y, rid));
                        }
                    }
                    continue;
                }
            }
            // P3a D59 (`d59_boot_led_blips`): the LED/health leaves only, a
            // boot-started session's mid-sequence one-sample boot states that
            // BOTH sides' live telemetry carried. Every such blip removed,
            // the sequences must be EQUAL; anything else stays judged below.
            if let Some(rid) = d59_row.as_ref() {
                let lv = if p.ends_with("led_state.value") {
                    live_led
                } else {
                    live
                };
                if let Some(drops) = d59_boot_led_blips(p, play, kinds, (&la, &lb), lv) {
                    for (side_a, st) in drops {
                        let f = format!("{}×{}@{}", st.0, st.1, st.2);
                        let (x, y) = if side_a {
                            (f, String::new())
                        } else {
                            (String::new(), f)
                        };
                        rep.push(mm("rec-telemetry", &at, &format!("{p}(values)"), x, y, rid));
                    }
                    continue;
                }
            }
            // NARROWED (review-wp6 re-check): the colliding release is THE
            // one that brought the step on the side holding it (the last
            // release of the group at or before that sample, ≤ 1.1 s before
            // it), and the colliding sample is the OTHER side's sample
            // nearest that release — at 1 Hz input a phase sitting near the
            // releases must not excuse every step it passes.
            // A release = one input BURST (an NMEA epoch is ~8 lines over
            // ~60 ms): releases ≤ 200 ms apart form one `(first, last)`.
            let mut bursts: Vec<(i64, i64)> = Vec::new();
            let mut ws = rel.clone();
            ws.sort();
            for w in ws {
                match bursts.last_mut() {
                    Some(b) if w - b.1 <= 200 => b.1 = w,
                    _ => bursts.push((w, w)),
                }
            }
            let raced = |l: &[(i64, BTreeMap<String, String>)], ts: i64| {
                let Some(&(r0, r1)) = bursts.iter().rfind(|b| b.0 <= ts) else {
                    return false;
                };
                let off = |t: i64| (r0 - t).max(t - r1).max(0);
                ts - r0 <= 1_100
                    && l.iter()
                        .map(|(t, _)| *t)
                        .min_by_key(|t| off(*t))
                        .is_some_and(|t| off(t) <= collision)
            };
            for (start, only_a, only_b) in step_hunks(&sa, &sb) {
                let edge = start == 0 || start + only_a.len() >= sa.len();
                let one_sided = only_a.is_empty() != only_b.is_empty();
                // review-wp6 F4: two ADJACENT misses, one per side (see
                // `adjacent_races`), which LCS shows as one substitution.
                let swap = has
                    && group == "gps"
                    && only_a.len() == 1
                    && only_b.len() == 1
                    && only_a[0].1 == 1
                    && only_b[0].1 == 1
                    && adjacent_races(
                        (la.as_slice(), lb.as_slice()),
                        (ea.get(p.as_str()), eb.get(p.as_str())),
                        ((only_a[0].2, &only_a[0].0), (only_b[0].2, &only_b[0].0)),
                        collision,
                    );
                let ok = swap
                    || (has
                        && one_sided
                        && only_a.iter().chain(only_b.iter()).all(|s| s.1 == 1)
                        // One race skips ONE value; two or more consecutive
                        // missing steps need a ≥ 2 s gap, not a raced tick.
                        && only_a.len() + only_b.len() == 1
                        && (if edge {
                            true
                        } else if only_b.is_empty() {
                            only_a.iter().all(|s| raced(&lb, s.2))
                        } else {
                            only_b.iter().all(|s| raced(&la, s.2))
                        }));
                // T7/D30 `d30_rec_value_gap` (F-E2-1): the JS side skipped
                // one released rail value through D30 (stale cache + a
                // system-less sample), the Rust side shows it.
                let d30_gap = !ok
                    && one_sided
                    && d30_gap_row.is_some()
                    && if only_b.is_empty() {
                        d30_rec_value_gap(p, (kinds.1, &lb), (kinds.0, &la), &only_a, pmic)
                    } else {
                        d30_rec_value_gap(p, (kinds.0, &la), (kinds.1, &lb), &only_b, pmic)
                    };
                // PROPOSED calibration-only arms (both sides js).
                let both_js = kinds.0 == "js" && kinds.1 == "js";
                let (hold, lack, only) = if only_b.is_empty() {
                    (&la, &lb, &only_a)
                } else {
                    (&lb, &la, &only_b)
                };
                let d30_js_gap = !ok
                    && !d30_gap
                    && one_sided
                    && both_js
                    && d30_js_gap_row.is_some()
                    && d30_js_rec_value_gap(p, lack, hold, only, pmic);
                // A one-sample LED-state value one RECORDER caught that BOTH
                // agents held: each side's live telemetry carried it within a
                // second of that sample (the LED heartbeat resolves the state,
                // ts:src/worker.ts:703-705; each recorder samples it on its own
                // 1 s phase, ts:src/recording/telemetry-recorder.ts:96-106, so
                // a state held < 1 s can fall between one recorder's reads --
                // the window tape's boot: JS 304 recorded, Rust 288, both
                // published 304 at their first tick).
                let (live_h, live_o) = if only_b.is_empty() {
                    (live.0, live.1)
                } else {
                    (live.1, live.0)
                };
                let fix_edge = !ok
                    && !d30_gap
                    && !d30_js_gap
                    && one_sided
                    && has_row(table, REC_INPUT_BETWEEN)
                    && led_state_both_held(p, only, live_h, live_o);
                let fmt = |v: &[Step]| {
                    v.iter()
                        .map(|s| format!("{}×{}@{}", s.0, s.1, s.2))
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let tol = if fix_edge {
                    REC_INPUT_BETWEEN.to_string()
                } else if ok {
                    id.to_string()
                } else if d30_gap {
                    d30_gap_row
                        .clone()
                        .unwrap_or_else(|| UNTOLERATED.to_string())
                } else if d30_js_gap {
                    d30_js_gap_row
                        .clone()
                        .unwrap_or_else(|| UNTOLERATED.to_string())
                } else {
                    UNTOLERATED.to_string()
                };
                rep.push(mm(
                    "rec-telemetry",
                    &at,
                    &format!("{p}(values)"),
                    fmt(&only_a),
                    fmt(&only_b),
                    &tol,
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
            // Phase 5b WP4: gps presence explained sample by sample on EACH
            // side by the tape (`carry_consistent`) — the two recorders read
            // at their own phases on either side of an NMEA epoch.
            // (imu: every released IMU line advances the TS imu cache,
            // ts:src/agent/collectors/imu.ts:81-106.)
            let advances: &[i64] = match g.as_str() {
                "gps" => &facts.gps_advances,
                "imu" => &facts.imu_lines,
                _ => &[],
            };
            let carry_ok = has_row(table, REC_INPUT_BETWEEN)
                && !advances.is_empty()
                && carry_consistent(&la, advances, &g)
                && carry_consistent(&lb, advances, &g);
            rep.push(mm(
                "rec-telemetry",
                &at,
                &format!("metrics.{g}(samples carrying)"),
                na.to_string(),
                nb.to_string(),
                if na.abs_diff(nb) <= trans {
                    "T4"
                } else if carry_ok {
                    REC_INPUT_BETWEEN
                } else {
                    UNTOLERATED
                },
            ));
        }
    }
}

/// A tape input (IMU line / gps snapshot) is "recent" for a recorded sample
/// when it is no older than ORACLE_LAG_MS before the NEWEST one the tape
/// released by the sample's read instant (+ the 50 ms delivery margin).
/// Review-wp4 R4-10: was the last 24 IMU lines / 40 gps snapshots. As a
/// TIME span, calibrated on the window tape (JS@09 vs Rust .fin.jsrs09): a
/// genuine read can match a line up to ~1.5 s before the newest release by
/// its read (the hold gate releases IMU lines in bursts after pair windows;
/// the recorder reads its cache on its own phase): 400 ms → 361 untolerated,
/// 800 → 58, 1 000 → 6, 1 500 → 0. A STALE side is caught instead by the
/// median-lag check (ORACLE_STALE_MS).
const ORACLE_LAG_MS: i64 = 1_500;

/// Review-wp4 R4-10: a STALE side. For each verified sample, its LAG is the
/// release of the NEWEST input the tape had delivered by the read (`oracle_
/// span`'s end) minus the release of the input the sample matched (the
/// oracle takes the newest match, so an unchanged value never lags). A
/// recorder reading what was current lags 0 bar read/release races; one
/// reading 1 s-stale inputs lags ~1 s on every changing value. Each side's
/// MEDIAN lag must stay within ORACLE_STALE_MS. (Was a cross-side compare of
/// median AGES, read − matched release: that measures each recorder's phase
/// against the 1 s NMEA epoch grid, which differs per side — stepC
/// p5b-clock-fallback.r27.jsrs1 A −8 ms / B 803 ms, both lagging 0.)
const ORACLE_STALE_MS: i64 = 500;

/// The per-group median-lag check (ORACLE_STALE_MS): `(summary, failed)`.
fn oracle_lag_check(
    la: &RecSamples,
    lb: &RecSamples,
    va: &BTreeMap<usize, i64>,
    vb: &BTreeMap<usize, i64>,
    ws: &[i64],
) -> Option<(String, bool)> {
    let med = |l: &RecSamples, v: &BTreeMap<usize, i64>| -> Option<i64> {
        let reads = read_instants(l);
        let mut lags: Vec<i64> = v
            .iter()
            .filter_map(|(i, w)| {
                let (_, end) = oracle_span(ws, reads[*i]);
                Some(ws[end.checked_sub(1)?] - w)
            })
            .collect();
        lags.sort();
        lags.get(lags.len() / 2).copied()
    };
    let (ma, mb) = (med(la, va), med(lb, vb));
    if ma.is_none() && mb.is_none() {
        return None;
    }
    let show = |m: Option<i64>| m.map_or("-".to_string(), |x| format!("{x} ms"));
    Some((
        format!(
            "median lag behind the newest input A {} / B {} (≤ {ORACLE_STALE_MS} ms)",
            show(ma),
            show(mb)
        ),
        ma.is_some_and(|x| x > ORACLE_STALE_MS) || mb.is_some_and(|x| x > ORACLE_STALE_MS),
    ))
}

/// The `[start, end)` of `ws` (sorted release instants) the oracle may match
/// for a read at `read`: released by read + 50 and within ORACLE_LAG_MS of
/// the newest such release.
fn oracle_span(ws: &[i64], read: i64) -> (usize, usize) {
    let end = ws.partition_point(|w| *w <= read + 50);
    let Some(newest) = end.checked_sub(1).map(|i| ws[i]) else {
        return (0, 0);
    };
    (ws.partition_point(|w| *w < newest - ORACLE_LAG_MS), end)
}

/// R-P3G-2: the indices of `l` whose every `metrics.imu.*` leaf equals
/// `map_latest` (the agent's own mapping, xorgate_core::imu_group, parity
/// with the TS `mapLatest`, ts:src/agent/collectors/imu.ts:81-106) of one IMU
/// line the tape released before that sample's read instant — within
/// ORACLE_LAG_MS of the newest one (`oracle_span`). euler is tried with the side's
/// final `imuMount` and with none.
fn imu_oracle(
    l: &RecSamples,
    lines: &[(i64, xorgate_core::imu::ImuSample)],
    mount: Option<&xorgate_core::orientation::ImuMount>,
) -> BTreeMap<usize, i64> {
    let reads = read_instants(l);
    let mut out = BTreeMap::new();
    for (i, (_, o)) in l.iter().enumerate() {
        let imu: BTreeMap<&str, &String> = o
            .iter()
            .filter_map(|(k, v)| {
                let leaf = k.strip_prefix("metrics.imu.")?.strip_suffix(".value")?;
                Some((leaf, v))
            })
            .collect();
        if imu.is_empty() {
            continue;
        }
        let ws: Vec<i64> = lines.iter().map(|(w, _)| *w).collect();
        let (start, end) = oracle_span(&ws, reads[i]);
        let hit = lines[start..end].iter().rev().find(|(_, smp)| {
            [mount, None].iter().any(|m| {
                let g = serde_json::to_value(xorgate_core::imu_group::map_latest(smp, *m))
                    .unwrap_or(Value::Null);
                // The TEXT (value + JS number format, T9) must match.
                imu.iter().all(|(leaf, v)| {
                    // serde_json keeps 1 (u64) ≠ 1.0 (f64): the format too.
                    serde_json::from_str::<Value>(v).ok().as_ref() == Some(&g[*leaf]["value"])
                })
            })
        });
        if let Some((w, _)) = hit {
            out.insert(i, *w);
        }
    }
    out
}

/// R-P3G-2 shape half: does every sample of `seg` carrying the leaf `p`
/// (an IMU leaf, or a gps fix leaf) pass the tape oracle?
fn segment_oracle_ok(
    seg: &[Value],
    p: &str,
    facts: &crate::diff::InputFacts,
    mount: Option<&xorgate_core::orientation::ImuMount>,
) -> bool {
    let l: RecSamples = seg
        .iter()
        .filter(|x| x["metrics"].is_object())
        .map(|x| {
            let mut o = BTreeMap::new();
            leaf_texts(&x["metrics"], "metrics", &mut o);
            (x["ts"].as_i64().unwrap_or(0), o)
        })
        .collect();
    let carrying: Vec<usize> = (0..l.len()).filter(|i| l[*i].1.contains_key(p)).collect();
    if carrying.is_empty() {
        return false;
    }
    let ok = if p.starts_with("metrics.imu.") && !facts.imu_samples.is_empty() {
        imu_oracle(&l, &facts.imu_samples, mount)
    } else if p
        .strip_prefix("metrics.gps.")
        .and_then(|r| r.strip_suffix(".value"))
        .is_some_and(|x| GPS_FIX_LEAVES.contains(&x))
        && !facts.gps_snaps.is_empty()
    {
        snap_oracle(&l, "gps", &facts.gps_snaps)
    } else {
        return false;
    };
    carrying.iter().all(|i| ok.contains_key(i))
}

/// The gps leaves the NMEA oracle verifies: the fix-derived ones
/// (`gps_fix_leaves`) and (Phase 5b WP4) the sky reductions `sats_heard` /
/// `snr_max` (`gps_sky_leaves`). `sky_state` stays out: its hysteresis
/// advances at each read instant, not at a tape release.
const GPS_FIX_LEAVES: &[&str] = &[
    "fix_quality",
    "satellites",
    "satellites_in_view",
    "sats_heard",
    "snr_max",
    "hdop",
    "lat",
    "lon",
    "alt",
    "speed",
    "course",
];

/// R-P3G-2: the indices of `l` whose fix-derived `metrics.<group>.*` leaves
/// equal ONE tape snapshot within ORACLE_LAG_MS of the newest before the sample's
/// read instant (+50 ms), leaf for leaf (a leaf present on one only fails);
/// a sample showing a fix the tape never gave there is not verified.
fn snap_oracle(
    l: &RecSamples,
    group: &str,
    snaps: &[(i64, BTreeMap<String, String>)],
) -> BTreeMap<usize, i64> {
    let reads = read_instants(l);
    let pre = format!("metrics.{group}.");
    let mut out = BTreeMap::new();
    for (i, (_, o)) in l.iter().enumerate() {
        let leaves: BTreeMap<&str, &String> = o
            .iter()
            .filter_map(|(k, v)| {
                let leaf = k.strip_prefix(pre.as_str())?.strip_suffix(".value")?;
                GPS_FIX_LEAVES.contains(&leaf).then_some((leaf, v))
            })
            .collect();
        if leaves.is_empty() {
            continue;
        }
        let ws: Vec<i64> = snaps.iter().map(|(w, _)| *w).collect();
        let (start, end) = oracle_span(&ws, reads[i]);
        let hit = snaps[start..end].iter().rev().find(|(_, s)| {
            GPS_FIX_LEAVES
                .iter()
                .all(|leaf| match (leaves.get(leaf), s.get(*leaf)) {
                    (None, None) => true,
                    (Some(v), Some(x)) => *v == x,
                    _ => false,
                })
        });
        if let Some((w, _)) = hit {
            out.insert(i, *w);
        }
    }
    out
}

/// R-P3G-2: remove, for the IMU leaf `p`, every time-paired couple of two
/// oracle-verified samples whose values differ, and every UNPAIRED verified
/// sample (the per-group presence check judges counts). A sample the oracle
/// could not verify stays and is judged exactly.
fn oracle_filter(
    la: &RecSamples,
    lb: &RecSamples,
    p: &str,
    va: &BTreeMap<usize, i64>,
    vb: &BTreeMap<usize, i64>,
) -> (RecSamples, RecSamples, usize) {
    let (ra, rb) = (read_instants(la), read_instants(lb));
    let (mut drop_a, mut drop_b) = (BTreeSet::new(), BTreeSet::new());
    let mut paired_b = BTreeSet::new();
    let mut j = 0usize;
    let mut n = 0usize;
    for i in 0..la.len() {
        while j < lb.len() && rb[j] < ra[i] - REC_PAIR_MS {
            j += 1;
        }
        let k = (j..lb.len())
            .take_while(|k| rb[*k] <= ra[i] + REC_PAIR_MS)
            .min_by_key(|k| (rb[*k] - ra[i]).abs());
        match k {
            Some(k) => {
                j = k + 1;
                paired_b.insert(k);
                // A side without the leaf in that sample skipped the group
                // (not updated since its last write, telemetry-recorder.ts:
                // 105-106); its count is the presence check's.
                let (pa, pb) = (la[i].1.get(p), lb[k].1.get(p));
                let ok_a = va.contains_key(&i) || pa.is_none();
                let ok_b = vb.contains_key(&k) || pb.is_none();
                if pa != pb && ok_a && ok_b && (pa.is_some() || pb.is_some()) {
                    drop_a.insert(i);
                    drop_b.insert(k);
                    n += 1;
                }
            }
            None => {
                if va.contains_key(&i) {
                    drop_a.insert(i);
                }
            }
        }
    }
    for k in 0..lb.len() {
        if !paired_b.contains(&k) && vb.contains_key(&k) {
            drop_b.insert(k);
        }
    }
    let keep = |l: &RecSamples, d: &BTreeSet<usize>| -> RecSamples {
        l.iter()
            .enumerate()
            .filter(|(i, _)| !d.contains(i))
            .map(|(_, x)| x.clone())
            .collect()
    };
    (keep(la, &drop_a), keep(lb, &drop_b), n)
}

/// Samples each side of the envelope that estimates a recorder fire.
const REC_ENVELOPE: usize = 5;

/// The estimated read (fire) instant of every sample (see `straddle_filter`).
fn read_instants(l: &RecSamples) -> Vec<i64> {
    let mut d: Vec<i64> = l
        .windows(2)
        .map(|w| w[1].0 - w[0].0)
        .filter(|d| *d > 0)
        .collect();
    d.sort();
    let period = d.get(d.len() / 2).copied().unwrap_or(1000).max(1);
    (0..l.len())
        .map(|k| {
            let lo = k.saturating_sub(REC_ENVELOPE);
            let hi = (k + REC_ENVELOPE).min(l.len().saturating_sub(1));
            (lo..=hi)
                .map(|j| l[j].0 - (j as i64 - k as i64) * period)
                .min()
                .unwrap_or(l[k].0)
                .min(l[k].0)
        })
        .collect()
}

/// See `fix_edge` in `rec_values_m`: `only` is ONE step of ONE sample of
/// `health_flags` on the holder, and BOTH sides' live telemetry carried that
/// exact value text within ±1 000 ms (one LED heartbeat) of the sample.
fn led_state_both_held(
    p: &str,
    only: &[Step],
    live_hold: &[(i64, String)],
    live_other: &[(i64, String)],
) -> bool {
    if p != "metrics.system.health_flags.value" {
        return false;
    }
    let [(v, 1, ts)] = only else {
        return false;
    };
    let near = |l: &[(i64, String)]| l.iter().any(|(t, x)| (t - ts).abs() <= 1_000 && x == v);
    near(live_hold) && near(live_other)
}

/// Phase 5b WP4: does every recorded sample's presence of `metrics.<group>`
/// follow from the tape at THIS side's read instants? The TS recorder writes
/// a group only when its cache advanced since the last sample
/// (ts:src/recording/telemetry-recorder.ts:96-106); so sample i must carry
/// the group when an advance fell in (read[i−1], read[i]] and must not when
/// none did. Within CARRY_EPS_MS of either read either is accepted (a read is
/// estimated, `read_instants`). The first sample is not judged.
fn carry_consistent(l: &RecSamples, advances: &[i64], group: &str) -> bool {
    let pre = format!("metrics.{group}.");
    let reads = read_instants(l);
    let any_in = |lo: i64, hi: i64| {
        let i = advances.partition_point(|w| *w <= lo);
        advances.get(i).is_some_and(|w| *w <= hi)
    };
    let last = advances.last().copied().unwrap_or(i64::MIN);
    (1..l.len()).all(|i| {
        let carries = l[i].1.keys().any(|k| k.starts_with(&pre));
        let (lo, hi) = (reads[i - 1], reads[i]);
        // The first read past the input's LAST release (the end of the
        // tape's stream: the helper's exit there is its own event) is not
        // judged.
        if hi > last && lo <= last + CARRY_EPS_MS {
            return true;
        }
        if carries {
            any_in(lo - CARRY_EPS_MS, hi + CARRY_EPS_MS)
        } else {
            !any_in(lo + CARRY_EPS_MS, hi - CARRY_EPS_MS)
        }
    })
}

/// `carry_consistent`'s read-instant uncertainty (the oracle's +50 ms).
const CARRY_EPS_MS: i64 = 50;

/// R-P3G-2 row id (T4's media rule `T4/rec-input-between`).
const REC_INPUT_BETWEEN: &str = "T4/rec-input-between";
/// Two recorder samples are one PAIR when their `ts` are this close (half a
/// 1 Hz tick: each agent's recorder runs on its own timer phase).
const REC_PAIR_MS: i64 = 500;

type RecSamples = Vec<(i64, BTreeMap<String, String>)>;
/// One side's live telemetry `(ts, health_flags text)` series.
type LiveSeries<'a> = &'a [(i64, String)];

/// R-P3G-2: pair A/B recorder samples by nearest `ts` (≤ REC_PAIR_MS, each
/// sample in at most one pair, in order) and remove from both sides a pair
/// whose `p` values differ only when the input it reads changed BETWEEN the
/// two reads (`(lo − collision, hi + collision]`, input changes = releases
/// grouped ≤ 30 ms apart, so an NMEA epoch is ONE change and a 16 Hz IMU
/// line is one each):
/// - two or more changes between the reads: the two samples read inputs the
///   other side never read (a high-rate IMU) — not comparable cross-agent;
/// - exactly one change: only a strict old/new split (the earlier reader's
///   value = the later reader's PREVIOUS sample, the later's = the earlier's
///   NEXT sample); anything else is kept and judged (T4/rec-sample).
///
/// A differing pair with no change between its reads is always kept.
/// Returns the filtered sequences and the number of pairs removed.
fn straddle_filter(
    la: &RecSamples,
    lb: &RecSamples,
    p: &str,
    releases: &[i64],
    collision: i64,
) -> (RecSamples, RecSamples, usize) {
    let mut rel = releases.to_vec();
    rel.sort();
    let mut changes: Vec<i64> = Vec::new();
    for w in rel {
        if changes.last().is_none_or(|l| w - l > 30) {
            changes.push(w);
        } else if let Some(l) = changes.last_mut() {
            *l = w;
        }
    }
    let n_between = |lo: i64, hi: i64| {
        let i = changes.partition_point(|w| *w <= lo - collision);
        let j = changes.partition_point(|w| *w <= hi + collision);
        j - i
    };
    let val = |l: &RecSamples, i: Option<usize>| {
        i.and_then(|i| l.get(i)).and_then(|x| x.1.get(p)).cloned()
    };
    // The READ instant of each sample: the recorder reads the groups at its
    // tick start and stamps `ts` only after `await sampleGroups()`
    // (ts:src/recording/telemetry-recorder.ts:96-103 @09da38d), so ts = fire
    // + that tick's sampling latency. The fire is on the side's own
    // setInterval grid: the lower envelope of ts_j − (j−k)·period over
    // ±REC_ENVELOPE samples (period = the side's median spacing).
    let (ra, rb) = (read_instants(la), read_instants(lb));
    let (mut drop_a, mut drop_b) = (BTreeSet::new(), BTreeSet::new());
    let mut j = 0usize;
    for (i, (ta, oa)) in la.iter().enumerate() {
        while j < lb.len() && lb[j].0 < ta - REC_PAIR_MS {
            j += 1;
        }
        let mut best: Option<usize> = None;
        let mut k = j;
        while k < lb.len() && lb[k].0 <= ta + REC_PAIR_MS {
            if best.is_none_or(|b| (lb[k].0 - ta).abs() < (lb[b].0 - ta).abs()) {
                best = Some(k);
            }
            k += 1;
        }
        let Some(k) = best else { continue };
        j = k + 1;
        let (Some(va), Some(vb)) = (oa.get(p), lb[k].1.get(p)) else {
            continue;
        };
        if va == vb {
            continue;
        }
        let (xa, xb) = (ra[i], rb[k]);
        let drop = match n_between(xa.min(xb), xa.max(xb)) {
            0 => false,
            1 => {
                // earlier = the side that read first
                let (le, ie, ve, ll, il, vl) = if xa <= xb {
                    (la, i, va, lb, k, vb)
                } else {
                    (lb, k, vb, la, i, va)
                };
                val(ll, il.checked_sub(1)).as_ref() == Some(ve)
                    && val(le, Some(ie + 1)).as_ref() == Some(vl)
            }
            // Two or more changes between the reads cannot be attributed by
            // timing alone (the IMU uses the tape oracle instead): judged.
            _ => false,
        };
        if drop {
            drop_a.insert(i);
            drop_b.insert(k);
        }
    }
    let keep = |l: &RecSamples, d: &BTreeSet<usize>| -> RecSamples {
        l.iter()
            .enumerate()
            .filter(|(i, _)| !d.contains(i))
            .map(|(_, x)| x.clone())
            .collect()
    };
    let n = drop_a.len();
    (keep(la, &drop_a), keep(lb, &drop_b), n)
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
    let delta = match (a.kind_of_session(sid), b.kind_of_session(sid)) {
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

/// `T12/cut-fragment` (review-wp6 M3, F1): the LAST fragment of a session
/// cut by the run's stop has no GOP boundary at its end — its `durationMs`
/// is `stop − session start`, so the two sides differ by the session-start
/// phase (boot skew, or each side's own shed tick for a transition-started
/// session, `manager.ts:612-615`), which T12 never bounds. Compared instead by
/// its END instant (`startTs + durationMs`, ±`endMs`: the shared stop); its
/// start is already judged (startTs vs PLAYING, T4 transitions). Conditions =
/// D30's `d30_session_end` (a) + (c) + (d) WITHOUT (b): (a) the last seq of the
/// session on BOTH sides, the capture ended at the agent's stop (int/eos) and
/// never respawned; (c) end instants within `endMs`; (d) each side's session
/// start is anchored to ITS OWN shed transition: decision → spawn is the same
/// code path on both sides, and what the rig sees of the decision is the
/// change in the telemetry, published on that side's next 1 s publish tick
/// (worker.ts:1207) — so the two sides' spawn-minus-change lags differ by at
/// most one publish period (`tickMs`) — or neither side had a change (a
/// boot-started session).
///
/// Phase 5b WP5 (R-P5bG-1), SCOPE only (no number changed): the cut may also
/// be a CLOUD-CONFIG video-tuple change (preset / record / segmentSeconds /
/// encoder). TS @09da38d: the config message handler applies the document and
/// calls `recording.reapply()` synchronously (worker.ts:960-980) → `apply()` →
/// `applyVideo()`, whose key holds the preset (manager.ts:570-580); a changed
/// key chains `prev.stopAndWait()` at once (manager.ts:620-626) and only then
/// starts the new session. So the cut instant is the player's release of that
/// config record — ONE instant for both agents, like the stop's SIGTERM — not
/// a per-agent timer (the 5 s settings poll, manager.ts:327, only catches
/// local edits). Instead of "never respawned": on BOTH sides the capture is
/// respawned for a NEW session at/after a released `…/config` `mqtt_in`
/// record `r` (the latest before both ends), both end instants lie in
/// [r, r + `endMs`] (so they also agree within `endMs`), and neither side
/// published a powerShed / liveBlock change within ±`tickMs` of `r` (a shed
/// cut is D30's, never this). (c) and (d) apply unchanged.
#[allow(clippy::too_many_arguments)]
fn t12_cut_fragment(
    a: &MediaCapture,
    b: &MediaCapture,
    ca: &BTreeMap<(String, u64), Value>,
    cb: &BTreeMap<(String, u64), Value>,
    fa: &BTreeMap<String, BTreeMap<u64, u64>>,
    fb: &BTreeMap<String, BTreeMap<u64, u64>>,
    key: &(String, u64),
    end_ms: f64,
    tick_ms: i64,
    config_releases: &[i64],
) -> bool {
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
    // (spawn w, the next capture spawn of that camera: (w, sid)) — the child
    // ended by the agent (int/eos), never by a kill.
    #[allow(clippy::type_complexity)]
    let spawn_at = |m: &MediaCapture| -> Option<(i64, Option<(i64, Option<String>)>)> {
        let s = m
            .spawns
            .iter()
            .find(|s| s.role == "capture" && s.sid.as_deref() == Some(sid.as_str()))?;
        let at_stop = matches!(s.end.as_deref(), Some("int") | Some("eos"));
        let next = m
            .spawns
            .iter()
            .filter(|o| o.role == "capture" && o.key == s.key && o.w > s.w)
            .min_by_key(|o| o.w)
            .map(|o| (o.w, o.sid.clone()));
        at_stop.then_some((s.w, next))
    };
    let (Some((wa, na)), Some((wb, nb))) = (spawn_at(a), spawn_at(b)) else {
        return false;
    };
    let f64_at = |c: &BTreeMap<(String, u64), Value>, q: u64, k: &str| {
        c.get(&(sid.clone(), q)).and_then(|v| v[k].as_f64())
    };
    let end = |c: &BTreeMap<(String, u64), Value>| {
        Some(f64_at(c, seq, "startTs")? + f64_at(c, seq, "durationMs")?)
    };
    let (Some(ea), Some(eb)) = (end(ca), end(cb)) else {
        return false;
    };
    if (ea - eb).abs() > end_ms {
        return false;
    }
    match (&na, &nb) {
        // The stop arm: never respawned on either side.
        (None, None) => {}
        // The config arm (R-P5bG-1): a new session on both sides after ONE
        // released config record that both end instants follow within endMs.
        (Some((ra, sa)), Some((rb, sb))) => {
            let new_session = |n: &Option<String>| n.as_deref().is_some_and(|x| x != sid.as_str());
            let Some(&r) = config_releases
                .iter()
                .filter(|r| (**r as f64) <= ea.min(eb))
                .max()
            else {
                return false;
            };
            let within = |e: f64| e >= r as f64 && e - r as f64 <= end_ms;
            let shed_near =
                |m: &MediaCapture| m.shed_changes.iter().any(|c| (*c - r).abs() <= tick_ms);
            if !(new_session(sa) && new_session(sb))
                || *ra < r
                || *rb < r
                || !within(ea)
                || !within(eb)
                || shed_near(a)
                || shed_near(b)
            {
                return false;
            }
        }
        // Respawned on one side only: not the same cut.
        _ => return false,
    }
    let lag = |m: &MediaCapture, w: i64| {
        m.shed_changes
            .iter()
            .rfind(|c| **c <= w + tick_ms)
            .map(|c| w - c)
    };
    match (lag(a, wa), lag(b, wb)) {
        (None, None) => true,
        (Some(x), Some(y)) => (x - y).abs() <= tick_ms,
        _ => false,
    }
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
    // The tape's input facts (also the R-P3G-2 oracle's, before the values).
    let facts_early = crate::diff::InputFacts::for_play(play);
    // R-WP5-4: whether disk_usage was isolated (the statvfs shim) in this run.
    rep.inputs.insert(
        "disk source (disk_usage)".into(),
        if play.disk_source.is_empty() {
            "unrecorded (pre-WP5 run)".to_string()
        } else {
            play.disk_source.clone()
        },
    );
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
    // R-P5bG-1: the player's release instants of the tape's cloud-config
    // records (`mqtt_in` on `…/config`) — the shared cut of a config-driven
    // video change (T12/cut-fragment's config arm).
    let config_releases: Vec<i64> = {
        // review-wp5 R-WP5-8: joined by (kind, tape t, occurrence) — the
        // player releases records in tape order, so the n-th `mqtt_in`
        // release at tape t is the n-th `mqtt_in` record at t; a non-config
        // message sharing a t with a config one is never counted.
        let mut is_cfg: BTreeMap<u64, Vec<bool>> = BTreeMap::new();
        for r in tape_recs(Path::new(&play.tape))
            .iter()
            .filter(|r| r["k"] == "mqtt_in")
        {
            if let Some(t) = r["t"].as_u64() {
                is_cfg
                    .entry(t)
                    .or_default()
                    .push(r["topic"].as_str().is_some_and(|x| x.ends_with("/config")));
            }
        }
        let mut seen: BTreeMap<u64, usize> = BTreeMap::new();
        play.releases
            .iter()
            .filter(|r| r.k == "mqtt_in")
            .filter_map(|r| {
                let n = seen.entry(r.t).or_default();
                let cfg = is_cfg.get(&r.t).and_then(|v| v.get(*n)).copied() == Some(true);
                *n += 1;
                cfg.then_some(r.w)
            })
            .collect()
    };
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
        let cut_ok = has_row(table, "T12/cut-fragment")
            && !sid_failed.contains(&key.0)
            && t12_cut_fragment(
                a,
                b,
                &ca,
                &cb,
                &fa,
                &fb,
                key,
                row_tol(table, "T12/cut-fragment", "endMs", 0.0),
                row_tol(table, "T12/cut-fragment", "tickMs", 0.0) as i64,
                &config_releases,
            );
        // review-wp6 F5: an INTERIOR fragment (its successor exists on both
        // sides) has no duration of its own — `durationMs` = mtime − birthtime
        // (video-segments.ts:178-182) and its mtime IS the rotation to the next
        // file (end = next startTs ±3 ms per side, measured), so it is the
        // difference of two boundaries, each a per-process fs instant. Its END
        // is judged like its start: relative to each side's own PLAYING within
        // T12's startTs bound.
        let next = |c: &BTreeMap<(String, u64), Value>| c.get(&(key.0.clone(), key.1 + 1)).cloned();
        let interior = has_row(table, "T12/interior-boundaries")
            && [(x, next(&ca)), (y, next(&cb))]
                .iter()
                .all(|(v, n)| n.as_ref().is_some_and(|n| seam_ok(v, n)))
            && ends_agree(x, y, (pa, pb), start_ms);
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
                    } else if interior {
                        Some("T12/interior-boundaries".to_string())
                    } else if cut_ok {
                        Some("T12/cut-fragment".to_string())
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
        // review-wp6 M2: the system-less samples that carry the D30
        // starvation SIGNATURE (`d30_starved_drops`), per side.
        let mut starved = (0usize, 0usize);
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
                    // R-P3G-2: likewise a high-rate input's leaf where every
                    // sample of the segment carrying it, on both sides, is a
                    // proven read of the tape (the oracle checks the TEXT, so
                    // the number format itself is still pinned).
                    let oracle_ok = num_only(sha.get(p))
                        && num_only(shb.get(p))
                        && has_row(table, REC_INPUT_BETWEEN)
                        && segment_oracle_ok(&pa, p, &facts_early, a.imu_mount.as_ref())
                        && segment_oracle_ok(&pb, p, &facts_early, b.imu_mount.as_ref());
                    let tol = match table.numeric("telemetry", p) {
                        Some((id, _)) if num_only(sha.get(p)) && num_only(shb.get(p)) => id,
                        _ if oracle_ok => REC_INPUT_BETWEEN.to_string(),
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
                starved.0 += d30_starved_drops(&pa);
                starved.1 += d30_starved_drops(&pb);
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
        // review-wp6 M2 (F3, ruled inherent): a RUST system-less sample with
        // the starvation signature is the TS mechanism (a delayed sample()
        // let the next one land < 1000 ms after the gate stamp) — cited
        // T7/D30 by itself, then left out of the Rust count. Never a count
        // rule: an unsigned Rust drop still counts (and fails).
        let rs_adj =
            |kind: &str, lack: usize, st: usize| if kind == "rust" { lack - st } else { lack };
        // P3a (R2): judged by the kind of the incarnation that RECORDED the
        // session (no handoff: the side's kind).
        let (ka, kb) = (a.kind_of_session(sid), b.kind_of_session(sid));
        for (side, kind, st) in [("A", ka, starved.0), ("B", kb, starved.1)] {
            if kind == "rust" && st > 0 {
                let id = table
                    .matcher_rows("d30_system_presence", "rec-telemetry")
                    .first()
                    .cloned()
                    .unwrap_or_else(|| UNTOLERATED.to_string());
                rep.push(mm(
                    "rec-telemetry",
                    sid,
                    "(starved system-less samples: isolated, mono gap 900-999 ms)",
                    if side == "A" {
                        st.to_string()
                    } else {
                        String::new()
                    },
                    if side == "B" {
                        st.to_string()
                    } else {
                        String::new()
                    },
                    &id,
                ));
            }
        }
        let (la, lb) = (rs_adj(ka, lack.0, starved.0), rs_adj(kb, lack.1, starved.1));
        if let Some(tol) = system_presence_tol(table, ka, kb, la, lb) {
            rep.push(mm(
                "rec-telemetry",
                sid,
                "(samples without system)",
                la.to_string(),
                lb.to_string(),
                &tol,
            ));
        }
    }

    // ---- recorded telemetry VALUES + per-group presence (G1)
    let facts = facts_early;
    let (va, vb) = (segments_by_seq(a), segments_by_seq(b));
    stat(rep, "rec-values");
    stat(rep, "rec-presence");
    // Each side's NMEA epochs at its OWN read instants (its agent tape); a
    // side without one (JS) gets the player's feed at the release instants.
    // review-wp6 S-F4: a Rust side's read instant is self-reported, so each
    // one is bounded against the player's release of the SAME line
    // within `collisionMs` — the race window itself: an anchor that may sit
    // further from its release than the window could move a race by more
    // than the rule admits (measured max 14 ms under 15 busy loops); an
    // unbounded read cannot anchor a race.
    // The lines AS WRITTEN: the player shifts each NMEA time-of-day/date
    // to the run's wall clock before the write (play.rs, rewrite.rs).
    let feed: Vec<(i64, String)> =
        nmea_lines(&tape_recs(Path::new(&play.tape)), Some(&play.releases))
            .into_iter()
            .map(|(w, l)| (w, crate::rewrite::shift_nmea_line(&l, play.shift_s)))
            .collect();
    let read_lag = row_tol(table, "T4/rec-sample", "collisionMs", 0.0) as i64;
    let mut timeline = |label: &str, m: &MediaCapture| {
        if m.nmea_reads.is_empty() {
            let all: Vec<(i64, String, bool)> =
                feed.iter().map(|(w, l)| (*w, l.clone(), true)).collect();
            return gps_timeline(&all);
        }
        let (bounded, lags) = bound_reads(&m.nmea_reads, &feed, read_lag);
        let out = bounded.iter().filter(|r| !r.2).count();
        let mut l = lags.clone();
        l.sort();
        rep.inputs.insert(
            format!("T4 nmea read − release {label} (S-F4)"),
            format!(
                "{} reads, {} matched, median {:?} / max {:?} ms, {out} outside [0, {read_lag}] ms",
                bounded.len(),
                lags.len(),
                l.get(l.len() / 2),
                l.last()
            ),
        );
        gps_timeline(&bounded)
    };
    let ep = (timeline("A", a), timeline("B", b));
    let pmic = pmic_series_with_boot(play);
    for sid in va.keys().filter(|k| vb.contains_key(*k)) {
        rec_values_m(
            sid,
            (&va[sid], &vb[sid]),
            &ep,
            play,
            &facts,
            (a.kind_of_session(sid), b.kind_of_session(sid)),
            &pmic,
            table,
            rep,
            (a.imu_mount.as_ref(), b.imu_mount.as_ref()),
            (&a.live_flags, &b.live_flags),
            (&a.live_led, &b.live_led),
        );
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
            // Session labels compare by their start ordinal (`…#10>` is after
            // `…#7>`), never as strings (Phase 5b WP4: the window tape's
            // cam1 #10-after-#7 "violation" on all four sides was this).
            let key = |x: &(String, u64)| (session_ordinal(&x.0), x.1);
            if let Some(w) = v.windows(2).find(|w| key(&w[1]) < key(&w[0])) {
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
        format!(
            "A {} {:?}, B {} {:?}",
            a.shm_orphans, a.shm_orphan_names, b.shm_orphans, b.shm_orphan_names
        ),
    );
    shm_orphans_verdict(a, b, play.end_wall, table, rep);
    d30_js_reference(a, b, &pmic_series(play), table, rep);
    d30_js_late_shed(a, b, play.start_wall, &power_changes(play), table, rep);
}

/// The end-of-run `/dev/shm` orphans (Phase 5b WP5, item 9 + D41, split by
/// mechanism per review-wp5 R-WP5-2):
/// - a RUST side's orphan is never tolerated: one row per Rust side that
///   strands anything, whatever the other side holds (D41: the Rust reaper
///   matches shmsink's padded names and the agent stops its children cleanly);
/// - JS-vs-Rust with Rust 0: `T7/D41` only when EVERY JS orphan is one of
///   - (a) REAPER MISS: its owner capture died MID-RUN (gone before the
///     player's SIGTERM `end_wall`: the JS agent alive, its ladder or the
///     tape SIGKILLed the child, `onExit` → `reapShmSegments` ran) AND the
///     name is SPACE-padded (owner pid < 10000) — the names the TS prefix
///     `shmpipe.${pid}.` can never match (ts:src/recording/video-capture.ts
///     :873-891, :876 @09da38d; gst shmpipe.c:307 `"/shmpipe.%5d.%5d"`); a 5+
///     digit name IS matched by the TS reaper, so it is UNTOLERATED;
///   - (b) END OF RUN: its owner capture was still running when the JS agent
///     was told to stop (gone at/after `end_wall`) AND that JS agent hung
///     past 30 s and was SIGKILLed (`agent_hung`, bug 3: its wedged event
///     loop never runs `onExit`'s reaper at all, so the pid width does not
///     matter) — any pid width;
/// - every other count difference (JS-vs-JS, Rust-vs-Rust) is UNTOLERATED.
fn shm_orphans_verdict(
    a: &MediaCapture,
    b: &MediaCapture,
    end_wall: i64,
    table: &Table,
    rep: &mut Report,
) {
    let padded = |n: &str| {
        n.strip_prefix("shmpipe.")
            .is_some_and(|r| r.starts_with(' '))
    };
    // Per JS orphan of a side: (a) reaper miss / (b) end of run / neither.
    let arms_of = |m: &MediaCapture| -> Vec<Option<&'static str>> {
        m.shm_orphan_names
            .iter()
            .zip(m.shm_orphan_gone.iter())
            .map(|(n, g)| match *g {
                Some(g) if g < end_wall && padded(n) => Some("reaper-miss"),
                Some(g) if g >= end_wall && m.agent_hung => Some("end-of-run"),
                _ => None,
            })
            .collect()
    };
    for (side, m) in [("A", a), ("B", b)] {
        if m.kind == "rust" && !m.capture_alive_at_stop.is_empty() {
            let pids = format!("{:?}", m.capture_alive_at_stop);
            rep.push(mm(
                "shm",
                "end",
                "(capture child of a Rust side alive at stop)",
                if side == "A" {
                    pids.clone()
                } else {
                    String::new()
                },
                if side == "B" { pids } else { String::new() },
                UNTOLERATED,
            ));
        }
        if m.kind == "rust" && m.shm_orphans > 0 {
            let names = format!("{:?}", m.shm_orphan_names);
            rep.push(mm(
                "shm",
                "end",
                "(orphan shmpipe segments of a Rust side)",
                if side == "A" {
                    names.clone()
                } else {
                    String::new()
                },
                if side == "B" { names } else { String::new() },
                UNTOLERATED,
            ));
        }
    }
    if a.shm_orphans == b.shm_orphans {
        return;
    }
    let tol = match (a.kind.as_str(), b.kind.as_str()) {
        ("js", "rust") | ("rust", "js") => {
            let (js, rs) = if a.kind == "js" { (a, b) } else { (b, a) };
            let d41 = table.matcher_rows("d41_js_shm_orphans", "shm");
            let arms = arms_of(js);
            rep.inputs.insert(
                "D41 arms (JS orphans)".into(),
                format!(
                    "{:?}",
                    js.shm_orphan_names.iter().zip(&arms).collect::<Vec<_>>()
                ),
            );
            match d41.first() {
                Some(id)
                    if rs.shm_orphans == 0
                        && js.shm_orphans == arms.len()
                        && arms.iter().all(Option::is_some) =>
                {
                    id.clone()
                }
                _ => UNTOLERATED.to_string(),
            }
        }
        // T18/shm-js-end-of-run (orchestrator ruling 2026-11-06), JS-vs-JS
        // calibration ONLY: the two JS references may differ in how many
        // END-OF-RUN orphans their hung shutdowns leave (bug 3 / P7: whether
        // a SIGINTed capture finished its EOS before the 30 s SIGKILL of the
        // hung agent). Tolerated only when every orphan on BOTH sides is
        // explained (arm a or b) and the arm-(a) (reaper-miss) counts are
        // EQUAL, so the difference is made of arm-(b) orphans only.
        ("js", "js") => {
            let (xa, xb) = (arms_of(a), arms_of(b));
            let n = |v: &[Option<&str>], k: &str| v.iter().filter(|x| **x == Some(k)).count();
            rep.inputs.insert(
                "orphan arms (JS-vs-JS)".into(),
                format!("A {xa:?}, B {xb:?}"),
            );
            let id = "T18/shm-js-end-of-run";
            if has_row(table, id)
                && xa.iter().chain(xb.iter()).all(Option::is_some)
                && n(&xa, "reaper-miss") == n(&xb, "reaper-miss")
            {
                id.to_string()
            } else {
                UNTOLERATED.to_string()
            }
        }
        _ => UNTOLERATED.to_string(),
    };
    rep.push(mm(
        "shm",
        "end",
        "(orphan shmpipe segments)",
        format!("{} {:?}", a.shm_orphans, a.shm_orphan_names),
        format!("{} {:?}", b.shm_orphans, b.shm_orphan_names),
        &tol,
    ));
}

/// Release instants (wall ms) where a power input changed: the pmic EXT5V
/// rail (`%.2f`) or the `vcgencmd get_throttled` answer.
fn power_changes(play: &PlayInfo) -> Vec<i64> {
    use xorgate_hal::tape::TapeEvent;
    let Ok(t) = xorgate_hal::tape::Tape::read(Path::new(&play.tape)) else {
        return Vec::new();
    };
    let walls = crate::diff::release_walls(&t.records, &play.releases);
    let mut last: BTreeMap<String, String> = BTreeMap::new();
    let mut out = Vec::new();
    for (r, w) in t.records.iter().zip(walls) {
        let (TapeEvent::Exec { argv, stdout, .. }, Some(w)) = (&r.ev, w) else {
            continue;
        };
        let v = match argv.get(1).map(String::as_str) {
            Some("pmic_read_adc") => xorgate_core::system::parse_pmic_adc(stdout)
                .and_then(|adc| xorgate_core::system::rail_5v_from(&adc))
                .map(|v| format!("{v:.2}")),
            Some("get_throttled") => Some(stdout.trim().to_string()),
            _ => None,
        };
        let (Some(v), Some(k)) = (v, argv.get(1)) else {
            continue;
        };
        if last.get(k).is_some_and(|l| *l != v) {
            out.push(w);
        }
        last.insert(k.clone(), v);
    }
    out.sort();
    out
}

/// Each recorded sample's power reading: `(ts, "<rail_5v>|<throttled>")`.
fn rec_power(m: &MediaCapture) -> Vec<(i64, String)> {
    let mut v: Vec<(i64, String)> = segments_by_seq(m)
        .values()
        .flat_map(|segs| segs.values())
        .flat_map(|t| t.lines())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|x| x["metrics"]["system"].is_object())
        .map(|x| {
            let sys = &x["metrics"]["system"];
            (
                x["ts"].as_i64().unwrap_or(0),
                format!("{}|{}", sys["rail_5v"]["value"], sys["throttled"]["value"]),
            )
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

/// A shed event text with its rail value masked (the pairing key).
fn shed_key(msg: &str) -> String {
    match shed_text(msg) {
        Some((_, masked)) => masked,
        None => msg.to_string(),
    }
}

/// The `N` of `for Ns` / `clean for Ns` in a shed event (0 = immediate).
fn shed_secs(msg: &str) -> i64 {
    msg.split(" for ")
        .nth(1)
        .and_then(|r| r.split('s').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// EVIDENCE (row `T8/d30-js-late-shed` (3)) that the JS shed tick deciding
/// `msg` at `ts` was one tick late because it REUSED its recorder's previous
/// computation: the decisive tick D (a restore: the last dirty read, `ts −
/// N s`; an escalation: the read before the first low one, `ts − (N+1) s`)
/// lies after a power-input release (≥ 50 ms before D) that its recorder's
/// previous sample (< 1 s before D) did not yet carry and its next sample
/// carrying `system` (< 2.1 s after D) did — so a fresh read at D would have decided on time.
fn js_stale_evidence(rec: &[(i64, String)], changes: &[i64], ts: i64, msg: &str) -> bool {
    let n = shed_secs(msg);
    let d = if msg.contains("restore") {
        ts - n * 1000
    } else {
        ts - (n + 1) * 1000
    };
    let prev = rec.iter().rfind(|(t, _)| *t < d && d - *t < 1000);
    // The next sample CARRYING system (a D30 recorder tick may write none:
    // the same stale cache, telemetry-recorder.ts:102) — ≤ 2 ticks on.
    let next = rec.iter().find(|(t, _)| *t > d && *t - d < 2_100);
    let (Some(p), Some(q)) = (prev, next) else {
        return false;
    };
    p.1 != q.1 && changes.iter().any(|c| *c > p.0 && *c <= d - 50)
}

/// Row `T8/d30-js-late-shed`: D30 — at a power crossing a JS shed tick may
/// REUSE its recorder's ≤ 1 s old reading (TS system.ts:104-113), so ONE
/// shed/restore transition lands one shed period LATER on that JS side;
/// Rust never reuses.
/// - Pairs (first statement): JS-vs-Rust with the JS side LATER, or JS-vs-JS
///   with EITHER side later; anything else (rust-vs-rust, a Rust-later
///   transition) never applies.
/// - Per transition (shed events paired by masked text + occurrence): the
///   later side is a JS side, Δ = later − earlier ∈ [850, 1150] ms AND
///   `js_stale_evidence` on THAT side; or it is the next transition of the
///   same chain shifted the same way by the same Δ ±150. One shift per
///   transition.
/// - Downstream rows re-judged (same direction, same Δ ±150): child spawn
///   instants within 6 s after the later side's transition; sidecar
///   `durationMs` of the session that capture started (later side shorter by
///   Δ) or of a fragment ending within ±1.5 s of it (later side longer by
///   Δ). Nothing else.
fn d30_js_late_shed(
    a: &MediaCapture,
    b: &MediaCapture,
    start_wall: i64,
    changes: &[i64],
    table: &Table,
    rep: &mut Report,
) {
    // Which columns may be the LATER (stale) side: only a JS one, and
    // against Rust only the JS side.
    let may_lag: [bool; 2] = match (a.kind.as_str(), b.kind.as_str()) {
        ("js", "rust") => [true, false],
        ("rust", "js") => [false, true],
        ("js", "js") => [true, true],
        _ => return,
    };
    let Some((rid, _)) = table.decision_row("d30-js-late-shed") else {
        return;
    };
    let id = format!("{rid}/d30-js-late-shed");
    const BAND: (i64, i64) = (850, 1150);
    const SAME: i64 = 150;
    let sides = [a, b];
    let keyed = |m: &MediaCapture| {
        let mut ev = m.shed_events.clone();
        ev.sort();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        ev.into_iter()
            .map(|(ts, msg)| {
                let k = shed_key(&msg);
                let n = seen.entry(k.clone()).or_default();
                *n += 1;
                ((k, *n), (ts, msg))
            })
            .collect::<BTreeMap<_, _>>()
    };
    let (ka, kb) = (keyed(a), keyed(b));
    let recs = [rec_power(a), rec_power(b)];
    // (A instant, B instant, A's msg, B's msg)
    let mut pairs: Vec<(i64, i64, String, String)> = ka
        .iter()
        .filter_map(|(k, (ta, ma))| kb.get(k).map(|(tb, mb)| (*ta, *tb, ma.clone(), mb.clone())))
        .collect();
    pairs.sort();
    // Tolerated transitions: (later side index, later side's instant, Δ > 0).
    let mut shifted: Vec<(usize, i64, i64)> = Vec::new();
    let mut prev: Option<(usize, i64)> = None;
    for (ta, tb, ma, mb) in &pairs {
        let pick = [(0usize, ta - tb, *ta, ma), (1, tb - ta, *tb, mb)]
            .into_iter()
            .find(|(side, d, t, msg)| {
                may_lag[*side]
                    && (BAND.0..=BAND.1).contains(d)
                    && (js_stale_evidence(&recs[*side], changes, *t, msg)
                        || prev.is_some_and(|(ps, pd)| ps == *side && (d - pd).abs() <= SAME))
            });
        match pick {
            Some((side, d, t, _)) => {
                shifted.push((side, t, d));
                prev = Some((side, d));
            }
            None => prev = None,
        }
    }
    if shifted.is_empty() {
        return;
    }
    let owner = |side: usize, w: i64| {
        shifted
            .iter()
            .rfind(|(s, t, _)| *s == side && *t <= w && w - *t <= 6_000)
            .copied()
    };
    let mut ok = BTreeSet::new();
    for (i, m) in rep.mismatches.iter().enumerate() {
        if m.tol != UNTOLERATED {
            continue;
        }
        let (Ok(x), Ok(y)) = (m.a.parse::<i64>(), m.b.parse::<i64>()) else {
            continue;
        };
        for side in [0usize, 1] {
            if !may_lag[side] {
                continue;
            }
            let (vl, ve) = if side == 0 { (x, y) } else { (y, x) };
            let hit = if m.stream == "children" && m.path == "(spawn ms after start)" {
                owner(side, start_wall + vl).is_some_and(|(_, _, d)| (vl - ve - d).abs() <= SAME)
            } else if m.stream == "sidecar" && (m.path == "startTs" || m.path == "startMono") {
                // The fragment that STARTS at the tolerated transition (the
                // one after the fragment it cut): its start shifts by Δ.
                let Some((sid, seq)) = m.at.rsplit_once('/') else {
                    continue;
                };
                let Ok(seq) = seq.parse::<u64>() else {
                    continue;
                };
                let st = sidecars(sides[side])
                    .get(&(sid.to_string(), seq))
                    .and_then(|v| v["startTs"].as_f64())
                    .map(|v| v as i64);
                st.and_then(|st| {
                    shifted
                        .iter()
                        .find(|(s, t, _)| *s == side && (st - t).abs() <= 1_500)
                        .copied()
                })
                .is_some_and(|(_, _, d)| (vl - ve - d).abs() <= SAME)
            } else if m.stream == "sidecar" && m.path == "durationMs" {
                let Some((sid, seq)) = m.at.rsplit_once('/') else {
                    continue;
                };
                let Ok(seq) = seq.parse::<u64>() else {
                    continue;
                };
                let lm = sides[side];
                let started = lm
                    .spawns
                    .iter()
                    .find(|s| s.role == "capture" && s.sid.as_deref() == Some(sid))
                    .and_then(|s| owner(side, s.w));
                let end = sidecars(lm).get(&(sid.to_string(), seq)).and_then(|v| {
                    Some(v["startTs"].as_f64()? as i64 + v["durationMs"].as_f64()? as i64)
                });
                let ended = end.and_then(|e| {
                    shifted
                        .iter()
                        .find(|(s, t, _)| *s == side && (e - t).abs() <= 1_500)
                        .copied()
                });
                // The fragment the late shed CUT (Phase 5b, orchestrator
                // 2026-10-23, rec-shed-uv cam1#0/00003 860 vs 1): the LAST seq
                // of its session on BOTH sides, and its END instant (startTs +
                // durationMs) later on the JS side by the transition's Δ ±150.
                // Its duration need not differ by Δ: the fragment's start is
                // the capture's own keyframe cadence, which may differ between
                // the sides by up to T12, and a shed landing just after a
                // keyframe leaves a 0–2 ms last fragment on the TS too (every
                // rec-shed-uv JS-vs-JS run, cam1 session 1).
                let other = sides[1 - side];
                let last_of = |m: &MediaCapture| {
                    sidecars(m)
                        .keys()
                        .filter(|(s, _)| s == sid)
                        .map(|(_, q)| *q)
                        .max()
                };
                let end_other = sidecars(other).get(&(sid.to_string(), seq)).and_then(|v| {
                    Some(v["startTs"].as_f64()? as i64 + v["durationMs"].as_f64()? as i64)
                });
                let cut_last = last_of(lm) == Some(seq) && last_of(other) == Some(seq);
                let end_shift = end.zip(end_other).map(|(l, e)| l - e);
                started.is_some_and(|(_, _, d)| (vl - ve + d).abs() <= SAME)
                    || ended.is_some_and(|(_, _, d)| {
                        (vl - ve - d).abs() <= SAME
                            || (cut_last && end_shift.is_some_and(|s| (s - d).abs() <= SAME))
                    })
            } else {
                false
            };
            if hit {
                ok.insert(i);
            }
        }
    }
    let Report {
        mismatches, stats, ..
    } = rep;
    for i in ok {
        let x = &mut mismatches[i];
        x.tol = id.clone();
        let st = stats.entry(x.stream.clone()).or_default();
        st.untolerated = st.untolerated.saturating_sub(1);
        st.tolerated += 1;
    }
}

/// A normalised session label's ordering key: `<sid:video:cam1#10>` →
/// (`<sid:video:cam1`, 10); a label without `#N>` keeps its text, ordinal 0.
fn session_ordinal(sid: &str) -> (String, u64) {
    sid.strip_suffix('>')
        .and_then(|s| s.rsplit_once('#'))
        .and_then(|(head, n)| n.parse::<u64>().ok().map(|n| (head.to_string(), n)))
        .unwrap_or_else(|| (sid.to_string(), 0))
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
    pmic_series_from(play, false)
}

/// [`pmic_series`] plus the PREFILLED boot value(s) (no release: present
/// from the play's start, `diff::release_walls`) at `play.start_wall` — the
/// rail the agents read before the first released change
/// (`d30_rec_value_gap` needs the predecessor of the first change).
fn pmic_series_with_boot(play: &PlayInfo) -> Vec<(i64, String)> {
    pmic_series_from(play, true)
}

fn pmic_series_from(play: &PlayInfo, boot: bool) -> Vec<(i64, String)> {
    use xorgate_hal::tape::TapeEvent;
    let Ok(t) = xorgate_hal::tape::Tape::read(Path::new(&play.tape)) else {
        return Vec::new();
    };
    let walls = crate::diff::release_walls(&t.records, &play.releases);
    let mut out: Vec<(i64, String)> = Vec::new();
    let mut released = false;
    for (r, w) in t.records.iter().zip(walls) {
        let TapeEvent::Exec { argv, stdout, .. } = &r.ev else {
            continue;
        };
        if argv.get(1).map(String::as_str) != Some("pmic_read_adc") {
            continue;
        }
        // Unreleased BEFORE the first release = prefilled (boot); after it
        // = never released (past the run's end): skipped.
        let w = match w {
            Some(w) => {
                released = true;
                w
            }
            None if boot && !released => play.start_wall,
            None => continue,
        };
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

    /// P3a D59, the two shapes seen (restart at 0 here; ms since it):
    /// L9 2026-11-07 — Rust records hf 16 for ONE sample (2 070) just before
    /// its LED went healthy, the JS sample 200 ms later already 0; L1.r2 —
    /// the JS sample there carried no `system` (D30). Both live series held
    /// 16 within 1 s. NEGATIVES: a blip > 5 s after the boot, a session that
    /// did not start at a boot, one live series without the value, a
    /// two-sample state, another leaf, a remaining difference: all None.
    #[test]
    fn d59_only_boot_transient_one_sample_led_states_both_lives_held() {
        let hf = "metrics.system.health_flags.value";
        let smp = |ts: i64, v: Option<&str>| -> LeafSample {
            let mut o = BTreeMap::from([("metrics.gps.fix.value".to_string(), "3".to_string())]);
            if let Some(v) = v {
                o.insert(hf.to_string(), v.to_string());
            }
            (ts, o)
        };
        let play = PlayInfo {
            start_wall: -200_000,
            end_wall: 400_000,
            handoff: Some(crate::takeover::HandoffInfo {
                stop_w: -100_000,
                restart_w: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let a = vec![
            smp(1_067, Some("0")),
            smp(2_070, Some("16")),
            smp(3_067, Some("0")),
            smp(4_070, Some("0")),
        ];
        let b = vec![
            smp(1_267, Some("0")),
            smp(2_272, Some("0")),
            smp(3_262, Some("0")),
            smp(4_273, Some("0")),
        ];
        let live_a = vec![(1_158, "16".to_string()), (2_163, "0".to_string())];
        let live_b = vec![(1_380, "16".to_string()), (2_383, "0".to_string())];
        let run = |p: &str,
                   a: &[LeafSample],
                   b: &[LeafSample],
                   la: &[(i64, String)],
                   lb: &[(i64, String)],
                   play: &PlayInfo| {
            d59_boot_led_blips(p, play, ("rust", "js"), (a, b), (la, lb))
        };
        let run_k = |k: (&str, &str),
                     a: &[LeafSample],
                     b: &[LeafSample],
                     la: &[(i64, String)],
                     lb: &[(i64, String)]| {
            d59_boot_led_blips(hf, &play, k, (a, b), (la, lb))
        };
        assert_eq!(
            run(hf, &a, &b, &live_a, &live_b, &play).unwrap(),
            vec![(true, ("16".to_string(), 1, 2_070))]
        );
        // L1.r2: the other side's sample there had no system (no hf leaf).
        let b_nosys = vec![
            smp(1_267, Some("0")),
            smp(2_272, None),
            smp(3_262, Some("0")),
        ];
        assert!(run(hf, &a, &b_nosys, &live_a, &live_b, &play).is_some());
        // Either column order (the Rust holder as B, the JS lacker as A).
        assert!(run_k(("js", "rust"), &b, &a, &live_b, &live_a).is_some());
        // JS-vs-JS: the lacking side is JS, the TS race (allowed).
        assert!(run_k(("js", "js"), &a, &b, &live_a, &live_b).is_some());
        // NEGATIVE (review-P3a F2): the side LACKING the blip is Rust — JS
        // holding it, or Rust-vs-Rust — stays judged.
        assert!(run_k(("js", "rust"), &a, &b, &live_a, &live_b).is_none());
        assert!(run_k(("rust", "rust"), &a, &b, &live_a, &live_b).is_none());
        assert!(run_k(("rust", "js"), &b, &a, &live_b, &live_a).is_none());
        // NEGATIVE: the same blip 6 s after the boot.
        let late_a = [
            smp(1_067, Some("0")),
            smp(5_067, Some("0")),
            smp(6_070, Some("16")),
            smp(7_067, Some("0")),
        ];
        let late_b = [
            smp(1_267, Some("0")),
            smp(5_262, Some("0")),
            smp(6_272, Some("0")),
            smp(7_273, Some("0")),
        ];
        let late_live = vec![(6_158, "16".to_string())];
        assert!(run(hf, &late_a, &late_b, &late_live, &late_live, &play).is_none());
        // NEGATIVE: a session that did not start at a boot (60 s after it).
        let mid = PlayInfo {
            handoff: Some(crate::takeover::HandoffInfo {
                stop_w: -100_000,
                restart_w: -60_000,
                ..Default::default()
            }),
            ..play.clone()
        };
        assert!(run(hf, &a, &b, &live_a, &live_b, &mid).is_none());
        // NEGATIVE: B's live telemetry never carried 16 near it.
        let live_b0 = vec![(1_380, "0".to_string())];
        assert!(run(hf, &a, &b, &live_a, &live_b0, &play).is_none());
        // NEGATIVE: a two-sample state.
        let two = vec![
            smp(1_067, Some("0")),
            smp(2_070, Some("16")),
            smp(3_067, Some("16")),
            smp(4_070, Some("0")),
        ];
        assert!(run(hf, &two, &b, &live_a, &live_b, &play).is_none());
        // NEGATIVE: another leaf; a remaining difference after the drop.
        assert!(run(
            "metrics.system.rail_5v.value",
            &a,
            &b,
            &live_a,
            &live_b,
            &play
        )
        .is_none());
        let more = vec![
            smp(1_067, Some("0")),
            smp(2_070, Some("16")),
            smp(3_067, Some("0")),
            smp(4_070, Some("64")),
        ];
        assert!(run(hf, &more, &b, &live_a, &live_b, &play).is_none());
    }

    #[test]
    fn p3a_r1_handoff_class_only_for_children_the_rig_signalled() {
        let dir = std::env::temp_dir().join(format!("xorgate-replay-r1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("shim.ndjson"),
            concat!(
                r#"{"k":"spawn","tool":"gst-launch-1.0","pid":9147,"argv":["videotestsrc"],"w":500}"#,
                "\n",
                r#"{"k":"line","pid":9147,"fd":1,"line":"Got EOS from element \"pipeline0\".","w":1400}"#,
                "\n",
                r#"{"k":"spawn","tool":"gst-launch-1.0","pid":9146,"argv":["videotestsrc"],"w":1000}"#,
                "\n",
                r#"{"k":"line","pid":9146,"fd":1,"line":"Got EOS from element \"pipeline0\".","w":4950}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("procs.ndjson"),
            concat!(
                "{\"k\":\"gone\",\"pid\":9147,\"w\":1500}\n",
                "{\"w\":4900,\"pid\":9146,\"k\":\"handoff\",\"sig\":\"term\"}\n",
                "{\"k\":\"gone\",\"pid\":9146,\"w\":5000}\n",
            ),
        )
        .unwrap();
        let m = MediaCapture::load(&dir, &[]);
        let end = |pid: u32| {
            m.spawns
                .iter()
                .find(|s| s.pid == pid)
                .and_then(|s| s.end.clone())
        };
        // Signalled while alive → the rig's class, even though gst printed EOS.
        assert_eq!(end(9146).as_deref(), Some("handoff:term"));
        // Ended before the handoff (no record) → its own class.
        assert_eq!(end(9147).as_deref(), Some("eos"));
        let _ = std::fs::remove_dir_all(&dir);

        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let cap = |w: i64, end: &str| Spawn {
            w,
            role: "capture".into(),
            key: "cam0".into(),
            pid: w as u32,
            argv: vec!["gst-launch-1.0".into()],
            env: BTreeMap::new(),
            playing: None,
            end: Some(end.into()),
            end_w: None,
            sid: None,
        };
        let side = |spawns: Vec<Spawn>| MediaCapture {
            kind: "js".into(),
            spawns,
            present: true,
            ..Default::default()
        };
        let unt = |a: &MediaCapture, b: &MediaCapture, path: &str| {
            let mut rep = Report::default();
            diff_media(a, b, &PlayInfo::default(), &table, &mut rep);
            rep.mismatches
                .iter()
                .filter(|x| x.stream == "children" && x.path == path && x.tol == UNTOLERATED)
                .count()
        };
        let same = side(vec![cap(1_000, "handoff:kill")]);
        assert_eq!(unt(&same, &same.clone(), "(ended)"), 0);
        // NEGATIVE: A's child was killed by the rig, B's ended by itself.
        assert_eq!(unt(&same, &side(vec![cap(1_000, "eos")]), "(ended)"), 1);
        // NEGATIVE: a spawn-count difference is not touched by R1.
        let two = side(vec![cap(1_000, "handoff:kill"), cap(20_000, "int")]);
        assert_eq!(unt(&same, &two, "(spawn count)"), 1);
    }

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

    /// review-wp6 M2: only the starvation SIGNATURE is a D30 drop — isolated,
    /// 900–999 ms after the previous sample; mono in ns like the recorders.
    /// F-E2-1 (Phase 4 gate, rec-disk run 1 = replay-20261021T074334Z,
    /// `<sid:telemetry#0>/segs0-2`): the JS recorder skipped the released
    /// rail value 5.11 through D30 — its 620.188 sample still carried the
    /// pre-release 5.10 (cache computed before the 619.452 release), its
    /// 621.200 sample had no `system` — while Rust wrote 5.11 at 620.097.
    /// The samples below are that run's (ts − 1792568600000); the series is
    /// the tape's released pmic values. Tolerated as T7/D30 ONLY for that
    /// shape; a real extra or wrong rail value stays UNTOLERATED.
    /// The PROPOSED rule block for the two rec-disk calibration rows
    /// (phase-5b/evidence/wp4.md "rec-disk proposals"; NOT in tolerances.toml
    /// until the reviewer signs it off).
    const PROPOSED_D30_JS: &str = r#"
[[rule]]
id = "TP5B"
title = "PROPOSED (Phase 5b WP4) - reviewer"
basis = "wp4.md rec-disk proposals"
kind = "decisions"

  [[rule.row]]
  id = "d30-js-rec-value-gap"
  what = "proposal"
  streams = ["rec-telemetry"]
  matcher = "d30_js_rec_value_gap"

  [[rule.row]]
  id = "d30-js-system-less-value"
  what = "proposal"
  streams = ["rec-telemetry"]
  matcher = "d30_js_system_less_value"
"#;

    /// PROPOSED d30_js_rec_value_gap / d30_js_system_less_value: inert with
    /// the committed table; with the proposed rows, only the two JS-vs-JS D30
    /// shapes of rec-disk .cal09r2 / .cal09 — never JS-vs-Rust, never a value
    /// the replay did not release, never a lacking sample that has `system`.
    #[test]
    fn proposed_d30_js_value_gaps_only_inside_the_reference() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/replay/tolerances.toml");
        let committed = Table::load(&path).unwrap();
        let proposed =
            Table::parse(&(std::fs::read_to_string(&path).unwrap() + PROPOSED_D30_JS)).unwrap();
        const T0: i64 = 1_792_568_600_000;
        // rows: (ts, Some((rail, hf))) or None = a sample without `system`.
        let seg = |rows: &[(i64, Option<(f64, i64)>)]| -> BTreeMap<u64, String> {
            let mut t = String::from("{\"type\":\"header\",\"v\":1}\n");
            for (i, (ts, sys)) in rows.iter().enumerate() {
                let sys = sys
                    .map(|(r, h)| {
                        format!(",\"system\":{{\"rail_5v\":{{\"value\":{r},\"unit\":\"V\"}},\"health_flags\":{{\"value\":{h}}}}}")
                    })
                    .unwrap_or_default();
                t.push_str(&format!(
                    "{{\"ts\":{},\"mono\":{},\"seq\":{i},\"metrics\":{{\"gps\":{{\"fix\":{{\"value\":3}}}}{sys}}}}}\n",
                    T0 + ts,
                    (T0 + ts) * 1_000_000
                ));
            }
            BTreeMap::from([(0u64, t)])
        };
        let pmic: Vec<(i64, String)> = [
            (17_000, "5.10"),
            (19_452, "5.11"),
            (20_978, "5.12"),
            (22_979, "5.13"),
        ]
        .iter()
        .map(|(w, v)| (T0 + w, v.to_string()))
        .collect();
        let run = |table: &Table,
                   a: &[(i64, Option<(f64, i64)>)],
                   b: &[(i64, Option<(f64, i64)>)],
                   kinds: (&str, &str)| {
            let mut rep = Report::default();
            let ep = (Timeline::new(), Timeline::new());
            rec_values(
                "<sid:telemetry#0>",
                (&seg(a), &seg(b)),
                &ep,
                &PlayInfo::default(),
                &crate::diff::InputFacts::default(),
                kinds,
                &pmic,
                table,
                &mut rep,
            );
            rep.mismatches
                .into_iter()
                .filter(|m| m.path.ends_with("(values)"))
                .map(|m| (m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let rail = "metrics.system.rail_5v.value(values)".to_string();
        let hf = "metrics.system.health_flags.value(values)".to_string();
        // .cal09r2: A records 5.11 once; B goes 5.10 (stale) → 5.12.
        let a = [
            (18_100, Some((5.1, 0))),
            (19_100, Some((5.1, 0))),
            (20_100, Some((5.1, 0))),
            (21_100, Some((5.11, 0))),
            (22_100, Some((5.12, 0))),
            (23_100, Some((5.13, 0))),
            (24_100, Some((5.13, 0))),
        ];
        let b = [
            (18_110, Some((5.1, 0))),
            (19_110, Some((5.1, 0))),
            (20_110, Some((5.1, 0))),
            (21_110, Some((5.12, 0))),
            (22_110, Some((5.12, 0))),
            (23_110, Some((5.13, 0))),
            (24_110, Some((5.13, 0))),
        ];
        assert_eq!(
            run(&proposed, &a, &b, ("js", "js")),
            vec![(rail.clone(), "TP5B/d30-js-rec-value-gap".to_string())]
        );
        assert_eq!(
            run(&committed, &a, &b, ("js", "js")),
            vec![(rail.clone(), UNTOLERATED.to_string())]
        );
        // JS-vs-Rust (the reviewed row wants the system-less JS sample): never.
        assert_eq!(
            run(&proposed, &b, &a, ("js", "rust")),
            vec![(rail.clone(), UNTOLERATED.to_string())]
        );
        // A value the replay never released then (5.14): never.
        let mut a2 = a;
        a2[3] = (21_100, Some((5.14, 0)));
        assert_eq!(
            run(&proposed, &a2, &b, ("js", "js")),
            vec![(rail.clone(), UNTOLERATED.to_string())]
        );
        // .cal09: A's boot transient hf 16 for one sample; B's sample then had no system.
        let a = [
            (18_100, Some((5.1, 0))),
            (19_100, Some((5.1, 16))),
            (20_100, Some((5.1, 0))),
            (21_100, Some((5.1, 0))),
            (22_100, Some((5.1, 16))),
        ];
        let b = [
            (18_110, Some((5.1, 0))),
            (19_105, None),
            (20_110, Some((5.1, 0))),
            (21_110, Some((5.1, 0))),
            (22_110, Some((5.1, 16))),
        ];
        assert_eq!(
            run(&proposed, &a, &b, ("js", "js")),
            vec![(hf.clone(), "TP5B/d30-js-system-less-value".to_string())]
        );
        assert!(run(&committed, &a, &b, ("js", "js"))
            .iter()
            .any(|(p, t)| *p == hf && t == UNTOLERATED));
        assert!(run(&proposed, &a, &b, ("js", "rust"))
            .iter()
            .any(|(p, t)| *p == hf && t == UNTOLERATED));
        // B's sample HAS system (it just did not see 16): never.
        let mut b2 = b;
        b2[1] = (19_105, Some((5.1, 0)));
        assert!(run(&proposed, &a, &b2, ("js", "js"))
            .iter()
            .any(|(p, t)| *p == hf && t == UNTOLERATED));
        // A holds 16 for TWO samples: never.
        let mut a3 = a;
        a3[2] = (20_100, Some((5.1, 16)));
        assert!(run(&proposed, &a3, &b, ("js", "js"))
            .iter()
            .any(|(p, t)| *p == hf && t == UNTOLERATED));
    }

    #[test]
    fn d30_rec_value_gap_only_for_a_js_d30_skip() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        const T0: i64 = 1_792_568_600_000;
        let seg = |rows: &[(i64, Option<f64>)]| -> BTreeMap<u64, String> {
            let mut t = String::from("{\"type\":\"header\",\"v\":1}\n");
            for (i, (ts, rail)) in rows.iter().enumerate() {
                let sys = rail
                    .map(|v| {
                        format!(",\"system\":{{\"rail_5v\":{{\"value\":{v},\"unit\":\"V\"}}}}")
                    })
                    .unwrap_or_default();
                t.push_str(&format!(
                    "{{\"ts\":{},\"mono\":{},\"seq\":{i},\"metrics\":{{\"gps\":{{\"fix\":{{\"value\":3}}}}{sys}}}}}\n",
                    T0 + ts,
                    (T0 + ts) * 1_000_000
                ));
            }
            BTreeMap::from([(0u64, t)])
        };
        let js = [
            (19_193, Some(5.1)),
            (20_188, Some(5.1)),
            (21_200, None),
            (22_202, Some(5.12)),
            (23_208, Some(5.12)),
            (24_218, Some(5.13)),
            (25_217, Some(5.13)),
            (26_221, Some(5.1)),
            (27_223, Some(5.1)),
            (28_224, Some(5.11)),
            (29_224, Some(5.11)),
            (30_228, Some(5.12)),
        ];
        let rs = [
            (19_095, Some(5.1)),
            (20_097, Some(5.11)),
            (21_100, Some(5.12)),
            (22_106, Some(5.12)),
            (23_109, Some(5.13)),
            (24_113, Some(5.13)),
            (25_114, Some(5.1)),
            (26_118, Some(5.1)),
            (27_125, Some(5.11)),
            (28_131, Some(5.11)),
            (29_134, Some(5.12)),
            (30_138, Some(5.12)),
        ];
        let pmic: Vec<(i64, String)> = [
            (17_000, "5.10"),
            (19_452, "5.11"),
            (20_978, "5.12"),
            (22_979, "5.13"),
            (24_976, "5.10"),
            (26_976, "5.11"),
            (28_978, "5.12"),
        ]
        .iter()
        .map(|(w, v)| (T0 + w, v.to_string()))
        .collect();
        let run = |a: &[(i64, Option<f64>)],
                   b: &[(i64, Option<f64>)],
                   kinds: (&str, &str),
                   pmic: &[(i64, String)]| {
            let mut rep = Report::default();
            let ep = (Timeline::new(), Timeline::new());
            rec_values(
                "<sid:telemetry#0>",
                (&seg(a), &seg(b)),
                &ep,
                &PlayInfo::default(),
                &crate::diff::InputFacts::default(),
                kinds,
                pmic,
                &table,
                &mut rep,
            );
            rep.mismatches
                .into_iter()
                .map(|m| format!("{} {} A={} B={}", m.path, m.tol, m.a, m.b))
                .collect::<Vec<_>>()
        };
        let row = |tol: &str| {
            vec![format!(
                "metrics.system.rail_5v.value(values) {tol} A= B=5.11×1@{}",
                T0 + 20_097
            )]
        };
        // The gate row: JS (A) lacks 5.11, Rust (B) holds it.
        assert_eq!(run(&js, &rs, ("js", "rust"), &pmic), row("T7/D30"));
        // Either column order.
        assert_eq!(
            run(&rs, &js, ("rust", "js"), &pmic),
            vec![format!(
                "metrics.system.rail_5v.value(values) T7/D30 A=5.11×1@{} B=",
                T0 + 20_097
            )]
        );
        // Never for JS-vs-JS, Rust-vs-Rust, or a RUST side lacking it.
        assert_eq!(run(&js, &rs, ("js", "js"), &pmic), row(UNTOLERATED));
        assert_eq!(run(&js, &rs, ("rust", "rust"), &pmic), row(UNTOLERATED));
        assert_eq!(run(&js, &rs, ("rust", "js"), &pmic), row(UNTOLERATED));
        // No released series to prove the value: untolerated.
        assert_eq!(run(&js, &rs, ("js", "rust"), &[]), row(UNTOLERATED));
        // A value the replay never released (an extra/wrong rail value).
        let mut wrong = rs;
        wrong[1].1 = Some(5.14);
        assert_eq!(
            run(&js, &wrong, ("js", "rust"), &pmic),
            vec![format!(
                "metrics.system.rail_5v.value(values) {UNTOLERATED} A= B=5.14×1@{}",
                T0 + 20_097
            )]
        );
        // The released value at the WRONG time (Rust shows 5.11 after 5.12
        // was released + the 1 s cache): untolerated.
        let late: Vec<(i64, Option<f64>)> = rs
            .iter()
            .map(|(t, v)| if *t == 20_097 { (22_050, *v) } else { (*t, *v) })
            .filter(|(t, _)| *t != 21_100 && *t != 22_106)
            .collect();
        let late_js: Vec<(i64, Option<f64>)> =
            js.iter().copied().filter(|(t, _)| *t != 22_202).collect();
        assert!(run(&late_js, &late, ("js", "rust"), &pmic)
            .iter()
            .any(|r| r.starts_with(&format!(
                "metrics.system.rail_5v.value(values) {UNTOLERATED}"
            ))));
        // No system-less JS sample in the window (the JS skipped it with
        // stale values only): not D30's drop → untolerated.
        let mut stale = js;
        stale[2].1 = Some(5.1);
        assert_eq!(run(&stale, &rs, ("js", "rust"), &pmic), row(UNTOLERATED));
        // review-gate-fixes F-R3: the ORDER is the signature — the
        // system-less sample first, then a stale repeat: untolerated.
        let mut swapped = js;
        swapped[1].1 = None;
        swapped[2].1 = Some(5.1);
        swapped[2].0 = 20_400; // still < release(v) + 1000: a stale repeat
        assert_eq!(run(&swapped, &rs, ("js", "rust"), &pmic), row(UNTOLERATED));
        // No stale repeat at all (only the system-less sample): untolerated.
        let mut no_stale = js;
        no_stale[1].1 = None;
        let got = run(&no_stale, &rs, ("js", "rust"), &pmic);
        assert!(got.iter().all(|r| !r.contains("T7/D30")), "{got:?}");
        // The exact value: a Rust 5.111 is not the released 5.11.
        let mut off = rs;
        off[1].1 = Some(5.111);
        assert_eq!(
            run(&js, &off, ("js", "rust"), &pmic),
            vec![format!(
                "metrics.system.rail_5v.value(values) {UNTOLERATED} A= B=5.111×1@{}",
                T0 + 20_097
            )]
        );
    }

    #[test]
    fn d30_starved_drop_signature_only() {
        let smp = |mono_ms: f64, sys: bool| {
            let mut m = serde_json::json!({"gps": {}});
            if sys {
                m["system"] = serde_json::json!({});
            }
            serde_json::json!({"mono": mono_ms * 1e6, "metrics": m})
        };
        let seg = |rows: &[(f64, bool)]| rows.iter().map(|(t, s)| smp(*t, *s)).collect::<Vec<_>>();
        // A late sample at 2050, the next one 950 ms later lacks system: 1.
        assert_eq!(
            d30_starved_drops(&seg(&[
                (0.0, true),
                (1000.0, true),
                (2050.0, true),
                (3000.0, false),
                (4000.0, true)
            ])),
            1
        );
        // The same drop on an on-time grid (gap 1000): no signature.
        assert_eq!(
            d30_starved_drops(&seg(&[
                (0.0, true),
                (1000.0, true),
                (2000.0, true),
                (3000.0, false),
                (4000.0, true)
            ])),
            0
        );
        // Two in a row, or at a segment edge: never.
        assert_eq!(
            d30_starved_drops(&seg(&[
                (0.0, true),
                (1050.0, true),
                (2000.0, false),
                (2950.0, false),
                (4000.0, true)
            ])),
            0
        );
        assert_eq!(
            d30_starved_drops(&seg(&[(1050.0, true), (2000.0, false)])),
            0
        );
        // Gap under 900 ms is not the signature either.
        assert_eq!(
            d30_starved_drops(&seg(&[
                (0.0, true),
                (1000.0, true),
                (2150.0, true),
                (3000.0, false),
                (4000.0, true)
            ])),
            0
        );
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
        assert_eq!(run(0, 5), vec!["T13/system-presence".to_string()]);
        assert_eq!(run(0, 6), unt);
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
        // review-wp6 M3: the cut fragment is judged by its END instant
        // (T12/cut-fragment = d30_session_end (a)+(c)+(d) without (b)), which
        // now covers the D30 JS-late restore too.
        let cut = "T12/cut-fragment";
        assert_eq!(run(js(), rs()), row(cut));
        assert_eq!(run(rs(), js()), row(cut), "either column order");
        // No δ band any more: a Rust-later start with the same end + anchor.
        let rs_late = side("rust", 527_774, 529_790, 2_540, 523_845);
        assert_eq!(run(js(), rs_late), row(cut));
        // Rust-vs-Rust too.
        let mut r2 = js();
        r2.kind = "rust".into();
        assert_eq!(run(r2, rs()), row(cut));
        // A boot-started session (no shed change on either side): anchored to
        // the boot, still the end instant.
        let mut jb = js();
        jb.shed_changes.clear();
        let mut rb = rs();
        rb.shed_changes.clear();
        assert_eq!(run(jb.clone(), rb), row(cut));
        // ... but a shed anchor on one side only is not the same session start.
        assert_eq!(run(jb, rs()), row(UNTOLERATED));
        // (d) a spawn more than one publish tick before its own side's change
        // is not anchored to it.
        assert_eq!(
            run(side("js", 526_868, 528_886, 3_451, 528_100), rs()),
            row(UNTOLERATED)
        );
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
        // (d) the spawn-to-restore lags differ by > one publish tick (1100 ms).
        assert_eq!(
            run(side("js", 526_868, 528_886, 3_451, 521_500), rs()),
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

    /// Phase 5b WP5 (R-P5bG-1): a fragment CUT by a cloud-config preset
    /// change is judged by T12/cut-fragment's end instant when both ends
    /// follow the shared release of that config record within endMs (TS: the
    /// config handler's synchronous `recording.reapply()` → `stopAndWait`,
    /// worker.ts:960-980, manager.ts:620-626). Numbers from the gate's
    /// `rec-preset.jsrs09` (cam1#0/00006: JS 4079 ms ending r + 14, Rust 4239
    /// ending r + 70; the JS session started 129 ms later). Every other shape
    /// stays UNTOLERATED.
    #[test]
    fn t12_cut_fragment_covers_a_config_preset_cut_only() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let sid = "<sid:video:cam1#0>";
        let next_sid = "<sid:video:cam1#1>";
        let r = 1_793_198_919_363i64; // the config release
                                      // (kind, first startTs, last durationMs, next capture sid, end tag)
        let side = |kind: &str, st0: i64, last_dur: i64, next: Option<&str>, end: &str| {
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
            let spawn = |w: i64, sid: Option<&str>, end: Option<&str>| Spawn {
                w,
                role: "capture".into(),
                key: "cam1".into(),
                pid: 7,
                argv: vec!["gst".into()],
                env: BTreeMap::new(),
                playing: Some(w + 2_000),
                end: end.map(Into::into),
                end_w: None,
                sid: sid.map(Into::into),
            };
            let mut spawns = vec![spawn(st0 - 2_000, Some(sid), Some(end))];
            if let Some(n) = next {
                spawns.push(spawn(r + 2_000, Some(n), None));
            }
            MediaCapture {
                kind: kind.into(),
                spawns,
                finals,
                present: true,
                ..Default::default()
            }
        };
        // A ends r + 14, B ends r + 70 (dur differs by 185 > T12's 150).
        let js = |next: Option<&str>| side("js", r + 14 - 4_079 - 10_000, 4_079, next, "int");
        let rs = |next: Option<&str>| side("rust", r + 70 - 4_239 - 10_000, 4_239, next, "int");
        // the tape: the boot config at t 400 and the preset change at t 70000
        let dir = std::env::temp_dir().join(format!("t12-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tape = |topic: &str| {
            let p = dir.join(format!("{}.ndjson", topic.replace('/', "_")));
            let rec = |t: u64| {
                serde_json::json!({"t": t, "w": 0, "k": "mqtt_in", "topic": topic, "payload": "{}"})
                    .to_string()
            };
            std::fs::write(&p, format!("{}\n{}\n", rec(400), rec(70_000))).unwrap();
            p.display().to_string()
        };
        let play_at = |topic: &str, release: i64| PlayInfo {
            tape: tape(topic),
            releases: vec![
                crate::diff::Release {
                    w: r - 70_000,
                    due: 0,
                    t: 400,
                    k: "mqtt_in".into(),
                },
                crate::diff::Release {
                    w: release,
                    due: 0,
                    t: 70_000,
                    k: "mqtt_in".into(),
                },
            ],
            ..Default::default()
        };
        let cfg = "xorgate/devices/x/config";
        let run = |a: MediaCapture, b: MediaCapture, play: &PlayInfo| {
            let mut rep = Report::default();
            diff_media(&a, &b, play, &table, &mut rep);
            rep.mismatches
                .into_iter()
                .filter(|m| m.stream == "sidecar" && m.path == "durationMs")
                .map(|m| format!("{} {}", m.at, m.tol))
                .collect::<Vec<_>>()
        };
        let row = |tol: &str| vec![format!("{sid}/00001 {tol}")];
        let cut = "T12/cut-fragment";
        let play = play_at(cfg, r);
        assert_eq!(run(js(Some(next_sid)), rs(Some(next_sid)), &play), row(cut));
        assert_eq!(
            run(rs(Some(next_sid)), js(Some(next_sid)), &play),
            row(cut),
            "either order"
        );
        // NEGATIVE: no config release on the tape (the pre-WP5 scope) — e.g. a
        // local edit picked up by each agent's own 5 s poll.
        assert_eq!(
            run(js(Some(next_sid)), rs(Some(next_sid)), &PlayInfo::default()),
            row(UNTOLERATED)
        );
        // NEGATIVE: the release is on another topic (not a config document).
        assert_eq!(
            run(
                js(Some(next_sid)),
                rs(Some(next_sid)),
                &play_at("xorgate/devices/x/commands", r)
            ),
            row(UNTOLERATED)
        );
        // NEGATIVE: one end more than endMs (150) after the release (both ends
        // still within 150 of each other: 14 vs 160).
        assert_eq!(
            run(
                js(Some(next_sid)),
                side(
                    "rust",
                    r + 160 - 4_239 - 10_000,
                    4_239,
                    Some(next_sid),
                    "int"
                ),
                &play
            ),
            row(UNTOLERATED)
        );
        // NEGATIVE: the fragment ended BEFORE the release (not cut by it).
        assert_eq!(
            run(
                js(Some(next_sid)),
                rs(Some(next_sid)),
                &play_at(cfg, r + 20)
            ),
            row(UNTOLERATED)
        );
        // NEGATIVE: respawned on one side only / not a new session / a kill.
        assert_eq!(run(js(Some(next_sid)), rs(None), &play), row(UNTOLERATED));
        assert_eq!(run(js(Some(sid)), rs(Some(sid)), &play), row(UNTOLERATED));
        assert_eq!(run(js(None), rs(Some(next_sid)), &play), row(UNTOLERATED));
        let k = |m: MediaCapture| {
            let mut m = m;
            m.spawns[0].end = Some("external".into());
            m
        };
        assert_eq!(
            run(k(js(Some(next_sid))), rs(Some(next_sid)), &play),
            row(UNTOLERATED)
        );
        // NEGATIVE: a powerShed/liveBlock change within tickMs of the release
        // (a shed cut is D30's, never this arm).
        let mut j = js(Some(next_sid));
        j.shed_changes = vec![r + 900];
        let mut b = rs(Some(next_sid));
        b.shed_changes = vec![r + 905];
        assert_eq!(run(j, b, &play), row(UNTOLERATED));

        // review-wp5 R-WP5-1: PER-SIDE negatives — each condition of the
        // config arm, loosened on ONE side only, must still fail.
        // (1) A's end > r + endMs while B's is in range (|Δ end| = 90 ≤ 150).
        let js_late = side("js", r + 160 - 4_079 - 10_000, 4_079, Some(next_sid), "int");
        assert_eq!(
            run(js_late, rs(Some(next_sid)), &play),
            row(UNTOLERATED),
            "A end r+160"
        );
        // (2) the new session's capture spawned BEFORE r on one side only.
        let early = |mut m: MediaCapture| {
            m.spawns[1].w = r - 50;
            m
        };
        assert_eq!(
            run(early(js(Some(next_sid))), rs(Some(next_sid)), &play),
            row(UNTOLERATED),
            "A respawn before r"
        );
        assert_eq!(
            run(js(Some(next_sid)), early(rs(Some(next_sid))), &play),
            row(UNTOLERATED),
            "B respawn before r"
        );
        // (3) a same-session respawn on ONE side only.
        assert_eq!(
            run(js(Some(sid)), rs(Some(next_sid)), &play),
            row(UNTOLERATED),
            "A same session"
        );
        assert_eq!(
            run(js(Some(next_sid)), rs(Some(sid)), &play),
            row(UNTOLERATED),
            "B same session"
        );
        // (4) a shed change near r on ONE side only.
        let shed = |mut m: MediaCapture| {
            m.shed_changes = vec![r + 900];
            m
        };
        assert_eq!(
            run(shed(js(Some(next_sid))), rs(Some(next_sid)), &play),
            row(UNTOLERATED),
            "A shed near r"
        );
        assert_eq!(
            run(js(Some(next_sid)), shed(rs(Some(next_sid))), &play),
            row(UNTOLERATED),
            "B shed near r"
        );
        // R-WP5-8: a non-config message released at the SAME tape t as the
        // config record is not a config release (joined by occurrence).
        let mixed = dir.join("mixed.ndjson");
        let rec = |t: u64, topic: &str| {
            serde_json::json!({"t": t, "w": 0, "k": "mqtt_in", "topic": topic, "payload": "{}"})
                .to_string()
        };
        std::fs::write(
            &mixed,
            format!(
                "{}\n{}\n{}\n",
                rec(400, cfg),
                rec(70_000, cfg),
                rec(70_000, "xorgate/devices/x/commands")
            ),
        )
        .unwrap();
        let rel = |w: i64, t: u64| crate::diff::Release {
            w,
            due: 0,
            t,
            k: "mqtt_in".into(),
        };
        // config released early (r − 500: the ends are > endMs after it), the
        // command at r: only the command explains the ends → UNTOLERATED.
        let p = PlayInfo {
            tape: mixed.display().to_string(),
            releases: vec![rel(r - 70_000, 400), rel(r - 500, 70_000), rel(r, 70_000)],
            ..Default::default()
        };
        assert_eq!(
            run(js(Some(next_sid)), rs(Some(next_sid)), &p),
            row(UNTOLERATED)
        );
        // … and with the config released at r it is the config arm again.
        let p = PlayInfo {
            tape: mixed.display().to_string(),
            releases: vec![rel(r - 70_000, 400), rel(r, 70_000), rel(r + 1, 70_000)],
            ..Default::default()
        };
        assert_eq!(run(js(Some(next_sid)), rs(Some(next_sid)), &p), row(cut));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// review-wp6 F5 (re-review run rr-m1, 15 busy loops + host overload):
    /// an interior fragment's boundary moved 272 ms on one side (inside
    /// T12's startTs ±300) while its neighbour compensated — its durationMs
    /// differs by 302 but both its boundaries agree within ±300: tolerated
    /// as `T12/interior-boundaries`. A moved END boundary (> 300) or the
    /// session's LAST fragment stays as before.
    #[test]
    fn interior_fragment_durations_are_judged_by_their_boundaries() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let sid = "<sid:video:cam0#0>";
        let side = |starts: &[i64], last_dur: i64| {
            let mut finals = BTreeMap::new();
            for (i, st) in starts.iter().enumerate() {
                let dur = starts.get(i + 1).map_or(last_dur, |n| n - st);
                finals.insert(
                    format!("recordings/video/cam0/{sid}/{i:05}.json"),
                    serde_json::to_vec(&serde_json::json!({
                        "seq": i, "startTs": st, "startMono": st - starts[0] + 5,
                        "durationMs": dur, "sizeBytes": 100,
                    }))
                    .unwrap(),
                );
            }
            MediaCapture {
                kind: "rust".into(),
                spawns: vec![Spawn {
                    w: starts[0] - 2_500,
                    role: "capture".into(),
                    key: "cam0".into(),
                    pid: 7,
                    argv: vec!["gst".into()],
                    env: BTreeMap::new(),
                    playing: Some(starts[0] - 2_000),
                    end: Some("int".into()),
                    end_w: None,
                    sid: Some(sid.into()),
                }],
                finals,
                present: true,
                ..Default::default()
            }
        };
        let run = |a: MediaCapture, b: MediaCapture| {
            let mut rep = Report::default();
            diff_media(&a, &b, &PlayInfo::default(), &table, &mut rep);
            rep.mismatches
                .into_iter()
                // within T12's own ±150 (the +30 at 00003): not this rule's
                .filter(|m| m.stream == "sidecar" && m.path == "durationMs" && m.tol != "T12")
                .map(|m| format!("{} {}", m.at, m.tol))
                .collect::<Vec<_>>()
        };
        let a = side(&[400_000, 410_000, 420_000, 430_000], 5_000);
        // B's 00002 boundary 272 ms late: 00001 is 272 longer, 00002 272
        // shorter (+30 at 00003's start): two interior rows by boundaries.
        let b = side(&[400_000, 410_000, 420_272, 430_030], 4_970);
        assert_eq!(
            run(a.clone(), b),
            vec![
                format!("{sid}/00001 T12/interior-boundaries"),
                format!("{sid}/00002 T12/interior-boundaries"),
            ]
        );
        // An END boundary 320 ms off its peer (beyond T12's startTs bound).
        let far = side(&[400_000, 410_000, 420_320, 430_000], 5_000);
        // (00002's own END agrees again: its start row fails instead.)
        assert_eq!(
            run(a.clone(), far),
            vec![
                format!("{sid}/00001 {UNTOLERATED}"),
                format!("{sid}/00002 T12/interior-boundaries"),
            ]
        );
        // A late finalize on B: 00001 ends 200 ms PAST its own successor's
        // start (the boundaries still agree within ±300, the seam does not).
        let mut late = side(&[400_000, 410_000, 420_000, 430_000], 5_000);
        let f = "recordings/video/cam0/<sid:video:cam0#0>/00001.json".to_string();
        let mut v: Value = serde_json::from_slice(&late.finals[&f]).unwrap();
        v["durationMs"] = serde_json::json!(10_200);
        late.finals.insert(f, serde_json::to_vec(&v).unwrap());
        assert_eq!(
            run(a.clone(), late),
            vec![format!("{sid}/00001 {UNTOLERATED}")]
        );
        // The LAST fragment (no successor) is not interior: its own rules.
        let short = side(&[400_000, 410_000, 420_000, 430_000], 4_700);
        assert_eq!(run(a, short), vec![format!("{sid}/00003 {UNTOLERATED}")]);
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
        // Three CONSECUTIVE steps missing, even with B's every tick 1 ms
        // after a release: never a raced tick.
        let three = [v(1.0), v(2.0), v(2.0), v(2.0), v(2.0), v(6.0)];
        assert_eq!(
            run(cap(200, &full), cap(651, &three)),
            vec![format!("metrics.gps.lat.value(values) {UNTOLERATED}")]
        );
        // The collision must be with the release that BROUGHT the step: B
        // ticks 60 ms off it (and 1 ms off nothing) → untolerated.
        assert_eq!(
            run(cap(200, &full), cap(710, &skip)),
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

    /// review-wp6 F4 (reviewer's heavy run 1, scratchpad `m1/diff.md`; gate
    /// run 033610Z `snr_max`): a two-sided 1×1 gps hunk is two adjacent
    /// misses — tolerated only when both values are consecutive on each
    /// side's own replayed collector state and each lacking side's tick raced
    /// the instant its missing value became current there.
    #[test]
    fn rec_values_two_adjacent_misses_need_their_own_races() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let nmea = |body: String| {
            let cs = body.bytes().fold(0u8, |x, c| x ^ c);
            format!("${body}*{cs:02X}")
        };
        // Epoch k: GGA at 100 000 + k*1000 (+ `late` on that side), the
        // position 1430.0000 + k*0.0006 min N; one-message GPGSV cycle 8 ms
        // later with SNR snr(k): 30 30 30 31 32 33 30 30 30.
        let gga = |k: i64| {
            nmea(format!(
                "GPGGA,000000.00,14{:07.4},N,12100.0000,E,1,08,0.9,545.{k},M,0.0,M,,",
                30.0 + k as f64 * 0.0006
            ))
        };
        let snr = |k: i64| [30, 30, 30, 31, 32, 33, 30, 30, 30][k as usize];
        let gsv = |k: i64| nmea(format!("GPGSV,1,1,01,05,31,077,{},1", snr(k)));
        let lat = |k: i64| {
            let f = xorgate_core::nmea::parse_sentence(&gga(k)).unwrap();
            xorgate_core::round::round(f.latitude.unwrap(), 6)
        };
        let reads = |late: i64| -> Vec<(i64, String)> {
            (0..9)
                .flat_map(|k| {
                    let w = 100_000 + k * 1000 + late;
                    [(w, gga(k)), (w + 8, gsv(k))]
                })
                .collect()
        };
        // Samples `(ts, leaf, value)` → one segment.
        let seg = |s: &[(i64, &str, f64)]| {
            let mut t = String::from("{\"type\":\"header\",\"v\":1}\n");
            for (i, (ts, leaf, v)) in s.iter().enumerate() {
                t.push_str(&format!(
                    "{{\"ts\":{ts},\"seq\":{i},\"metrics\":{{\"gps\":{{\"{leaf}\":{{\"value\":{v}}}}}}}}}\n"
                ));
            }
            t
        };
        let cap = |s: &[(i64, &str, f64)], reads: Vec<(i64, String)>| MediaCapture {
            kind: "rust".into(),
            finals: BTreeMap::from([(
                "recordings/telemetry/<sid:telemetry#0>/seg-1-00000.jsonl.gz".to_string(),
                seg(s).into_bytes(),
            )]),
            nmea_reads: reads,
            present: true,
            ..Default::default()
        };
        // The player's tape + releases of the same lines (S-F4: a Rust read
        // anchors a race only within collisionMs of the release of its line).
        let tape_path =
            std::env::temp_dir().join(format!("xorgate-replay-f4-{}.ndjson", std::process::id()));
        let mut tape_text = String::from("{\"v\":1,\"k\":\"meta\"}\n");
        let mut releases = Vec::new();
        for (i, (w, line)) in reads(0).into_iter().enumerate() {
            tape_text.push_str(
                &serde_json::json!({"t": i, "w": w, "k": "nmea", "line": line}).to_string(),
            );
            tape_text.push('\n');
            releases.push(crate::diff::Release {
                w,
                due: w,
                t: i as u64,
                k: "nmea".into(),
            });
        }
        std::fs::write(&tape_path, tape_text).unwrap();
        let play = PlayInfo {
            tape: tape_path.display().to_string(),
            releases,
            ..Default::default()
        };
        let run_with = |a: MediaCapture, b: MediaCapture, play: &PlayInfo| {
            let mut rep = Report::default();
            diff_media(&a, &b, play, &table, &mut rep);
            rep.mismatches
                .into_iter()
                .filter(|m| m.stream == "rec-telemetry" && m.path.starts_with("metrics.gps"))
                .map(|m| format!("{} {}", m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let run = |a: MediaCapture, b: MediaCapture| run_with(a, b, &play);
        let t = |i: i64, d: i64| 100_000 + i * 1000 + d;
        let la = |v: &[(i64, i64)]| {
            v.iter()
                .map(|&(ts, k)| (ts, "lat", lat(k)))
                .collect::<Vec<_>>()
        };
        let ok = vec!["metrics.gps.lat.value(values) T4/rec-sample".to_string()];
        let bad = vec![format!("metrics.gps.lat.value(values) {UNTOLERATED}")];
        // POSITION. Both recorders tick on the GGA phase. A: slots 3 and 4
        // tick 3 ms BEFORE their read → 1 2 2 3 5 6 (misses 4); B: slot 3
        // before, slot 4 after → 1 2 2 4 5 6 (misses 3). LCS: one hunk
        // A=[3×1] B=[4×1] at slot 4 — the reviewer's shape.
        let sa = |d: i64| {
            la(&[
                (t(1, d), 1),
                (t(2, d), 2),
                (t(3, -3), 2),
                (t(4, -3), 3),
                (t(5, d), 5),
                (t(6, d), 6),
            ])
        };
        let sb = |d3: i64| {
            la(&[
                (t(1, 10), 1),
                (t(2, 10), 2),
                (t(3, d3), 2),
                (t(4, 10), 4),
                (t(5, 10), 5),
                (t(6, 10), 6),
            ])
        };
        assert_eq!(run(cap(&sa(10), reads(0)), cap(&sb(-3), reads(0))), ok);
        // B's slot-3 tick 120 ms before ITS read of 3: no race at B's miss.
        assert_eq!(run(cap(&sa(10), reads(0)), cap(&sb(-120), reads(0))), bad);
        // Judged at each side's OWN read: A reads every epoch 80 ms late, so
        // its slot-4 tick is 83 ms from its read of 4 → no race.
        assert_eq!(run(cap(&sa(100), reads(80)), cap(&sb(-3), reads(0))), bad);
        // B holds a value no input carried: a different value, never.
        let mut s9 = sb(-3);
        s9[3].2 = 14.6;
        assert_eq!(run(cap(&sa(10), reads(0)), cap(&s9, reads(0))), bad);
        // No reads and no player tape (nothing replays the collector).
        assert_eq!(
            run_with(
                cap(&sa(10), vec![]),
                cap(&sb(-3), vec![]),
                &PlayInfo::default()
            ),
            bad
        );
        // S-F4: A's reader 300 ms late on every line, its ticks 3 ms before
        // ITS OWN reads (a self-consistent late reader): its reads are
        // outside collisionMs of the player's releases → no anchor → UNTOLERATED;
        // the same shape without a player tape cannot anchor either.
        let late = la(&[
            (t(1, 310), 1),
            (t(2, 310), 2),
            (t(3, 297), 2),
            (t(4, 297), 3),
            (t(5, 310), 5),
            (t(6, 310), 6),
        ]);
        assert_eq!(run(cap(&late, reads(300)), cap(&sb(-3), reads(0))), bad);
        assert_eq!(
            run_with(
                cap(&sa(10), reads(0)),
                cap(&sb(-3), reads(0)),
                &PlayInfo::default()
            ),
            bad
        );
        // The player shifts each NMEA time to the run's clock before the
        // write: the agent reads the SHIFTED line, which still anchors.
        let shifted = |r: Vec<(i64, String)>| -> Vec<(i64, String)> {
            r.into_iter()
                .map(|(w, l)| (w, crate::rewrite::shift_nmea_line(&l, 3_600)))
                .collect()
        };
        assert_ne!(shifted(reads(0))[0].1, reads(0)[0].1, "the GGA time moved");
        let play_shift = PlayInfo {
            shift_s: 3_600,
            ..play.clone()
        };
        assert_eq!(
            run_with(
                cap(&sa(10), shifted(reads(0))),
                cap(&sb(-3), shifted(reads(0))),
                &play_shift
            ),
            ok
        );
        let (bounded, lags) = bound_reads(&reads(30), &reads(0), 50);
        assert!(bounded.iter().all(|r| r.2) && lags.iter().all(|l| *l == 30));
        let (bounded, _) = bound_reads(&reads(80), &reads(0), 50);
        assert!(bounded.iter().all(|r| !r.2));
        // Non-consecutive values (A holds 2 where B holds 4 at slot 3/4,
        // every tick raced): never two misses.
        let anchored = |r: Vec<(i64, String)>| -> Vec<(i64, String, bool)> {
            r.into_iter().map(|(w, l)| (w, l, true)).collect()
        };
        let tl = gps_timeline(&anchored(reads(0)));
        let leaves = |v: &[(i64, &str, f64)]| -> Vec<(i64, BTreeMap<String, String>)> {
            v.iter()
                .map(|(ts, leaf, x)| {
                    (
                        *ts,
                        BTreeMap::from([(format!("metrics.gps.{leaf}.value"), x.to_string())]),
                    )
                })
                .collect()
        };
        let p = "metrics.gps.lat.value";
        let (l_a, l_b) = (leaves(&sa(10)), leaves(&sb(-3)));
        let (x, y) = (lat(3).to_string(), lat(4).to_string());
        assert!(adjacent_races(
            (&l_a, &l_b),
            (tl.get(p), tl.get(p)),
            ((t(4, -3), &x), (t(4, 10), &y)),
            50
        ));
        let x2 = lat(2).to_string();
        assert!(!adjacent_races(
            (&l_a, &l_b),
            (tl.get(p), tl.get(p)),
            ((t(3, -3), &x2), (t(4, 10), &y)),
            50
        ));
        assert_eq!(tl["metrics.gps.lat.value"][3], (t(3, 0), lat(3), true));
        assert_eq!(
            tl["metrics.gps.snr_max.value"]
                .iter()
                .map(|c| c.1)
                .collect::<Vec<_>>(),
            [30.0, 31.0, 32.0, 33.0, 30.0]
        );
        assert_eq!(
            tl["metrics.gps.snr_max.value"][1].0,
            t(3, 8),
            "a GSV value is current at its cycle's last message"
        );
        // SKY (gate run 033610Z): snr_max becomes current at the GSV, 8 ms
        // after the GGA. A ticks 34 ms after each epoch but its slot-3 read
        // fell before the GSV (stamped 26 ms after it) → 30 30 30 32 33 30
        // (misses 31); B ticks 2 ms before the GSV at slots 3-4 and 1 ms
        // after at 5 → 30 30 30 31 33 30 (misses 32). Hunk A=[32] B=[31].
        let sn = |v: &[(i64, f64)]| {
            v.iter()
                .map(|&(ts, x)| (ts, "snr_max", x))
                .collect::<Vec<_>>()
        };
        let sky_a = sn(&[
            (t(1, 34), 30.0),
            (t(2, 34), 30.0),
            (t(3, 34), 30.0),
            (t(4, 34), 32.0),
            (t(5, 34), 33.0),
            (t(6, 34), 30.0),
        ]);
        let sky_b = |d4: i64| {
            sn(&[
                (t(1, 34), 30.0),
                (t(2, 34), 30.0),
                (t(3, 6), 30.0),
                (t(4, d4), 31.0),
                (t(5, 9), 33.0),
                (t(6, 34), 30.0),
            ])
        };
        assert_eq!(
            run(cap(&sky_a, reads(0)), cap(&sky_b(6), reads(0))),
            vec!["metrics.gps.snr_max.value(values) T4/rec-sample".to_string()]
        );
        // B's slot-4 tick 200 ms before the GSV it missed: no race.
        assert_eq!(
            run(cap(&sky_a, reads(0)), cap(&sky_b(-192), reads(0))),
            vec![format!("metrics.gps.snr_max.value(values) {UNTOLERATED}")]
        );
        let _ = std::fs::remove_file(&tape_path);
    }

    /// review-wp5 R-WP5-4: the diff reports the run's disk source (play.json
    /// `diskSource`); a pre-WP5 run says so.
    #[test]
    fn the_disk_source_is_reported() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let src = |play: &PlayInfo| {
            let mut rep = Report::default();
            diff_media(
                &MediaCapture::default(),
                &MediaCapture::default(),
                play,
                &table,
                &mut rep,
            );
            rep.inputs["disk source (disk_usage)"].clone()
        };
        let p: PlayInfo = serde_json::from_str(r#"{"diskSource":"shim"}"#).unwrap();
        assert_eq!(src(&p), "shim");
        let p: PlayInfo = serde_json::from_str(r#"{"diskSource":"host"}"#).unwrap();
        assert_eq!(src(&p), "host");
        assert_eq!(src(&PlayInfo::default()), "unrecorded (pre-WP5 run)");
        let back: serde_json::Value = serde_json::to_value(PlayInfo {
            disk_source: "shim".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(back["diskSource"], "shim");
    }

    /// review-close F2: a capture child that OUTLIVED its agent (the
    /// observer's `alive_at_stop`: alive itself, or a live member of its own
    /// process group, at the stop) strands its segments: they count as
    /// orphans, and on a Rust side the child itself is UNTOLERATED. Before
    /// the fix such a child was never "gone", so its segment counted 0.
    #[test]
    fn a_capture_child_alive_at_stop_strands_its_segments() {
        let dir =
            std::env::temp_dir().join(format!("xorgate-replay-shm-alive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("shim.ndjson"),
            concat!(
                r#"{"k":"spawn","tool":"gst-launch-1.0","pid":1185,"argv":["videotestsrc","!","shmsink","socket-path=/w/a/home/shm/cam0.sock"],"w":1000}"#,
                "\n",
                r#"{"k":"spawn","tool":"gst-launch-1.0","pid":1190,"argv":["videotestsrc","!","shmsink","socket-path=/w/a/home/shm/cam1.sock"],"w":1000}"#,
                "\n",
            ),
        )
        .unwrap();
        // 1185 exited cleanly and its segment is gone; 1190 outlived the
        // agent (its group leader exited, a member still runs), and kept
        // its segment; 4242 is not ours.
        std::fs::write(
            dir.join("procs.ndjson"),
            concat!(
                "{\"k\":\"gone\",\"pid\":1185,\"w\":3100}\n",
                "{\"k\":\"gone\",\"pid\":1190,\"w\":3200}\n",
                "{\"k\":\"alive_at_stop\",\"pid\":1190,\"w\":9000,\"pgid\":1190,\"members\":[1201]}\n",
                "{\"k\":\"alive_at_stop\",\"pid\":4242,\"w\":9000,\"pgid\":4242,\"members\":[]}\n",
            ),
        )
        .unwrap();
        let shm = [
            serde_json::json!({"w": 9000, "names": ["shmpipe. 1190.    0", "shmpipe. 4242.    0"]}),
        ];
        let mut m = MediaCapture::load(&dir, &shm);
        assert_eq!(
            m.capture_alive_at_stop,
            [1190],
            "only this side's capture children"
        );
        assert_eq!(m.shm_orphan_names, ["shmpipe. 1190.    0"]);
        assert_eq!(m.shm_orphans, 1);
        // A child alive at the stop with no gone record at all counts too.
        std::fs::write(
            dir.join("procs.ndjson"),
            "{\"k\":\"alive_at_stop\",\"pid\":1190,\"w\":9000,\"pgid\":1190,\"members\":[1190]}\n",
        )
        .unwrap();
        let mut js = MediaCapture::load(&dir, &shm);
        assert_eq!(js.shm_orphans, 1);
        assert_eq!(js.shm_orphan_gone, [None], "alive at stop: never seen gone");
        let _ = std::fs::remove_dir_all(&dir);

        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        m.kind = "rust".into();
        m.present = true;
        let clean = MediaCapture {
            kind: "rust".into(),
            present: true,
            ..Default::default()
        };
        let mut rep = Report::default();
        shm_orphans_verdict(&clean, &m, 10_000, &table, &mut rep);
        let rows: Vec<String> = rep
            .mismatches
            .iter()
            .map(|r| format!("{} {} {}", r.path, r.b, r.tol))
            .collect();
        assert!(
            rows.contains(&format!(
                "(capture child of a Rust side alive at stop) [1190] {UNTOLERATED}"
            )),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(
                |r| r.starts_with("(orphan shmpipe segments of a Rust side)")
                    && r.ends_with(UNTOLERATED)
            ),
            "{rows:?}"
        );
        // The mirror: no child alive at stop, no orphan, no row.
        let mut rep = Report::default();
        shm_orphans_verdict(&clean, &clean, 10_000, &table, &mut rep);
        assert!(rep.mismatches.is_empty(), "{:?}", rep.mismatches);

        // review-close-fixes N2 (c) + N3: the same child on a JS side. The
        // Rust-only row never names a JS side; but its segment is a JS
        // orphan with no `gone` instant, which neither D41 arm (reaper miss,
        // hung end-of-run) nor T18 explains, so the orphan row itself is
        // UNTOLERATED in JS-vs-Rust AND in JS-vs-JS: a JS capture child alive
        // at the stop is a rig defect (the player SIGKILLs the JS agent's
        // group before the observer stops), never a tolerated JS orphan.
        js.kind = "js".into();
        js.present = true;
        let js_clean = MediaCapture {
            kind: "js".into(),
            present: true,
            ..Default::default()
        };
        for other in [&clean, &js_clean] {
            let mut rep = Report::default();
            shm_orphans_verdict(&js, other, 10_000, &table, &mut rep);
            let rows: Vec<String> = rep
                .mismatches
                .iter()
                .map(|r| format!("{} {}", r.path, r.tol))
                .collect();
            assert!(
                !rows
                    .iter()
                    .any(|r| r.contains("capture child of a Rust side")),
                "{rows:?}"
            );
            assert!(
                rows.contains(&format!("(orphan shmpipe segments) {UNTOLERATED}")),
                "{} side: {rows:?}",
                other.kind
            );
        }
    }

    /// Phase 5b WP5 item 9: the end-of-run orphan counter reads shmsink's REAL
    /// names (`"shmpipe.%5d.%5d"`, gst shmpipe.c:307 — a 4-digit pid is
    /// space-padded). The pre-fix parse (`" 1185".parse::<u32>()`) failed on
    /// every rig child, so a stranded segment counted 0 and the `shm` row
    /// could never fail (`replay-20261019T181016Z`: 3 present, "A 0, B 0").
    #[test]
    fn shm_orphans_count_padded_names_and_a_strand_fails() {
        let dir = std::env::temp_dir().join(format!("xorgate-replay-shm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("shim.ndjson"),
            concat!(
                r#"{"k":"spawn","tool":"gst-launch-1.0","pid":1185,"argv":["videotestsrc","!","shmsink","socket-path=/w/a/home/shm/cam0.sock"],"w":1000}"#,
                "\n",
                r#"{"k":"spawn","tool":"gst-launch-1.0","pid":1190,"argv":["videotestsrc","!","shmsink","socket-path=/w/a/home/shm/cam1.sock"],"w":1000}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("procs.ndjson"),
            "{\"k\":\"gone\",\"pid\":1185,\"w\":3100}\n",
        )
        .unwrap();
        // 1185 dead + stranded; 1190 still alive (not an orphan); 4242 not ours.
        let shm = [serde_json::json!({"w": 4000, "names": [
            "shmpipe. 1185.    0", "shmpipe. 1190.    0", "shmpipe. 4242.    0"
        ]})];
        let m = MediaCapture::load(&dir, &shm);
        assert_eq!(m.shm_orphans, 1);
        assert_eq!(m.shm_orphan_names, ["shmpipe. 1185.    0"]);
        assert_eq!(m.shm_orphan_gone, [Some(3100)], "the owner's gone instant");
        assert!(!m.agent_hung);
        std::fs::write(
            dir.join("stderr.log"),
            "x\n[rig] agent a did NOT exit in 30 s; SIGKILLed after SIGTERM in 30021 ms (its process group lingered 0 ms)\n",
        )
        .unwrap();
        assert!(
            MediaCapture::load(&dir, &shm).agent_hung,
            "the player's hang line"
        );
        std::fs::write(
            dir.join("stderr.log"),
            "[rig] agent a exited after SIGTERM in 89 ms (its process group lingered 0 ms)\n",
        )
        .unwrap();
        assert!(!MediaCapture::load(&dir, &shm).agent_hung);
        // The old parse never saw it:
        assert!(" 1185.    0"
            .split('.')
            .next()
            .unwrap()
            .parse::<u32>()
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);

        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        // review-wp5 R-WP5-2: D41 split by mechanism. The run's SIGTERM is at
        // END; an orphan's owner died MID (before it) or LATE (after it).
        const END: i64 = 100_000;
        const MID: i64 = 50_000;
        const LATE: i64 = END + 60;
        // (kind, [(name, owner gone w)], agent hung past 30 s?)
        let cap = |kind: &str, orphans: &[(&str, i64)], hung: bool| MediaCapture {
            kind: kind.into(),
            shm_orphans: orphans.len(),
            shm_orphan_names: orphans.iter().map(|(n, _)| n.to_string()).collect(),
            shm_orphan_gone: orphans.iter().map(|(_, g)| Some(*g)).collect(),
            agent_hung: hung,
            present: true,
            ..Default::default()
        };
        let run = |a: MediaCapture, b: MediaCapture| {
            let mut rep = Report::default();
            shm_orphans_verdict(&a, &b, END, &table, &mut rep);
            rep.mismatches
                .into_iter()
                .map(|m| format!("{} {}", m.path, m.tol))
                .collect::<Vec<_>>()
        };
        let pad = "shmpipe. 1185.    0";
        let wide = "shmpipe.12022.    0";
        let d41 = vec!["(orphan shmpipe segments) T7/D41".to_string()];
        let unt = vec![format!("(orphan shmpipe segments) {UNTOLERATED}")];
        let rust_row = format!("(orphan shmpipe segments of a Rust side) {UNTOLERATED}");
        let none = |k: &str| cap(k, &[], false);
        // (a) reaper miss: a padded orphan of a child that died mid-run, Rust 0
        // (either column order; several orphans).
        assert_eq!(run(cap("js", &[(pad, MID)], false), none("rust")), d41);
        assert_eq!(
            run(
                none("rust"),
                cap("js", &[(pad, MID), ("shmpipe.  983.    0", MID)], false)
            ),
            d41
        );
        // (b) end of run: a child still running at the stop of a JS agent that
        // hung past 30 s — any pid width.
        assert_eq!(run(cap("js", &[(wide, LATE)], true), none("rust")), d41);
        assert_eq!(run(cap("js", &[(pad, LATE)], true), none("rust")), d41);
        assert_eq!(
            run(cap("js", &[(pad, MID), (wide, LATE)], true), none("rust")),
            d41,
            "one orphan per arm"
        );
        // NEGATIVE (a): a 5-digit JS orphan from a mid-run death (the TS
        // reaper matches it) — hung agent or not.
        assert_eq!(run(cap("js", &[(wide, MID)], false), none("rust")), unt);
        assert_eq!(run(cap("js", &[(wide, MID)], true), none("rust")), unt);
        assert_eq!(
            run(cap("js", &[(pad, MID), (wide, MID)], false), none("rust")),
            unt
        );
        // NEGATIVE (b): an end-of-run orphan of a capture that had ALREADY ended
        // before the stop is not (b) (a wide one fails; a padded one is (a)).
        assert_eq!(run(cap("js", &[(wide, END - 1)], true), none("rust")), unt);
        // NEGATIVE (b): the JS agent exited gracefully (not hung) — a late orphan
        // is not the bug-3 path, whatever its width.
        assert_eq!(run(cap("js", &[(wide, LATE)], false), none("rust")), unt);
        assert_eq!(run(cap("js", &[(pad, LATE)], false), none("rust")), unt);
        // NEGATIVE: an orphan whose owner was never seen gone.
        let mut g = cap("js", &[(pad, MID)], true);
        g.shm_orphan_gone = vec![None];
        assert_eq!(run(g, none("rust")), unt);
        // NEGATIVE: every Rust orphan shape fails (mid-run / end-of-run, padded /
        // wide, hung or not, equal counts, Rust-vs-Rust, Rust-only).
        for (n, w) in [(pad, MID), (wide, MID), (pad, LATE), (wide, LATE)] {
            for hung in [false, true] {
                assert_eq!(
                    run(
                        cap("js", &[(pad, MID)], false),
                        cap("rust", &[(n, w)], hung)
                    ),
                    vec![rust_row.clone()],
                    "rust orphan {n:?} at {w} hung={hung} (equal counts)"
                );
                assert_eq!(
                    run(cap("rust", &[(n, w)], hung), none("js")),
                    vec![rust_row.clone(), unt[0].clone()],
                    "rust-only orphan {n:?}"
                );
            }
        }
        assert_eq!(
            run(
                cap("rust", &[("shmpipe. 1199.    0", MID)], false),
                cap("rust", &[("shmpipe. 1189.    0", LATE)], true)
            ),
            vec![rust_row.clone(), rust_row.clone()]
        );
        // JS-vs-JS (T18/shm-js-end-of-run, ruling 2026-11-06): a difference
        // made ONLY of arm-(b) orphans on hung sides is the reference's own
        // nondeterminism (rec-preset.cal.rv1: A [1196 (a)], B [1195 (a), 5832 (b)]).
        let t18 = vec!["(orphan shmpipe segments) T18/shm-js-end-of-run".to_string()];
        assert_eq!(
            run(
                cap("js", &[("shmpipe. 1196.    0", MID)], true),
                cap(
                    "js",
                    &[("shmpipe. 1195.    0", MID), ("shmpipe. 5832.    0", LATE)],
                    true
                )
            ),
            t18
        );
        assert_eq!(
            run(cap("js", &[(wide, LATE)], true), none("js")),
            t18,
            "any width"
        );
        assert_eq!(
            run(none("js"), cap("js", &[(pad, LATE), (wide, LATE)], true)),
            t18
        );
        // NEGATIVE: a JS-vs-JS difference that includes an arm-(a) orphan stays
        // exact (one side's reaper miss the other lacks) …
        assert_eq!(run(cap("js", &[(pad, MID)], false), none("js")), unt);
        assert_eq!(
            run(
                cap("js", &[(pad, MID), (wide, LATE)], true),
                cap("js", &[(wide, LATE)], true)
            ),
            unt,
            "the (b) counts match; the extra is an (a)"
        );
        // … and so does one with an UNEXPLAINED orphan: a late orphan of a side
        // that did NOT hang, a 5-digit mid-run one, an owner never seen gone.
        assert_eq!(run(cap("js", &[(wide, LATE)], false), none("js")), unt);
        assert_eq!(
            run(
                cap("js", &[(pad, MID), (wide, MID)], true),
                cap("js", &[(pad, MID)], true)
            ),
            unt
        );
        let mut g = cap("js", &[(pad, LATE)], true);
        g.shm_orphan_gone = vec![None];
        assert_eq!(run(g, none("js")), unt);
        // … and an unexplained orphan on the OTHER side too.
        assert_eq!(
            run(
                cap("js", &[(wide, LATE)], true),
                cap("js", &[(wide, MID), (wide, LATE), (pad, LATE)], true)
            ),
            unt
        );
        assert!(run(
            cap("js", &[(pad, MID)], false),
            cap("js", &[("shmpipe. 1199.    0", MID)], false)
        )
        .is_empty());
        assert!(run(none("js"), none("rust")).is_empty());
    }

    /// A capture SIGINTed at the agent's stop and killed by the agent's exit
    /// before gst printed `handling interrupt.` (gate run 032624Z, 15 busy
    /// loops) is `int` by the agent's own tape record of the SIGINT — only
    /// when that record proves delivery (review-wp6 M5); otherwise `kill`.
    #[test]
    fn capture_end_int_from_the_agents_own_sigint_record() {
        let dir = std::env::temp_dir().join(format!("xorgate-replay-end-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("shim.ndjson"),
            concat!(
                r#"{"k":"spawn","tool":"gst-launch-1.0","pid":9146,"argv":["videotestsrc","!","shmsink","socket-path=/w/a/home/shm/cam1.sock"],"w":1000}"#,
                "\n",
                r#"{"k":"line","pid":9146,"fd":1,"line":"Redistribute latency...","w":2000}"#,
                "\n",
                r#"{"k":"eof","pid":9146,"w":3021}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("procs.ndjson"),
            "{\"k\":\"gone\",\"pid\":9146,\"w\":3100}\n",
        )
        .unwrap();
        let end = |dir: &std::path::Path| {
            MediaCapture::load(dir, &[])
                .spawns
                .iter()
                .find(|s| s.pid == 9146)
                .and_then(|s| s.end.clone())
        };
        assert_eq!(end(&dir).as_deref(), Some("kill"));
        let with_tape = |recs: &[&str]| {
            let mut tape = String::from("{\"v\":1,\"k\":\"meta\"}\n");
            for r in recs {
                tape.push_str(r);
                tape.push('\n');
            }
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut gz, tape.as_bytes()).unwrap();
            std::fs::write(dir.join("agent-tape.ndjson.gz"), gz.finish().unwrap()).unwrap();
            end(&dir)
        };
        let sent = |w: i64, sig: i32, errno: &str| {
            format!(
                r#"{{"t":67618,"w":{w},"k":"child_signal","role":"capture","key":"cam1","pid":9146,"sig":{sig}{errno}}}"#
            )
        };
        // SIGINT really sent (taped after kill(2), errno 0), before the
        // child's death, never reaped by the agent → `int`.
        assert_eq!(
            with_tape(&[&sent(3000, 2, r#","errno":0"#)]).as_deref(),
            Some("int")
        );
        // (1) Not proven delivered: kill(2) failed (ESRCH 3), a record
        // written BEFORE the syscall (no errno), or another signal (9).
        assert_eq!(
            with_tape(&[&sent(3000, 2, r#","errno":3"#)]).as_deref(),
            Some("kill")
        );
        assert_eq!(with_tape(&[&sent(3000, 2, "")]).as_deref(), Some("kill"));
        assert_eq!(
            with_tape(&[&sent(3000, 9, r#","errno":0"#)]).as_deref(),
            Some("kill")
        );
        // (2) Stamped after the child died (its eof at 3021).
        assert_eq!(
            with_tape(&[&sent(3022, 2, r#","errno":0"#)]).as_deref(),
            Some("kill")
        );
        // (3) The agent reaped the child itself (a death while it lived).
        let exit = r#"{"t":67620,"w":3010,"k":"child_exit","role":"capture","key":"cam1","pid":9146,"code":null,"signal":9,"external":false}"#;
        assert_eq!(
            with_tape(&[&sent(3000, 2, r#","errno":0"#), exit]).as_deref(),
            Some("kill")
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// T8/d30-js-late-shed: the replay-20261019T190757Z shape (JS restore
    /// one shed period late after reusing its recorder's dirty reading) is
    /// tolerated ONLY JS-vs-Rust with JS later, Δ in the band and the stale
    /// evidence; swapped sides, Δ 1300, no evidence, js/js, rust/rust fail.
    #[test]
    fn d30_js_late_shed_only_js_later_with_evidence() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let sid = "<sid:video:cam0#1>";
        // `late` = this side's transitions/spawns at the JS instants of the
        // run; the other side `d` ms earlier. `rails` = (ts, V) samples.
        let side = |kind: &str, shift: i64, rails: &[(i64, f64)]| {
            let mut seg = String::from("{\"type\":\"header\",\"v\":1}\n");
            for (ts, v) in rails {
                seg.push_str(&format!(
                    "{{\"ts\":{ts},\"metrics\":{{\"system\":{{\"rail_5v\":{{\"value\":{v}}},\"throttled\":{{\"value\":0}}}}}}}}\n"
                ));
            }
            MediaCapture {
                kind: kind.into(),
                shed_events: vec![
                    (
                        130_607 - shift,
                        "info: power shed restore → no-live (clean for 20s)".into(),
                    ),
                    (
                        150_679 - shift,
                        "info: power shed restore → full (clean for 20s)".into(),
                    ),
                ],
                spawns: vec![Spawn {
                    w: 130_628 - shift,
                    role: "capture".into(),
                    key: "cam0".into(),
                    pid: 9,
                    argv: vec![],
                    env: BTreeMap::new(),
                    playing: None,
                    end: Some("int".into()),
                    end_w: None,
                    sid: Some(sid.into()),
                }],
                finals: BTreeMap::from([
                    (
                        "recordings/telemetry/<sid:telemetry#0>/seg-1-00000.jsonl.gz".to_string(),
                        seg.into_bytes(),
                    ),
                    (
                        // The fragment that STARTS at the restore.
                        format!("recordings/video/cam0/{sid}/00000.json"),
                        format!("{{\"startTs\":{}}}", 130_700 - shift).into_bytes(),
                    ),
                ]),
                present: true,
                ..Default::default()
            }
        };
        let stale = [
            (108_710, 5.1),
            (109_715, 5.1),
            (110_732, 5.3),
            (111_729, 5.3),
        ];
        let fresh = [
            (108_710, 5.1),
            (109_715, 5.3),
            (110_732, 5.3),
            (111_729, 5.3),
        ];
        let changes = [110_073];
        // Rows as diff.rs writes them: column A = side `a`. `d` = how much
        // EARLIER side b's instants are than side a's.
        let run = |a: MediaCapture, b: MediaCapture, d: i64| {
            let mut rep = Report::default();
            for (at, path, x, y) in [
                (
                    "capture/cam0#0",
                    "(spawn ms after start)",
                    130_628,
                    130_628 - d,
                ),
                (
                    "kvs/xorgate-{id}-cam0#1",
                    "(spawn ms after start)",
                    150_695,
                    150_695 - d,
                ),
                (&format!("{sid}/00006")[..], "durationMs", 7_423, 7_423 + d),
                (&format!("{sid}/00000")[..], "startTs", 130_700, 130_700 - d),
            ] {
                let st = if path == "durationMs" || path == "startTs" {
                    "sidecar"
                } else {
                    "children"
                };
                rep.push(mm(st, at, path, x.to_string(), y.to_string(), UNTOLERATED));
            }
            d30_js_late_shed(&a, &b, 0, &changes, &table, &mut rep);
            rep.mismatches
                .into_iter()
                .map(|m| m.tol)
                .collect::<Vec<_>>()
        };
        let ok = vec!["T8/d30-js-late-shed".to_string(); 4];
        let unt = vec![UNTOLERATED.to_string(); 4];
        // JS (later by 1013) vs Rust: tolerated, in either column order.
        assert_eq!(
            run(side("js", 0, &stale), side("rust", 1_013, &fresh), 1_013),
            ok
        );
        assert_eq!(
            run(side("rust", 1_013, &fresh), side("js", 0, &stale), -1_013),
            ok
        );
        // Sides swapped: the RUST side is the later one.
        assert_eq!(
            run(side("rust", 0, &stale), side("js", 1_013, &fresh), 1_013),
            unt
        );
        // Δ outside one shed period.
        assert_eq!(
            run(side("js", 0, &stale), side("rust", 1_300, &fresh), 1_300),
            unt
        );
        // No stale-reuse evidence: the JS recorder already carried 5.30.
        assert_eq!(
            run(side("js", 0, &fresh), side("rust", 1_013, &fresh), 1_013),
            unt
        );
        // js/js (re-check extension): EITHER side may be the later one, if
        // THAT side carries the stale-reuse evidence.
        assert_eq!(
            run(side("js", 0, &stale), side("js", 1_013, &fresh), 1_013),
            ok
        );
        assert_eq!(
            run(side("js", 1_013, &fresh), side("js", 0, &stale), -1_013),
            ok
        );
        // js/js without evidence on the later side: fails.
        assert_eq!(
            run(side("js", 0, &fresh), side("js", 1_013, &stale), 1_013),
            unt
        );
        assert_eq!(
            run(side("js", 0, &fresh), side("js", 1_013, &fresh), 1_013),
            unt
        );
        // Anything with Rust the later side, and rust/rust: never.
        assert_eq!(
            run(side("rust", 0, &stale), side("js", 1_013, &stale), 1_013),
            unt
        );
        assert_eq!(
            run(side("rust", 0, &stale), side("rust", 1_013, &fresh), 1_013),
            unt
        );
    }

    /// T8/d30-js-late-shed, the fragment a late JS shed CUT (rec-shed-uv
    /// stepC .jsrs / .jsrs.r2: cam1#0/00003 JS 933 / 860 vs Rust 0 / 1, the
    /// fragment start 208 ms apart, the END instants exactly Δ apart):
    /// matched by its end instant only when it is the last seq of its session
    /// on both sides and the end shift is the transition's Δ ±150.
    #[test]
    fn d30_js_late_shed_cut_fragment_by_its_end_instant_only() {
        let table = Table::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let sid = "<sid:video:cam1#0>";
        // frags: (seq, startTs, durationMs) — the session's sidecars.
        let side = |kind: &str, shift: i64, rails: &[(i64, f64)], frags: &[(u64, i64, i64)]| {
            let mut seg = String::from("{\"type\":\"header\",\"v\":1}\n");
            for (ts, v) in rails {
                seg.push_str(&format!(
                    "{{\"ts\":{ts},\"metrics\":{{\"system\":{{\"rail_5v\":{{\"value\":{v}}},\"throttled\":{{\"value\":0}}}}}}}}\n"
                ));
            }
            let mut finals = BTreeMap::from([(
                "recordings/telemetry/<sid:telemetry#0>/seg-1-00000.jsonl.gz".to_string(),
                seg.into_bytes(),
            )]);
            for (seq, st, dur) in frags {
                finals.insert(
                    format!("recordings/video/cam1/{sid}/{seq:05}.json"),
                    format!("{{\"seq\":{seq},\"startTs\":{st},\"durationMs\":{dur}}}").into_bytes(),
                );
            }
            MediaCapture {
                kind: kind.into(),
                shed_events: vec![
                    (
                        130_607 - shift,
                        "info: power shed restore → no-live (clean for 20s)".into(),
                    ),
                    (
                        150_679 - shift,
                        "info: power shed restore → full (clean for 20s)".into(),
                    ),
                ],
                finals,
                present: true,
                ..Default::default()
            }
        };
        let stale = [
            (108_710, 5.1),
            (109_715, 5.1),
            (110_732, 5.3),
            (111_729, 5.3),
        ];
        let fresh = [
            (108_710, 5.1),
            (109_715, 5.3),
            (110_732, 5.3),
            (111_729, 5.3),
        ];
        let changes = [110_073];
        // JS: #3 starts 129 740, 860 ms → ends 130 600 (7 ms before its
        // transition). Rust (Δ = 1013 earlier): #3 starts 207 ms earlier and
        // ends at 130 600 − Δ, so its duration is 54 ms — 806 apart, not Δ.
        let js_frags = [(2u64, 125_740, 4_000), (3, 129_740, 860)];
        // `start` = the Rust #3's start (its duration follows from the end).
        let rs_at = |start: i64, end_shift: i64| {
            [
                (2u64, 125_533, start - 125_533),
                (3, start, 130_600 - end_shift - start),
            ]
        };
        let rs = |end_shift: i64| rs_at(129_533, end_shift);
        let run = |a: MediaCapture, b: MediaCapture, x: i64, y: i64| {
            let mut rep = Report::default();
            rep.push(mm(
                "sidecar",
                &format!("{sid}/00003"),
                "durationMs",
                x.to_string(),
                y.to_string(),
                UNTOLERATED,
            ));
            d30_js_late_shed(&a, &b, 0, &changes, &table, &mut rep);
            rep.mismatches[0].tol.clone()
        };
        let ok = "T8/d30-js-late-shed".to_string();
        // The rec-shed-uv shape: tolerated, either column order.
        let r = rs(1_013);
        assert_eq!(
            run(
                side("js", 0, &stale, &js_frags),
                side("rust", 1_013, &fresh, &r),
                860,
                r[1].2
            ),
            ok
        );
        assert_eq!(
            run(
                side("rust", 1_013, &fresh, &r),
                side("js", 0, &stale, &js_frags),
                r[1].2,
                860
            ),
            ok
        );
        // End shift 300 ms past Δ (the duration difference, 373, not Δ
        // either): UNTOLERATED.
        let r = rs_at(128_800, 1_313);
        assert_eq!(
            run(
                side("js", 0, &stale, &js_frags),
                side("rust", 1_013, &fresh, &r),
                860,
                r[1].2
            ),
            UNTOLERATED
        );
        // Not the last fragment on the Rust side (a #4 follows): UNTOLERATED.
        let r = rs(1_013);
        let mut r4 = r.to_vec();
        r4.push((4, 129_600, 2_000));
        assert_eq!(
            run(
                side("js", 0, &stale, &js_frags),
                side("rust", 1_013, &fresh, &r4),
                860,
                r[1].2
            ),
            UNTOLERATED
        );
        // Rust the later side, or no stale-reuse evidence on the JS: UNTOLERATED.
        assert_eq!(
            run(
                side("rust", 0, &stale, &js_frags),
                side("js", 1_013, &fresh, &r),
                860,
                r[1].2
            ),
            UNTOLERATED
        );
        assert_eq!(
            run(
                side("js", 0, &fresh, &js_frags),
                side("rust", 1_013, &fresh, &r),
                860,
                r[1].2
            ),
            UNTOLERATED
        );
    }

    /// The S3 order check compares sessions by their ordinal: `#10` after
    /// `#7` is oldest-first; `#7` after `#10` is a violation.
    #[test]
    fn s3_put_order_compares_session_ordinals_not_strings() {
        assert!(session_ordinal("<sid:video:cam1#7>") < session_ordinal("<sid:video:cam1#10>"));
        assert!(session_ordinal("<sid:video:cam1#10>") > session_ordinal("<sid:video:cam1#9>"));
        assert_eq!(session_ordinal("sess-1"), ("sess-1".to_string(), 0));
        let put = |key: &str| S3Obs {
            key: key.into(),
            ctype: "application/json".into(),
            status: 200,
            sig: "ok".into(),
        };
        let side = |keys: &[&str]| MediaCapture {
            kind: "js".into(),
            present: true,
            s3: keys.iter().map(|k| put(k)).collect(),
            ..Default::default()
        };
        let order_rows = |keys: &[&str]| {
            let mut rep = Report::default();
            let t = Table::load(
                &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tools/replay/tolerances.toml"),
            )
            .unwrap();
            diff_media(&side(keys), &side(keys), &PlayInfo::default(), &t, &mut rep);
            rep.mismatches
                .into_iter()
                .filter(|m| m.path == "(PUT order)")
                .count()
        };
        let k = |n: u32| format!("media-sessions/v1/{{id}}/cam1/<sid:video:cam1#{n}>/00000.json");
        assert_eq!(order_rows(&[&k(7), &k(10)]), 0);
        assert_eq!(order_rows(&[&k(10), &k(7)]), 2);
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

    /// R-P3G-2: a sample's read instant is its fire on the 1 s grid (the
    /// lower envelope), not its post-await `ts`.
    /// gps presence oracle: a sample carries `metrics.gps` exactly when the
    /// tape advanced the gps cache since the previous read (±50 ms).
    /// A one-sample LED-state value is tolerated only when BOTH agents'
    /// live telemetry carried it within a second of the recorded sample.
    #[test]
    fn led_state_both_held_only_with_both_live_ticks() {
        let p = "metrics.system.health_flags.value";
        let only: Vec<Step> = vec![("304".into(), 1, 2_000)];
        let a = vec![(1_150, "304".to_string()), (2_150, "288".to_string())];
        let b = vec![(1_210, "304".to_string()), (2_210, "288".to_string())];
        assert!(led_state_both_held(p, &only, &a, &b));
        let b_never = vec![(1_210, "288".to_string()), (2_210, "288".to_string())];
        assert!(
            !led_state_both_held(p, &only, &a, &b_never),
            "the other agent never held it"
        );
        let b_old = vec![(500, "304".to_string()), (2_210, "288".to_string())];
        assert!(
            !led_state_both_held(p, &only, &a, &b_old),
            "held > 1 s before"
        );
        let two: Vec<Step> = vec![("304".into(), 2, 2_000)];
        assert!(!led_state_both_held(p, &two, &a, &b), "two samples");
        assert!(!led_state_both_held(
            "metrics.system.led_state.value",
            &only,
            &a,
            &b
        ));
    }

    #[test]
    fn carry_consistent_follows_the_tape_advances_only() {
        let s = |ts: i64, gps: bool| {
            let mut m = BTreeMap::new();
            m.insert(
                "metrics.system.cpu_temp.value".to_string(),
                "50".to_string(),
            );
            if gps {
                m.insert("metrics.gps.fix_quality.value".to_string(), "1".to_string());
            }
            (ts, m)
        };
        // Reads at 1000, 2000, 3000, 4000; advances at 1500 and 3500.
        let adv = [1_500, 3_500];
        let ok: RecSamples = vec![
            s(1_000, true),
            s(2_000, true),
            s(3_000, false),
            s(4_000, true),
        ];
        assert!(carry_consistent(&ok, &adv, "gps"));
        // A sample carrying gps with no advance since the previous read: no.
        let extra: RecSamples = vec![
            s(1_000, true),
            s(2_000, true),
            s(3_000, true),
            s(4_000, true),
        ];
        assert!(!carry_consistent(&extra, &adv, "gps"));
        // A sample MISSING gps after an advance: no.
        let missing: RecSamples = vec![
            s(1_000, true),
            s(2_000, false),
            s(3_000, false),
            s(4_000, true),
        ];
        assert!(!carry_consistent(&missing, &adv, "gps"));
        // An advance 20 ms before a read: either is accepted.
        let edge = [1_980, 3_500];
        let a: RecSamples = vec![
            s(1_000, true),
            s(2_000, false),
            s(3_000, true),
            s(4_000, true),
        ];
        assert!(carry_consistent(&a, &edge, "gps"));
    }

    /// Review-wp4 R4-10: the oracle window is ORACLE_LAG_MS back from the
    /// newest release by the read: a 1 s-stale input never verifies.
    #[test]
    fn oracle_span_rejects_a_one_second_stale_input() {
        // The single-read span is ORACLE_LAG_MS (the window tape's measured
        // delivery lag); a 1 s-stale read is caught by the PAIR check in
        // `oracle_filter` (test imu_oracle_verifies_only_genuine_recent_reads).
        let ws: Vec<i64> = (0..=80).map(|k| k * 62).collect();
        let (s, e) = oracle_span(&ws, 4_000);
        assert!(ws[s] >= ws[e - 1] - ORACLE_LAG_MS);
        assert!(s == 0 || ws[s - 1] < ws[e - 1] - ORACLE_LAG_MS);
        assert!(ws[e - 1] <= 4_050);
        assert_eq!(oracle_span(&ws, -100), (0, 0));
    }

    #[test]
    fn read_instants_are_the_lower_envelope_of_the_grid() {
        // A 1 s grid from 1 000 with per-tick sampling latencies 0,150,5,300,10,3,2.
        let l: RecSamples = [1_000, 2_150, 3_005, 4_300, 5_010, 6_003, 7_002]
            .iter()
            .map(|t| (*t, BTreeMap::new()))
            .collect();
        let r = read_instants(&l);
        for (k, x) in r.iter().enumerate() {
            let grid = 1_000 + k as i64 * 1_000;
            assert!((x - grid).abs() <= 10, "sample {k}: {x} vs {grid}");
        }
    }

    /// R-P3G-2: a differing pair is removed only when its reads straddle
    /// ≥ 2 input changes, or exactly one as a strict old/new split.
    #[test]
    fn straddle_filter_drops_only_pairs_that_read_different_inputs() {
        let s = |ts: i64, v: &str| {
            (
                ts,
                BTreeMap::from([("metrics.imu.quat_i.value".to_string(), v.to_string())]),
            )
        };
        let p = "metrics.imu.quat_i.value";
        // High-rate input: 16 Hz lines (62 ms apart) between every pair.
        let imu: Vec<i64> = (0..80).map(|k| 900 + k * 62).collect();
        let la = vec![s(1_000, "0.1"), s(2_000, "0.2"), s(3_000, "0.3")];
        let lb = vec![s(1_250, "0.11"), s(2_250, "0.21"), s(3_250, "0.31")];
        // NEGATIVE: timing alone never removes a pair with ≥ 2 changes between
        // (the IMU goes through the tape oracle, `imu_oracle`).
        let (fa, fb, n) = straddle_filter(&la, &lb, p, &imu, 50);
        assert_eq!((n, fa.len(), fb.len()), (0, 3, 3));
        // One change between: a strict old/new split is removed …
        let la = vec![s(1_000, "1"), s(2_000, "1"), s(3_000, "2"), s(4_000, "2")];
        let lb = vec![s(1_250, "1"), s(2_250, "2"), s(3_250, "2"), s(4_250, "2")];
        let (_, _, n) = straddle_filter(&la, &lb, p, &[2_100], 50);
        assert_eq!(n, 1);
        // NEGATIVE: … a stale value (B skips "3": its previous is not A's) is kept.
        let la = vec![s(1_000, "1"), s(2_000, "2"), s(3_000, "3"), s(4_000, "4")];
        let lb = vec![s(1_250, "1"), s(2_250, "2"), s(3_250, "2"), s(4_250, "4")];
        let (_, fb, n) = straddle_filter(&la, &lb, p, &[1_100, 2_100, 3_100], 50);
        assert_eq!(n, 0);
        assert_eq!(fb.len(), 4);
        // NEGATIVE: no change between the reads → kept.
        let la = vec![s(1_000, "1"), s(2_000, "5")];
        let lb = vec![s(1_250, "1"), s(2_250, "6")];
        let (_, _, n) = straddle_filter(&la, &lb, p, &[2_600], 50);
        assert_eq!(n, 0);
        // NEGATIVE: samples too far apart are not a pair.
        let (_, _, n) = straddle_filter(
            &[s(1_000, "1")].to_vec(),
            &[s(1_700, "2")].to_vec(),
            p,
            &imu,
            50,
        );
        assert_eq!(n, 0);
        // An NMEA epoch (8 lines 8 ms apart) is ONE change.
        let epoch: Vec<i64> = (0..8).map(|k| 2_100 + k * 8).collect();
        let la = vec![s(1_000, "1"), s(2_000, "1"), s(3_000, "2")];
        let lb = vec![s(1_250, "1"), s(2_250, "2"), s(3_250, "2")];
        let (_, _, n) = straddle_filter(&la, &lb, p, &epoch, 50);
        assert_eq!(n, 1);
    }

    /// R-P3G-2 IMU oracle: a recorded sample is verified only when its TEXT
    /// equals `map_latest` of a line released before its read; a value no
    /// line gave, a line from long before, or `1.0` for `1` are not.
    #[test]
    fn imu_oracle_verifies_only_genuine_recent_reads() {
        use xorgate_core::imu::{ImuSample, LinAccel};
        use xorgate_core::orientation::Quat;
        let line = |z: f64| ImuSample {
            quat: Quat {
                r: 1.0,
                i: 0.0,
                j: 0.0,
                k: 0.0,
            },
            linaccel: LinAccel { x: 0.0, y: 0.0, z },
        };
        let leaves = |smp: &ImuSample, z_text: Option<&str>| {
            let g = serde_json::to_value(xorgate_core::imu_group::map_latest(smp, None)).unwrap();
            let mut o = BTreeMap::new();
            leaf_texts(&g, "metrics.imu", &mut o);
            o.retain(|k, _| k.ends_with(".value"));
            if let Some(t) = z_text {
                o.insert("metrics.imu.accel_z.value".into(), t.into());
            }
            o
        };
        // 16 Hz lines from 0; recorder reads at 1 000, 2 000, 3 000, 4 000.
        let lines: Vec<(i64, ImuSample)> =
            (0..80).map(|k| (k * 62, line(k as f64 / 1000.0))).collect();
        let near = |t: i64| lines.iter().rev().find(|(w, _)| *w <= t).unwrap().1;
        let l: RecSamples = vec![
            (1_000, leaves(&near(1_000), None)),        // genuine
            (2_000, leaves(&near(2_000), Some("0.5"))), // a value no line gave
            (3_000, leaves(&lines[2].1, None)),         // a line from ~3 s before
            (4_000, leaves(&line(1.0), None)),          // z = 1 → "1" … ok only if released
        ];
        let ok = imu_oracle(&l, &lines, None);
        assert!(ok.contains_key(&0));
        assert!(!ok.contains_key(&1), "a value the tape never released");
        assert!(!ok.contains_key(&2), "older than the recent-line window");
        assert!(!ok.contains_key(&3));
        // Format: the same value written `1.0` (not JS) fails, `1` passes.
        let lines1: Vec<(i64, ImuSample)> = vec![(1_000, line(1.0))];
        let l1: RecSamples = vec![(1_010, leaves(&line(1.0), None))];
        assert!(imu_oracle(&l1, &lines1, None).contains_key(&0));
        let l2: RecSamples = vec![(1_010, leaves(&line(1.0), Some("1.0")))];
        assert!(
            !imu_oracle(&l2, &lines1, None).contains_key(&0),
            "1.0 is not the JS text of 1"
        );
        // R4-10: a recorder reading 1 s-stale lines lags ~1 s behind the
        // newest line; a fresh one at ANY phase of the grid lags 0.
        let ws: Vec<i64> = lines.iter().map(|(w, _)| *w).collect();
        let fresh: RecSamples = (0..4)
            .map(|k| (1_000 + k * 1000, leaves(&near(1_000 + k * 1000), None)))
            .collect();
        let late_phase: RecSamples = (0..4)
            .map(|k| (1_040 + k * 1000, leaves(&near(1_040 + k * 1000), None)))
            .collect();
        let stale: RecSamples = (0..4)
            .map(|k| (1_010 + k * 1000, leaves(&near(10 + k * 1000), None)))
            .collect();
        let (vf, vp, vs) = (
            imu_oracle(&fresh, &lines, None),
            imu_oracle(&late_phase, &lines, None),
            imu_oracle(&stale, &lines, None),
        );
        assert_eq!(vs.len(), 4, "each stale read is still a genuine (old) line");
        let (_, bad) = oracle_lag_check(&fresh, &stale, &vf, &vs, &ws).unwrap();
        assert!(bad, "a 1 s-stale side FAILS the median-lag check");
        let (_, bad) = oracle_lag_check(&stale, &fresh, &vs, &vf, &ws).unwrap();
        assert!(bad, "either side");
        let (_, ok) = oracle_lag_check(&fresh, &late_phase, &vf, &vp, &ws).unwrap();
        assert!(!ok, "two fresh recorders on different phases pass");
    }
}
