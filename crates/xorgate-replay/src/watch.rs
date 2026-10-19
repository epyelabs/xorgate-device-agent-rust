//! The watch gate: input releases kept off each agent's PERIODIC WATCH reads.
//!
//! The hold gate (`play.rs` [`crate::play::Gate`]) pairs input releases with
//! the agents' 1 Hz telemetry ticks only. Both agents also re-read some inputs
//! on slower timers of their own, whose phases differ between the agents by
//! roughly the start-up offset (Node module load vs a native binary, ~300 ms).
//! A release that lands between agent A's and agent B's tick of the same watch
//! is seen by one and not the other: replay `022651Z` released an `lte_file`
//! version between Rust's (xx:x4.38) and JS's (xx:x4.68) 30 s APN-watch ticks,
//! so only JS logged `modem APN state changed — re-reporting`
//! (`phase-2/evidence/apn-rereport.md`). That is a rig artefact; this gate
//! removes it by holding the input until BOTH agents' corresponding ticks are
//! past.
//!
//! Periodic (≥ 1 s) watches that read a rig input, TS @ 89b5c64 vs Rust:
//!
//! | watch | period | input | TS | Rust | gated here |
//! |---|---|---|---|---|---|
//! | APN watch | 30 s, standing | `lte_file` | `config-sync.ts:1486-1503` | `config_plane/sync.rs:433-470` | yes ([`Kind::Apn`]) |
//! | modem-arrival | 30 s chain, ≤ 20 tries, armed by a report without `sim`/`uplink` | `lte_file` | `:1593-1630` | `sync.rs:537-575` | yes ([`Kind::ModemArrival`]) |
//! | settings fingerprint | 5 s, standing | `settings.json` (`cli` writes it; a `config` apply rewrites it) | `:1365-1371` | `sync.rs:296-320` | yes ([`Kind::SettingsBoot`]/[`Kind::Settings`]) |
//! | config report build | 2 s after any trigger (debounce) | lte_file, settings.json | `config-sync.ts:1436-1449` | `sync.rs:375-392` | yes: held from one agent's captured report until the other's ([`REPORT_PAIR_MS`]) |
//! | telemetry | 1 s | lte_file, exec, NMEA, IMU | `worker.ts:1191` | `mqtt_plane.rs:456` | the hold gate |
//! | heartbeat | 1 h | (report build) | `:1452-1459` | `sync.rs:396-410` | no: never fires in a rig run |
//! | camera-mount poll | 60 s | settings.json | `worker.ts:648` | — (Phase 3) | no partner: T6 |
//! | recording settings poll / shed / disk | 5 s / … | settings.json, vcgencmd | `recording/manager.ts:319,613,790` | — (Phase 3) | no partner: T6 (its settings reads are tracked like any 5 s grid, which only over-holds) |
//! | LED tick | 1 s | lte_file | `agent/led-manager.ts:53` | — (Phase 3) | no partner: T6 |
//! | clock-step | 30 s | modem clock | `agent/clock-step.ts:291` | — (Phase 4, CF20) | no partner: T6 `[clock]` |
//!
//! Settle (40 s) and debounce (2 s) timers are one-shots that only REQUEST a
//! report; the GPS reopen watchdog (30 s of silence) is driven by the NMEA
//! stream, which the player already releases to both agents or neither.
//!
//! How the ticks are known. Each agent's timers start at a point the player
//! cannot see, but which lies between its spawn and its first MQTT publish
//! (TS: `startWatcher`/`startApnWatch` run synchronously at `worker.ts:741-746`,
//! before the event loop delivers the connect; Rust: `config_plane::start_with`
//! spawns both tasks before the MQTT plane connects; the 022651Z tape shows its
//! APN anchor at mono 12 ms, connect at 85 ms). So tick k of a standing watch
//! lies in `[spawn, first publish + BOOT_SLACK] + k·period`, widened by a
//! per-tick drift margin (Node's `setInterval` re-arms after the callback, a
//! few ms per tick; tokio's `Delay` shifts on a tick > 5 ms late).
//! The settings watch is narrowed by OBSERVATION: the player sees every
//! `IN_OPEN` of each agent's `settings.json` (inotify, [`observe`]), and an
//! open inside a predicted window pins that tick; an open that matches no
//! window seeds a speculative 5 s track that survives only if it repeats. The
//! LTE file is also read at every 1 Hz telemetry tick, so an open there cannot
//! be attributed to the APN watch live: the LTE windows stay the boot-anchored
//! prediction, and every open is logged (`<label>/reads.ndjson`) so the run's
//! `watch.json` can show that each predicted window held a read.
//! Modem-arrival ticks are anchored on the capture time of the report that
//! armed the chain.
//!
//! A release of input X waits while `[now, now + lookahead]` meets the window
//! of a tick of a watch reading X, or the span between the two agents'
//! corresponding ticks (paired when closer than [`PAIR_MS`]). `lookahead` is
//! the time from release to the agent-visible write: ~0 for the player's own
//! LTE write, up to [`CLI_WRITE_MS`] for `xorgate set` (Node start-up).

use serde_json::{json, Value};

/// A rig input that a periodic watch reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// The modem telemetry file (`LTE_TELEMETRY_FILE`).
    Lte,
    /// `settings.json` (written by the `cli` tape records).
    Settings,
}

impl Input {
    pub fn as_str(self) -> &'static str {
        match self {
            Input::Lte => "lte_file",
            Input::Settings => "settings.json",
        }
    }
}

/// `DEFAULT_APN_WATCH_MS` (`config-sync.ts:1198`, `config_sync_rules.rs:48`).
pub const APN_WATCH_MS: i64 = 30_000;
/// `DEFAULT_MODEM_ARRIVAL_MS` (`:1188`, `config_sync_rules.rs:44`).
pub const MODEM_ARRIVAL_MS: i64 = 30_000;
/// `MODEM_ARRIVAL_TRIES` (`:1189`, `config_sync_rules.rs:46`).
pub const MODEM_ARRIVAL_TRIES: u32 = 20;
/// `DEFAULT_WATCH_MS` (`:1160`, `config_sync_rules.rs:37`).
pub const SETTINGS_WATCH_MS: i64 = 5_000;

/// Hold this long before a predicted tick window.
const GUARD_MS: i64 = 150;
/// … and this long after it (the read itself, plus jitter).
const READ_MS: i64 = 150;
/// Early jitter allowed on a tick predicted from an observed one.
const JITTER_MS: i64 = 15;
/// Per-tick widening of a 30 s watch (Node re-arm lateness: ~6.5 ms/tick in 022651Z).
const LTE_DRIFT_MS: i64 = 15;
/// Per-tick widening of the 5 s watch.
const SETTINGS_DRIFT_MS: i64 = 8;
/// After the first captured publish (the timers started before it).
const BOOT_SLACK_MS: i64 = 250;
/// An open this close to a window is that tick's read.
const MATCH_MS: i64 = 40;
/// Modem-arrival anchor around the capture of the arming report.
const ARRIVAL_BEFORE_MS: i64 = 100;
const ARRIVAL_AFTER_MS: i64 = 80;
/// Two agents' ticks of one watch closer than this are the same tick.
pub const PAIR_MS: i64 = 2_500;
/// `xorgate set` writes settings.json at most this long after its spawn
/// (Node start-up under load; the Rust CLI takes ~50 ms).
pub const CLI_WRITE_MS: i64 = 1_500;
/// The player's own LTE write (both agents, one after the other).
pub const LTE_WRITE_MS: i64 = 30;
/// A config report of one agent waits this long for the other's counterpart.
pub const REPORT_PAIR_MS: i64 = 3_000;
/// Never hold one release longer than this (logged as a timeout).
pub const MAX_HOLD_MS: i64 = 10_000;
/// REPORT FENCE (CI run 39276719573): a report trigger (a released `mqtt_in`
/// / `cli`) arms each agent's 2 s report debounce (`config-sync.ts:1435-1448`
/// ↔ `sync.rs`); an LTE rewrite falling due inside [trigger, trigger +
/// `FENCE_WINDOW_MS`] would land before one agent's report build and after
/// the other's (delivery lag up to ~0.8 s on a loaded runner), so it waits
/// until BOTH agents published a report captured after the trigger — at most
/// `FENCE_CAP_MS` past its due time (then released and logged). One release
/// still writes both agents' LTE files in one step (`play.rs` apply_state).
pub const FENCE_WINDOW_MS: i64 = 2_000 + 500;
pub const FENCE_CAP_MS: i64 = 5_000;

/// Which watch a track predicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The standing 30 s APN watch, boot-anchored.
    Apn,
    /// The 30 s modem-arrival chain, report-anchored.
    ModemArrival,
    /// The settings watch's first tick, boot-anchored.
    SettingsBoot,
    /// A 5 s grid of observed settings.json opens.
    Settings,
}

impl Kind {
    fn input(self) -> Input {
        match self {
            Kind::Apn | Kind::ModemArrival => Input::Lte,
            Kind::SettingsBoot | Kind::Settings => Input::Settings,
        }
    }
    /// Ticks of the same class in the two agents are paired.
    fn class(self) -> &'static str {
        match self {
            Kind::Apn => "apn-watch",
            Kind::ModemArrival => "modem-arrival",
            Kind::SettingsBoot | Kind::Settings => "settings-watch",
        }
    }
    fn period(self) -> i64 {
        match self {
            Kind::Apn => APN_WATCH_MS,
            Kind::ModemArrival => MODEM_ARRIVAL_MS,
            Kind::SettingsBoot | Kind::Settings => SETTINGS_WATCH_MS,
        }
    }
    fn drift(self) -> i64 {
        match self.input() {
            Input::Lte => LTE_DRIFT_MS,
            Input::Settings => SETTINGS_DRIFT_MS,
        }
    }
    /// Pinned by observed opens?
    fn observed(self) -> bool {
        self.input() == Input::Settings
    }
}

/// One predicted periodic tick: the NEXT tick lies in `[lo, hi]`.
#[derive(Debug, Clone)]
struct Track {
    kind: Kind,
    lo: i64,
    hi: i64,
    /// Tick index (0 = first predicted tick).
    k: u32,
    hits: u32,
    misses: u32,
    /// Ticks left (modem-arrival chain).
    left: Option<u32>,
    /// The open that pinned the current tick.
    hit: Option<i64>,
}

impl Track {
    fn new(kind: Kind, lo: i64, hi: i64) -> Self {
        Self {
            kind,
            lo,
            hi,
            k: 0,
            hits: 0,
            misses: 0,
            left: (kind == Kind::ModemArrival).then_some(MODEM_ARRIVAL_TRIES),
            hit: None,
        }
    }
    fn window(&self) -> (i64, i64) {
        (self.lo - GUARD_MS, self.hi + READ_MS)
    }
    /// Kept after this tick passed?
    fn keep(&self) -> bool {
        match self.kind {
            Kind::Apn => true,
            Kind::ModemArrival => self.left != Some(0),
            Kind::SettingsBoot => self.misses < 2,
            // Speculative (never repeated): dropped at the first miss.
            Kind::Settings => (self.hits > 0 && self.misses < 3) || self.misses == 0,
        }
    }
}

#[derive(Debug, Default)]
struct AgentWatch {
    spawn: Option<i64>,
    first_msg: Option<i64>,
    /// `modemSeen` (`config-sync.ts:1606`): latched by a report with `sim`/`uplink`.
    modem_seen: bool,
    tracks: Vec<Track>,
    /// Observed opens (for `watch.json`): input, wall, part of the 1 Hz series.
    opens: Vec<(Input, i64, bool)>,
    /// The 1 Hz read series per input (index = [`Input`] as usize).
    hz: [HzSeries; 2],
    writes: Vec<(Input, i64)>,
}

/// Both agents ALSO read settings.json and the LTE file at every 1 Hz
/// telemetry tick (022651Z-era reads.ndjson: ~1 open/s each, drifting a few
/// ms per tick). Those reads are the hold gate's business; an open that
/// continues that series (1 s ± 60 ms, or 2 s after a missed one, after the
/// series' last read) is classified out before the watch tracks see it. Of
/// two opens in one slot the first is taken as the series' read — the other
/// is the watch's, at the same instant either way.
#[derive(Debug, Default)]
struct HzSeries {
    last: Option<i64>,
    recent: Vec<i64>,
}

impl HzSeries {
    const SLOP: i64 = 60;
    fn near(d: i64, k: i64) -> bool {
        (d - k * 1000).abs() <= Self::SLOP
    }
    /// Is the open at `o` a read of the 1 Hz series?
    fn classify(&mut self, o: i64) -> bool {
        if let Some(h) = self.last {
            let d = o - h;
            if Self::near(d, 1) || Self::near(d, 2) {
                self.last = Some(o);
                return true;
            }
        }
        if self.recent.iter().any(|r| Self::near(o - r, 1)) {
            self.last = Some(o);
            self.recent.clear();
            return true;
        }
        self.recent.retain(|r| o - r < 1_200);
        self.recent.push(o);
        false
    }
}

/// See the module doc. Index 0 = agent A, 1 = agent B.
#[derive(Debug, Default)]
pub struct WatchGate {
    agents: [AgentWatch; 2],
    /// Every passed tick window: `{agent, class, k, lo, hi, hit}`.
    ticks: Vec<Value>,
    /// Every release that waited: `{input, due, released, held_ms, timeout}`.
    holds: Vec<Value>,
    /// Every release of a gated input: `(input, wall)`.
    releases: Vec<(Input, i64)>,
    /// Capture times of config reports not yet matched by the other agent's.
    unmatched: [Vec<i64>; 2],
    /// Releases that waited for a report pair.
    report_holds: usize,
    /// Report-fence triggers: `(release wall ms, [A reported, B reported])`.
    triggers: Vec<(i64, [bool; 2])>,
    /// Every release the report fence held: `{due, released, capped}`.
    fenced: Vec<Value>,
}

impl WatchGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Agent `idx` was spawned at `now`.
    pub fn on_spawn(&mut self, idx: usize, now: i64) {
        self.agents[idx].spawn.get_or_insert(now);
    }

    /// Agent `idx` published for the first time (any topic) at `now`: its
    /// standing watches started in `[spawn, now]`.
    pub fn on_publish(&mut self, idx: usize, now: i64) {
        let a = &mut self.agents[idx];
        if a.first_msg.is_some() {
            return;
        }
        a.first_msg = Some(now);
        let lo = a.spawn.unwrap_or(now - 2_000);
        let hi = now + BOOT_SLACK_MS;
        a.tracks
            .push(Track::new(Kind::Apn, lo + APN_WATCH_MS, hi + APN_WATCH_MS));
        a.tracks.push(Track::new(
            Kind::SettingsBoot,
            lo + SETTINGS_WATCH_MS,
            hi + SETTINGS_WATCH_MS,
        ));
    }

    /// Agent `idx` published a config report at `now`. `has_modem`: its
    /// `effective` carries `sim` or `uplink` (`watchForModemArrival`,
    /// `config-sync.ts:1622-1629`).
    pub fn on_report(&mut self, idx: usize, now: i64, has_modem: bool) {
        for (t, done) in self.triggers.iter_mut() {
            if now > *t {
                done[idx] = true;
            }
        }
        // Report pairing: a report build reads the LTE file and settings.json
        // (effective) at each agent's own instant, tens of ms apart; a release
        // between them splits the pair (CI smoke 9abb559 attempt 2: an LTE
        // version landed between two reports 37 ms apart →
        // `effective.sim.checked_at` 15:20:17Z vs 15:20:27Z). Between one
        // agent's report and the other's counterpart, LTE/settings releases
        // wait (≤ REPORT_PAIR_MS; an unmatched report — e.g. one agent's
        // reconnect sees an intermediate rev — just expires).
        let o = 1 - idx;
        self.unmatched[o].retain(|q| now - q <= REPORT_PAIR_MS);
        if self.unmatched[o].is_empty() {
            self.unmatched[idx].push(now);
        } else {
            self.unmatched[o].remove(0);
        }
        let a = &mut self.agents[idx];
        if has_modem {
            a.modem_seen = true;
            a.tracks.retain(|t| t.kind != Kind::ModemArrival);
            return;
        }
        if a.modem_seen || a.tracks.iter().any(|t| t.kind == Kind::ModemArrival) {
            return;
        }
        a.tracks.push(Track::new(
            Kind::ModemArrival,
            now - ARRIVAL_BEFORE_MS + MODEM_ARRIVAL_MS,
            now + ARRIVAL_AFTER_MS + MODEM_ARRIVAL_MS,
        ));
    }

    /// Agent `idx`'s copy of `input` was opened at `now` (inotify `IN_OPEN`).
    pub fn on_open(&mut self, idx: usize, input: Input, now: i64) {
        self.advance(now);
        let a = &mut self.agents[idx];
        let hz = a.hz[input as usize].classify(now);
        a.opens.push((input, now, hz));
        if input != Input::Settings || hz {
            return;
        }
        // One open pins ONE tick: the nearest unpinned window it falls in
        // (JS runs two 5 s grids ~30 ms apart — the watcher and the
        // recording poll — and each must keep its own track).
        let best = a
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.kind.observed()
                    && t.hit.is_none()
                    && now >= t.lo - MATCH_MS
                    && now <= t.hi + MATCH_MS
            })
            .min_by_key(|(_, t)| (now - (t.lo + t.hi) / 2).abs())
            .map(|(i, _)| i);
        let matched = best.is_some();
        if let Some(i) = best {
            let t = &mut a.tracks[i];
            t.hit = Some(now);
            t.lo = now;
            t.hi = now;
            t.hits += 1;
            t.misses = 0;
        }
        if !matched {
            let mut t = Track::new(
                Kind::Settings,
                now + SETTINGS_WATCH_MS - JITTER_MS,
                now + SETTINGS_WATCH_MS + SETTINGS_DRIFT_MS,
            );
            t.k = 1;
            a.tracks.push(t);
        }
    }

    /// Agent `idx`'s copy of `input` was replaced at `now` (inotify
    /// `IN_MOVED_TO`/`IN_CLOSE_WRITE`): for the evidence only.
    pub fn on_write(&mut self, idx: usize, input: Input, now: i64) {
        self.agents[idx].writes.push((input, now));
    }

    /// Roll every track whose tick is past `now` on to its next tick.
    fn advance(&mut self, now: i64) {
        for (idx, a) in self.agents.iter_mut().enumerate() {
            let mut keep = Vec::with_capacity(a.tracks.len());
            for mut t in a.tracks.drain(..) {
                let mut alive = true;
                while alive && now > t.hi + READ_MS {
                    if t.kind != Kind::Settings || t.hits > 0 || t.hit.is_some() {
                        self.ticks.push(json!({
                            "agent": idx, "class": t.kind.class(), "kind": format!("{:?}", t.kind),
                            "k": t.k, "lo": t.lo, "hi": t.hi, "hit": t.hit,
                        }));
                    }
                    let p = t.kind.period();
                    if t.hit.take().is_some() {
                        t.lo += p - JITTER_MS;
                        t.hi += p + t.kind.drift();
                    } else {
                        if t.kind.observed() {
                            t.misses += 1;
                        }
                        t.lo += p;
                        t.hi += p + t.kind.drift();
                    }
                    t.k += 1;
                    if let Some(l) = t.left.as_mut() {
                        *l = l.saturating_sub(1);
                    }
                    if t.kind == Kind::SettingsBoot && t.hits > 0 {
                        // Pinned once: the observed grid takes over.
                        t.kind = Kind::Settings;
                    }
                    alive = t.keep();
                }
                if alive {
                    keep.push(t);
                }
            }
            // Two tracks pinned by the same open predict the same tick: keep one.
            keep.sort_by_key(|t| t.lo);
            keep.dedup_by(|x, y| {
                x.kind == y.kind && (x.lo - y.lo).abs() <= 2 && (x.hi - y.hi).abs() <= 2
            });
            a.tracks = keep;
        }
    }

    /// The hold regions for `input` (sorted, may overlap).
    fn regions(&self, input: Input) -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        let tracks = |i: usize| {
            self.agents[i]
                .tracks
                .iter()
                .filter(move |t| t.kind.input() == input)
        };
        for i in 0..2 {
            for t in tracks(i) {
                out.push(t.window());
                // Pair with the other agent's nearest tick of the same class.
                let near = tracks(1 - i)
                    .filter(|u| u.kind.class() == t.kind.class())
                    .min_by_key(|u| gap(t, u));
                if let Some(u) = near {
                    if gap(t, u) <= PAIR_MS {
                        out.push((t.lo.min(u.lo) - GUARD_MS, t.hi.max(u.hi) + READ_MS));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Must a release of `input` at `now`, visible to the agents within
    /// `lookahead` ms, wait?
    pub fn holds(&mut self, input: Input, now: i64, lookahead: i64) -> bool {
        self.advance(now);
        if self
            .unmatched
            .iter()
            .flatten()
            .any(|q| now >= *q && now - q <= REPORT_PAIR_MS)
        {
            return true;
        }
        let end = now + lookahead;
        self.regions(input)
            .iter()
            .any(|&(lo, hi)| lo <= end && now <= hi)
    }

    /// A report trigger (a `mqtt_in` / `cli` record) was released at `now`.
    pub fn on_trigger(&mut self, now: i64) {
        self.triggers
            .retain(|(t, d)| !(d[0] && d[1]) && now - t <= FENCE_WINDOW_MS + FENCE_CAP_MS);
        self.triggers.push((now, [false, false]));
    }

    /// Report fence: must an LTE release that fell due at `due` still wait at
    /// `now`? `Some(false)` = fenced but capped (release it; logged once by
    /// [`Self::note_fenced`]), `Some(true)` = wait, `None` = not fenced.
    pub fn fence(&self, input: Input, due: i64, now: i64) -> Option<bool> {
        if input != Input::Lte {
            return None;
        }
        self.triggers
            .iter()
            .any(|(t, d)| *t <= due && due - t <= FENCE_WINDOW_MS && !(d[0] && d[1]))
            .then_some(now - due <= FENCE_CAP_MS)
    }

    /// The fence held a release due at `due` until `released`.
    pub fn note_fenced(&mut self, due: i64, released: i64, capped: bool) {
        self.fenced.push(
            json!({"due": due, "released": released, "held_ms": released - due, "capped": capped}),
        );
    }

    /// A release of `input` happened at `released` (due at `due`).
    pub fn note_release(&mut self, input: Input, due: i64, released: i64, timeout: bool) {
        if self
            .unmatched
            .iter()
            .flatten()
            .any(|q| *q <= released && released - q <= REPORT_PAIR_MS + 50 && *q >= due)
        {
            self.report_holds += 1;
        }
        self.releases.push((input, released));
        if released - due > 0 || timeout {
            self.holds.push(json!({
                "input": input.as_str(), "due": due, "released": released,
                "held_ms": released - due, "timeout": timeout,
            }));
        }
    }

    /// `watch.json`: tick windows, holds, and the per-window read evidence.
    pub fn report(&mut self, end: i64) -> Value {
        self.advance(end);
        let mut summary = Vec::new();
        for (idx, a) in self.agents.iter().enumerate() {
            for class in ["apn-watch", "modem-arrival", "settings-watch"] {
                let wins: Vec<&Value> = self
                    .ticks
                    .iter()
                    .filter(|t| t["agent"] == idx && t["class"] == class)
                    .collect();
                let input = if class == "settings-watch" {
                    Input::Settings
                } else {
                    Input::Lte
                };
                // A read in the window that is NOT the 1 Hz series' read is
                // the watch's own (the evidence that the prediction held it).
                let with_read = wins
                    .iter()
                    .filter(|w| {
                        let (lo, hi) = (
                            w["lo"].as_i64().unwrap_or(0) - MATCH_MS,
                            w["hi"].as_i64().unwrap_or(0) + MATCH_MS,
                        );
                        a.opens
                            .iter()
                            .any(|&(i, o, hz)| i == input && !hz && o >= lo && o <= hi)
                    })
                    .count();
                summary.push(json!({
                    "agent": idx, "class": class, "ticks": wins.len(), "ticks_with_watch_read": with_read,
                }));
            }
        }
        // A release inside a paired span would split that tick (only a timeout can).
        let timeouts = self.holds.iter().filter(|h| h["timeout"] == true).count();
        json!({
            "summary": summary,
            "releases": self.releases.iter().map(|(i, w)| json!({"input": i.as_str(), "w": w})).collect::<Vec<_>>(),
            "holds": self.holds,
            "timeouts": timeouts,
            "report_pair_holds": self.report_holds,
            "report_fence": self.fenced,
            "ticks": self.ticks,
            "opens": self.agents.iter().map(|a| a.opens.len()).collect::<Vec<_>>(),
            "writes": self.agents.iter().map(|a| a.writes.iter().map(|(i, w)| json!({"input": i.as_str(), "w": w})).collect::<Vec<_>>()).collect::<Vec<_>>(),
        })
    }
}

/// Distance between two tracks' next-tick windows (0 when they overlap).
fn gap(t: &Track, u: &Track) -> i64 {
    (u.lo - t.hi).max(t.lo - u.hi).max(0)
}

/// A cloud `config` message is applied by each agent into ITS OWN
/// settings.json (and the watch baseline moves with it) within this long of
/// the release (broker hop + apply).
pub const CONFIG_APPLY_MS: i64 = 200;

/// The gated input a tape record releases, with its release → write lookahead.
///
/// `config` counts as a settings.json input: a `cli` edit that is still
/// undetected when a config apply rewrites the file is absorbed into the
/// apply's baseline (no `local settings change detected`), so whether an
/// agent's watcher ticked between the edit and the apply decides what it
/// logs and reports (calibration validation run 134720Z: Rust ticked
/// between them, JS did not). Holding both releases off every paired tick
/// keeps the set of ticks between them the same for both agents.
pub fn gated(ev: &xorgate_hal::tape::TapeEvent) -> Option<(Input, i64)> {
    use xorgate_hal::tape::TapeEvent;
    match ev {
        TapeEvent::LteFile { .. } => Some((Input::Lte, LTE_WRITE_MS)),
        TapeEvent::Cli { .. } => Some((Input::Settings, CLI_WRITE_MS)),
        TapeEvent::MqttIn { topic, .. } if topic.ends_with("/config") => {
            Some((Input::Settings, CONFIG_APPLY_MS))
        }
        _ => None,
    }
}

/// Watch `settings.json` (in each agent's home) and the LTE file (in its state
/// dir) with inotify; feed opens to `gate` and log every event to
/// `<label>/reads.ndjson` as `{"w", "input", "ev": "open"|"write"}`.
/// `dirs[i] = (home, state, reads.ndjson path)`. Runs until `stop` is set.
#[cfg(target_os = "linux")]
pub fn observe(
    dirs: Vec<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)>,
    gate: std::sync::Arc<std::sync::Mutex<WatchGate>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    use std::io::Write;
    let ino = Inotify::init(InitFlags::empty())?;
    let flags = AddWatchFlags::IN_OPEN | AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CLOSE_WRITE;
    let mut map = Vec::new();
    let mut logs = Vec::new();
    for (i, (home, state, log)) in dirs.iter().enumerate() {
        map.push((
            ino.add_watch(home, flags)?,
            i,
            Input::Settings,
            "settings.json",
        ));
        map.push((
            ino.add_watch(state, flags)?,
            i,
            Input::Lte,
            "telemetry.json",
        ));
        logs.push(std::fs::File::create(log)?);
    }
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let Ok(events) = ino.read_events() else {
                continue;
            };
            let now = xorgate_hal::tape::wall_now_ms();
            for e in events {
                let Some(&(_, idx, input, name)) = map.iter().find(|m| m.0 == e.wd) else {
                    continue;
                };
                if e.name.as_deref().and_then(|n| n.to_str()) != Some(name) {
                    continue;
                }
                let open = e.mask.contains(AddWatchFlags::IN_OPEN);
                let _ = writeln!(
                    logs[idx],
                    "{}",
                    json!({"w": now, "input": input.as_str(), "ev": if open { "open" } else { "write" }})
                );
                if let Ok(mut g) = gate.lock() {
                    if open {
                        g.on_open(idx, input, now);
                    } else {
                        g.on_write(idx, input, now);
                    }
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CI run 39276719573 shape: a `refresh` at t = 18.1 s arms both
    /// agents' 2 s report debounce; the tape's LTE rewrite falls due at
    /// 20.08 s, ~20 ms before the reports (A 20.12, B 20.9 with delivery lag).
    /// The fence holds it until BOTH reported, never past FENCE_CAP_MS.
    #[test]
    fn report_fence_holds_an_lte_rewrite_until_both_reports() {
        let mut g = WatchGate::new();
        g.on_trigger(18_100);
        let due = 20_080;
        assert_eq!(g.fence(Input::Lte, due, due), Some(true));
        assert_eq!(g.fence(Input::Settings, due, due), None, "LTE only");
        g.on_report(0, 20_120, true);
        assert_eq!(
            g.fence(Input::Lte, due, 20_500),
            Some(true),
            "one report is not enough"
        );
        g.on_report(1, 20_900, true);
        assert_eq!(
            g.fence(Input::Lte, due, 20_901),
            None,
            "both reported: released"
        );
        // Outside the trigger's window [T, T + 2.5 s]: never fenced.
        let mut g = WatchGate::new();
        g.on_trigger(18_100);
        assert_eq!(g.fence(Input::Lte, 20_700, 20_700), None);
        assert_eq!(
            g.fence(Input::Lte, 18_000, 18_100),
            None,
            "due before the trigger"
        );
        // No report ever comes (a command that triggers none): capped.
        assert_eq!(g.fence(Input::Lte, due, due + FENCE_CAP_MS), Some(true));
        assert_eq!(
            g.fence(Input::Lte, due, due + FENCE_CAP_MS + 1),
            Some(false)
        );
        // A report captured BEFORE the trigger does not count.
        let mut g = WatchGate::new();
        g.on_report(0, 18_000, true);
        g.on_report(1, 18_050, true);
        g.on_trigger(18_100);
        assert_eq!(g.fence(Input::Lte, due, due), Some(true));
    }

    /// Release an input due at `due`: step 5 ms until the gate lets it go.
    fn release(g: &mut WatchGate, input: Input, due: i64, look: i64) -> i64 {
        let mut now = due;
        while g.holds(input, now, look) {
            now += 5;
            assert!(now - due < MAX_HOLD_MS, "starved at {due}");
        }
        now
    }

    /// The 022651Z schedule: JS (A) APN ticks at xx:x4.68 drifting ~6.5 ms per
    /// tick, Rust (B) at xx:x4.38; an LTE version due at .333 (between them
    /// once the telemetry gate held it to .490) must wait until both have read.
    #[test]
    fn lte_release_waits_for_both_apn_ticks() {
        let mut g = WatchGate::new();
        // Spawned together at 0; B's anchor at 40 (first publish 120), A's at
        // 335 (first publish 350).
        g.on_spawn(0, 0);
        g.on_spawn(1, 0);
        g.on_publish(1, 120);
        g.on_publish(0, 350);
        let apn_a = |k: i64| 335 + k * APN_WATCH_MS + k * 6;
        let apn_b = |k: i64| 40 + k * APN_WATCH_MS;
        let mut splits = 0;
        let mut max_held = 0;
        // LTE versions every 10 s at a few phases, incl. ones right between.
        let mut dues: Vec<i64> = (0..60)
            .flat_map(|i| [i * 10_000 + 190, i * 10_000 + 2_333, i * 10_000 + 9_990])
            .collect();
        dues.sort();
        let mut last = 0;
        for due in dues {
            let due = due.max(last);
            let r = release(&mut g, Input::Lte, due, LTE_WRITE_MS);
            last = r;
            max_held = max_held.max(r - due);
            for k in 1..22 {
                let (x, y) = (apn_a(k).min(apn_b(k)), apn_a(k).max(apn_b(k)));
                if r + LTE_WRITE_MS >= x && r <= y {
                    splits += 1;
                }
            }
        }
        assert_eq!(splits, 0);
        assert!(max_held < 1_500, "held {max_held} ms");
    }

    /// The CLI writes settings.json ~900 ms (JS) / ~50 ms (Rust) after its
    /// release; the two agents' watchers tick on observed 5 s grids. Each edit
    /// must be detected at the SAME tick pair, never one tick apart.
    #[test]
    fn cli_release_never_splits_a_settings_tick_pair() {
        let mut g = WatchGate::new();
        g.on_spawn(0, 0);
        g.on_spawn(1, 0);
        g.on_publish(1, 120);
        g.on_publish(0, 350);
        // A: watcher grid at 340 (immediate read) drifting 2 ms/tick, plus a
        // second (recording-like) grid at 120; B: watcher at 346 + 5000 m.
        let mut opens: Vec<(i64, usize)> = Vec::new();
        for m in 0..130i64 {
            opens.push((340 + m * 5_000 + 2 * m, 0));
            opens.push((120 + m * 5_000, 0));
            if m > 0 {
                opens.push((346 + m * 5_000, 1));
            }
        }
        opens.sort();
        let tick_after = |w: i64, idx: usize| {
            opens
                .iter()
                .filter(|(o, i)| *i == idx && *o >= w && (*o - 120) % 5_000 != 0)
                .map(|(o, _)| *o)
                .next()
                .unwrap()
        };
        let mut oi = 0;
        let mut feed = |g: &mut WatchGate, upto: i64| {
            while oi < opens.len() && opens[oi].0 <= upto {
                g.on_open(opens[oi].1, Input::Settings, opens[oi].0);
                oi += 1;
            }
        };
        let mut due = 7_000;
        while due < 600_000 {
            feed(&mut g, due);
            let mut now = due;
            loop {
                feed(&mut g, now);
                if !g.holds(Input::Settings, now, CLI_WRITE_MS) {
                    break;
                }
                now += 5;
                assert!(now - due < MAX_HOLD_MS);
            }
            let (ta, tb) = (tick_after(now + 900, 0), tick_after(now + 50, 1));
            assert!(
                (ta - tb).abs() < PAIR_MS,
                "split: release {now}, A {ta}, B {tb}"
            );
            due = now + 3_137;
        }
    }

    #[test]
    fn modem_arrival_chain_is_held_until_modem_seen() {
        let mut g = WatchGate::new();
        g.on_spawn(0, 0);
        g.on_spawn(1, 0);
        g.on_publish(0, 300);
        g.on_publish(1, 100);
        // Both report without sim/uplink at ~2.3 s: chain ticks at ~32.3 s.
        g.on_report(0, 2_310, false);
        g.on_report(1, 2_330, false);
        assert!(g.holds(Input::Lte, 32_300, LTE_WRITE_MS));
        assert!(g.holds(Input::Lte, 62_330, LTE_WRITE_MS));
        // A report with sim latches modemSeen: no more chain ticks.
        g.on_report(0, 63_000, true);
        g.on_report(1, 63_010, true);
        assert!(!g.holds(Input::Lte, 92_330, LTE_WRITE_MS));
        // A later sim-less report does not re-arm (modemSeen latches).
        g.on_report(0, 95_000, false);
        assert!(!g.holds(Input::Lte, 125_000, LTE_WRITE_MS));
    }

    /// Both agents read settings.json at every 1 Hz tick too (drifting
    /// ~5 ms/tick): those reads must not become 5 s watch tracks, and a CLI
    /// release must still find a slot (no starvation).
    #[test]
    fn one_hz_reads_are_not_watch_ticks() {
        let mut g = WatchGate::new();
        g.on_spawn(0, 0);
        g.on_publish(0, 300);
        let mut t = 500;
        let mut w = 4_845;
        let mut now = 0;
        while now < 120_000 {
            now += 5;
            if now >= t {
                g.on_open(0, Input::Settings, t);
                t += 1_005;
            }
            if now >= w {
                g.on_open(0, Input::Settings, w);
                w += 5_002;
            }
        }
        let settings: Vec<_> = g.agents[0]
            .tracks
            .iter()
            .filter(|t| t.kind.input() == Input::Settings)
            .collect();
        assert_eq!(settings.len(), 1, "{settings:?}");
        assert!(settings[0].hits > 10);
        // Free time for a 1.5 s CLI lookahead exists in every 5 s.
        let free = (120_000..125_000)
            .step_by(10)
            .filter(|n| !g.holds(Input::Settings, *n, CLI_WRITE_MS))
            .count();
        assert!(free > 100, "free {free}");
    }

    /// JS: the watcher grid and the recording poll grid ~30 ms apart. Each
    /// keeps one track; the track count stays bounded.
    #[test]
    fn two_close_grids_keep_two_tracks() {
        let mut g = WatchGate::new();
        for m in 0..50i64 {
            g.on_open(0, Input::Settings, 4_910 + m * 5_000);
            g.on_open(0, Input::Settings, 4_942 + m * 5_000);
        }
        let n = g.agents[0]
            .tracks
            .iter()
            .filter(|t| t.kind.input() == Input::Settings)
            .count();
        assert_eq!(n, 2);
    }

    /// Between one agent's config report and the other's, LTE/settings
    /// releases wait; an unmatched report expires after REPORT_PAIR_MS.
    #[test]
    fn report_pair_holds_until_the_counterpart() {
        let mut g = WatchGate::new();
        g.on_report(1, 10_000, true);
        assert!(g.holds(Input::Lte, 10_010, LTE_WRITE_MS));
        assert!(g.holds(Input::Settings, 10_020, 0));
        g.on_report(0, 10_037, true);
        assert!(!g.holds(Input::Lte, 10_040, LTE_WRITE_MS));
        // A report the other agent never matches (reconnect cadence).
        g.on_report(0, 20_000, true);
        assert!(g.holds(Input::Lte, 22_000, LTE_WRITE_MS));
        assert!(!g.holds(Input::Lte, 23_100, LTE_WRITE_MS));
        // A later report of B is not paired with the expired one.
        g.on_report(1, 30_000, true);
        assert!(g.holds(Input::Lte, 30_100, LTE_WRITE_MS));
    }

    #[test]
    fn speculative_settings_tracks_die_without_a_repeat() {
        let mut g = WatchGate::new();
        g.on_open(0, Input::Settings, 1_000); // a one-off (CLI, report build)
        assert!(g.holds(Input::Settings, 6_000, 0));
        assert!(!g.holds(Input::Settings, 11_000, 0));
        assert!(g.agents[0].tracks.is_empty());
    }
}
