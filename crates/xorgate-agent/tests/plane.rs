//! The MQTT plane through its real handlers (`worker.ts:1130-1325`) with a
//! fake transport, recording fakes and tokio's paused clock:
//! - the connect handler's ORDER (state → scope → hooks → `online:true` →
//!   subscribe config, command → `config.on_connect`) and the first tick at
//!   +interval (CF2);
//! - message dispatch + the `mqtt_in` / `conn` tape hooks;
//! - the scope fallback loop (`telemetry-scope.test.ts` badCycle) driven
//!   through connect / tick / close, with the event wording dumped by tsx from
//!   the real `TelemetryScope` + `noteScopeChange` (`fixtures/agent_plane/scope-plane.json`);
//! - the shutdown ORDER (`worker.ts:1291-1325`), also when the MQTT loop
//!   task dies (review-wp1 #10a: the same path, exit 1);
//! - `publish_acked` (PUBACK, timeout) and the tick's publish callback (the
//!   offline queue itself is the client's: `xorgate-cloud/tests/mqtt_offline.rs`);
//! - `read error: …` and the error/close path.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use serde::Deserialize;
use xorgate_agent::ctx::{AgentCtx, NoopHooks, PhaseHooks, ReportRequester, TelemetrySource};
use xorgate_agent::mqtt_plane::{AckOutcome, MqttHandle, MqttPlane, PublishTick};
use xorgate_agent::scope::ScopeHandle;
use xorgate_agent::shutdown::{
    on_mqtt_loop_ended, on_recording_fatal, Shutdown, LOOP_ENDED, RECORDING_FATAL,
};
use xorgate_cloud::mqtt::{ConnectionState as LinkState, Message, MqttEvent, QoS};
use xorgate_core::config_report::ReportOrigin;
use xorgate_core::telemetry_scope::TenantScope;

struct Rig {
    ctx: AgentCtx,
    link: Arc<FakeLink>,
    mqtt: MqttHandle,
    plane: MqttPlane,
    scope: ScopeHandle,
    config: Arc<FakeConfig>,
    source: Arc<FakeSource>,
    timeline: Timeline,
    reports: tokio::sync::mpsc::UnboundedReceiver<ReportOrigin>,
    tape: Arc<FakeTape>,
}

fn rig(tag: &str, device: &str) -> Rig {
    rig_with(tag, device, |t| Arc::new(FakeHooks(t)))
}

fn rig_with(tag: &str, device: &str, hooks: impl FnOnce(Timeline) -> Arc<dyn PhaseHooks>) -> Rig {
    let timeline: Timeline = Arc::new(Mutex::new(vec![]));
    let home = scratch(tag);
    let tape = Arc::new(FakeTape::default());
    let ctx = test_ctx(&home, device, hooks(timeline.clone()), tape.clone());
    let link = FakeLink::new(timeline.clone());
    let mqtt = MqttHandle::new(link.clone());
    let (requester, reports) = ReportRequester::channel();
    let scope = ScopeHandle::new(
        device,
        &ctx.env.config.topics.telemetry,
        ctx.state.clone(),
        requester,
    );
    let config = Arc::new(FakeConfig(timeline.clone()));
    let source = Arc::new(FakeSource {
        timeline: timeline.clone(),
        fail: Mutex::new(None),
    });
    let plane = MqttPlane::new(
        ctx.clone(),
        mqtt.clone(),
        scope.clone(),
        config.clone(),
        Arc::new(FakeCommands(timeline.clone())),
        source.clone(),
    );
    Rig {
        ctx,
        link,
        mqtt,
        plane,
        scope,
        config,
        source,
        timeline,
        reports,
        tape,
    }
}

/// [`FakeHooks`] whose `recording_stop` fails like a throwing telemetry
/// finalize (D29: the TS `recording.stop()` throws at `worker.ts:1306`).
struct FailingStopHooks(Timeline);
impl PhaseHooks for FailingStopHooks {
    fn notify_connectivity(&self, online: bool) {
        push(&self.0, format!("hooks.notify_connectivity {online}"));
    }
    fn led_stop(&self) {
        push(&self.0, "hooks.led_stop");
    }
    fn recording_stop(&self) -> Result<(), String> {
        push(&self.0, "hooks.recording_stop");
        Err("ENOSPC: no space left on device, write".into())
    }
    fn final_recording_status(&self) -> Option<xorgate_core::telemetry::RecordingStatus> {
        push(&self.0, "hooks.final_recording_status");
        None
    }
}

impl Rig {
    fn connect(&mut self) {
        self.link.set(LinkState::Connected);
        self.plane.on_event(MqttEvent::Connected {
            session_present: false,
        });
    }
    fn close(&mut self) {
        self.link.set(LinkState::Closed);
        self.plane.on_event(MqttEvent::Disconnected {
            reason: "Connection closed by peer abruptly".into(),
            emits_error: false,
        });
    }
    fn take_timeline(&self) -> Vec<String> {
        std::mem::take(&mut *self.timeline.lock().unwrap())
    }
}

async fn settle(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// `client.on("connect")` (`worker.ts:1145-1188`) in order, and the publish
/// timer's first fire at +interval (CF2), not at connect.
#[tokio::test(start_paused = true)]
async fn connect_handler_order_and_first_tick_at_interval() {
    let mut r = rig("connect", "dev-c");
    r.connect();
    assert_eq!(
        r.take_timeline(),
        [
            "hooks.notify_connectivity true",
            "publish status online:true",
            "subscribe dev-c/config",
            "subscribe dev-c/command",
            "config.on_connect",
        ]
    );
    let ev = state_events(&r.ctx);
    assert_eq!(ev, ["connected to ep"]);
    assert_eq!(r.ctx.state.snapshot().connection.as_str(), "connected");
    assert_eq!(r.tape.0.lock().unwrap().as_slice(), ["conn true"]);

    settle(999).await;
    assert!(r.take_timeline().is_empty(), "no tick before +1000 ms");
    settle(2).await;
    assert_eq!(r.take_timeline(), ["publish dev-c/telemetry"]);
    let p = r.link.publishes.lock().unwrap()[1].clone();
    assert_eq!(p.2, QoS::AtMostOnce);
    let v: serde_json::Value = serde_json::from_slice(&p.1).unwrap();
    assert_eq!(v["seq"], 0);
    assert_eq!(v["metrics"]["system"]["cpu_usage"]["value"], 12.5);
    assert_eq!(
        v["recording"],
        serde_json::json!({"enabled":false,"sessionId":null,"backlogBytes":0,"backlogSegments":0,"droppedSegments":0,"uploading":false,"lastUploadAt":null})
    );
    let snap = r.ctx.state.snapshot();
    assert_eq!(snap.published, 1);
    assert_eq!(snap.seq, 0);

    // A reconnect re-runs the handler but never starts a second timer.
    r.close();
    r.connect();
    r.take_timeline();
    settle(1000).await;
    assert_eq!(
        r.take_timeline()
            .iter()
            .filter(|l| l.contains("telemetry"))
            .count(),
        1
    );
}

/// `client.on("message")` (`worker.ts:1130-1143`) + the `mqtt_in` tape hook.
#[tokio::test(start_paused = true)]
async fn messages_are_dispatched_by_topic_and_taped() {
    let mut r = rig("msg", "dev-m");
    r.connect();
    r.take_timeline();
    for (topic, body) in [
        ("xorgate/devices/dev-m/config", "{\"rev\":3}"),
        ("xorgate/devices/dev-m/command", "{\"command\":\"refresh\"}"),
        ("xorgate/devices/other/config", "{}"),
    ] {
        r.plane.on_event(MqttEvent::Message(Message {
            topic: topic.into(),
            payload: body.as_bytes().to_vec(),
            retain: topic.ends_with("/config"),
            qos: QoS::AtLeastOnce,
        }));
    }
    assert_eq!(
        r.take_timeline(),
        [
            "config.on_message {\"rev\":3}",
            "commands.on_message {\"command\":\"refresh\"}",
        ]
    );
    let tape = r.tape.0.lock().unwrap();
    assert_eq!(tape.len(), 4); // conn + 3 inbound, the ignored one included
    assert_eq!(
        tape[1],
        "mqtt_in xorgate/devices/dev-m/config {\"rev\":3} retain=true qos=1"
    );
}

#[derive(Deserialize)]
struct ScopeEvent {
    msg: String,
}
#[derive(Deserialize)]
struct ScopeExpected {
    events: Vec<ScopeEvent>,
    #[serde(rename = "fallbackAfterCycles")]
    fallback_after_cycles: usize,
    #[serde(rename = "topicsAfterScope")]
    topics_after_scope: String,
}
#[derive(Deserialize)]
struct ScopeCase {
    expected: ScopeExpected,
}
#[derive(Deserialize)]
struct ScopeFixture {
    cases: Vec<ScopeCase>,
}

fn new_scope_msgs(ctx: &AgentCtx, seen: &mut Vec<(i64, String)>) {
    for e in ctx.state.snapshot().recent_events {
        if e.msg.starts_with("telemetry topic") && !seen.iter().any(|s| s.0 == e.ts && s.1 == e.msg)
        {
            seen.push((e.ts, e.msg));
        }
    }
}

/// The 2026-09-18 loop through the plane: `badCycle` (connect, one publish,
/// close 1.3 s later, every 3.3 s) until the fallback, then the timed
/// re-probe from the publish tick — event strings verbatim from the tsx dump.
#[tokio::test(start_paused = true)]
async fn scope_fallback_and_reprobe_through_the_plane() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/agent_plane/scope-plane.json"
    ))
    .unwrap();
    let f: ScopeFixture = serde_json::from_str(&text).unwrap();
    let want = &f.cases[0].expected;

    let mut r = rig("scope", "dev-scope");
    let mut seen = vec![];
    r.scope.apply_scope(
        TenantScope {
            organization_id: "org-1".into(),
            workspace_id: "ws-a".into(),
        },
        r.ctx.now_ms(),
    );
    assert_eq!(r.scope.topic(), want.topics_after_scope);
    new_scope_msgs(&r.ctx, &mut seen);

    let mut cycles = 0;
    while !r.scope.report().fallback.is_some() && cycles < 100 {
        r.connect();
        settle(1300).await; // the tick at +1000 publishes once on the scoped topic
        r.close();
        new_scope_msgs(&r.ctx, &mut seen);
        settle(2000).await;
        cycles += 1;
    }
    assert_eq!(cycles, want.fallback_after_cycles);
    assert!(r
        .link
        .topics()
        .iter()
        .any(|t| t.starts_with("xorgate/orgs/org-1/ws/ws-a/")));

    // Fallen back: the next publish goes to the unscoped topic; the re-probe
    // fires from the tick 30 min later.
    r.connect();
    settle(1000).await;
    assert_eq!(
        r.link.topics().last().unwrap(),
        "xorgate/devices/dev-scope/telemetry"
    );
    for _ in 0..31 {
        settle(60_000).await;
        new_scope_msgs(&r.ctx, &mut seen);
    }
    let got: Vec<&str> = seen.iter().map(|s| s.1.as_str()).collect();
    let exp: Vec<&str> = want.events.iter().map(|e| e.msg.as_str()).collect();
    assert_eq!(got, exp);
    assert!(r.link.topics().last().unwrap().starts_with("xorgate/orgs/"));

    // Every scope change asked for a `boot` report.
    let mut n = 0;
    while let Ok(o) = r.reports.try_recv() {
        assert_eq!(o, ReportOrigin::Boot);
        n += 1;
    }
    assert_eq!(n, 3);
}

/// `shutdown(sig)` (`worker.ts:1291-1325`) in order, then mqtt.js's `close`.
#[tokio::test(start_paused = true)]
async fn shutdown_order() {
    let mut r = rig("shutdown", "dev-s");
    std::fs::write(&r.ctx.paths.pid, "{}").unwrap();
    r.connect();
    r.take_timeline();
    let t2 = r.timeline.clone();
    let pid = r.ctx.paths.pid.clone();
    let ctx = r.ctx.clone();
    let source: Arc<dyn TelemetrySource> = r.source.clone();
    let code = Shutdown {
        ctx: &ctx,
        mqtt: &r.mqtt,
        plane: &mut r.plane,
        config: r.config.as_ref(),
        source: source.as_ref(),
        link_closes: true,
    }
    .run_with("SIGTERM", 0, |how| async move {
        push(
            &t2,
            format!("end {how:?} (pidfile present: {})", pid.exists()),
        );
        true
    })
    .await;
    assert_eq!(code, 0);
    // Graceful (review-wp1 X1): `online:false`, then the DISCONNECT end
    // (`MqttShutdown::shutdown`, which makes the broker discard the will).
    assert_eq!(
        r.take_timeline(),
        [
            "hooks.led_stop",
            "config.stop",
            "hooks.recording_stop",
            "hooks.final_recording_status",
            "source.stop",
            "publish status online:false",
            "end Graceful(2s) (pidfile present: false)",
            "hooks.notify_connectivity false",
        ]
    );
    let ev = state_events(&r.ctx);
    assert_eq!(
        &ev[ev.len() - 2..],
        ["shutting down (SIGTERM)", "connection closed"]
    );
    assert!(r.ctx.shutting_down());
    // The publish loop is stopped by the token: no tick after shutdown.
    settle(5000).await;
    assert!(r.take_timeline().is_empty());
}

/// review-wp1 #10a: the MQTT loop task ended on its own (a panic). The
/// worker runs `on_mqtt_loop_ended` and then the NORMAL shutdown with exit 1
/// and `link_closes: false` (`worker.rs`): pidfile removed, an
/// `online:false` attempt, no `close` (the stream is already gone), within
/// the 2 s budget.
#[tokio::test(start_paused = true)]
async fn loop_task_end_runs_the_shutdown_path_with_exit_1() {
    let mut r = rig("loop-ended", "dev-l");
    std::fs::write(&r.ctx.paths.pid, "{}").unwrap();
    r.connect();
    r.take_timeline();
    let ctx = r.ctx.clone();
    let source: Arc<dyn TelemetrySource> = r.source.clone();
    let code = on_mqtt_loop_ended(&ctx);
    assert_eq!(ctx.state.snapshot().last_error.as_deref(), Some(LOOP_ENDED));
    let started = tokio::time::Instant::now();
    let code = Shutdown {
        ctx: &ctx,
        mqtt: &r.mqtt,
        plane: &mut r.plane,
        config: r.config.as_ref(),
        source: source.as_ref(),
        link_closes: false,
    }
    .run(LOOP_ENDED, code, |_| async { true })
    .await;
    assert_eq!(code, 1);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!r.ctx.paths.pid.exists(), "pidfile removed");
    let tl = r.take_timeline();
    assert!(
        tl.contains(&"publish status online:false".to_string()),
        "{tl:?}"
    );
    assert!(
        !tl.contains(&"hooks.notify_connectivity false".to_string()),
        "{tl:?}"
    );
    assert_eq!(
        state_events(&r.ctx).last().unwrap(),
        "shutting down (mqtt event loop ended)"
    );
    assert!(r.ctx.shutting_down());
}

/// review-wp1 M1, 5 s poll path: the recording manager's fatal channel
/// carries a failed telemetry finalize. The TS crashes there (an uncaught
/// exception, exit 1). The worker logs `[agent] fatal: <msg>` and runs the
/// shutdown's local teardown with exit 1 so systemd restarts it and the boot
/// orphan finalize completes the segment. Like the TS crash (review-wp1 X1)
/// it sends NO `online:false` and ends the client with `Abort` (no
/// DISCONNECT), so the broker fires the Last-Will. The local `close` still
/// runs.
#[tokio::test(start_paused = true)]
async fn recording_fatal_runs_the_normal_shutdown_with_exit_1() {
    let mut r = rig("rec-fatal", "dev-f");
    std::fs::write(&r.ctx.paths.pid, "{}").unwrap();
    r.connect();
    r.take_timeline();
    let t2 = r.timeline.clone();
    let ctx = r.ctx.clone();
    let source: Arc<dyn TelemetrySource> = r.source.clone();
    let code = on_recording_fatal("ENOSPC: no space left on device, write");
    assert_eq!(code, 1);
    let code = Shutdown {
        ctx: &ctx,
        mqtt: &r.mqtt,
        plane: &mut r.plane,
        config: r.config.as_ref(),
        source: source.as_ref(),
        link_closes: true,
    }
    .run_with(RECORDING_FATAL, code, |how| {
        push(&t2, format!("end {how:?}"));
        async { true }
    })
    .await;
    assert_eq!(code, 1);
    assert!(!r.ctx.paths.pid.exists(), "pidfile removed");
    assert_eq!(
        r.take_timeline(),
        [
            "hooks.led_stop",
            "config.stop",
            "hooks.recording_stop",
            "hooks.final_recording_status",
            "source.stop",
            "end Abort",
            "hooks.notify_connectivity false",
        ]
    );
    let ev = state_events(&r.ctx);
    assert_eq!(
        &ev[ev.len() - 2..],
        [
            "shutting down (recording settings apply failed)",
            "connection closed"
        ]
    );
}

/// D29 + review-wp1 X1: a SIGTERM whose `recording.stop()` finalize throws.
/// The TS crashes at `worker.ts:1306`, before the `online:false` publish
/// (`:1317`), so the broker fires the Last-Will. Rust keeps the local
/// teardown and the signal path's exit code (0), but like the TS sends no
/// `online:false` and ends the client with `Abort` (no DISCONNECT). The rule
/// is keyed on the crash, not on the exit code.
#[tokio::test(start_paused = true)]
async fn a_failed_finalize_at_sigterm_leaves_the_offline_to_the_will() {
    let mut r = rig_with("d29-lwt", "dev-d", |t| Arc::new(FailingStopHooks(t)));
    std::fs::write(&r.ctx.paths.pid, "{}").unwrap();
    r.connect();
    r.take_timeline();
    let t2 = r.timeline.clone();
    let ctx = r.ctx.clone();
    let source: Arc<dyn TelemetrySource> = r.source.clone();
    let code = Shutdown {
        ctx: &ctx,
        mqtt: &r.mqtt,
        plane: &mut r.plane,
        config: r.config.as_ref(),
        source: source.as_ref(),
        link_closes: true,
    }
    .run_with("SIGTERM", 0, |how| {
        push(&t2, format!("end {how:?}"));
        async { true }
    })
    .await;
    assert_eq!(code, 0);
    assert!(!r.ctx.paths.pid.exists(), "pidfile removed");
    assert_eq!(
        r.take_timeline(),
        [
            "hooks.led_stop",
            "config.stop",
            "hooks.recording_stop",
            "hooks.final_recording_status",
            "source.stop",
            "end Abort",
            "hooks.notify_connectivity false",
        ]
    );
    let ev = state_events(&r.ctx);
    assert_eq!(
        &ev[ev.len() - 2..],
        ["shutting down (SIGTERM)", "connection closed"]
    );
}

/// `run` (the budget-only form) never calls `end` on a crash-equivalent exit:
/// no `online:false`, no graceful end.
#[tokio::test(start_paused = true)]
async fn run_skips_the_graceful_end_on_a_crash_equivalent_exit() {
    let mut r = rig("rec-fatal-run", "dev-g");
    r.connect();
    r.take_timeline();
    let t2 = r.timeline.clone();
    let ctx = r.ctx.clone();
    let source: Arc<dyn TelemetrySource> = r.source.clone();
    let code = Shutdown {
        ctx: &ctx,
        mqtt: &r.mqtt,
        plane: &mut r.plane,
        config: r.config.as_ref(),
        source: source.as_ref(),
        link_closes: true,
    }
    .run(RECORDING_FATAL, 1, |_| {
        push(&t2, "end graceful");
        async { true }
    })
    .await;
    assert_eq!(code, 1);
    let tl = r.take_timeline();
    assert!(!tl.iter().any(|l| l.starts_with("publish")), "{tl:?}");
    assert!(!tl.contains(&"end graceful".to_string()), "{tl:?}");
}

/// Shutting down while the link is down: no `online:false`, no `close`.
#[tokio::test(start_paused = true)]
async fn shutdown_while_disconnected() {
    let mut r = rig("shutdown-off", "dev-o");
    let ctx = r.ctx.clone();
    let source: Arc<dyn TelemetrySource> = r.source.clone();
    let code = Shutdown {
        ctx: &ctx,
        mqtt: &r.mqtt,
        plane: &mut r.plane,
        config: r.config.as_ref(),
        source: source.as_ref(),
        link_closes: true,
    }
    .run("SIGINT", 0, |_| async { true })
    .await;
    assert_eq!(code, 0);
    assert!(!r.take_timeline().iter().any(|l| l.starts_with("publish")));
    assert_eq!(
        state_events(&r.ctx).last().unwrap(),
        "shutting down (SIGINT)"
    );
}

/// `publishCommandAck` semantics (`worker.ts:953-974`).
#[tokio::test(start_paused = true)]
async fn publish_acked_resolves_on_puback_or_timeout() {
    let r = rig("acked", "dev-a");
    r.link.set(LinkState::Connected);
    let t = Duration::from_millis(3000);
    assert_eq!(
        r.mqtt
            .publish_acked("x/command/response", b"{}".to_vec(), t)
            .await,
        AckOutcome::Acked
    );

    *r.link.ack.lock().unwrap() = None; // the broker never PUBACKs
    let start = tokio::time::Instant::now();
    assert_eq!(
        r.mqtt
            .publish_acked("x/command/response", b"{}".to_vec(), t)
            .await,
        AckOutcome::TimedOut
    );
    assert_eq!(start.elapsed(), t);

    // The handle no longer gates on the link (that hid a `watch::Ref` across
    // an await — WP4): while disconnected the CLIENT stores and resends
    // (mqtt.js outgoingStore), so the call reaches the transport at once.
    *r.link.ack.lock().unwrap() = Some(AckOutcome::Acked);
    r.link.set(LinkState::Reconnecting);
    let before = r.link.topics().len();
    assert_eq!(
        r.mqtt
            .publish_acked("x/command/response", b"{}".to_vec(), t)
            .await,
        AckOutcome::Acked
    );
    assert_eq!(r.link.topics().len(), before + 1);
}

/// mqtt.js offline queue (`queueQoSZero`, review-wp1 I-b): a tick while
/// disconnected still hands its payload to the client, and `published` counts
/// it only when the client calls back — after the reconnect flush
/// (`worker.ts:1235-1245`). Phase 0 deviation #8 is gone.
#[tokio::test(start_paused = true)]
async fn publish_while_disconnected_is_queued_and_counted_on_flush() {
    let mut r = rig("queue", "dev-k");
    r.connect();
    r.close();
    r.take_timeline();
    *r.link.held.lock().unwrap() = Some(vec![]);
    settle(3500).await; // ticks at 1000, 2000, 3000
    assert_eq!(r.take_timeline(), ["publish dev-k/telemetry"; 3]);
    let snap = r.ctx.state.snapshot();
    assert_eq!(snap.published, 0);
    assert_eq!(snap.publish_errors, 0);
    assert_eq!(snap.latest.as_ref().map(|l| l.seq), Some(2));

    // The reconnect flush: the client writes the queue and calls back.
    r.connect();
    let held = r.link.held.lock().unwrap().take().unwrap();
    for (i, tx) in held.into_iter().enumerate() {
        let _ = tx.send(if i == 2 {
            Some(Err("boom".into()))
        } else {
            Some(Ok(()))
        });
    }
    settle(1).await;
    let snap = r.ctx.state.snapshot();
    assert_eq!(snap.published, 2);
    assert_eq!(snap.publish_errors, 1);
    assert_eq!(
        snap.recent_events.last().unwrap().msg,
        "publish error: boom"
    );
}

/// `client.on("error")` then `close` (`worker.ts:1264-1284`), and the
/// `reconnect` event (`:1256-1262`).
#[tokio::test(start_paused = true)]
async fn error_close_reconnect_events() {
    let mut r = rig("err", "dev-e");
    r.connect();
    r.link.set(LinkState::Closed);
    r.plane.on_event(MqttEvent::Disconnected {
        reason: "I/O: Connection reset by peer (os error 104)".into(),
        emits_error: true,
    });
    r.plane.on_event(MqttEvent::Reconnecting {
        attempt: 1,
        delay: Duration::from_secs(1),
    });
    let snap = r.ctx.state.snapshot();
    let ev: Vec<(&str, &str)> = snap
        .recent_events
        .iter()
        .map(|e| {
            (
                match e.level {
                    xorgate_core::telemetry::ActivityLevel::Info => "info",
                    xorgate_core::telemetry::ActivityLevel::Warn => "warn",
                    xorgate_core::telemetry::ActivityLevel::Error => "error",
                },
                e.msg.as_str(),
            )
        })
        .collect();
    assert_eq!(
        ev,
        [
            ("info", "connected to ep"),
            ("error", "I/O: Connection reset by peer (os error 104)"),
            ("warn", "connection closed"),
            ("warn", "reconnecting…"),
        ]
    );
    assert_eq!(
        snap.last_error.as_deref(),
        Some("I/O: Connection reset by peer (os error 104)")
    );
    assert_eq!(snap.connection.as_str(), "reconnecting");
    assert_eq!(
        r.tape.0.lock().unwrap().as_slice(),
        ["conn true", "conn false"]
    );
}

/// A failing `source.read()` → `read error: <msg>` (`worker.ts:1245-1250`),
/// no publish, seq not consumed.
#[tokio::test(start_paused = true)]
async fn read_error_sets_last_error() {
    let r = rig("readerr", "dev-r");
    r.link.set(LinkState::Connected);
    *r.source.fail.lock().unwrap() = Some("boom".into());
    let tick = PublishTick {
        ctx: r.ctx.clone(),
        mqtt: r.mqtt.clone(),
        scope: r.scope.clone(),
        source: r.source.clone(),
        seq: Default::default(),
        obd_last: Default::default(),
    };
    tick.run().await;
    let snap = r.ctx.state.snapshot();
    assert_eq!(snap.last_error.as_deref(), Some("read error: boom"));
    assert_eq!(snap.recent_events.last().unwrap().msg, "read error: boom");
    assert!(r.link.topics().is_empty());
    *r.source.fail.lock().unwrap() = None;
    tick.run().await;
    assert_eq!(r.ctx.state.snapshot().latest.unwrap().seq, 0);
}

/// `NoopHooks` keeps every Phase 3/4 call site inert.
#[test]
fn noop_hooks_are_inert() {
    use xorgate_agent::ctx::PhaseHooks;
    let h = NoopHooks;
    assert_eq!(
        h.recording_status(),
        xorgate_core::telemetry::RecordingStatus::all_off()
    );
    assert!(h.camera_mount_state().is_none());
    assert_eq!(h.report_hints(), Default::default());
    assert!(h.final_recording_status().is_none());
}

/// Phase 5b (`worker.ts:1374-1412` @ 09da38d): while the clock trust gate
/// HOLDS, a tick builds the payload (`seq += 1`, `setLatest`) and returns
/// before the publish — so there is no publish callback and state.json is
/// NOT re-flushed by the tick (the file keeps the earlier snapshot until
/// trust). A `[clock]` event during the hold still writes it (`onEvent` →
/// `store.event` + forced flush), as in the TS. Checked on the file's bytes
/// BEFORE any shutdown flush.
#[tokio::test(start_paused = true)]
async fn a_held_tick_advances_seq_but_does_not_write_state_json() {
    use xorgate_agent::ops::clock_stepper::agent_event_sink;
    use xorgate_agent::ops::clock_trust::{ClockTrust, ClockTrustOptions, TrustProbe};
    use xorgate_agent::ops::hooks::OpsHooks;
    use xorgate_agent::recording::RecordingHooks;

    let slot: Arc<std::sync::OnceLock<Arc<OpsHooks>>> = Default::default();
    let s2 = slot.clone();
    let r = rig_with("p5b-held-flush", "dev-h", move |_t| {
        let clock = Arc::new(xorgate_agent::ctx::AgentClock::following_tokio(
            1_800_000_000_000,
        ));
        let rec = Arc::new(RecordingHooks::new(None, None, Arc::new(|| None), clock));
        let ops = Arc::new(OpsHooks::new(rec));
        let _ = s2.set(ops.clone());
        ops as Arc<dyn PhaseHooks>
    });
    let ops = slot.get().unwrap().clone();
    // A probe that never syncs and throws on the 2nd call (a `[clock]` warn
    // event while still held).
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c2 = calls.clone();
    let probe: TrustProbe = Box::new(move || {
        let n = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if n == 1 {
                Err("timedatectl: boom".to_string())
            } else {
                Ok(xorgate_core::session::ProbedTimeSource::Unsynced)
            }
        })
    });
    let trust = ClockTrust::new(ClockTrustOptions::new(
        probe,
        r.ctx.clock.clone(),
        agent_event_sink(r.ctx.state.clone(), r.ctx.clock.clone()),
    ));
    ops.bind_clock_trust(trust.clone());
    r.link.set(LinkState::Connected);
    r.ctx.state.set_connection(
        xorgate_core::telemetry::ConnectionState::Connected,
        r.ctx.now_ms(),
    );

    let path = r.ctx.paths.state.clone();
    r.ctx.state.flush(r.ctx.now_ms(), true);
    let before = std::fs::read(&path).unwrap();
    let tick = PublishTick {
        ctx: r.ctx.clone(),
        mqtt: r.mqtt.clone(),
        scope: r.scope.clone(),
        source: r.source.clone(),
        seq: Default::default(),
        obd_last: Default::default(),
    };
    for _ in 0..3 {
        tokio::time::advance(Duration::from_millis(1_000)).await;
        tick.run().await;
    }
    // In memory: seq + latest advanced on every held tick; nothing published.
    let snap = r.ctx.state.snapshot();
    assert_eq!(snap.latest.as_ref().unwrap().seq, 2);
    assert_eq!(snap.seq, 2);
    assert_eq!(snap.published, 0);
    assert!(r.link.topics().is_empty());
    assert_eq!(trust.status().held, 3);
    // On disk: untouched by the held ticks.
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a held tick wrote state.json"
    );

    // A `[clock]` event during the hold flushes (with the advanced seq).
    trust.tick().await; // probe #1: unsynced → nothing
    assert_eq!(std::fs::read(&path).unwrap(), before);
    trust.tick().await; // probe #2 throws → warn event → forced flush
    assert!(!trust.trusted());
    let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(after["seq"], 2);
    assert_eq!(
        after["recentEvents"].as_array().unwrap().last().unwrap()["msg"],
        "[clock] trust probe failed: timedatectl: boom"
    );
}
