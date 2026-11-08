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

use std::collections::{BTreeMap, BTreeSet};
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
use crate::media::{self, MediaPlan, ObsAgent};
use crate::watch::{self, WatchGate};
use crate::{aws, handoff, p4, p5b, rewrite};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AgentKind {
    Js,
    Rust,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
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
    /// The JS build (`dist/worker.js`, `dist/cli.js`, `node_modules/`) of
    /// both JS sides unless `--js-dir-a` / `--js-dir-b` names one per side.
    #[arg(long, default_value = "/rig/cache/js-09da38d")]
    pub js_dir: PathBuf,
    /// Phase 5b (dual ref): the JS build agent A runs (default `--js-dir`),
    /// so a JS-vs-JS run can pair two refs (`run-rig.sh --a js@<ref>`).
    #[arg(long)]
    pub js_dir_a: Option<PathBuf>,
    /// Phase 5b: the JS build agent B runs (default `--js-dir`).
    #[arg(long)]
    pub js_dir_b: Option<PathBuf>,
    #[arg(long, default_value = "/rig/cache/bin/xorgate-rs")]
    pub rs_bin: PathBuf,
    /// Phase 5b: the Rust repo's vendored `python/` (the OBD helper a Rust
    /// side runs; falls back to the JS build's while it lacks `obd_read.py`).
    #[arg(long, default_value = "/rig/python")]
    pub rs_python: PathBuf,
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
    /// The rig's media dir (the fake KVS master, `tools/replay/media/`).
    #[arg(long, default_value = "/rig/tools/media")]
    pub media_dir: PathBuf,
    /// Where the fake IoT credentials + S3 listen (`aws.rs`).
    #[arg(long, default_value = "0.0.0.0:443")]
    pub aws_listen: String,
    /// Start agent B this many ms after A (calibration: forces the two tick
    /// phases apart so the hold gate is exercised like JS vs Rust).
    #[arg(long, default_value_t = 0)]
    pub stagger_ms: u64,
    /// Pair gap above which the hold gate flips the alignment (skips one lead
    /// tick). 500 = always the nearest alignment (series 5: at 800 a 772 ms
    /// pair outlived a 5 s sky_state dwell between its two reads).
    #[arg(long, default_value_t = DEFAULT_FLIP_GAP_MS)]
    pub flip_gap_ms: i64,
    /// Phase 4: the private-mount-namespace entry for Pi-namespaced tapes
    /// (`meta.rig.pi`, `p4.rs`).
    #[arg(long, default_value = "/rig/tools/pi-ns.sh")]
    pub ns_script: PathBuf,
    /// Phase 5b (R-P3G-4): read the rig VM's own /proc/meminfo (the pre-5b
    /// behaviour) instead of binding the tape's fixed meminfo over it.
    #[arg(long)]
    pub host_meminfo: bool,
    /// Phase 5b WP5 (item 4): the preload that answers statvfs() of the root
    /// filesystem from [`RIG_STATVFS_ROOT`] for BOTH agents (and the TS's `df`
    /// child) — `tools/replay/statvfs-shim/rig_statvfs.c`, built by
    /// build-bins.sh. A missing file FAILS the run unless `--host-disk`
    /// (review-wp5 R-WP5-4); `play.json` records `diskSource`.
    #[arg(long, default_value = "/rig/cache/bin/rig-statvfs.so")]
    pub statvfs_shim: PathBuf,
    /// Read the rig VM's own root filesystem for `disk_usage` (the pre-WP5
    /// behaviour) instead of the fixed [`RIG_STATVFS_ROOT`].
    #[arg(long)]
    pub host_disk: bool,
    /// Phase 4 (E12): the REAL camera-rotation helper the `sudo` shim executes
    /// for `xorgate-camrotate` (default: the Rust repo's vendored
    /// `/rig/system/xorgate-camrotate`, else the JS build's byte-identical copy).
    #[arg(long)]
    pub camrotate: Option<PathBuf>,
    /// Phase 6 P3a (`handoff.rs`): at the tape's `meta.rig.handoff.atMs`,
    /// stop BOTH agents the way systemd stops a unit (`term`: SIGTERM to every
    /// process of the unit, SIGKILL after 90 s; `kill`: SIGKILL to all) and
    /// restart each side on the SAME home as `--then-a` / `--then-b`.
    #[arg(long, value_enum)]
    pub handoff_signal: Option<handoff::StopSignal>,
    /// The handoff instant (tape ms) instead of the tape's `atMs`.
    #[arg(long)]
    pub handoff_at_ms: Option<u64>,
    /// What side A runs after the handoff (default: `--a` again).
    #[arg(long, value_enum)]
    pub then_a: Option<AgentKind>,
    /// What side B runs after the handoff (default: `--b` again).
    #[arg(long, value_enum)]
    pub then_b: Option<AgentKind>,
    /// Stop → start gap after the slower side's whole stop.
    #[arg(long, default_value_t = handoff::RESTART_GAP_MS)]
    pub handoff_gap_ms: u64,
    /// The stop's SIGTERM → SIGKILL timeout (systemd's default 90 s).
    #[arg(long, default_value_t = handoff::STOP_TIMEOUT_MS)]
    pub handoff_stop_timeout_ms: u64,
}

impl PlayArgs {
    /// The JS build the agent labelled `label` (`a` / `b`) runs.
    pub fn js_dir_for(&self, label: &str) -> &Path {
        let side = match label {
            "a" => self.js_dir_a.as_deref(),
            "b" => self.js_dir_b.as_deref(),
            _ => None,
        };
        side.unwrap_or(&self.js_dir)
    }
}

pub const DEVICE_A: &str = "00000000-0000-4000-8000-00000000000a";
pub const DEVICE_B: &str = "00000000-0000-4000-8000-00000000000b";
const INTERVAL_MS: i64 = 1000;
/// Default `--flip-gap-ms`: half the tick interval, the nearest alignment.
pub const DEFAULT_FLIP_GAP_MS: i64 = 500;
/// Hysteresis on the flip: an alignment is given up only past flip gap +
/// this, so the new one starts ≥ 2·this inside the band. Without it a gap
/// hovering at 500 (JS-vs-JS, stagger 300) flipped back and forth every few
/// ticks (195214Z: 25 skips, 23 untolerated gps rows on torn pairs).
const FLIP_HYST_MS: i64 = 50;
/// A hold whose pair was not observed in full ends after this long.
const HOLD_TIMEOUT_MS: i64 = 2000;
const STALE_MS: i64 = 2500;
/// [`Gate::held_alive`]: an agent's first publish this long ago (two ticks).
const HELD_ALIVE_MS: i64 = 2000;
/// Post-hold settle caps ([`Gate::expect_resume`]), from the TS trust timing
/// (ts:src/agent/clock-trust.ts @09da38d, CLOCK_TRUST_DEFAULTS :66-70): an
/// NTP verdict is seen on the next 5 s probe poll (+ the exec), the grace
/// fallback on the first poll past 300 s, a GPS/NITZ step at once
/// (`noteStep`); the first post-trust publish is the next 1 s tick after.
/// (+ 3.5 s: the rig's NTP probe latency on p5b-clock-hold, `ntpProbeDelayMs`.)
pub const RESUME_NTP_CAP_MS: i64 = 5_000 + 3_500 + 1_000 + 1_500;
pub const RESUME_FALLBACK_CAP_MS: i64 = 5_000 + 1_000 + 2_000;
pub const RESUME_STEP_CAP_MS: i64 = 1_000 + 1_500;
/// The clock trust grace window (`graceMs`, 5 min) and how early before it
/// the fallback settle opens (the agent's clockTrust starts after its spawn).
pub const CLOCK_GRACE_MS: i64 = 300_000;
const FALLBACK_EARLY_MS: i64 = 500;
/// `NMEA_STALE_MS` (`gps.ts:15`): a gps group older than this is dropped.
const NMEA_STALE_MS: i64 = 10_000;
/// `REOPEN_SILENT_MS` (`gps.ts`): silence after which an agent reopens its port.
const REOPEN_SILENT_MS: i64 = 30_000;
/// Cut mode: how long after the lag agent's predicted tick its read is
/// assumed done (the JS `ts` is stamped after `await source.read()`).
const CUT_READ_MS: i64 = 150;
/// Cut mode: the per-tick drift a track's own history may predict (Node's
/// `setInterval` re-arms after each callback: 6-7 ms/tick measured in series
/// 5; tokio keeps its phase), and the most it may add in total.
const CUT_DRIFT_MAX_PER_TICK_MS: i64 = 20;
const CUT_DRIFT_MAX_MS: i64 = 300;
/// Cut mode: a predicted hold leaves at least this much of each second open.
const CUT_MIN_OPEN_MS: i64 = 200;
/// After a restore, barrier inputs (cli / mqtt_in / lte_file / conn) wait
/// until BOTH agents publish again + this settle (their retained config
/// delivery applied and reported) …
const POST_CUT_SETTLE_MS: i64 = 3_000;
/// … but never longer than the worst-case reconnect + 15 s after the
/// restore: both agents retry a fixed 2000 ms after every close (mqtt.js
/// `reconnectPeriod: 2000` ↔ xorgate-cloud backoff.rs `fixed`) and the proxy
/// refuses in-span attempts at once (no attempt hangs into the restore), so
/// the latest reconnect is 2 s + a connect after the restore.
const POST_CUT_MAX_MS: i64 = 2_000 + 15_000;
const PREFILL_MS: u64 = 1000;
/// A power-threshold crossing lands at least this far from both agents'
/// predicted 1 Hz ticks (`meta.rig.powerCrossings`).
const CROSSING_CLEAR_MS: i64 = 300;
/// The power-read fence never holds a `vcgencmd` answer longer than this.
const EXEC_FENCE_CAP_MS: i64 = 1_500;
/// P3a: the release loop's pick that means "do the handoff now".
const HANDOFF_PICK: usize = usize::MAX;

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
    /// The tick index of each entry of `bases`.
    base_ks: std::collections::VecDeque<i64>,
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
        self.base_ks.push_back(self.k);
        if self.bases.len() > Self::WINDOW {
            self.bases.pop_front();
            self.base_ks.pop_front();
        }
        self.k
    }

    /// Estimated tick time of tick index `k`.
    fn tick_at(&self, k: i64) -> Option<i64> {
        self.bases.iter().min().map(|b| b + k * INTERVAL_MS)
    }

    /// Phase drift per tick (ms, ≥ 0): the lower envelope of the newer half
    /// of the window minus that of the older half, over their index distance.
    fn drift_per_tick(&self) -> i64 {
        let n = self.bases.len();
        if n < 4 {
            return 0;
        }
        let h = n / 2;
        let min_of = |r: std::ops::Range<usize>| r.map(|i| self.bases[i]).min().unwrap_or(0);
        let mid = |r: std::ops::Range<usize>| {
            let len = r.len() as i64;
            r.map(|i| self.base_ks[i]).sum::<i64>() / len.max(1)
        };
        let dk = (mid(h..n) - mid(0..h)).max(1);
        ((min_of(h..n) - min_of(0..h)) / dk).clamp(0, CUT_DRIFT_MAX_PER_TICK_MS)
    }

    /// `(early, late)` estimate of tick `k`: early = the lower envelope (the
    /// hold's start); late = the NEWEST base (its read latency included) plus
    /// the measured drift over the ticks since the last observed one (the
    /// hold's end) — a cut extrapolates ~20 s, and Node drifts ~7 ms/tick.
    fn span_at(&self, k: i64) -> Option<(i64, i64)> {
        let early = self.tick_at(k)?;
        let last = *self.bases.back()?;
        let drift = (self.drift_per_tick() * (k - self.k).max(0)).min(CUT_DRIFT_MAX_MS);
        Some((early, (last + k * INTERVAL_MS + drift).max(early)))
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
    /// Until then the gate stays on PREDICTED ticks (series 5: Rust, then
    /// still in its old 1→60 s backoff 7 s after the restore, read an NMEA burst 5 ms in — GGA
    /// new, RMC old — because an unobserved agent left the gate open).
    restored_at: Option<i64>,
    /// When both agents had published again after the last restore.
    back_at: Option<i64>,
    /// Phase 5b: arrival of each agent's first outbound publish of any kind.
    first_publish: [Option<i64>; 2],
    /// Phase 5b: a held agent's expected resume, `(from, cap)` per agent
    /// ([`Gate::expect_resume`]).
    resume: [Option<(i64, i64)>; 2],
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
            back_at: None,
            first_publish: [None, None],
            resume: [None, None],
            pairs: Vec::new(),
        }
    }

    /// The broker path was restored at `now`: keep holding on predicted ticks
    /// until both agents' telemetry arrives again (each reconnects on its own
    /// 2 s retry phase, counted from its own close).
    pub fn on_restore(&mut self, now: i64) {
        self.restored_at = Some(now);
    }

    /// Phase 6 P3a: both agents were restarted at a handoff. Everything the
    /// gate learned about their ticks belongs to the stopped incarnations, so
    /// it starts over as at a boot (fresh tracks, no hold, no resume); the
    /// pairs so far are kept and a `{"handoff": now}` line marks the seam in
    /// `pairs.ndjson` (`load_run` reads only `a`/`b` and `skip` lines).
    pub fn on_handoff(&mut self, now: i64) {
        let mut fresh = Gate::new(self.guard_before).with_flip_gap(self.flip_gap);
        fresh.guard_after = self.guard_after;
        fresh.pairs = std::mem::take(&mut self.pairs);
        fresh.pairs.push(json!({ "handoff": now }));
        *self = fresh;
    }

    /// Is an agent still unobserved since the last restore?
    fn reconnecting(&mut self) -> bool {
        let Some(r) = self.restored_at else {
            return false;
        };
        if let [Some(a), Some(b)] = self.seen_at {
            if a >= r && b >= r {
                self.restored_at = None;
                self.back_at = Some(a.max(b));
                return false;
            }
        }
        true
    }

    /// Must a barrier input (cli / mqtt_in / lte_file / conn) still wait
    /// after a restore? Until both agents publish again + POST_CUT_SETTLE_MS
    /// (≤ POST_CUT_MAX_MS after the restore). Series 5 calibration 193432Z:
    /// the Rust agent, 30 s into its (since replaced) backoff, reconnected 50 ms after the
    /// tape's CLI edit, and its retained rev 2 delivery overwrote the edit
    /// before its 5 s settings watch saw it; mqtt.js (2 s) had applied rev 2
    /// 23 s earlier and reported the edit `local`.
    pub fn post_cut_hold(&mut self, now: i64) -> bool {
        if let Some(r) = self.restored_at {
            if now - r >= POST_CUT_MAX_MS {
                return false;
            }
            if self.reconnecting() {
                return true;
            }
        }
        self.back_at.is_some_and(|b| now < b + POST_CUT_SETTLE_MS)
    }

    /// The rig's `--flip-gap-ms`.
    pub fn with_flip_gap(mut self, ms: i64) -> Self {
        self.flip_gap = ms;
        self
    }

    /// Is `now` at least `ms` away (circular, mod the 1 s tick) from BOTH
    /// agents' predicted next tick? Unknown phase (< 2 ticks seen) = yes.
    pub fn mid_period(&self, now: i64, ms: i64) -> bool {
        self.track.iter().all(|t| match t.tick_at(t.k) {
            Some(tick) => {
                let ph = (now - tick).rem_euclid(INTERVAL_MS);
                ph.min(INTERVAL_MS - ph) >= ms
            }
            None => true,
        })
    }

    /// Has agent `idx` published telemetry at least once (booted, connected,
    /// ticking)? Both = the pre-5b NMEA/IMU delivery rule ([`inputs_ticking`]).
    pub fn ticking(&self, idx: usize) -> bool {
        self.track[idx].last_ts.is_some()
    }

    /// Any outbound publish of agent `idx` (report, status, telemetry …).
    pub fn on_publish(&mut self, idx: usize, now: i64) {
        self.first_publish[idx].get_or_insert(now);
    }

    /// Phase 5b (clock trust, X5): is agent `idx` running and connected while
    /// its TELEMETRY publish is held? The TS builds the payload (its gps
    /// collector opening the NMEA port lazily on that first tick) and only
    /// then returns at the trust gate (ts:src/worker.ts:1397-1411 @09da38d),
    /// so no telemetry reaches the broker and state.json is not re-flushed
    /// either; its config report / status do. An agent that published
    /// anything at least [`HELD_ALIVE_MS`] ago counts as ticking for input
    /// delivery (the caller still requires its NMEA port to be open). Without
    /// this the symmetric NMEA/IMU delivery waited for a telemetry publish the
    /// hold never lets happen: no GPS → no gps step → no trust (the p4-clock-*
    /// tapes deadlocked, sweep-A 2026-10-22).
    pub fn held_alive(&self, idx: usize, now: i64) -> bool {
        self.first_publish[idx].is_some_and(|t| now - t >= HELD_ALIVE_MS)
    }

    /// Is agent `idx` held by the clock trust gate (alive, publishing, but
    /// no telemetry yet)? Trust latches for the process
    /// (ts:src/agent/clock-trust.ts:1-30), so only the hold from boot exists.
    pub fn held(&self, idx: usize, now: i64) -> bool {
        !self.ticking(idx) && self.held_alive(idx, now)
    }

    /// Phase 5b POST-HOLD SETTLE (an input-pairing rule, not a tolerance):
    /// the held agent `idx` is about to resume at a trust instant the player
    /// can see — the tape's NTP flip, the agent's own GPS/NITZ step (its
    /// `xorgate-setclock` sudo call), or its 300 s grace fallback — but its
    /// tick phase is unobserved until its first telemetry publish. Barrier
    /// inputs (LTE file, cli, mqtt_in, conn) released in that gap can land
    /// between the two agents' reads of the first post-hold pair (clock-cap
    /// sweep-A: an LTE rewrite at 300.08 s, the fallback at 300 s). So from
    /// `from` until the agent's first telemetry publish after it, at most
    /// `cap` ms, barrier inputs wait — exactly like the post-cut hold while
    /// an agent reconnects. It only DELAYS inputs (≤ cap): no tick is paired,
    /// skipped or re-paired by it, and the diff compares every pair as
    /// before, so a resumed agent whose first tick is late or early still
    /// shows as an unpaired / mis-timed payload (tests
    /// `post_hold_settle_*`).
    pub fn expect_resume(&mut self, idx: usize, from: i64, cap: i64) {
        if self.resume[idx].is_none() {
            self.resume[idx] = Some((from, cap));
        }
    }

    /// Must a barrier input wait for a held agent's first post-trust tick?
    pub fn post_hold_hold(&mut self, now: i64) -> bool {
        let mut hold = false;
        for i in 0..2 {
            let Some((from, cap)) = self.resume[i] else {
                continue;
            };
            if now < from {
                continue;
            }
            let resumed = self.seen_at[i].is_some_and(|t| t >= from);
            if resumed || now - from >= cap {
                self.resume[i] = None;
            } else {
                hold = true;
            }
        }
        hold
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

    /// Is `now` between the predicted ticks of a pair (cut mode)? From the
    /// lead's early estimate − guard to the later late estimate + its read
    /// (series 5: the lower envelope alone, extrapolated through a 20 s cut,
    /// ran ~180 ms behind the drifting JS tick, so NMEA was released right at
    /// its read: 2, 0, 2, 0 new samples per tick against the Rust agent's 1).
    fn predicted_pair_window(&self, now: i64) -> bool {
        let first_k = |t: &TickTrack| -> Option<i64> {
            let base = *t.bases.iter().min()?;
            let from = now - self.guard_before - CUT_READ_MS - INTERVAL_MS - CUT_DRIFT_MAX_MS;
            Some((from - base).div_euclid(INTERVAL_MS) + 1)
        };
        let (Some(mut k0), Some(mut k1)) = (first_k(&self.track[0]), first_k(&self.track[1]))
        else {
            return false;
        };
        let (Some((n0, _)), Some((n1, _))) = (self.track[0].span_at(k0), self.track[1].span_at(k1))
        else {
            return false;
        };
        // Pair each tick with the other's nearest one.
        if n1 - n0 > INTERVAL_MS / 2 {
            k1 -= 1;
        } else if n0 - n1 > INTERVAL_MS / 2 {
            k0 -= 1;
        }
        for j in 0..4 {
            let (Some((ea, la)), Some((eb, lb))) =
                (self.track[0].span_at(k0 + j), self.track[1].span_at(k1 + j))
            else {
                return false;
            };
            let (l, g) = (ea.min(eb), ea.max(eb));
            let start = l - self.guard_before;
            let end = (la.max(lb) + CUT_READ_MS).min(start + INTERVAL_MS - CUT_MIN_OPEN_MS);
            if g - l <= self.flip_gap + FLIP_HYST_MS && now >= start && now <= end {
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
            if (0..=self.flip_gap + FLIP_HYST_MS).contains(&d_mid) && d_mid < (n[x] - n[y]).abs() {
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
        if n[lag] - n[lead] > self.flip_gap + FLIP_HYST_MS {
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
    /// Phase 4: Pi namespace dir + the SMS pty kept open (`p4.rs`).
    p4: p4::AgentP4,
    /// Phase 5b: this side's JS build (`PlayArgs::js_dir_for`).
    js_dir: PathBuf,
    /// Phase 5b: this side's ELM327 emulator (OBD tapes), stopped on drop.
    _obd_emu: Option<p5b::Emu>,
    /// Phase 6 P3a: `(spawn wall ms, kind)` of every incarnation started on
    /// this home (one without a handoff).
    incarnations: Vec<(i64, AgentKind)>,
}

/// The Rust agent's own re-recorded tape of incarnation `n` (P3a: a Rust
/// incarnation after a handoff gets `agent-tape.<n>.ndjson.gz`).
pub fn agent_tape_name(n: usize) -> String {
    if n == 0 {
        "agent-tape.ndjson.gz".into()
    } else {
        format!("agent-tape.{n}.ndjson.gz")
    }
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

/// The tape's first `i2cdetect` stdout, unless that first record is
/// `notFound` (then the shim is left off PATH instead, `first_i2c_absent`).
fn boot_i2c_answer(tape: &Tape) -> Option<&str> {
    tape.records.iter().find_map(|r| match &r.ev {
        TapeEvent::I2cdetect { stdout, not_found } => {
            Some(if *not_found { None } else { stdout.as_deref() })
        }
        _ => None,
    })?
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    fn prepare(
        label: &'static str,
        kind: AgentKind,
        device_id: &'static str,
        args: &PlayArgs,
        tape: &Tape,
        i2c_absent: bool,
        plan: &MediaPlan,
        p4plan: &p4::P4Plan,
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
        // The I2C bus as it was at boot (Phase 5b gate R-P5bG-3): the tape's
        // FIRST i2cdetect answer is what the recorded agent saw on its own boot
        // probe (bench tapes: t = 1.1–1.5 s), i.e. the bus state from power-on.
        // Released only at that t, an agent that probes earlier (the Rust
        // agent probes at ~1.08 s in the rig) got the shim's "no bus" error and,
        // like the TS (detection is not retried until a reader stream ends),
        // never streamed the IMU, while a slower JS start landed after the
        // release and did: a one-sided `metrics.imu` on every tick. Seed it
        // before the agents start; the tape-time release rewrites the same text.
        if let Some(out) = boot_i2c_answer(tape) {
            write_atomic(&state.join("i2cdetect.out"), out.as_bytes())?;
        }
        // PATH shims (copied so i2cdetect can be absent per agent).
        for entry in
            fs::read_dir(&args.shims).with_context(|| format!("{}", args.shims.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            if (i2c_absent && name == "i2cdetect")
                || (!args.with_rpicam && !plan.rpicam && name == "rpicam-hello")
                || name.to_str().is_some_and(|n| p4plan.skip_shim(n))
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
            media::kvs_manifest(plan, tape, device_id).to_string(),
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
        let mut env: Vec<(String, String)> = vec![
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
                    dir.join(agent_tape_name(0)).display().to_string(),
                )
            }),
        )
        .collect();
        // Phase 3 media side (media.rs): fake KVS master, rig CA for Node,
        // filler tree. Later entries replace earlier ones of the same key.
        for (k, v) in
            media::prepare_agent(plan, &work, &home, device_id, &args.certs, &args.media_dir)?
        {
            env.retain(|(ek, _)| *ek != k);
            env.push((k, v));
        }
        // Phase 4 (p4.rs): Pi namespace, SMS pty + fake modem, LED log, writer knob.
        let camrotate = args.camrotate.clone().or_else(|| {
            [
                PathBuf::from("/rig/system/xorgate-camrotate"),
                args.js_dir_for(label).join("system/xorgate-camrotate"),
            ]
            .into_iter()
            .find(|p| p.is_file())
        });
        // Phase 5b: the NTP probe's answer latency (timedatectl shim), from
        // `meta.rig.ntpProbeDelayMs` (p5b-clock-hold; see the shim).
        if let Some(ms) = tape
            .meta
            .rig
            .as_ref()
            .and_then(|r| r.get("ntpProbeDelayMs"))
            .and_then(Value::as_u64)
        {
            fs::write(
                state.join("ntp_delay"),
                format!("{:.3}", ms as f64 / 1000.0),
            )?;
        }
        let (mut p4_state, p4_env) = p4::prepare_agent(p4plan, &work, &dir, camrotate.as_deref())?;
        // Phase 5b (R-P3G-4): the agent's memory input is the tape's, bound
        // over /proc/meminfo in its own mount namespace (pi-ns.sh); the rig
        // VM's RAM (other agents' encoders, cargo builds) never reaches
        // `ram_usage`. Skipped when the rig cannot unshare (CI smoke).
        if meminfo_isolation(args) {
            let mi = work.join("meminfo");
            fs::write(&mi, RIG_MEMINFO)?;
            env.retain(|(k, _)| k != "RIG_MEMINFO");
            env.push(("RIG_MEMINFO".into(), mi.display().to_string()));
            if p4_state.pi_dir.is_none() {
                p4_state.pi_dir = Some(work.join("no-pi"));
            }
        }
        // Phase 5b WP5 (item 4): `disk_usage` (and the recording disk
        // manager's free-space probe of a root-fs path) reads the fixture, not
        // the shared Docker VM's disk that other containers and the run's own
        // recordings move (phase-3 gate D3 FINAL `.jsrs09`).
        for (k, v) in disk_isolation_env(args)? {
            env.retain(|(e, _)| *e != k);
            env.push((k, v));
        }
        for (k, v) in p4_env {
            env.retain(|(ek, _)| *ek != k);
            env.push((k, v));
        }
        // Phase 5b (p5b.rs): the OBD helper + emulator + bluetoothctl answers.
        let mut obd_emu = None;
        if let Some(obd) = &p4plan.obd {
            let js_py = args.js_dir_for(label).join("python");
            let src = match kind {
                AgentKind::Js => js_py,
                AgentKind::Rust => p5b::helper_source(&args.rs_python, &args.js_dir.join("python")),
            };
            let tools = args
                .shims
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let (emu, obd_env) = p5b::prepare_agent(
                obd,
                &work,
                &dir,
                &state,
                &args.fake_python,
                &src,
                &tools,
                (!args.no_gate)
                    .then(|| obd_gate_path(&args.work))
                    .as_deref(),
            )?;
            obd_emu = emu;
            for (k, v) in obd_env {
                env.retain(|(ek, _)| *ek != k);
                env.push((k, v));
            }
        }
        let agent = Self {
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
            p4: p4_state,
            js_dir: args.js_dir_for(label).to_path_buf(),
            _obd_emu: obd_emu,
            incarnations: Vec::new(),
        };
        agent.write_meta()?;
        Ok(agent)
    }

    /// `<label>/meta.json`. After a handoff (P3a) `kind` is the CURRENT
    /// incarnation's and `incarnations` lists every one with its spawn wall
    /// (`diff`: per-item kinds, R2 of the P3a design).
    fn write_meta(&self) -> Result<()> {
        fs::write(
            self.dir.join("meta.json"),
            serde_json::to_vec_pretty(&json!({
                "label": self.label, "kind": self.kind.as_str(), "deviceId": self.device_id,
                "jsDir": (self.kind == AgentKind::Js).then(|| self.js_dir.display().to_string()),
                "home": self.home, "ptyPath": self.pty_path,
                "env": self.env.iter().cloned().collect::<BTreeMap<_, _>>(),
                "incarnations": handoff::incarnations_json(&self.incarnations),
            }))?,
        )?;
        Ok(())
    }

    /// P3a: the next incarnation runs `kind` on the same home. Only the
    /// agent's own tape path depends on the kind (`media::prepare_agent`'s
    /// env does not; the OBD helper source and the Pi helper do, and a
    /// handoff refuses those tapes, `handoff::Plan::of`).
    fn switch_kind(&mut self, kind: AgentKind) {
        let n = self.incarnations.len();
        self.kind = kind;
        self.env.retain(|(k, _)| k != "XORGATE_RS_TAPE");
        if kind == AgentKind::Rust {
            self.env.push((
                "XORGATE_RS_TAPE".into(),
                self.dir.join(agent_tape_name(n)).display().to_string(),
            ));
        }
        self.pty_open_cache = None;
    }

    fn obs(&self) -> ObsAgent {
        ObsAgent {
            work: self.work.clone(),
            home: self.home.clone(),
            dir: self.dir.clone(),
        }
    }

    fn command(&self, args: &PlayArgs, argv: &[String], worker: bool) -> Command {
        let (program, mut full): (String, Vec<String>) = match self.kind {
            AgentKind::Js => {
                let entry = if worker {
                    "dist/worker.js"
                } else {
                    "dist/cli.js"
                };
                (
                    args.node.clone(),
                    vec![self.js_dir.join(entry).display().to_string()],
                )
            }
            AgentKind::Rust => (
                args.rs_bin.display().to_string(),
                if worker {
                    vec!["run".to_string()]
                } else {
                    vec![]
                },
            ),
        };
        full.extend(argv.iter().cloned());
        // Phase 4: a Pi-namespaced tape runs the agent in its own mount
        // namespace (same pid: `unshare` and pi-ns.sh exec).
        let (program, full) =
            p4::wrap_command(self.p4.pi_dir.as_deref(), &args.ns_script, &program, &full);
        let mut cmd = Command::new(program);
        cmd.args(full);
        cmd.env_clear();
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.current_dir(&self.home);
        cmd
    }

    fn start(&mut self, args: &PlayArgs) -> Result<()> {
        // P3a: a later incarnation APPENDS to the side's logs after a marker
        // line (`takeover.rs` reads the recovery lines after it).
        let n = self.incarnations.len();
        let w = wall_now_ms();
        let open = |name: &str| -> Result<File> {
            let p = self.dir.join(name);
            if n == 0 {
                return Ok(File::create(p)?);
            }
            let mut f = OpenOptions::new().create(true).append(true).open(p)?;
            writeln!(
                f,
                "{} incarnation {n} ({}) starts at {w}",
                crate::takeover::MARKER,
                self.kind.as_str()
            )?;
            Ok(f)
        };
        let (out, err) = (open("stdout.log")?, open("stderr.log")?);
        self.incarnations.push((w, self.kind));
        self.write_meta()?;
        let mut cmd = self.command(args, &[], true);
        cmd.stdout(Stdio::from(out));
        cmd.stderr(Stdio::from(err));
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

    /// The shims (`vcgencmd`, `rpicam-hello`) answer from these per-argv
    /// files: `.out` → stdout, `.err` → stderr (the tape's `stderr`, absent =
    /// none; review-wp5 S1), `.code` → exit code.
    fn exec(
        &self,
        argv: &[String],
        stdout: &str,
        stderr: Option<&str>,
        code: Option<i32>,
    ) -> Result<()> {
        let key = argv.join(" ").replace([' ', '/'], "_");
        let key = match key.split_once('_') {
            Some((tool, rest)) => format!("{tool}__{rest}"),
            None => key,
        };
        let base = self.state.join("exec").join(key);
        write_atomic(&base.with_extension("err"), stderr.unwrap_or("").as_bytes())?;
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
        // The agent's capture children got their SIGINT (stop = EOS, the last
        // fragment finalises) and may outlive the agent by the EOS: give the
        // group the capture stop grace (`stopAndWait(graceMs = 10_000)`,
        // video-capture.ts:516) before the SIGKILL, so how a child ENDED
        // (int vs kill, media_diff `children`) is the agent's doing, not a
        // race between the EOS and this kill on a loaded host (WP6: CI run
        // 39328644627 attempt 1, Rust-vs-Rust capture/cam0#1 int vs kill).
        let mut lingered_ms = 0u64;
        if let (Some(pg), true) = (pgid, graceful) {
            while lingered_ms < 10_000
                && nix::sys::signal::killpg(pg, None::<nix::sys::signal::Signal>).is_ok()
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
                lingered_ms += 50;
            }
        }
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
                "[rig] agent {} {} after SIGTERM in {} ms (its process group lingered {lingered_ms} ms)",
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

/// Phase 6 P3a: the handoff itself (`handoff.rs` module doc): signal every
/// member of both agents' cgroup-equivalent sets at once, wait for the whole
/// stop (SIGKILL at the timeout), snapshot each side's inherited state, wait
/// the restart gap, then start each side as `plan.then[i]` on its own home.
/// The caller freezes tape time across it.
async fn run_handoff(
    agents: &mut [Agent],
    plan: &handoff::Plan,
    args: &PlayArgs,
    plog: &dyn Fn(String),
) -> Result<crate::takeover::HandoffInfo> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let me = nix::unistd::getpgrp().as_raw();
    let before: Vec<AgentKind> = agents.iter().map(|a| a.kind).collect();
    let procs = handoff::scan_proc();
    let mut members: Vec<handoff::Members> = agents
        .iter()
        .map(|a| {
            let root = a
                .child
                .as_ref()
                .and_then(|c| c.id())
                .map_or(-1, |p| p as i32);
            handoff::Members::new(root, &procs, me)
        })
        .collect();
    let stop_w = wall_now_ms();
    let first = plan.signal.first();
    let mut signalled = [0usize; 2];
    for (i, (a, m)) in agents.iter().zip(&members).enumerate() {
        for pid in m.live(&procs) {
            let _ = kill(Pid::from_raw(pid), first);
            signalled[i] += 1;
            handoff::append_line(
                &a.dir.join("procs.ndjson"),
                &json!({"w": stop_w, "pid": pid, "k": "handoff", "sig": plan.signal.as_str()}),
            );
        }
    }
    let deadline = stop_w + plan.stop_timeout_ms as i64;
    let mut sigkilled = plan.signal == handoff::StopSignal::Kill;
    let mut killed_after = [0usize; 2];
    let mut stopped_w: [Option<i64>; 2] = [None, None];
    let mut exit: [Option<std::process::ExitStatus>; 2] = [None, None];
    loop {
        let now = wall_now_ms();
        for (i, a) in agents.iter_mut().enumerate() {
            if exit[i].is_none() {
                if let Some(Ok(Some(st))) = a.child.as_mut().map(|c| c.try_wait()) {
                    exit[i] = Some(st);
                }
            }
        }
        let procs = handoff::scan_proc();
        for (i, m) in members.iter_mut().enumerate() {
            m.refresh(&procs);
            if stopped_w[i].is_none() && m.live(&procs).is_empty() {
                stopped_w[i] = Some(now);
            }
        }
        if stopped_w.iter().all(Option::is_some) {
            break;
        }
        if !sigkilled && now >= deadline {
            sigkilled = true;
            for (i, (a, m)) in agents.iter().zip(&members).enumerate() {
                for pid in m.live(&procs) {
                    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                    killed_after[i] += 1;
                    handoff::append_line(
                        &a.dir.join("procs.ndjson"),
                        &json!({"w": now, "pid": pid, "k": "handoff", "sig": "kill-at-timeout"}),
                    );
                }
            }
            plog(format!(
                "handoff: SIGKILL at the {} ms stop timeout (A {}, B {} members left)",
                plan.stop_timeout_ms, killed_after[0], killed_after[1]
            ));
        }
        if now >= deadline.max(stop_w) + handoff::KILL_WAIT_MS as i64 {
            bail!(
                "handoff: members still alive {} ms after the SIGKILL",
                handoff::KILL_WAIT_MS
            );
        }
        tokio::time::sleep(Duration::from_millis(handoff::POLL_MS)).await;
    }
    let mut agent_exit = [String::new(), String::new()];
    for (i, a) in agents.iter_mut().enumerate() {
        if exit[i].is_none() {
            if let Some(c) = a.child.as_mut() {
                exit[i] = c.wait().await.ok();
            }
        }
        a.child = None;
        agent_exit[i] = handoff::exit_text(exit[i]);
    }
    let stopped = [
        stopped_w[0].unwrap_or(stop_w),
        stopped_w[1].unwrap_or(stop_w),
    ];
    for (i, a) in agents.iter().enumerate() {
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(a.dir.join("stderr.log"))
        {
            let _ = writeln!(
                f,
                "[rig] handoff stop ({}): agent {} {} ; {} member(s) signalled, all gone {} ms after the signal, {} SIGKILLed at the timeout",
                plan.signal.as_str(),
                a.label,
                agent_exit[i],
                signalled[i],
                stopped[i] - stop_w,
                killed_after[i]
            );
        }
        handoff::snapshot(&a.home, &a.dir, stopped[0].max(stopped[1]))?;
    }
    let restart_at = stopped[0].max(stopped[1]) + plan.gap_ms as i64;
    let left = restart_at - wall_now_ms();
    if left > 0 {
        tokio::time::sleep(Duration::from_millis(left as u64)).await;
    }
    let restart_w = wall_now_ms();
    for (i, a) in agents.iter_mut().enumerate() {
        a.switch_kind(plan.then[i]);
        a.start(args)?;
    }
    Ok(crate::takeover::HandoffInfo {
        at_ms: plan.at_ms,
        signal: plan.signal.as_str().into(),
        stop_w,
        stopped_w: stopped,
        restart_w,
        pause_ms: 0,
        before: [before[0].as_str().into(), before[1].as_str().into()],
        after: [plan.then[0].as_str().into(), plan.then[1].as_str().into()],
        signalled,
        killed_after_timeout: killed_after,
        agent_exit,
        spec: plan.spec.clone(),
    })
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
                    if let Ok(mut g) = caps.gate.lock() {
                        g.on_publish(idx, now);
                    }
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
    let first_i2c_absent = tape.records.iter().find_map(|r| match &r.ev {
        TapeEvent::I2cdetect { not_found, .. } => Some(*not_found),
        _ => None,
    }) == Some(true);
    let plan = MediaPlan::of(&tape);
    // Phase 5b WP4: the tape's external child exits, per side and per attempt
    // (media.rs `exit_plan`); only an exit no recorded spawn explains is still
    // re-enacted at tape time.
    let (exit_plan, unattributed_exits) = media::exit_plan(&tape);
    let unattributed_exits: BTreeSet<u64> = unattributed_exits.into_iter().collect();
    let p4plan = p4::P4Plan::of(&tape);
    // Phase 6 P3a: the handoff (if any) and what it cannot freeze.
    let handoff_plan = handoff::Plan::of(
        &tape,
        args.handoff_signal,
        args.handoff_at_ms,
        [args.a, args.b],
        [args.then_a, args.then_b],
        args.handoff_gap_ms,
        args.handoff_stop_timeout_ms,
        if !exit_plan.is_empty() {
            Some("per-attempt external child exits (media exit_plan)")
        } else if p4plan.pi.is_some() {
            Some("a Pi namespace (its DT writes run on the unfrozen t0)")
        } else if p4plan.obd.is_some() {
            Some("OBD (the helper source depends on the kind)")
        } else {
            None
        },
    )?;
    // A calibration = the same kind sequence on both sides.
    let calibration =
        args.a == args.b && handoff_plan.as_ref().is_none_or(|h| h.then[0] == h.then[1]);
    if p4plan.pi.is_some() {
        // The bind-mount target of each agent's namespace (pi-ns.sh).
        fs::create_dir_all("/boot/firmware").context("mkdir /boot/firmware (rig container)")?;
    }
    let mut agents = vec![
        Agent::prepare(
            "a",
            args.a,
            DEVICE_A,
            &args,
            &tape,
            first_i2c_absent,
            &plan,
            &p4plan,
        )?,
        Agent::prepare(
            "b",
            args.b,
            DEVICE_B,
            &args,
            &tape,
            first_i2c_absent,
            &plan,
            &p4plan,
        )?,
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

    // Phase 3: fake IoT credentials + S3 (aws.rs), trusted via the rig CA.
    let aws_fake = aws::AwsFake::new(
        agents
            .iter()
            .map(|a| (a.device_id.to_string(), a.label.to_string(), a.dir.clone()))
            .collect(),
        aws::faults_of(tape.meta.rig.as_ref()),
        aws::holds_of(tape.meta.rig.as_ref()),
        &args.out,
    )?;
    if plan.full_manifest {
        match aws::trust_rig_ca(&args.certs) {
            Ok(m) => plog(format!("rig CA added to the system store: {m}")),
            Err(e) => plog(format!(
                "WARNING: rig CA not installed ({e:#}); Rust uploads will fail TLS"
            )),
        }
        let hosts: Vec<String> = plan
            .buckets
            .iter()
            .map(|b| format!("{b}.s3.{}.amazonaws.com", aws::RIG_REGION))
            .chain(std::iter::once(aws::RIG_CREDS_HOST.to_string()))
            .collect();
        if let Err(e) = aws::add_hosts(&hosts) {
            plog(format!("WARNING: /etc/hosts not updated ({e:#})"));
        }
        let (fake, listen, certs) = (
            aws_fake.clone(),
            args.aws_listen.clone(),
            args.certs.clone(),
        );
        let alog = log.clone();
        tokio::spawn(async move {
            if let Err(e) = aws::serve(fake, listen, certs).await {
                if let Ok(mut f) = alog.lock() {
                    let _ = writeln!(f, "{} aws fake stopped: {e:#}", wall_now_ms());
                }
            }
        });
    }

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
                | TapeEvent::NtpProbe { .. }
        ) {
            apply_state(&agents, &r.ev, shift_s)?;
            prefilled[i] = true;
        }
    }
    // Phase 4: a recorded `sudo` record is the ANSWER the device gave; the
    // shim needs it before the call, so every one is applied up front.
    for r in &tape.records {
        if matches!(r.ev, TapeEvent::Sudo { .. }) {
            apply_state(&agents, &r.ev, shift_s)?;
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
    let mut t0 = tokio::time::Instant::now();
    let play_start_wall = wall_now_ms();
    let mut spawn_wall = [0i64; 2];
    for (i, a) in agents.iter_mut().enumerate() {
        if i == 1 && args.stagger_ms > 0 {
            tokio::time::sleep(Duration::from_millis(args.stagger_ms)).await;
        }
        if let Ok(mut w) = caps.watch.lock() {
            w.on_spawn(i, wall_now_ms());
        }
        spawn_wall[i] = wall_now_ms();
        a.start(&args)?;
    }
    // Phase 5b post-hold settle triggers ([`Gate::expect_resume`]): the grace
    // fallback (spawn + 300 s) and each agent's own clock step (the sudo
    // shim's `xorgate-setclock` line in `<work>/sudo.log`).
    let sudo_logs: Vec<PathBuf> = agents.iter().map(|a| a.work.join("sudo.log")).collect();
    let mut resume_watch = ResumeWatch::new(spawn_wall, sudo_logs.clone());
    plog(format!(
        "agents started (B staggered {} ms); prefilled {idx} records",
        args.stagger_ms
    ));
    {
        let dts: Vec<PathBuf> = agents
            .iter()
            .filter_map(|a| a.p4.pi_dir.as_ref().map(|d| d.join("dt")))
            .collect();
        let dlog = log.clone();
        tokio::spawn(p4::run_dt_writes(p4plan.clone(), dts, t0, move |m| {
            if let Ok(mut f) = dlog.lock() {
                let _ = writeln!(f, "{} {m}", wall_now_ms());
            }
        }));
    }
    if let Ok(mut g) = aws_fake.start_wall.lock() {
        *g = play_start_wall;
    }
    let obs_stop = CancellationToken::new();
    let obs_task = tokio::spawn(media::observe(
        agents.iter().map(Agent::obs).collect(),
        args.out.clone(),
        obs_stop.clone(),
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
    // Evidence: when each record was actually released (vs the tape's due
    // time) — the only record of the hold gate's effect on line inputs.
    let mut releases = std::io::BufWriter::new(File::create(args.out.join("releases.ndjson"))?);
    let cutoff = args.max_s.map(|s| s * 1000).unwrap_or(u64::MAX);
    // The per-attempt exits run on their own 2 ms task (not the release loop,
    // whose passes can block for ~100 ms on a release): each side's kill lands
    // within a few ms of its own spawn + the tape offset, also in the settle.
    let exit_task = (!exit_plan.is_empty()).then(|| {
        let obs: Vec<media::ObsAgent> = agents.iter().map(Agent::obs).collect();
        let (plan, xp, stop) = (plan.clone(), exit_plan.clone(), obs_stop.clone());
        tokio::spawn(async move {
            let mut runners: Vec<media::ExitRunner> =
                obs.iter().map(|_| media::ExitRunner::default()).collect();
            while !stop.is_cancelled() {
                let now = wall_now_ms();
                for (a, r) in obs.iter().zip(runners.iter_mut()) {
                    r.poll(a, &plan, &xp, now);
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    });
    // Phase 5b: the OBD helpers' stdout follows the hold gate (obd-gate.py).
    let has_obd = !args.no_gate && p5b::ObdPlan::of(&tape).is_some();
    let mut obd_gate = ObdGate::new(obd_gate_path(&args.work));
    let obd_bar = obd_gate_path(&args.work).with_file_name("obd-barrier");
    let mut obd_cap_log = ObdCapLog::default();

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
    // REPORT FENCE (`watch.rs` FENCE_WINDOW_MS): an LTE rewrite due inside a
    // report trigger's debounce window waits for BOTH agents' report.
    let fenced_at = |ev: &TapeEvent, due: i64, now: i64| -> Option<bool> {
        if args.no_gate {
            return None;
        }
        let (input, _) = watch::gated(ev)?;
        caps.watch.lock().ok()?.fence(input, due, now)
    };
    let mut fenced: BTreeSet<usize> = BTreeSet::new();
    // Barrier inputs wait out both agents' reconnect after a cut.
    let post_cut = |now: i64| {
        !args.no_gate
            && caps
                .gate
                .lock()
                .map(|mut g| g.post_cut_hold(now) | g.post_hold_hold(now))
                .unwrap_or(false)
    };
    // POWER CROSSINGS (`meta.rig.powerCrossings`, synth exec-rail /
    // exec-thermal): a record that moves a power input across a shed /
    // restore threshold is released mid-period — ≥ CROSSING_CLEAR_MS from
    // both agents' predicted 1 Hz ticks (held ≤ 1 s, then released).
    let crossing_ts: BTreeSet<u64> = tape
        .meta
        .rig
        .as_ref()
        .and_then(|r| r.get("powerCrossings"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .collect();
    // POWER-READ FENCE (CI smoke flake, runs 39311147277 #1/#2): every
    // `vcgencmd` answer (pmic rail, throttled bits — the system group's taped
    // power inputs) waits in `exec_q` (order kept, only those records wait)
    // while one agent has READ those answers (inotify open of
    // `<state>/exec/vcgencmd__*.out`, watch.rs `exec_holds`) and the other
    // has not read since — a release then would split that read pair — and
    // while a report trigger's debounce window is open (the report fence,
    // `fence`). Bounded (EXEC_FENCE_CAP_MS) and logged.
    let mut exec_q: std::collections::VecDeque<(usize, i64)> = Default::default();
    let is_power = |ev: &TapeEvent| matches!(ev, TapeEvent::Exec { argv, .. } if argv.first().map(String::as_str) == Some("vcgencmd"));
    let mid_now = |now: i64| {
        caps.gate
            .lock()
            .map(|g| g.mid_period(now, CROSSING_CLEAR_MS))
            .unwrap_or(true)
    };
    let mut pending: std::collections::VecDeque<(usize, i64)> = Default::default();
    let mut next = 0usize;
    let n_rec = tape.records.len();
    let mut handoff_info: Option<crate::takeover::HandoffInfo> = None;
    loop {
        // (record index, wall ms it fell due, released by a hold timeout)
        let picked: Option<(usize, i64, bool)> = loop {
            if !args.no_gate {
                resume_watch.poll(&caps.gate, wall_now_ms(), &plog);
            }
            // P3a: the handoff falls due at its tape instant (before any
            // record due at or after it is released).
            if let Some(h) = handoff_plan.as_ref().filter(|_| handoff_info.is_none()) {
                let at = t0 + Duration::from_millis((h.at_ms as f64 / args.speed) as u64);
                if tokio::time::Instant::now() >= at {
                    break Some((HANDOFF_PICK, 0, false));
                }
            }
            while next < n_rec && prefilled[next] {
                next += 1;
            }
            if next < n_rec && tape.records[next].t > cutoff {
                next = n_rec;
            }
            if next >= n_rec && pending.is_empty() && exec_q.is_empty() {
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
            let open_now = tel_open(now);
            if has_obd {
                for line in obd_cap_log.released(&obd_bar, now) {
                    plog(line);
                }
                // Closed inside a tick pair AND while a config-report pair is
                // open (a report build reads `effective.obd` at each agent's
                // own instant: stepC p5b-obd-config.bar.* `probing` vs `live`).
                let report_pair = caps
                    .watch
                    .lock()
                    .map(|w| w.report_pair_open(now))
                    .unwrap_or(false);
                obd_gate.write(open_now && !report_pair, now);
            }
            if let Some(&(ci, since)) = exec_q.front() {
                let r = &tape.records[ci];
                let crossing = crossing_ts.contains(&r.t);
                let mid = !crossing || mid_now(now);
                let (read_split, report) = caps
                    .watch
                    .lock()
                    .map(|w| (w.exec_holds(now), w.report_open(since, now)))
                    .unwrap_or((false, false));
                let waited = now - since;
                let capped = waited >= EXEC_FENCE_CAP_MS;
                if ((mid && !read_split && !report) || capped) && tel_open(now) {
                    exec_q.pop_front();
                    if waited >= 50 {
                        plog(format!(
                            "power-read fence: exec t={} released {waited} ms after due{}{}",
                            r.t,
                            if crossing { " (crossing)" } else { "" },
                            if capped { " (CAPPED)" } else { "" }
                        ));
                    }
                    if let Ok(mut w) = caps.watch.lock() {
                        w.note_exec_fence(since, now, capped);
                    }
                    break Some((ci, since, false));
                }
            }
            if let Some(&(pi, due_w)) = pending.front() {
                let ev = &tape.records[pi].ev;
                let fence = fenced_at(ev, due_w, now) == Some(true);
                if fence {
                    fenced.insert(pi);
                }
                let held =
                    watch_holds(ev, now) || fence || (has_obd && obd_relays_holding(&obd_bar, now));
                let timeout = held && now - due_w > watch::MAX_HOLD_MS;
                if (!held || timeout) && !post_cut(now) && tel_open(now) {
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
                        && (!pending.is_empty()
                            || (has_obd && obd_relays_holding(&obd_bar, now))
                            || watch_holds(&r.ev, now)
                            || post_cut(now)
                            || fenced_at(&r.ev, now, now) == Some(true))
                    {
                        pending.push_back((next, now));
                        next += 1;
                        continue;
                    }
                    if !args.no_gate && is_power(&r.ev) {
                        // Only the power answers wait (in order); the other
                        // inputs keep their timing (exec is not a barrier).
                        exec_q.push_back((next, now));
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
        if let (true, Some(h)) = (i == HANDOFF_PICK, handoff_plan.as_ref()) {
            // P3a: stop like systemd, snapshot, restart; then FREEZE tape
            // time across the pause: t0 and the fake S3's fault origin move
            // by it, so tape[h..] keeps its designed timing relative to the
            // restart. The hold gate and the watch gate restart as at a boot.
            let due_at = t0 + Duration::from_millis((h.at_ms as f64 / args.speed) as u64);
            plog(format!(
                "handoff ({}) at t={}: A {} → {}, B {} → {}",
                h.signal.as_str(),
                h.at_ms,
                args.a.as_str(),
                h.then[0].as_str(),
                args.b.as_str(),
                h.then[1].as_str()
            ));
            let mut info = run_handoff(&mut agents, h, &args, &plog).await?;
            let pause = tokio::time::Instant::now().saturating_duration_since(due_at);
            t0 += pause;
            info.pause_ms = pause.as_millis() as i64;
            if let Ok(mut g) = aws_fake.start_wall.lock() {
                *g += info.pause_ms;
            }
            if let Ok(mut g) = caps.gate.lock() {
                g.on_handoff(wall_now_ms());
            }
            if let Ok(mut w) = caps.watch.lock() {
                for k in 0..agents.len() {
                    w.on_restart(k, info.restart_w);
                }
            }
            resume_watch = ResumeWatch::new([info.restart_w; 2], sudo_logs.clone());
            fs::write(
                args.out.join("handoff.json"),
                serde_json::to_vec_pretty(&info)?,
            )?;
            plog(format!(
                "handoff done: stopped A {} B {} ms after the signal, restart {}, tape paused {} ms",
                info.stopped_w[0] - info.stop_w,
                info.stopped_w[1] - info.stop_w,
                info.restart_w,
                info.pause_ms
            ));
            handoff_info = Some(info);
            continue;
        }
        let r = &tape.records[i];
        if timeout {
            plog(format!("watch gate: hold timeout at t={}", r.t));
        }
        if let Some((input, _)) = watch::gated(&r.ev) {
            if let Ok(mut w) = caps.watch.lock() {
                let at = wall_now_ms();
                w.note_release(input, due_w, at, timeout);
                if fenced.remove(&i) {
                    let capped = w.fence(input, due_w, at).is_some();
                    w.note_fenced(due_w, at, capped);
                    plog(format!(
                        "report fence: lte_file t={} held {} ms{}",
                        r.t,
                        at - due_w,
                        if capped {
                            " (CAPPED: a report never came)"
                        } else {
                            ""
                        }
                    ));
                }
            }
        }
        if matches!(r.ev, TapeEvent::MqttIn { .. } | TapeEvent::Cli { .. }) {
            if let Ok(mut w) = caps.watch.lock() {
                w.on_trigger(wall_now_ms());
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
        let _ = writeln!(
            releases,
            "{}",
            json!({"w": now, "due": due_w, "t": r.t, "k": r.ev.kind()})
        );
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
                let ticking = inputs_ticking(&caps.gate, now);
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
                let ticking = inputs_ticking(&caps.gate, now);
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
            // A child the agent did NOT signal exited on the recording device
            // (crash, EOF): re-enacted on both agents at tape time (media.rs).
            TapeEvent::ChildExit(x) if x.external => {
                transitions.push(now);
                if unattributed_exits.contains(&r.t) {
                    for a in &agents {
                        let rec = media::external_exit(&a.obs(), &plan, x);
                        plog(format!("external child exit at t={}: {}", r.t, rec));
                    }
                }
            }
            // Phase 3 output kinds: compared by the diff, not replayed.
            TapeEvent::ChildSpawn(_)
            | TapeEvent::ChildLine { .. }
            | TapeEvent::ChildSignal { .. }
            | TapeEvent::ChildExit(_)
            | TapeEvent::FsOut { .. }
            | TapeEvent::S3Put(_)
            | TapeEvent::Shm { .. } => {}
            // Phase 4 kinds (I9): known to the reader, not replayed yet — the
            // shims (fake-modem pty, pinctrl, sudo, timedatectl) consume them
            // from WP6 step 2 on. Tolerated so a Phase 4 tape still plays.
            // Phase 4: the NTP verdict is an input (timedatectl shim).
            TapeEvent::NtpProbe { synced } => {
                transitions.push(now);
                apply_state(&agents, &r.ev, shift_s)?;
                if *synced {
                    note_resume(&caps.gate, now, RESUME_NTP_CAP_MS, &plog, "ntp");
                }
            }
            // Phase 4 outputs / answers: the fake modem answers from the
            // transcript, the shims log `gpio`, `sudo` answers were applied
            // up front.
            TapeEvent::AtTx { .. }
            | TapeEvent::AtRx { .. }
            | TapeEvent::Gpio { .. }
            | TapeEvent::Sudo { .. } => {}
        }
    }
    if let Some(s) = closed_since.take() {
        closed_spans.push((s, wall_now_ms()));
        if let Ok(mut g) = cut.lock() {
            *g = Some(CancellationToken::new());
        }
    }
    // The settle has no gate: the relays fall back to open (a missing file).
    obd_gate.clear();
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
    // Children outlive their agent by up to a moment (SIGINT → EOS): one more
    // observer pass sees them gone, then the final media copies.
    tokio::time::sleep(Duration::from_millis(500)).await;
    obs_stop.cancel();
    let _ = obs_task.await;
    if let Some(t) = exit_task {
        let _ = t.await;
    }
    for a in &agents {
        if let Err(e) = media::finish(&a.obs()) {
            plog(format!("agent {}: media finish failed: {e:#}", a.label));
        }
    }
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
    if handoff_plan.is_some() && handoff_info.is_none() {
        bail!("the handoff never fell due (--max-s cut the tape before atMs?)");
    }
    let info = PlayInfo {
        handoff: handoff_info,
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
        releases: Vec::new(),
        nmea_resumes,
        no_gate: args.no_gate,
        disk_source: disk_source(&args).into(),
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

/// The fixed `/proc/meminfo` every agent reads (R-P3G-4): a CM5 8 GB board,
/// `MemAvailable` constant → `ram_usage` 21.2 % on both sides, exactly. The
/// TS and the Rust collector both compute (MemTotal − MemAvailable) / MemTotal
/// (ts:src/hardware/system.ts:86-98, rs xorgate-hal system.rs `read_memory`).
pub const RIG_MEMINFO: &str = "MemTotal:        8245360 kB\nMemFree:         5990000 kB\nMemAvailable:    6500000 kB\nBuffers:           80000 kB\nCached:           600000 kB\nSwapCached:            0 kB\nSwapTotal:        204796 kB\nSwapFree:         204796 kB\n";

/// The fixed root filesystem every agent's `disk_usage` reads (Phase 5b WP5
/// item 4): `f_blocks:f_bfree:f_bavail:f_frsize` = 30 GB, 19.2 % used (df's
/// `Used` = blocks − bfree, as both agents compute it), 5 % reserved.
pub const RIG_STATVFS_ROOT: &str = "7500000:6060000:5685000:4096";

/// `LD_PRELOAD` + `RIG_STATVFS_ROOT` for an agent; nothing under
/// `--host-disk`. A shim that was not built is an ERROR (review-wp5 R-WP5-4:
/// a silent fallback made a non-isolated run look like an isolated one).
fn disk_isolation_env(args: &PlayArgs) -> Result<Vec<(String, String)>> {
    if args.host_disk {
        return Ok(Vec::new());
    }
    if !args.statvfs_shim.is_file() {
        bail!(
            "{} missing — run tools/replay/build-bins.sh, or pass --host-disk to read the HOST disk on purpose",
            args.statvfs_shim.display()
        );
    }
    Ok(vec![
        ("LD_PRELOAD".into(), args.statvfs_shim.display().to_string()),
        ("RIG_STATVFS_ROOT".into(), RIG_STATVFS_ROOT.into()),
    ])
}

/// `play.json` `diskSource`: what every agent's `disk_usage` read.
fn disk_source(args: &PlayArgs) -> &'static str {
    if args.host_disk {
        "host"
    } else {
        "shim"
    }
}

/// Is the per-agent meminfo bind usable here (not `--host-meminfo`, and the
/// rig may `unshare -m`)? Probed once per process.
fn meminfo_isolation(args: &PlayArgs) -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    !args.host_meminfo
        && *OK.get_or_init(|| {
            std::process::Command::new("unshare")
                .args(["-m", "--propagation", "private", "--", "true"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
}

/// May NMEA / IMU lines flow (both agents booted and ticking)? An agent
/// counts once it has published telemetry, or — while its telemetry is held
/// by the clock trust gate — once it has been publishing anything for two
/// ticks ([`Gate::held_alive`]). Both publishing telemetry = the pre-5b rule.
/// The fallback / clock-step triggers of the post-hold settle, polled from
/// the release loop (every 200 ms at most).
struct ResumeWatch {
    spawn_wall: [i64; 2],
    sudo_logs: Vec<PathBuf>,
    setclocks: [usize; 2],
    fallback_done: [bool; 2],
    last: i64,
}

impl ResumeWatch {
    fn new(spawn_wall: [i64; 2], sudo_logs: Vec<PathBuf>) -> Self {
        Self {
            spawn_wall,
            sudo_logs,
            setclocks: [0, 0],
            fallback_done: [false, false],
            last: 0,
        }
    }

    fn poll(&mut self, gate: &Mutex<Gate>, now: i64, plog: &dyn Fn(String)) {
        if now - self.last < 200 {
            return;
        }
        self.last = now;
        let Ok(mut g) = gate.lock() else { return };
        for i in 0..2 {
            if !g.held(i, now) {
                continue;
            }
            // A GPS/NITZ step = trust at once (`noteStep`).
            let n = fs::read_to_string(&self.sudo_logs[i])
                .map(|t| t.lines().filter(|l| l.contains("xorgate-setclock")).count())
                .unwrap_or(0);
            if n > self.setclocks[i] {
                self.setclocks[i] = n;
                if g.resume[i].is_none() {
                    g.expect_resume(i, now, RESUME_STEP_CAP_MS);
                    plog(format!("post-hold settle: agent {i} (clock step) from {now}, cap {RESUME_STEP_CAP_MS} ms"));
                }
            }
            // The grace fallback: the first 5 s poll past 300 s after start.
            let from = self.spawn_wall[i] + CLOCK_GRACE_MS - FALLBACK_EARLY_MS;
            if !self.fallback_done[i] && now >= from {
                self.fallback_done[i] = true;
                if g.resume[i].is_none() {
                    g.expect_resume(i, from, RESUME_FALLBACK_CAP_MS);
                    plog(format!("post-hold settle: agent {i} (grace fallback) from {from}, cap {RESUME_FALLBACK_CAP_MS} ms"));
                }
            }
        }
    }
}

/// Arm the post-hold settle for every agent the clock trust gate holds.
fn note_resume(gate: &Mutex<Gate>, now: i64, cap: i64, plog: &dyn Fn(String), why: &str) {
    if let Ok(mut g) = gate.lock() {
        for i in 0..2 {
            if g.held(i, now) && g.resume[i].is_none() {
                g.expect_resume(i, now, cap);
                plog(format!(
                    "post-hold settle: agent {i} ({why}) from {now}, cap {cap} ms"
                ));
            }
        }
    }
}

/// Phase 5b (orchestrator ruling 2026-10-23): where the player publishes the
/// hold gate's state for the OBD relays (`tools/replay/obd-gate.py`).
fn obd_gate_path(work: &Path) -> PathBuf {
    work.join("obd-gate")
}

/// Review-wp4 R7-1: the most a barrier input waits for the OBD relays' held
/// helper lines (a relay that died with its flag set cannot wedge the tape).
const OBD_FLUSH_CAP_MS: i64 = 2_000;

/// Does an OBD relay still HOLD a helper line (obd-gate.py `_held`: the
/// `<side>.held` flag in the shared barrier dir, its content the wall second
/// the hold began)? Then no barrier input (config / command / cli / LTE file /
/// conn) is released: at a gate opening the held helper lines reach both
/// agents first, in the same order on both sides. A flag older than
/// OBD_FLUSH_CAP_MS no longer holds.
fn obd_relays_holding(bar: &Path, now: i64) -> bool {
    obd_held_flags(bar)
        .iter()
        .any(|(_, since)| now - since <= OBD_FLUSH_CAP_MS)
}

/// The relays' `<side>.held` flags present now: `(side, hold start in wall ms)`
/// (an unparseable flag is not a flag).
fn obd_held_flags(bar: &Path) -> Vec<(&'static str, i64)> {
    ["a", "b"]
        .into_iter()
        .filter_map(|side| {
            let t = fs::read_to_string(bar.join(format!("{side}.held"))).ok()?;
            let since = t.trim().parse::<f64>().ok()?;
            Some((side, (since * 1000.0) as i64))
        })
        .collect()
}

/// Review-wp4 NIT (Phase 5b WP5): a flag still present but older than
/// [`OBD_FLUSH_CAP_MS`] no longer holds the barrier inputs — the cap released
/// them. Logged ONCE per flag (side + hold start) so such a release is never
/// silent; a cap release only happens when a relay died with its flag set or
/// a barrier wait already went past the 400 ms the diff tolerates.
#[derive(Default)]
struct ObdCapLog {
    logged: std::collections::BTreeSet<(&'static str, i64)>,
}

impl ObdCapLog {
    /// The lines to log for flags the cap releases at `now` (new ones only).
    fn released(&mut self, bar: &Path, now: i64) -> Vec<String> {
        obd_held_flags(bar)
            .into_iter()
            .filter(|(_, since)| now - since > OBD_FLUSH_CAP_MS)
            .filter(|k| self.logged.insert(*k))
            .map(|(side, since)| {
                format!(
                    "obd relay {side}: held flag from {since} is {} ms old (> {OBD_FLUSH_CAP_MS} ms cap) — barrier inputs released past it",
                    now - since
                )
            })
            .collect()
    }
}

/// The OBD relays' gate file: `1` open / `0` closed, rewritten on every change
/// and at least every [`OBD_GATE_BEAT_MS`] (a relay treats a file older than
/// 1.5 s, or none, as OPEN, so a stopped player never wedges a helper).
struct ObdGate {
    path: PathBuf,
    last: Option<(bool, i64)>,
    /// Review-wp4 R7-1: the number of closed → open transitions, written
    /// after the state (`1 <epoch>`), so the relays release the lines one
    /// opening freed at ONE shared GO (obd-gate.py `wait_open_line`).
    epoch: u64,
}

const OBD_GATE_BEAT_MS: i64 = 500;

impl ObdGate {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            last: None,
            epoch: 0,
        }
    }

    fn write(&mut self, open: bool, now: i64) {
        if self
            .last
            .is_some_and(|(o, at)| o == open && now - at < OBD_GATE_BEAT_MS)
        {
            return;
        }
        if open && self.last.is_none_or(|(o, _)| !o) {
            self.epoch += 1;
        }
        let tmp = self.path.with_extension("tmp");
        if fs::write(&tmp, format!("{} {}", u8::from(open), self.epoch)).is_ok()
            && fs::rename(&tmp, &self.path).is_ok()
        {
            self.last = Some((open, now));
        }
    }

    fn clear(&mut self) {
        let _ = fs::remove_file(&self.path);
        self.last = None;
    }
}

fn inputs_ticking(gate: &Mutex<Gate>, now: i64) -> bool {
    let Ok(g) = gate.lock() else { return true };
    (0..2).all(|i| g.ticking(i) || g.held_alive(i, now))
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
            TapeEvent::Exec {
                argv,
                stdout,
                code,
                stderr,
            } => a.exec(argv, stdout, stderr.as_deref(), *code)?,
            TapeEvent::I2cdetect { stdout, .. } => a.i2c(stdout.as_deref())?,
            TapeEvent::Dt { .. } => {
                // /proc/device-tree is a fixed path in both agents and cannot
                // be redirected in a container: psu_max_current is absent on
                // both sides of the rig (README "Limits").
            }
            // Phase 4: the timedatectl shim's NTPSynchronized answer.
            TapeEvent::NtpProbe { synced } => write_atomic(
                &a.state.join("ntp"),
                if *synced { b"yes\n" } else { b"no\n" },
            )?,
            // Phase 4: the sudo shim's answer for that helper.
            TapeEvent::Sudo {
                argv,
                code,
                stdout,
                stderr,
                ..
            } => {
                let helper = argv
                    .iter()
                    .skip(1)
                    .find(|x| !x.starts_with('-'))
                    .and_then(|x| x.rsplit('/').next())
                    .unwrap_or("sudo");
                let dir = a.state.join("sudo");
                fs::create_dir_all(&dir)?;
                let base = dir.join(helper);
                write_atomic(
                    &base.with_extension("out"),
                    stdout.as_deref().unwrap_or("").as_bytes(),
                )?;
                write_atomic(
                    &base.with_extension("err"),
                    stderr.as_deref().unwrap_or("").as_bytes(),
                )?;
                write_atomic(
                    &base.with_extension("code"),
                    code.unwrap_or(1).to_string().as_bytes(),
                )?;
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
    fn boot_i2c_answer_is_the_first_i2cdetect_record_only() {
        use xorgate_hal::tape::TapeRecord;
        let rec = |t: u64, ev: TapeEvent| TapeRecord { t, w: t as i64, ev };
        let mut tape = Tape {
            meta: xorgate_hal::tape::TapeMeta::new("d", "b", 0),
            records: vec![],
            truncated: false,
        };
        assert_eq!(
            boot_i2c_answer(&tape),
            None,
            "no i2cdetect record: nothing seeded"
        );
        tape.records = vec![
            rec(
                1_396,
                TapeEvent::I2cdetect {
                    stdout: Some("40: -- 4a\n".into()),
                    not_found: false,
                },
            ),
            rec(
                276_394,
                TapeEvent::I2cdetect {
                    stdout: Some("40: --\n".into()),
                    not_found: false,
                },
            ),
        ];
        assert_eq!(
            boot_i2c_answer(&tape),
            Some("40: -- 4a\n"),
            "the FIRST answer, not a later one"
        );
        tape.records[0] = rec(
            1_000,
            TapeEvent::I2cdetect {
                stdout: None,
                not_found: true,
            },
        );
        assert_eq!(
            boot_i2c_answer(&tape),
            None,
            "first notFound: the shim is absent, nothing seeded"
        );
        tape.records[0] = rec(
            1_000,
            TapeEvent::I2cdetect {
                stdout: None,
                not_found: false,
            },
        );
        assert_eq!(
            boot_i2c_answer(&tape),
            None,
            "first record without stdout: nothing seeded"
        );
    }

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

    /// P3a R3 (rig rule, NOT a tolerance): at a handoff the gate forgets the
    /// stopped incarnations' tick phases and behaves exactly like a fresh
    /// gate for the restarted ones, keeping the pairs so far + a seam marker.
    /// NEGATIVE: it is not left open — the new pair is held like a boot's.
    /// The gate only ever DELAYS inputs; the diff compares every observed
    /// payload pair and lists every unpaired tick, so R3 cannot hide a
    /// payload difference.
    #[test]
    fn p3a_r3_handoff_restarts_the_gate_like_a_boot() {
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_005);
        g.on_tick(1, 1_300, 1_305);
        assert!(!g.is_open(1_941, false));
        g.on_tick(0, 2_000, 2_004);
        g.on_tick(1, 2_300, 2_306);
        assert_eq!(g.pairs, vec![json!({"a": 2_000, "b": 2_300})]);
        g.on_handoff(10_000);
        let mut fresh = Gate::new(60);
        // The restarted agents tick on NEW phases (A 20 500, B 20 800).
        for x in [&mut g, &mut fresh] {
            x.on_tick(0, 20_500, 20_505);
            x.on_tick(1, 20_800, 20_805);
        }
        for now in [20_900, 21_000, 21_200, 21_439, 21_441, 21_600] {
            assert_eq!(g.is_open(now, false), fresh.is_open(now, false), "{now}");
        }
        // NEGATIVE: held from the NEW lead tick − guard (21 500 − 60) …
        let mut g2 = Gate::new(60);
        g2.on_tick(0, 1_000, 1_005);
        g2.on_tick(1, 1_300, 1_305);
        g2.on_handoff(10_000);
        g2.on_tick(0, 20_500, 20_505);
        g2.on_tick(1, 20_800, 20_805);
        assert!(g2.is_open(21_400, false));
        assert!(!g2.is_open(21_441, false));
        // … the old phase (… 21 000 − 60) is forgotten: open there.
        let mut g3 = Gate::new(60);
        g3.on_tick(0, 1_000, 1_005);
        g3.on_tick(1, 1_300, 1_305);
        g3.on_handoff(10_000);
        g3.on_tick(0, 20_500, 20_505);
        g3.on_tick(1, 20_800, 20_805);
        assert!(g3.is_open(20_950, false));
        // Pairs: the pre-handoff one, the seam marker, then the new pairs.
        g.on_tick(0, 21_500, 21_504);
        g.on_tick(1, 21_800, 21_806);
        assert_eq!(
            g.pairs,
            vec![
                json!({"a": 2_000, "b": 2_300}),
                json!({"handoff": 10_000}),
                json!({"a": 21_500, "b": 21_800}),
            ]
        );
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
        // Cut 1500..5000; A is back at once, B (its own 2 s phase) is not.
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
    fn barrier_inputs_wait_until_both_agents_are_back_after_a_restore() {
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_300, 1_300);
        assert!(!g.post_cut_hold(2_000), "no cut yet");
        g.on_restore(10_000);
        g.on_tick(0, 10_000, 10_100); // one agent back at once
        assert!(g.post_cut_hold(12_000), "the other still in its 2 s retry");
        g.on_tick(1, 9_300, 12_500); // it flushes its queue 2.5 s in
        assert!(g.post_cut_hold(15_000), "settle");
        assert!(!g.post_cut_hold(15_501));
        // An agent that never returns: released POST_CUT_MAX_MS after the restore.
        let mut h = Gate::new(60);
        h.on_tick(0, 1_000, 1_000);
        h.on_tick(1, 1_300, 1_300);
        h.on_restore(10_000);
        assert!(h.post_cut_hold(10_000 + POST_CUT_MAX_MS - 1));
        assert!(!h.post_cut_hold(10_000 + POST_CUT_MAX_MS));
    }

    #[test]
    fn cut_window_follows_a_drifting_tick() {
        // A drifts 7 ms/tick (Node), B keeps its phase (tokio).
        let mut g = Gate::new(60);
        for i in 0..8 {
            g.on_tick(0, 1_100 + i * 1_007, 0);
            g.on_tick(1, 1_750 + i * 1_000, 0);
        }
        assert_eq!(g.track[0].drift_per_tick(), 7);
        assert_eq!(g.track[1].drift_per_tick(), 0);
        // 20 ticks into a cut A really ticks at 1100 + 27·1007 = 28289; the
        // lower envelope says 28100 (+150 read = 28250: open before A reads).
        // Pair B 27750 ↔ A 28289: held from 27600 through A's read.
        assert!(g.is_open(27_590, true));
        assert!(!g.is_open(27_700, true));
        assert!(!g.is_open(28_300, true));
        assert!(!g.is_open(28_420, true));
        assert!(g.is_open(28_500, true));
        // Never more than 1000 − CUT_MIN_OPEN_MS of a second held.
        let mut h = Gate::new(60);
        for i in 0..8 {
            h.on_tick(0, 1_000 + i * 1_020, 0);
            h.on_tick(1, 1_400 + i * 1_000, 0);
        }
        let held = (40_000..41_000)
            .step_by(10)
            .filter(|t| !h.is_open(*t, true))
            .count();
        assert!(held <= 80, "{held}");
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
        // Inside the hysteresis band (500 < 530 ≤ 550) the alignment is kept.
        let mut g = Gate::new(60);
        g.on_tick(0, 1_000, 1_000);
        g.on_tick(1, 1_530, 1_530);
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

    /// Phase 5b: per-side JS builds (JS-vs-JS across two refs); a side
    /// without its own flag keeps `--js-dir`.
    #[test]
    fn js_dir_per_side() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct W {
            #[command(flatten)]
            play: PlayArgs,
        }
        let base = ["x", "--tape", "t", "--out", "o"];
        let w = W::parse_from(base);
        assert_eq!(w.play.js_dir_for("a"), Path::new("/rig/cache/js-09da38d"));
        assert_eq!(w.play.js_dir_for("b"), Path::new("/rig/cache/js-09da38d"));
        let w = W::parse_from(base.iter().copied().chain([
            "--js-dir",
            "/j/js-x",
            "--js-dir-b",
            "/j/js-09da38d",
        ]));
        assert_eq!(w.play.js_dir_for("a"), Path::new("/j/js-x"));
        assert_eq!(w.play.js_dir_for("b"), Path::new("/j/js-09da38d"));
        let w = W::parse_from(base.iter().copied().chain(["--js-dir-a", "/j/js-89b5c64"]));
        assert_eq!(w.play.js_dir_for("a"), Path::new("/j/js-89b5c64"));
        assert_eq!(w.play.js_dir_for("b"), Path::new("/rig/cache/js-09da38d"));
    }

    /// Phase 5b: an agent whose telemetry is held (clock trust) still counts
    /// for input delivery once it has published anything for two ticks.
    #[test]
    fn held_agent_counts_after_two_ticks_of_any_publish() {
        let g = Mutex::new(Gate::new(150));
        assert!(!inputs_ticking(&g, 10_000));
        g.lock().unwrap().on_tick(0, 1_000, 1_005);
        assert!(!inputs_ticking(&g, 10_000), "B never published");
        g.lock().unwrap().on_publish(1, 1_200); // B's config report
        assert!(!inputs_ticking(&g, 3_199));
        assert!(inputs_ticking(&g, 3_200));
        // Both publishing telemetry: the pre-5b rule, at once.
        let g = Mutex::new(Gate::new(150));
        g.lock().unwrap().on_tick(0, 1_000, 1_005);
        g.lock().unwrap().on_tick(1, 1_100, 1_105);
        assert!(inputs_ticking(&g, 1_106));
    }

    /// Post-hold settle: barrier inputs wait from the trust instant until the
    /// held agent's first telemetry publish — and NOTHING else changes.
    #[test]
    fn post_hold_settle_waits_for_the_first_post_trust_tick_only() {
        let mut g = Gate::new(150);
        g.on_tick(0, 1_000, 1_005); // A publishing
        g.on_publish(1, 1_100); // B alive, telemetry held
        assert!(g.held(1, 5_000));
        g.expect_resume(1, 300_000, RESUME_FALLBACK_CAP_MS);
        assert!(!g.post_hold_hold(299_999), "not before the trust instant");
        assert!(g.post_hold_hold(300_000));
        assert!(g.post_hold_hold(300_900));
        g.on_tick(1, 300_950, 300_960); // B's first post-trust tick
        assert!(!g.post_hold_hold(300_961), "released at B's first publish");
        assert!(
            g.pairs.iter().all(|p| p.get("skip").is_none()),
            "never skips a tick"
        );
    }

    /// NEGATIVE: a resumed agent that is late beyond the TS bound is not
    /// waited for (the settle expires at its cap, inputs flow again, and the
    /// missing / late payloads are judged by the diff as before); an early
    /// tick (before the trust instant) does not count as the resume.
    #[test]
    fn post_hold_settle_never_waits_past_its_cap_nor_counts_an_early_tick() {
        let mut g = Gate::new(150);
        g.on_tick(0, 1_000, 1_005);
        g.on_publish(1, 1_100);
        g.expect_resume(1, 60_000, RESUME_NTP_CAP_MS);
        assert!(g.post_hold_hold(60_000 + RESUME_NTP_CAP_MS - 1));
        assert!(!g.post_hold_hold(60_000 + RESUME_NTP_CAP_MS), "capped");
        assert!(!g.post_hold_hold(90_000));
        // early: a publish before `from` is not the post-trust tick
        let mut h = Gate::new(150);
        h.on_tick(0, 1_000, 1_005);
        h.on_publish(1, 1_100);
        h.expect_resume(1, 10_000, RESUME_STEP_CAP_MS);
        h.on_tick(1, 9_000, 9_990);
        assert!(
            h.post_hold_hold(10_000),
            "the 9.99 s publish predates the step"
        );
        assert!(h.pairs.is_empty());
    }

    /// R-P3G-4: the rig meminfo parses like a real one (both agents read
    /// MemTotal/MemAvailable from it) and pins ram_usage at 21.2 %.
    #[test]
    fn obd_gate_file_follows_the_hold_gate_with_a_heartbeat() {
        let d = std::env::temp_dir().join(format!("obd-gate-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let mut g = ObdGate::new(obd_gate_path(&d));
        let read = || fs::read_to_string(d.join("obd-gate")).unwrap();
        g.write(false, 1_000);
        assert_eq!(read(), "0 0");
        // A change is written at once…
        g.write(true, 1_010);
        assert_eq!(read(), "1 1");
        // …an unchanged state only on the heartbeat.
        fs::write(d.join("obd-gate"), "x").unwrap();
        g.write(true, 1_400);
        assert_eq!(read(), "x");
        g.write(true, 1_510);
        assert_eq!(read(), "1 1");
        // R7-1: every closed → open transition is a new opening epoch.
        g.write(false, 2_000);
        assert_eq!(read(), "0 1");
        g.write(true, 2_010);
        assert_eq!(read(), "1 2");
        // Tape done: no file = the relays' OPEN fallback.
        g.clear();
        assert!(!d.join("obd-gate").exists());
        let _ = fs::remove_dir_all(&d);
    }

    /// Review-wp4 R7-1: a barrier input waits while a relay's `<side>.held`
    /// flag is set (fresh), never past OBD_FLUSH_CAP_MS.
    #[test]
    fn barrier_inputs_wait_for_the_relays_held_lines() {
        let d = std::env::temp_dir().join(format!("obd-held-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let now = 1_793_000_000_000i64;
        assert!(!obd_relays_holding(&d, now), "no flags");
        fs::write(d.join("b.held"), format!("{}", (now - 100) as f64 / 1000.0)).unwrap();
        assert!(obd_relays_holding(&d, now), "b holds a line");
        fs::write(
            d.join("b.held"),
            format!("{}", (now - 2_500) as f64 / 1000.0),
        )
        .unwrap();
        assert!(!obd_relays_holding(&d, now), "past the cap");
        fs::remove_file(d.join("b.held")).unwrap();
        fs::write(d.join("a.held"), "garbage").unwrap();
        assert!(!obd_relays_holding(&d, now), "unparseable = not holding");
        let _ = fs::remove_dir_all(&d);
    }

    /// Phase 5b WP5 (review-wp4 NIT): a cap release is logged, once per flag;
    /// a fresh flag, no flag or an unparseable one logs nothing.
    #[test]
    fn a_cap_release_of_a_held_flag_is_logged_once() {
        let d = std::env::temp_dir().join(format!("obd-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let now = 1_793_000_000_000i64;
        let mut log = ObdCapLog::default();
        assert!(log.released(&d, now).is_empty(), "no flags");
        fs::write(d.join("a.held"), format!("{}", (now - 100) as f64 / 1000.0)).unwrap();
        assert!(
            log.released(&d, now).is_empty(),
            "fresh flag: still holding"
        );
        fs::write(
            d.join("a.held"),
            format!("{}", (now - 2_500) as f64 / 1000.0),
        )
        .unwrap();
        let l = log.released(&d, now);
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(
            l[0].starts_with("obd relay a: held flag from ") && l[0].contains("2500 ms old"),
            "{l:?}"
        );
        assert!(
            log.released(&d, now + 5).is_empty(),
            "the same flag is logged once"
        );
        fs::write(d.join("b.held"), "garbage").unwrap();
        assert!(
            log.released(&d, now + 10).is_empty(),
            "unparseable = not a flag"
        );
        // A NEW hold that ages past the cap is a new release.
        fs::write(
            d.join("a.held"),
            format!("{}", (now - 1_000) as f64 / 1000.0),
        )
        .unwrap();
        assert!(log.released(&d, now).is_empty());
        assert_eq!(log.released(&d, now + 1_500).len(), 1);
        let _ = fs::remove_dir_all(&d);
    }

    /// Phase 5b WP5 item 4: both agents get the statvfs preload + the fixture
    /// (both agents' formulas give 19.2 %); `--host-disk` or a missing shim
    /// gives nothing.
    #[test]
    fn disk_isolation_env_is_the_preload_and_the_fixture() {
        use clap::Parser;
        let shim = std::env::temp_dir().join(format!("rig-statvfs-{}.so", std::process::id()));
        std::fs::write(&shim, b"x").unwrap();
        #[derive(clap::Parser)]
        struct W {
            #[command(flatten)]
            play: PlayArgs,
        }
        let parse = |extra: &[&str]| {
            let mut v = vec!["x", "--tape", "t", "--out", "o"];
            v.extend_from_slice(extra);
            W::parse_from(v).play
        };
        let s = shim.display().to_string();
        let env = disk_isolation_env(&parse(&["--statvfs-shim", &s])).unwrap();
        assert_eq!(disk_source(&parse(&["--statvfs-shim", &s])), "shim");
        assert_eq!(
            env,
            vec![
                ("LD_PRELOAD".to_string(), s.clone()),
                ("RIG_STATVFS_ROOT".to_string(), RIG_STATVFS_ROOT.to_string())
            ]
        );
        let host = parse(&["--statvfs-shim", &s, "--host-disk"]);
        assert!(disk_isolation_env(&host).unwrap().is_empty());
        assert_eq!(disk_source(&host), "host");
        // R-WP5-4: a missing shim FAILS (no silent host fallback) …
        let err = disk_isolation_env(&parse(&["--statvfs-shim", "/nonexistent.so"])).unwrap_err();
        assert!(err.to_string().contains("--host-disk"), "{err:#}");
        // … unless the host disk is asked for.
        let host_missing = parse(&["--statvfs-shim", "/nonexistent.so", "--host-disk"]);
        assert!(disk_isolation_env(&host_missing).unwrap().is_empty());
        assert_eq!(disk_source(&host_missing), "host");
        let _ = std::fs::remove_file(&shim);
        let f: Vec<u64> = RIG_STATVFS_ROOT
            .split(':')
            .map(|x| x.parse().unwrap())
            .collect();
        let u = xorgate_core::system::disk_usage_from_statvfs(f[0], f[1], f[3]).unwrap();
        assert_eq!((u.used_pct * 10.0).round() / 10.0, 19.2);
    }

    #[test]
    fn rig_meminfo_is_a_fixed_parseable_input() {
        let m = xorgate_core::system::parse_mem_available(RIG_MEMINFO).expect("parses");
        let used = (m.total_bytes - m.available_bytes) / m.total_bytes * 100.0;
        assert_eq!((used * 10.0).round() / 10.0, 21.2);
    }
}
