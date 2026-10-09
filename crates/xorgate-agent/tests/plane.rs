//! The MQTT plane through its real handlers (`worker.ts:1130-1325`) with a
//! fake transport, recording fakes and tokio's paused clock:
//! - the connect handler's ORDER (state → scope → hooks → `online:true` →
//!   subscribe config, command → `config.on_connect`) and the first tick at
//!   +interval (CF2);
//! - message dispatch + the `mqtt_in` / `conn` tape hooks;
//! - the scope fallback loop (`telemetry-scope.test.ts` badCycle) driven
//!   through connect / tick / close, with the event wording dumped by tsx from
//!   the real `TelemetryScope` + `noteScopeChange` (`fixtures/agent_plane/scope-plane.json`);
//! - the shutdown ORDER (`worker.ts:1291-1325`);
//! - `publish_acked` (PUBACK, timeout) and the tick's publish callback (the
//!   offline queue itself is the client's: `xorgate-cloud/tests/mqtt_offline.rs`);
//! - `read error: …` and the error/close path.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use serde::Deserialize;
use xorgate_agent::ctx::{AgentCtx, NoopHooks, ReportRequester, TelemetrySource};
use xorgate_agent::mqtt_plane::{AckOutcome, MqttHandle, MqttPlane, PublishTick};
use xorgate_agent::scope::ScopeHandle;
use xorgate_agent::shutdown::Shutdown;
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
    let timeline: Timeline = Arc::new(Mutex::new(vec![]));
    let home = scratch(tag);
    let tape = Arc::new(FakeTape::default());
    let ctx = test_ctx(
        &home,
        device,
        Arc::new(FakeHooks(timeline.clone())),
        tape.clone(),
    );
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
    .run("SIGTERM", 0, |budget| async move {
        assert_eq!(budget, Duration::from_millis(2000));
        push(&t2, format!("end (pidfile present: {})", pid.exists()));
        true
    })
    .await;
    assert_eq!(code, 0);
    assert_eq!(
        r.take_timeline(),
        [
            "hooks.led_stop",
            "config.stop",
            "hooks.recording_stop",
            "source.stop",
            "publish status online:false",
            "end (pidfile present: false)",
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
}
