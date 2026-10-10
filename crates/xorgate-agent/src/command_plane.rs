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
    decide_command, execution_outcome, ledger_write_failed, read_sms_level, read_sms_line,
    read_sms_publish_failed_log, refresh_flag_failed_log, sms_sweep, ConsoleLevel, Effect,
    RefreshWindow, SmsSource, AGENT_PREFIX,
};
use xorgate_core::commands::{
    is_expired, parse_command, AckStatus, CommandAck, DeviceCommand, Ledger, ACK_FLUSH_TIMEOUT_MS,
};
use xorgate_core::config_report::ReportOrigin;
use xorgate_core::sms::{build_sms_payload, SmsPayloadRequest};
use xorgate_hal::io::{node_fd_message, node_fs_message};
use xorgate_hal::ledger_file;
use xorgate_hal::privileged::{PrivilegedArgv, PrivilegedRunner, SudoRunner};

use crate::ctx::{AgentCtx, CommandPlane, CommandPlaneDeps, ReportRequester};
use crate::mqtt_plane::{AckOutcome, MqttHandle, PublishFailure, QoS};

/// `ACK_FLUSH_TIMEOUT_MS` (`worker.ts:903`).
const ACK_FLUSH: Duration = Duration::from_millis(ACK_FLUSH_TIMEOUT_MS as u64);

type AckFut = Pin<Box<dyn Future<Output = AckOutcome> + Send>>;

/// The command plane (frozen signature): `sudo -n` through `PATH`.
pub fn start(ctx: &AgentCtx, deps: CommandPlaneDeps) -> Arc<dyn CommandPlane> {
    start_with(ctx, deps, Arc::new(SudoRunner))
}

/// [`start`] with an injected privileged runner (tests: `RecordingRunner`).
pub fn start_with(
    ctx: &AgentCtx,
    deps: CommandPlaneDeps,
    runner: Arc<dyn PrivilegedRunner>,
) -> Arc<dyn CommandPlane> {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = CommandTask {
        fx: Effects {
            ctx: ctx.clone(),
            mqtt: deps.mqtt,
            reports: deps.reports,
            runner,
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
        let d = decide_command(parsed.as_ref(), now, &ledger, window, ctx.env.config.mode);
        let mut effects: VecDeque<Effect> = d.effects.into();
        let mut mark = d.refresh_mark;

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
                } => self.run_sms_read(source, &request_id, limit),
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

    /// `runSmsRead` (`worker.ts:924-945`): sweep (O4 / fixture), log + event,
    /// publish QoS 1 on `…/sms` once (mqtt.js-style offline queueing lives
    /// in the client, `xorgate_cloud::mqtt`, WP1 follow-up — no retry here;
    /// the `NotConnected` arm below is unreachable since then).
    fn run_sms_read(&self, source: SmsSource, request_id: &str, limit: u32) {
        let sweep = sms_sweep(source, self.ctx.now_ms());
        let payload = build_sms_payload(
            &sweep,
            &SmsPayloadRequest {
                device_id: self.ctx.env.config.device_id.clone(),
                request_id: request_id.to_string(),
                limit,
            },
        );
        let line = read_sms_line(request_id, &payload);
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
            warn!("{}", read_sms_publish_failed_log(request_id, &msg));
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
