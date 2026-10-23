//! RGB status LED — the moving parts around the pure policy. Port of
//! `src/agent/led-manager.ts` (187 lines @ 89b5c64; Phase 5b WP3 @ 09da38d:
//! `tick()` resolves on EVERY host, LED or not, and keeps the last resolved
//! `(state, signals)` for `system.led_state` / `system.health_flags` —
//! `led-manager.ts:48-54, 95-137`).
//!
//! Split of responsibility (`led-manager.ts:4-7`):
//! - `xorgate_core::led_state` decides WHAT to show (pure, no clocks);
//! - `xorgate_hal::led_driver` knows HOW to show it (gpiocdev / pinctrl);
//! - this file: when to ask, and what to do about time — the `config-applied`
//!   overlay and the re-render-on-change latch.
//!
//! `tick()` is called from the worker's 1 Hz heartbeat with snapshots it
//! already holds (the WP6 wiring); the only read of its own is the LTE
//! daemon's tmpfs file, throttled to once a second.
//!
//! **CF59 / D-row (LED LTE refresh on mono).** The TS throttles the LTE
//! re-read on `Date.now()` (`led-manager.ts:156-168`); this port uses the
//! monotonic clock (tokio `Instant`, invariant 2). A wall step of ≥ 1 s
//! changes only WHEN the next re-read of a fail-open input happens (a
//! backward step can no longer stall the refresh; a forward one no longer
//! forces an early read) — never what is shown for a given file content.
//!
//! The TS's fire-and-forget promises (`void this.refreshLte()`, `void
//! this.driver.play(…)`, the overlay's `.finally`) are tokio tasks; the
//! driver methods run their synchronous TS prefix at call time (see
//! `LedDriver`), so supersession order is the call order.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::time::Instant;
use xorgate_core::led_state::{
    encode_led_telemetry, resolve_led_state, rung, LedSignals, LedState, LedTelemetry,
    LteStatusView,
};
use xorgate_core::telemetry::{ConnectionState, RecordingStatus};
use xorgate_core::types::GnssView;
use xorgate_hal::led_driver::{BoxFut, LedDriver};

/// `LTE_REFRESH_MS` (`led-manager.ts:20`): how stale the cached LTE status
/// may get before another read is kicked off.
pub const LTE_REFRESH_MS: u64 = 1000;

/// `Omit<LedSignals, "lte">` (`led-manager.ts:27`) — what the worker passes
/// each tick. `lte` is deliberately NOT part of it: the manager reads it for
/// itself (`refresh_lte`).
#[derive(Debug, Clone, PartialEq)]
pub struct LedSignalsOwned {
    pub connection: ConnectionState,
    pub recording: RecordingStatus,
    pub gnss: Option<GnssView>,
    pub starting: bool,
}

/// Signals pulled on each tick; must not do I/O. A panic here is the TS's
/// throwing `signals()` (`led-manager.ts:90-97`): logged, state held.
pub type SignalsFn = Box<dyn Fn() -> LedSignalsOwned + Send + Sync>;
/// `readLte` (`led-manager.ts:29`): the LTE daemon's status view.
/// `Ok(None)` = unreadable / no status (the default reader never errs);
/// `Err` = the TS rejection — both fail open to `null`.
pub type ReadLteFn =
    Box<dyn Fn() -> BoxFut<'static, Result<Option<LteStatusView>, String>> + Send + Sync>;
pub type LogFn = Box<dyn Fn(&str) + Send + Sync>;

/// `resolved` (`led-manager.ts:48-54` @ 09da38d): the last state the ladder
/// resolved and the signals it was resolved from (the I6 `snapshot()`; the
/// TS `LedSignals` is `signals` + `lte`). Never the overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct LedSnapshot {
    pub state: LedState,
    pub signals: LedSignalsOwned,
    pub lte: Option<LteStatusView>,
}

impl LedSnapshot {
    /// `encodeLedTelemetry(state, signals)`.
    pub fn telemetry(&self) -> LedTelemetry {
        encode_led_telemetry(
            &self.state,
            &LedSignals {
                connection: self.signals.connection,
                recording: &self.signals.recording,
                lte: self.lte.clone(),
                gnss: self.signals.gnss,
                starting: self.signals.starting,
            },
        )
    }
}

#[derive(Default)]
struct State {
    current: Option<LedState>,
    /// Kept on EVERY host (D4): telemetry reports the device's state, not
    /// the light's.
    resolved: Option<LedSnapshot>,
    overlay_running: bool,
    stopped: bool,
    /// Last LTE daemon status we read. `None` = unreadable, NOT down.
    lte: Option<LteStatusView>,
    /// When the last read FINISHED (mono); `None` = never (`lteAt = 0`).
    lte_at: Option<Instant>,
    lte_reading: bool,
}

struct Inner {
    driver: LedDriver,
    signals: SignalsFn,
    read_lte: ReadLteFn,
    log: LogFn,
    st: Mutex<State>,
}

/// `LedManager` (`led-manager.ts:35-187`). Cheap to clone (shared state).
#[derive(Clone)]
pub struct LedManager {
    inner: Arc<Inner>,
}

impl LedManager {
    /// `new LedManager({ driver, signals, readLte, log })`
    /// (`led-manager.ts:50-55`). `log` default: `[led] <msg>` at info
    /// (`console.log`).
    pub fn new(
        driver: LedDriver,
        signals: SignalsFn,
        read_lte: ReadLteFn,
        log: Option<LogFn>,
    ) -> Self {
        LedManager {
            inner: Arc::new(Inner {
                driver,
                signals,
                read_lte,
                log: log.unwrap_or_else(|| Box::new(|m| tracing::info!("[led] {m}"))),
                st: Mutex::new(State::default()),
            }),
        }
    }

    pub fn driver(&self) -> &LedDriver {
        &self.inner.driver
    }

    /// `get available()` (`led-manager.ts:57-59`).
    pub fn available(&self) -> bool {
        self.inner.driver.available()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn log(&self, msg: &str) {
        (self.inner.log)(msg);
    }

    /// `start()` (`led-manager.ts:66-80`): probe the hardware; inert →
    /// `inert: <reason>` and every method is a no-op. Otherwise prime the LTE
    /// read (so the first tick has a status) and show `starting` at once.
    /// The worker does not await it (`worker.ts:625-628`).
    pub fn start(&self) -> BoxFut<'static, ()> {
        let me = self.clone();
        let init = self.inner.driver.init();
        Box::pin(async move {
            let init = init.await;
            if !init.ok {
                // `${init.reason}` — a reason-less verdict prints `undefined`.
                me.log(&format!(
                    "inert: {}",
                    init.reason.as_deref().unwrap_or("undefined")
                ));
                return;
            }
            if let Some(read) = me.refresh_lte() {
                read.await;
            }
            me.render(rung_state("starting"));
        })
    }

    /// `tick()` (`led-manager.ts:100-123` @ 09da38d): called from the 1 Hz
    /// heartbeat. Resolves the state on EVERY host (telemetry needs it even
    /// where there is no LED), stores `(state, signals)`, and only then
    /// applies the hardware gate (inert driver → stop) and the overlay gate;
    /// re-renders only on a real change. A signal-read failure keeps the last
    /// resolved pair (fail open). The LTE refresh is fire-and-forget: this
    /// tick uses the previous read's value.
    ///
    /// **Runtime (review-wp1 S1):** must be called from inside the tokio
    /// runtime (it spawns tasks; the TS method is plain sync on the event
    /// loop). Never call it from `Drop` or a synchronous signal handler.
    /// Outside a runtime it does not panic: it logs
    /// `[led] <method>() called outside the tokio runtime — ignored` at error
    /// and does nothing.
    pub fn tick(&self) {
        if !in_runtime("tick") {
            return;
        }
        if self.lock().stopped {
            return;
        }
        // `void this.refreshLte()` — its synchronous prefix never changes
        // `this.lte`, so the value read below is the previous read's.
        if let Some(read) = self.refresh_lte() {
            tokio::spawn(read);
        }
        let lte = self.lock().lte.clone();
        let signals = &self.inner.signals;
        let next = catch_unwind(AssertUnwindSafe(|| {
            let s = signals();
            let next = resolve_led_state(&LedSignals {
                connection: s.connection,
                recording: &s.recording,
                lte: lte.clone(),
                gnss: s.gnss,
                starting: s.starting,
            });
            (next, s)
        }));
        let next = match next {
            Ok((next, signals)) => {
                self.lock().resolved = Some(LedSnapshot {
                    state: next,
                    signals,
                    lte,
                });
                next
            }
            Err(payload) => {
                // Fail open: a signal-gathering bug must not light red and
                // must not take the worker down with it. The last resolved
                // state is held for telemetry too.
                self.log(&format!(
                    "signal read failed, holding: {}",
                    panic_message(payload.as_ref())
                ));
                return;
            }
        };
        // Hardware gate: an inert driver (dev box, CM4 without a status LED)
        // stops here, after the state is known.
        if !self.inner.driver.available() {
            return;
        }
        // The overlay owns the LED until its flashes finish; a state change
        // during it is picked up by the re-render that follows.
        if self.lock().overlay_running {
            return;
        }
        self.render(next);
    }

    /// `snapshot()` (`led-manager.ts:130-132` @ 09da38d): the last resolved
    /// state and its inputs, `None` before the first tick. The
    /// config-applied overlay never appears here.
    pub fn snapshot(&self) -> Option<LedSnapshot> {
        self.lock().resolved.clone()
    }

    /// `telemetry()` (`led-manager.ts:135-137` @ 09da38d):
    /// `system.led_state` / `system.health_flags` for the last tick, `None`
    /// before the first.
    pub fn telemetry(&self) -> Option<LedTelemetry> {
        self.lock().resolved.as_ref().map(LedSnapshot::telemetry)
    }

    /// `configApplied()` (`led-manager.ts:111-123`): two white flashes
    /// pre-empting whatever is showing, then an immediate re-render of the
    /// resolved state (`current = null` forces it even when unchanged).
    ///
    /// **Runtime (review-wp1 S1):** must be called from inside the tokio
    /// runtime (it spawns tasks; the TS method is plain sync on the event
    /// loop). Never call it from `Drop` or a synchronous signal handler.
    /// Outside a runtime it does not panic: it logs
    /// `[led] <method>() called outside the tokio runtime — ignored` at error
    /// and does nothing.
    pub fn config_applied(&self) {
        if !in_runtime("config_applied") {
            return;
        }
        {
            let st = self.lock();
            if st.stopped {
                return;
            }
            drop(st);
            if !self.inner.driver.available() {
                return;
            }
            let mut st = self.lock();
            if st.overlay_running {
                return;
            }
            st.overlay_running = true;
        }
        let r = rung_state("config-applied");
        let play = self.inner.driver.play(r.colour, r.pattern);
        let me = self.clone();
        tokio::spawn(async move {
            // `.catch(() => {})`: a panicking play still releases the overlay.
            let _ = tokio::spawn(play).await;
            let stopped = {
                let mut st = me.lock();
                st.overlay_running = false;
                st.current = None;
                st.stopped
            };
            if !stopped {
                me.tick();
            }
        });
    }

    /// `stop()` (`led-manager.ts:130-136`): blank on shutdown, best-effort and
    /// not awaited (`ExecStopPost=… led exit-state` is the real guarantee);
    /// `quiet` because it races the process exit.
    ///
    /// **Runtime (review-wp1 S1):** must be called from inside the tokio
    /// runtime (it spawns tasks; the TS method is plain sync on the event
    /// loop). Never call it from `Drop` or a synchronous signal handler.
    /// Outside a runtime it does not panic: it logs
    /// `[led] <method>() called outside the tokio runtime — ignored` at error
    /// and only latches `stopped` + cancels the pattern (the blank is
    /// skipped; `ExecStopPost` still blanks).
    pub fn stop(&self) {
        self.lock().stopped = true;
        self.inner.driver.stop();
        if !in_runtime("stop") {
            return;
        }
        tokio::spawn(self.inner.driver.blank(true));
    }

    /// `refreshLte()` (`led-manager.ts:155-170`): single-flight, throttled to
    /// [`LTE_REFRESH_MS`] since the last read FINISHED (mono, CF59). `None`
    /// = throttled (the TS's resolved promise). The read is started at call
    /// time; an `Err` (or a panicking reader) is "unknown" = `None`, which
    /// fails open.
    fn refresh_lte(&self) -> Option<BoxFut<'static, ()>> {
        let now = Instant::now();
        {
            let mut st = self.lock();
            let fresh = st.lte_at.is_some_and(|at| {
                now.saturating_duration_since(at) < Duration::from_millis(LTE_REFRESH_MS)
            });
            if st.lte_reading || fresh {
                return None;
            }
            st.lte_reading = true;
        }
        let read = (self.inner.read_lte)();
        let me = self.clone();
        Some(Box::pin(async move {
            let lte = match tokio::spawn(read).await {
                Ok(Ok(v)) => v,
                _ => None,
            };
            let mut st = me.lock();
            st.lte = lte;
            st.lte_reading = false;
            st.lte_at = Some(Instant::now());
        }))
    }

    /// `render(next)` (`led-manager.ts:172-186`): colour + pattern elision
    /// (restarting a running pattern would stutter it); log
    /// `${from} → ${reason} (${colour} ${pattern})`; start the pattern, not
    /// awaited (repeating rungs never end).
    fn render(&self, next: LedState) {
        let from = {
            let mut st = self.lock();
            if let Some(cur) = st.current {
                if cur.colour == next.colour && cur.pattern == next.pattern {
                    return;
                }
            }
            let from = st.current.map_or("none", |c| c.reason);
            st.current = Some(next);
            from
        };
        self.log(&format!(
            "{from} → {} ({} {})",
            next.reason,
            next.colour.as_str(),
            next.pattern.as_str()
        ));
        tokio::spawn(self.inner.driver.play(next.colour, next.pattern));
    }
}

/// `true` when a tokio runtime is current (S1). Otherwise logs the misuse
/// (no TS counterpart: the TS methods cannot run off the event loop) and
/// returns `false`, so the caller backs out instead of `tokio::spawn`
/// panicking.
fn in_runtime(method: &str) -> bool {
    if tokio::runtime::Handle::try_current().is_ok() {
        return true;
    }
    tracing::error!("[led] {method}() called outside the tokio runtime — ignored");
    false
}

/// `rung(reason)` as the `LedState` the TS passes straight to `render` /
/// `play` (a rung carries colour, pattern and reason).
fn rung_state(reason: &str) -> LedState {
    let r = rung(reason).unwrap_or_else(|| panic!("no LED ladder rung for reason '{reason}'"));
    LedState {
        colour: r.colour,
        pattern: r.pattern,
        reason: r.reason,
    }
}

/// `err instanceof Error ? err.message : String(err)` for a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// The default `readLte` (`readModemTelemetry()`, `modem.ts:203-213`): one
/// read of the LTE daemon's telemetry file (tape-reported), its `status`
/// view; absent/unparseable = `None`.
pub fn file_read_lte(
    path: std::path::PathBuf,
    tape: Arc<dyn xorgate_hal::io::TapeSink>,
) -> ReadLteFn {
    Box::new(move || {
        let (path, tape) = (path.clone(), tape.clone());
        Box::pin(async move {
            let snap = xorgate_hal::modem_file::read(&path, tape.as_ref()).await;
            Ok(snap.raw.as_ref().and_then(LteStatusView::from_value))
        })
    })
}
