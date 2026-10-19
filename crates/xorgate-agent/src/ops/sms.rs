//! `SmsReader` (`ts:src/hardware/sms-modem.ts:472-541` @ 89b5c64, Phase 4
//! WP2, I4): one sweep at a time per process — concurrent callers join the
//! running sweep, and a caller within the reuse window (5 s) of the last
//! finish gets that result instead of a fresh modem round (the plan's "one
//! sweep per 5 s" rate limit). The cross-process lock keeps `xorgate-rs sms`
//! on the box from interleaving with the agent's `read_sms`.
//!
//! Invariant 2: the reuse window and `durationMs` are measured on the
//! MONOTONIC clock (`Clock::mono_ms`); only `finishedAt` is a wall stamp
//! (`Date.now()` in the TS, which uses one clock for both).
//!
//! The sweep runs on its own task: a caller that goes away (a dropped
//! future) does not cancel it, as a TS promise keeps running — the joiners
//! and the reuse window still get its result.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use xorgate_core::clock::Clock;
use xorgate_core::sms::{
    SmsReadStatus, SmsSweep, StorageCounts, SMS_COMMAND_TIMEOUT_MS, SMS_REUSE_WINDOW_MS,
    SMS_SWEEP_BUDGET_MS,
};
use xorgate_hal::at_port::{
    acquire_sms_lock, open_serial_transport_taped, resolve_sms_port, sweep_transport, AtTransport,
    ResolvePortOptions, SmsLock, SmsReadError, SweepOptions,
};
use xorgate_hal::io::{null_tape, TapeSink};

use crate::ctx::BoxFut;

/// `resolvePort` dep.
pub type ResolvePortFn = Box<dyn Fn() -> Option<PathBuf> + Send + Sync>;
/// `openTransport` dep.
pub type OpenTransportFn = Box<
    dyn Fn(PathBuf) -> BoxFut<'static, Result<Box<dyn AtTransport>, SmsReadError>> + Send + Sync,
>;

/// `SmsReaderDeps` (`sms-modem.ts:472-480`).
pub struct SmsReaderDeps {
    /// Cross-process lock file; `None` = no lock (tests).
    pub lock_path: Option<PathBuf>,
    /// `resolveSmsPort` by default.
    pub resolve_port: ResolvePortFn,
    /// `openSerialTransport` by default.
    pub open: OpenTransportFn,
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// `at_tx` / `at_rx` + the `stty` exec record (I9).
    pub tape: Arc<dyn TapeSink>,
    /// `sweep.commandTimeoutMs` (3 s).
    pub command_timeout_ms: u64,
    /// `sweep.budgetMs` (20 s).
    pub budget_ms: u64,
    /// `reuseWindowMs` (`SMS_REUSE_WINDOW_MS`, 5 s).
    pub reuse_window_ms: u64,
}

impl SmsReaderDeps {
    /// The production wiring (`new SmsReader({ lockPath })`, `worker.ts:917`,
    /// `sms.ts:46`): port by USB identity (or `XORGATE_SMS_PORT`), the real
    /// tty, the TS defaults.
    pub fn system(
        lock_path: Option<PathBuf>,
        clock: Arc<dyn Clock + Send + Sync>,
        tape: Arc<dyn TapeSink>,
    ) -> Self {
        let open_tape = tape.clone();
        Self {
            lock_path,
            resolve_port: Box::new(|| resolve_sms_port(&ResolvePortOptions::system())),
            open: Box::new(move |p: PathBuf| {
                let tape = open_tape.clone();
                Box::pin(async move { open_serial_transport_taped(&p, tape).await })
            }),
            clock,
            tape,
            command_timeout_ms: SMS_COMMAND_TIMEOUT_MS,
            budget_ms: SMS_SWEEP_BUDGET_MS,
            reuse_window_ms: SMS_REUSE_WINDOW_MS,
        }
    }

    /// Test wiring: no lock, the given port + transport factory, no tape.
    pub fn with(
        resolve_port: ResolvePortFn,
        open: OpenTransportFn,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Self {
        Self {
            lock_path: None,
            resolve_port,
            open,
            clock,
            tape: null_tape(),
            command_timeout_ms: SMS_COMMAND_TIMEOUT_MS,
            budget_ms: SMS_SWEEP_BUDGET_MS,
            reuse_window_ms: SMS_REUSE_WINDOW_MS,
        }
    }
}

#[derive(Default)]
struct State {
    /// `inflight`: the running sweep's result channel.
    inflight: Option<watch::Receiver<Option<Arc<SmsSweep>>>>,
    /// `last` + its finish on the monotonic clock.
    last: Option<(Arc<SmsSweep>, u64)>,
}

/// `SmsReader` (`sms-modem.ts:487-541`). Construct one per process
/// (`worker.ts:917`) and share it (`Arc`).
pub struct SmsReader {
    deps: SmsReaderDeps,
    state: Mutex<State>,
}

impl SmsReader {
    pub fn new(deps: SmsReaderDeps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            state: Mutex::new(State::default()),
        })
    }

    /// `read()` (`sms-modem.ts:493-502`): join the running sweep, else reuse
    /// the last result inside the window, else start a sweep. Never fails —
    /// the status is inside. The same `Arc` goes to every joiner and reuser
    /// (the TS's `a === b`).
    pub fn read(self: &Arc<Self>) -> BoxFut<'static, Arc<SmsSweep>> {
        let mut rx = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match &st.inflight {
                Some(rx) => rx.clone(),
                None => {
                    let now = self.deps.clock.mono_ms();
                    if let Some((last, at)) = &st.last {
                        if now.saturating_sub(*at) < self.deps.reuse_window_ms {
                            let last = last.clone();
                            return Box::pin(async move { last });
                        }
                    }
                    let (tx, rx) = watch::channel(None);
                    st.inflight = Some(rx.clone());
                    let me = self.clone();
                    tokio::spawn(async move {
                        let sweep = me.run_sweep().await;
                        // `.finally(() => { this.inflight = null })` — `last`
                        // was set by `finish()` inside the sweep, before the
                        // transport closed; here both land together.
                        {
                            let mut st = me.state.lock().unwrap_or_else(|e| e.into_inner());
                            st.inflight = None;
                        }
                        let _ = tx.send(Some(sweep));
                    });
                    rx
                }
            }
        };
        Box::pin(async move {
            loop {
                if let Some(s) = rx.borrow_and_update().clone() {
                    return s;
                }
                if rx.changed().await.is_err() {
                    // The sweep task died (panic): the TS catch-all shape.
                    return Arc::new(empty_sweep(SmsReadStatus::Error, "unknown", 0, 0));
                }
            }
        })
    }

    /// `runSweep` (`sms-modem.ts:504-540`): resolve → lock → open → sweep →
    /// `finish` (stamps + `last`) → close the transport → release the lock.
    async fn run_sweep(self: &Arc<Self>) -> Arc<SmsSweep> {
        let clock = self.deps.clock.clone();
        let start = clock.mono_ms();
        let finish = |r: Result<SmsSweep, SmsReadError>| -> Arc<SmsSweep> {
            let end_mono = clock.mono_ms();
            let end_wall = clock.wall_ms();
            let duration = end_mono.saturating_sub(start) as i64;
            let sweep = match r {
                Ok(mut s) => {
                    s.status = SmsReadStatus::Ok;
                    s.error = None;
                    s.finished_at = end_wall;
                    s.duration_ms = duration;
                    s
                }
                Err(e) => empty_sweep(e.status, &e.message, end_wall, duration),
            };
            let sweep = Arc::new(sweep);
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.last = Some((sweep.clone(), end_mono));
            sweep
        };
        let Some(port) = (self.deps.resolve_port)() else {
            return finish(Err(SmsReadError::new(
                SmsReadStatus::NoModem,
                "no SIMCom interface 05 tty",
            )));
        };
        let lock: SmsLock = match &self.deps.lock_path {
            Some(p) => match acquire_sms_lock(p) {
                Some(l) => l,
                None => {
                    return finish(Err(SmsReadError::new(
                        SmsReadStatus::ModemBusy,
                        "another sms read is running",
                    )))
                }
            },
            None => SmsLock::unlocked(),
        };
        let mut transport = match (self.deps.open)(port.clone()).await {
            Ok(t) => t,
            Err(e) => {
                let s = finish(Err(e));
                drop(lock);
                return s;
            }
        };
        let opts = SweepOptions {
            command_timeout_ms: self.deps.command_timeout_ms,
            budget_ms: self.deps.budget_ms,
            clock: clock.clone(),
            port: port.to_string_lossy().into_owned(),
            tape: self.deps.tape.clone(),
        };
        let r = sweep_transport(&mut *transport, opts).await;
        let sweep = finish(r);
        // `finally { await transport?.close().catch(() => {}); release(); }`
        transport.close().await;
        drop(lock);
        sweep
    }

    /// The last finished sweep, if any (tests / diagnostics).
    pub fn last(&self) -> Option<Arc<SmsSweep>> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.last.as_ref().map(|(s, _)| s.clone())
    }
}

/// `finish({ status, error })` with the defaults of `sms-modem.ts:511-519`.
fn empty_sweep(status: SmsReadStatus, error: &str, finished_at: i64, duration_ms: i64) -> SmsSweep {
    SmsSweep {
        status,
        error: Some(error.to_string()),
        finished_at,
        duration_ms,
        storage: StorageCounts::new(),
        skipped: 0,
        partial: false,
        messages: Vec::new(),
    }
}

/// `join(runtimeHome(), "sms-reader.lock")` (`worker.ts:917`, `sms.ts:46`).
pub fn lock_path(home: &Path) -> PathBuf {
    home.join(xorgate_core::sms::SMS_READER_LOCK_FILE)
}
