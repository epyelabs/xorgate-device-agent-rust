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
//! - **Publish completion** ([`MqttPublisher::publish_tracked`] /
//!   [`MqttPublisher::publish_acked`]): every publish is registered, under one
//!   lock, in the order it enters rumqttc's request channel; rumqttc emits one
//!   `Outgoing::Publish(pkid)` per publish in that same order (pkid 0 for
//!   QoS 0), so the n-th registered publish is the n-th `Outgoing::Publish`.
//!   QoS 0 completes when written (mqtt.js calls a QoS 0 callback on write),
//!   QoS 1 on the matching `PubAck`.
//! - **Plane-driven subscribe** ([`MqttPublisher::subscribe`]): the worker
//!   subscribes from its own connect handler, AFTER publishing `online:true`,
//!   in the TS order (`worker.ts:1157-1180`); a SUBACK failure comes back as
//!   [`MqttEvent::SubscribeFailed`] naming the topic.
//! - `Reconnecting` is emitted when the next attempt STARTS (after the
//!   backoff sleep), like mqtt.js's `reconnect` event.
//!
//! WP1 follow-up (review-wp1 #2, #3, #7, I-b) — **mqtt.js offline semantics**
//! (`node_modules/mqtt` 5.15.1 `build/lib/client.js`):
//! - a publish made while the link is down is kept: QoS ≥ 1 in the
//!   `outgoingStore`, QoS 0 in the `queue` because the TS options leave
//!   `queueQoSZero` at its default `true` (`client.js:156-157,1068-1096`);
//! - QoS ≥ 1 publishes not PUBACKed when the link drops go back to the store
//!   (mqtt.js keeps them in `outgoingStore`; only volatile entries are failed,
//!   `client.js:794-803`) and are resent on the next CONNACK, so the waiter of
//!   [`MqttPublisher::publish_acked`] resolves on the RESENT packet's PUBACK
//!   (within its budget);
//! - on CONNACK the store is resent first, then the queue, then the automatic
//!   resubscribe (`client.js:180-213,1132-1255`), all ahead of the caller's
//!   `Connected` handler;
//! - the link state flips with the tracker lock held, and requests that raced
//!   into rumqttc's channel around a drop are drained and re-queued by us, so a
//!   waiter can never bind to a packet of another connection (review-wp1 #2);
//! - rumqttc's `AwaitPingResp` is reported as mqtt.js's `Keepalive timeout`
//!   and the connect timeout as `connack timeout` (`client.js:363,1128`; #3);
//! - a broker DISCONNECT surfaces as [`MqttEvent::ServerDisconnect`] before
//!   the `Disconnected` it causes (#7).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rumqttc::v5::mqttbytes::v5::{LastWill, Packet, SubscribeReasonCode};
use rumqttc::v5::{AsyncClient, ConnectionError, Event, EventLoop, MqttOptions, StateError};
use rumqttc::{Outgoing, TlsConfiguration, Transport};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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
    /// text ([`error_text`]); `emits_error` is whether mqtt.js would have
    /// emitted its `error` event first (every coded stream error, keepalive and
    /// connack timeouts, TLS failures) — false for a peer that simply closed
    /// the socket or sent DISCONNECT, where mqtt.js only emits `close`.
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
    /// Always followed by the `Disconnected` it causes.
    ServerDisconnect {
        reason: String,
    },
    Message(Message),
    /// A subscribe made by [`MqttPublisher::subscribe`] failed for `topic`: a
    /// SUBACK failure code, or `Connection closed` when the link dropped
    /// before its SUBACK (mqtt.js `_flushVolatile`, `client.js:794-803`).
    /// The automatic resubscribe reports nothing, as in mqtt.js.
    SubscribeFailed {
        topic: String,
        reason: String,
    },
}

/// A publish/subscribe the client refused (request channel full, the event
/// loop gone, or — QoS 0 with [`QUEUE_QOS_ZERO`] off — no connection).
/// rumqttc's `ClientError` carries the whole request, so it is reduced to its
/// message here.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct PublishError(pub String);

/// How a tracked publish ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckOutcome {
    /// The broker PUBACKed it (QoS 1) / it was written (QoS 0, tracked only).
    Acked,
    /// No PUBACK within the timeout. The publish itself stays queued.
    TimedOut,
    /// The client refused it, the offline store overflowed, or the client was
    /// shut down first.
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

/// mqtt.js `queueQoSZero` under the TS options: `mqtt.ts:51-66` leaves it
/// unset, so the client default (`client.js:156-157`) — `true` — applies: a
/// QoS 0 publish made while disconnected is queued and sent after reconnect
/// (its completion — the TS publish callback — fires then).
pub const QUEUE_QOS_ZERO: bool = true;

/// Bound on EACH offline queue (the QoS ≥ 1 store and the QoS 0 / subscribe
/// queue). mqtt.js keeps both in memory without a bound (`client.js:1081-1096`,
/// the default in-memory `Store`); the Rust client drops the OLDEST entry once
/// a queue holds this many, so a days-long outage cannot grow the heap without
/// limit. A dropped QoS ≥ 1 publish fails its waiter (`offline queue full`); a
/// dropped QoS 0 one completes never (the TS callback would only have fired
/// after reconnect). At the TS 1 Hz telemetry rate the QoS 0 bound is ~16 min
/// of outage (tolerance row, WP7).
pub const OFFLINE_QUEUE_LIMIT: usize = 1000;

/// rumqttc request-channel capacity: room to flush both full offline queues on
/// CONNACK in one go, plus live traffic (review-wp1 #10b: mqtt.js buffers
/// without limit; past this a live publish is refused with rumqttc's error).
/// flume allocates lazily, so the cap costs nothing until used.
const REQUEST_CHANNEL_CAP: usize = 2 * OFFLINE_QUEUE_LIMIT + 64;

/// mqtt.js's `Error('Connection closed')` text (`client.js:798,812`).
const CONNECTION_CLOSED: &str = "Connection closed";

type Waiter = Option<oneshot::Sender<AckOutcome>>;

/// One publish the client still owes the broker: QoS ≥ 1 until its PUBACK,
/// QoS 0 until it is written.
struct Pending {
    /// Enqueue order — mqtt.js's outgoingStore replays in insertion order.
    seq: u64,
    topic: String,
    payload: Bytes,
    qos: QoS,
    retain: bool,
    waiter: Waiter,
}

impl Pending {
    fn resolve(self, outcome: AckOutcome) {
        if let Some(tx) = self.waiter {
            let _ = tx.send(outcome);
        }
    }

    fn fail(self, reason: &str) {
        self.resolve(AckOutcome::Failed(reason.to_string()));
    }
}

/// An entry of mqtt.js's offline `queue` (`client.js:1081-1083`): QoS 0
/// publishes (`queueQoSZero`) and every non-publish packet (subscribes).
enum Queued {
    Publish(Pending),
    /// `report`: the subscribe's caller still waits for an outcome (mqtt.js
    /// fails a queued subscribe's callback once, at the next close, and then
    /// still sends it — `_flushVolatile` deletes the callback, not the packet).
    Subscribe {
        topic: String,
        qos: QoS,
        report: bool,
    },
}

/// A SUBSCRIBE awaiting its SUBACK. `report` is false for the automatic
/// resubscribe on reconnect, whose failures mqtt.js reports to nobody (the
/// call has no callback, `client.js:1143-1151`).
struct SubTrack {
    topic: String,
    report: bool,
}

/// Packet-id bookkeeping (see the module doc for why FIFO order is sound),
/// plus mqtt.js's offline state.
///
/// Invariant (review-wp1 #2): a request enters rumqttc's channel only while
/// the link state is `Connected`, every transition into or out of `Connected`
/// happens with this lock held, and on a drop the loop (lock held) drains
/// whatever reached the channel and re-queues it from `unsent`/`inflight` — so
/// a waiter can never be bound to a packet of another connection.
#[derive(Default)]
struct Tracker {
    next_seq: u64,
    /// Publishes in rumqttc's channel, not yet `Outgoing::Publish`ed (all QoS).
    unsent: VecDeque<Pending>,
    /// pkid → QoS ≥ 1 publish, until the PUBACK.
    inflight: HashMap<u16, Pending>,
    /// mqtt.js `outgoingStore`: QoS ≥ 1 publishes made while disconnected, and
    /// (after a drop) the unacknowledged ones, resent in order on CONNACK
    /// BEFORE the `connect` event (`client.js:1165-1255`).
    store: VecDeque<Pending>,
    /// mqtt.js `queue`: delivered on `connect`, after the store (`client.js:180-213`).
    queue: VecDeque<Queued>,
    /// mqtt.js `_resubscribeTopics` (insertion order).
    resubscribe: Vec<(String, QoS)>,
    /// `!_firstConnection`: resubscribe only from the second CONNACK on.
    connected_before: bool,
    /// Subscribes in rumqttc's channel, not yet given a pkid.
    sub_unsent: VecDeque<SubTrack>,
    /// pkid → subscribe, until the SUBACK.
    sub_inflight: HashMap<u16, SubTrack>,
    /// One overflow warning per outage.
    overflow_warned: bool,
}

impl Tracker {
    fn pending(
        &mut self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Bytes,
        waiter: Waiter,
    ) -> Pending {
        self.next_seq += 1;
        Pending {
            seq: self.next_seq,
            topic: topic.to_string(),
            payload,
            qos,
            retain,
            waiter,
        }
    }

    /// `Outgoing::Publish(pkid)`: the oldest unsent publish was written.
    fn sent(&mut self, pkid: u16) {
        if pkid != 0 && self.inflight.contains_key(&pkid) {
            return; // a retransmission of a publish already bound
        }
        let Some(p) = self.unsent.pop_front() else {
            return;
        };
        if p.qos == QoS::AtMostOnce {
            p.resolve(AckOutcome::Acked);
        } else {
            self.inflight.insert(pkid, p);
        }
    }

    fn acked(&mut self, pkid: u16) {
        if let Some(p) = self.inflight.remove(&pkid) {
            p.resolve(AckOutcome::Acked);
        }
    }

    /// `Outgoing::Subscribe(pkid)`.
    fn subscribe_sent(&mut self, pkid: u16) {
        if let Some(s) = self.sub_unsent.pop_front() {
            self.sub_inflight.insert(pkid, s);
        }
    }

    /// Hand one publish to rumqttc (link up, lock held). On a refusal the
    /// waiter resolves `Failed` with the client's message.
    fn send_now(&mut self, client: &AsyncClient, p: Pending) -> Result<(), PublishError> {
        match client.try_publish(p.topic.clone(), p.qos, p.retain, p.payload.clone()) {
            Ok(()) => {
                self.unsent.push_back(p);
                Ok(())
            }
            Err(e) => {
                let e = PublishError(e.to_string());
                p.fail(&e.0);
                Err(e)
            }
        }
    }

    /// mqtt.js `_storePacket` (`client.js:1068-1096`) for a publish.
    fn store_offline(&mut self, p: Pending) -> Result<(), PublishError> {
        if p.qos != QoS::AtMostOnce {
            self.store.push_back(p);
            while self.store.len() > OFFLINE_QUEUE_LIMIT {
                if let Some(old) = self.store.pop_front() {
                    warn!(
                        "[mqtt] offline queue full ({OFFLINE_QUEUE_LIMIT}) — dropped the oldest QoS 1 publish ({})",
                        old.topic
                    );
                    old.fail("offline queue full");
                }
            }
            Ok(())
        } else if QUEUE_QOS_ZERO {
            self.queue_offline(Queued::Publish(p));
            Ok(())
        } else {
            let e = "No connection to broker"; // `client.js:1094`
            p.fail(e);
            Err(PublishError(e.to_string()))
        }
    }

    fn queue_offline(&mut self, q: Queued) {
        self.queue.push_back(q);
        while self.queue.len() > OFFLINE_QUEUE_LIMIT {
            let Some(old) = self.queue.pop_front() else {
                break;
            };
            if !self.overflow_warned {
                self.overflow_warned = true;
                warn!(
                    "[mqtt] offline queue full ({OFFLINE_QUEUE_LIMIT}) — dropping the oldest queued QoS 0 publishes until reconnect"
                );
            }
            match old {
                // Never completes (the TS callback would only fire after reconnect).
                Queued::Publish(p) => drop(p),
                Queued::Subscribe { topic, .. } => {
                    warn!("[mqtt] offline queue full — dropped a queued subscribe ({topic})");
                }
            }
        }
    }

    /// mqtt.js `parseSub` (`client.js:489-505`): record the topic for the
    /// automatic resubscribe; `false` when it is already recorded at this QoS
    /// or higher (mqtt.js then sends nothing and calls back `(null, [])`).
    fn remember_subscription(&mut self, topic: &str, qos: QoS) -> bool {
        match self.resubscribe.iter_mut().find(|(t, _)| t == topic) {
            Some((_, q)) if *q >= qos => false,
            Some((_, q)) => {
                *q = qos;
                true
            }
            None => {
                self.resubscribe.push((topic.to_string(), qos));
                true
            }
        }
    }

    /// The stream closed (lock held, state already off `Connected`; also run
    /// for a failed connect attempt, where mqtt.js emits `close` too):
    /// - every unacknowledged QoS ≥ 1 publish goes back to the store in its
    ///   original order, ahead of anything stored since (mqtt.js keeps it in
    ///   `outgoingStore` with its callback in `outgoing`);
    /// - a QoS 0 publish handed to rumqttc but not yet written is dropped
    ///   without completing (mqtt.js would already have written it to the
    ///   dying socket);
    /// - subscribes still awaiting a SUBACK, and queued ones not yet failed,
    ///   fail their caller with `Connection closed` (`_flushVolatile`); the
    ///   automatic resubscribe covers the subscription itself.
    ///
    /// Returns the topics whose subscribe failed, in send order.
    fn on_close(&mut self) -> Vec<String> {
        let mut back: Vec<Pending> = self
            .inflight
            .drain()
            .map(|(_, p)| p)
            .chain(self.unsent.drain(..).filter(|p| p.qos != QoS::AtMostOnce))
            .chain(self.store.drain(..))
            .collect();
        back.sort_by_key(|p| p.seq);
        for p in back {
            let _ = self.store_offline(p);
        }

        let mut inflight: Vec<(u16, SubTrack)> = self.sub_inflight.drain().collect();
        inflight.sort_by_key(|(pkid, _)| *pkid);
        let mut failed: Vec<String> = inflight
            .into_iter()
            .map(|(_, s)| s)
            .chain(self.sub_unsent.drain(..))
            .filter(|s| s.report)
            .map(|s| s.topic)
            .collect();
        for q in self.queue.iter_mut() {
            if let Queued::Subscribe { topic, report, .. } = q {
                if *report {
                    *report = false;
                    failed.push(topic.clone());
                }
            }
        }
        failed
    }

    /// CONNACK (lock held, state just set to `Connected`): mqtt.js's order —
    /// the outgoingStore (`_onConnect`), then the `connect` listener's queue
    /// delivery and `_resubscribe` (`client.js:180-213,1132-1164`) — all of it
    /// ahead of the caller's own `connect` handler. (mqtt.js waits for each
    /// stored packet's PUBACK before the next and before `connect`; here they
    /// are pipelined in the same order.)
    fn on_connack(&mut self, client: &AsyncClient) {
        self.overflow_warned = false;
        for p in std::mem::take(&mut self.store) {
            if let Err(e) = self.send_now(client, p) {
                warn!("[mqtt] stored publish not resent: {e}");
            }
        }
        for q in std::mem::take(&mut self.queue) {
            match q {
                Queued::Publish(p) => {
                    if let Err(e) = self.send_now(client, p) {
                        warn!("[mqtt] queued publish not sent: {e}");
                    }
                }
                Queued::Subscribe { topic, qos, report } => {
                    match client.try_subscribe(topic.clone(), qos) {
                        Ok(()) => self.sub_unsent.push_back(SubTrack { topic, report }),
                        Err(e) => warn!("[mqtt] queued subscribe ({topic}) not sent: {e}"),
                    }
                }
            }
        }
        if self.connected_before {
            for (topic, qos) in self.resubscribe.clone() {
                match client.try_subscribe(topic.clone(), qos) {
                    Ok(()) => self.sub_unsent.push_back(SubTrack {
                        topic,
                        report: false,
                    }),
                    Err(e) => warn!("[mqtt] resubscribe ({topic}) not sent: {e}"),
                }
            }
        }
        self.connected_before = true;
    }

    /// Graceful stop (mqtt.js `end()` → `_flush`): every waiter fails, every
    /// queue is dropped.
    fn fail_all(&mut self, reason: &str) {
        let all: Vec<Pending> = self
            .unsent
            .drain(..)
            .chain(self.inflight.drain().map(|(_, p)| p))
            .chain(self.store.drain(..))
            .collect();
        for p in all {
            p.fail(reason);
        }
        for q in self.queue.drain(..) {
            if let Queued::Publish(p) = q {
                p.fail(reason);
            }
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

    /// `client.publish` (`client.js:375-436` → `_sendPacket` `:1024-1066`):
    /// link up → straight to rumqttc; link down → the offline store/queue.
    /// The state is read under the tracker lock (review-wp1 #2).
    fn enqueue(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Bytes,
        waiter: Waiter,
    ) -> Result<(), PublishError> {
        let mut t = lock_tracker(&self.tracker);
        let p = t.pending(topic, qos, retain, payload, waiter);
        let up = *self.state.borrow() == ConnectionState::Connected;
        if up {
            t.send_now(&self.client, p)
        } else {
            t.store_offline(p)
        }
    }

    /// Fire-and-forget publish (`client.publish` without a callback). While
    /// disconnected it is kept (QoS ≥ 1 in the store, QoS 0 per
    /// [`QUEUE_QOS_ZERO`]) and sent after reconnect; errors only when the
    /// client refused it.
    pub fn publish(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Vec<u8>,
    ) -> Result<(), PublishError> {
        self.enqueue(topic, qos, retain, payload.into(), None)
    }

    /// `client.publish(topic, payload, { qos }, callback)`: queued exactly like
    /// [`publish`](Self::publish) (synchronously, so call order is wire order);
    /// the receiver is the callback — `Acked` once written (QoS 0) or PUBACKed
    /// (QoS ≥ 1, after any resend), `Failed` when refused or ended by
    /// shutdown. It closes without a value when a QoS 0 publish is discarded
    /// (written into a link that dropped, or pushed out of a full offline queue).
    pub fn publish_tracked(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Vec<u8>,
    ) -> oneshot::Receiver<AckOutcome> {
        let (tx, rx) = oneshot::channel();
        // A refusal resolves the waiter itself.
        let _ = self.enqueue(topic, qos, retain, payload.into(), Some(tx));
        rx
    }

    /// QoS 1 publish that resolves on its PUBACK, or after `timeout`. While
    /// disconnected it is stored and sent after reconnect; an in-flight
    /// publish whose link dropped is resent; either way the PUBACK of the
    /// (re)sent packet resolves it. A timeout only stops the WAIT — the
    /// publish stays queued, exactly as mqtt.js keeps a packet whose caller
    /// gave up (`publishCommandAck`'s `setTimeout(done, …)`, `worker.ts:953-974`).
    pub async fn publish_acked(
        &self,
        topic: &str,
        retain: bool,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> AckOutcome {
        let rx = self.publish_tracked(topic, QoS::AtLeastOnce, retain, payload);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => AckOutcome::Failed("mqtt client gone".to_string()),
            Err(_) => AckOutcome::TimedOut,
        }
    }

    /// `client.subscribe` (`client.js:467-590`): recorded for the automatic
    /// resubscribe on every reconnect (`resubscribe` default, clean start); a
    /// topic already recorded at this QoS is a no-op (`parseSub`); queued
    /// while disconnected. A failure arrives as [`MqttEvent::SubscribeFailed`].
    pub fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), PublishError> {
        let mut t = lock_tracker(&self.tracker);
        if !t.remember_subscription(topic, qos) {
            return Ok(());
        }
        let up = *self.state.borrow() == ConnectionState::Connected;
        if !up {
            t.queue_offline(Queued::Subscribe {
                topic: topic.to_string(),
                qos,
                report: true,
            });
            return Ok(());
        }
        self.client
            .try_subscribe(topic, qos)
            .map_err(|e| PublishError(e.to_string()))?;
        t.sub_unsent.push_back(SubTrack {
            topic: topic.to_string(),
            report: true,
        });
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

        let (client, eventloop) = AsyncClient::new(options, REQUEST_CHANNEL_CAP);
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

    /// Non-blocking publish (see [`MqttPublisher::publish`]).
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

/// The error text the worker writes (`lastError` / the `error` event): the
/// two failures mqtt.js raises itself carry its literal message —
/// `Keepalive timeout` (`client.js:1128`; rumqttc: a ping still unanswered at
/// the next ping, `StateError::AwaitPingResp`) and `connack timeout`
/// (`client.js:363`; rumqttc: the connect timeout) — everything else is
/// rumqttc's text (amb. #6, tolerance row).
pub fn error_text(e: &ConnectionError) -> String {
    match e {
        ConnectionError::MqttState(StateError::AwaitPingResp) => "Keepalive timeout".to_string(),
        ConnectionError::Timeout(_) => "connack timeout".to_string(),
        _ => e.to_string(),
    }
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

/// One event of the poll stream other than CONNACK (lock held): packet-id
/// bookkeeping, and the events it yields for the consumer.
fn on_loop_event(t: &mut Tracker, ev: Event, out: &mut Vec<MqttEvent>) {
    match ev {
        Event::Incoming(Packet::Publish(p)) => out.push(MqttEvent::Message(Message {
            topic: String::from_utf8_lossy(&p.topic).into_owned(),
            payload: p.payload.to_vec(),
            retain: p.retain,
            qos: p.qos,
        })),
        Event::Outgoing(Outgoing::Publish(pkid)) => t.sent(pkid),
        Event::Incoming(Packet::PubAck(ack)) => t.acked(ack.pkid),
        Event::Outgoing(Outgoing::Subscribe(pkid)) => t.subscribe_sent(pkid),
        Event::Incoming(Packet::SubAck(ack)) => {
            let Some(sub) = t.sub_inflight.remove(&ack.pkid) else {
                return;
            };
            for code in &ack.return_codes {
                if !matches!(code, SubscribeReasonCode::Success(_)) {
                    if sub.report {
                        out.push(MqttEvent::SubscribeFailed {
                            topic: sub.topic.clone(),
                            reason: format!("{code:?}"),
                        });
                    } else {
                        debug!("[mqtt] resubscribe {} refused: {code:?}", sub.topic);
                    }
                }
            }
        }
        // The DISCONNECT itself surfaces through the error it raises.
        _ => {}
    }
}

async fn run_loop(mut cx: LoopCtx) {
    let mut backoff = Backoff::new(cx.backoff.clone());
    let mut ever_connected = false;
    let events_tx = cx.events_tx.clone();
    let send = move |out: Vec<MqttEvent>| {
        for ev in out {
            // Unbounded: never drops (CF1). Errors only once the receiver is gone.
            let _ = events_tx.send(ev);
        }
    };
    loop {
        let polled = cx.eventloop.poll().await;
        let mut out = Vec::new();
        let mut down = false;
        match polled {
            Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                backoff.reset();
                ever_connected = true;
                {
                    let mut t = lock_tracker(&cx.tracker);
                    let _ = cx.state_tx.send(ConnectionState::Connected);
                    t.on_connack(&cx.client);
                    for (topic, qos) in &cx.subscriptions {
                        match cx.client.try_subscribe(topic.clone(), *qos) {
                            Ok(()) => t.sub_unsent.push_back(SubTrack {
                                topic: topic.clone(),
                                report: true,
                            }),
                            Err(e) => out.push(MqttEvent::SubscribeFailed {
                                topic: topic.clone(),
                                reason: e.to_string(),
                            }),
                        }
                    }
                }
                out.push(MqttEvent::Connected {
                    session_present: ack.session_present,
                });
            }
            Ok(Event::Outgoing(Outgoing::Disconnect)) if cx.cancel.is_cancelled() => {
                info!("[mqtt] disconnect sent — event loop ending");
                break;
            }
            Ok(ev) => on_loop_event(&mut lock_tracker(&cx.tracker), ev, &mut out),
            Err(e) => {
                down = true;
                {
                    let mut t = lock_tracker(&cx.tracker);
                    let _ = cx.state_tx.send(ConnectionState::Closed);
                    // review-wp1 #2: rumqttc's own clean() ran inside poll()
                    // BEFORE the state left `Connected`, so a publisher may
                    // have slipped a request into the channel since. Drain it
                    // now (state is off `Connected`, nothing new can enter),
                    // then drop everything rumqttc kept — a clean-start
                    // session discards it anyway — because the tracker holds
                    // every one of those publishes and re-queues them itself.
                    cx.eventloop.clean();
                    cx.eventloop.pending.clear();
                    // Events rumqttc decoded before the failure but never
                    // yielded (a batch read, or the packet that raised it):
                    // messages, acks and write records, in order.
                    let buffered: Vec<Event> = cx.eventloop.state.events.drain(..).collect();
                    for ev in buffered {
                        on_loop_event(&mut t, ev, &mut out);
                    }
                    for topic in t.on_close() {
                        out.push(MqttEvent::SubscribeFailed {
                            topic,
                            reason: CONNECTION_CLOSED.to_string(),
                        });
                    }
                }
                if let ConnectionError::MqttState(StateError::ServerDisconnect {
                    reason_code,
                    reason_string,
                }) = &e
                {
                    out.push(MqttEvent::ServerDisconnect {
                        reason: match reason_string {
                            Some(s) => format!("{reason_code:?}: {s}"),
                            None => format!("{reason_code:?}"),
                        },
                    });
                }
                out.push(MqttEvent::Disconnected {
                    reason: error_text(&e),
                    emits_error: emits_node_error(&e),
                });
            }
        }
        send(out);
        if !down {
            continue;
        }
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
        send(vec![MqttEvent::Reconnecting { attempt, delay }]);
        // Re-stamp the Last-Will so it carries THIS attempt's connect ts.
        if let Some(factory) = &cx.will {
            cx.eventloop.options.set_last_will(to_last_will(factory()));
        }
    }
    let mut t = lock_tracker(&cx.tracker);
    t.fail_all("client disconnecting");
    let _ = cx.state_tx.send(ConnectionState::Closed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publisher_less_pending(
        t: &mut Tracker,
        qos: QoS,
    ) -> (Pending, oneshot::Receiver<AckOutcome>) {
        let (tx, rx) = oneshot::channel();
        (t.pending("t", qos, false, Bytes::new(), Some(tx)), rx)
    }

    #[test]
    fn tracker_binds_in_fifo_order_and_ignores_retransmits() {
        let mut t = Tracker::default();
        let (a, mut a_rx) = publisher_less_pending(&mut t, QoS::AtLeastOnce);
        let (z, mut z_rx) = publisher_less_pending(&mut t, QoS::AtMostOnce);
        let (b, mut b_rx) = publisher_less_pending(&mut t, QoS::AtLeastOnce);
        t.unsent.extend([a, z, b]);
        t.sent(1);
        t.sent(1); // retransmission
        t.sent(0); // the QoS 0 telemetry in between: written
        assert_eq!(z_rx.try_recv().unwrap(), AckOutcome::Acked);
        t.sent(2);
        t.acked(2);
        assert_eq!(b_rx.try_recv().unwrap(), AckOutcome::Acked);
        assert!(a_rx.try_recv().is_err());
        t.fail_all("gone");
        assert_eq!(a_rx.try_recv().unwrap(), AckOutcome::Failed("gone".into()));
        assert!(t.inflight.is_empty() && t.unsent.is_empty());
    }

    /// review-wp1 I-b: a drop puts every unacknowledged QoS 1 publish back in
    /// the store in publish order, ahead of later offline ones; QoS 0 handed to
    /// rumqttc is discarded without completing; the waiters stay pending.
    #[test]
    fn close_requeues_unacked_qos1_in_order_and_discards_unwritten_qos0() {
        let mut t = Tracker::default();
        let (a, mut a_rx) = publisher_less_pending(&mut t, QoS::AtLeastOnce);
        let (b, mut b_rx) = publisher_less_pending(&mut t, QoS::AtLeastOnce);
        let (z, mut z_rx) = publisher_less_pending(&mut t, QoS::AtMostOnce);
        t.unsent.extend([a, b, z]);
        t.sent(7); // a written, awaiting its PUBACK; b and z still in the channel
        t.on_close();
        let (c, _c_rx) = publisher_less_pending(&mut t, QoS::AtLeastOnce);
        t.store_offline(c).unwrap();
        let seqs: Vec<u64> = t.store.iter().map(|p| p.seq).collect();
        assert_eq!(seqs, [1, 2, 4]);
        assert!(t.unsent.is_empty() && t.inflight.is_empty());
        assert!(matches!(
            z_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(
            a_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            b_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }

    /// `queueQoSZero: true` — QoS 0 is queued offline; the oldest is pushed
    /// out (never completing) past the bound; QoS 1 overflow fails its waiter.
    #[test]
    fn offline_queues_are_bounded() {
        let mut t = Tracker::default();
        let (first, mut first_rx) = publisher_less_pending(&mut t, QoS::AtMostOnce);
        t.store_offline(first).unwrap();
        for _ in 0..OFFLINE_QUEUE_LIMIT {
            let p = t.pending("t", QoS::AtMostOnce, false, Bytes::new(), None);
            t.store_offline(p).unwrap();
        }
        assert_eq!(t.queue.len(), OFFLINE_QUEUE_LIMIT);
        assert!(matches!(
            first_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        let (old, mut old_rx) = publisher_less_pending(&mut t, QoS::AtLeastOnce);
        t.store_offline(old).unwrap();
        for _ in 0..OFFLINE_QUEUE_LIMIT {
            let p = t.pending("t", QoS::AtLeastOnce, false, Bytes::new(), None);
            t.store_offline(p).unwrap();
        }
        assert_eq!(t.store.len(), OFFLINE_QUEUE_LIMIT);
        assert_eq!(
            old_rx.try_recv().unwrap(),
            AckOutcome::Failed("offline queue full".into())
        );
    }

    /// `_flushVolatile` (`client.js:794-803`): a subscribe awaiting its SUBACK
    /// or still queued fails once with `Connection closed`; the resubscribe
    /// never reports.
    #[test]
    fn close_fails_pending_subscribes_once() {
        let mut t = Tracker::default();
        t.sub_inflight.insert(
            2,
            SubTrack {
                topic: "cmd".into(),
                report: true,
            },
        );
        t.sub_inflight.insert(
            1,
            SubTrack {
                topic: "cfg".into(),
                report: true,
            },
        );
        t.sub_unsent.push_back(SubTrack {
            topic: "re".into(),
            report: false,
        });
        t.queue.push_back(Queued::Subscribe {
            topic: "q".into(),
            qos: QoS::AtLeastOnce,
            report: true,
        });
        assert_eq!(t.on_close(), ["cfg", "cmd", "q"]);
        assert!(t.on_close().is_empty());
        assert_eq!(t.queue.len(), 1, "the queued subscribe is still sent later");
    }

    #[test]
    fn subscription_is_remembered_once_per_qos() {
        let mut t = Tracker::default();
        assert!(t.remember_subscription("a", QoS::AtMostOnce));
        assert!(!t.remember_subscription("a", QoS::AtMostOnce));
        assert!(t.remember_subscription("a", QoS::AtLeastOnce));
        assert!(!t.remember_subscription("a", QoS::AtMostOnce));
        assert_eq!(t.resubscribe, [("a".to_string(), QoS::AtLeastOnce)]);
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

    /// review-wp1 #3: mqtt.js's own error texts (`client.js:363,1128`).
    #[test]
    fn keepalive_and_connack_timeouts_use_the_mqtt_js_text() {
        let ka = ConnectionError::MqttState(StateError::AwaitPingResp);
        assert_eq!(error_text(&ka), "Keepalive timeout");
        assert!(emits_node_error(&ka));
        let io = ConnectionError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert_eq!(error_text(&io), io.to_string());
    }

    fn assert_send<T: Send>(_: &T) {}

    /// WP4: the publish futures must be `Send` (they run inside spawned tasks).
    #[allow(dead_code)]
    fn publish_futures_are_send(p: &MqttPublisher) {
        assert_send(&p.publish_acked("t", false, vec![], Duration::ZERO));
        assert_send(&p.publish_tracked("t", QoS::AtMostOnce, false, vec![]));
    }
}
