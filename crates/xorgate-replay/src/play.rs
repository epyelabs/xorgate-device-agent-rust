//! `xorgate-replay play` — the rig player (runs INSIDE the rig container).
//!
//! Starts two agents (JS `node dist/worker.js` and/or `xorgate-rs run`) with
//! distinct deviceIds against one TLS mosquitto, feeds both the SAME tape inputs
//! through the OS-boundary knobs the TS already reads (phase file §Record/replay
//! "Black-box"), and captures everything they publish and write.
//!
//! Per agent (`<out>/<label>/`):
//! - `GPS_NMEA_PORT` → a pty in raw mode (no echo, no CR/NL translation; the
//!   player keeps the slave open so a reopen never sees a hangup); NMEA is
//!   written only once both agents tick and while BOTH hold their port open
//!   (the TS opens it on its first tick, at its own phase), so both see the
//!   same sentences at a moment neither is busy booting;
//! - `LTE_TELEMETRY_FILE` → a file the player rewrites atomically (or removes);
//! - `XORGATE_PYTHON_DIR` → the fake `bno085_read.py`, which relays the lines the
//!   player sends on the agent's unix socket and exits on `__EXIT__ <code>`;
//! - PATH shims (`vcgencmd`, `i2cdetect`, `timedatectl`, `sudo`, `rpicam-hello`)
//!   answering from `RIG_STATE` (per-argv files the player writes) and logging
//!   sudo argv to `sudo.log`;
//! - `IOT_DATA_ENDPOINT=broker` resolves to this container, where a TCP proxy
//!   forwards to the real mosquitto: a tape `conn closed` span is reproduced by
//!   dropping every proxied connection and refusing new ones until the span ends
//!   (the phase file says "docker pause of the broker"; the proxy does the same
//!   thing to the agents without freezing the capture client or needing the
//!   docker socket in the rig — see the README);
//! - KVS_MANIFEST `{"channels":[]}`, HARDWARE_MANIFEST from the tape meta,
//!   KVS_PIPELINE_DIR / XORGATE_HOME scratch, `TZ=UTC`, `MODE` from the tape
//!   meta `env.MODE` (default `real`).
//!
//! Input timing — the hold gate. Both agents tick at 1 Hz with their OWN phase
//! (and Node's `setInterval` drifts ~1 ms/tick). An input landing between the
//! two agents' ticks would be seen by one tick and not by its partner, which
//! would make every stream-fed field (gps, imu, lte) differ by one sample. The
//! player therefore never changes an input between the two ticks of a pair: it
//! predicts each agent's next tick from the `ts` of its last telemetry publish
//! and HOLDS releases from `lead tick − guard` until the partner's tick has
//! been observed. Inputs are delayed by at most one pair gap (< 1 s), their
//! order is preserved, and each pair is written to `pairs.ndjson` so the diff
//! compares exactly the ticks that saw the same inputs. When the drifting gap
//! exceeds the flip gap (`--flip-gap-ms`, 500 ms = the nearest alignment; was
//! 800) the roles flip and the one lead tick in between is recorded as a
//! `skip` (it has no partner that saw the same inputs).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use xorgate_hal::tape::{wall_now_ms, ConnState, Tape, TapeEvent};

use crate::diff::PlayInfo;
use crate::rewrite;
use crate::watch::{self, WatchGate};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AgentKind {
    Js,
    Rust,
}

impl AgentKind {
    fn as_str(self) -> &'static str {
        match self {
            AgentKind::Js => "js",
            AgentKind::Rust => "rust",
        }
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct PlayArgs {
    #[arg(long)]
    pub tape: PathBuf,
    #[arg(long)]
    pub out: PathBuf,
    /// Container-local scratch for homes, ptys, unix sockets and shims (bind
    /// mounts from macOS cannot hold sockets).
    #[arg(long, default_value = "/tmp/rig-work")]
    pub work: PathBuf,
    #[arg(long, value_enum, default_value = "js")]
    pub a: AgentKind,
    #[arg(long, value_enum, default_value = "rust")]
    pub b: AgentKind,
    /// The JS build (`dist/worker.js`, `dist/cli.js`, `node_modules/`).
    #[arg(long, default_value = "/rig/cache/js-89b5c64")]
    pub js_dir: PathBuf,
    #[arg(long, default_value = "/rig/cache/bin/xorgate-rs")]
    pub rs_bin: PathBuf,
    #[arg(long, default_value = "node")]
    pub node: String,
    /// The real broker (capture client + proxy upstream).
    #[arg(long, default_value = "mosquitto")]
    pub broker_host: String,
    #[arg(long, default_value_t = 8883)]
    pub broker_port: u16,
    /// Where the agents connect (`broker` → 127.0.0.1 in the rig).
    #[arg(long, default_value = "0.0.0.0:8883")]
    pub proxy_listen: String,
    #[arg(long, default_value = "broker")]
    pub endpoint: String,
    #[arg(long, default_value = "/rig/certs")]
    pub certs: PathBuf,
    #[arg(long, default_value = "/rig/tools/shims")]
    pub shims: PathBuf,
    #[arg(long, default_value = "/rig/tools/fake-python")]
    pub fake_python: PathBuf,
    /// Real time only: other speeds are NOT parity evidence (timers differ).
    #[arg(long, default_value_t = 1.0)]
    pub speed: f64,
    /// Do not shift embedded wall times (rewrite.rs).
    #[arg(long)]
    pub no_shift: bool,
    /// Keep running this long after the last tape event.
    #[arg(long, default_value_t = 20)]
    pub settle_s: u64,
    /// Stop the tape after this many seconds.
    #[arg(long)]
    pub max_s: Option<u64>,
    /// Hold inputs from this long before the lead agent's predicted tick.
    /// 150 ms (was 60): a line released just before the hold must be HANDLED
    /// by Node before its read; in a loaded VM its event loop / threadpool fs
    /// read took > 60 ms (fix_quality one sample apart on a 4 ms pair).
    #[arg(long, default_value_t = 150)]
    pub guard_ms: i64,
    /// Disable the hold gate (debugging only; not parity evidence).
    #[arg(long)]
    pub no_gate: bool,
    /// Put the `rpicam-hello` shim ("No cameras available!") on PATH; by
    /// default the tool is ABSENT (phase file).
    #[arg(long)]
    pub with_rpicam: bool,
    /// Start agent B this many ms after A (calibration: forces the two tick
    /// phases apart so the hold gate is exercised like JS vs Rust).
    #[arg(long, default_value_t = 0)]
    pub stagger_ms: u64,
    /// Pair gap above which the hold gate flips the alignment (skips one lead
    /// tick). 500 = always the nearest alignment (series 5: at 800 a 772 ms
    /// pair outlived a 5 s sky_state dwell between its two reads).
    #[arg(long, default_value_t = DEFAULT_FLIP_GAP_MS)]
    pub flip_gap_ms: i64,
}

pub const DEVICE_A: &str = "00000000-0000-4000-8000-00000000000a";
pub const DEVICE_B: &str = "00000000-0000-4000-8000-00000000000b";
const INTERVAL_MS: i64 = 1000;
/// Default `--flip-gap-ms`: half the tick interval, the nearest alignment.
pub const DEFAULT_FLIP_GAP_MS: i64 = 500;
/// A hold whose pair was not observed in full ends after this long.
const HOLD_TIMEOUT_MS: i64 = 2000;
const STALE_MS: i64 = 2500;
/// `NMEA_STALE_MS` (`gps.ts:15`): a gps group older than this is dropped.
const NMEA_STALE_MS: i64 = 10_000;
/// `REOPEN_SILENT_MS` (`gps.ts`): silence after which an agent reopens its port.
const REOPEN_SILENT_MS: i64 = 30_000;
/// Cut mode: how long after the lag agent's predicted tick its read is
/// assumed done (the JS `ts` is stamped after `await source.read()`).
const CUT_READ_MS: i64 = 150;
const PREFILL_MS: u64 = 1000;

// ------------------------------------------------------------------ gate

/// What the player knows about one agent's 1 Hz tick.
///
/// The TS stamps a payload AFTER `await source.read()` (`worker.ts:1192-1194`),
/// so `ts = tick + read latency`, and the latency varies (two `vcgencmd` runs,
/// file reads, a loaded VM). The tick itself is estimated from the LOWER
/// envelope of `ts − k·interval` over the last few ticks (latency ≥ 0); `k` is
/// the tick index, advanced by the rounded gap between consecutive payloads
/// (queued payloads flushed after a reconnect arrive in order).
#[derive(Debug, Clone, Default)]
struct TickTrack {
    k: i64,
    last_ts: Option<i64>,
    bases: std::collections::VecDeque<i64>,
}

impl TickTrack {
    const WINDOW: usize = 8;

    /// Record a payload `ts`; returns its tick index.
    fn note(&mut self, ts: i64) -> i64 {
        if let Some(prev) = self.last_ts {
            let steps = ((ts - prev) as f64 / INTERVAL_MS as f64).round() as i64;
            self.k += steps.max(1);
        }
        self.last_ts = Some(ts);
        self.bases.push_back(ts - self.k * INTERVAL_MS);
        if self.bases.len() > Self::WINDOW {
            self.bases.pop_front();
        }
        self.k
    }

    /// Estimated tick time of tick index `k`.
    fn tick_at(&self, k: i64) -> Option<i64> {
        self.bases.iter().min().map(|b| b + k * INTERVAL_MS)
    }
}

#[derive(Debug, Clone)]
struct Hold {
    start: i64,
    /// Tick indices of the pair.
    k: [i64; 2],
    ts: [Option<i64>; 2],
}

/// The hold gate (module doc). Indices 0 = A, 1 = B.
#[derive(Debug)]
pub struct Gate {
    track: [TickTrack; 2],
    hold: Option<Hold>,
    open_after: i64,
    /// A tick index that was given up at a phase flip, per agent.
    skip_k: [Option<i64>; 2],
    /// The tick index of each agent's last paired tick.
    paired_k: [Option<i64>; 2],
    guard_before: i64,
    guard_after: i64,
    /// Pair gap above which the alignment flips (`--flip-gap-ms`).
    flip_gap: i64,
    /// Arrival (`now`) of each agent's last telemetry publish.
    seen_at: [Option<i64>; 2],
    /// Set when a cut ends; cleared once BOTH agents have published again.
    /// Until then the gate stays on PREDICTED ticks (series 5: Rust, still in
    /// its backoff 7 s after the restore, read an NMEA burst 5 ms in — GGA
    /// new, RMC old — because an unobserved agent left the gate open).
    restored_at: Option<i64>,
    pub pairs: Vec<Value>,
}

impl Gate {
    pub fn new(guard_ms: i64) -> Self {
        Self {
            track: [TickTrack::default(), TickTrack::default()],
            hold: None,
            open_after: 0,
            skip_k: [None, None],
            paired_k: [None, None],
            guard_before: guard_ms,
            guard_after: 20,
            flip_gap: DEFAULT_FLIP_GAP_MS,
            seen_at: [None, None],
            restored_at: None,
            pairs: Vec::new(),
        }
    }

    /// The broker path was restored at `now`: keep holding on predicted ticks
    /// until both agents' telemetry arrives again (each reconnects on its own
    /// cadence: mqtt.js 2 s, Rust 1→60 s backoff).
    pub fn on_restore(&mut self, now: i64) {
        self.restored_at = Some(now);
    }

    /// Is an agent still unobserved since the last restore?
    fn reconnecting(&mut self) -> bool {
        let Some(r) = self.restored_at else {
            return false;
        };
        if self.seen_at.iter().all(|s| s.is_some_and(|s| s >= r)) {
            self.restored_at = None;
            return false;
        }
        true
    }

    /// The rig's `--flip-gap-ms`.
    pub fn with_flip_gap(mut self, ms: i64) -> Self {
        self.flip_gap = ms;
        self
    }

    /// Have both agents published telemetry at least once (booted,
    /// connected, ticking)?
    pub fn both_ticking(&self) -> bool {
        self.track.iter().all(|t| t.last_ts.is_some())
    }

    /// A telemetry publish of agent `idx` with payload `ts` arrived at `now`.
    pub fn on_tick(&mut self, idx: usize, ts: i64, now: i64) {
        self.seen_at[idx] = Some(now);
        let k = self.track[idx].note(ts);
        let Some(h) = self.hold.as_mut() else { return };
        if k == h.k[idx] {
            h.ts[idx].get_or_insert(ts);
        }
        if let [Some(a), Some(b)] = h.ts {
            self.paired_k = [Some(h.k[0]), Some(h.k[1])];
            self.pairs.push(json!({"a": a, "b": b}));
            self.hold = None;
            self.open_after = now + self.guard_after;
        }
    }

    /// Is `now` between the predicted ticks of a pair (cut mode)?
    fn predicted_pair_window(&self, now: i64) -> bool {
        let next = |t: &TickTrack| -> Option<i64> {
            let base = *t.bases.iter().min()?;
            let from = now - self.guard_before - CUT_READ_MS - INTERVAL_MS;
            let k = (from - base).div_euclid(INTERVAL_MS) + 1;
            Some(base + k * INTERVAL_MS)
        };
        let (Some(mut n0), Some(mut n1)) = (next(&self.track[0]), next(&self.track[1])) else {
            return false;
        };
        // Pair each tick with the other's nearest one.
        if n1 - n0 > INTERVAL_MS / 2 {
            n1 -= INTERVAL_MS;
        } else if n0 - n1 > INTERVAL_MS / 2 {
            n0 -= INTERVAL_MS;
        }
        let (lead, lag) = (n0.min(n1), n0.max(n1));
        for k in 0..3 {
            let (l, g) = (lead + k * INTERVAL_MS, lag + k * INTERVAL_MS);
            if g - l <= self.flip_gap && now >= l - self.guard_before && now <= g + CUT_READ_MS {
                return true;
            }
        }
        false
    }

    /// May an input change now? Never while the broker path is cut (`cut`):
    /// nothing is observable then and the telemetry generated in a cut is not
    /// value-compared (T8 `cadence`).
    pub fn is_open(&mut self, now: i64, cut: bool) -> bool {
        let (Some(a), Some(b)) = (self.track[0].last_ts, self.track[1].last_ts) else {
            return true;
        };
        if cut || self.reconnecting() {
            // Ticks are not observable while the broker path is cut (the
            // agents queue their payloads), so hold on PREDICTED ticks: the
            // lower-envelope estimate extrapolated through the cut (Node's
            // drift is a few ms per tick). Inputs wait from the lead tick −
            // guard until the lag tick + CUT_READ_MS (its read); in a flip
            // region (gap > flip gap) nothing is held.
            self.hold = None;
            return !self.predicted_pair_window(now);
        }
        if now - a > STALE_MS || now - b > STALE_MS {
            self.hold = None;
            return true;
        }
        if let Some(h) = &self.hold {
            if now > h.start + HOLD_TIMEOUT_MS {
                self.hold = None;
                self.open_after = now + self.guard_after;
            } else {
                return false;
            }
        }
        if now < self.open_after {
            return false;
        }
        // The next unobserved tick of each agent (skipping a given-up one).
        let mut k = [self.track[0].k + 1, self.track[1].k + 1];
        for (ki, skip) in k.iter_mut().zip(self.skip_k) {
            if skip == Some(*ki) {
                *ki += 1;
            }
        }
        let (Some(n0), Some(n1)) = (self.track[0].tick_at(k[0]), self.track[1].tick_at(k[1]))
        else {
            return true;
        };
        let n = [n0, n1];
        // Mid-pair: agent x already ticked (unpaired, not given up) and the
        // other's NEXT tick is its natural partner (closer than the alignment
        // with x's next tick): an input released now would land between the
        // two reads of that pair, and pairing the next ticks instead would
        // orphan x's tick without a `skip` (seen JS-vs-Rust 2026-10-09).
        for x in 0..2 {
            let y = 1 - x;
            let kx = self.track[x].k;
            if self.paired_k[x] == Some(kx) || self.skip_k[x] == Some(kx) {
                continue;
            }
            let Some(tx) = self.track[x].tick_at(kx) else {
                continue;
            };
            let d_mid = n[y] - tx;
            if (0..=self.flip_gap).contains(&d_mid) && d_mid < (n[x] - n[y]).abs() {
                let mut hk = k;
                hk[x] = kx;
                let mut ts = [None, None];
                ts[x] = self.track[x].last_ts;
                self.hold = Some(Hold {
                    start: now,
                    k: hk,
                    ts,
                });
                return false;
            }
        }
        let lead = if n[0] <= n[1] { 0 } else { 1 };
        let lag = 1 - lead;
        if now < n[lead] - self.guard_before {
            return true;
        }
        if n[lag] - n[lead] > self.flip_gap {
            // Flip: this lead tick has no partner that will see the same inputs.
            self.pairs.push(
                json!({"skip": if lead == 0 { "a" } else { "b" }, "k": k[lead], "tick": n[lead]}),
            );
            self.skip_k[lead] = Some(k[lead]);
            return true;
        }
        self.hold = Some(Hold {
            start: n[lead] - self.guard_before,
            k,
            ts: [None, None],
        });
        false
    }
}

// ------------------------------------------------------------------ agents

struct Agent {
    label: &'static str,
    kind: AgentKind,
    device_id: &'static str,
    /// Capture output (`<out>/<label>`).
    dir: PathBuf,
    /// Runtime scratch (`<work>/<label>`).
    work: PathBuf,
    home: PathBuf,
    state: PathBuf,
    lte_path: PathBuf,
    pty_master: File,
    _pty_slave: OwnedFd,
    pty_path: PathBuf,
    imu: Arc<tokio::sync::Mutex<Option<tokio::net::unix::OwnedWriteHalf>>>,
    env: Vec<(String, String)>,
    child: Option<Child>,
    pty_drops: u64,
    /// NMEA lines not written because the agent did not hold the port open.
    pty_unopened: u64,
    pty_open_cache: Option<(std::time::Instant, bool)>,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp-rig");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Every file either agent can write in Phase 2 scope, as `(key, path
/// relative to the agent's work dir)`. Home files keep their bare name (the
/// `final/<name>` layout `diff` already reads); the manifests are `etc/…`.
/// TS writers (@89b5c64): `daemon/settings.ts:641`, `agent/config-sync.ts:241`,
/// `agent/commands.ts:213`, `agent/collectors/gps.ts:243` (gnss-sky),
/// `daemon/state.ts:126`, `daemon/pidfile.ts:98`, `worker.ts:1058`
/// (refresh-modem.flag), `media/manifest.ts:113` (kvs.json uploadBucket
/// backfill), `hardware/hardware-config.ts:155` (hardware.json persist).
/// `etc/pipelines/*` (`media/hardware-reconcile.ts`, `worker.ts:249-256`) is
/// added per run by [`snapshot_targets`].
pub const SNAPSHOT_FILES: &[(&str, &str)] = &[
    ("settings.json", "home/settings.json"),
    ("config-sync.json", "home/config-sync.json"),
    ("commands.json", "home/commands.json"),
    ("gnss-sky.json", "home/gnss-sky.json"),
    ("state.json", "home/state.json"),
    ("agent.pid", "home/agent.pid"),
    ("refresh-modem.flag", "home/refresh-modem.flag"),
    ("etc/kvs.json", "etc/kvs.json"),
    ("etc/hardware.json", "etc/hardware.json"),
];

/// [`SNAPSHOT_FILES`] plus every entry of `etc/pipelines/` (sorted).
fn snapshot_targets(work: &Path) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = SNAPSHOT_FILES
        .iter()
        .map(|(k, rel)| (k.to_string(), work.join(rel)))
        .collect();
    let mut pipes: Vec<String> = fs::read_dir(work.join("etc/pipelines"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    pipes.sort();
    for n in pipes {
        v.push((
            format!("etc/pipelines/{n}"),
            work.join("etc/pipelines").join(&n),
        ));
    }
    v
}

/// Standard base64 (RFC 4648, padded); no extra crate for one call site.
fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for (i, sh) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= c.len() {
                s.push(A[(n >> sh & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// One file's snapshot: raw bytes (base64) + verbatim text when UTF-8, or
/// `{"present": false}` when absent.
fn file_snapshot(path: &Path) -> Value {
    match fs::read(path) {
        Ok(b) => json!({
            "present": true,
            "len": b.len(),
            "b64": base64(&b),
            "text": std::str::from_utf8(&b).ok(),
        }),
        Err(_) => json!({"present": false}),
    }
}

/// `{"w": wall ms, "files": {key: snapshot}}` over [`snapshot_targets`].
fn snapshot_all(work: &Path) -> Value {
    let files: serde_json::Map<String, Value> = snapshot_targets(work)
        .into_iter()
        .map(|(k, p)| (k, file_snapshot(&p)))
        .collect();
    json!({"w": wall_now_ms(), "files": files})
}

fn open_pty() -> Result<(File, OwnedFd, PathBuf)> {
    use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
    let pty = nix::pty::openpty(None, None).context("openpty")?;
    // openpty fds are NOT close-on-exec: without this every agent would
    // inherit both ptys (and `pty_is_open` would always say yes).
    for fd in [&pty.master, &pty.slave] {
        nix::fcntl::fcntl(
            fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )?;
    }
    let mut t = tcgetattr(&pty.slave).context("tcgetattr")?;
    cfmakeraw(&mut t);
    tcsetattr(&pty.slave, SetArg::TCSANOW, &t).context("tcsetattr")?;
    let path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
        .context("pty slave path")?;
    // Non-blocking master: a reader that stopped reading must not stall the
    // player (a full tty buffer drops data, like a real UART overrun).
    let flags = nix::fcntl::fcntl(&pty.master, nix::fcntl::FcntlArg::F_GETFL)?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags.insert(nix::fcntl::OFlag::O_NONBLOCK);
    nix::fcntl::fcntl(&pty.master, nix::fcntl::FcntlArg::F_SETFL(oflags))?;
    Ok((File::from(pty.master), pty.slave, path))
}

/// The agents' `MODE`: the tape meta's `env.MODE` when it is a TS run mode
/// (`sim` / `replay` / `real`; `synth sms-sim` sets `sim`), else `real`.
fn tape_mode(tape: &Tape) -> &'static str {
    match tape.meta.env.get("MODE").and_then(Value::as_str) {
        Some("sim") => "sim",
        Some("replay") => "replay",
        _ => "real",
    }
}

impl Agent {
    fn prepare(
        label: &'static str,
        kind: AgentKind,
        device_id: &'static str,
        args: &PlayArgs,
        tape: &Tape,
        i2c_absent: bool,
    ) -> Result<Self> {
        let dir = args.out.join(label);
        let work = args.work.join(label);
        let _ = fs::remove_dir_all(&work);
        let home = work.join("home");
        let state = work.join("state");
        let bin = work.join("bin");
        let etc = work.join("etc");
        for d in [
            &home,
            &state.join("exec"),
            &bin,
            &etc.join("pipelines"),
            &dir.join("final"),
        ] {
            fs::create_dir_all(d)?;
        }
        // PATH shims (copied so i2cdetect can be absent per agent).
        for entry in
            fs::read_dir(&args.shims).with_context(|| format!("{}", args.shims.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            if (i2c_absent && name == "i2cdetect") || (!args.with_rpicam && name == "rpicam-hello")
            {
                continue;
            }
            let dst = bin.join(&name);
            fs::copy(entry.path(), &dst)?;
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))?;
        }
        // Manifests.
        fs::write(
            etc.join("kvs.json"),
            json!({"channels": tape.meta.kvs.channels}).to_string(),
        )?;
        let hw_path = etc.join("hardware.json");
        if !tape.meta.hardware.is_null() {
            fs::write(&hw_path, serde_json::to_vec_pretty(&tape.meta.hardware)?)?;
        }
        // Home seeded from the tape's start state.
        if let Some(files) = &tape.meta.home {
            for (name, text) in files {
                if let Some(t) = text.as_str() {
                    let t = t.replace(&tape.meta.device_id, device_id);
                    fs::write(home.join(name), t)?;
                }
            }
        }
        let (pty_master, pty_slave, pty_path) = open_pty()?;
        let gps_link = work.join("gps-tty");
        let _ = fs::remove_file(&gps_link);
        std::os::unix::fs::symlink(&pty_path, &gps_link)?;
        let lte_path = state.join("telemetry.json");
        let path_env = format!(
            "{}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            bin.display()
        );
        let cert = |n: &str| args.certs.join(n).display().to_string();
        let env: Vec<(String, String)> = vec![
            ("PATH", path_env),
            ("HOME", work.display().to_string()),
            ("TZ", "UTC".into()),
            ("MODE", tape_mode(tape).into()),
            ("RATE_HZ", "1".into()),
            ("DEVICE_ID", device_id.into()),
            ("IOT_DATA_ENDPOINT", args.endpoint.clone()),
            ("CERT_PATH", cert(&format!("{label}.crt"))),
            ("KEY_PATH", cert(&format!("{label}.key"))),
            ("CA_PATH", cert("ca.pem")),
            ("XORGATE_HOME", home.display().to_string()),
            ("GPS_NMEA_PORT", gps_link.display().to_string()),
            ("LTE_TELEMETRY_FILE", lte_path.display().to_string()),
            ("XORGATE_PYTHON_DIR", args.fake_python.display().to_string()),
            ("KVS_MANIFEST", etc.join("kvs.json").display().to_string()),
            ("HARDWARE_MANIFEST", hw_path.display().to_string()),
            (
                "KVS_PIPELINE_DIR",
                etc.join("pipelines").display().to_string(),
            ),
            (
                "KVS_RUNTIME_BIN",
                work.join("no-kvs-runtime").display().to_string(),
            ),
            ("XORGATE_MOCK", "0".into()),
            ("RIG_STATE", state.display().to_string()),
            ("RIG_SUDO_LOG", work.join("sudo.log").display().to_string()),
            ("RIG_IMU_SOCK", work.join("imu.sock").display().to_string()),
            ("NO_COLOR", "1".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .chain(
            // The Rust agent re-records what it was fed: a round-trip check of
            // the tape hooks (`xorgate-replay inspect <run>/b/agent-tape.ndjson.gz`).
            (kind == AgentKind::Rust).then(|| {
                (
                    "XORGATE_RS_TAPE".to_string(),
                    dir.join("agent-tape.ndjson.gz").display().to_string(),
                )
            }),
        )
        .collect();
        fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(&json!({
                "label": label, "kind": kind.as_str(), "deviceId": device_id,
                "home": home, "ptyPath": pty_path, "env": env.iter().cloned().collect::<BTreeMap<_, _>>(),
            }))?,
        )?;
        Ok(Self {
            label,
            kind,
            device_id,
            dir,
            work,
            home,
            state,
            lte_path,
            pty_master,
            _pty_slave: pty_slave,
            pty_path,
            imu: Arc::new(tokio::sync::Mutex::new(None)),
            env,
            child: None,
            pty_drops: 0,
            pty_unopened: 0,
            pty_open_cache: None,
        })
    }

    fn command(&self, args: &PlayArgs, argv: &[String], worker: bool) -> Command {
        let mut cmd = match self.kind {
            AgentKind::Js => {
                let mut c = Command::new(&args.node);
                let entry = if worker {
                    "dist/worker.js"
                } else {
                    "dist/cli.js"
                };
                c.arg(args.js_dir.join(entry));
                c
            }
            AgentKind::Rust => {
                let mut c = Command::new(&args.rs_bin);
                if worker {
                    c.arg("run");
                }
                c
            }
        };
        cmd.args(argv);
        cmd.env_clear();
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.current_dir(&self.home);
        cmd
    }

    fn start(&mut self, args: &PlayArgs) -> Result<()> {
        let mut cmd = self.command(args, &[], true);
        cmd.stdout(Stdio::from(File::create(self.dir.join("stdout.log"))?));
        cmd.stderr(Stdio::from(File::create(self.dir.join("stderr.log"))?));
        cmd.stdin(Stdio::null());
        cmd.process_group(0);
        cmd.kill_on_drop(true);
        self.child = Some(
            cmd.spawn()
                .with_context(|| format!("starting agent {}", self.label))?,
        );
        Ok(())
    }

    /// Does the agent process hold the pty slave open right now? NMEA is only
    /// written while it does: a line written before the agent opened the port
    /// would sit in the pty queue and reach that agent LATER than its partner
    /// (shifting e.g. the TTFF clock, which starts at the first quality-0 GGA).
    /// Cached for 200 ms.
    fn pty_is_open(&mut self) -> bool {
        let now = std::time::Instant::now();
        if let Some((at, open)) = self.pty_open_cache {
            if now.duration_since(at) < Duration::from_millis(200) {
                return open;
            }
        }
        let open = self
            .child
            .as_ref()
            .and_then(|c| c.id())
            .and_then(|pid| fs::read_dir(format!("/proc/{pid}/fd")).ok())
            .is_some_and(|dir| {
                dir.flatten()
                    .any(|e| fs::read_link(e.path()).is_ok_and(|t| t == self.pty_path))
            });
        self.pty_open_cache = Some((now, open));
        open
    }

    fn nmea(&mut self, line: &str) {
        let mut buf = line.as_bytes().to_vec();
        buf.push(b'\n');
        match self.pty_master.write(&buf) {
            Ok(n) if n == buf.len() => {}
            _ => self.pty_drops += 1,
        }
    }

    fn lte(&self, bytes: Option<&[u8]>) -> Result<()> {
        match bytes {
            Some(b) => write_atomic(&self.lte_path, b),
            None => {
                let _ = fs::remove_file(&self.lte_path);
                Ok(())
            }
        }
    }

    fn exec(&self, argv: &[String], stdout: &str, code: Option<i32>) -> Result<()> {
        let key = argv.join(" ").replace([' ', '/'], "_");
        let key = match key.split_once('_') {
            Some((tool, rest)) => format!("{tool}__{rest}"),
            None => key,
        };
        let base = self.state.join("exec").join(key);
        write_atomic(&base.with_extension("out"), stdout.as_bytes())?;
        write_atomic(
            &base.with_extension("code"),
            code.unwrap_or(127).to_string().as_bytes(),
        )
    }

    fn i2c(&self, stdout: Option<&str>) -> Result<()> {
        match stdout {
            Some(s) => write_atomic(&self.state.join("i2cdetect.out"), s.as_bytes()),
            None => {
                let _ = fs::remove_file(self.state.join("i2cdetect.out"));
                Ok(())
            }
        }
    }

    async fn imu_connected(&self) -> bool {
        self.imu.lock().await.is_some()
    }

    async fn imu_line(&self, line: &str) {
        let mut g = self.imu.lock().await;
        if let Some(w) = g.as_mut() {
            let mut b = line.as_bytes().to_vec();
            b.push(b'\n');
            if w.write_all(&b).await.is_err() {
                *g = None;
            }
        }
    }

    async fn imu_exit(&self, code: Option<i32>) {
        let mut g = self.imu.lock().await;
        if let Some(mut w) = g.take() {
            let _ = w
                .write_all(format!("__EXIT__ {}\n", code.unwrap_or(1)).as_bytes())
                .await;
        }
    }

    /// SIGTERM the agent (both agents are signalled together, then awaited).
    fn signal_term(&self) {
        if let Some(pid) = self.child.as_ref().and_then(|c| c.id()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }

    /// Wait up to 30 s for the graceful exit, then SIGKILL the process group
    /// (which also takes a surviving IMU helper).
    async fn wait_stopped(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let pgid = child.id().map(|p| nix::unistd::Pid::from_raw(p as i32));
        let started = std::time::Instant::now();
        let graceful = tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .is_ok();
        if let Some(pg) = pgid {
            let _ = nix::sys::signal::killpg(pg, nix::sys::signal::Signal::SIGKILL);
        }
        if !graceful {
            let _ = child.wait().await;
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("stderr.log"))
        {
            let _ = writeln!(
                f,
                "[rig] agent {} {} after SIGTERM in {} ms",
                self.label,
                if graceful {
                    "exited"
                } else {
                    "did NOT exit in 30 s; SIGKILLed"
                },
                started.elapsed().as_millis()
            );
        }
    }
}

async fn imu_acceptor(
    sock: PathBuf,
    slot: Arc<tokio::sync::Mutex<Option<tokio::net::unix::OwnedWriteHalf>>>,
    log: Arc<Mutex<File>>,
) -> Result<()> {
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (_r, w) = stream.into_split();
        if let Ok(mut f) = log.lock() {
            let _ = writeln!(f, "{} imu helper connected", wall_now_ms());
        }
        *slot.lock().await = Some(w);
    }
}

// ------------------------------------------------------------------ broker side

async fn proxy(
    listen: String,
    upstream: String,
    cut: Arc<Mutex<Option<CancellationToken>>>,
) -> Result<()> {
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("proxy bind {listen}"))?;
    loop {
        let (mut inbound, _) = listener.accept().await?;
        let token = cut.lock().ok().and_then(|g| g.clone());
        let Some(token) = token else {
            drop(inbound); // span closed: refuse
            continue;
        };
        let upstream = upstream.clone();
        tokio::spawn(async move {
            let Ok(mut out) = TcpStream::connect(&upstream).await else {
                return;
            };
            tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut inbound, &mut out) => {}
                _ = token.cancelled() => {}
            }
        });
    }
}

fn tls_config(certs: &Path) -> Result<Arc<rustls::ClientConfig>> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(certs.join("ca.pem"))? {
        roots.add(c?)?;
    }
    let chain: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(certs.join("player.crt"))?.collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_file(certs.join("player.key"))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)?;
    Ok(Arc::new(cfg))
}

struct Captures {
    files: [Mutex<File>; 2],
    inputs: Mutex<File>,
    gate: Mutex<Gate>,
    /// Periodic-watch holds (`watch.rs`).
    watch: Arc<Mutex<WatchGate>>,
    start_wall: i64,
}

fn mqtt_options(args: &PlayArgs, id: &str) -> Result<rumqttc::v5::MqttOptions> {
    let mut o = rumqttc::v5::MqttOptions::new(id, &args.broker_host, args.broker_port);
    o.set_transport(rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Rustls(
        tls_config(&args.certs)?,
    )));
    o.set_keep_alive(Duration::from_secs(30));
    o.set_clean_start(true);
    o.set_max_packet_size(Some(1_000_000));
    Ok(o)
}

/// The capture subscription. It records each publish's OWN QoS and retain bit
/// (review-wp7 SHOULD 8): granted QoS 2 so the delivered QoS is the
/// publisher's (min(pub, sub)); Retain-As-Published so a live retained publish
/// keeps `retain: true` (without it the broker clears the bit on forwarding);
/// no retained replay on subscribe, so every captured line is a live publish
/// (a stale retained message of an earlier run, or a capture reconnect, never
/// shows up as a publish).
fn capture_filter() -> rumqttc::v5::mqttbytes::v5::Filter {
    use rumqttc::v5::mqttbytes::v5::{Filter, RetainForwardRule};
    let mut f = Filter::new("xorgate/#", rumqttc::v5::mqttbytes::QoS::ExactlyOnce);
    f.nolocal = false;
    f.preserve_retain = true;
    f.retain_forward_rule = RetainForwardRule::Never;
    f
}

async fn capture_loop(
    mut el: rumqttc::v5::EventLoop,
    client: rumqttc::v5::AsyncClient,
    caps: Arc<Captures>,
) {
    use rumqttc::v5::mqttbytes::v5::Packet;
    use rumqttc::v5::Event;
    loop {
        match el.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                let _ = client.subscribe_many([capture_filter()]).await;
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let now = wall_now_ms();
                let topic = String::from_utf8_lossy(&p.topic).to_string();
                let payload = String::from_utf8_lossy(&p.payload).to_string();
                let idx = if topic.contains(DEVICE_A) {
                    0
                } else if topic.contains(DEVICE_B) {
                    1
                } else {
                    continue;
                };
                let inbound = topic.ends_with("/config") || topic.ends_with("/command");
                let line = json!({
                    "t": now - caps.start_wall, "w": now, "topic": topic,
                    "retain": p.retain, "qos": p.qos as u8, "payload": payload,
                })
                .to_string();
                let file = if inbound {
                    &caps.inputs
                } else {
                    &caps.files[idx]
                };
                if let Ok(mut f) = file.lock() {
                    let _ = writeln!(f, "{line}");
                }
                if !inbound {
                    if let Ok(mut w) = caps.watch.lock() {
                        w.on_publish(idx, now);
                        if topic.ends_with("/config/reported") {
                            let eff = serde_json::from_str::<Value>(&payload)
                                .ok()
                                .map(|v| v["effective"].clone())
                                .unwrap_or(Value::Null);
                            let has_modem = eff.get("sim").is_some() || eff.get("uplink").is_some();
                            w.on_report(idx, now, has_modem);
                        }
                    }
                }
                if topic.ends_with("/telemetry") {
                    if let Some(ts) = serde_json::from_str::<Value>(&payload)
                        .ok()
                        .and_then(|v| v["ts"].as_i64())
                    {
                        if let Ok(mut g) = caps.gate.lock() {
                            g.on_tick(idx, ts, now);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Polls every [`snapshot_targets`] file once a second and appends each change
/// (incl. appear/vanish: `text` null) to `<label>/files.ndjson`. `works` are
/// `(work dir, capture dir)` per agent.
async fn snapshot_loop(works: Vec<(PathBuf, PathBuf)>, stop: CancellationToken) {
    let mut last: BTreeMap<(usize, String), Option<Vec<u8>>> = BTreeMap::new();
    type Stat = Option<(u64, u64, i64, i64)>;
    let mut stats: BTreeMap<(usize, String), Stat> = BTreeMap::new();
    let mut files: Vec<File> = works
        .iter()
        .map(|(_, dir)| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("files.ndjson"))
                .expect("files.ndjson")
        })
        .collect();
    loop {
        for (i, (work, _)) in works.iter().enumerate() {
            for (name, path) in snapshot_targets(work) {
                // Stat first and read only on a change: an `open` here would
                // look like an agent's read to the watch gate's inotify.
                let st = fs::metadata(&path).ok().map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    (m.ino(), m.len(), m.mtime(), m.mtime_nsec())
                });
                if stats.get(&(i, name.clone())) == Some(&st) {
                    continue;
                }
                stats.insert((i, name.clone()), st);
                let cur = fs::read(&path).ok();
                let prev = last.get(&(i, name.clone()));
                // The first poll always logs (absent → `text: null`).
                if prev != Some(&cur) {
                    let text = cur.as_ref().map(|b| String::from_utf8_lossy(b).to_string());
                    let _ = writeln!(
                        files[i],
                        "{}",
                        json!({"w": wall_now_ms(), "name": name, "text": text})
                    );
                    last.insert((i, name), cur);
                }
            }
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(1000)) => {}
        }
    }
}

/// `ram_usage` as the agents compute it (TS `hardware/system.ts:85-98`:
/// (MemTotal − MemAvailable) / MemTotal × 100), unrounded.
fn host_ram_pct() -> Option<f64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |key: &str| -> Option<f64> {
        text.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let (total, avail) = (kb("MemTotal:")?, kb("MemAvailable:")?);
    (total > 0.0).then(|| (total - avail) / total * 100.0)
}

/// Aggregate `/proc/stat` jiffies `(busy, total)`: busy = user + nice +
/// system + irq + softirq + steal, total = busy + idle + iowait.
fn host_cpu_jiffies() -> Option<(f64, f64)> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let f: Vec<f64> = text
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    let g = |i: usize| f.get(i).copied().unwrap_or(0.0);
    let busy = g(0) + g(1) + g(2) + g(5) + g(6) + g(7);
    Some((busy, busy + g(3) + g(4)))
}

/// Host reference series for T3 (`host.ndjson`, every 50 ms): both agents read
/// the rig's live /proc, so a concurrent load on the Docker VM moves their
/// values apart between their two tick instants. The diff attributes such a
/// difference to the host only where this series shows the host moved.
async fn host_loop(out: PathBuf, stop: CancellationToken) {
    let Ok(mut f) = File::create(out.join("host.ndjson")) else {
        return;
    };
    loop {
        let mut row = serde_json::Map::new();
        row.insert("w".into(), json!(wall_now_ms()));
        if let Some(ram) = host_ram_pct() {
            row.insert("ram".into(), json!(ram));
        }
        if let Some((busy, total)) = host_cpu_jiffies() {
            row.insert("cpu_busy".into(), json!(busy));
            row.insert("cpu_total".into(), json!(total));
        }
        if row.len() > 1 {
            let _ = writeln!(f, "{}", Value::Object(row));
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

fn gga_quality(line: &str) -> Option<String> {
    let body = line.trim().strip_prefix('$')?;
    let f: Vec<&str> = body.split(['*', ',']).collect();
    (f.first()?.ends_with("GGA")).then(|| f.get(6).unwrap_or(&"").to_string())
}

// ------------------------------------------------------------------ main loop

pub async fn run(args: PlayArgs) -> Result<()> {
    if (args.speed - 1.0).abs() > f64::EPSILON {
        eprintln!(
            "WARNING: --speed {} — not valid parity evidence (timers differ)",
            args.speed
        );
    }
    let tape =
        Tape::read(&args.tape).with_context(|| format!("reading {}", args.tape.display()))?;
    fs::create_dir_all(&args.out)?;
    let calibration = args.a == args.b;
    let first_i2c_absent = tape.records.iter().find_map(|r| match &r.ev {
        TapeEvent::I2cdetect { not_found, .. } => Some(*not_found),
        _ => None,
    }) == Some(true);
    let mut agents = vec![
        Agent::prepare("a", args.a, DEVICE_A, &args, &tape, first_i2c_absent)?,
        Agent::prepare("b", args.b, DEVICE_B, &args, &tape, first_i2c_absent)?,
    ];
    let log = Arc::new(Mutex::new(File::create(args.out.join("player.log"))?));
    let plog = |msg: String| {
        if let Ok(mut f) = log.lock() {
            let _ = writeln!(f, "{} {msg}", wall_now_ms());
        }
    };

    let start_wall = wall_now_ms();
    let shift_s = if args.no_shift {
        0
    } else {
        ((start_wall - tape.meta.start_wall) as f64 / 1000.0).round() as i64
    };
    plog(format!(
        "tape {} ({} records), shift {shift_s} s, A={} B={}",
        args.tape.display(),
        tape.records.len(),
        args.a.as_str(),
        args.b.as_str()
    ));

    // Broker side: capture client, input publisher, proxy.
    let caps = Arc::new(Captures {
        files: [
            Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(agents[0].dir.join("mqtt.ndjson"))?,
            ),
            Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(agents[1].dir.join("mqtt.ndjson"))?,
            ),
        ],
        inputs: Mutex::new(File::create(args.out.join("inputs-mqtt.ndjson"))?),
        gate: Mutex::new(Gate::new(args.guard_ms).with_flip_gap(args.flip_gap_ms)),
        watch: Arc::new(Mutex::new(WatchGate::new())),
        start_wall,
    });
    let (cap_client, cap_el) =
        rumqttc::v5::AsyncClient::new(mqtt_options(&args, "rig-capture")?, 1024);
    tokio::spawn(capture_loop(cap_el, cap_client.clone(), caps.clone()));
    let (pub_client, mut pub_el) =
        rumqttc::v5::AsyncClient::new(mqtt_options(&args, "rig-player")?, 1024);
    tokio::spawn(async move {
        loop {
            if pub_el.poll().await.is_err() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    });
    let cut: Arc<Mutex<Option<CancellationToken>>> =
        Arc::new(Mutex::new(Some(CancellationToken::new())));
    let upstream = format!("{}:{}", args.broker_host, args.broker_port);
    tokio::spawn(proxy(args.proxy_listen.clone(), upstream, cut.clone()));

    // Clear any retained config from a previous run.
    for a in &agents {
        let topic = format!("xorgate/devices/{}/config", a.device_id);
        let _ = pub_client
            .publish(
                topic,
                rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
                true,
                Vec::<u8>::new(),
            )
            .await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // IMU sockets.
    for a in &agents {
        tokio::spawn(imu_acceptor(
            a.work.join("imu.sock"),
            a.imu.clone(),
            log.clone(),
        ));
    }

    // Prefill the state-like inputs the agents read at boot (the files and
    // tools existed before the recording agent started).
    let mut transitions: Vec<i64> = Vec::new();
    let mut prefilled = vec![false; tape.records.len()];
    for (i, r) in tape.records.iter().enumerate() {
        if r.t >= PREFILL_MS {
            break;
        }
        if matches!(
            r.ev,
            TapeEvent::LteFile { .. }
                | TapeEvent::Exec { .. }
                | TapeEvent::I2cdetect { .. }
                | TapeEvent::Dt { .. }
        ) {
            apply_state(&agents, &r.ev, shift_s)?;
            prefilled[i] = true;
        }
    }
    let idx = prefilled.iter().filter(|p| **p).count();
    let snap_stop = CancellationToken::new();
    tokio::spawn(snapshot_loop(
        agents
            .iter()
            .map(|a| (a.work.clone(), a.dir.clone()))
            .collect(),
        snap_stop.clone(),
    ));
    tokio::spawn(host_loop(args.out.clone(), snap_stop.clone()));

    // The seeded state each agent starts from (manifests, home seed).
    for a in &agents {
        fs::write(
            a.dir.join("initial.json"),
            serde_json::to_vec_pretty(&snapshot_all(&a.work))?,
        )?;
    }

    // The watch gate's eyes: every open/replace of each agent's
    // settings.json and LTE file (`watch.rs`, `<label>/reads.ndjson`).
    let watch_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(target_os = "linux")]
    watch::observe(
        agents
            .iter()
            .map(|a| (a.home.clone(), a.state.clone(), a.dir.join("reads.ndjson")))
            .collect(),
        caps.watch.clone(),
        watch_stop.clone(),
    )
    .context("inotify on the agents' settings/LTE dirs")?;

    // Start both agents together.
    let t0 = tokio::time::Instant::now();
    let play_start_wall = wall_now_ms();
    for (i, a) in agents.iter_mut().enumerate() {
        if i == 1 && args.stagger_ms > 0 {
            tokio::time::sleep(Duration::from_millis(args.stagger_ms)).await;
        }
        if let Ok(mut w) = caps.watch.lock() {
            w.on_spawn(i, wall_now_ms());
        }
        a.start(&args)?;
    }
    plog(format!(
        "agents started (B staggered {} ms); prefilled {idx} records",
        args.stagger_ms
    ));

    let mut closed_spans: Vec<(i64, i64)> = Vec::new();
    let mut closed_since: Option<i64> = None;
    let mut retained: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut last_nmea: Option<(u64, i64)> = None;
    let mut nmea_resumes: Vec<i64> = Vec::new();
    let mut asym_since: Option<i64> = None;
    let mut imu_asym_since: Option<i64> = None;
    let mut last_quality: Option<String> = None;
    let mut last_imu: Option<u64> = None;
    let mut wall_steps: Vec<(i64, i64)> = Vec::new();
    let mut prev_tw: Option<(u64, i64)> = None;
    let mut released = 0usize;
    let cutoff = args.max_s.map(|s| s * 1000).unwrap_or(u64::MAX);

    // Release order = tape order, except that a record the WATCH gate holds
    // (lte_file / cli / config, `watch.rs`) waits in `pending` together with
    // every later record that must keep its order relative to it (cli,
    // mqtt_in, conn, lte_file), while the line-like inputs (NMEA, IMU, exec,
    // i2cdetect, dt, imu spawn/exit) keep their own timing: holding those too
    // bunched NMEA bursts behind a ~1-2 s watch hold (calibration 140727Z: a
    // burst released after Rust's read and before JS's while Rust was still
    // reconnecting). Every release still passes the telemetry hold gate.
    let barrier = |ev: &TapeEvent| {
        matches!(
            ev,
            TapeEvent::LteFile { .. }
                | TapeEvent::Cli { .. }
                | TapeEvent::MqttIn { .. }
                | TapeEvent::Conn { .. }
        )
    };
    let watch_holds = |ev: &TapeEvent, now: i64| {
        watch::gated(ev).is_some_and(|(input, look)| {
            caps.watch
                .lock()
                .map(|mut w| w.holds(input, now, look))
                .unwrap_or(false)
        })
    };
    let mut pending: std::collections::VecDeque<(usize, i64)> = Default::default();
    let mut next = 0usize;
    let n_rec = tape.records.len();
    loop {
        // (record index, wall ms it fell due, released by a hold timeout)
        let picked: Option<(usize, i64, bool)> = loop {
            while next < n_rec && prefilled[next] {
                next += 1;
            }
            if next < n_rec && tape.records[next].t > cutoff {
                next = n_rec;
            }
            if next >= n_rec && pending.is_empty() {
                break None;
            }
            let now = wall_now_ms();
            let cut_active = closed_since.is_some();
            let tel_open = |now: i64| {
                args.no_gate
                    || caps
                        .gate
                        .lock()
                        .map(|mut g| g.is_open(now, cut_active))
                        .unwrap_or(true)
            };
            // Consult the hold gate on EVERY pass, also when nothing is due:
            // it forms a pair hold only when asked near a lead tick, so on a
            // sparse stretch (sms-sim: 9 records, 0 pairs; a tape IMU-helper
            // restart: a tick orphaned with no `skip`) ticks went unpaired.
            let _ = tel_open(now);
            if let Some(&(pi, due_w)) = pending.front() {
                let ev = &tape.records[pi].ev;
                let held = watch_holds(ev, now);
                let timeout = held && now - due_w > watch::MAX_HOLD_MS;
                if (!held || timeout) && tel_open(now) {
                    pending.pop_front();
                    break Some((pi, due_w, timeout));
                }
            }
            let mut wait = Duration::from_millis(5);
            if next < n_rec {
                let r = &tape.records[next];
                let due = t0 + Duration::from_millis((r.t as f64 / args.speed) as u64);
                let left = due.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    if !args.no_gate
                        && barrier(&r.ev)
                        && (!pending.is_empty() || watch_holds(&r.ev, now))
                    {
                        pending.push_back((next, now));
                        next += 1;
                        continue;
                    }
                    if tel_open(now) {
                        next += 1;
                        break Some((next - 1, now, false));
                    }
                } else if pending.is_empty() && args.no_gate {
                    wait = left;
                } else {
                    wait = wait.min(left);
                }
            }
            tokio::time::sleep(wait).await;
        };
        let Some((i, due_w, timeout)) = picked else {
            break;
        };
        let r = &tape.records[i];
        if timeout {
            plog(format!("watch gate: hold timeout at t={}", r.t));
        }
        if let Some((input, _)) = watch::gated(&r.ev) {
            if let Ok(mut w) = caps.watch.lock() {
                w.note_release(input, due_w, wall_now_ms(), timeout);
            }
        }
        let now = wall_now_ms();
        if let Some((pt, pw)) = prev_tw {
            let skew = (r.w - pw) - (r.t as i64 - pt as i64);
            if skew.abs() > crate::inspect::STEP_THRESHOLD_MS {
                wall_steps.push((now, skew));
            }
        }
        prev_tw = Some((r.t, r.w));
        released += 1;
        match &r.ev {
            TapeEvent::Nmea { line } => {
                let line = rewrite::shift_nmea_line(line, shift_s);
                if let Some((lt, lw)) = last_nmea {
                    if r.t - lt > 2_000 {
                        transitions.push(lw);
                        // The gps group goes stale NMEA_STALE_MS after the
                        // last line (gps.ts:15, gps_group.rs): the input
                        // transition's effect, at each agent's own tick.
                        if r.t - lt > NMEA_STALE_MS as u64 {
                            transitions.push(lw + NMEA_STALE_MS);
                        }
                        if r.t - lt >= REOPEN_SILENT_MS as u64 {
                            nmea_resumes.push(now);
                        }
                        transitions.push(now);
                    }
                } else {
                    transitions.push(now);
                }
                if let Some(q) = gga_quality(&line) {
                    if last_quality.as_ref() != Some(&q) {
                        transitions.push(now);
                        last_quality = Some(q);
                    }
                }
                last_nmea = Some((r.t, now));
                // Symmetric delivery: a line goes to BOTH agents only while
                // both hold their port open (the TS opens it lazily on its
                // first tick and reopens after 30 s of silence — at its own
                // phase). If one agent has not opened it for 5 s while the
                // other has, feed the other alone so a never-opened port
                // shows up in the diff instead of hiding both gps groups.
                let ticking = caps.gate.lock().map(|g| g.both_ticking()).unwrap_or(true);
                let open: Vec<bool> = agents
                    .iter_mut()
                    .map(|a| ticking && a.pty_is_open())
                    .collect();
                let all = open.iter().all(|o| *o);
                if all || !open.iter().any(|o| *o) {
                    asym_since = None;
                } else if asym_since.is_none() {
                    asym_since = Some(now);
                }
                let feed_alone = asym_since.is_some_and(|t| now - t > 5_000);
                for (a, is_open) in agents.iter_mut().zip(&open) {
                    if all || (feed_alone && *is_open) {
                        a.nmea(&line);
                    } else {
                        a.pty_unopened += 1;
                    }
                }
            }
            TapeEvent::Imu { line } => {
                if last_imu.is_none_or(|lt| r.t - lt > 2_000) {
                    transitions.push(now);
                }
                last_imu = Some(r.t);
                // Same symmetric rule as NMEA: a line goes to both helpers or
                // to neither (each agent spawns its helper at its own pace);
                // after 5 s of asymmetry the connected one is fed alone.
                let ticking = caps.gate.lock().map(|g| g.both_ticking()).unwrap_or(true);
                let mut conn = Vec::new();
                for a in &agents {
                    conn.push(ticking && a.imu_connected().await);
                }
                let all = conn.iter().all(|c| *c);
                if all || !conn.iter().any(|c| *c) {
                    imu_asym_since = None;
                } else if imu_asym_since.is_none() {
                    imu_asym_since = Some(now);
                }
                let alone = imu_asym_since.is_some_and(|t| now - t > 5_000);
                for (a, c) in agents.iter().zip(&conn) {
                    if all || (alone && *c) {
                        a.imu_line(line).await;
                    }
                }
            }
            TapeEvent::ImuSpawn { .. } => {
                transitions.push(now);
                plog(format!("tape imu_spawn at t={}", r.t));
            }
            TapeEvent::ImuExit { code } => {
                transitions.push(now);
                for a in &agents {
                    a.imu_exit(*code).await;
                }
            }
            TapeEvent::LteFile { .. }
            | TapeEvent::Exec { .. }
            | TapeEvent::I2cdetect { .. }
            | TapeEvent::Dt { .. } => {
                transitions.push(now);
                apply_state(&agents, &r.ev, shift_s)?;
            }
            TapeEvent::MqttIn {
                topic, retain, qos, ..
            } => {
                transitions.push(now);
                let payload = r.ev.mqtt_payload().unwrap_or_default();
                let is_config = topic.ends_with("/config");
                let is_command = topic.ends_with("/command");
                for a in &agents {
                    let t = topic.replace(&tape.meta.device_id, a.device_id);
                    let mut p = rewrite::substitute_id(&payload, &tape.meta.device_id, a.device_id);
                    if is_command {
                        p = rewrite::shift_command_payload(&p, shift_s * 1000);
                    }
                    if is_config && *retain && retained.get(&t) == Some(&p) {
                        // A retained REDELIVERY: the broker redelivers on the
                        // agent's resubscribe by itself.
                        continue;
                    }
                    let q = if *qos >= 1 {
                        rumqttc::v5::mqttbytes::QoS::AtLeastOnce
                    } else {
                        rumqttc::v5::mqttbytes::QoS::AtMostOnce
                    };
                    // The cloud publishes config retained; commands are not.
                    let ret = is_config;
                    if is_config {
                        retained.insert(t.clone(), p.clone());
                    }
                    let _ = pub_client.publish(t, q, ret, p).await;
                }
            }
            TapeEvent::Conn { state } => {
                transitions.push(now);
                match state {
                    ConnState::Closed => {
                        if closed_since.is_none() {
                            closed_since = Some(now);
                            if let Ok(mut g) = cut.lock() {
                                if let Some(tok) = g.take() {
                                    tok.cancel();
                                }
                            }
                            plog(format!("conn closed (proxy cut) at t={}", r.t));
                        }
                    }
                    ConnState::Connected => {
                        if let Some(s) = closed_since.take() {
                            closed_spans.push((s, now));
                            if let Ok(mut g) = cut.lock() {
                                *g = Some(CancellationToken::new());
                            }
                            if let Ok(mut g) = caps.gate.lock() {
                                g.on_restore(now);
                            }
                            plog(format!("conn restored at t={}", r.t));
                        }
                    }
                }
            }
            TapeEvent::Cli { argv } => {
                transitions.push(now);
                for a in &agents {
                    let mut cmd = a.command(&args, argv, false);
                    let out = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(a.dir.join("cli.log"))?;
                    let mut header = out.try_clone()?;
                    let _ = writeln!(header, "=== {} $ {}", now, argv.join(" "));
                    cmd.stdout(Stdio::from(out.try_clone()?));
                    cmd.stderr(Stdio::from(out));
                    cmd.stdin(Stdio::null());
                    match cmd.spawn() {
                        Ok(mut child) => {
                            tokio::spawn(async move {
                                let st =
                                    tokio::time::timeout(Duration::from_secs(60), child.wait())
                                        .await;
                                let _ = writeln!(
                                    header,
                                    "=== exit {:?}",
                                    st.ok().and_then(|r| r.ok()).and_then(|s| s.code())
                                );
                            });
                        }
                        Err(e) => {
                            let _ = writeln!(header, "=== spawn failed: {e}");
                        }
                    }
                }
            }
            TapeEvent::Clock { .. } | TapeEvent::Proc { .. } => {}
        }
    }
    if let Some(s) = closed_since.take() {
        closed_spans.push((s, wall_now_ms()));
        if let Ok(mut g) = cut.lock() {
            *g = Some(CancellationToken::new());
        }
    }
    plog(format!(
        "tape done ({released} released); settling {} s",
        args.settle_s
    ));
    tokio::time::sleep(Duration::from_secs(args.settle_s)).await;

    let end_wall = wall_now_ms();
    snap_stop.cancel();
    watch_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let watch_report = caps
        .watch
        .lock()
        .map(|mut w| w.report(end_wall))
        .unwrap_or(Value::Null);
    fs::write(
        args.out.join("watch.json"),
        serde_json::to_vec_pretty(&watch_report)?,
    )?;
    for a in agents.iter() {
        a.signal_term();
    }
    let (first, second) = agents.split_at_mut(1);
    tokio::join!(first[0].wait_stopped(), second[0].wait_stopped());
    // Final file copies (`final/<key>`, raw bytes) + `final.json`: every
    // [`snapshot_targets`] file with base64 bytes, or `present: false`.
    for a in &agents {
        for (name, path) in snapshot_targets(&a.work) {
            if let Ok(b) = fs::read(&path) {
                let dst = a.dir.join("final").join(&name);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(dst, b)?;
            }
        }
        fs::write(
            a.dir.join("final.json"),
            serde_json::to_vec_pretty(&snapshot_all(&a.work))?,
        )?;
        if let Ok(b) = fs::read(a.work.join("sudo.log")) {
            fs::write(a.dir.join("sudo.log"), b)?;
        }
        if a.pty_unopened > 0 {
            plog(format!(
                "agent {}: {} NMEA lines not written (port not open by both agents)",
                a.label, a.pty_unopened
            ));
        }
        if a.pty_drops > 0 {
            plog(format!(
                "agent {}: {} pty writes dropped (reader not reading)",
                a.label, a.pty_drops
            ));
        }
        let _ = &a.pty_path;
    }
    let pairs = caps
        .gate
        .lock()
        .map(|g| g.pairs.clone())
        .unwrap_or_default();
    let mut pf = File::create(args.out.join("pairs.ndjson"))?;
    for p in &pairs {
        writeln!(pf, "{p}")?;
    }
    if let Some((_, lw)) = last_nmea {
        // NMEA ends with the tape: the gps group goes stale 10 s later.
        transitions.push(lw + NMEA_STALE_MS);
    }
    transitions.sort();
    transitions.dedup();
    let info = PlayInfo {
        tape: args.tape.display().to_string(),
        start_wall: play_start_wall,
        end_wall,
        shift_s,
        closed_spans,
        transitions,
        wall_steps,
        calibration,
        host: Vec::new(),
        skips: Vec::new(),
        nmea_resumes,
    };
    fs::write(
        args.out.join("play.json"),
        serde_json::to_vec_pretty(&info)?,
    )?;
    plog(format!("done: {} pairs", pairs.len()));
    if pairs.is_empty() && !args.no_gate {
        bail!(
            "no telemetry pairs observed — did both agents connect? see {}/*/stderr.log",
            args.out.display()
        );
    }
    Ok(())
}

fn apply_state(agents: &[Agent], ev: &TapeEvent, shift_s: i64) -> Result<()> {
    for a in agents {
        match ev {
            TapeEvent::LteFile { .. } => {
                let bytes = ev.lte_bytes().map(|b| match String::from_utf8(b) {
                    Ok(s) => rewrite::shift_lte_text(&s, shift_s).into_bytes(),
                    Err(e) => e.into_bytes(),
                });
                a.lte(bytes.as_deref())?;
            }
            TapeEvent::Exec { argv, stdout, code } => a.exec(argv, stdout, *code)?,
            TapeEvent::I2cdetect { stdout, .. } => a.i2c(stdout.as_deref())?,
            TapeEvent::Dt { .. } => {
                // /proc/device-tree is a fixed path in both agents and cannot
                // be redirected in a container: psu_max_current is absent on
                // both sides of the rig (README "Limits").
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_holds_between_paired_ticks() {
        let mut g = Gate::new(60);
        assert!(g.is_open(0, false));
        g.on_tick(0, 1_000, 1_005); // A tick 0 at 1000
        g.on_tick(1, 1_300, 1_305); // B tick 0 at 1300
        assert!(g.is_open(1_400, false));
        assert!(g.is_open(1_939, false));
        assert!(!g.is_open(1_941, false)); // A due at 2000
        g.on_tick(0, 2_000, 2_004);
        assert!(!g.is_open(2_100, false)); // waiting for B
        g.on_tick(1, 2_300, 2_306);
        assert!(!g.is_open(2_310, false)); // guard after
        assert!(g.is_open(2_330, false));
        assert_eq!(g.pairs, vec![json!({"a": 2_000, "b": 2_300})]);
    }

    #[test]
    fn gate_uses_the_lower_envelope_of_ts_minus_latency() {
        let mut g = Gate::new(60);
        // A ticks at …000 with read latency 5..250 ms; B at …500.
        for (i, lat) in [5, 250, 10, 120].iter().enumerate() {
            g.on_tick(0, 1_000 + i as i64 * 1_000 + lat, 0);
            g.on_tick(1, 1_500 + i as i64 * 1_000 + 3, 0);
        }
        // Next A tick is at 5000 (not 5000 + latency): hold from 4940.
        assert!(g.is_open(4_930, false));
        assert!(!g.is_open(4_945, false));
        g.on_tick(1, 5_503, 5_504); // B first (A slow to read) — still waiting
        assert!(!g.is_open(5_510, false));
        g.on_tick(0, 5_260, 5_520);
        assert!(g.is_open(5_545, false));
        assert_eq!(g.pairs.last(), Some(&json!({"a": 5_260, "b": 5_503})));
    }

    #[test]
    fn gate_is_open_during_a_cut_and_for_silent_agents() {
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_300, 1_300);
        // Cut: predicted ticks A 2000 / B 2300 → held 1940..2450, open after.
        assert!(g.is_open(1_900, true));
        assert!(!g.is_open(1_950, true));
        assert!(!g.is_open(2_400, true));
        assert!(g.is_open(2_500, true));
        // …and 20 ticks later (predicted through the cut).
        assert!(!g.is_open(22_100, true));
        assert!(g.is_open(22_600, true));
        assert!(!g.is_open(1_950, false));
        // No partner observed: still holding until an agent is silent > 2.5 s.
        assert!(!g.is_open(3_400, false));
        assert!(g.is_open(3_600, false));
    }

    #[test]
    fn gate_keeps_predicted_holds_until_both_agents_publish_after_a_restore() {
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_300, 1_300);
        // Cut 1500..5000; A (mqtt.js) is back at once, B (backoff) is not.
        assert!(!g.is_open(1_950, true));
        g.on_restore(5_000);
        g.on_tick(0, 5_000, 5_010);
        // B is unobserved (> 2.5 s silent): without the restore latch the gate
        // would be open; it holds on B's PREDICTED tick 5300 (+ read) instead.
        assert!(!g.is_open(5_200, false));
        assert!(!g.is_open(5_440, false));
        assert!(g.is_open(5_500, false));
        // B publishes again (its queue flushes): normal gating resumes.
        g.on_tick(1, 5_300, 5_600);
        assert!(g.restored_at.is_some());
        assert!(g.is_open(5_700, false));
        assert!(g.restored_at.is_none());
        // Without a restore an agent silent > 2.5 s leaves the gate open.
        let mut h = Gate::new(60);
        h.on_tick(0, 1_000, 1_000);
        h.on_tick(1, 1_300, 1_300);
        h.on_tick(0, 5_000, 5_010);
        assert!(h.is_open(5_200, false));
    }

    #[test]
    fn gate_holds_a_pair_already_half_observed() {
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_440, 1_440);
        g.on_tick(0, 2_000, 2_002);
        // A's tick at 2000 is observed; B's partner is due at 2440 (440 ms)
        // — closer than A's next (3000 − 2440 = 560): hold for (2000, 2440).
        assert!(!g.is_open(2_100, false));
        g.on_tick(1, 2_445, 2_446);
        assert!(g.is_open(2_500, false));
        assert_eq!(g.pairs, vec![json!({"a": 2_000, "b": 2_445})]);
    }

    #[test]
    fn gate_flips_when_the_gap_exceeds_800() {
        // Without an observed partner the lead tick is given up (`skip`).
        let mut s = Gate::new(60);
        s.on_tick(0, 1_000, 1_000);
        s.on_tick(1, 150, 150);
        s.paired_k = [Some(0), Some(0)];
        // A due 2000, B due 1150 → B leads; next: A 2000 vs B 1150 gap 850.
        assert!(s.is_open(1_100, false));
        assert_eq!(s.pairs, vec![json!({"skip": "b", "k": 1, "tick": 1_150})]);

        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_850, 1_850);
        // At 1945: A due 2000, B due 2850 → gap 850; but B's tick at 1850 is
        // observed and unpaired and A's 2000 is its natural partner (150 ms):
        // the mid-pair hold pairs them instead of giving up A's tick.
        assert!(!g.is_open(1_945, false));
        g.on_tick(0, 2_010, 2_011);
        assert_eq!(g.pairs, vec![json!({"a": 2_010, "b": 1_850})]);
        // Now B (2850) leads A (3000).
        assert!(g.is_open(2_700, false));
        assert!(!g.is_open(2_800, false));
        g.on_tick(1, 2_860, 2_861);
        g.on_tick(0, 3_004, 3_005);
        assert_eq!(g.pairs.last(), Some(&json!({"a": 3_004, "b": 2_860})));
    }

    #[test]
    fn gate_flip_gap_is_a_rig_parameter_defaulting_to_the_nearest_alignment() {
        // A ticks at …000, B at …620: the 620 ms alignment (A k, B k) vs the
        // 380 ms one (B k, A k+1).
        let run = |g: &mut Gate| {
            g.on_tick(0, 1_000, 1_000);
            g.on_tick(1, 1_620, 1_620);
            g.paired_k = [Some(0), Some(0)];
            g.is_open(1_945, false)
        };
        // Default 500: 620 > 500 → A's tick 1 (2000) is given up and B 2620
        // pairs with A 3000 (380 ms apart).
        let mut g = Gate::new(60);
        assert_eq!(g.flip_gap, DEFAULT_FLIP_GAP_MS);
        assert_eq!(DEFAULT_FLIP_GAP_MS, 500);
        assert!(run(&mut g));
        assert_eq!(g.pairs, vec![json!({"skip": "a", "k": 1, "tick": 2_000})]);
        assert!(!g.is_open(2_570, false), "B 2620 leads now");
        g.on_tick(1, 2_620, 2_621);
        g.on_tick(0, 3_000, 3_001);
        assert_eq!(g.pairs.last(), Some(&json!({"a": 3_000, "b": 2_620})));
        // The old 800: the 620 ms alignment is kept (no skip, A leads).
        let mut g = Gate::new(60).with_flip_gap(800);
        assert!(!run(&mut g));
        assert!(g.pairs.is_empty());
        // 500 exactly is not past the gap (either alignment is 500 ms).
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_500, 1_500);
        g.paired_k = [Some(0), Some(0)];
        assert!(!g.is_open(1_945, false));
        assert!(g.pairs.is_empty());
    }

    #[test]
    fn capture_keeps_publish_qos_and_retain() {
        use rumqttc::v5::mqttbytes::v5::RetainForwardRule;
        let f = capture_filter();
        assert_eq!(f.qos, rumqttc::v5::mqttbytes::QoS::ExactlyOnce);
        assert!(f.preserve_retain);
        assert!(!f.nolocal);
        assert!(matches!(f.retain_forward_rule, RetainForwardRule::Never));
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn snapshot_covers_the_diff_files_and_records_absence() {
        for name in crate::diff::FILES {
            assert!(
                SNAPSHOT_FILES.iter().any(|(k, _)| k == name),
                "{name} not snapshotted"
            );
        }
        let work = std::env::temp_dir().join(format!("xr-snap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(work.join("home")).unwrap();
        fs::create_dir_all(work.join("etc/pipelines")).unwrap();
        fs::write(work.join("etc/kvs.json"), b"{\"channels\":[]}\n").unwrap();
        fs::write(work.join("etc/pipelines/cam0.gst"), b"x\n").unwrap();
        let v = snapshot_all(&work);
        let f = &v["files"];
        assert_eq!(f["etc/kvs.json"]["present"], true);
        assert_eq!(f["etc/kvs.json"]["text"], "{\"channels\":[]}\n");
        assert_eq!(f["etc/kvs.json"]["len"], 16);
        assert_eq!(f["etc/hardware.json"], json!({"present": false}));
        assert_eq!(f["refresh-modem.flag"], json!({"present": false}));
        assert_eq!(f["etc/pipelines/cam0.gst"]["b64"], "eAo=");
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn gga_quality_parses() {
        assert_eq!(
            gga_quality("$GPGGA,1,2,N,3,E,1,08,0.9*00\r").as_deref(),
            Some("1")
        );
        assert_eq!(
            gga_quality("$GPGGA,083745.000,,,,,0,00,,,M,,M,,*75").as_deref(),
            Some("0")
        );
        assert_eq!(gga_quality("$GPRMC,1"), None);
    }
}
