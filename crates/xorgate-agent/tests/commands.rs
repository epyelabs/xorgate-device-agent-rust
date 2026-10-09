//! The command plane end to end (Phase 2 WP4) on paused tokio time, through
//! a fake MQTT link and a `RecordingRunner` (no sudo is ever run):
//! - the tsx trace fixture of the REAL `worker.ts:902-1126`
//!   (`fixtures/command_flow/handle-command.json`) replayed through the
//!   running plane — acks / sms bytes, sudo argv (O3 unit), state.json
//!   events, report requests, the refresh flag and `commands.json` per step,
//!   and the ack-vs-exec order;
//! - a PUBACK that never comes: the ack resolves at 3 s, execution proceeds;
//! - a ledger write failure on disk;
//! - acks enqueued in arrival order while an earlier PUBACK is pending.
//!
//! Queueing note (review-wp1 I-b): every test here runs with the link UP,
//! so none depends on `MqttHandle` offline queueing (WP1 follow-up).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde::Deserialize;
use tokio::sync::mpsc;
use xorgate_agent::command_plane::start_with;
use xorgate_agent::ctx::{
    AgentClock, AgentCtx, CommandPlane, CommandPlaneDeps, NoopHooks, ReportRequester,
};
use xorgate_agent::env::AgentEnv;
use xorgate_agent::mqtt_plane::{MqttHandle, QoS};
use xorgate_cloud::mqtt::ConnectionState as LinkState;
use xorgate_core::commands::RunResult;
use xorgate_core::config_report::ReportOrigin;
use xorgate_core::telemetry::RunMode;
use xorgate_hal::io::null_tape;
use xorgate_hal::privileged::RecordingRunner;

const T0: i64 = 1_750_000_000_000;

#[derive(Deserialize)]
struct Step {
    at: i64,
    payload: String,
    #[serde(default)]
    exec: Option<RunResult>,
    #[serde(default, rename = "ledgerError")]
    ledger_error: Option<String>,
    #[serde(default)]
    puback: Option<bool>,
    trace: Vec<String>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    mode: String,
    #[serde(default, rename = "ledgerText")]
    ledger_text: Option<String>,
    steps: Vec<Step>,
    after: Vec<String>,
    #[serde(rename = "ledgerFinal")]
    ledger_final: Option<String>,
}

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

struct Rig {
    ctx: AgentCtx,
    link: Arc<FakeLink>,
    runner: Arc<RecordingRunner>,
    plane: Arc<dyn CommandPlane>,
    reports: mpsc::UnboundedReceiver<ReportOrigin>,
    timeline: Timeline,
}

fn rig(tag: &str, device: &str, mode: RunMode, wall0: i64) -> Rig {
    let home = scratch(tag);
    let mut ctx = test_ctx(&home, device, Arc::new(NoopHooks), null_tape());
    let mut env: AgentEnv = (*ctx.env).clone();
    env.config.mode = mode;
    ctx.env = Arc::new(env);
    ctx.clock = Arc::new(AgentClock::following_tokio(wall0));
    let timeline: Timeline = Default::default();
    let link = FakeLink::new(timeline.clone());
    link.set(LinkState::Connected);
    let runner = Arc::new(RecordingRunner::with_timeline(timeline.clone()));
    let (reports, rx) = ReportRequester::channel();
    let plane = start_with(
        &ctx,
        CommandPlaneDeps {
            mqtt: MqttHandle::new(link.clone()),
            reports,
        },
        runner.clone(),
    );
    Rig {
        ctx,
        link,
        runner,
        plane,
        reports: rx,
        timeline,
    }
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

/// Advance paused time in 1 ms-resolution chunks small enough that every
/// timer in between fires at its own instant.
async fn advance_to(ctx: &AgentCtx, wall: i64) {
    let now = ctx.now_ms();
    assert!(wall >= now, "clock already past {wall} ({now})");
    tokio::time::advance(Duration::from_millis((wall - now) as u64)).await;
    settle().await;
}

fn drain(rx: &mut mpsc::UnboundedReceiver<ReportOrigin>) -> usize {
    let mut n = 0;
    while let Ok(o) = rx.try_recv() {
        assert_eq!(o, ReportOrigin::Boot);
        n += 1;
    }
    n
}

/// `{level} {msg}` of every state.json event, oldest first.
fn events(ctx: &AgentCtx) -> Vec<String> {
    ctx.state
        .snapshot()
        .recent_events
        .iter()
        .map(|e| {
            let v = serde_json::to_value(e).unwrap();
            format!("{} {}", v["level"].as_str().unwrap(), e.msg)
        })
        .collect()
}

fn publishes(link: &FakeLink) -> Vec<(String, String)> {
    link.publishes
        .lock()
        .unwrap()
        .iter()
        .map(|(t, p, q)| {
            assert_eq!(*q, QoS::AtLeastOnce);
            let kind = t
                .rsplit_once("/devices/")
                .unwrap()
                .1
                .split_once('/')
                .unwrap()
                .1;
            (kind.to_string(), String::from_utf8(p.clone()).unwrap())
        })
        .collect()
}

/// The fixture's `exec` line as the Rust agent runs it (O3 unit).
fn o3(line: &str) -> String {
    line.replace("xorgate-updater.service", "xorgate-updater-rs.service")
}

#[tokio::test(start_paused = true)]
async fn handle_command_trace_fixture_through_the_running_plane() {
    let text = std::fs::read_to_string(format!(
        "{}/../../fixtures/command_flow/handle-command.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let f: Fixture = serde_json::from_str(&text).unwrap();
    let mut checked = 0;
    for (ci, c) in f.cases.iter().enumerate() {
        let mode = RunMode::parse(&c.mode).unwrap();
        // Replay never starts in Rust (O7); ledger-error steps inject a
        // message no real filesystem produces (covered below + in core).
        if mode == RunMode::Replay || c.steps.iter().any(|s| s.ledger_error.is_some()) {
            continue;
        }
        let mut r = rig(&format!("cf{ci}"), "dev-1", mode, T0);
        if let Some(t) = &c.ledger_text {
            std::fs::write(&r.ctx.paths.commands, t).unwrap();
        }
        for (si, s) in c.steps.iter().enumerate() {
            let what = format!("{} — step {si}", c.name);
            let ev0 = events(&r.ctx).len();
            let pub0 = publishes(&r.link).len();
            let tl0 = r.timeline.lock().unwrap().len();
            if let Some(res) = &s.exec {
                r.runner.answer(res.clone());
            }
            if s.puback == Some(false) {
                *r.link.ack.lock().unwrap() = None;
            }
            advance_to(&r.ctx, s.at).await;
            r.plane.on_message(s.payload.as_bytes().to_vec());
            settle().await;
            if s.puback == Some(false) {
                // Two un-PUBACKed acks (accepted, then failed): 3 s each.
                for _ in 0..2 {
                    tokio::time::advance(Duration::from_millis(3000)).await;
                    settle().await;
                }
            }
            *r.link.ack.lock().unwrap() = Some(xorgate_agent::mqtt_plane::AckOutcome::Acked);

            // Wire bytes, in order.
            let want_pubs: Vec<(String, String)> = s
                .trace
                .iter()
                .filter_map(|l| {
                    l.strip_prefix("ack ")
                        .map(|p| ("command/response".to_string(), p.to_string()))
                        .or_else(|| {
                            l.strip_prefix("sms ")
                                .map(|p| ("sms".to_string(), p.to_string()))
                        })
                })
                .collect();
            assert_eq!(
                publishes(&r.link)[pub0..],
                want_pubs[..],
                "{what}: publishes"
            );
            // Publish/exec interleaving.
            let want_order: Vec<String> = s
                .trace
                .iter()
                .filter_map(|l| {
                    if l.starts_with("ack ") {
                        Some("publish_acked dev-1/command/response".to_string())
                    } else if l.starts_with("sms ") {
                        Some("publish dev-1/sms".to_string())
                    } else {
                        l.starts_with("exec ").then(|| o3(l))
                    }
                })
                .collect();
            assert_eq!(
                r.timeline.lock().unwrap()[tl0..],
                want_order[..],
                "{what}: order"
            );
            // state.json events (level + msg), in order.
            let want_ev: Vec<String> = s
                .trace
                .iter()
                .filter_map(|l| l.strip_prefix("event ").map(str::to_string))
                .collect();
            assert_eq!(events(&r.ctx)[ev0..], want_ev[..], "{what}: events");
            // configSync.request("boot") count (incl. a +45 s timer that fired before this step).
            let want_reports = s.trace.iter().filter(|l| *l == "report boot").count();
            assert_eq!(drain(&mut r.reports), want_reports, "{what}: reports");
            // refresh flag
            if let Some(flag) = s
                .trace
                .iter()
                .find_map(|l| l.strip_prefix("flag refresh-modem.flag "))
            {
                assert_eq!(
                    std::fs::read_to_string(&r.ctx.paths.refresh_flag).unwrap(),
                    flag,
                    "{what}: flag"
                );
            }
            // commands.json after the claim
            if let Some(l) = s.trace.iter().find_map(|l| l.strip_prefix("ledger ")) {
                assert_eq!(
                    std::fs::read_to_string(&r.ctx.paths.commands).unwrap(),
                    l,
                    "{what}: ledger"
                );
            }
            checked += 1;
        }
        // Pending +45 s report timers.
        tokio::time::advance(Duration::from_millis(45_000)).await;
        settle().await;
        let want_after = c.after.iter().filter(|l| *l == "report boot").count();
        assert_eq!(drain(&mut r.reports), want_after, "{}: after", c.name);
        let file = std::fs::read_to_string(&r.ctx.paths.commands).ok();
        assert_eq!(file, c.ledger_final, "{}: ledgerFinal", c.name);
        r.ctx.cancel.cancel();
    }
    assert_eq!(
        checked, 31,
        "steps replayed (replay-mode + ledger-error cases excluded)"
    );
}

#[tokio::test(start_paused = true)]
async fn ack_without_puback_resolves_at_3s_then_executes() {
    let r = rig("puback", "dev-1", RunMode::Real, T0);
    *r.link.ack.lock().unwrap() = None;
    r.plane.on_message(
        br#"{"v":1,"requestId":"q","command":"reboot","ts":1750000000000,"expiresAt":1750000060000}"#.to_vec(),
    );
    settle().await;
    assert_eq!(
        *r.timeline.lock().unwrap(),
        vec!["publish_acked dev-1/command/response"]
    );
    tokio::time::advance(Duration::from_millis(2999)).await;
    settle().await;
    assert!(r.runner.calls().is_empty(), "still waiting for the PUBACK");
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(
        r.runner.calls(),
        vec![vec!["sudo", "-n", "systemctl", "reboot"]]
    );
    // claimed before anything was published (reboot does not return)
    assert!(std::fs::read_to_string(&r.ctx.paths.commands)
        .unwrap()
        .contains(r#""requestId":"q""#));
    r.ctx.cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn ledger_write_failure_refuses_without_executing() {
    let r = rig("ledgerfail", "dev-1", RunMode::Real, T0);
    // commands.json is a non-empty directory: the claim's rename fails.
    std::fs::create_dir_all(r.ctx.paths.commands.join("x")).unwrap();
    r.plane.on_message(
        br#"{"v":1,"requestId":"q","command":"reboot","ts":1750000000000,"expiresAt":1750000060000}"#.to_vec(),
    );
    settle().await;
    assert!(r.runner.calls().is_empty());
    let pubs = publishes(&r.link);
    assert_eq!(pubs.len(), 1);
    let ack: serde_json::Value = serde_json::from_str(&pubs[0].1).unwrap();
    assert_eq!(ack["status"], "failed");
    let reason = ack["reason"].as_str().unwrap();
    assert!(
        reason
            .starts_with("ledger write failed: EISDIR: illegal operation on a directory, rename '"),
        "{reason}"
    );
    assert_eq!(
        events(&r.ctx),
        vec![format!("warn refused reboot: {reason}")],
        "no `accepted` event"
    );
    r.ctx.cancel.cancel();
}

#[tokio::test(start_paused = true)]
async fn acks_are_enqueued_in_arrival_order_while_a_puback_is_pending() {
    let r = rig("order", "dev-1", RunMode::Real, T0 + 1000);
    *r.link.ack.lock().unwrap() = None;
    let reboot =
        br#"{"v":1,"requestId":"a","command":"power_off","ts":1750000000000,"expiresAt":1750000060000}"#;
    let refresh =
        br#"{"v":1,"requestId":"b","command":"refresh","ts":1750000000000,"expiresAt":1750000060000}"#;
    r.plane.on_message(reboot.to_vec());
    r.plane.on_message(refresh.to_vec());
    r.plane.on_message(reboot.to_vec()); // duplicate of "a", decided while a's PUBACK is pending
    settle().await;
    let ids: Vec<(String, String)> = publishes(&r.link)
        .iter()
        .map(|(_, p)| {
            let v: serde_json::Value = serde_json::from_str(p).unwrap();
            (
                v["requestId"].as_str().unwrap().into(),
                v["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            ("a".into(), "accepted".into()),
            ("b".into(), "accepted".into()),
            ("a".into(), "rejected".into()),
        ]
    );
    assert!(r.runner.calls().is_empty(), "power_off waits for its ack");
    tokio::time::advance(Duration::from_millis(3000)).await;
    settle().await;
    assert_eq!(
        r.runner.calls(),
        vec![vec!["sudo", "-n", "systemctl", "poweroff"]]
    );
    r.ctx.cancel.cancel();
}
