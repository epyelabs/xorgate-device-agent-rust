//! Phase 3 rig additions to the player (phase-3-recording-media.md
//! §Record/replay): the media/recording side of each agent's sandbox and the
//! observers that capture what it does.
//!
//! Inputs (per agent, set up by [`prepare_agent`]):
//! - `kvs.json`: the tape's channels + (when the tape uses the cloud upload
//!   path or KVS) a COMPLETE manifest naming the rig's fake AWS
//!   ([`crate::aws`] constants) — never the tape's real endpoints;
//! - `KVS_RUNTIME_BIN` → `<work>/kvs/xorgate-kvs-webrtc` (the fake master,
//!   `tools/replay/media/`) when the tape has channels, else a missing path
//!   (Phase 2 behaviour: media self-gates);
//! - `gst-launch-1.0` on PATH = the shim (`tools/replay/shims/`), which execs
//!   the real binary with `videotestsrc` for `libcamerasrc`;
//! - `rpicam-hello` on PATH when the tape carries its `exec` answer;
//! - `NODE_EXTRA_CA_CERTS` = the rig CA (Node; the Rust agent reads the
//!   system store the player updated, P5);
//! - optional filler tree (`meta.rig.filler`) seeded into the home (D9).
//!
//! Outputs (per agent `<run>/<label>/`):
//! - `shim.ndjson` — the shims' own log: every gst/kvs spawn (argv verbatim,
//!   kvs env contract keys), gst stdout/stderr lines, kvs signals/exits;
//! - `procs.ndjson` — the player's view of those pids: `{w, pid, k: "gone"}`
//!   when one disappears, `{k: "external", …}` when the player killed one at a
//!   tape `child_exit` with `external: true`;
//! - `recordings.json` — every file seen under `<home>/recordings` (1 s poll):
//!   `{rel: {first, last, size, gone}}`, so evicted / uploaded-and-deleted
//!   files are visible; `final/recordings/**` — the end-of-run tree (json and
//!   telemetry bodies copied; mp4 as `{size, sha256}` in `recordings.json`);
//! - `s3.ndjson`, `s3/**` — the fake S3's log and stored bodies ([`crate::aws`]);
//! - `<run>/shm.ndjson` — `/dev/shm/shmpipe.*` every 5 s (shared by both).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use xorgate_hal::tape::{wall_now_ms, ChildExitRec, Tape, TapeEvent, ROLE_CAPTURE, ROLE_KVS};

use crate::aws;

/// What the tape asks of the media side.
#[derive(Debug, Clone, Default)]
pub struct MediaPlan {
    /// `(streamKey, channelName)` of the tape's manifest channels.
    pub channels: Vec<(String, String)>,
    /// Write a complete manifest (upload identity → the rig's fake AWS).
    pub full_manifest: bool,
    /// The tape answers `rpicam-hello --list-cameras`.
    pub rpicam: bool,
    /// `meta.rig.filler`.
    pub filler: Option<Value>,
    /// Bucket names a tape config names (`uploadBucket`), for /etc/hosts.
    pub buckets: Vec<String>,
}

impl MediaPlan {
    pub fn of(tape: &Tape) -> Self {
        let channels: Vec<(String, String)> = tape
            .meta
            .kvs
            .channels
            .iter()
            .filter_map(|c| {
                Some((
                    c.get("streamKey")?.as_str()?.to_string(),
                    c.get("channelName")?.as_str()?.to_string(),
                ))
            })
            .collect();
        let mut buckets = BTreeSet::new();
        buckets.insert(aws::RIG_BUCKET.to_string());
        let mut has_media_records = false;
        let mut rpicam = false;
        for r in &tape.records {
            match &r.ev {
                TapeEvent::MqttIn { topic, .. } if topic.ends_with("/config") => {
                    if let Some(b) =
                        r.ev.mqtt_payload()
                            .and_then(|p| serde_json::from_slice::<Value>(&p).ok())
                            .and_then(|v| v.get("uploadBucket")?.as_str().map(str::to_string))
                    {
                        if is_bucket_name(&b) {
                            buckets.insert(b);
                        }
                    }
                }
                TapeEvent::Exec { argv, .. }
                    if argv.first().map(String::as_str) == Some("rpicam-hello") =>
                {
                    rpicam = true
                }
                TapeEvent::ChildSpawn(_) | TapeEvent::S3Put(_) | TapeEvent::FsOut { .. } => {
                    has_media_records = true
                }
                _ => {}
            }
        }
        Self {
            full_manifest: tape.meta.kvs.upload || !channels.is_empty() || has_media_records,
            channels,
            rpicam,
            filler: tape
                .meta
                .rig
                .as_ref()
                .and_then(|r| r.get("filler"))
                .cloned(),
            buckets: buckets.into_iter().collect(),
        }
    }

    /// The channelName of a stream key.
    pub fn channel_of(&self, key: &str) -> Option<&str> {
        self.channels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, c)| c.as_str())
    }
}

/// S3 bucket name charset (keeps /etc/hosts lines well-formed).
fn is_bucket_name(b: &str) -> bool {
    (3..=63).contains(&b.len())
        && b.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
}

/// The agent's `kvs.json`: Phase 2's `{"channels": …}` stub, or (Phase 3) the
/// complete manifest pointing at the rig fakes, `deviceId` = the rig id.
pub fn kvs_manifest(plan: &MediaPlan, tape: &Tape, device_id: &str) -> Value {
    let channels: Vec<Value> = tape
        .meta
        .kvs
        .channels
        .iter()
        .map(|c| {
            serde_json::from_str(&c.to_string().replace(&tape.meta.device_id, device_id))
                .unwrap_or_else(|_| c.clone())
        })
        .collect();
    if !plan.full_manifest {
        return json!({ "channels": channels });
    }
    json!({
        "deviceId": device_id,
        "region": aws::RIG_REGION,
        "credentialEndpoint": aws::RIG_CREDS_HOST,
        "roleAlias": aws::RIG_ROLE_ALIAS,
        "channels": channels,
        "uploadBucket": aws::RIG_BUCKET,
    })
}

/// Media env + files for one agent; returns the env entries to add/replace.
pub fn prepare_agent(
    plan: &MediaPlan,
    work: &Path,
    home: &Path,
    device_id: &str,
    certs: &Path,
    media_dir: &Path,
) -> Result<Vec<(String, String)>> {
    let mut env = vec![(
        "NODE_EXTRA_CA_CERTS".to_string(),
        certs.join("ca.pem").display().to_string(),
    )];
    if !plan.channels.is_empty() {
        let kvs = work.join("kvs");
        fs::create_dir_all(kvs.join("lib"))?;
        fs::create_dir_all(work.join("state/kvs"))?;
        let bin = kvs.join("xorgate-kvs-webrtc");
        fs::copy(media_dir.join("xorgate-kvs-webrtc"), &bin)
            .with_context(|| format!("{}", media_dir.display()))?;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
        // The KVS CA bundle the agent passes as AWS_KVS_CACERT_PATH (default
        // `dirname(bin)/cert.pem`, config.ts:270): any file will do.
        let _ = fs::copy(certs.join("ca.pem"), kvs.join("cert.pem"));
        env.push(("KVS_RUNTIME_BIN".into(), bin.display().to_string()));
        env.push((
            "KVS_RUNTIME_LIB_DIR".into(),
            kvs.join("lib").display().to_string(),
        ));
    }
    if let Some(f) = &plan.filler {
        seed_filler(home, device_id, f)?;
    }
    Ok(env)
}

// ------------------------------------------------------------------ filler

/// A uuidv7 string for `ms` with a fixed random part (deterministic).
pub fn uuidv7_at(ms: i64, tail: u64) -> String {
    let ms = ms as u64 & 0xFFFF_FFFF_FFFF;
    let a = ms >> 16;
    let b = ms & 0xFFFF;
    let c = 0x7000 | (tail & 0x0FFF);
    let d = 0x8000 | ((tail >> 12) & 0x3FFF);
    let e = (tail >> 26) & 0xFFFF_FFFF_FFFF;
    format!("{a:08x}-{b:04x}-{c:04x}-{d:04x}-{e:012x}")
}

/// D9's filler: `{mib, fragmentMib, ageDays, key, trimLastBytes?}` → one
/// closed old session `recordings/video/<key>/<uuidv7 ageDays ago>/` with
/// `session.json`, SPARSE `NNNNN.mp4` fragments (apparent size counts, like
/// `stat().size`) and a finalized sidecar each, mtimes `ageDays` in the past.
/// `trimLastBytes` shortens the NEWEST fragment (evicted last) so a scenario
/// can place its eviction threshold away from a knife edge. Same tree in
/// both homes (deviceId substituted).
pub fn seed_filler(home: &Path, device_id: &str, f: &Value) -> Result<()> {
    let mib = f.get("mib").and_then(Value::as_u64).unwrap_or(600);
    let frag_mib = f
        .get("fragmentMib")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .max(1);
    let age_days = f.get("ageDays").and_then(Value::as_i64).unwrap_or(7);
    let trim_last = f.get("trimLastBytes").and_then(Value::as_u64).unwrap_or(0);
    let key = f.get("key").and_then(Value::as_str).unwrap_or("cam0");
    let start = wall_now_ms() - age_days * 86_400_000;
    let start = start - start.rem_euclid(1000);
    let sid = uuidv7_at(start, 0x5eed_f111_e700);
    let dir = home.join("recordings/video").join(key).join(&sid);
    fs::create_dir_all(&dir)?;
    let seg_s = 60i64;
    let session = json!({
        "v": 1, "deviceId": device_id, "streamKey": key, "sessionId": sid,
        "bootId": "00000000000000000000000000f111e7", "timeSource": "ntp",
        "preset": "720p15-1mbps", "width": 1280, "height": 720, "fps": 15,
        "bitrateKbps": 1000, "segmentSeconds": seg_s, "anchorTs": start,
        "anchorMono": "1000000000",
    });
    fs::write(dir.join("session.json"), format!("{session}\n"))?;
    let n = mib.div_ceil(frag_mib);
    let mtime =
        std::time::SystemTime::UNIX_EPOCH + Duration::from_millis((start + 3_600_000) as u64);
    for seq in 0..n {
        let mp4 = dir.join(format!("{seq:05}.mp4"));
        let full = frag_mib * 1024 * 1024;
        let size = if seq + 1 == n {
            full.saturating_sub(trim_last)
        } else {
            full
        };
        let file = fs::File::create(&mp4)?;
        file.set_len(size)?;
        file.set_modified(mtime)?;
        let side = json!({
            "v": 1, "deviceId": device_id, "streamKey": key, "sessionId": sid,
            "seq": seq, "startTs": start + seq as i64 * seg_s * 1000,
            "startMono": 1_000_000 + seq as i64 * seg_s * 1000,
            "durationMs": seg_s * 1000, "sizeBytes": size, "timeSource": "ntp",
            "codec": "h264", "width": 1280, "height": 720, "fps": 15,
            "bitrateKbps": 1000, "anchor": "fstat", "finalized": true,
        });
        let sp = dir.join(format!("{seq:05}.json"));
        fs::write(&sp, side.to_string())?;
        fs::File::options()
            .write(true)
            .open(&sp)?
            .set_modified(mtime)?;
    }
    fs::File::options()
        .write(true)
        .open(dir.join("session.json"))?
        .set_modified(mtime)?;
    Ok(())
}

// --------------------------------------------------------------- observers

/// One agent as the observers see it.
#[derive(Debug, Clone)]
pub struct ObsAgent {
    pub work: PathBuf,
    pub home: PathBuf,
    pub dir: PathBuf,
}

/// A child the shims logged.
#[derive(Debug, Clone)]
pub struct ShimChild {
    pub pid: u32,
    pub tool: String,
    pub argv: Vec<String>,
}

/// Every spawn record of `<work>/shim.ndjson`.
pub fn shim_children(work: &Path) -> Vec<ShimChild> {
    let Ok(f) = fs::File::open(work.join("shim.ndjson")) else {
        return Vec::new();
    };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v["k"] == "spawn")
        .filter_map(|v| {
            Some(ShimChild {
                pid: v["pid"].as_u64()? as u32,
                tool: v["tool"].as_str()?.to_string(),
                argv: v["argv"]
                    .as_array()?
                    .iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect(),
            })
        })
        .collect()
}

/// The stream key a capture argv belongs to: its shmsink socket
/// `<home>/shm/<key>.sock`.
pub fn capture_key(argv: &[String]) -> Option<String> {
    argv.iter().find_map(|a| {
        let p = a.strip_prefix("socket-path=")?;
        let f = Path::new(p).file_name()?.to_str()?;
        f.strip_suffix(".sock").map(str::to_string)
    })
}

fn alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s
            .rsplit_once(") ")
            .map(|(_, rest)| !rest.starts_with('Z') && !rest.starts_with('X'))
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn append(path: &Path, v: &Value) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{v}");
    }
}

/// Phase 5b WP4 (orchestrator ruling 2026-10-23): the tape's external child
/// exits re-enacted PER SIDE and PER ATTEMPT. The recording device's N-th
/// spawn of a `(role, key)` child that exited externally (a camera dying
/// 110 ms in, a `kill -9`) becomes "kill this side's own N-th spawn of that
/// child, `offset_ms` after THAT side's spawn instant" — never "kill whatever
/// is alive at the tape's wall instant", which forked the two agents' retry
/// ladders whenever they were a few ms out of phase (one side in its 2 s retry
/// gap at the instant, the other not: window tape cam1, `.jsrs09`). A side
/// whose ladder genuinely differs keeps differing: the plan is keyed by
/// attempt ordinal only and never re-aligns attempts by time.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptExit {
    /// Tape ms from the attempt's spawn to its exit.
    pub offset_ms: i64,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// `(role, key)` → attempt index (0-based, the tape's spawn order) → exit.
pub type ExitPlan = BTreeMap<(String, String), BTreeMap<usize, AttemptExit>>;

/// The per-attempt plan of a tape's external exits, plus the exits it could
/// not attribute to a recorded spawn (`(t, rec)`, re-enacted at tape time).
pub fn exit_plan(tape: &Tape) -> (ExitPlan, Vec<u64>) {
    let mut ord: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut by_pid: BTreeMap<u32, ((String, String), usize, u64)> = BTreeMap::new();
    let mut plan = ExitPlan::new();
    let mut unattributed = Vec::new();
    for r in &tape.records {
        match &r.ev {
            TapeEvent::ChildSpawn(sp) => {
                let k = (sp.role.clone(), sp.key.clone());
                let n = ord.entry(k.clone()).or_default();
                if sp.pid != 0 {
                    by_pid.insert(sp.pid, (k, *n, r.t));
                }
                *n += 1;
            }
            TapeEvent::ChildExit(x) if x.external => match by_pid.get(&x.pid) {
                Some((k, n, t0)) if *k == (x.role.clone(), x.key.clone()) => {
                    plan.entry(k.clone()).or_default().insert(
                        *n,
                        AttemptExit {
                            offset_ms: r.t.saturating_sub(*t0) as i64,
                            code: x.code,
                            signal: x.signal,
                        },
                    );
                }
                _ => unattributed.push(r.t),
            },
            _ => {}
        }
    }
    (plan, unattributed)
}

/// One spawn the shims logged, with its wall instant and (KVS) channel.
#[derive(Debug, Clone, PartialEq)]
pub struct ShimSpawn {
    pub w: i64,
    pub pid: u32,
    pub tool: String,
    pub argv: Vec<String>,
    pub channel: Option<String>,
}

/// `(role, key)` of a shim spawn: a capture by its shmsink socket, a KVS
/// master by its OWN channel (`<deviceId>-<key>`, never the tape's name).
pub fn spawn_role_key(s: &ShimSpawn, plan: &MediaPlan) -> Option<(String, String)> {
    if s.tool == "gst-launch-1.0" {
        return capture_key(&s.argv).map(|k| (ROLE_CAPTURE.to_string(), k));
    }
    if s.tool == "xorgate-kvs-webrtc" {
        let ch = s.channel.as_deref()?;
        return plan
            .channels
            .iter()
            .map(|(k, _)| k)
            .find(|k| ch.ends_with(&format!("-{k}")))
            .map(|k| (ROLE_KVS.to_string(), k.clone()));
    }
    None
}

/// A scheduled per-attempt exit on one side.
#[derive(Debug, Clone, PartialEq)]
pub struct DueExit {
    pub due_w: i64,
    pub role: String,
    pub key: String,
    /// 1-based attempt number on THIS side.
    pub attempt: usize,
    pub pid: u32,
    pub channel: Option<String>,
    pub exit: AttemptExit,
}

/// Pure scheduling: each new spawn gets its own attempt number per
/// `(role, key)`; one with a planned exit is due at its OWN spawn instant +
/// the tape's offset.
pub fn schedule_exits(
    counts: &mut BTreeMap<(String, String), usize>,
    spawns: &[ShimSpawn],
    xp: &ExitPlan,
    plan: &MediaPlan,
) -> Vec<DueExit> {
    let mut out = Vec::new();
    for s in spawns {
        let Some(rk) = spawn_role_key(s, plan) else {
            continue;
        };
        let n = counts.entry(rk.clone()).or_default();
        let idx = *n;
        *n += 1;
        if let Some(x) = xp.get(&rk).and_then(|m| m.get(&idx)) {
            out.push(DueExit {
                due_w: s.w + x.offset_ms,
                role: rk.0.clone(),
                key: rk.1.clone(),
                attempt: idx + 1,
                pid: s.pid,
                channel: s.channel.clone(),
                exit: x.clone(),
            });
        }
    }
    out
}

/// One side's runner: tails `<work>/shim.ndjson` for new spawns, keeps their
/// due exits, delivers each to THAT pid when due (records in `procs.ndjson`).
#[derive(Debug, Default)]
pub struct ExitRunner {
    offset: u64,
    partial: String,
    counts: BTreeMap<(String, String), usize>,
    pending: Vec<DueExit>,
}

impl ExitRunner {
    pub fn poll(&mut self, a: &ObsAgent, plan: &MediaPlan, xp: &ExitPlan, now: i64) -> Vec<Value> {
        let spawns = self.read_new(&a.work);
        let due = schedule_exits(&mut self.counts, &spawns, xp, plan);
        self.pending.extend(due);
        let (ready, keep): (Vec<DueExit>, Vec<DueExit>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|d| d.due_w <= now);
        self.pending = keep;
        ready.iter().map(|d| deliver_exit(a, d, now)).collect()
    }

    fn read_new(&mut self, work: &Path) -> Vec<ShimSpawn> {
        let Ok(mut f) = fs::File::open(work.join("shim.ndjson")) else {
            return Vec::new();
        };
        use std::io::{Seek, SeekFrom};
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return Vec::new();
        }
        self.offset += buf.len() as u64;
        let text = std::mem::take(&mut self.partial) + &buf;
        let (whole, rest) = match text.rfind('\n') {
            Some(i) => (text[..=i].to_string(), text[i + 1..].to_string()),
            None => (String::new(), text),
        };
        self.partial = rest;
        whole
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["k"] == "spawn")
            .filter_map(|v| {
                Some(ShimSpawn {
                    w: v["w"].as_i64()?,
                    pid: v["pid"].as_u64()? as u32,
                    tool: v["tool"].as_str()?.to_string(),
                    argv: v["argv"]
                        .as_array()?
                        .iter()
                        .filter_map(|a| a.as_str().map(str::to_string))
                        .collect(),
                    channel: v["channel"].as_str().map(str::to_string),
                })
            })
            .collect()
    }
}

/// Deliver one per-attempt exit to its own pid: a capture is signalled (the
/// tape's signal, SIGKILL for a code exit — gst cannot exit with a chosen
/// code); a KVS master exits with the tape's code through ITS channel's
/// control file (or takes the tape's signal). A pid already gone = that
/// attempt ended first on this side (`ok: false`, recorded).
fn deliver_exit(a: &ObsAgent, d: &DueExit, now: i64) -> Value {
    let live = alive(d.pid);
    let sig = |n: i32| {
        nix::sys::signal::Signal::try_from(n).is_ok_and(|s| {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(d.pid as i32), s).is_ok()
        })
    };
    let ok = live
        && if d.role == ROLE_KVS && d.exit.signal.is_none() {
            d.channel.as_ref().is_some_and(|ch| {
                fs::write(
                    a.work.join("state/kvs").join(format!("{ch}.exit")),
                    d.exit.code.unwrap_or(1).to_string(),
                )
                .is_ok()
            })
        } else {
            sig(d.exit.signal.unwrap_or(9))
        };
    let rec = json!({"w": now, "k": "external", "role": d.role, "key": d.key, "pid": d.pid,
        "attempt": d.attempt, "due": d.due_w, "code": d.exit.code, "sig": d.exit.signal,
        "channel": d.channel, "ok": ok});
    append(&a.dir.join("procs.ndjson"), &rec);
    rec
}

/// Re-enact a tape `child_exit` with `external: true` on one agent: a KVS
/// master exits with the tape's code (control file the fake polls); a capture
/// child is killed with the tape's signal (SIGKILL when it exited with a code
/// — gst cannot be made to exit with a chosen code; both agents get the same).
pub fn external_exit(a: &ObsAgent, plan: &MediaPlan, r: &ChildExitRec) -> Value {
    let w = wall_now_ms();
    let rec = if r.role == ROLE_KVS {
        let ch = plan.channel_of(&r.key).unwrap_or(&r.key).to_string();
        let ctl = a.work.join("state/kvs").join(format!("{ch}.exit"));
        let code = r.code.unwrap_or(1);
        let res = fs::write(&ctl, code.to_string());
        json!({"w": w, "k": "external", "role": ROLE_KVS, "key": r.key, "channel": ch, "code": code, "ok": res.is_ok()})
    } else if r.role == ROLE_CAPTURE {
        let pid = shim_children(&a.work)
            .into_iter()
            .rev()
            .find(|c| {
                c.tool == "gst-launch-1.0"
                    && capture_key(&c.argv).as_deref() == Some(r.key.as_str())
                    && alive(c.pid)
            })
            .map(|c| c.pid);
        let sig = r.signal.unwrap_or(9);
        let ok = pid.is_some_and(|p| {
            nix::sys::signal::Signal::try_from(sig).is_ok_and(|s| {
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(p as i32), s).is_ok()
            })
        });
        json!({"w": w, "k": "external", "role": ROLE_CAPTURE, "key": r.key, "pid": pid, "sig": sig, "ok": ok})
    } else {
        json!({"w": w, "k": "external", "role": r.role, "key": r.key, "ok": false, "why": "unknown role"})
    };
    append(&a.dir.join("procs.ndjson"), &rec);
    rec
}

#[derive(Debug, Clone, Default)]
struct Seen {
    first: i64,
    last: i64,
    size: u64,
    gone: Option<i64>,
}

fn walk(root: &Path, base: &Path, out: &mut BTreeMap<String, u64>) {
    let Ok(rd) = fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, base, out),
            Ok(t) if t.is_file() => {
                if let (Ok(rel), Ok(md)) = (p.strip_prefix(base), e.metadata()) {
                    out.insert(rel.display().to_string(), md.len());
                }
            }
            _ => {}
        }
    }
}

/// The observers: pid liveness of the shims' children (100 ms), the
/// recordings tree (1 s), `/dev/shm/shmpipe.*` (5 s). Returns when `stop` is
/// cancelled, after writing `recordings.json` per agent.
pub async fn observe(agents: Vec<ObsAgent>, run: PathBuf, stop: CancellationToken) {
    let mut known: Vec<BTreeMap<u32, bool>> = vec![BTreeMap::new(); agents.len()];
    let mut seen: Vec<BTreeMap<String, Seen>> = vec![BTreeMap::new(); agents.len()];
    let mut tick = 0u64;
    let mut last_shm: Option<Vec<String>> = None;
    loop {
        let now = wall_now_ms();
        for (i, a) in agents.iter().enumerate() {
            for c in shim_children(&a.work) {
                known[i].entry(c.pid).or_insert(true);
            }
            for (pid, live) in known[i].iter_mut() {
                if *live && !alive(*pid) {
                    *live = false;
                    append(
                        &a.dir.join("procs.ndjson"),
                        &json!({"w": now, "pid": pid, "k": "gone"}),
                    );
                }
            }
            if tick.is_multiple_of(10) {
                let mut cur = BTreeMap::new();
                let root = a.home.join("recordings");
                walk(&root, &root, &mut cur);
                for (rel, size) in &cur {
                    let s = seen[i].entry(rel.clone()).or_insert(Seen {
                        first: now,
                        ..Default::default()
                    });
                    s.last = now;
                    s.size = *size;
                    s.gone = None;
                }
                for (rel, s) in seen[i].iter_mut() {
                    if s.gone.is_none() && !cur.contains_key(rel) {
                        s.gone = Some(now);
                    }
                }
            }
        }
        if tick.is_multiple_of(50) {
            let mut names: Vec<String> = fs::read_dir("/dev/shm")
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with("shmpipe."))
                .collect();
            names.sort();
            if last_shm.as_ref() != Some(&names) {
                append(&run.join("shm.ndjson"), &json!({"w": now, "names": names}));
                last_shm = Some(names);
            }
        }
        tick += 1;
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    for (i, a) in agents.iter().enumerate() {
        let files: Map<String, Value> = seen[i]
            .iter()
            .map(|(rel, s)| {
                (
                    rel.clone(),
                    json!({"first": s.first, "last": s.last, "size": s.size, "gone": s.gone}),
                )
            })
            .collect();
        let _ = fs::write(
            a.dir.join("recordings.json"),
            serde_json::to_vec_pretty(&json!({"files": files})).unwrap_or_default(),
        );
    }
}

fn sha256_file(p: &Path) -> Option<String> {
    let mut f = fs::File::open(p).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(hex::encode(h.finalize()))
}

/// End of run (agents stopped): copy the shim log, the final recordings tree
/// (json/jsonl/gz bodies; mp4 as size + sha256 in `final/recordings.mp4.json`)
/// and `counters.json`.
pub fn finish(a: &ObsAgent) -> Result<()> {
    // The gst shim's tee logs a child's last lines (`handling interrupt.`,
    // `Got EOS`) and then `eof` once that child's pipes close; on a loaded
    // host it can lag the agent's own exit (CI run 39290924951: both lines
    // missing → the child read as "kill"). Wait (≤ 5 s) until every gst spawn
    // has its `eof` before copying the log.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let text = fs::read_to_string(a.work.join("shim.ndjson")).unwrap_or_default();
        let (mut spawned, mut eof) = (BTreeSet::new(), BTreeSet::new());
        for v in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            match (v["k"].as_str(), v["tool"].as_str()) {
                (Some("spawn"), Some("gst-launch-1.0")) => {
                    spawned.insert(v["pid"].as_u64());
                }
                (Some("eof"), _) => {
                    eof.insert(v["pid"].as_u64());
                }
                _ => {}
            }
        }
        if spawned.is_subset(&eof) || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if let Ok(b) = fs::read(a.work.join("shim.ndjson")) {
        fs::write(a.dir.join("shim.ndjson"), b)?;
    }
    let root = a.home.join("recordings");
    let mut cur = BTreeMap::new();
    walk(&root, &root, &mut cur);
    let mut mp4 = Map::new();
    for (rel, size) in &cur {
        let src = root.join(rel);
        if rel.ends_with(".mp4") {
            // Filler fragments are sparse GiBs: hash only real captures.
            let sha = if *size <= 64 * 1024 * 1024 {
                sha256_file(&src)
            } else {
                None
            };
            mp4.insert(rel.clone(), json!({"size": size, "sha256": sha}));
            continue;
        }
        let dst = a.dir.join("final/recordings").join(rel);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        let _ = fs::copy(&src, &dst);
    }
    fs::create_dir_all(a.dir.join("final"))?;
    fs::write(
        a.dir.join("final/recordings.mp4.json"),
        serde_json::to_vec_pretty(&Value::Object(mp4))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-attempt external exits (orchestrator ruling 2026-10-23): the
    /// tape's N-th attempt's exit lands on each side's OWN N-th attempt at its
    /// own spawn + the tape offset, whatever the two sides' phase; a side
    /// whose ladder genuinely differs is NOT re-aligned (by ordinal only), so
    /// the diff still sees the difference.
    #[test]
    fn exits_are_per_side_per_attempt_and_never_realign_a_different_ladder() {
        use xorgate_hal::tape::{ChildSpawnRec, TapeMeta, TapeRecord};
        let rec = |t: u64, ev| TapeRecord { t, w: t as i64, ev };
        let spawn = |key: &str, role: &str, pid| {
            TapeEvent::ChildSpawn(ChildSpawnRec {
                role: role.into(),
                key: key.into(),
                argv: vec![],
                env: BTreeMap::new(),
                pid,
                nice: None,
            })
        };
        let exit = |key: &str, role: &str, pid, code, signal, external| {
            TapeEvent::ChildExit(ChildExitRec {
                role: role.into(),
                key: key.into(),
                pid,
                code,
                signal,
                external,
            })
        };
        let tape = Tape {
            meta: TapeMeta::new("d", "b", 0),
            records: vec![
                rec(1_000, spawn("cam1", ROLE_CAPTURE, 11)),
                rec(1_110, exit("cam1", ROLE_CAPTURE, 11, Some(1), None, true)),
                rec(3_100, spawn("cam1", ROLE_CAPTURE, 12)), // attempt 2: agent-stopped
                rec(9_000, exit("cam1", ROLE_CAPTURE, 12, None, Some(2), false)),
                rec(9_500, spawn("cam1", ROLE_CAPTURE, 13)),
                rec(9_740, exit("cam1", ROLE_CAPTURE, 13, None, Some(9), true)),
                rec(2_000, spawn("cam1", ROLE_KVS, 21)),
                rec(2_500, exit("cam1", ROLE_KVS, 21, Some(1), None, true)),
                rec(9_900, exit("cam0", ROLE_CAPTURE, 99, None, Some(9), true)), // no spawn
            ],
            truncated: false,
        };
        let (xp, unattributed) = exit_plan(&tape);
        assert_eq!(unattributed, vec![9_900]);
        let cap = &xp[&(ROLE_CAPTURE.to_string(), "cam1".to_string())];
        assert_eq!(cap.keys().copied().collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(
            cap[&0],
            AttemptExit {
                offset_ms: 110,
                code: Some(1),
                signal: None
            }
        );
        assert_eq!(cap[&2].offset_ms, 240);
        let plan = MediaPlan {
            channels: vec![("cam1".into(), "tape-chan-cam1".into())],
            ..Default::default()
        };
        let gst = |w, pid| ShimSpawn {
            w,
            pid,
            tool: "gst-launch-1.0".into(),
            argv: vec!["socket-path=/h/shm/cam1.sock".into()],
            channel: None,
        };
        let kvs = |w, pid| ShimSpawn {
            w,
            pid,
            tool: "xorgate-kvs-webrtc".into(),
            argv: vec![],
            channel: Some("00000000-0000-4000-8000-00000000000a-cam1".into()),
        };
        let due = |spawns: &[ShimSpawn]| {
            let mut counts = BTreeMap::new();
            schedule_exits(&mut counts, spawns, &xp, &plan)
                .into_iter()
                .map(|d| (d.role, d.attempt, d.pid, d.due_w, d.channel))
                .collect::<Vec<_>>()
        };
        // Side A and side B 7 ms apart: each side's attempts die at ITS spawn + offset.
        let a = due(&[
            gst(50_000, 1),
            kvs(50_900, 5),
            gst(52_000, 2),
            gst(58_500, 3),
        ]);
        let b = due(&[
            gst(50_007, 101),
            kvs(50_905, 105),
            gst(52_009, 102),
            gst(58_511, 103),
        ]);
        assert_eq!(
            a,
            vec![
                ("capture".into(), 1, 1, 50_110, None),
                (
                    "kvs".into(),
                    1,
                    5,
                    51_400,
                    Some("00000000-0000-4000-8000-00000000000a-cam1".into())
                ),
                ("capture".into(), 3, 3, 58_740, None),
            ]
        );
        assert_eq!(
            b.iter().map(|x| x.3).collect::<Vec<_>>(),
            vec![50_117, 51_405, 58_751]
        );
        // The KVS exit names the side's OWN channel, never the tape's.
        assert!(a.iter().all(|x| x.4.as_deref() != Some("tape-chan-cam1")));
        // NEGATIVE: a side with an EXTRA early attempt is not re-aligned: its
        // attempt 3 is the extra one's successor, so the kills shift with it
        // and the ladder difference survives (the diff's children/events rows).
        let c = due(&[
            gst(50_000, 1),
            gst(50_500, 9),
            gst(52_000, 2),
            gst(58_500, 3),
        ]);
        assert_eq!(
            c.iter().map(|x| (x.1, x.2)).collect::<Vec<_>>(),
            vec![(1, 1), (3, 2)],
            "attempt ordinals, never time matching"
        );
        // Incremental: attempts counted across polls.
        let mut counts = BTreeMap::new();
        assert_eq!(
            schedule_exits(&mut counts, &[gst(1, 1), gst(2, 2)], &xp, &plan).len(),
            1
        );
        let third = schedule_exits(&mut counts, &[gst(3, 3)], &xp, &plan);
        assert_eq!(third[0].attempt, 3);
    }

    #[test]
    fn uuidv7_shape_and_capture_key() {
        let u = uuidv7_at(1_785_353_400_000, 0x5eed_f111_e700);
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "7");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(&u[..8], &format!("{:08x}", 1_785_353_400_000u64 >> 16));
        assert_eq!(
            capture_key(&[
                "!".into(),
                "shmsink".into(),
                "socket-path=/tmp/rig-work/a/home/shm/cam1.sock".into()
            ]),
            Some("cam1".into())
        );
        assert!(is_bucket_name("xorgate-synth-uploads"));
        assert!(!is_bucket_name("a b"));
    }

    #[test]
    fn filler_is_sparse_and_old() {
        let home = std::env::temp_dir().join(format!("rig-filler-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        seed_filler(
            &home,
            "dev",
            &json!({"mib": 30, "fragmentMib": 10, "ageDays": 7}),
        )
        .unwrap();
        let mut cur = BTreeMap::new();
        let root = home.join("recordings");
        walk(&root, &root, &mut cur);
        let mp4: Vec<_> = cur.iter().filter(|(k, _)| k.ends_with(".mp4")).collect();
        assert_eq!(mp4.len(), 3);
        assert!(mp4.iter().all(|(_, s)| **s == 10 * 1024 * 1024));
        assert_eq!(cur.iter().filter(|(k, _)| k.ends_with(".json")).count(), 4);
        // `trimLastBytes` shortens only the newest fragment (+ its sidecar).
        let home2 = std::env::temp_dir().join(format!("rig-filler-trim-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home2);
        seed_filler(
            &home2,
            "dev",
            &json!({"mib": 30, "fragmentMib": 10, "ageDays": 7, "trimLastBytes": 40960}),
        )
        .unwrap();
        let mut cur2 = BTreeMap::new();
        let root2 = home2.join("recordings");
        walk(&root2, &root2, &mut cur2);
        let sizes: Vec<u64> = cur2
            .iter()
            .filter(|(k, _)| k.ends_with(".mp4"))
            .map(|(_, s)| *s)
            .collect();
        assert_eq!(sizes, vec![10 << 20, 10 << 20, (10 << 20) - 40960]);
        let side2 = cur2
            .keys()
            .find(|k| k.ends_with("00002.json"))
            .unwrap()
            .clone();
        let side2: Value =
            serde_json::from_str(&fs::read_to_string(root2.join(&side2)).unwrap()).unwrap();
        assert_eq!(side2["sizeBytes"], (10u64 << 20) - 40960);
        let _ = fs::remove_dir_all(&home2);
        let md = fs::metadata(
            home.join("recordings/video/cam0")
                .read_dir()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path()
                .join("00000.mp4"),
        )
        .unwrap();
        let age = std::time::SystemTime::now()
            .duration_since(md.modified().unwrap())
            .unwrap();
        assert!(age > Duration::from_secs(6 * 86_400));
        fs::remove_dir_all(&home).ok();
    }
}
