//! The worker's command handler as DATA (Phase 2 WP4; CF7 + CF8):
//! `handleCommandMessage` (`worker.ts:988-1126`), `runSmsRead`
//! (`:924-945`) and the post-`executeCommand` branch (`:1100-1125`).
//!
//! [`decide_command`] is pure: it turns one parsed command, the current
//! ledger and the refresh window into an ordered list of [`Effect`]s in the
//! exact order the TS performs them. The agent's command plane is only an
//! interpreter of that list (it writes the ledger, logs, notes events,
//! publishes and awaits acks, writes the refresh flag, asks for reports,
//! runs `sudo`). The two decisions that depend on I/O results are separate
//! pure functions: [`ledger_write_failed`] (the claim's `atomicWriteJson`
//! threw) and [`execution_outcome`] (what `sudo` returned).
//!
//! **ORDER** (`worker.ts:988-1126`, reproduced by [`decide_command`]):
//! 1. malformed → log + event, NO ack (`:991-997`);
//! 2. expired → log + event + ack `rejected expired`, NOT claimed (`:999-1006`);
//! 3. claim BEFORE executing; a write error → [`ledger_write_failed`] (`:1010-1021`);
//! 4. already claimed → ack `rejected duplicate` (`:1022-1028`);
//! 5. replay `update` → ack `rejected managed by host image` — CLAIMED, never "accepted" (`:1032-1038`);
//! 6. `<cmd> command accepted` log + event (`:1040-1042`);
//! 7. `refresh`: rate-limited → ack `rejected rate-limited` (claimed, the
//!    window mark untouched); else mark, ack accepted, flag, report now and
//!    at +45 s (`:1047-1068`);
//! 8. `read_sms`: ack accepted, then the sweep (`:1074-1083`);
//! 9. replay `reboot`/`power_off`: ack accepted, container exit (`:1090-1098`);
//! 10. ack accepted → `executeCommand` → [`execution_outcome`] (`:1100-1125`).
//!
//! Every consumer-visible string (state.json event `msg`, operator log line,
//! ack reason) is a const or formatter here with its TS line (CF8).
//!
//! **O2 (CF9):** the refresh window is measured on the MONOTONIC clock in the
//! agent ([`RefreshWindow`] is clock-agnostic; the fixture feeds it the TS
//! wall values). Values are identical unless the wall clock steps.
//! [`crate::commands::refresh_rate_limited`] stays TS-exact for its fixture.

use serde::{Deserialize, Serialize};

use crate::clock::{Clock, WallMs};
use crate::commands::{
    claim, container_exit_code, failure_reason, is_expired, plan, refresh_plan,
    refresh_rate_limited, AckStatus, CommandPlan, DeviceCommand, DeviceCommandName, Ledger,
    PrivilegedCall, RejectReason, RunResult, REFRESH_REPROBE_REPORT_MS,
};
use crate::js::number_to_string;
use crate::round::js_math_round;
use crate::sms::{
    fixture_sms_sweep, SmsReadStatus, SmsResultPayload, SmsSweep, StorageCounts, SMS_LIMIT_DEFAULT,
};
use crate::telemetry::{ActivityLevel, RunMode};

// ---------------------------------------------------------------------------
// Consumer-visible strings (CF8)
// ---------------------------------------------------------------------------

/// The `ledger write failed: ` ack-reason prefix (`worker.ts:1015`).
pub const LEDGER_WRITE_FAILED_PREFIX: &str = "ledger write failed: ";

/// Log + event for a payload `parseCommand` refused (`worker.ts:993-994`).
pub const MALFORMED_MSG: &str = "ignoring malformed command message";

/// O4: the `error` of the real-mode `read_sms` result until the Phase 4 SMS
/// reader exists (`phase-2-cloud-plane.md` O4; tolerance T8).
pub const SMS_READER_UNAVAILABLE: &str = "sms reader not available in this build";

/// `[agent] ` — every worker line carries it (`console.*("[agent] …")`).
pub const AGENT_PREFIX: &str = "[agent] ";

/// `Math.round((now - cmd.ts) / 1000)` printed like JS (`worker.ts:1000`).
pub fn expired_age_text(now: WallMs, cmd_ts: f64) -> String {
    number_to_string(js_math_round((now as f64 - cmd_ts) / 1000.0))
}

/// `[agent] rejecting expired ${cmd} command (issued ${age}s ago)` (`worker.ts:1001`).
pub fn expired_log(command: DeviceCommandName, age: &str) -> String {
    format!("{AGENT_PREFIX}rejecting expired {command} command (issued {age}s ago)")
}

/// `rejected expired ${cmd} command (${age}s old)` (`worker.ts:1002`).
pub fn expired_event(command: DeviceCommandName, age: &str) -> String {
    format!("rejected expired {command} command ({age}s old)")
}

/// `ledger write failed: ${err.message}` (`worker.ts:1015`).
pub fn ledger_write_failed_reason(message: &str) -> String {
    format!("{LEDGER_WRITE_FAILED_PREFIX}{message}")
}

/// `[agent] refusing ${cmd}: ${reason}` (`worker.ts:1016`).
pub fn refusing_log(command: DeviceCommandName, reason: &str) -> String {
    format!("{AGENT_PREFIX}refusing {command}: {reason}")
}

/// `refused ${cmd}: ${reason}` (`worker.ts:1017`).
pub fn refused_event(command: DeviceCommandName, reason: &str) -> String {
    format!("refused {command}: {reason}")
}

/// `[agent] ignoring duplicate ${cmd} command ${requestId}` (`worker.ts:1023`).
pub fn duplicate_log(command: DeviceCommandName, request_id: &str) -> String {
    format!("{AGENT_PREFIX}ignoring duplicate {command} command {request_id}")
}

/// `ignored duplicate ${cmd} command` (`worker.ts:1024`).
pub fn duplicate_event(command: DeviceCommandName) -> String {
    format!("ignored duplicate {command} command")
}

/// `[agent] update command rejected (${requestId}): managed by host image` (`worker.ts:1033`).
pub fn replay_update_log(request_id: &str) -> String {
    format!("{AGENT_PREFIX}update command rejected ({request_id}): managed by host image")
}

/// `update command rejected: managed by host image` (`worker.ts:1034`).
pub const REPLAY_UPDATE_EVENT: &str = "update command rejected: managed by host image";

/// `[agent] ${cmd} command accepted (${requestId})` (`worker.ts:1040`).
pub fn accepted_log(command: DeviceCommandName, request_id: &str) -> String {
    format!("{AGENT_PREFIX}{command} command accepted ({request_id})")
}

/// `${cmd} command accepted` (`worker.ts:1041`).
pub fn accepted_event(command: DeviceCommandName) -> String {
    format!("{command} command accepted")
}

/// `[agent] refresh flag write failed: ${err.message}` (`worker.ts:1060`).
pub fn refresh_flag_failed_log(message: &str) -> String {
    format!("{AGENT_PREFIX}refresh flag write failed: {message}")
}

/// `[agent] ${cmd}: exiting with code ${code} (replay container)` (`worker.ts:1094`).
pub fn container_exit_log(command: DeviceCommandName, code: i32) -> String {
    format!("{AGENT_PREFIX}{command}: exiting with code {code} (replay container)")
}

/// `${cmd}: container exit ${code}` (`worker.ts:1095`).
pub fn container_exit_event(command: DeviceCommandName, code: i32) -> String {
    format!("{command}: container exit {code}")
}

/// `` `${cmd} command` `` — the `shutdown(reason, code)` reason (`worker.ts:1097`).
pub fn container_exit_reason(command: DeviceCommandName) -> String {
    format!("{command} command")
}

/// `[agent] updater started; this process will be replaced when it finishes` (`worker.ts:1111`).
pub const UPDATER_STARTED_LOG: &str =
    "[agent] updater started; this process will be replaced when it finishes";

/// `agent self-update started` (`worker.ts:1112`).
pub const UPDATER_STARTED_EVENT: &str = "agent self-update started";

/// `[agent] ${cmd} failed: ${reason}` (`worker.ts:1122`).
pub fn exec_failed_log(command: DeviceCommandName, reason: &str) -> String {
    format!("{AGENT_PREFIX}{command} failed: {reason}")
}

/// `${cmd} failed: ${reason}` (`worker.ts:1123`).
pub fn exec_failed_event(command: DeviceCommandName, reason: &str) -> String {
    format!("{command} failed: {reason}")
}

/// The `read_sms` summary line (`worker.ts:932-936`), WITHOUT the `[agent] `
/// prefix: it is the state.json event `msg` as is, and the log line with
/// the prefix. Counts / status / duration only — never a number or a body
/// (contract §6).
pub fn read_sms_line(request_id: &str, p: &SmsResultPayload) -> String {
    // Phase 5b: one implementation for both commands (`runSms` @ 09da38d).
    crate::sms::sms_result_line("read_sms", request_id, p)
}

/// The read_sms event level (`worker.ts:939`): `info` when ok, else `warn`.
/// The log line is `console.log` either way (`:937`).
pub fn read_sms_level(p: &SmsResultPayload) -> ActivityLevel {
    if p.status == SmsReadStatus::Ok {
        ActivityLevel::Info
    } else {
        ActivityLevel::Warn
    }
}

/// `[agent] ${command} rejected (${requestId}): no OBD peripheral on this
/// device` (`worker.ts:1221` @ 09da38d) — `obd_scan` / `obd_forget` without
/// the collector.
pub fn obd_rejected_log(command: DeviceCommandName, request_id: &str) -> String {
    format!("{AGENT_PREFIX}{command} rejected ({request_id}): no OBD peripheral on this device")
}

/// `${command} rejected: no OBD peripheral on this device` (`worker.ts:1222` @ 09da38d).
pub fn obd_rejected_event(command: DeviceCommandName) -> String {
    format!("{command} rejected: no OBD peripheral on this device")
}

/// `scan: ${n} candidate(s)` (`worker.ts:1231-1233` @ 09da38d).
pub fn obd_scan_summary(candidates: usize) -> String {
    format!("scan: {candidates} candidate(s)")
}

/// `forget: ${mac ?? "no adapter"}` (`worker.ts:1234` @ 09da38d).
pub fn obd_forget_summary(mac: Option<&str>) -> String {
    format!("forget: {}", mac.unwrap_or("no adapter"))
}

/// `[agent] ${command} ${requestId}: ${summary}` (`worker.ts:1235` @ 09da38d),
/// `console.log`; followed by `store.flush(t, true)` and
/// `configSync.request("boot")`.
pub fn obd_job_done_log(command: DeviceCommandName, request_id: &str, summary: &str) -> String {
    format!("{AGENT_PREFIX}{command} {request_id}: {summary}")
}

/// `[agent] ${command} ${requestId} crashed: ${err.message}` (`console.error`;
/// `worker.ts:1203, 1243` @ 09da38d — the SMS and OBD job nets).
pub fn job_crashed_log(command: DeviceCommandName, request_id: &str, message: &str) -> String {
    format!("{AGENT_PREFIX}{command} {request_id} crashed: {message}")
}

/// `${command} crashed: ${err.message}` (warn event; `worker.ts:1204, 1244` @ 09da38d).
pub fn job_crashed_event(command: DeviceCommandName, message: &str) -> String {
    format!("{command} crashed: {message}")
}

/// `[agent] read_sms ${requestId}: publish failed: ${err.message}` (`worker.ts:942`).
pub fn read_sms_publish_failed_log(request_id: &str, message: &str) -> String {
    format!("{AGENT_PREFIX}read_sms {request_id}: publish failed: {message}")
}

// ---------------------------------------------------------------------------
// Effects
// ---------------------------------------------------------------------------

/// Which `console` method the TS line goes through (journal priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleLevel {
    /// `console.log`
    Info,
    /// `console.warn`
    Warn,
    /// `console.error`
    Error,
}

/// Where a `read_sms` sweep comes from (`worker.ts:929-930`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmsSource {
    /// `config.mode === "real"`: the modem reader. Phase 2: the O4 sweep
    /// ([`sms_reader_unavailable_sweep`]) until the Phase 4 reader lands.
    Reader,
    /// `sim` / `replay`: [`fixture_sms_sweep`] at the time of the sweep.
    Fixture,
}

/// The accepted-path marker of [`Effect::ClearSms`]: its field is private
/// and its one constructor is private to this module, called only by
/// [`decide_command`]'s `clear_sms` arm (pinned by
/// `xorgate-agent/tests/sms_clear_safety.rs`). Deliberately NOT `Clone` (so
/// neither is [`Effect`] nor [`CommandDecision`]): a claim cannot be copied
/// into a second `Effect::ClearSms` (review-wp3 N5; pinned at compile time by
/// `the_clear_claim_is_not_clone`).
#[derive(Debug, PartialEq, Eq)]
pub struct ClearClaim {
    _private: (),
}

impl ClearClaim {
    fn after_claim() -> Self {
        Self { _private: () }
    }
}

/// One side effect of the command handler, in TS order.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// `claimCommand`'s `atomicWriteJson(commands.json)` — ALWAYS first when
    /// present. On a write error the interpreter drops every later effect
    /// and runs [`ledger_write_failed`] instead (`worker.ts:1010-1021`).
    PersistLedger(Ledger),
    /// `console.{log,warn,error}` — the full line, `[agent] ` included.
    Log(ConsoleLevel, String),
    /// `store.event(level, msg, t); store.flush(t, true)`.
    Event(ActivityLevel, String),
    /// `await publishCommandAck({...cmd, status, reason})` (`worker.ts:953-974`):
    /// publish QoS 1 on `…/command/response`, then wait for the PUBACK or
    /// `ACK_FLUSH_TIMEOUT_MS`, whichever is first, before the next effect.
    Ack {
        status: AckStatus,
        reason: Option<String>,
    },
    /// `writeFileSync(<home>/refresh-modem.flag, String(now))` (`worker.ts:1058`);
    /// an error only logs [`refresh_flag_failed_log`].
    WriteRefreshFlag(String),
    /// `configSync.request("boot")` (`worker.ts:1065`).
    RequestReport,
    /// `setTimeout(() => configSync.request("boot"), ms).unref()` (`worker.ts:1066`).
    ScheduleReport { after_ms: i64 },
    /// `void runSmsRead(cmd)` (`worker.ts:1076`): sweep, log + event, publish.
    ReadSms {
        source: SmsSource,
        request_id: String,
        limit: u32,
    },
    /// `void runSms({...cmd, command: "clear_sms"})` (`worker.ts:1200-1206` @
    /// 09da38d): clear (real → the reader's `clear()`, else the fixture
    /// clear), log + event, publish `action:"clear"`. A crash →
    /// [`job_crashed_log`] / [`job_crashed_event`]. Comes AFTER the accepted
    /// ack and the ledger claim (invariant §6.2: a redelivery never clears twice).
    ClearSms {
        source: SmsSource,
        request_id: String,
        limit: u32,
        /// Proof that [`decide_command`] produced this effect on its accepted
        /// path (after the expiry check and the ledger claim). Only this
        /// module can build one, so no other code can construct an
        /// `Effect::ClearSms` (review-wp3 F2; enum variants have no
        /// struct-update syntax). Carries no data.
        claim: ClearClaim,
    },
    /// `obd_scan` / `obd_forget` with the collector present (`worker.ts:1219-1247`
    /// @ 09da38d). The TS STARTS the job (`source.obdScan?.()`) before the
    /// ack, so the job's synchronous start event (`scanning for Bluetooth
    /// adapters …`) precedes the ack: the interpreter calls
    /// `ObdControl::scan()` / `forget()` here, then runs the following
    /// `Ack accepted`, then awaits the job → [`obd_job_done_log`] with
    /// [`obd_scan_summary`] / [`obd_forget_summary`], flush, request report
    /// `boot`; a crash → [`job_crashed_log`] + event + request report `boot`.
    ObdJob(DeviceCommandName),
    /// `executeCommand(cmd.command)` then [`execution_outcome`] (`worker.ts:1102-1125`).
    /// The call is the TS-exact plan; the agent substitutes the O3 unit.
    Execute(PrivilegedCall),
    /// Replay mode: `shutdown(\`${cmd} command\`, code)` (`worker.ts:1097`).
    ContainerExit { reason: String, code: i32 },
}

/// The refresh rate-limit window (`lastRefreshAt`, `worker.ts:909, 1048`).
/// `now` and `last` are on the same clock: wall ms in the TS (and the
/// fixture), monotonic ms in the agent (O2). `last: None` = never refreshed
/// (the TS `0`, which is always ≥ 30 s in the past on a real wall clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshWindow {
    pub now: i64,
    pub last: Option<i64>,
}

/// What [`decide_command`] decided.
#[derive(Debug, PartialEq)]
pub struct CommandDecision {
    /// The parsed command (`None` = malformed).
    pub command: Option<DeviceCommand>,
    /// The effects, in TS order.
    pub effects: Vec<Effect>,
    /// `lastRefreshAt = now` (`worker.ts:1051`): set ONLY by an honoured
    /// refresh, applied by the interpreter once the ledger write succeeded.
    pub refresh_mark: Option<i64>,
}

impl CommandDecision {
    fn new(command: Option<&DeviceCommand>) -> Self {
        Self {
            command: command.cloned(),
            effects: Vec::new(),
            refresh_mark: None,
        }
    }

    fn push(&mut self, e: Effect) {
        self.effects.push(e);
    }

    fn note(&mut self, console: ConsoleLevel, line: String, level: ActivityLevel, msg: String) {
        self.push(Effect::Log(console, line));
        self.push(Effect::Event(level, msg));
    }

    fn ack(&mut self, status: AckStatus, reason: Option<String>) {
        self.push(Effect::Ack { status, reason });
    }
}

/// `handleCommandMessage` (`worker.ts:988-1126`) up to (and including) the
/// call it hands to the I/O world, as data. `parsed` is
/// [`crate::commands::parse_command`] of the payload; `now` is the handler's
/// `Date.now()` (`:989`); `ledger` is what `loadLedger` reads at claim time
/// (`commands.ts:209`); `mode` is `config.mode` (`replay` ⇔ the worker's
/// `replay` source is non-null, `worker.ts:235-236`).
pub fn decide_command(
    parsed: Option<&DeviceCommand>,
    now: WallMs,
    ledger: &Ledger,
    refresh: RefreshWindow,
    mode: RunMode,
    obd_present: bool,
) -> CommandDecision {
    let mut d = CommandDecision::new(parsed);
    // 1. malformed (`:990-997`): no trustworthy requestId to ack against.
    let Some(cmd) = parsed else {
        d.note(
            ConsoleLevel::Error,
            format!("{AGENT_PREFIX}{MALFORMED_MSG}"),
            ActivityLevel::Warn,
            MALFORMED_MSG.to_string(),
        );
        return d;
    };
    let name = cmd.command;

    // 2. expired (`:999-1006`): rejected, NOT claimed.
    if is_expired(cmd, now) {
        let age = expired_age_text(now, cmd.ts);
        d.note(
            ConsoleLevel::Warn,
            expired_log(name, &age),
            ActivityLevel::Warn,
            expired_event(name, &age),
        );
        d.ack(
            AckStatus::Rejected,
            Some(RejectReason::Expired.as_str().to_string()),
        );
        return d;
    }

    // 3–4. claim BEFORE executing (`:1008-1028`).
    let claimed = claim(ledger, &cmd.request_id, now);
    if !claimed.accepted {
        d.note(
            ConsoleLevel::Warn,
            duplicate_log(name, &cmd.request_id),
            ActivityLevel::Warn,
            duplicate_event(name),
        );
        d.ack(
            AckStatus::Rejected,
            Some(RejectReason::Duplicate.as_str().to_string()),
        );
        return d;
    }
    d.push(Effect::PersistLedger(claimed.ledger));

    let replay = mode == RunMode::Replay;
    // 5. replay `update` (`:1032-1038`): refused before anything says accepted.
    if replay && name == DeviceCommandName::Update {
        d.note(
            ConsoleLevel::Warn,
            replay_update_log(&cmd.request_id),
            ActivityLevel::Warn,
            REPLAY_UPDATE_EVENT.to_string(),
        );
        d.ack(
            AckStatus::Rejected,
            Some(RejectReason::ManagedByHostImage.as_str().to_string()),
        );
        return d;
    }

    // 6. accepted trail (`:1040-1042`) — also for a refresh rate-limited below.
    d.note(
        ConsoleLevel::Info,
        accepted_log(name, &cmd.request_id),
        ActivityLevel::Info,
        accepted_event(name),
    );

    match name {
        // 7. refresh (`:1047-1068`): in-process, never via sudo.
        DeviceCommandName::Refresh => {
            let limited = refresh
                .last
                .is_some_and(|last| refresh_rate_limited(refresh.now, last));
            if limited {
                d.ack(
                    AckStatus::Rejected,
                    Some(RejectReason::RateLimited.as_str().to_string()),
                );
                return d;
            }
            d.refresh_mark = Some(refresh.now);
            d.ack(AckStatus::Accepted, None);
            let p = refresh_plan(now);
            d.push(Effect::WriteRefreshFlag(p.flag_content));
            d.push(Effect::RequestReport);
            d.push(Effect::ScheduleReport {
                after_ms: REFRESH_REPROBE_REPORT_MS,
            });
        }
        // 8. read_sms (`:1074-1083`): ack, then the sweep.
        DeviceCommandName::ReadSms => {
            d.ack(AckStatus::Accepted, None);
            d.push(Effect::ReadSms {
                source: if mode == RunMode::Real {
                    SmsSource::Reader
                } else {
                    SmsSource::Fixture
                },
                request_id: cmd.request_id.clone(),
                limit: cmd.args.map(|a| a.limit).unwrap_or(SMS_LIMIT_DEFAULT),
            });
        }
        // 8b. clear_sms (`worker.ts:1195-1207` @ 09da38d): the read path's twin.
        DeviceCommandName::ClearSms => {
            d.ack(AckStatus::Accepted, None);
            d.push(Effect::ClearSms {
                source: if mode == RunMode::Real {
                    SmsSource::Reader
                } else {
                    SmsSource::Fixture
                },
                request_id: cmd.request_id.clone(),
                limit: cmd.args.map(|a| a.limit).unwrap_or(SMS_LIMIT_DEFAULT),
                claim: ClearClaim::after_claim(),
            });
        }
        // 8c. obd_scan / obd_forget (`worker.ts:1217-1249` @ 09da38d).
        DeviceCommandName::ObdScan | DeviceCommandName::ObdForget => {
            if !obd_present {
                d.note(
                    ConsoleLevel::Warn,
                    obd_rejected_log(name, &cmd.request_id),
                    ActivityLevel::Warn,
                    obd_rejected_event(name),
                );
                d.ack(
                    AckStatus::Rejected,
                    Some(RejectReason::NoObdPeripheral.as_str().to_string()),
                );
                return d;
            }
            d.push(Effect::ObdJob(name));
            d.ack(AckStatus::Accepted, None);
        }
        _ => {
            d.ack(AckStatus::Accepted, None);
            // 9. replay container exit (`:1090-1098`).
            if replay {
                if let Some(code) = container_exit_code(name) {
                    d.note(
                        ConsoleLevel::Info,
                        container_exit_log(name, code),
                        ActivityLevel::Info,
                        container_exit_event(name, code),
                    );
                    d.push(Effect::ContainerExit {
                        reason: container_exit_reason(name),
                        code,
                    });
                    return d;
                }
            }
            // 10. the privileged call (`:1100-1102`).
            match plan(name) {
                CommandPlan::Privileged(call) => d.push(Effect::Execute(call)),
                // Unreachable: refresh/read_sms returned above.
                CommandPlan::InProcess(_) => {}
            }
        }
    }
    d
}

/// The claim's write threw (`worker.ts:1012-1020`): can't write the ledger
/// ⇒ can't promise this won't replay ⇒ don't do it. `message` is the Node
/// `err.message` of the failed write.
pub fn ledger_write_failed(cmd: &DeviceCommand, message: &str) -> Vec<Effect> {
    let reason = ledger_write_failed_reason(message);
    vec![
        Effect::Log(ConsoleLevel::Error, refusing_log(cmd.command, &reason)),
        Effect::Event(ActivityLevel::Warn, refused_event(cmd.command, &reason)),
        Effect::Ack {
            status: AckStatus::Failed,
            reason: Some(reason),
        },
    ]
}

/// What follows `executeCommand` (`worker.ts:1102-1125`). Success: nothing
/// (systemd's SIGTERM drives shutdown), except the `update` trail. Failure:
/// log + event + ack `failed` with [`failure_reason`].
pub fn execution_outcome(command: DeviceCommandName, res: &RunResult) -> Vec<Effect> {
    if !res.failed {
        if command == DeviceCommandName::Update {
            return vec![
                Effect::Log(ConsoleLevel::Info, UPDATER_STARTED_LOG.to_string()),
                Effect::Event(ActivityLevel::Info, UPDATER_STARTED_EVENT.to_string()),
            ];
        }
        return Vec::new();
    }
    let reason = failure_reason(res);
    vec![
        Effect::Log(ConsoleLevel::Error, exec_failed_log(command, &reason)),
        Effect::Event(ActivityLevel::Warn, exec_failed_event(command, &reason)),
        Effect::Ack {
            status: AckStatus::Failed,
            reason: Some(reason),
        },
    ]
}

/// O4: the real-mode sweep until the Phase 4 SMS reader exists. Shaped like
/// the reader's own `finish({status:"error", error})` (`sms-modem.ts:507-521`):
/// empty storage, nothing skipped, not partial, no messages, finished `now`,
/// lasting `duration_ms` (see [`sms_sweep`]).
pub fn sms_reader_unavailable_sweep(now: WallMs, duration_ms: i64) -> SmsSweep {
    SmsSweep {
        status: SmsReadStatus::Error,
        error: Some(SMS_READER_UNAVAILABLE.to_string()),
        finished_at: now,
        duration_ms,
        storage: StorageCounts::new(),
        skipped: 0,
        partial: false,
        messages: Vec::new(),
        cleared: None,
    }
}

/// The sweep `runSmsRead` gets (`worker.ts:929-930`).
///
/// `Reader` (real mode) is timed like `SmsReader.runSweep`
/// (`sms-modem.ts:504-519`): `start = now()` when the attempt begins,
/// `end = now()` in `finish`, `finishedAt: end`, `durationMs: end - start` —
/// integer ms, here on the monotonic clock (agent-anatomy invariant 2) so a
/// wall step cannot make it negative. `Fixture` (`sim` / `replay`) is
/// `fixtureSmsSweep(Date.now())`, which is NOT timed in the TS: its
/// `durationMs` is the literal `0` (`sms-modem.ts:686`), kept here.
pub fn sms_sweep(source: SmsSource, clock: &dyn Clock) -> SmsSweep {
    match source {
        SmsSource::Reader => {
            let start = clock.mono_ms();
            let finished_at = clock.wall_ms();
            let end = clock.mono_ms();
            sms_reader_unavailable_sweep(finished_at, end.saturating_sub(start) as i64)
        }
        SmsSource::Fixture => fixture_sms_sweep(clock.wall_ms()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::parse_command;

    const T0: i64 = 1_750_000_000_000;

    fn cmd(json: &str) -> DeviceCommand {
        parse_command(json.as_bytes()).unwrap()
    }

    /// Every `mono_ms()` read advances mono by `step` (a sweep that takes time).
    struct TickingClock {
        wall: WallMs,
        mono: std::cell::Cell<u64>,
        step: u64,
    }

    impl Clock for TickingClock {
        fn wall_ms(&self) -> WallMs {
            self.wall + self.mono.get() as i64
        }
        fn mono_ms(&self) -> u64 {
            let m = self.mono.get();
            self.mono.set(m + self.step);
            m
        }
    }

    /// `worker.ts:1217-1226` @ 09da38d: without the collector, `obd_scan` /
    /// `obd_forget` are claimed, logged "accepted", then rejected on the ack
    /// with `no OBD peripheral` (+ the warn log and event, verbatim).
    #[test]
    fn obd_commands_without_the_collector_are_rejected() {
        for (name, wire) in [
            (DeviceCommandName::ObdScan, "obd_scan"),
            (DeviceCommandName::ObdForget, "obd_forget"),
        ] {
            let c = cmd(&format!(
                r#"{{"v":1,"requestId":"r-1","command":"{wire}","ts":{T0},"expiresAt":{}}}"#,
                T0 + 60_000
            ));
            let d = decide_command(
                Some(&c),
                T0 + 1,
                &Ledger::empty(),
                RefreshWindow { now: 0, last: None },
                RunMode::Real,
                false,
            );
            assert!(matches!(d.effects[0], Effect::PersistLedger(_)));
            assert_eq!(
                d.effects[1..],
                [
                    Effect::Log(
                        ConsoleLevel::Info,
                        format!("[agent] {wire} command accepted (r-1)")
                    ),
                    Effect::Event(ActivityLevel::Info, format!("{wire} command accepted")),
                    Effect::Log(
                        ConsoleLevel::Warn,
                        format!("[agent] {wire} rejected (r-1): no OBD peripheral on this device")
                    ),
                    Effect::Event(
                        ActivityLevel::Warn,
                        format!("{wire} rejected: no OBD peripheral on this device")
                    ),
                    Effect::Ack {
                        status: AckStatus::Rejected,
                        reason: Some("no OBD peripheral".into())
                    },
                ]
            );
            // With the collector: the job starts BEFORE the accepted ack.
            let d = decide_command(
                Some(&c),
                T0 + 1,
                &Ledger::empty(),
                RefreshWindow { now: 0, last: None },
                RunMode::Sim,
                true,
            );
            assert_eq!(
                d.effects[3..],
                [
                    Effect::ObdJob(name),
                    Effect::Ack {
                        status: AckStatus::Accepted,
                        reason: None
                    },
                ]
            );
        }
        assert_eq!(obd_scan_summary(3), "scan: 3 candidate(s)");
        assert_eq!(obd_forget_summary(None), "forget: no adapter");
        assert_eq!(
            obd_forget_summary(Some("AA:BB:CC:DD:EE:FF")),
            "forget: AA:BB:CC:DD:EE:FF"
        );
        assert_eq!(
            obd_job_done_log(DeviceCommandName::ObdScan, "r-1", "scan: 0 candidate(s)"),
            "[agent] obd_scan r-1: scan: 0 candidate(s)"
        );
    }

    /// `worker.ts:1195-1207` @ 09da38d: `clear_sms` acks, then runs the clear
    /// (real → the reader, sim → the fixture inbox); a duplicate never
    /// reaches it (the ledger claim comes first).
    #[test]
    fn clear_sms_acks_then_clears_and_a_duplicate_never_clears() {
        let c = cmd(&format!(
            r#"{{"v":1,"requestId":"r-2","command":"clear_sms","ts":{T0},"expiresAt":{},"args":{{"limit":500}}}}"#,
            T0 + 60_000
        ));
        let d = decide_command(
            Some(&c),
            T0 + 1,
            &Ledger::empty(),
            RefreshWindow { now: 0, last: None },
            RunMode::Real,
            false,
        );
        assert_eq!(
            d.effects[3..],
            [
                Effect::Ack {
                    status: AckStatus::Accepted,
                    reason: None
                },
                Effect::ClearSms {
                    source: SmsSource::Reader,
                    request_id: "r-2".into(),
                    limit: 50,
                    claim: ClearClaim::after_claim(),
                },
            ]
        );
        let Effect::PersistLedger(ledger) = &d.effects[0] else {
            panic!("claim first")
        };
        let again = decide_command(
            Some(&c),
            T0 + 2,
            ledger,
            RefreshWindow { now: 0, last: None },
            RunMode::Real,
            false,
        );
        assert!(again
            .effects
            .iter()
            .all(|e| !matches!(e, Effect::ClearSms { .. })));
        assert_eq!(
            again.effects.last(),
            Some(&Effect::Ack {
                status: AckStatus::Rejected,
                reason: Some("duplicate".into())
            })
        );
    }

    #[test]
    fn real_mode_sms_sweep_measures_its_span_fixture_stays_zero() {
        let c = TickingClock {
            wall: T0,
            mono: std::cell::Cell::new(100),
            step: 3,
        };
        let s = sms_sweep(SmsSource::Reader, &c);
        assert_eq!(s.duration_ms, 3, "end - start on the mono clock");
        assert_eq!(s.finished_at, T0 + 103, "finishedAt = the wall at finish");
        assert_eq!(s.status, SmsReadStatus::Error);
        assert_eq!(s.error.as_deref(), Some(SMS_READER_UNAVAILABLE));
        // A clock that does not move: 0, never negative.
        let f = crate::clock::FixedClock::new(T0, 7);
        assert_eq!(sms_sweep(SmsSource::Reader, &f).duration_ms, 0);
        // sim/replay: fixtureSmsSweep's literal 0 (sms-modem.ts:686), untimed.
        let s = sms_sweep(SmsSource::Fixture, &c);
        assert_eq!(s.duration_ms, 0);
        assert_eq!(s.status, SmsReadStatus::Ok);
    }

    #[test]
    fn age_rounds_like_math_round() {
        assert_eq!(expired_age_text(T0 + 200_499, T0 as f64), "200");
        assert_eq!(expired_age_text(T0 + 200_500, T0 as f64), "201");
        assert_eq!(expired_age_text(T0 - 500, T0 as f64), "0");
    }

    #[test]
    fn rate_limited_refresh_is_claimed_and_leaves_the_mark() {
        let c = cmd(
            r#"{"v":1,"requestId":"r","command":"refresh","ts":1750000000000,"expiresAt":1750000060000}"#,
        );
        let d = decide_command(
            Some(&c),
            T0 + 1,
            &Ledger::empty(),
            RefreshWindow {
                now: 40_000,
                last: Some(20_000),
            },
            RunMode::Real,
            false,
        );
        assert!(matches!(d.effects[0], Effect::PersistLedger(_)));
        assert_eq!(d.refresh_mark, None);
        assert_eq!(
            d.effects.last(),
            Some(&Effect::Ack {
                status: AckStatus::Rejected,
                reason: Some("rate-limited".into())
            })
        );
        // Never refreshed: the first one passes even at mono 0.
        let d = decide_command(
            Some(&c),
            T0 + 1,
            &Ledger::empty(),
            RefreshWindow { now: 0, last: None },
            RunMode::Real,
            false,
        );
        assert_eq!(d.refresh_mark, Some(0));
    }
}
