//! Clock trust gate — the runtime half of `ts:src/agent/clock-trust.ts` @
//! 09da38d (`createClockTrust`, `:84-212`). Every decision and event string
//! is core [`ClockTrustCore`]; this module owns the probe timer and performs
//! the core's [`TrustOutput`]s in order.
//!
//! - `start()` = `setInterval(tick, 5 s)` on [`SetInterval`] (re-arm from
//!   the fire, P-WP6-1) + an immediate first tick;
//! - trust latched → the timer is cleared; the fallback flip (on a probe OR
//!   on a `trusted()` call that crosses the 5 min grace) swaps it for a 30 s
//!   one, only while a timer runs (`if (timer)`, `:145-149`);
//! - `tick()` is single-flight (`ticking`) and skips once stopped or latched;
//! - `trusted()` runs the grace check on every call, so the fallback flips at
//!   the publish tick that crosses the window.
//!
//! Real mode only: the worker never builds one in sim (`worker.ts:476`).
//! All intervals are measured on the injected MONOTONIC clock (the TS
//! `monoMs` is monotonic too).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use tokio::task::JoinHandle;
use xorgate_core::clock::Clock;
use xorgate_core::clock_step::EventLevel;
use xorgate_core::clock_trust::{
    ClockTrustCore, ClockTrustStatus, TrustEventLevel, TrustOutput, CLOCK_TRUST_GRACE_MS,
    CLOCK_TRUST_POLL_MS, CLOCK_TRUST_SLOW_POLL_MS,
};
use xorgate_core::session::ProbedTimeSource;
use xorgate_core::types::SteppedClockSource;

use crate::ctx::BoxFut;
use crate::ops::clock_stepper::EventSink;
use crate::timer::SetInterval;

/// The `NTPSynchronized` probe; `Err(message)` = the TS probe threw.
pub type TrustProbe =
    Box<dyn Fn() -> BoxFut<'static, Result<ProbedTimeSource, String>> + Send + Sync>;

/// `ClockTrustOptions` (`clock-trust.ts:57-70`).
pub struct ClockTrustOptions {
    /// `probeTimeSource()` (`xorgate_hal::boot_id::probe_time_source_taped`).
    pub probe: TrustProbe,
    pub grace_ms: u64,
    pub poll_ms: u64,
    pub slow_poll_ms: u64,
    /// `monoMs` = `clock.mono_ms()` (a clock step must not shorten the grace).
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// `onEvent(level, msg)` — the worker: [`crate::ops::clock_stepper::agent_event_sink`].
    pub on_event: Box<EventSink>,
}

impl ClockTrustOptions {
    /// The TS defaults (`CLOCK_TRUST_DEFAULTS`, `:75-79`).
    pub fn new(
        probe: TrustProbe,
        clock: Arc<dyn Clock + Send + Sync>,
        on_event: Box<EventSink>,
    ) -> Self {
        Self {
            probe,
            grace_ms: CLOCK_TRUST_GRACE_MS,
            poll_ms: CLOCK_TRUST_POLL_MS,
            slow_poll_ms: CLOCK_TRUST_SLOW_POLL_MS,
            clock,
            on_event,
        }
    }
}

/// `createClockTrust(opts)`. Cheap to clone (one shared state).
#[derive(Clone)]
pub struct ClockTrust {
    inner: Arc<Inner>,
}

struct Inner {
    opts: ClockTrustOptions,
    core: Mutex<ClockTrustCore>,
    /// `timer` (`:101`): the interval task.
    timer: Mutex<Option<JoinHandle<()>>>,
    /// `ticking` (`:102`).
    ticking: AtomicBool,
    /// `stopped` (`:103`).
    stopped: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Resets `ticking` even if the tick future is dropped mid-way (`finally`).
struct TickingGuard<'a>(&'a AtomicBool);
impl Drop for TickingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl ClockTrust {
    /// `createClockTrust(opts)`: `startedMono = monoMs()` now.
    pub fn new(opts: ClockTrustOptions) -> Self {
        let started = opts.clock.mono_ms();
        let core = ClockTrustCore::new(started, opts.grace_ms);
        Self {
            inner: Arc::new(Inner {
                opts,
                core: Mutex::new(core),
                timer: Mutex::new(None),
                ticking: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
            }),
        }
    }

    /// `start()` (`clock-trust.ts:171-176`): no-op when running, stopped or
    /// latched; arms the 5 s interval, then ticks at once ("the boot window
    /// is the point"). Needs a tokio runtime.
    pub fn start(&self) {
        {
            let timer = lock(&self.inner.timer);
            if timer.is_some()
                || self.inner.stopped.load(Ordering::SeqCst)
                || lock(&self.inner.core).latched()
            {
                return;
            }
        }
        Inner::arm(&self.inner, self.inner.opts.poll_ms);
        let inner = self.inner.clone();
        tokio::spawn(async move { Inner::tick(inner).await });
    }

    /// `stop()` (`clock-trust.ts:177-183`).
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        if let Some(t) = lock(&self.inner.timer).take() {
            t.abort();
        }
    }

    /// `tick()` (`clock-trust.ts:152-165`): one probe pass (the timer calls
    /// it; exposed for tests).
    pub fn tick(&self) -> BoxFut<'static, ()> {
        Box::pin(Inner::tick(self.inner.clone()))
    }

    /// `trusted()` (`clock-trust.ts:185-189`): the gate.
    pub fn trusted(&self) -> bool {
        let now = self.inner.opts.clock.mono_ms();
        let (t, out) = lock(&self.inner.core).trusted(now);
        self.inner.perform(out);
        t
    }

    /// `noteHeld()` (`clock-trust.ts:190-192`): count a withheld publish.
    pub fn note_held(&self) {
        lock(&self.inner.core).note_held();
    }

    /// `noteStep(source)` (`clock-trust.ts:193-195`): the clock stepper
    /// applied a GPS/NITZ step — that is trust.
    pub fn note_step(&self, source: SteppedClockSource) {
        let now = self.inner.opts.clock.mono_ms();
        let out = lock(&self.inner.core).note_step(source, now);
        self.inner.perform(out);
    }

    /// `status()` (`clock-trust.ts:196-210`): calls `trusted()` first.
    /// Never holds a lock across a callback (safe under ConfigSync's mutex).
    pub fn status(&self) -> ClockTrustStatus {
        let now = self.inner.opts.clock.mono_ms();
        let (s, out) = lock(&self.inner.core).status(now);
        self.inner.perform(out);
        s
    }

    /// Test seam: is the poll timer armed, and at which period?
    #[doc(hidden)]
    pub fn timer_armed(&self) -> bool {
        lock(&self.inner.timer).is_some()
    }
}

impl Inner {
    /// `timer = setInterval(() => void tick(), period)` — replaces any
    /// running timer.
    fn arm(this: &Arc<Inner>, period_ms: u64) {
        let mut iv = SetInterval::new(Duration::from_millis(period_ms));
        let weak: Weak<Inner> = Arc::downgrade(this);
        let handle = tokio::spawn(async move {
            loop {
                iv.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                // `void tick()`: a slow probe never delays the next fire
                // (it only skips it, via `ticking`).
                tokio::spawn(async move { Inner::tick(inner).await });
            }
        });
        if let Some(old) = lock(&this.timer).replace(handle) {
            old.abort();
        }
    }

    async fn tick(this: Arc<Inner>) {
        if this.stopped.load(Ordering::SeqCst) || lock(&this.core).latched() {
            return;
        }
        if this.ticking.swap(true, Ordering::SeqCst) {
            return;
        }
        let _guard = TickingGuard(&this.ticking);
        let answer = (this.opts.probe)().await;
        let now = this.opts.clock.mono_ms();
        let out = lock(&this.core).on_probe(answer, now);
        this.perform(out);
    }

    /// The core's outputs, in order. Called with no lock held.
    fn perform(self: &Arc<Self>, out: Vec<TrustOutput>) {
        for o in out {
            match o {
                TrustOutput::Event(level, msg) => {
                    let level = match level {
                        TrustEventLevel::Warn => EventLevel::Warn,
                        TrustEventLevel::Info => EventLevel::Info,
                    };
                    (self.opts.on_event)(level, &msg);
                }
                TrustOutput::StopTimer => {
                    if let Some(t) = lock(&self.timer).take() {
                        t.abort();
                    }
                }
                TrustOutput::SlowPoll => {
                    // `if (timer) { clearInterval; timer = setInterval(…, slowPollMs) }`
                    let running = lock(&self.timer).is_some();
                    if running {
                        Inner::arm(self, self.opts.slow_poll_ms);
                    }
                }
            }
        }
    }
}

/// The real probe: one uncached `timedatectl show -p NTPSynchronized --value`
/// (taped as an `exec`; the rig's `timedatectl` shim answers from its NTP
/// timeline). It never throws, like the TS `probeTimeSource`.
pub fn system_probe(tape: Arc<dyn xorgate_hal::io::TapeSink>) -> TrustProbe {
    Box::new(move || {
        let tape = tape.clone();
        Box::pin(
            async move { Ok(xorgate_hal::boot_id::probe_time_source_taped(tape.as_ref()).await) },
        )
    })
}
