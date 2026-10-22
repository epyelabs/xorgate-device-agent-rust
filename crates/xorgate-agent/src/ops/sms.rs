//! `SmsReader` (`ts:src/hardware/sms-modem.ts:598-733` @ 09da38d; Phase 4
//! WP2 I4, Phase 5b WP3 I7): ONE modem operation at a time per process (plus
//! the cross-process lock, which keeps `xorgate-rs sms` on the box from
//! interleaving with the agent).
//!
//! Reads: concurrent callers join the running read, and a caller within the
//! reuse window (5 s) of the last READ's finish gets that result instead of
//! a fresh modem round (the plan's "one sweep per 5 s" rate limit).
//!
//! Clears (phase-5b §6 point 5): concurrent clears join the running (or
//! queued) clear — one delete. A clear never joins a running READ (its result
//! predates the delete): it queues behind the read and becomes the in-flight
//! tail at once, so a read arriving meanwhile waits for the clear and then
//! sweeps afresh. A clear drops the cached read when requested AND again
//! when it starts, and its own result never feeds the reuse window. Once a
//! delete may have happened every failure is `error`, and a delete error
//! wins over a clean post-clear sweep (whose storage counts the reply keeps).
//!
//! The ONLY caller of `xorgate_hal::at_clear::clear_transport` is
//! [`SmsReader::clear`] (grep-tested, `tests/sms_clear_safety.rs`), whose
//! only callers are the command plane's accepted `clear_sms` and the
//! confirmed `xorgate-rs sms clear`.
//!
//! Invariant 2: the reuse window and `durationMs` are measured on the
//! MONOTONIC clock (`Clock::mono_ms`); only `finishedAt` is a wall stamp
//! (`Date.now()` in the TS, which uses one clock for both:
//! `sms-modem.ts:495-497, 505-519`). Accepted divergence **D32**
//! (`phase-1/evidence/accepted-divergences.md`): across a wall step the TS
//! reuses a stale sweep after a backward step and re-sweeps at once after a
//! forward one; Rust does neither. Pinned by `tests/sms_reader.rs`
//! `d32_reuse_window_runs_on_mono_across_wall_steps`.
//!
//! Every operation runs on its own task: a caller that goes away (a dropped
//! future) does not cancel it, as a TS promise keeps running — the joiners
//! and the reuse window still get its result, and a clear always reaches its
//! mem1 restore.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use xorgate_core::clock::Clock;
use xorgate_core::sms::{
    ClearedCounts, SmsReadStatus, SmsSweep, StorageCounts, SMS_COMMAND_TIMEOUT_MS,
    SMS_DELETE_TIMEOUT_MS, SMS_REUSE_WINDOW_MS, SMS_SWEEP_BUDGET_MS,
};
use xorgate_hal::at_clear::{clear_transport, ClearOptions, ClearOutcome};
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

/// `SmsReaderDeps` (`sms-modem.ts:599-607` @ 09da38d).
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
    /// `clear.commandTimeoutMs` (3 s; independent of the sweep's).
    pub clear_command_timeout_ms: u64,
    /// `clear.deleteTimeoutMs` (`SMS_DELETE_TIMEOUT_MS`, 25 s).
    pub delete_timeout_ms: u64,
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
            clear_command_timeout_ms: SMS_COMMAND_TIMEOUT_MS,
            delete_timeout_ms: SMS_DELETE_TIMEOUT_MS,
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
            clear_command_timeout_ms: SMS_COMMAND_TIMEOUT_MS,
            delete_timeout_ms: SMS_DELETE_TIMEOUT_MS,
        }
    }
}

/// `SmsOp` (`sms-modem.ts:610` @ 09da38d).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Read,
    Clear,
}

/// Why a clear runs — the two entry points of phase-5b §6 point 2, named so
/// the caller grep (`tests/sms_clear_safety.rs`) can list them. Carries no
/// behaviour: both run the same clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearRequest {
    /// An ACCEPTED `clear_sms` from the command plane, after the ledger's
    /// duplicate check (a QoS-1 redelivery never reaches here).
    AcceptedCommand { request_id: String },
    /// `xorgate-rs sms clear` with `--yes` / `-y` or an interactive yes.
    ConfirmedCli,
}

/// The tracked in-flight operation (`inflight: {op, p}`).
struct Inflight {
    op: Op,
    /// `this.inflight?.p === p` identity for the `.finally` clean-up.
    id: u64,
    rx: watch::Receiver<Option<Arc<SmsSweep>>>,
}

#[derive(Default)]
struct State {
    inflight: Option<Inflight>,
    /// `last` (reads only) + its finish on the monotonic clock.
    last: Option<(Arc<SmsSweep>, u64)>,
    next_id: u64,
}

/// `SmsReader` (`sms-modem.ts:625-733` @ 09da38d). Construct one per process
/// (`worker.ts:1027`) and share it (`Arc`).
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

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `read()` (`sms-modem.ts:632-640`): join the running read; wait for a
    /// running or queued clear and then read afresh; else reuse the last
    /// read inside the window; else start a read. Never fails — the status
    /// is inside. The same `Arc` goes to every joiner and reuser (`a === b`).
    pub fn read(self: &Arc<Self>) -> BoxFut<'static, Arc<SmsSweep>> {
        let mut st = self.lock();
        match &st.inflight {
            Some(cur) if cur.op == Op::Read => {
                let rx = cur.rx.clone();
                drop(st);
                Box::pin(wait(rx))
            }
            Some(cur) => {
                // `cur.p.then(() => this.read())`
                let rx = cur.rx.clone();
                drop(st);
                let me = self.clone();
                Box::pin(async move {
                    let _ = wait(rx).await;
                    me.read().await
                })
            }
            None => {
                let now = self.deps.clock.mono_ms();
                if let Some((last, at)) = &st.last {
                    if now.saturating_sub(*at) < self.deps.reuse_window_ms {
                        let last = last.clone();
                        return Box::pin(async move { last });
                    }
                }
                let rx = self.track(&mut st, Op::Read, None);
                drop(st);
                Box::pin(wait(rx))
            }
        }
    }

    /// `clear()` (`sms-modem.ts:643-655`): delete every stored message (SM,
    /// then ME), then sweep. DESTRUCTIVE. Joins a running or queued clear;
    /// queued behind a running read; drops the cached read now and again
    /// when it starts. `why` names the entry point (phase-5b §6 point 2).
    pub fn clear(self: &Arc<Self>, why: ClearRequest) -> BoxFut<'static, Arc<SmsSweep>> {
        let _ = why;
        let mut st = self.lock();
        if let Some(cur) = &st.inflight {
            if cur.op == Op::Clear {
                let rx = cur.rx.clone();
                drop(st);
                return Box::pin(wait(rx));
            }
        }
        st.last = None;
        let prev = st.inflight.as_ref().map(|c| c.rx.clone());
        let rx = self.track(&mut st, Op::Clear, prev);
        drop(st);
        Box::pin(wait(rx))
    }

    /// `track(op, run)` (`sms-modem.ts:657-664`): the op becomes `inflight`
    /// at once and runs on its own task (after `after`, the read a clear
    /// queues behind); `.finally` clears `inflight` only if it is still this
    /// op — on success, panic and cancellation alike (drop guard).
    fn track(
        self: &Arc<Self>,
        st: &mut State,
        op: Op,
        after: Option<watch::Receiver<Option<Arc<SmsSweep>>>>,
    ) -> watch::Receiver<Option<Arc<SmsSweep>>> {
        let (tx, rx) = watch::channel(None);
        st.next_id += 1;
        let id = st.next_id;
        st.inflight = Some(Inflight {
            op,
            id,
            rx: rx.clone(),
        });
        let me = self.clone();
        tokio::spawn(async move {
            // Review WP2 S1: a panic inside the op must not wedge the
            // reader; the guard clears `inflight` however the task ends.
            let guard = ClearInflight(me.clone(), id);
            if let Some(prev) = after {
                let _ = wait(prev).await;
            }
            let sweep = match op {
                Op::Read => me.run_op(Op::Read).await,
                Op::Clear => {
                    // `this.last = null` again: the read we waited for may
                    // have just cached itself.
                    me.lock().last = None;
                    me.run_op(Op::Clear).await
                }
            };
            // `.finally` runs before the awaiting callers resume.
            drop(guard);
            let _ = tx.send(Some(sweep));
        });
        rx
    }

    /// `runOp(op, body)` (`sms-modem.ts:688-732`): resolve → lock → open →
    /// body → `finish` (stamps; `last` for a read only) → close the
    /// transport → release the lock.
    async fn run_op(self: &Arc<Self>, op: Op) -> Arc<SmsSweep> {
        let clock = self.deps.clock.clone();
        let start = clock.mono_ms();
        let finish = |r: Result<SmsSweep, SmsReadError>| -> Arc<SmsSweep> {
            let end_mono = clock.mono_ms();
            let end_wall = clock.wall_ms();
            let duration = end_mono.saturating_sub(start) as i64;
            let mut sweep = match r {
                Ok(s) => s,
                Err(e) => empty_sweep(e.status, &e.message, 0, 0),
            };
            // `...(op === "clear" ? { cleared: {} } : {}), ...r`
            if op == Op::Clear && sweep.cleared.is_none() {
                sweep.cleared = Some(ClearedCounts::new());
            }
            sweep.finished_at = end_wall;
            sweep.duration_ms = duration;
            let sweep = Arc::new(sweep);
            // Only reads feed the reuse window; a clear's result is never
            // replayed as the answer to a later read.
            if op == Op::Read {
                self.lock().last = Some((sweep.clone(), end_mono));
            }
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
        let port = port.to_string_lossy().into_owned();
        let r = match op {
            Op::Read => self.sweep_body(&mut *transport, &port).await,
            Op::Clear => self.clear_body(&mut *transport, &port).await,
        };
        let sweep = finish(r);
        // `finally { await transport?.close().catch(() => {}); release(); }`
        // (`sms-modem.ts:728-731`, with `close()` = `closed = true; await
        // loop; await fh.close()`): the tty fd is closed BEFORE the lock is
        // released. `close()` stops the reader thread (which drops its
        // handle); dropping the transport drops the last `Arc<File>`, i.e.
        // closes the fd; only then is the lock unlinked.
        transport.close().await;
        drop(transport);
        drop(lock);
        sweep
    }

    fn sweep_options(&self, port: &str) -> SweepOptions {
        SweepOptions {
            command_timeout_ms: self.deps.command_timeout_ms,
            budget_ms: self.deps.budget_ms,
            clock: self.deps.clock.clone(),
            port: port.to_string(),
            tape: self.deps.tape.clone(),
        }
    }

    /// `sweepBody` (`sms-modem.ts:666-669`).
    async fn sweep_body(
        &self,
        t: &mut dyn AtTransport,
        port: &str,
    ) -> Result<SmsSweep, SmsReadError> {
        sweep_transport(t, self.sweep_options(port)).await
    }

    /// `clearBody` (`sms-modem.ts:677-686`): delete, then sweep on the same
    /// transport and lock. A failed delete wins over a clean sweep (the
    /// reply still carries the post-delete storage counts); a failed
    /// post-clear sweep is `error` with the delete error, else
    /// `post-clear read failed: <why>`.
    async fn clear_body(
        &self,
        t: &mut dyn AtTransport,
        port: &str,
    ) -> Result<SmsSweep, SmsReadError> {
        let opts = ClearOptions {
            command_timeout_ms: self.deps.clear_command_timeout_ms,
            delete_timeout_ms: self.deps.delete_timeout_ms,
            port: port.to_string(),
            tape: self.deps.tape.clone(),
        };
        let ClearOutcome {
            cleared,
            delete_error,
        } = clear_transport(t, opts).await?;
        Ok(match sweep_transport(t, self.sweep_options(port)).await {
            Ok(mut r) => {
                r.status = if delete_error.is_some() {
                    SmsReadStatus::Error
                } else {
                    SmsReadStatus::Ok
                };
                r.error = delete_error;
                r.cleared = Some(cleared);
                r
            }
            Err(e) => {
                // `err.message.slice(0, 120)` (already cut by the sweep's
                // own mapping; the cut is idempotent).
                let why: String = {
                    let units: Vec<u16> = e.message.encode_utf16().take(120).collect();
                    String::from_utf16_lossy(&units)
                };
                let mut s = empty_sweep(
                    SmsReadStatus::Error,
                    &delete_error.unwrap_or_else(|| format!("post-clear read failed: {why}")),
                    0,
                    0,
                );
                s.cleared = Some(cleared);
                s
            }
        })
    }

    /// The last finished READ, if any (tests / diagnostics).
    pub fn last(&self) -> Option<Arc<SmsSweep>> {
        self.lock().last.as_ref().map(|(s, _)| s.clone())
    }
}

/// Await an op's result; a dead op (its task panicked: the sender is gone)
/// is the TS catch-all shape. (A TS throw past `runOp`'s own catch would take
/// `worker.ts`'s "crashed" path and publish nothing; Rust publishes this
/// `error` payload. Both are unreachable in practice — review WP2 S1 side
/// note, no D-row.)
async fn wait(mut rx: watch::Receiver<Option<Arc<SmsSweep>>>) -> Arc<SmsSweep> {
    loop {
        if let Some(s) = rx.borrow_and_update().clone() {
            return s;
        }
        if rx.changed().await.is_err() {
            return Arc::new(empty_sweep(SmsReadStatus::Error, "unknown", 0, 0));
        }
    }
}

/// Clears `SmsReader::inflight` on drop when it is still this op — the
/// `.finally` of `sms-modem.ts:658-660`, run on success, panic and
/// cancellation alike.
struct ClearInflight(Arc<SmsReader>, u64);

impl Drop for ClearInflight {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        if st.inflight.as_ref().is_some_and(|i| i.id == self.1) {
            st.inflight = None;
        }
    }
}

/// `finish({ status, error })` with the defaults of `sms-modem.ts:693-703`.
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
        cleared: None,
    }
}

/// `join(runtimeHome(), "sms-reader.lock")` (`worker.ts:917`, `sms.ts:46`).
pub fn lock_path(home: &Path) -> PathBuf {
    home.join(xorgate_core::sms::SMS_READER_LOCK_FILE)
}
