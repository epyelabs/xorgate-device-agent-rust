//! The explicit SMS clear — `clearTransport` of `src/hardware/sms-modem.ts`
//! @ 09da38d (`:436-544`, plans/device-sms-clear), Phase 5b WP3.
//!
//! **This is the ONLY module allowed to spell the modem's delete command**
//! (phase-5b §6 point 1): the one literal is [`CMGD_DELETE_ALL`] below, the
//! delete-all form (`<index> 0`, `<delflag> 4`: every message of the selected
//! store). No other delete flag and no single-index delete exists anywhere in
//! the workspace; `crates/xorgate-agent/tests/sms_clear_safety.rs` greps every
//! `.rs` file for it and fails the build otherwise, and lists the callers of
//! [`clear_transport`] (only `xorgate_agent::ops::sms::SmsReader`).
//!
//! Who may reach it (§6 point 2) — enforced by the COMPILER: the command
//! constant is private, [`clear_transport`] is its only sender and takes a
//! [`ClearRequest`] by value, and a `ClearRequest` can only be minted in
//! `crate::sms_clear_token` from a proof of an accepted, ledger-claimed
//! `clear_sms` or of a confirmed `xorgate-rs sms clear` (see that module).
//! Nothing else (read path, reuse window, sweep, startup, shutdown, manage
//! TUI) can hold one; a source test pins the mint sites.
//!
//! Port rules (§6 point 3) are the read's: the transport handed in was opened
//! on what `at_port::resolve_sms_port` returned (interface 05 by USB identity
//! or `XORGATE_SMS_PORT`, never the daemon's tty) and the caller holds the
//! cross-process lock for the clear AND the post-clear sweep.
//!
//! Order (§6 point 4, `sms-modem.ts:476-544`): `ATE0` → `AT+CPMS?` (silent →
//! `modem_busy`, no SIM → `no_sim`, otherwise unreadable → `error`; all three
//! mean "nothing deleted") → per store of `SWEEP_STORES` (SM, then ME): a
//! store known empty from `CPMS?` is skipped (`cleared 0`, never selected);
//! `AT+CPMS="<mem>"` (a timeout stops the phase; SM missing on a SIM-less
//! modem is skipped; another select error is recorded and the next store is
//! tried); the delete with the 25 s delete timeout (a timeout STOPS the
//! phase, so a late `OK` can never be taken as the next command's answer); a
//! delete error is recorded and the next store is tried → mem1 restored on
//! every path. `CMGF` is never touched: the delete-all works in text and PDU
//! mode alike.
//!
//! "`finally` on every path" in Rust: the store phase runs inside
//! [`CatchUnwind`]; the restore runs after it whatever it returned —
//! `Ok`, an `Err` (a transport error, rethrown after the restore as the TS
//! does), or a panic (restored, then the panic resumes). Cancellation: the
//! only caller, `SmsReader`, runs every clear on its own spawned task, so a
//! dropped caller never drops this future; only a runtime shutdown can, which
//! is the TS process exit (where `finally` does not run either).
//!
//! Privacy (contract §6): nothing here logs.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use xorgate_core::sms::{
    is_no_sim, parse_cpms_query, parse_cpms_set, ClearedCounts, SmsReadStatus, StoreCount,
    SMS_COMMAND_TIMEOUT_MS, SMS_DELETE_TIMEOUT_MS, SWEEP_STORES,
};
use xorgate_core::sms_pdu::SmsStorage;

use crate::at_port::{AtError, AtResponse, AtSession, AtTransport, SmsReadError};
use crate::io::{null_tape, TapeSink};
use crate::sms_clear_token::ClearRequest;

/// The delete-all command: every message of the store `CPMS` mem1 selects.
/// THE ONLY spelling of the delete command in the workspace (§6 point 1).
const CMGD_DELETE_ALL: &str = "AT+CMGD=0,4";

/// The delete command as TEXT, for `xorgate-rs sms clear --help` (which
/// prints it, `sms.ts:55`), so no other file has to spell it.
pub fn delete_all_text() -> &'static str {
    CMGD_DELETE_ALL
}

/// `ClearOptions` (`sms-modem.ts:448-453` @ 09da38d).
#[derive(Clone)]
pub struct ClearOptions {
    /// Per-command timeout for everything but the delete (plan: 3 s).
    pub command_timeout_ms: u64,
    /// Timeout for each delete (default [`SMS_DELETE_TIMEOUT_MS`]).
    pub delete_timeout_ms: u64,
    /// The port the transport is on (the tape's `at_tx.port`).
    pub port: String,
    pub tape: Arc<dyn TapeSink>,
}

impl Default for ClearOptions {
    /// The TS defaults (`opts.commandTimeoutMs ?? 3000`,
    /// `opts.deleteTimeoutMs ?? SMS_DELETE_TIMEOUT_MS`), no tape.
    fn default() -> Self {
        Self {
            command_timeout_ms: SMS_COMMAND_TIMEOUT_MS,
            delete_timeout_ms: SMS_DELETE_TIMEOUT_MS,
            port: String::new(),
            tape: null_tape(),
        }
    }
}

/// `ClearOutcome` (`sms-modem.ts:455-460` @ 09da38d).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearOutcome {
    /// Messages present in each store right before its delete.
    pub cleared: ClearedCounts,
    /// `None` when every store that had messages answered the delete `OK`;
    /// otherwise the failures, `; `-joined, each naming its store.
    pub delete_error: Option<String>,
}

/// `clearTransport(transport, opts)` (`sms-modem.ts:478-544` @ 09da38d):
/// delete every message in SM, then ME, over an open transport. Does not
/// close it and does not sweep (the reader sweeps next on the same
/// transport and lock, so the reply proves the stores are empty).
///
/// `Err` before anything was deleted maps to the contract's "nothing was
/// deleted" statuses (`modem_busy` / `no_sim` / `error`). Once the store
/// phase started, failures go to `delete_error`; the one exception is a
/// transport (non-timeout) error, which the TS rethrows past its `finally`:
/// here it is `Err(error, <message>)` after the restore, as the reader's
/// catch maps it.
pub async fn clear_transport(
    transport: &mut dyn AtTransport,
    opts: ClearOptions,
    token: ClearRequest,
) -> Result<ClearOutcome, SmsReadError> {
    // The authorisation is spent whatever happens next.
    token.spend();
    let mut at =
        AtSession::new(transport, opts.command_timeout_ms).with_tape(&opts.port, opts.tape);
    let delete_timeout = Duration::from_millis(opts.delete_timeout_ms);

    match at.command("ATE0", None, false).await {
        Ok(_) => {}
        Err(AtError::Timeout(_)) => {
            return Err(SmsReadError::new(
                SmsReadStatus::ModemBusy,
                "no response on sms port",
            ))
        }
        Err(e) => return Err(e.into_read_error()),
    }
    let cpms = at
        .command("AT+CPMS?", Some("+CPMS:"), false)
        .await
        .map_err(AtError::into_read_error)?;
    if !cpms.ok {
        if is_no_sim(&cpms.final_line) {
            return Err(SmsReadError::new(SmsReadStatus::NoSim, "sim not ready"));
        }
        return Err(SmsReadError::new(
            SmsReadStatus::Error,
            format!("CPMS? failed: {}", cpms.final_line),
        ));
    }
    let triples = parse_cpms_query(cpms.lines.first().map(String::as_str).unwrap_or(""));
    let Some(first) = triples.first() else {
        return Err(SmsReadError::new(
            SmsReadStatus::Error,
            "unparseable CPMS? answer",
        ));
    };
    let orig_mem1 = first.mem.clone();
    // `known[t.mem] = {used, total}` for SM / ME: the last mention wins.
    let mut known: Vec<(SmsStorage, StoreCount)> = Vec::new();
    for t in &triples {
        if let Some(mem) = SmsStorage::parse(&t.mem) {
            let c = StoreCount {
                used: t.used,
                total: t.total,
            };
            match known.iter_mut().find(|(m, _)| *m == mem) {
                Some(slot) => slot.1 = c,
                None => known.push((mem, c)),
            }
        }
    }
    let known_of = |mem: SmsStorage| known.iter().find(|(m, _)| *m == mem).map(|(_, c)| *c);

    let mut cleared = ClearedCounts::new();
    let mut failures: Vec<String> = Vec::new();
    let mut mem1_changed = false;
    // The `try` block (`:507-540`); its `finally` (`:541-543`) runs below on
    // every outcome of this future, a panic included.
    let phase = CatchUnwind(Box::pin(async {
        for mem in SWEEP_STORES {
            if known_of(mem).is_some_and(|k| k.used == 0) {
                cleared.insert(mem, 0);
                continue;
            }
            let sel = match at
                .command(
                    &format!("AT+CPMS=\"{}\"", mem.as_str()),
                    Some("+CPMS:"),
                    false,
                )
                .await
            {
                Ok(r) => r,
                Err(AtError::Timeout(_)) => {
                    // It may have switched before going quiet.
                    mem1_changed = true;
                    failures.push(format!("delete {} failed (select timeout)", mem.as_str()));
                    break;
                }
                Err(e) => return Err(e),
            };
            mem1_changed = true;
            if !sel.ok {
                // SM missing on a SIM-less modem: nothing there to clear.
                if mem == SmsStorage::SM && is_no_sim(&sel.final_line) {
                    continue;
                }
                failures.push(format!(
                    "delete {} failed (select: {})",
                    mem.as_str(),
                    sel.final_line
                ));
                continue;
            }
            let counts = sel
                .lines
                .first()
                .and_then(|l| parse_cpms_set(l))
                .or(known_of(mem));
            cleared.insert(mem, counts.map(|c| c.used).unwrap_or(0));
            // An unparseable select answer gives no count: delete anyway, it
            // is what the operator asked for and the delete-all on an empty
            // store is harmless.
            if counts.is_some_and(|c| c.used == 0) {
                continue;
            }
            let del: AtResponse = match at
                .command_with_timeout(CMGD_DELETE_ALL, None, false, delete_timeout)
                .await
            {
                Ok(r) => r,
                Err(AtError::Timeout(_)) => {
                    failures.push(format!("delete {} failed (timeout)", mem.as_str()));
                    break;
                }
                Err(e) => return Err(e),
            };
            if !del.ok {
                failures.push(format!(
                    "delete {} failed ({})",
                    mem.as_str(),
                    del.final_line
                ));
            }
        }
        Ok::<(), AtError>(())
    }))
    .await;
    // `finally { if (mem1Changed) await at.command(`AT+CPMS="${origMem1}"`)
    // .catch(() => undefined); }` — on Ok, Err and panic alike.
    if mem1_changed {
        let _ = at
            .command(&format!("AT+CPMS=\"{orig_mem1}\""), Some("+CPMS:"), false)
            .await;
    }
    match phase {
        Ok(Ok(())) => Ok(ClearOutcome {
            cleared,
            delete_error: (!failures.is_empty()).then(|| failures.join("; ")),
        }),
        Ok(Err(e)) => Err(e.into_read_error()),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// `catch_unwind` for a boxed future (no `futures` crate): a panic while
/// polling resolves to `Err(payload)` so the caller can run its `finally`
/// before resuming the unwind.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(p) => Poll::Ready(Err(p)),
        }
    }
}
