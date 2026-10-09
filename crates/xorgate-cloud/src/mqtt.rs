//! AWS IoT Core MQTT v5 client wrapper (rumqttc 0.25 `v5` + rustls).
//!
//! Port of the connection contract in `src/agent/mqtt.ts` (`:38-67`) + the
//! connect / reconnect / close handling the worker hangs off it:
//! - `clientId` = deviceId (the IoT policy scopes `iot:Connect` to the thing name);
//! - mTLS on `IOT_DATA_ENDPOINT:8883`, MQTT 5, clean start, keepalive
//!   `MQTT_KEEPALIVE_SEC` (default 60), connect timeout 30 s;
//! - Last-Will on `…/status` with the CONNECT-time `ts` (re-stamped before
//!   every connect attempt), QoS 0, not retained;
//! - `set_max_packet_size(Some(150_000))` (rumqttc #1018: the 10 KiB default
//!   would reset the connection on a large retained config);
//! - exponential backoff with jitter between attempts (ours — rumqttc has none);
//! - a `watch` channel exposing the connection state.
//!
//! Phase 2 (WP1) additions:
//! - **Lossless inbound** (CF1): events go through an UNBOUNDED channel, so a
//!   retained config or a command is never dropped while a consumer lags.
//! - **PUBACK-awaited publish** ([`MqttPublisher::publish_acked`]): every
//!   QoS ≥ 1 publish is registered, under one lock, in the order it enters
//!   rumqttc's request channel; rumqttc assigns packet ids in that same order
//!   (`Outgoing::Publish(pkid)`), so the n-th registered waiter is bound to the
//!   n-th non-zero pkid and resolved by the matching `PubAck`. On a link error
//!   every unresolved waiter fails (clean start: rumqttc drops what it held).
//! - **Plane-driven subscribe** ([`MqttPublisher::subscribe`]): the worker
//!   subscribes from its own connect handler, AFTER publishing `online:true`,
//!   in the TS order (`worker.ts:1157-1180`); a SUBACK failure comes back as
//!   [`MqttEvent::SubscribeFailed`] naming the topic.
//! - `Reconnecting` is emitted when the next attempt STARTS (after the
//!   backoff sleep), like mqtt.js's `reconnect` event.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::v5::mqttbytes::v5::{LastWill, Packet, SubscribeReasonCode};
use rumqttc::v5::{AsyncClient, ConnectionError, Event, EventLoop, MqttOptions, StateError};
use rumqttc::{Outgoing, TlsConfiguration, Transport};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub use rumqttc::v5::mqttbytes::QoS;

use crate::backoff::{Backoff, BackoffConfig};

/// Mirrors the TS `ConnectionState` strings written to `state.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Closed,
}

impl ConnectionState {
    /// The exact string the TS worker writes (`"connecting" | "connected" | …`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ConnectionState::Connecting => "connecting",
            ConnectionState::Connected => "connected",
            ConnectionState::Reconnecting => "reconnecting",
            ConnectionState::Closed => "closed",
        }
    }
}

/// An inbound application message.
#[derive(Debug, Clone)]
pub struct Message {
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
    pub qos: QoS,
}

#[derive(Debug, Clone)]
pub enum MqttEvent {
    /// CONNACK received.
    Connected {
        session_present: bool,
    },
    /// The link dropped or a connect attempt failed. `reason` is the error
    /// text; `emits_error` is whether mqtt.js would have emitted its `error`
    /// event first (every coded stream error, keepalive and connack timeouts,
    /// TLS failures) — false for a peer that simply closed the socket or sent
    /// DISCONNECT, where mqtt.js only emits `close`.
    Disconnected {
        reason: String,
        emits_error: bool,
    },
    /// The next connect attempt is starting (1-based attempt number; `delay`
    /// is the backoff that preceded it).
    Reconnecting {
        attempt: u32,
        delay: Duration,
    },
    /// The broker sent DISCONNECT (e.g. another client took our clientId).
    ServerDisconnect {
        reason: String,
    },
    Message(Message),
    /// A SUBACK carried a failure code for `topic`.
    SubscribeFailed {
        topic: String,
        reason: String,
    },
}

/// A publish/subscribe that could not even be queued (request channel full or
/// the event loop gone). rumqttc's `ClientError` carries the whole request, so
/// it is reduced to its message here.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct PublishError(pub String);

/// How an acknowledged publish ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckOutcome {
    /// The broker PUBACKed it.
    Acked,
    /// No PUBACK within the timeout.
    TimedOut,
    /// It could not be queued, or the link dropped before the PUBACK.
    Failed(String),
}

/// The Last-Will as built right before a connect attempt.
#[derive(Debug, Clone)]
pub struct Will {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: QoS,
    pub retain: bool,
}

/// Called before EVERY connect attempt so the will carries that attempt's ts.
pub type WillFactory = Arc<dyn Fn() -> Will + Send + Sync>;

pub struct MqttConfig {
    pub client_id: String,
    pub host: String,
    pub port: u16,
    pub keep_alive: Duration,
    pub connect_timeout: Duration,
    pub tls: Arc<rustls::ClientConfig>,
    pub max_packet_size: u32,
    /// (topic, qos) subscribed by the LOOP on every CONNACK, before the
    /// `Connected` event. The agent leaves this empty and subscribes itself
    /// ([`MqttPublisher::subscribe`]) to keep the TS order.
    pub subscriptions: Vec<(String, QoS)>,
    pub backoff: BackoffConfig,
    pub will: Option<WillFactory>,
}

impl MqttConfig {
    /// The TS defaults: port 8883, keepalive 60 s, connect timeout 30 s,
    /// max packet 150 000, backoff 1 s → 60 s with jitter.
    pub fn new(client_id: &str, host: &str, tls: Arc<rustls::ClientConfig>) -> Self {
        Self {
            client_id: client_id.to_string(),
            host: host.to_string(),
            port: 8883,
            keep_alive: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(30),
            tls,
            max_packet_size: 150_000,
            subscriptions: Vec::new(),
            backoff: BackoffConfig::default(),
            will: None,
        }
    }
}

type Waiter = Option<oneshot::Sender<AckOutcome>>;

/// Packet-id bookkeeping for QoS ≥ 1 publishes and subscribes (see the
/// module doc for why FIFO order is sound).
#[derive(Default)]
struct Tracker {
    /// Queued publishes not yet given a pkid by the event loop.
    unsent: VecDeque<Waiter>,
    /// pkid → waiter, until the PUBACK.
    inflight: HashMap<u16, Waiter>,
    /// Queued subscribes (topic) not yet given a pkid.
    sub_unsent: VecDeque<String>,
    /// pkid → topic, until the SUBACK.
    sub_inflight: HashMap<u16, String>,
}

impl Tracker {
    fn sent(&mut self, pkid: u16) {
        if pkid == 0 || self.inflight.contains_key(&pkid) {
            return; // QoS 0, or a retransmission of a publish we already bound
        }
        if let Some(w) = self.unsent.pop_front() {
            self.inflight.insert(pkid, w);
        }
    }

    fn acked(&mut self, pkid: u16) {
        if let Some(Some(tx)) = self.inflight.remove(&pkid) {
            let _ = tx.send(AckOutcome::Acked);
        }
    }

    fn fail_all(&mut self, reason: &str) {
        let waiters = self
            .unsent
            .drain(..)
            .chain(self.inflight.drain().map(|(_, w)| w));
        for tx in waiters.flatten() {
            let _ = tx.send(AckOutcome::Failed(reason.to_string()));
        }
        self.sub_unsent.clear();
        self.sub_inflight.clear();
    }
}

/// Cloneable publishing half of a connection.
#[derive(Clone)]
pub struct MqttPublisher {
    client: AsyncClient,
    tracker: Arc<Mutex<Tracker>>,
    state: watch::Receiver<ConnectionState>,
}

impl MqttPublisher {
    pub fn state(&self) -> ConnectionState {
        *self.state.borrow()
    }

    pub fn state_watch(&self) -> watch::Receiver<ConnectionState> {
        self.state.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.state() == ConnectionState::Connected
    }

    fn enqueue(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Vec<u8>,
        waiter: Waiter,
    ) -> Result<(), (PublishError, Waiter)> {
        let mut t = self.tracker.lock().unwrap_or_else(|p| p.into_inner());
        match self.client.try_publish(topic, qos, retain, payload) {
            Ok(()) => {
                if qos != QoS::AtMostOnce {
                    t.unsent.push_back(waiter);
                }
                Ok(())
            }
            Err(e) => Err((PublishError(e.to_string()), waiter)),
        }
    }

    /// Fire-and-forget publish (errors only when it could not be queued).
    pub fn publish(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Vec<u8>,
    ) -> Result<(), PublishError> {
        self.enqueue(topic, qos, retain, payload, None)
            .map_err(|(e, _)| e)
    }

    /// QoS 1 publish that resolves on its PUBACK, or after `timeout`.
    pub async fn publish_acked(
        &self,
        topic: &str,
        retain: bool,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> AckOutcome {
        let (tx, rx) = oneshot::channel();
        if let Err((e, _)) = self.enqueue(topic, QoS::AtLeastOnce, retain, payload, Some(tx)) {
            return AckOutcome::Failed(e.0);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => AckOutcome::Failed("mqtt client gone".to_string()),
            Err(_) => AckOutcome::TimedOut,
        }
    }

    /// Queue a SUBSCRIBE; a SUBACK failure arrives as [`MqttEvent::SubscribeFailed`].
    pub fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), PublishError> {
        let mut t = self.tracker.lock().unwrap_or_else(|p| p.into_inner());
        self.client
            .try_subscribe(topic, qos)
            .map_err(|e| PublishError(e.to_string()))?;
        t.sub_unsent.push_back(topic.to_string());
        Ok(())
    }
}

/// Handle to a running connection. Dropping it does NOT disconnect — call
/// [`MqttClient::shutdown`] for the graceful path.
pub struct MqttClient {
    publisher: MqttPublisher,
    events: mpsc::UnboundedReceiver<MqttEvent>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl MqttClient {
    /// Build the options, spawn the event-loop task and return immediately;
    /// the first connect attempt starts at once.
    pub fn connect(cfg: MqttConfig) -> Self {
        let mut options = MqttOptions::new(cfg.client_id.clone(), cfg.host.clone(), cfg.port);
        options.set_transport(Transport::Tls(TlsConfiguration::Rustls(cfg.tls.clone())));
        options.set_keep_alive(cfg.keep_alive);
        options.set_clean_start(true);
        options.set_connection_timeout(cfg.connect_timeout.as_secs().max(1));
        options.set_max_packet_size(Some(cfg.max_packet_size));
        if let Some(will) = &cfg.will {
            options.set_last_will(to_last_will(will()));
        }

        let (client, eventloop) = AsyncClient::new(options, 64);
        let (state_tx, state_rx) = watch::channel(ConnectionState::Connecting);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let tracker = Arc::new(Mutex::new(Tracker::default()));
        let task = tokio::spawn(run_loop(LoopCtx {
            eventloop,
            client: client.clone(),
            subscriptions: cfg.subscriptions,
            backoff: cfg.backoff,
            will: cfg.will,
            state_tx,
            events_tx,
            cancel: cancel.clone(),
            tracker: tracker.clone(),
        }));
        Self {
            publisher: MqttPublisher {
                client,
                tracker,
                state: state_rx,
            },
            events: events_rx,
            cancel,
            task,
        }
    }

    /// A cloneable publishing handle.
    pub fn publisher(&self) -> MqttPublisher {
        self.publisher.clone()
    }

    pub fn state(&self) -> ConnectionState {
        self.publisher.state()
    }

    pub fn state_watch(&self) -> watch::Receiver<ConnectionState> {
        self.publisher.state_watch()
    }

    pub fn is_connected(&self) -> bool {
        self.publisher.is_connected()
    }

    /// Split into the publisher, the (lossless) event stream and a shutdown
    /// handle, for callers that consume events on another task.
    pub fn split(
        self,
    ) -> (
        MqttPublisher,
        mpsc::UnboundedReceiver<MqttEvent>,
        MqttShutdown,
    ) {
        (
            self.publisher.clone(),
            self.events,
            MqttShutdown {
                client: self.publisher.client,
                cancel: self.cancel,
                task: self.task,
            },
        )
    }

    /// Next event from the loop; `None` once the loop task has ended.
    pub async fn next_event(&mut self) -> Option<MqttEvent> {
        self.events.recv().await
    }

    /// Non-blocking publish (errors when the request channel is full/closed).
    pub fn try_publish(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Vec<u8>,
    ) -> Result<(), PublishError> {
        self.publisher.publish(topic, qos, retain, payload)
    }

    /// See [`MqttShutdown::shutdown`].
    pub async fn shutdown(self, wait: Duration) -> bool {
        let (_, _, s) = self.split();
        s.shutdown(wait).await
    }
}

/// The graceful-stop half of a split [`MqttClient`].
pub struct MqttShutdown {
    client: AsyncClient,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl MqttShutdown {
    /// Graceful stop: queue DISCONNECT behind whatever is already queued
    /// (e.g. the final `online:false` status), then wait up to `wait` for the
    /// loop to see it on the wire. Returns `true` when the loop ended cleanly
    /// within the budget, `false` when it had to be abandoned.
    pub async fn shutdown(self, wait: Duration) -> bool {
        self.cancel.cancel();
        if let Err(e) = self.client.try_disconnect() {
            debug!("[mqtt] disconnect request not queued: {e}");
        }
        let task = self.task;
        let abort = task.abort_handle();
        let clean = tokio::time::timeout(wait, task).await.is_ok();
        if !clean {
            abort.abort();
        }
        clean
    }
}

fn to_last_will(w: Will) -> LastWill {
    LastWill::new(w.topic, w.payload, w.qos, w.retain, None)
}

/// Whether mqtt.js would emit `error` before `close` for this failure: a peer
/// that closed the socket (rumqttc `ConnectionAborted`) or a broker DISCONNECT
/// ends the stream without an error; everything else (coded socket errors,
/// TLS, keepalive / connack timeouts, refused CONNACK) emits one.
pub fn emits_node_error(e: &ConnectionError) -> bool {
    !matches!(
        e,
        ConnectionError::MqttState(StateError::ConnectionAborted)
            | ConnectionError::MqttState(StateError::ServerDisconnect { .. })
    )
}

fn lock_tracker(t: &Mutex<Tracker>) -> std::sync::MutexGuard<'_, Tracker> {
    t.lock().unwrap_or_else(|p| p.into_inner())
}

struct LoopCtx {
    eventloop: EventLoop,
    client: AsyncClient,
    subscriptions: Vec<(String, QoS)>,
    backoff: BackoffConfig,
    will: Option<WillFactory>,
    state_tx: watch::Sender<ConnectionState>,
    events_tx: mpsc::UnboundedSender<MqttEvent>,
    cancel: CancellationToken,
    tracker: Arc<Mutex<Tracker>>,
}

async fn run_loop(mut cx: LoopCtx) {
    let mut backoff = Backoff::new(cx.backoff.clone());
    let mut ever_connected = false;
    let events_tx = cx.events_tx.clone();
    let send = move |ev: MqttEvent| {
        // Unbounded: never drops (CF1). Errors only once the receiver is gone.
        let _ = events_tx.send(ev);
    };
    loop {
        match cx.eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                backoff.reset();
                ever_connected = true;
                let _ = cx.state_tx.send(ConnectionState::Connected);
                for (topic, qos) in &cx.subscriptions {
                    let mut t = lock_tracker(&cx.tracker);
                    match cx.client.try_subscribe(topic.clone(), *qos) {
                        Ok(()) => t.sub_unsent.push_back(topic.clone()),
                        Err(e) => send(MqttEvent::SubscribeFailed {
                            topic: topic.clone(),
                            reason: e.to_string(),
                        }),
                    }
                }
                send(MqttEvent::Connected {
                    session_present: ack.session_present,
                });
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                send(MqttEvent::Message(Message {
                    topic: String::from_utf8_lossy(&p.topic).into_owned(),
                    payload: p.payload.to_vec(),
                    retain: p.retain,
                    qos: p.qos,
                }));
            }
            Ok(Event::Outgoing(Outgoing::Publish(pkid))) => lock_tracker(&cx.tracker).sent(pkid),
            Ok(Event::Incoming(Packet::PubAck(ack))) => lock_tracker(&cx.tracker).acked(ack.pkid),
            Ok(Event::Outgoing(Outgoing::Subscribe(pkid))) => {
                let mut t = lock_tracker(&cx.tracker);
                if let Some(topic) = t.sub_unsent.pop_front() {
                    t.sub_inflight.insert(pkid, topic);
                }
            }
            Ok(Event::Incoming(Packet::SubAck(ack))) => {
                let topic = lock_tracker(&cx.tracker)
                    .sub_inflight
                    .remove(&ack.pkid)
                    .unwrap_or_default();
                for code in &ack.return_codes {
                    if !matches!(code, SubscribeReasonCode::Success(_)) {
                        send(MqttEvent::SubscribeFailed {
                            topic: topic.clone(),
                            reason: format!("{code:?}"),
                        });
                    }
                }
            }
            Ok(Event::Incoming(Packet::Disconnect(d))) => {
                send(MqttEvent::ServerDisconnect {
                    reason: format!("{:?}", d.reason_code),
                });
            }
            Ok(Event::Outgoing(Outgoing::Disconnect)) => {
                if cx.cancel.is_cancelled() {
                    info!("[mqtt] disconnect sent — event loop ending");
                    lock_tracker(&cx.tracker).fail_all("client disconnecting");
                    let _ = cx.state_tx.send(ConnectionState::Closed);
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => {
                let reason = e.to_string();
                lock_tracker(&cx.tracker).fail_all(&reason);
                let _ = cx.state_tx.send(ConnectionState::Closed);
                send(MqttEvent::Disconnected {
                    reason,
                    emits_error: emits_node_error(&e),
                });
                if cx.cancel.is_cancelled() {
                    break;
                }
                let delay = backoff.next_delay();
                let attempt = backoff.attempt();
                tokio::select! {
                    _ = cx.cancel.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {}
                }
                let _ = cx.state_tx.send(if ever_connected {
                    ConnectionState::Reconnecting
                } else {
                    ConnectionState::Connecting
                });
                send(MqttEvent::Reconnecting { attempt, delay });
                // Re-stamp the Last-Will so it carries THIS attempt's connect ts.
                if let Some(factory) = &cx.will {
                    cx.eventloop.options.set_last_will(to_last_will(factory()));
                }
            }
        }
    }
    let _ = cx.state_tx.send(ConnectionState::Closed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_binds_waiters_in_fifo_order_and_ignores_retransmits() {
        let mut t = Tracker::default();
        let (a_tx, mut a_rx) = oneshot::channel();
        let (b_tx, mut b_rx) = oneshot::channel();
        t.unsent.push_back(Some(a_tx));
        t.unsent.push_back(None); // a fire-and-forget QoS 1 report in between
        t.unsent.push_back(Some(b_tx));
        t.sent(0); // QoS 0 telemetry: not ours
        t.sent(1);
        t.sent(1); // retransmission
        t.sent(2);
        t.sent(3);
        t.acked(3);
        assert_eq!(b_rx.try_recv().unwrap(), AckOutcome::Acked);
        assert!(a_rx.try_recv().is_err());
        t.acked(2);
        t.fail_all("gone");
        assert_eq!(a_rx.try_recv().unwrap(), AckOutcome::Failed("gone".into()));
        assert!(t.inflight.is_empty() && t.unsent.is_empty());
    }

    #[test]
    fn peer_close_is_not_a_node_error() {
        assert!(!emits_node_error(&ConnectionError::MqttState(
            StateError::ConnectionAborted
        )));
        assert!(emits_node_error(&ConnectionError::Io(
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused)
        )));
    }
}
