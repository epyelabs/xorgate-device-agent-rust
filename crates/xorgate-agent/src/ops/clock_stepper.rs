//! Offline clock stepper — the runtime half of `ts:src/agent/clock-step.ts`
//! @89b5c64 (`createClockStepper`, `:194-316`; `defaultApplyStep`, `:185-192`),
//! Phase 4 WP3 (I6). Every decision is core [`decide`] / [`settle`] (Phase 1,
//! fixtured); this module performs the effects in the TS order:
//!
//! 1. not armed → nothing, and NO NTP probe (CF20);
//! 2. probe NTP (`probeNtpSynced`, the Phase 3 time-source probe says `ntp`)
//!    → synced: disarm (clear the interval) + the stood-down event;
//! 3. ONE `config()` read (CF20); the kill switch / hold-off (MONO) / cap
//!    gates run before any clock source is read, as in the TS;
//! 4. GPS (`gpsClock()`) preferred; only without a valid GPS time is the
//!    modem's NITZ snapshot read (`modemClock(now)`, CF16: one read per tick);
//! 5. [`decide`] (plausibility, threshold, hold-off armed BEFORE the apply);
//! 6. apply = `sudo -n /usr/local/bin/xorgate-setclock <round(target/1000)>`
//!    through [`PrivilegedRunner::run_helper`] (I5: the argv comes only from
//!    `clock_step::setclock_call`);
//! 7. [`settle`] → events, then `onStepped(source, offsetMs)`.
//!
//! `start()` = `setInterval(tick, 30 s)` on [`SetInterval`] (CF55, re-arm
//! from the fire instant) + an immediate first tick; `tick()` is
//! single-flight and turns a panicking dependency into
//! `[clock] tick failed: <msg>` (the TS `catch`, CF20). Real mode only — the
//! worker (WP6) decides whether to build one at all (`worker.ts:536`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{info, warn};
use xorgate_core::clock::Clock;
use xorgate_core::clock_step::{
    decide, setclock_call, settle, tick_failed_message, ClockStepperState, ClockStepperStatus,
    DecideInputs, EventLevel, ModemClockReading, PendingStep, StepResult, TimeSyncConfig,
    CHECK_INTERVAL_MS, HOLD_OFF_MS, TIME_SYNC_DEFAULTS,
};
use xorgate_core::js;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::types::{GpsClockReading, SteppedClockSource};
use xorgate_hal::privileged::{HelperArgv, PrivilegedRunner};

use crate::ctx::BoxFut;
use crate::state::StateHandle;
use crate::timer::SetInterval;

/// An `onEvent(level, msg)` / `log(level, msg)` sink.
pub type EventSink = dyn Fn(EventLevel, &str) + Send + Sync;

/// `ClockStepperOptions` (`clock-step.ts:155-172`) as the worker fills it
/// (`worker.ts:537-552`). `applyStep` is not a seam here: the step always
/// goes through `runner.run_helper` (tests inject a recording runner);
/// `checkIntervalMs`/`holdOffMs` are the core constants (the worker never
/// overrides them).
pub struct ClockStepperOptions {
    /// `resolveTimeSyncConfig(loadSettings().timeSync)` — read ONCE per evaluation.
    pub config: Box<dyn Fn() -> TimeSyncConfig + Send + Sync>,
    /// `source.gpsClock?.() ?? null`.
    pub gps_clock: Box<dyn Fn() -> Option<GpsClockReading> + Send + Sync>,
    /// `readModemClock(now)` over the modem daemon's telemetry snapshot.
    pub modem_clock: Box<dyn Fn(i64) -> BoxFut<'static, Option<ModemClockReading>> + Send + Sync>,
    /// `probeTimeSource() === "ntp"` (`clock-step.ts:181-183`).
    pub probe_ntp: Box<dyn Fn() -> BoxFut<'static, bool> + Send + Sync>,
    pub runner: Arc<dyn PrivilegedRunner>,
    /// `onStepped(source, offsetMs)` — the worker: `timeSource.noteStep` +
    /// `void timeSource.refresh()` (`worker.ts:541-545`, CF58/D26).
    pub on_stepped: Box<dyn Fn(SteppedClockSource, f64) + Send + Sync>,
    /// `onEvent(level, msg)` — the worker: see [`agent_event_sink`].
    pub on_event: Box<EventSink>,
    /// `now` = `wall_ms`, `monoMs` = `mono_ms` (hold-off survives the step).
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// I9 `ntp_probe{synced}` after every probe (addition to the frozen I6
    /// field list; `NullTape` when no tape is configured).
    pub tape: Arc<dyn xorgate_hal::io::TapeSink>,
}

/// `createClockStepper(opts)` (`clock-step.ts:194-316`). Cheap to clone (one
/// shared state).
#[derive(Clone)]
pub struct ClockStepper {
    inner: Arc<Inner>,
}

struct Inner {
    opts: ClockStepperOptions,
    state: Mutex<ClockStepperState>,
    /// `ticking` (`:217`).
    ticking: AtomicBool,
    /// `timer` (`:210`): the interval task.
    timer: Mutex<Option<JoinHandle<()>>>,
}

/// Resets `ticking` even if the tick future is dropped mid-way.
struct TickingGuard<'a>(&'a AtomicBool);
impl Drop for TickingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl ClockStepper {
    pub fn new(opts: ClockStepperOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                opts,
                state: Mutex::new(ClockStepperState::new()),
                ticking: AtomicBool::new(false),
                timer: Mutex::new(None),
            }),
        }
    }

    /// `start()` (`clock-step.ts:289-294`): no-op if running or disarmed;
    /// the interval is armed first, then the first evaluation runs
    /// immediately ("the boot window is the point"). Needs a tokio runtime.
    pub fn start(&self) {
        let mut timer = lock(&self.inner.timer);
        if timer.is_some() || !lock(&self.inner.state).armed {
            return;
        }
        let mut iv = SetInterval::new(Duration::from_millis(CHECK_INTERVAL_MS));
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        *timer = Some(tokio::spawn(async move {
            loop {
                iv.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                // `setInterval(() => void this.tick(), …)`: fire and forget, so
                // a slow tick never delays (only skips, via `ticking`) the next.
                let s = ClockStepper { inner };
                tokio::spawn(async move { s.tick().await });
            }
        }));
        drop(timer);
        let s = self.clone();
        tokio::spawn(async move { s.tick().await });
    }

    /// `stop()` (`clock-step.ts:295-300`).
    pub fn stop(&self) {
        if let Some(t) = lock(&self.inner.timer).take() {
            t.abort();
        }
    }

    /// `tick()` (`clock-step.ts:301-311`): single-flight; a panicking
    /// dependency (the TS `throw`) is caught and logged as
    /// `[clock] tick failed: <message>` (warn), the stepper keeps running.
    ///
    /// Never drop the returned future mid-flight (no `timeout`/`select!`
    /// around it — review-wp3 N1): the evaluation runs as a detached task
    /// while the single-flight guard lives in this future, so dropping it
    /// releases `ticking` while that evaluation keeps running and a second
    /// one could overlap it (a double `sudo` step). The shipped callers
    /// (`start()`'s interval, the immediate first tick) spawn it and run it
    /// to completion.
    pub fn tick(&self) -> BoxFut<'static, ()> {
        let inner = self.inner.clone();
        Box::pin(async move {
            if inner.ticking.swap(true, Ordering::SeqCst) {
                return; // a slow probe/apply must not stack evaluations
            }
            let _guard = TickingGuard(&inner.ticking);
            let eval = inner.clone();
            // The evaluation runs as its own task so a panic is observable
            // (`JoinError::is_panic`) — the TS `catch (err)`.
            if let Err(e) = tokio::spawn(async move { eval.evaluate().await }).await {
                if e.is_panic() {
                    let msg = panic_message(e.into_panic());
                    (inner.opts.on_event)(EventLevel::Warn, &tick_failed_message(&msg));
                }
            }
        })
    }

    /// `status()` (`clock-step.ts:312-314`): a snapshot — safe under
    /// ConfigSync's mutex (the state lock is never held across a callback).
    pub fn status(&self) -> ClockStepperStatus {
        lock(&self.inner.state).status()
    }
}

fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

impl Inner {
    fn emit(&self, events: Vec<xorgate_core::clock_step::ClockEvent>) {
        for e in events {
            (self.opts.on_event)(e.level, &e.msg);
        }
    }

    /// `disarm()` (`clock-step.ts:219-225`).
    fn disarm(&self) {
        if let Some(t) = lock(&self.timer).take() {
            t.abort();
        }
    }

    /// `evaluate()` (`clock-step.ts:226-286`) — see the module doc for the order.
    async fn evaluate(&self) {
        let st = *lock(&self.state);
        if !st.armed {
            return;
        }
        let synced = (self.opts.probe_ntp)().await;
        self.opts.tape.ntp_probe(synced);
        if synced {
            // decide() returns before reading cfg on this branch; the TS does
            // not call `config()` here either.
            let d = decide(
                &st,
                &DecideInputs {
                    ntp_synced: true,
                    cfg: &TIME_SYNC_DEFAULTS,
                    gps: None,
                    modem: None,
                    now: self.opts.clock.wall_ms(),
                    mono: self.opts.clock.mono_ms(),
                    hold_off_ms: HOLD_OFF_MS,
                },
            );
            *lock(&self.state) = d.state;
            self.disarm();
            self.emit(d.events);
            return;
        }
        let cfg = (self.opts.config)();
        // The TS gates (`:235-244`) run before either source is read.
        let mono = self.opts.clock.mono_ms();
        let gates_open =
            cfg.offline_sources && mono >= st.hold_until_mono && st.steps < cfg.max_steps_per_boot;
        let (gps, modem) = if gates_open {
            let gps = (self.opts.gps_clock)();
            if gps.is_some_and(|g| g.valid) {
                (gps, None)
            } else {
                let now = self.opts.clock.wall_ms();
                (gps, (self.opts.modem_clock)(now).await)
            }
        } else {
            (None, None)
        };
        // `now()` / `monoMs()` as the TS reads them after the (possibly
        // awaited) source read: target/offset arithmetic and the hold-off set.
        let d = decide(
            &st,
            &DecideInputs {
                ntp_synced: false,
                cfg: &cfg,
                gps,
                modem,
                now: self.opts.clock.wall_ms(),
                mono: self.opts.clock.mono_ms().max(mono),
                hold_off_ms: HOLD_OFF_MS,
            },
        );
        *lock(&self.state) = d.state;
        self.emit(d.events);
        let Some(step) = d.step else { return };
        let result = self.apply_step(&step).await;
        let now_after = self.opts.clock.wall_ms();
        let st = *lock(&self.state);
        let s = settle(&st, &step, &result, now_after, &cfg);
        *lock(&self.state) = s.state;
        self.emit(s.events);
        if let Some((source, offset_ms)) = s.stepped {
            (self.opts.on_stepped)(source, offset_ms);
        }
    }

    /// `defaultApplyStep(targetMs)` (`clock-step.ts:185-192`): `notFound` →
    /// `sudo not found`; failed → `stderr.trim() || stdout.trim() || exit N`
    /// (`exit null` without a code); else ok. A target the helper would refuse
    /// never reaches sudo (`setclock_call`), its refusal text is the detail.
    async fn apply_step(&self, step: &PendingStep) -> StepResult {
        let call = match setclock_call(step.target_ms) {
            Ok(c) => c,
            Err(detail) => return StepResult { ok: false, detail },
        };
        let argv = HelperArgv::from(&call);
        let res = self.opts.runner.run_helper(&argv).await;
        if res.not_found {
            return StepResult {
                ok: false,
                detail: "sudo not found".to_string(),
            };
        }
        if res.failed {
            let (e, o) = (js::trim(&res.stderr), js::trim(&res.stdout));
            let detail = if !e.is_empty() {
                e.to_string()
            } else if !o.is_empty() {
                o.to_string()
            } else {
                match res.exit_code {
                    Some(c) => format!("exit {c}"),
                    None => "exit null".to_string(),
                }
            };
            return StepResult { ok: false, detail };
        }
        StepResult {
            ok: true,
            detail: String::new(),
        }
    }
}

/// The worker's `onEvent` (`worker.ts:546-551`): `[agent] <msg>` on the
/// console (warn → `console.warn`), then `store.event(level, msg, t)` +
/// `store.flush(t, true)` with one `Date.now()`.
pub fn agent_event_sink(state: StateHandle, clock: Arc<dyn Clock + Send + Sync>) -> Box<EventSink> {
    Box::new(move |level, msg| {
        let t = clock.wall_ms();
        let lvl = match level {
            EventLevel::Warn => {
                warn!("[agent] {msg}");
                ActivityLevel::Warn
            }
            EventLevel::Info => {
                info!("[agent] {msg}");
                ActivityLevel::Info
            }
        };
        state.note(lvl, msg, t);
    })
}

/// `defaultProbeNtpSynced` (`clock-step.ts:181-183`): one RAW
/// `probeTimeSource()` per evaluation (`timedatectl show -p NTPSynchronized
/// --value`, taped as an `exec`) — not the tracker's cached value — `=== "ntp"`.
pub fn system_probe_ntp(
    tape: Arc<dyn xorgate_hal::io::TapeSink>,
) -> Box<dyn Fn() -> BoxFut<'static, bool> + Send + Sync> {
    Box::new(move || {
        let tape = tape.clone();
        Box::pin(async move {
            xorgate_hal::boot_id::probe_time_source_taped(&*tape).await
                == xorgate_core::session::ProbedTimeSource::Ntp
        })
    })
}

/// [`time_source_on_stepped`] plus `clockTrust?.noteStep(stepSource)` —
/// "a stepped clock is a trusted clock" (`worker.ts:546` @ 09da38d), after
/// the time-source note and the background refresh, as in the TS.
pub fn time_source_on_stepped_with_trust(
    ts: crate::recording::time_source::TimeSourceHandle,
    trust: Option<crate::ops::clock_trust::ClockTrust>,
) -> Box<dyn Fn(SteppedClockSource, f64) + Send + Sync> {
    let base = time_source_on_stepped(ts);
    Box::new(move |source, offset_ms| {
        base(source, offset_ms);
        if let Some(t) = &trust {
            t.note_step(source);
        }
    })
}

/// The worker's `onStepped` (`worker.ts:541-545`, CF58 / D26):
/// `timeSource.noteStep(stepSource)` FIRST (future finalizations are stamped
/// honestly at once), then `void timeSource.refresh().catch(() => {})` in the
/// background so the cached value stays coherent if NTP lands at the same
/// time. The offset is not used (TS: `(stepSource) => …`).
pub fn time_source_on_stepped(
    ts: crate::recording::time_source::TimeSourceHandle,
) -> Box<dyn Fn(SteppedClockSource, f64) + Send + Sync> {
    Box::new(move |source, _offset_ms| {
        ts.note_step(source);
        let t = ts.clone();
        tokio::spawn(async move {
            t.refresh().await;
        });
    })
}
