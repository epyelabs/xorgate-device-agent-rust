//! Clock trust gate — the PURE half of `src/agent/clock-trust.ts` (JS @
//! 09da38d, 195 lines): a sans-I/O state machine over injected monotonic ms
//! (the TS `monoMs` is already monotonic, so no D-row is needed).
//!
//! A battery-less CM5 boots on the fake-hwclock / timesyncd restore stamp and
//! stays there until NTP (or a GPS/NITZ step) lands. The live telemetry
//! publish and the telemetry recorder are HELD until the clock is trusted,
//! for at most [`CLOCK_TRUST_GRACE_MS`]; past that the clock is used anyway,
//! flagged ("fallback": a warn event and the `clockTrust` report hint).
//!
//! Trust sources, any of which latches for the process: `timedatectl
//! NTPSynchronized=yes` (polled every [`CLOCK_TRUST_POLL_MS`] while held,
//! every [`CLOCK_TRUST_SLOW_POLL_MS`] in fallback) and a GPS/NITZ step
//! applied by the clock stepper ([`ClockTrustCore::note_step`]). An `rtc`
//! probe answer is NOT trust.
//!
//! The runtime half (`xorgate_agent::ops::clock_trust`) owns the timer and
//! the probe; every decision and every event string is here. Each method
//! returns the [`TrustOutput`]s the TS performs inline, in its order.

use serde::{Deserialize, Serialize};

use crate::js::to_fixed;
use crate::session::ProbedTimeSource;
use crate::types::SteppedClockSource;

/// `CLOCK_TRUST_DEFAULTS.graceMs` (`clock-trust.ts:76`): hold at most 5 min.
pub const CLOCK_TRUST_GRACE_MS: u64 = 5 * 60_000;
/// `CLOCK_TRUST_DEFAULTS.pollMs` (`:77`): probe cadence while held.
pub const CLOCK_TRUST_POLL_MS: u64 = 5_000;
/// `CLOCK_TRUST_DEFAULTS.slowPollMs` (`:78`): probe cadence in fallback.
pub const CLOCK_TRUST_SLOW_POLL_MS: u64 = 30_000;

/// How trust was established (`ClockTrustSource`, `clock-trust.ts:39`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockTrustSource {
    Ntp,
    Gps,
    Nitz,
}

impl ClockTrustSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ClockTrustSource::Ntp => "ntp",
            ClockTrustSource::Gps => "gps",
            ClockTrustSource::Nitz => "nitz",
        }
    }
}

impl From<SteppedClockSource> for ClockTrustSource {
    fn from(s: SteppedClockSource) -> Self {
        match s {
            SteppedClockSource::Gps => ClockTrustSource::Gps,
            SteppedClockSource::Nitz => ClockTrustSource::Nitz,
        }
    }
}

/// One word for the config report hint and the status line
/// (`ClockTrustState`, `clock-trust.ts:42`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockTrustState {
    Trusted,
    Held,
    Fallback,
}

impl ClockTrustState {
    pub fn as_str(self) -> &'static str {
        match self {
            ClockTrustState::Trusted => "trusted",
            ClockTrustState::Held => "held",
            ClockTrustState::Fallback => "fallback",
        }
    }
}

/// `ClockTrustStatus` (`clock-trust.ts:44-55`). Key order = the TS literal
/// of `status()` (`:203-209`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockTrustStatus {
    /// Producers may stamp with the wall clock (synced, or past the grace window).
    pub trusted: bool,
    pub state: ClockTrustState,
    /// How trust was established; `null` while held or in fallback.
    pub source: Option<ClockTrustSource>,
    /// Telemetry publishes withheld so far this process.
    pub held: u64,
    /// Monotonic ms the hold lasted (frozen once trusted or in fallback).
    pub held_ms: u64,
}

/// The `onEvent` level (`"info" | "warn"`, `clock-trust.ts:64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustEventLevel {
    Info,
    Warn,
}

/// What a [`ClockTrustCore`] call asks the runtime to do, in TS order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustOutput {
    /// `onEvent(level, msg)`: the worker logs `[agent] <msg>` (console.warn
    /// for warn, console.log otherwise) and notes the event (`worker.ts:476-483`).
    Event(TrustEventLevel, String),
    /// `clearInterval(timer); timer = null` — trust latched (`:122-125`).
    StopTimer,
    /// `clearInterval(timer); timer = setInterval(tick, slowPollMs)` — the
    /// fallback flip, only when a timer is running (`:145-149`).
    SlowPoll,
}

/// `secs(ms)` (`clock-trust.ts:107-109`): `(ms / 1000).toFixed(0)`.
pub fn trust_secs(ms: u64) -> String {
    to_fixed(ms as f64 / 1000.0, 0)
}

/// `[clock] <src> synchronized — clock trusted (telemetry held N s, M sample(s))`
/// (`clock-trust.ts:129`).
pub fn trusted_event(src: ClockTrustSource, held_ms: u64, held: u64) -> String {
    format!(
        "[clock] {} synchronized — clock trusted (telemetry held {} s, {held} sample(s))",
        src.as_str(),
        trust_secs(held_ms)
    )
}

/// `[clock] <src> synchronized N s after boot — clock trusted (published M s
/// on an untrusted clock)` (`clock-trust.ts:128`): the late sync after a fallback.
pub fn late_trusted_event(src: ClockTrustSource, since_start_ms: u64, untrusted_ms: u64) -> String {
    format!(
        "[clock] {} synchronized {} s after boot — clock trusted (published {} s on an untrusted clock)",
        src.as_str(),
        trust_secs(since_start_ms),
        trust_secs(untrusted_ms)
    )
}

/// The fallback warn (`clock-trust.ts:141-144`).
pub fn fallback_event(elapsed_ms: u64, held: u64) -> String {
    format!(
        "[clock] no NTP/GPS/NITZ sync {} s after start — publishing on an UNTRUSTED clock ({held} sample(s) held); timestamps may be early until a sync lands",
        trust_secs(elapsed_ms)
    )
}

/// `[clock] trust probe failed: <err.message>` (`clock-trust.ts:159`).
pub fn probe_failed_event(message: &str) -> String {
    format!("[clock] trust probe failed: {message}")
}

/// The `createClockTrust` closure state (`clock-trust.ts:94-105`) without the
/// timer, the probe and the `ticking`/`stopped` flags (runtime concerns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockTrustCore {
    grace_ms: u64,
    started_mono: u64,
    source: Option<ClockTrustSource>,
    fallback: bool,
    held: u64,
    held_ms: u64,
}

impl ClockTrustCore {
    /// `createClockTrust(...)`: `startedMono = monoMs()` at construction.
    pub fn new(started_mono: u64, grace_ms: u64) -> Self {
        Self {
            grace_ms,
            started_mono,
            source: None,
            fallback: false,
            held: 0,
            held_ms: 0,
        }
    }

    /// `source !== null` — latched; `tick()` and `start()` do nothing then.
    pub fn latched(&self) -> bool {
        self.source.is_some()
    }

    /// Whether the fallback flag is up (sticky until trust latches).
    pub fn in_fallback(&self) -> bool {
        self.fallback
    }

    fn elapsed(&self, now_mono: u64) -> u64 {
        now_mono.saturating_sub(self.started_mono)
    }

    /// `establish(src)` (`clock-trust.ts:111-131`).
    pub fn establish(&mut self, src: ClockTrustSource, now_mono: u64) -> Vec<TrustOutput> {
        if self.source.is_some() {
            return Vec::new();
        }
        self.source = Some(src);
        let was = self.fallback;
        self.fallback = false;
        if !was {
            self.held_ms = self.elapsed(now_mono);
        }
        let since = self.elapsed(now_mono);
        let msg = if was {
            late_trusted_event(src, since, since.saturating_sub(self.held_ms))
        } else {
            trusted_event(src, self.held_ms, self.held)
        };
        vec![
            TrustOutput::StopTimer,
            TrustOutput::Event(TrustEventLevel::Info, msg),
        ]
    }

    /// `checkGrace()` (`clock-trust.ts:134-150`): flips to fallback once,
    /// with its warn event, and asks for the slow poll.
    pub fn check_grace(&mut self, now_mono: u64) -> Vec<TrustOutput> {
        if self.source.is_some() || self.fallback {
            return Vec::new();
        }
        let elapsed = self.elapsed(now_mono);
        if elapsed < self.grace_ms {
            return Vec::new();
        }
        self.fallback = true;
        self.held_ms = elapsed;
        vec![
            TrustOutput::Event(TrustEventLevel::Warn, fallback_event(elapsed, self.held)),
            TrustOutput::SlowPoll,
        ]
    }

    /// The body of `tick()` after `await probe()` (`clock-trust.ts:152-165`):
    /// `ntp` establishes, anything else checks the grace; a thrown probe
    /// (`Err(message)`) warns, then checks the grace. `now_mono` is read
    /// after the probe resolved, like the TS.
    pub fn on_probe(
        &mut self,
        answer: Result<ProbedTimeSource, String>,
        now_mono: u64,
    ) -> Vec<TrustOutput> {
        match answer {
            Ok(ProbedTimeSource::Ntp) => self.establish(ClockTrustSource::Ntp, now_mono),
            Ok(_) => self.check_grace(now_mono),
            Err(message) => {
                let mut out = vec![TrustOutput::Event(
                    TrustEventLevel::Warn,
                    probe_failed_event(&message),
                )];
                out.extend(self.check_grace(now_mono));
                out
            }
        }
    }

    /// `trusted()` (`clock-trust.ts:185-189`): true once latched; else
    /// `checkGrace()` THEN the fallback flag — so the flip happens on the
    /// call that crosses the window (the publish tick).
    pub fn trusted(&mut self, now_mono: u64) -> (bool, Vec<TrustOutput>) {
        if self.source.is_some() {
            return (true, Vec::new());
        }
        let out = self.check_grace(now_mono);
        (self.fallback, out)
    }

    /// `noteHeld()` (`clock-trust.ts:190-192`).
    pub fn note_held(&mut self) {
        self.held += 1;
    }

    /// `noteStep(src)` (`clock-trust.ts:193-195`).
    pub fn note_step(&mut self, src: SteppedClockSource, now_mono: u64) -> Vec<TrustOutput> {
        self.establish(src.into(), now_mono)
    }

    /// `status()` (`clock-trust.ts:196-210`): calls `trusted()` first (which
    /// may flip to fallback), then reads the state.
    pub fn status(&mut self, now_mono: u64) -> (ClockTrustStatus, Vec<TrustOutput>) {
        let (trusted, out) = self.trusted(now_mono);
        let state = if self.source.is_some() {
            ClockTrustState::Trusted
        } else if self.fallback {
            ClockTrustState::Fallback
        } else {
            ClockTrustState::Held
        };
        let held_ms = if self.source.is_some() || self.fallback {
            self.held_ms
        } else {
            self.elapsed(now_mono)
        };
        (
            ClockTrustStatus {
                trusted,
                state,
                source: self.source,
                held: self.held,
                held_ms,
            },
            out,
        )
    }
}

#[cfg(test)]
mod tests {
    //! The six `it()` of `test/clock-trust.test.ts` @ 09da38d, on the core
    //! machine (the runtime's `tick()` skips the probe once latched; the rig
    //! below models that guard). Byte-exact event texts are pinned by
    //! `fixtures/clock_trust/clock-trust.json` (`tests/clock_trust_fixtures.rs`).
    use super::*;

    struct Rig {
        core: ClockTrustCore,
        mono: u64,
        answers: Vec<Result<ProbedTimeSource, String>>,
        probes: usize,
        events: Vec<(TrustEventLevel, String)>,
    }

    impl Rig {
        fn new(answers: Vec<Result<ProbedTimeSource, String>>, grace: u64, mono: u64) -> Self {
            Self {
                core: ClockTrustCore::new(mono, grace),
                mono,
                answers,
                probes: 0,
                events: Vec::new(),
            }
        }
        fn take(&mut self, out: Vec<TrustOutput>) {
            for o in out {
                if let TrustOutput::Event(l, m) = o {
                    self.events.push((l, m));
                }
            }
        }
        fn tick(&mut self) {
            if self.core.latched() {
                return;
            }
            self.probes += 1;
            let a = self.answers[self.probes.min(self.answers.len()) - 1].clone();
            let out = self.core.on_probe(a, self.mono);
            self.take(out);
        }
        fn trusted(&mut self) -> bool {
            let (t, out) = self.core.trusted(self.mono);
            self.take(out);
            t
        }
        fn status(&mut self) -> ClockTrustStatus {
            let (s, out) = self.core.status(self.mono);
            self.take(out);
            s
        }
    }

    use ProbedTimeSource::{Ntp, Rtc, Unsynced};

    #[test]
    fn holds_until_ntp_then_latches_with_one_info_event() {
        let mut r = Rig::new(vec![Ok(Unsynced), Ok(Unsynced), Ok(Ntp)], 300_000, 1_000);
        assert!(!r.trusted());
        let s = r.status();
        assert_eq!(
            (s.trusted, s.state, s.source, s.held),
            (false, ClockTrustState::Held, None, 0)
        );
        r.core.note_held();
        r.core.note_held();
        r.tick();
        r.mono += 5_000;
        r.tick();
        assert!(!r.trusted());
        assert!(r.events.is_empty());
        r.mono += 5_000;
        r.tick();
        assert!(r.trusted());
        assert_eq!(
            r.status(),
            ClockTrustStatus {
                trusted: true,
                state: ClockTrustState::Trusted,
                source: Some(ClockTrustSource::Ntp),
                held: 2,
                held_ms: 10_000
            }
        );
        assert_eq!(
            r.events,
            vec![(
                TrustEventLevel::Info,
                "[clock] ntp synchronized — clock trusted (telemetry held 10 s, 2 sample(s))"
                    .into()
            )]
        );
        let before = r.probes;
        r.tick();
        assert_eq!(r.probes, before);
        assert_eq!(r.events.len(), 1);
    }

    #[test]
    fn rtc_is_not_trust_but_a_gps_step_is() {
        let mut r = Rig::new(vec![Ok(Rtc)], 300_000, 1_000);
        r.tick();
        assert!(!r.trusted());
        let out = r.core.note_step(SteppedClockSource::Gps, r.mono);
        r.take(out);
        assert!(r.trusted());
        assert_eq!(r.status().source, Some(ClockTrustSource::Gps));
        assert!(r.events[0].1.starts_with("[clock] gps synchronized"));
    }

    #[test]
    fn falls_back_after_the_grace_window_with_one_warn() {
        let mut r = Rig::new(vec![Ok(Unsynced)], 60_000, 1_000);
        r.tick();
        r.core.note_held();
        r.mono += 59_999;
        assert!(!r.trusted());
        r.mono += 1;
        assert!(r.trusted());
        assert_eq!(
            r.status(),
            ClockTrustStatus {
                trusted: true,
                state: ClockTrustState::Fallback,
                source: None,
                held: 1,
                held_ms: 60_000
            }
        );
        assert_eq!(
            r.events,
            vec![(
                TrustEventLevel::Warn,
                "[clock] no NTP/GPS/NITZ sync 60 s after start — publishing on an UNTRUSTED clock (1 sample(s) held); timestamps may be early until a sync lands".into()
            )]
        );
        r.mono += 100_000;
        assert!(r.trusted());
        assert_eq!(r.events.len(), 1);
    }

    #[test]
    fn ntp_after_fallback_clears_the_flag_and_says_how_long() {
        let mut r = Rig::new(vec![Ok(Unsynced), Ok(Ntp)], 60_000, 1_000);
        r.tick();
        r.mono += 60_000;
        assert!(r.trusted());
        assert_eq!(r.status().state, ClockTrustState::Fallback);
        r.mono += 30_000;
        r.tick();
        let s = r.status();
        assert_eq!(
            (s.state, s.source, s.held_ms),
            (
                ClockTrustState::Trusted,
                Some(ClockTrustSource::Ntp),
                60_000
            )
        );
        let levels: Vec<_> = r.events.iter().map(|e| e.0).collect();
        assert_eq!(levels, [TrustEventLevel::Warn, TrustEventLevel::Info]);
        assert_eq!(
            r.events[1].1,
            "[clock] ntp synchronized 90 s after boot — clock trusted (published 30 s on an untrusted clock)"
        );
    }

    #[test]
    fn a_throwing_probe_counts_toward_the_grace_window() {
        let mut r = Rig::new(vec![Err("timedatectl: boom".into())], 1_000, 0);
        r.tick();
        assert!(!r.trusted());
        r.mono = 1_000;
        r.tick();
        assert!(r.trusted());
        assert_eq!(
            r.events[0].1,
            "[clock] trust probe failed: timedatectl: boom"
        );
    }

    #[test]
    fn defaults_5_min_grace_5_s_poll_30_s_slow_poll() {
        assert_eq!(
            (
                CLOCK_TRUST_GRACE_MS,
                CLOCK_TRUST_POLL_MS,
                CLOCK_TRUST_SLOW_POLL_MS
            ),
            (300_000, 5_000, 30_000)
        );
    }

    #[test]
    fn secs_is_to_fixed_0() {
        assert_eq!(trust_secs(10_000), "10");
        assert_eq!(trust_secs(2_500), "3"); // a tie rounds up, like toFixed
        assert_eq!(trust_secs(1_499), "1");
        assert_eq!(trust_secs(0), "0");
    }
}
