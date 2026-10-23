//! The MQTT plane: the publishing handle every subsystem shares
//! ([`MqttHandle`], frozen for wave B) and the worker's MQTT event handlers
//! (`worker.ts:1130-1284`): message dispatch, the connect handler (state,
//! scope cycle, `online:true`, subscribes, `boot` report + heartbeat, the
//! 1 Hz publish loop), reconnect / error / close, and the publish tick itself.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tracing::{error, info};
use xorgate_cloud::mqtt::{ConnectionState as LinkState, MqttEvent, MqttPublisher};
use xorgate_core::telemetry::{ActivityLevel, ConnectionState, StatusPayload, TelemetryPayload};

pub use xorgate_cloud::mqtt::{AckOutcome, QoS};

use crate::ctx::{AgentCtx, BoxFut, CommandPlane, ConfigPlane, TelemetrySource};
use crate::scope::ScopeHandle;
use crate::timer::SetInterval;

/// Why a fire-and-forget publish did not go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishFailure {
    /// Kept for the frozen wave-B enum; no longer returned. Since the WP1
    /// follow-up a publish made while disconnected is QUEUED by the client and
    /// sent after reconnect, exactly like mqtt.js's offline queue
    /// (`client.js:1068-1096`; review-wp1 I-b) — Phase 0 deviation #8 is gone.
    NotConnected,
    /// The client refused it (request channel full / loop gone / offline
    /// queue disabled); the text is the client's error message.
    Failed(String),
}

/// The outcome of a publish with a callback (`client.publish(…, cb)`):
/// `Some(Ok(()))` = sent (QoS 0 written / QoS 1 PUBACKed), `Some(Err(msg))` =
/// the callback's error, `None` = the callback never fires (a QoS 0 publish
/// discarded by a link drop or a full offline queue, or the client gone).
pub type PublishDone = Option<Result<(), String>>;

/// The transport under [`MqttHandle`]: the real client, the `--offline`
/// stub, or a test fake.
pub trait MqttTransport: Send + Sync + 'static {
    /// Queue a publish (never blocks).
    fn publish(&self, topic: &str, payload: Vec<u8>, qos: QoS, retain: bool) -> Result<(), String>;
    /// QoS 1 publish resolved on its PUBACK or `timeout`.
    fn publish_acked(
        &self,
        topic: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> BoxFut<'static, AckOutcome>;
    /// Queue a SUBSCRIBE.
    fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), String>;
    /// `client.publish(topic, payload, { qos, retain }, callback)`: queued NOW
    /// (call order = wire order), the future is the callback. The default — for
    /// fakes and the `--offline` stub, whose callback fires at once
    /// (`mqtt.ts:91-94`) — is [`publish`](Self::publish) resolved immediately.
    fn publish_notify(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> BoxFut<'static, PublishDone> {
        let r = self.publish(topic, payload, qos, retain);
        Box::pin(async move { Some(r) })
    }
    /// The link state.
    fn connection_watch(&self) -> watch::Receiver<LinkState>;
}

/// Cloneable publishing handle (frozen for wave B).
#[derive(Clone)]
pub struct MqttHandle {
    t: Arc<dyn MqttTransport>,
}

impl MqttHandle {
    pub fn new(t: Arc<dyn MqttTransport>) -> Self {
        Self { t }
    }

    /// True while the link is up (CONNACK received, not yet closed).
    pub fn connected(&self) -> bool {
        *self.t.connection_watch().borrow() == LinkState::Connected
    }

    /// Watch the link state (`connecting | connected | reconnecting | closed`).
    pub fn connection_watch(&self) -> watch::Receiver<LinkState> {
        self.t.connection_watch()
    }

    /// `client.publish(topic, payload, { qos, retain })` without a callback
    /// (reports, sms results, status). While disconnected the client queues it
    /// and sends it after reconnect (mqtt.js offline queue, review-wp1 I-b).
    pub fn publish(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> Result<(), PublishFailure> {
        self.t
            .publish(topic, payload, qos, retain)
            .map_err(PublishFailure::Failed)
    }

    /// `client.publish(…, callback)`: queued now, the returned future is the
    /// callback (see [`PublishDone`]). The future is `Send` and `'static`.
    pub fn publish_notify(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> BoxFut<'static, PublishDone> {
        self.t.publish_notify(topic, payload, qos, retain)
    }

    /// `publishCommandAck`'s wait (`worker.ts:953-974`): QoS 1, resolves on
    /// the PUBACK or after `timeout`, whichever is first. While disconnected
    /// the client stores the publish and sends it after reconnect, and a
    /// publish whose link dropped before its PUBACK is resent; the PUBACK of
    /// the (re)sent packet resolves it within the same budget — mqtt.js's
    /// outgoingStore (review-wp1 I-b). The future is `Send` (WP4).
    pub async fn publish_acked(
        &self,
        topic: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> AckOutcome {
        self.t.publish_acked(topic, payload, timeout).await
    }

    pub(crate) fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), String> {
        self.t.subscribe(topic, qos)
    }

    pub(crate) fn publish_raw(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
    ) -> Result<(), String> {
        self.t.publish(topic, payload, qos, false)
    }
}

/// The real client's publishing half.
pub struct RealTransport(pub MqttPublisher);

impl MqttTransport for RealTransport {
    fn publish(&self, topic: &str, payload: Vec<u8>, qos: QoS, retain: bool) -> Result<(), String> {
        self.0.publish(topic, qos, retain, payload).map_err(|e| e.0)
    }

    fn publish_acked(
        &self,
        topic: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> BoxFut<'static, AckOutcome> {
        let p = self.0.clone();
        let topic = topic.to_string();
        Box::pin(async move { p.publish_acked(&topic, false, payload, timeout).await })
    }

    fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), String> {
        self.0.subscribe(topic, qos).map_err(|e| e.0)
    }

    fn connection_watch(&self) -> watch::Receiver<LinkState> {
        self.0.state_watch()
    }

    fn publish_notify(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> BoxFut<'static, PublishDone> {
        let rx = self.0.publish_tracked(topic, qos, retain, payload);
        Box::pin(async move {
            match rx.await {
                Ok(AckOutcome::Acked) => Some(Ok(())),
                Ok(AckOutcome::Failed(msg)) => Some(Err(msg)),
                Ok(AckOutcome::TimedOut) | Err(_) => None,
            }
        })
    }
}

/// `createStubClient()` (`mqtt.ts:74-105`): connects "on the next tick",
/// every publish succeeds at once, subscribes succeed. Phase 0 A2: each
/// telemetry payload is printed to stdout (one JSON line per tick).
pub struct OfflineTransport {
    state: watch::Sender<LinkState>,
    /// Held so the event stream stays open (the stub never closes).
    _events: mpsc::UnboundedSender<MqttEvent>,
    telemetry_suffix: &'static str,
}

impl OfflineTransport {
    /// The transport and the event stream that delivers its one `Connected`.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<MqttEvent>) {
        let (state, _) = watch::channel(LinkState::Connected);
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(MqttEvent::Connected {
            session_present: false,
        });
        (
            Arc::new(Self {
                state,
                _events: tx,
                telemetry_suffix: "/telemetry",
            }),
            rx,
        )
    }
}

impl MqttTransport for OfflineTransport {
    fn publish(
        &self,
        topic: &str,
        payload: Vec<u8>,
        _qos: QoS,
        _retain: bool,
    ) -> Result<(), String> {
        if topic.ends_with(self.telemetry_suffix) {
            println!("{}", String::from_utf8_lossy(&payload));
        }
        Ok(())
    }

    fn publish_acked(&self, _: &str, _: Vec<u8>, _: Duration) -> BoxFut<'static, AckOutcome> {
        Box::pin(async { AckOutcome::Acked })
    }

    fn subscribe(&self, _: &str, _: QoS) -> Result<(), String> {
        Ok(())
    }

    fn connection_watch(&self) -> watch::Receiver<LinkState> {
        self.state.subscribe()
    }
}

/// The worker's MQTT event handlers + the publish loop it starts.
pub struct MqttPlane {
    ctx: AgentCtx,
    mqtt: MqttHandle,
    scope: ScopeHandle,
    config: Arc<dyn ConfigPlane>,
    commands: Arc<dyn CommandPlane>,
    source: Arc<dyn TelemetrySource>,
    /// `timer` (`worker.ts:1189`): started once, survives reconnects.
    timer_started: bool,
    /// `seq` (`worker.ts:575`): +1 per built payload, never reset.
    seq: Arc<AtomicU64>,
    /// Whether the link is up as far as the handlers know (for the shutdown close).
    connected: bool,
}

impl MqttPlane {
    pub fn new(
        ctx: AgentCtx,
        mqtt: MqttHandle,
        scope: ScopeHandle,
        config: Arc<dyn ConfigPlane>,
        commands: Arc<dyn CommandPlane>,
        source: Arc<dyn TelemetrySource>,
    ) -> Self {
        Self {
            ctx,
            mqtt,
            scope,
            config,
            commands,
            source,
            timer_started: false,
            seq: Arc::new(AtomicU64::new(0)),
            connected: false,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// One event from the client, in arrival order.
    pub fn on_event(&mut self, ev: MqttEvent) {
        match ev {
            MqttEvent::Message(m) => {
                self.ctx
                    .tape
                    .mqtt_in(&m.topic, &m.payload, m.retain, m.qos as u8);
                self.on_message(&m.topic, m.payload);
            }
            MqttEvent::Connected { .. } => self.on_connect(),
            MqttEvent::Disconnected {
                reason,
                emits_error,
            } => {
                if emits_error {
                    self.on_error(&reason);
                }
                self.on_close();
            }
            MqttEvent::Reconnecting { attempt, delay } => self.on_reconnect(attempt, delay),
            MqttEvent::ServerDisconnect { reason } => {
                // mqtt.js emits `disconnect`, which the worker does not handle;
                // journal only (amb. #7). The close follows as `Disconnected`.
                info!("[agent] broker sent DISCONNECT ({reason})");
            }
            MqttEvent::SubscribeFailed { topic, reason } => {
                let cfg = &self.ctx.env.config.topics;
                let label = if topic == cfg.command {
                    "command"
                } else {
                    "config"
                };
                self.on_subscribe_failed(label, &reason);
            }
        }
    }

    /// `client.on("message")` (`worker.ts:1130-1143`). Other topics are ignored.
    fn on_message(&self, topic: &str, payload: Vec<u8>) {
        let t = &self.ctx.env.config.topics;
        if topic == t.config {
            self.config.on_message(payload);
        } else if topic == t.command {
            self.commands.on_message(payload);
        }
    }

    /// `client.on("connect")` (`worker.ts:1145-1188`), in the TS order.
    fn on_connect(&mut self) {
        let now = self.ctx.now_ms();
        let cfg = &self.ctx.env.config;
        info!(
            "[agent] connected as {} → {} (mode={}, {} Hz{})",
            cfg.device_id,
            cfg.endpoint,
            cfg.mode.as_str(),
            xorgate_core::js::number_to_string(cfg.rate_hz),
            if cfg.offline { ", offline" } else { "" }
        );
        self.connected = true;
        self.ctx.tape.conn(true);
        self.ctx
            .state
            .set_connection(ConnectionState::Connected, now);
        self.ctx.state.note(
            ActivityLevel::Info,
            format!("connected to {}", cfg.endpoint),
            now,
        );
        self.scope.note_connect(now);
        self.ctx.hooks.notify_connectivity(true);

        let online =
            serde_json::to_vec(&StatusPayload::new(&cfg.device_id, now, true)).unwrap_or_default();
        if let Err(e) = self
            .mqtt
            .publish_raw(&cfg.topics.status, online, QoS::AtMostOnce)
        {
            error!("[agent] status publish failed: {e}");
        }

        for (label, topic) in [
            ("config", &cfg.topics.config),
            ("command", &cfg.topics.command),
        ] {
            if let Err(e) = self.mqtt.subscribe(topic, QoS::AtLeastOnce) {
                self.on_subscribe_failed(label, &e);
            }
        }

        self.config.on_connect();

        if !self.timer_started {
            self.timer_started = true;
            self.start_publish_loop();
        }
    }

    /// The subscribe callback's error branch (`worker.ts:1171-1177`).
    fn on_subscribe_failed(&self, label: &str, msg: &str) {
        let t = self.ctx.now_ms();
        error!("[agent] {label} subscribe failed {msg}");
        self.ctx.state.note(
            ActivityLevel::Warn,
            format!("{label} subscribe failed: {msg}"),
            t,
        );
    }

    /// `client.on("reconnect")` (`worker.ts:1256-1262`).
    fn on_reconnect(&self, attempt: u32, delay: Duration) {
        let now = self.ctx.now_ms();
        info!(
            "[agent] reconnecting… (attempt {attempt}, after {:.1} s)",
            delay.as_secs_f64()
        );
        self.ctx
            .state
            .set_connection(ConnectionState::Reconnecting, now);
        self.ctx
            .state
            .note(ActivityLevel::Warn, "reconnecting…", now);
    }

    /// `client.on("error")` (`worker.ts:1264-1269`).
    fn on_error(&self, msg: &str) {
        let now = self.ctx.now_ms();
        error!("[agent] mqtt error {msg}");
        self.ctx.state.set_error(msg, now);
        self.ctx.state.flush(now, true);
    }

    /// `client.on("close")` (`worker.ts:1271-1284`). Also run once by the
    /// shutdown path after the final DISCONNECT (mqtt.js emits `close` when
    /// `end()` closes the stream), where `noteClose` is skipped.
    pub fn on_close(&mut self) {
        let now = self.ctx.now_ms();
        info!("[agent] connection closed");
        self.connected = false;
        self.ctx.tape.conn(false);
        self.ctx.state.set_connection(ConnectionState::Closed, now);
        self.ctx
            .state
            .note(ActivityLevel::Warn, "connection closed", now);
        if !self.ctx.shutting_down() {
            self.scope.note_close(now);
        }
        self.ctx.hooks.notify_connectivity(false);
    }

    /// The `setInterval(…, config.intervalMs)` of `worker.ts:1190`: first tick
    /// at +interval (CF2), runs across reconnects, stops at shutdown.
    fn start_publish_loop(&self) {
        let tick = PublishTick {
            ctx: self.ctx.clone(),
            mqtt: self.mqtt.clone(),
            scope: self.scope.clone(),
            source: self.source.clone(),
            seq: self.seq.clone(),
        };
        let period = Duration::from_millis(self.ctx.env.config.interval_ms.max(1.0) as u64);
        let cancel = self.ctx.cancel.clone();
        self.ctx.spawn("publish_loop", async move {
            // setInterval: re-armed from each fire (P-WP6-1).
            let mut iv = SetInterval::new(period);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = iv.tick() => tick.run().await,
                }
            }
        });
    }
}

/// One publish tick (`worker.ts:1190-1251`).
pub struct PublishTick {
    pub ctx: AgentCtx,
    pub mqtt: MqttHandle,
    pub scope: ScopeHandle,
    pub source: Arc<dyn TelemetrySource>,
    pub seq: Arc<AtomicU64>,
}

impl PublishTick {
    pub async fn run(&self) {
        if self.ctx.shutting_down() {
            return;
        }
        let reading = match self.source.read().await {
            Ok(r) => r,
            Err(msg) => {
                // A collector/read failure must not crash the daemon.
                let t = self.ctx.now_ms();
                self.ctx.state.set_error(&format!("read error: {msg}"), t);
                self.ctx.state.flush(t, true);
                return;
            }
        };
        if self.ctx.shutting_down() {
            return; // shutdown happened during the await
        }
        let ts = self.ctx.now_ms();
        let media = self.ctx.hooks.overlay_media(reading.media);
        let recording = self.ctx.hooks.recording_status();
        self.ctx.hooks.note_recording_status(&recording);
        let payload = TelemetryPayload {
            v: 1,
            device_id: self.ctx.env.config.device_id.clone(),
            ts,
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
            // `system.led_state` / `system.health_flags` on every live
            // sample (`worker.ts:1385-1386` @ 09da38d); the reading is this
            // tick's own copy, so no collector cache is touched.
            metrics: xorgate_core::led_telemetry::with_led_leaves(
                reading.metrics,
                self.ctx.hooks.led_telemetry(),
            ),
            media,
            recording: Some(recording),
        };
        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
        self.ctx.state.set_latest(payload);
        // Clock trust gate (`worker.ts:1406-1412` @ 09da38d): the local
        // snapshot above is kept (and `seq` already advanced — the cloud sees
        // the gap, CF72); only the cloud publish is withheld while the wall
        // clock is untrusted, never past the gate's grace window.
        if !self.ctx.hooks.publish_clock_gate() {
            return;
        }
        // The backoff re-probe lives on this tick (`:1233-1234`).
        self.scope.note_tick(ts);
        let mut done = self
            .mqtt
            .publish_notify(&self.scope.topic(), bytes, QoS::AtMostOnce, false);
        let on_done = PublishCallback {
            ctx: self.ctx.clone(),
            scope: self.scope.clone(),
        };
        // The callback (`:1235-1245`). Connected, it fires once the payload is
        // written; disconnected, the payload waits in the client's offline
        // queue and the callback fires when it is flushed after the reconnect
        // (`queueQoSZero`) — so `published` counts what left the client, as in
        // the TS. Run inline when already settled (fakes, the offline stub).
        if let Some(r) = ready_now(&mut done) {
            on_done.run(r);
            return;
        }
        let cancel = self.ctx.cancel.clone();
        self.ctx.spawn("publish_callback", async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                r = done => on_done.run(r),
            }
        });
    }
}

/// The tick's publish callback (`worker.ts:1235-1245`).
struct PublishCallback {
    ctx: AgentCtx,
    scope: ScopeHandle,
}

impl PublishCallback {
    fn run(&self, r: PublishDone) {
        let Some(r) = r else {
            return; // never called back (discarded QoS 0)
        };
        if self.ctx.shutting_down() {
            return; // don't re-write state after the final flush
        }
        let t = self.ctx.now_ms();
        match r {
            Ok(()) => {
                self.ctx.state.record_publish(t);
                // Counts toward "this connection carried traffic on the scoped topic".
                self.scope.note_publish();
            }
            Err(msg) => self.ctx.state.record_publish_error(&msg, t),
        }
        self.ctx.state.flush(t, false);
    }
}

/// Poll a future once without a runtime waker: `Some` when already settled.
fn ready_now<T>(f: &mut BoxFut<'static, T>) -> Option<T> {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match f.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => Some(v),
        std::task::Poll::Pending => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>(_: &T) {}

    /// WP4 (`publish_acked` used to hold a `watch::Ref` across an await): the
    /// publish futures must be `Send` so planes can spawn them.
    #[allow(dead_code)]
    fn publish_futures_are_send(h: &MqttHandle) {
        assert_send(&h.publish_acked("t", vec![], Duration::ZERO));
        assert_send(&h.publish_notify("t", vec![], QoS::AtMostOnce, false));
    }

    #[test]
    fn ready_now_polls_once() {
        let mut ready: BoxFut<'static, u8> = Box::pin(async { 7 });
        assert_eq!(ready_now(&mut ready), Some(7));
        let mut pending: BoxFut<'static, u8> = Box::pin(std::future::pending());
        assert_eq!(ready_now(&mut pending), None);
    }
}
