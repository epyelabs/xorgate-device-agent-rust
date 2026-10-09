//! AWS IoT Core MQTT v5 client wrapper (rumqttc 0.25 `v5` + rustls).
//!
//! Port of the connection contract in `src/agent/mqtt.ts` + the connect /
//! reconnect / close handling of `src/worker.ts`:
//! - `clientId` = deviceId (the IoT policy scopes `iot:Connect` to the thing name);
//! - mTLS on `IOT_DATA_ENDPOINT:8883`, MQTT 5, clean start, keepalive
//!   `MQTT_KEEPALIVE_SEC` (default 60), connect timeout 30 s;
//! - Last-Will on `…/status` with the CONNECT-time `ts` (re-stamped before
//!   every connect attempt), QoS 0, not retained;
//! - `set_max_packet_size(Some(150_000))` (rumqttc #1018: the 10 KiB default
//!   would reset the connection on a large retained config);
//! - subscriptions re-made on every CONNACK (clean session);
//! - exponential backoff with jitter between attempts (ours — rumqttc has none);
//! - a `watch` channel exposing the connection state, an event channel for
//!   messages/connect/disconnect, publish helpers (QoS 0 telemetry/status,
//!   QoS 1 reports), and a bounded graceful shutdown that flushes the final
//!   publish before the DISCONNECT.

use std::sync::Arc;
use std::time::Duration;

use rumqttc::v5::mqttbytes::v5::{LastWill, Packet, SubscribeReasonCode};
use rumqttc::v5::{AsyncClient, Event, EventLoop, MqttOptions};
use rumqttc::{Outgoing, TlsConfiguration, Transport};
use tokio::sync::{mpsc, watch};
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
    /// CONNACK received; subscriptions have been (re)requested.
    Connected {
        session_present: bool,
    },
    /// The link dropped or a connect attempt failed (`reason` is the error text).
    Disconnected {
        reason: String,
    },
    /// About to retry after `delay` (attempt number is 1-based).
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

/// A publish that could not even be queued (request channel full or the
/// event loop gone). rumqttc's `ClientError` carries the whole request, so it
/// is reduced to its message here.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct PublishError(pub String);

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
    /// (topic, qos) re-subscribed on every CONNACK.
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

/// Handle to a running connection. Dropping it does NOT disconnect — call
/// [`MqttClient::shutdown`] for the graceful path.
pub struct MqttClient {
    client: AsyncClient,
    state: watch::Receiver<ConnectionState>,
    events: mpsc::Receiver<MqttEvent>,
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
        let (events_tx, events_rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_loop(
            eventloop,
            client.clone(),
            cfg.subscriptions,
            cfg.backoff,
            cfg.will,
            state_tx,
            events_tx,
            cancel.clone(),
        ));
        Self {
            client,
            state: state_rx,
            events: events_rx,
            cancel,
            task,
        }
    }

    pub fn state(&self) -> ConnectionState {
        *self.state.borrow()
    }

    pub fn state_watch(&self) -> watch::Receiver<ConnectionState> {
        self.state.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.state() == ConnectionState::Connected
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
        self.client
            .try_publish(topic, qos, retain, payload)
            .map_err(|e| PublishError(e.to_string()))
    }

    /// Publish, waiting for request-channel space.
    pub async fn publish(
        &self,
        topic: &str,
        qos: QoS,
        retain: bool,
        payload: Vec<u8>,
    ) -> Result<(), PublishError> {
        self.client
            .publish(topic, qos, retain, payload)
            .await
            .map_err(|e| PublishError(e.to_string()))
    }

    /// Graceful stop: queue DISCONNECT behind whatever is already queued
    /// (e.g. the final `online:false` status), then wait up to `wait` for the
    /// loop to see it on the wire. Returns `true` when the loop ended cleanly
    /// within the budget, `false` when it had to be aborted.
    pub async fn shutdown(self, wait: Duration) -> bool {
        self.cancel.cancel();
        if let Err(e) = self.client.try_disconnect() {
            debug!("[mqtt] disconnect request not queued: {e}");
        }
        tokio::time::timeout(wait, self.task).await.is_ok()
    }
}

fn to_last_will(w: Will) -> LastWill {
    LastWill::new(w.topic, w.payload, w.qos, w.retain, None)
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    mut eventloop: EventLoop,
    client: AsyncClient,
    subscriptions: Vec<(String, QoS)>,
    backoff_cfg: BackoffConfig,
    will: Option<WillFactory>,
    state_tx: watch::Sender<ConnectionState>,
    events_tx: mpsc::Sender<MqttEvent>,
    cancel: CancellationToken,
) {
    let mut backoff = Backoff::new(backoff_cfg);
    let mut ever_connected = false;
    let send = |ev: MqttEvent| {
        if let Err(e) = events_tx.try_send(ev) {
            warn!("[mqtt] event dropped (consumer lagging): {e}");
        }
    };
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                backoff.reset();
                ever_connected = true;
                let _ = state_tx.send(ConnectionState::Connected);
                for (topic, qos) in &subscriptions {
                    if let Err(e) = client.subscribe(topic.clone(), *qos).await {
                        send(MqttEvent::SubscribeFailed {
                            topic: topic.clone(),
                            reason: e.to_string(),
                        });
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
            Ok(Event::Incoming(Packet::SubAck(ack))) => {
                for (i, code) in ack.return_codes.iter().enumerate() {
                    if !matches!(code, SubscribeReasonCode::Success(_)) {
                        let topic = subscriptions
                            .get(i)
                            .map(|s| s.0.clone())
                            .unwrap_or_default();
                        send(MqttEvent::SubscribeFailed {
                            topic,
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
                if cancel.is_cancelled() {
                    info!("[mqtt] disconnect sent — event loop ending");
                    let _ = state_tx.send(ConnectionState::Closed);
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => {
                let reason = e.to_string();
                let _ = state_tx.send(ConnectionState::Closed);
                send(MqttEvent::Disconnected {
                    reason: reason.clone(),
                });
                if cancel.is_cancelled() {
                    break;
                }
                let delay = backoff.next_delay();
                let attempt = backoff.attempt();
                send(MqttEvent::Reconnecting { attempt, delay });
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {}
                }
                let _ = state_tx.send(if ever_connected {
                    ConnectionState::Reconnecting
                } else {
                    ConnectionState::Connecting
                });
                // Re-stamp the Last-Will so it carries THIS attempt's connect ts.
                if let Some(factory) = &will {
                    eventloop.options.set_last_will(to_last_will(factory()));
                }
            }
        }
    }
    let _ = state_tx.send(ConnectionState::Closed);
}
