//! Offline clock stepper — the PURE half of `src/agent/clock-step.ts` (316
//! lines): CCLK parsing, the NITZ trust reduction, and the step decision
//! extracted from `createClockStepper().evaluate()` (`clock-step.ts:226-286`).
//!
//! Guards (all mandatory, `clock-step.ts:12-28`): kill switch
//! (`timeSync.offlineSources`), self-disarm once NTP is synchronized this
//! boot, threshold (|offset| > `stepThresholdSecs`), hold-off after ANY
//! attempt + per-boot cap, plausibility window [2025, 2100), and the NITZ
//! trust gate (registered 1/5 only, CCLK years 80–99 → 19xx) — the latter is
//! deliberately fail-CLOSED (invariant 3).
//!
//! What stays out (impure, `clock-step.ts:41-43,181-192,202,291`): the
//! `timedatectl` probe, `readModemTelemetry`, `sudo -n xorgate-setclock`, the
//! hrtime default and the `setInterval` loop. The agent drives the two pure
//! functions [`decide`] (everything up to and including the hold-off that
//! precedes the apply) and [`settle`] (what happens after the apply result
//! is known), passing explicit `now`/`mono` values.
//!
//! `TimeSyncConfig`, `TIME_SYNC_DEFAULTS` and `resolveTimeSyncConfig`
//! (`clock-step.ts:47-66`) were parked in [`crate::settings`] by WP1 (the
//! contract test pins them there); they are re-exported here, not duplicated.

use serde::{Deserialize, Serialize};

use crate::clock::{MonoMs, WallMs};
use crate::js;
pub use crate::settings::{resolve_time_sync_config, TimeSyncConfig, TIME_SYNC_DEFAULTS};
use crate::telemetry::js_num;
use crate::types::{GpsClockReading, ModemTelemetry, SteppedClockSource};

/// `CHECK_INTERVAL_MS` (`clock-step.ts:69`): check cadence while armed.
pub const CHECK_INTERVAL_MS: u64 = 30_000;
/// `HOLD_OFF_MS` (`clock-step.ts:70`): hold-off after any step attempt.
pub const HOLD_OFF_MS: u64 = 5 * 60_000;
/// `MODEM_CLOCK_FRESH_MS` (`clock-step.ts:73`): reject NITZ readings older than this.
pub const MODEM_CLOCK_FRESH_MS: f64 = 5.0 * 60_000.0;
/// `MIN_PLAUSIBLE_MS` (`clock-step.ts:76`): `Date.UTC(2025, 0, 1)`.
pub const MIN_PLAUSIBLE_MS: f64 = 1_735_689_600_000.0;
/// `MAX_PLAUSIBLE_MS` (`clock-step.ts:77`): `Date.UTC(2100, 0, 1)` (exclusive).
pub const MAX_PLAUSIBLE_MS: f64 = 4_102_444_800_000.0;
/// `SETCLOCK_HELPER` (`clock-step.ts:79`).
pub const SETCLOCK_HELPER: &str = "/usr/local/bin/xorgate-setclock";
/// `REGISTERED_STATS` (`clock-step.ts:87`): 3GPP 27.007 <stat> 1 = registered
/// home, 5 = registered roaming. NITZ is network-originated, so an
/// unregistered modem CANNOT have a NITZ-disciplined clock.
pub const REGISTERED_STATS: [i64; 2] = [1, 5];

/// `parseCclkUtcMs` (`clock-step.ts:104-115`): AT+CCLK? value
/// `"yy/MM/dd,hh:mm:ss±zz"` (zz = zone in quarter hours, time is LOCAL) →
/// UTC epoch ms, or `None` on any malformation including a missing zone.
/// Century mapping: 80–99 → 19xx, 00–79 → 20xx, so the SIM7600's factory
/// default `80/01/06` (the GPS epoch) fails plausibility on its own.
pub fn parse_cclk_utc_ms(cclk: &str) -> Option<i64> {
    // /^(\d{2})\/(\d{2})\/(\d{2}),(\d{2}):(\d{2}):(\d{2})([+-]\d{1,2})$/
    let t = js::trim(cclk);
    let b = t.as_bytes();
    let two = |at: usize| -> Option<u32> {
        let s = b.get(at..at + 2)?;
        if s.iter().all(u8::is_ascii_digit) {
            Some(((s[0] - b'0') as u32) * 10 + (s[1] - b'0') as u32)
        } else {
            None
        }
    };
    let lit = |at: usize, c: u8| -> Option<()> { (b.get(at) == Some(&c)).then_some(()) };
    let yy = two(0)?;
    lit(2, b'/')?;
    let mo = two(3)?;
    lit(5, b'/')?;
    let dd = two(6)?;
    lit(8, b',')?;
    let hh = two(9)?;
    lit(11, b':')?;
    let mi = two(12)?;
    lit(14, b':')?;
    let ss = two(15)?;
    let sign = match b.get(17) {
        Some(b'+') => 1.0,
        Some(b'-') => -1.0,
        _ => return None,
    };
    let zone_digits = &b[18..];
    if zone_digits.is_empty()
        || zone_digits.len() > 2
        || !zone_digits.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let zone: f64 = zone_digits
        .iter()
        .fold(0.0, |acc, d| acc * 10.0 + f64::from(d - b'0'));
    if !(1..=12).contains(&mo) || !(1..=31).contains(&dd) {
        return None;
    }
    if hh > 23 || mi > 59 || ss > 60 {
        return None;
    }
    let year = if yy >= 80 { 1900 + yy } else { 2000 + yy };
    let local_as_utc = js::date_utc(
        f64::from(year),
        f64::from(mo - 1),
        f64::from(dd),
        f64::from(hh),
        f64::from(mi),
        f64::from(ss),
    );
    Some((local_as_utc - sign * zone * 15.0 * 60_000.0) as i64)
}

/// `ModemClockReading` (`clock-step.ts:118-121`): the clock offset a usable
/// NITZ reading implies, ms (`parse(cclk) − read_at·1000`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModemClockReading {
    #[serde(with = "js_num")]
    pub offset_ms: f64,
}

/// The pure reduction inside `readModemClock` (`clock-step.ts:138-153`) over
/// an already-read telemetry snapshot (`None` = file absent/unparseable).
///
/// Freshness is judged with the same (possibly wrong) device clock that
/// stamped `read_at`; a `read_at` more than 5 s in the future (we stepped the
/// clock backwards since the read) or older than [`MODEM_CLOCK_FRESH_MS`] is
/// stale. Trust gate (Phase 8): used ONLY when `modem_clock.reg` — falling
/// back to the snapshot's top-level `reg` for pre-Phase-8 daemons — is 1 or
/// 5. Fail CLOSED: no evidence of registration ⇒ `None`.
///
/// Field checks are per field, as the TS `typeof`s (`:144-146`): a
/// non-string `cclk` or non-number `read_at` ⇒ `None`; a `modem_clock.reg`
/// that is not a number (absent, null, `"1"`) falls back to the top-level
/// `reg`, while a NUMBER that is not 1/5 does not fall back. The stat
/// comparison is on the JS number (`Set.has(1.0)` is `has(1)`).
pub fn read_modem_clock(now: WallMs, t: Option<&ModemTelemetry>) -> Option<ModemClockReading> {
    let t = t?;
    let mc = t.modem_clock.as_ref()?;
    let (cclk, read_at) = (mc.cclk.as_deref()?, mc.read_at?);
    let reg = mc.reg.or(t.reg)?;
    if !REGISTERED_STATS.iter().any(|&s| s as f64 == reg) {
        return None;
    }
    let read_at_ms = read_at * 1000.0;
    let age = now as f64 - read_at_ms;
    if age < -5_000.0 || age > MODEM_CLOCK_FRESH_MS {
        return None;
    }
    let utc_ms = parse_cclk_utc_ms(cclk)?;
    Some(ModemClockReading {
        offset_ms: utc_ms as f64 - read_at_ms,
    })
}

/// `ClockStepperStatus.lastStep` (`clock-step.ts:178`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastStep {
    pub source: SteppedClockSource,
    #[serde(with = "js_num")]
    pub offset_ms: f64,
    /// `now()` AFTER the step was applied (the wall clock has moved).
    pub at: WallMs,
}

/// `ClockStepperStatus` (`clock-step.ts:174-179`). `lastStep` is an explicit
/// `null` before the first step, as the TS object literal writes it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockStepperStatus {
    /// false once NTP has been seen synchronized this boot (stood down for good).
    pub armed: bool,
    pub steps: u32,
    pub last_step: Option<LastStep>,
}

/// The closure state of `createClockStepper` (`clock-step.ts:211-216`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockStepperState {
    pub armed: bool,
    pub steps: u32,
    pub last_step: Option<LastStep>,
    /// `holdUntilMono` (`:214`): no evaluation before this monotonic instant.
    pub hold_until_mono: MonoMs,
    /// `capWarned` (`:215`): the cap warning is logged once per boot.
    pub cap_warned: bool,
}

impl Default for ClockStepperState {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockStepperState {
    pub fn new() -> Self {
        Self {
            armed: true,
            steps: 0,
            last_step: None,
            hold_until_mono: 0,
            cap_warned: false,
        }
    }

    /// `status()` (`clock-step.ts:312-314`).
    pub fn status(&self) -> ClockStepperStatus {
        ClockStepperStatus {
            armed: self.armed,
            steps: self.steps,
            last_step: self.last_step,
        }
    }
}

/// `onEvent` level (`clock-step.ts:166`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventLevel {
    Info,
    Warn,
}

/// One `onEvent(level, msg)` call; `msg` wording is consumer-visible (journal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockEvent {
    pub level: EventLevel,
    pub msg: String,
}

impl ClockEvent {
    fn info(msg: String) -> Self {
        Self {
            level: EventLevel::Info,
            msg,
        }
    }
    fn warn(msg: String) -> Self {
        Self {
            level: EventLevel::Warn,
            msg,
        }
    }
}

/// Everything one `evaluate()` pass reads (`clock-step.ts:228-257`), as values.
///
/// `modem` is the NITZ reading the TS fetched lazily (`:254`, only when no
/// valid GPS time exists); the agent may skip the telemetry read when
/// `gps.valid` — the decision is identical either way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecideInputs<'a> {
    /// `probeNtpSynced()` result.
    pub ntp_synced: bool,
    /// `opts.config()` — live-resolved each tick.
    pub cfg: &'a TimeSyncConfig,
    /// `opts.gpsClock()`.
    pub gps: Option<GpsClockReading>,
    /// `modemClock(now())`.
    pub modem: Option<ModemClockReading>,
    /// `now()`.
    pub now: WallMs,
    /// `monoMs()`.
    pub mono: MonoMs,
    /// `opts.holdOffMs ?? HOLD_OFF_MS` (`:207`).
    pub hold_off_ms: u64,
}

/// A step the agent must now apply (`applyStep(targetMs)`, `clock-step.ts:272`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingStep {
    pub source: SteppedClockSource,
    /// Epoch ms to set the clock to.
    pub target_ms: f64,
    /// `targetMs - now()` at decision time.
    pub offset_ms: f64,
}

/// The outcome of [`decide`].
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub state: ClockStepperState,
    /// `Some` ⇒ call the setclock helper, then [`settle`].
    pub step: Option<PendingStep>,
    pub events: Vec<ClockEvent>,
}

fn source_label(s: SteppedClockSource) -> &'static str {
    match s {
        SteppedClockSource::Gps => "gps",
        SteppedClockSource::Nitz => "nitz",
    }
}

/// `[clock] NTP synchronized …` (`clock-step.ts:232`).
pub const STOOD_DOWN_MSG: &str =
    "[clock] NTP synchronized — offline clock stepping stood down for this boot";

/// `evaluate()` up to the apply (`clock-step.ts:226-271`), as a pure function
/// over the closure state. Line by line:
/// - `:227` not armed → nothing;
/// - `:228-233` NTP synced → disarm for the boot + info event;
/// - `:235-236` kill switch off → nothing (stays armed);
/// - `:237` inside the hold-off → nothing;
/// - `:238-244` cap reached → one warn per boot, nothing;
/// - `:246-258` GPS (valid) preferred: target = `utcMs + ageMs`; else NITZ:
///   target = `now + offsetMs`; neither → nothing;
/// - `:260-264` implausible target → warn (ISO time) + hold-off;
/// - `:266-267` |offset| ≤ threshold → nothing;
/// - `:269-271` hold-off is armed BEFORE the apply, success or failure.
pub fn decide(state: &ClockStepperState, inputs: &DecideInputs) -> Decision {
    let mut st = *state;
    let mut events = Vec::new();
    let done = |st: ClockStepperState, events: Vec<ClockEvent>| Decision {
        state: st,
        step: None,
        events,
    };
    if !st.armed {
        return done(st, events);
    }
    if inputs.ntp_synced {
        // Once NTP has synced this boot the kernel flag stays set — this is
        // the permanent per-boot stand-down, agent restarts included.
        st.armed = false;
        events.push(ClockEvent::info(STOOD_DOWN_MSG.to_string()));
        return done(st, events);
    }
    let cfg = inputs.cfg;
    if !cfg.offline_sources {
        return done(st, events); // kill switch — stay armed, do nothing
    }
    if inputs.mono < st.hold_until_mono {
        return done(st, events);
    }
    if st.steps >= cfg.max_steps_per_boot {
        if !st.cap_warned {
            st.cap_warned = true;
            events.push(ClockEvent::warn(format!(
                "[clock] step cap reached ({}/{} this boot) — no further offline steps",
                st.steps, cfg.max_steps_per_boot
            )));
        }
        return done(st, events);
    }

    // GPS preferred; NITZ only when no valid fresh GPS time exists.
    let (source, target_ms) = match (inputs.gps, inputs.modem) {
        (Some(gps), _) if gps.valid => (
            SteppedClockSource::Gps,
            gps.utc_ms as f64 + gps.age_ms as f64,
        ),
        (_, Some(modem)) => (
            SteppedClockSource::Nitz,
            inputs.now as f64 + modem.offset_ms,
        ),
        _ => return done(st, events), // neither source usable — nothing to do this tick
    };

    if target_ms < MIN_PLAUSIBLE_MS || target_ms >= MAX_PLAUSIBLE_MS {
        events.push(ClockEvent::warn(format!(
            "[clock] ignoring implausible {} time {}",
            source_label(source),
            js::to_iso_string(target_ms)
        )));
        st.hold_until_mono = inputs.mono + inputs.hold_off_ms; // don't re-log a bad source every 30 s
        return done(st, events);
    }

    let offset_ms = target_ms - inputs.now as f64;
    if offset_ms.abs() <= f64::from(cfg.step_threshold_secs) * 1000.0 {
        return done(st, events);
    }

    // Hold off after ANY attempt, success or failure — a broken sudoers must
    // not be retried (and re-logged) every 30 seconds.
    st.hold_until_mono = inputs.mono + inputs.hold_off_ms;
    Decision {
        state: st,
        step: Some(PendingStep {
            source,
            target_ms,
            offset_ms,
        }),
        events,
    }
}

/// `applyStep`'s result (`clock-step.ts:163`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub ok: bool,
    pub detail: String,
}

/// The outcome of [`settle`].
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    pub state: ClockStepperState,
    pub events: Vec<ClockEvent>,
    /// `onStepped(source, offsetMs)` (`clock-step.ts:285`), when the step landed.
    pub stepped: Option<(SteppedClockSource, f64)>,
}

/// `evaluate()` after the apply (`clock-step.ts:273-285`): a failure is
/// logged without counting a step (the hold-off from [`decide`] stands); a
/// success counts, records `lastStep` with `at = now()` read AFTER the clock
/// moved, and logs the signed step in seconds (`toFixed(1)`).
pub fn settle(
    state: &ClockStepperState,
    step: &PendingStep,
    result: &StepResult,
    now_after: WallMs,
    cfg: &TimeSyncConfig,
) -> Settled {
    let mut st = *state;
    let mut events = Vec::new();
    if !result.ok {
        events.push(ClockEvent::warn(format!(
            "[clock] step from {} failed: {}",
            source_label(step.source),
            result.detail
        )));
        return Settled {
            state: st,
            events,
            stepped: None,
        };
    }
    st.steps += 1;
    st.last_step = Some(LastStep {
        source: step.source,
        offset_ms: step.offset_ms,
        at: now_after,
    });
    let secs = js::to_fixed(step.offset_ms / 1000.0, 1);
    events.push(ClockEvent::info(format!(
        "[clock] stepped system clock {}{}s from {} (NTP unsynced; step {}/{} this boot)",
        if step.offset_ms >= 0.0 { "+" } else { "" },
        secs,
        source_label(step.source),
        st.steps,
        cfg.max_steps_per_boot
    )));
    Settled {
        state: st,
        events,
        stepped: Some((step.source, step.offset_ms)),
    }
}

/// `defaultApplyStep`'s argv (`clock-step.ts:186`): the arguments after
/// `sudo` — `-n <helper> <epoch seconds>` with `String(Math.round(targetMs / 1000))`.
pub fn setclock_argv(target_ms: f64) -> [String; 3] {
    let secs = crate::round::js_math_round(target_ms / 1000.0);
    [
        "-n".to_string(),
        SETCLOCK_HELPER.to_string(),
        format!("{}", secs as i64),
    ]
}

/// `tick()`'s catch-all wording (`clock-step.ts:307`).
pub fn tick_failed_message(err: &str) -> String {
    format!("[clock] tick failed: {err}")
}

/// `system/xorgate-setclock:17` — its usage line (stderr, exit 2).
pub const SETCLOCK_USAGE: &str = "usage: xorgate-setclock <epoch-seconds>";
/// The helper's own lower bound (`system/xorgate-setclock:20`), epoch s.
pub const SETCLOCK_MIN_EPOCH_S: u64 = 1_735_689_600;
/// The helper's own upper bound, exclusive (`system/xorgate-setclock:21`).
pub const SETCLOCK_MAX_EPOCH_S: u64 = 4_102_444_800;

/// The ONLY mint of a `xorgate-setclock` [`HelperCall`] (I5):
/// `defaultApplyStep(targetMs)` (`clock-step.ts:185-186`) with the helper's
/// own validation (`system/xorgate-setclock:15-21`) applied first.
///
/// The epoch is `String(Math.round(targetMs / 1000))`, exactly the argument
/// the TS passes. `Err` = what the helper would print on stderr (exit 2) for
/// that argument — the caller turns it into the same
/// `[clock] step from <src> failed: <detail>` the TS logs after the helper
/// refused it. Only reachable at the rounding edge of the plausibility window
/// ([`decide`] already refuses targets outside [2025, 2100)): e.g. a target
/// 400 ms before 2100 rounds to 4102444800 and the helper says
/// `refusing epoch 4102444800 (after 2100)`. The sudo call itself is skipped
/// (no journal line) — the detail is identical.
pub fn setclock_call(target_ms: f64) -> Result<crate::commands::HelperCall, String> {
    let secs = crate::round::js_math_round(target_ms / 1000.0);
    // `case "$epoch" in ''|*[!0-9]*)`: NaN, a minus sign, or an exponent
    // (JS prints integers >= 1e21 as `1e+21`) are not all-digits.
    if !secs.is_finite() || secs < 0.0 || secs >= 1e21 {
        return Err(SETCLOCK_USAGE.to_string());
    }
    // An integral f64 < 1e21 is exact in u128 and prints as JS `String()` does.
    let digits = secs as u128;
    if digits < u128::from(SETCLOCK_MIN_EPOCH_S) {
        return Err(format!("refusing epoch {digits} (before 2025)"));
    }
    if digits >= u128::from(SETCLOCK_MAX_EPOCH_S) {
        return Err(format!("refusing epoch {digits} (after 2100)"));
    }
    Ok(crate::commands::HelperCall::mint(
        crate::commands::HelperKind::SetClock {
            epoch_s: digits as u64,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plausibility_constants_are_date_utc() {
        assert_eq!(
            js::date_utc(2025.0, 0.0, 1.0, 0.0, 0.0, 0.0),
            MIN_PLAUSIBLE_MS
        );
        assert_eq!(
            js::date_utc(2100.0, 0.0, 1.0, 0.0, 0.0, 0.0),
            MAX_PLAUSIBLE_MS
        );
    }

    #[test]
    fn status_shape() {
        let s = ClockStepperState::new().status();
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"armed":true,"steps":0,"lastStep":null}"#
        );
        let s = ClockStepperStatus {
            armed: true,
            steps: 1,
            last_step: Some(LastStep {
                source: SteppedClockSource::Gps,
                offset_ms: 300_000.0,
                at: 1_785_326_700_000,
            }),
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"armed":true,"steps":1,"lastStep":{"source":"gps","offsetMs":300000,"at":1785326700000}}"#
        );
    }

    #[test]
    fn setclock_argv_rounds_to_seconds() {
        assert_eq!(
            setclock_argv(1_785_326_700_499.0),
            ["-n", SETCLOCK_HELPER, "1785326700"]
        );
        assert_eq!(setclock_argv(1_500.0)[2], "2");
    }

    #[test]
    fn setclock_call_applies_the_helpers_own_window() {
        // Inside the window: the TS argument, `sudo -n` first.
        let c = setclock_call(MIN_PLAUSIBLE_MS).unwrap();
        assert_eq!(c.argv(), ["sudo", "-n", SETCLOCK_HELPER, "1735689600"]);
        assert_eq!(
            setclock_call(MAX_PLAUSIBLE_MS - 501.0)
                .unwrap()
                .helper_args(),
            ["4102444799"]
        );
        // Rounding edge: decide() lets it through, the helper refuses it.
        assert_eq!(
            setclock_call(MAX_PLAUSIBLE_MS - 400.0).unwrap_err(),
            "refusing epoch 4102444800 (after 2100)"
        );
        assert_eq!(
            setclock_call(MIN_PLAUSIBLE_MS - 501.0).unwrap_err(),
            "refusing epoch 1735689599 (before 2025)"
        );
        assert_eq!(
            setclock_call(-400.0).unwrap_err(),
            "refusing epoch 0 (before 2025)"
        ); // Math.round(-0.4) = -0, String(-0) = "0"
        assert_eq!(setclock_call(-1_000.0).unwrap_err(), SETCLOCK_USAGE);
        assert_eq!(setclock_call(f64::NAN).unwrap_err(), SETCLOCK_USAGE);
        assert_eq!(setclock_call(1e24).unwrap_err(), SETCLOCK_USAGE);
    }
}
