//! The runtime of `createTimeSourceTracker` + the session I/O of
//! `src/recording/session.ts` (`:40-157`): a cloneable [`TimeSourceHandle`]
//! over core's [`TimeSourceTracker`] state machine, whose probe execs
//! `timedatectl` ([`probe_time_source`]); and [`read_boot_id`].
//!
//! The recorder reads `current()` every tick at up to 20 Hz, so it returns
//! the cache and kicks a background re-probe at most every `refreshMs`
//! (default 60 s). The manager AWAITS `refresh()` (bounded by its 2 s
//! `TIME_PROBE_TIMEOUT_MS`, WP1) before its first apply, so the first
//! session headers carry the probed value rather than the pessimistic
//! initial `unsynced`. `note_step` is the offline clock stepper's (Phase 4).
//!
//! Clock: the TS gates the 60 s re-probe on `Date.now()`; here it is the
//! agent's MONOTONIC clock (common-brief rule 3 / P8 — a backward wall step,
//! the normal end of a battery-less boot, would otherwise make `now -
//! lastProbeAt` negative and freeze the cached provenance for the rest of
//! the boot; the same field wedge Phase 8 fixed in the recorder). The mono
//! value is offset by `refresh_ms` so the very first `current()` is due, as
//! it always is in the TS (`Date.now() - 0 >= refreshMs`). Evidence wp2,
//! ambiguity A1.

use std::sync::{Arc, Mutex};

use xorgate_core::clock::Clock;
use xorgate_core::session::{ProbedTimeSource, TimeSourceTracker};
use xorgate_core::types::{SteppedClockSource, TimeSource};
use xorgate_hal::io::TapeSink;

use crate::ctx::BoxFut;

pub use xorgate_hal::boot_id::{probe_time_source, read_boot_id};

/// The probe a handle runs (`probe` parameter of `createTimeSourceTracker`).
pub type ProbeFn = Arc<dyn Fn() -> BoxFut<'static, ProbedTimeSource> + Send + Sync>;

struct Inner {
    tracker: Mutex<TimeSourceTracker>,
    refresh_ms: i64,
    probe: ProbeFn,
    clock: Arc<dyn Clock + Send + Sync>,
}

/// `TimeSourceTracker` (`session.ts:94-112`) as a cloneable handle.
#[derive(Clone)]
pub struct TimeSourceHandle {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for TimeSourceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimeSourceHandle")
            .field("value", &self.lock().value())
            .finish()
    }
}

/// Resets `probing` if a probe future is dropped or panics before it
/// answered (the `finally` of `refresh()`, `session.ts:135-137`).
struct ProbeGuard<'a> {
    inner: &'a Inner,
    armed: bool,
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut t) = self.inner.tracker.lock() {
                t.probe_failed();
            }
        }
    }
}

impl Inner {
    /// The tracker's `now`: monotonic ms offset by `refresh_ms` (module doc).
    fn now(&self) -> i64 {
        self.clock.mono_ms() as i64 + self.refresh_ms
    }

    /// `refresh()` after its synchronous prefix (`probing = true`).
    async fn run_probe(&self) -> TimeSource {
        let mut guard = ProbeGuard {
            inner: self,
            armed: true,
        };
        let probed = (self.probe)().await;
        guard.armed = false;
        let now = self.now();
        let mut t = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
        t.apply_probe(probed, now)
    }
}

impl TimeSourceHandle {
    /// `createTimeSourceTracker(refreshMs, probe)` (`session.ts:119-126`).
    pub fn create(refresh_ms: i64, probe: ProbeFn, clock: Arc<dyn Clock + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Inner {
                tracker: Mutex::new(TimeSourceTracker::new(refresh_ms)),
                refresh_ms,
                probe,
                clock,
            }),
        }
    }

    /// `createTimeSourceTracker()` with its defaults: 60 s, the real
    /// `timedatectl` probe (reported to the tape as an `exec`).
    pub fn system(clock: Arc<dyn Clock + Send + Sync>, tape: Arc<dyn TapeSink>) -> Self {
        let probe: ProbeFn = Arc::new(move || {
            let tape = tape.clone();
            Box::pin(async move { xorgate_hal::boot_id::probe_time_source_taped(&*tape).await })
        });
        Self::create(TimeSourceTracker::DEFAULT_REFRESH_MS, probe, clock)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TimeSourceTracker> {
        self.inner.tracker.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `current()` (`session.ts:142-148`): the last known value — never
    /// blocks — kicking a background refresh when stale (the slot is claimed
    /// before the probe lands, so a 20 Hz reader never piles probes up).
    /// Outside a tokio runtime no probe can run; the claim is released.
    pub fn current(&self) -> TimeSource {
        let now = self.inner.now();
        let (value, due) = self.lock().current(now);
        if due {
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    let inner = self.inner.clone();
                    // `void refresh().catch(() => {})`
                    rt.spawn(async move {
                        inner.run_probe().await;
                    });
                }
                Err(_) => self.lock().probe_failed(),
            }
        }
        value
    }

    /// `refresh()` (`session.ts:128-139`): force a probe now; resolves to
    /// the new value. NTP supersedes any offline step for good.
    pub async fn refresh(&self) -> TimeSource {
        self.lock().begin_refresh();
        self.inner.run_probe().await
    }

    /// `noteStep(source)` (`session.ts:150-156`): reflected immediately;
    /// never displaces `ntp`.
    pub fn note_step(&self, source: SteppedClockSource) {
        self.lock().note_step(source);
    }
}
