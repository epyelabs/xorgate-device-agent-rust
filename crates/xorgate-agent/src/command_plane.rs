//! The command plane (Phase 2 WP4): `handleCommandMessage`, `publishCommandAck`
//! and `runSmsRead` (`ts:src/worker.ts:902-1126`) as an interpreter of the
//! pure [`decide_command`] effects (`xorgate_core::command_flow`).
//!
//! **Concurrency (O11)** — the TS runs each message's handler synchronously
//! up to its first `await` (parse → expiry → claim → events → `lastRefreshAt`
//! → the ack's `client.publish`), so decisions, ledger writes and ack
//! publishes happen strictly in arrival order, while the waits (PUBACK,
//! `sudo`, the sms sweep) of several commands overlap. Here: ONE task owns
//! the ledger and the refresh mark and runs that synchronous prefix for each
//! message in turn, including the ENQUEUE of the first ack (its future is
//! polled once on the command task); everything after it is spawned.
//! `on_message` pushes into an unbounded queue (CF1: no message loss).
//!
//! **Decisions in force:** O2/CF9 the refresh window is monotonic
//! ([`RefreshWindow`] fed `ctx.clock.mono_ms()`); O3 `update` runs
//! `xorgate-updater-rs.service` ([`PrivilegedArgv::for_rust_agent`]); O4
//! real-mode `read_sms` answers the "sms reader not available in this build"
//! sweep; replay mode never starts (O7), so the container-exit branch is
//! unreachable and only logged.
//!
//! **`clear_sms` (Phase 5b WP3, `runSms` @ 09da38d `worker.ts:1027-1067`):**
//! the `Effect::ClearSms` interpreter. It runs only AFTER the ledger claim and
//! the accepted ack (core `decide_command`: a QoS-1 redelivery is a ledger
//! duplicate and never yields the effect, so it can never clear twice nor eat
//! messages that arrived since). Real mode → the process's one reader's
//! `clear()` (an accepted-command `ClearRequest`); sim → the fixture inbox, emptied
//! for the rest of the process (the `fixtureInboxCleared` module flag of the
//! TS, owned here). The reply carries `action:"clear"` and `cleared`.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use xorgate_core::clock::Clock;
use xorgate_core::command_flow::{
    decide_command, execution_outcome, ledger_write_failed, obd_forget_summary, obd_job_done_log,
    obd_scan_summary, read_sms_level, refresh_flag_failed_log, sms_sweep, ConsoleLevel, Effect,
    RefreshWindow, SmsSource as SmsDecision, AGENT_PREFIX,
};
use xorgate_core::commands::{
    is_expired, parse_command, AckStatus, CommandAck, DeviceCommand, DeviceCommandName, Ledger,
    ACK_FLUSH_TIMEOUT_MS,
};
use xorgate_core::config_report::ReportOrigin;
use std::sync::atomic::{AtomicBool, Ordering};
use xorgate_core::sms::{
    build_sms_payload, fixture_sms_clear, fixture_sms_read, sms_publish_failed_log,
    sms_result_line, SmsAction, SmsPayloadRequest, SmsSweep,
};
use xorgate_hal::io::{node_fd_message, node_fs_message};
use xorgate_hal::ledger_file;
use xorgate_hal::privileged::{PrivilegedArgv, PrivilegedRunner, SudoRunner};

use crate::ctx::{AgentCtx, BoxFut, CommandPlane, CommandPlaneDeps, ObdControl, ReportRequester};
use crate::mqtt_plane::{AckOutcome, MqttHandle, PublishFailure, QoS};
use crate::ops::sms::{ClearRequest, SmsReader};

/// Where a real-mode `read_sms` sweep comes from (I4; `worker.ts:917, 929-930`).
/// `sim` / `replay` always answer from `fixtureSmsSweep` (core
/// [`SmsDecision::Fixture`]); in `real` mode:
#[derive(Clone, Default)]
pub enum SmsSource {
    /// No reader wired: Phase 2's O4 sweep (`sms reader not available in
    /// this build`). Kept only until WP6 wires [`SmsSource::Real`] in the
    /// worker (CF48 route half).
    #[default]
    Unavailable,
    /// The process's one `SmsReader` (`const smsReader = new SmsReader(…)`,
    /// `worker.ts:917`): `await smsReader.read()`.
    Real(Arc<SmsReader>),
}

/// `ACK_FLUSH_TIMEOUT_MS` (`worker.ts:903`).
const ACK_FLUSH: Duration = Duration::from_millis(ACK_FLUSH_TIMEOUT_MS as u64);

type AckFut = Pin<Box<dyn Future<Output = AckOutcome> + Send>>;

/// The command plane (frozen signature): `sudo -n` through `PATH`.
pub fn start(ctx: &AgentCtx, deps: CommandPlaneDeps) -> Arc<dyn CommandPlane> {
    start_with(ctx, deps, Arc::new(SudoRunner))
}

/// [`start_with`] plus the real-mode sms source (I4; WP6 passes
/// `SmsSource::Real(reader)` from the worker).
pub fn start_with_sms(
    ctx: &AgentCtx,
    deps: CommandPlaneDeps,
    runner: Arc<dyn PrivilegedRunner>,
    sms: SmsSource,
) -> Arc<dyn CommandPlane> {
    start_inner(ctx, deps, runner, sms)
}

/// [`start`] with an injected privileged runner (tests: `RecordingRunner`).
pub fn start_with(
    ctx: &AgentCtx,
    deps: CommandPlaneDeps,
    runner: Arc<dyn PrivilegedRunner>,
) -> Arc<dyn CommandPlane> {
    start_inner(ctx, deps, runner, SmsSource::Unavailable)
}

fn start_inner(
    ctx: &AgentCtx,
    deps: CommandPlaneDeps,
    runner: Arc<dyn PrivilegedRunner>,
    sms: SmsSource,
) -> Arc<dyn CommandPlane> {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = CommandTask {
        fx: Effects {
            ctx: ctx.clone(),
            mqtt: deps.mqtt,
            reports: deps.reports,
            runner,
            sms,
            fixture_cleared: Arc::new(AtomicBool::new(false)),
            obd: deps.obd,
        },
        last_refresh: None,
    };
    ctx.spawn("commands", task.run(rx));
    Arc::new(Plane { tx })
}

struct Plane {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl CommandPlane for Plane {
    /// `client.on("message")` → `void handleCommandMessage(payload)`
    /// (`worker.ts:1135-1142`), queued in broker order.
    fn on_message(&self, payload: Vec<u8>) {
        let _ = self.tx.send(payload);
    }
}

/// The single owner of the ledger writes and `lastRefreshAt` (O11).
struct CommandTask {
    fx: Effects,
    /// `lastRefreshAt` (`worker.ts:909`) on the MONOTONIC clock (O2); `None`
    /// = never refreshed (the TS `0`).
    last_refresh: Option<i64>,
}

impl CommandTask {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
        let cancel = self.fx.ctx.cancel.clone();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                m = rx.recv() => match m {
                    Some(payload) => self.handle(payload).await,
                    None => break,
                },
            }
        }
    }

    /// `handleCommandMessage` (`worker.ts:988-1126`): the synchronous prefix
    /// here, the rest spawned after the first ack is enqueued.
    async fn handle(&mut self, payload: Vec<u8>) {
        let ctx = &self.fx.ctx;
        // `const now = Date.now(); const cmd = parseCommand(payload);` (`:989-990`)
        let now = ctx.now_ms();
        let parsed = parse_command(&payload);
        // `loadLedger` runs inside `claimCommand` (`commands.ts:209`), i.e.
        // only for a well-formed, unexpired command.
        let ledger = match &parsed {
            Some(c) if !is_expired(c, now) => ledger_file::load(&ctx.paths.commands),
            _ => Ledger::empty(),
        };
        let window = RefreshWindow {
            now: ctx.clock.mono_ms() as i64,
            last: self.last_refresh,
        };
        let d = decide_command(
            parsed.as_ref(),
            now,
            &ledger,
            window,
            ctx.env.config.mode,
            self.fx.obd.is_some(),
        );
        let mut effects: VecDeque<Effect> = d.effects.into();
        let mut mark = d.refresh_mark;
        // `obd_scan` / `obd_forget` (`worker.ts:1219` @ 09da38d): the job is
        // STARTED synchronously (its start event / goOff run now), before the
        // accepted ack is published — all before the next command is handled.
        let mut obd_job: Option<ObdJob> = None;

        // Synchronous prefix: ledger write, logs and events up to the first ack.
        while let Some(e) = effects.pop_front() {
            match e {
                Effect::PersistLedger(l) => {
                    if let Err(msg) = ledger_file::save(&ctx.paths.commands, &l) {
                        // `catch (err)` (`:1012-1020`): nothing below runs.
                        let cmd = parsed.as_ref().expect("a claim implies a command");
                        effects = ledger_write_failed(cmd, &msg).into();
                        mark = None;
                    }
                }
                Effect::Log(level, line) => log(level, &line),
                Effect::Event(level, msg) => ctx.state.note(level, msg, now),
                Effect::Ack { status, reason } => {
                    effects.push_front(Effect::Ack { status, reason });
                    break;
                }
                Effect::ObdJob(name) => obd_job = self.fx.start_obd_job(name),
                other => {
                    // Every other effect follows an ack in the TS order.
                    effects.push_front(other);
                    break;
                }
            }
        }
        if let Some(m) = mark {
            // `lastRefreshAt = now` (`:1051`), before the ack is awaited.
            self.last_refresh = Some(m);
        }
        let Some(cmd) = parsed else { return };
        if effects.is_empty() {
            return;
        }
        // Enqueue the first ack NOW (its `client.publish` runs synchronously
        // in the TS, `:962-967`), then await it and run the rest off-task.
        let first = match effects.front() {
            Some(Effect::Ack { status, reason }) => {
                let f = self.fx.ack_future(&cmd, *status, reason.clone());
                effects.pop_front();
                match poll_once(f).await {
                    Ok(outcome) => {
                        note_ack_outcome(&cmd, outcome);
                        None
                    }
                    Err(pending) => Some(pending),
                }
            }
            _ => None,
        };
        let fx = self.fx.clone();
        ctx.spawn("command", async move {
            if let Some(f) = first {
                note_ack_outcome(&cmd, f.await);
            }
            fx.run(&cmd, effects).await;
            if let Some(job) = obd_job {
                fx.finish_obd_job(&cmd, job).await;
            }
        });
    }
}

/// Everything the spawned half needs.
#[derive(Clone)]
struct Effects {
    ctx: AgentCtx,
    mqtt: MqttHandle,
    reports: ReportRequester,
    runner: Arc<dyn PrivilegedRunner>,
    /// The real-mode sweep source (I4).
    sms: SmsSource,
    /// `fixtureInboxCleared` (`sms-modem.ts:900` @ 09da38d): a sim-mode
    /// `clear_sms` empties the fixture inbox for the rest of this process.
    fixture_cleared: Arc<AtomicBool>,
    /// The OBD collector (Phase 5b I4); `None` = no `peripherals.obd`.
    obd: Option<Arc<dyn ObdControl>>,
}

/// A started `obd_scan` / `obd_forget` job (`worker.ts:1219` @ 09da38d).
enum ObdJob {
    Scan(BoxFut<'static, xorgate_core::obd::ObdScanResult>),
    Forget(BoxFut<'static, Option<String>>),
}

impl Effects {
    /// `publishCommandAck` (`worker.ts:953-974`): `{v:1, deviceId, ts, ...cmd,
    /// status, reason?}` QoS 1, resolved on the PUBACK or after 3 s.
    fn ack_future(&self, cmd: &DeviceCommand, status: AckStatus, reason: Option<String>) -> AckFut {
        let ack = CommandAck::new(&self.ctx.env.config.device_id, cmd, status, reason);
        let payload = serde_json::to_vec(&ack).unwrap_or_default();
        let topic = self.ctx.env.config.topics.command_response.clone();
        let mqtt = self.mqtt.clone();
        Box::pin(async move { mqtt.publish_acked(&topic, payload, ACK_FLUSH).await })
    }

    /// The effects after the first ack, in order (`worker.ts:1053-1125`,
    /// `runSmsRead` `:924-945`). Stamps are `Date.now()` at the time (`t`).
    async fn run(&self, cmd: &DeviceCommand, mut effects: VecDeque<Effect>) {
        while let Some(e) = effects.pop_front() {
            match e {
                Effect::Log(level, line) => log(level, &line),
                Effect::Event(level, msg) => self.ctx.state.note(level, msg, self.ctx.now_ms()),
                Effect::Ack { status, reason } => {
                    let outcome = self.ack_future(cmd, status, reason).await;
                    note_ack_outcome(cmd, outcome);
                }
                Effect::WriteRefreshFlag(content) => self.write_refresh_flag(&content),
                Effect::RequestReport => self.reports.request(ReportOrigin::Boot),
                Effect::ScheduleReport { after_ms } => self.schedule_report(after_ms),
                Effect::ReadSms {
                    source,
                    request_id,
                    limit,
                } => {
                    self.run_sms(SmsAction::Read, source, &request_id, limit)
                        .await
                }
                Effect::ClearSms {
                    source,
                    request_id,
                    limit,
                } => {
                    self.run_sms(SmsAction::Clear, source, &request_id, limit)
                        .await
                }
                Effect::ObdJob(_) => {
                    // Unreachable: started on the command task's synchronous prefix.
                }
                Effect::Execute(call) => {
                    // `executeCommand(cmd.command)` (`:1102`), O3 unit.
                    let res = self
                        .runner
                        .run(&PrivilegedArgv::for_rust_agent(&call))
                        .await;
                    for (i, e) in execution_outcome(cmd.command, &res).into_iter().enumerate() {
                        effects.insert(i, e);
                    }
                }
                Effect::ContainerExit { reason, code } => {
                    // Unreachable: MODE=replay refuses to start (O7).
                    warn!("[agent] {reason}: container exit {code} is not supported by the Rust agent (replay mode, O7)");
                }
                Effect::PersistLedger(_) => {
                    // Only ever first, handled on the command task.
                }
            }
        }
    }

    /// `source.obdScan?.()` / `source.obdForget?.()` (`worker.ts:1219` @
    /// 09da38d): calls the collector NOW (its synchronous part runs here).
    fn start_obd_job(&self, name: DeviceCommandName) -> Option<ObdJob> {
        let obd = self.obd.as_ref()?;
        Some(if name == DeviceCommandName::ObdScan {
            ObdJob::Scan(obd.scan())
        } else {
            ObdJob::Forget(obd.forget())
        })
    }

    /// `job.then(...)` (`worker.ts:1228-1239` @ 09da38d): the summary line,
    /// a forced flush and `configSync.request("boot")`.
    async fn finish_obd_job(&self, cmd: &DeviceCommand, job: ObdJob) {
        let summary = match job {
            ObdJob::Scan(f) => obd_scan_summary(f.await.candidates.len()),
            ObdJob::Forget(f) => obd_forget_summary(f.await.as_deref()),
        };
        info!(
            "{}",
            obd_job_done_log(cmd.command, &cmd.request_id, &summary)
        );
        self.ctx.state.flush(self.ctx.now_ms(), true);
        self.reports.request(ReportOrigin::Boot);
    }

    /// `writeFileSync(join(runtimeHome(), "refresh-modem.flag"), String(now))`
    /// (`worker.ts:1054-1061`); a failure only warns, with Node's message of
    /// the failing step: `…, open '<path>'` or the pathless `…, write`
    /// (review-wp4 #3).
    fn write_refresh_flag(&self, content: &str) {
        use std::io::Write;
        let path = &self.ctx.paths.refresh_flag;
        let res = match std::fs::File::create(path) {
            Ok(mut f) => f
                .write_all(content.as_bytes())
                .map_err(|e| node_fd_message(&e, "write")),
            Err(e) => Err(node_fs_message(&e, "open", path)),
        };
        if let Err(msg) = res {
            warn!("{}", refresh_flag_failed_log(&msg));
        }
    }

    /// `setTimeout(() => configSync.request("boot"), 45 s).unref()`
    /// (`worker.ts:1066`): dropped at shutdown, never holds it up.
    fn schedule_report(&self, after_ms: i64) {
        let reports = self.reports.clone();
        let cancel = self.ctx.cancel.clone();
        let after = Duration::from_millis(after_ms.max(0) as u64);
        self.ctx.spawn("refresh-report", async move {
            tokio::select! {
                _ = tokio::time::sleep(after) => reports.request(ReportOrigin::Boot),
                _ = cancel.cancelled() => {}
            }
        });
    }

    /// `runSms` (`worker.ts:1027-1067` @ 09da38d): sweep or clear (sim: the
    /// fixture inbox), log + event, publish QoS 1 on `…/sms` once (mqtt.js-
    /// style offline queueing lives in the client, `xorgate_cloud::mqtt` — no
    /// retry here). Logs counts/status/duration only (contract §6; the
    /// `cleared` counts are fine per clear §6).
    async fn run_sms(&self, action: SmsAction, source: SmsDecision, request_id: &str, limit: u32) {
        let command = match action {
            SmsAction::Read => DeviceCommandName::ReadSms,
            SmsAction::Clear => DeviceCommandName::ClearSms,
        }
        .to_string();
        let sweep: SmsSweep = match (action, source, &self.sms) {
            // `real ? await smsReader.clear() : fixtureSmsClear(Date.now())`
            (SmsAction::Clear, SmsDecision::Reader, SmsSource::Real(reader)) => (*reader
                .clear(ClearRequest::AcceptedCommand {
                    request_id: request_id.to_string(),
                })
                .await)
                .clone(),
            (SmsAction::Clear, SmsDecision::Fixture, _) => {
                let mut cleared = self.fixture_cleared.load(Ordering::SeqCst);
                let s = fixture_sms_clear(self.ctx.now_ms(), &mut cleared);
                self.fixture_cleared.store(cleared, Ordering::SeqCst);
                s
            }
            // `real ? await smsReader.read() : fixtureSmsRead(Date.now())`
            (SmsAction::Read, SmsDecision::Reader, SmsSource::Real(reader)) => {
                (*reader.read().await).clone()
            }
            (SmsAction::Read, SmsDecision::Fixture, _) => fixture_sms_read(
                self.ctx.now_ms(),
                self.fixture_cleared.load(Ordering::SeqCst),
            ),
            // No reader wired (tests only; the worker always wires one in
            // real mode): the O4 sweep, `cleared: {}` on a clear.
            (action, source, SmsSource::Unavailable) => {
                let mut s = sms_sweep(source, &*self.ctx.clock);
                if action == SmsAction::Clear {
                    s.cleared = Some(Default::default());
                }
                s
            }
        };
        let payload = build_sms_payload(
            &sweep,
            &SmsPayloadRequest {
                device_id: self.ctx.env.config.device_id.clone(),
                request_id: request_id.to_string(),
                limit,
                action,
            },
        );
        let line = sms_result_line(&command, request_id, &payload);
        info!("{AGENT_PREFIX}{line}");
        let t = self.ctx.now_ms();
        self.ctx.state.note(read_sms_level(&payload), line, t);
        let body = serde_json::to_vec(&payload).unwrap_or_default();
        let topic = &self.ctx.env.config.topics.sms;
        if let Err(e) = self.mqtt.publish(topic, body, QoS::AtLeastOnce, false) {
            let msg = match e {
                PublishFailure::NotConnected => "not connected".to_string(),
                PublishFailure::Failed(m) => m,
            };
            warn!("{}", sms_publish_failed_log(&command, request_id, &msg));
        }
    }
}

/// The TS `done` callback ignores the publish error (`worker.ts:961-967`);
/// keep a debug trail only.
fn note_ack_outcome(cmd: &DeviceCommand, outcome: AckOutcome) {
    match outcome {
        AckOutcome::Acked => {}
        AckOutcome::TimedOut => debug!(
            "{AGENT_PREFIX}{} ack for {}: no PUBACK within {} ms",
            cmd.command, cmd.request_id, ACK_FLUSH_TIMEOUT_MS
        ),
        AckOutcome::Failed(m) => debug!(
            "{AGENT_PREFIX}{} ack for {}: publish failed: {m}",
            cmd.command, cmd.request_id
        ),
    }
}

/// `console.{log,warn,error}` → the journal priority.
fn log(level: ConsoleLevel, line: &str) {
    match level {
        ConsoleLevel::Info => info!("{line}"),
        ConsoleLevel::Warn => warn!("{line}"),
        ConsoleLevel::Error => error!("{line}"),
    }
}

/// Poll `f` once on the current task so its synchronous part (the ack's
/// enqueue) runs now, in arrival order: `Ok` if it already finished, else
/// the pending future to await elsewhere.
async fn poll_once(mut f: AckFut) -> Result<AckOutcome, AckFut> {
    let r = std::future::poll_fn(|cx| Poll::Ready(f.as_mut().poll(cx))).await;
    match r {
        Poll::Ready(o) => Ok(o),
        Poll::Pending => Err(f),
    }
}
