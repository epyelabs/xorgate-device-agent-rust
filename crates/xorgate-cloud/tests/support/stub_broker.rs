//! A local MQTT 5 stub broker over mTLS (`fixtures/s3/tls/`, test-only PKI)
//! for driving the REAL client (`xorgate_cloud::mqtt`): per-connection kill /
//! DISCONNECT / held CONNACK, selective PUBACKs, ignored pings. Shared by
//! `xorgate-cloud/tests/mqtt_offline.rs` and the agent's `tests/commands.rs`
//! (`#[path]` include), on real time (sockets + paused tokio time don't mix).

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::BytesMut;
use rumqttc::v5::mqttbytes::v5::{
    ConnAck, ConnectReturnCode, DisconnectReasonCode, Packet, PingResp, PubAck, SubAck,
    SubscribeReasonCode,
};
use rumqttc::v5::mqttbytes::Error as WireError;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use xorgate_cloud::backoff::BackoffConfig;
use xorgate_cloud::mqtt::{MqttClient, MqttConfig, MqttEvent, QoS};

pub fn tls(name: &str) -> String {
    // Both crates sit at `crates/<name>`, so the fixture path is the same.
    format!(
        "{}/../../fixtures/s3/tls/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// What the broker saw: (connection number, topic, qos).
pub type Seen = (usize, String, QoS);

/// Which QoS 1 topics the broker PUBACKs.
pub type AckFilter = fn(&str) -> bool;

pub enum Cmd {
    /// Drop the socket without a word (a dead link).
    Kill,
    /// Send DISCONNECT with this reason, then close.
    Disconnect(DisconnectReasonCode),
    /// Send the CONNACK held back by `hold_connack`.
    ConnAck,
}

#[derive(Default)]
pub struct Ctl {
    /// PUBACK QoS 1 publishes whose topic passes this test (None = all).
    pub ack_if: Mutex<Option<AckFilter>>,
    /// Withhold every PUBACK.
    pub no_ack: AtomicBool,
    /// Ignore PINGREQ.
    pub no_pong: AtomicBool,
    /// Wait for `Cmd::ConnAck` before answering CONNECT.
    pub hold_connack: AtomicBool,
    pub conns: Mutex<Vec<mpsc::UnboundedSender<Cmd>>>,
    /// Publishes received so far, all connections.
    pub publishes: AtomicUsize,
}

pub struct Broker {
    pub port: u16,
    pub ctl: Arc<Ctl>,
    pub seen: mpsc::UnboundedReceiver<Seen>,
}

impl Broker {
    pub fn cmd(&self, conn: usize, c: Cmd) {
        let tx = self.ctl.conns.lock().unwrap()[conn - 1].clone();
        let _ = tx.send(c);
    }

    pub async fn next_seen(&mut self) -> Seen {
        tokio::time::timeout(Duration::from_secs(10), self.seen.recv())
            .await
            .expect("broker saw nothing within 10 s")
            .expect("broker gone")
    }
}

async fn write(w: &mut (impl AsyncWriteExt + Unpin), p: Packet) {
    let mut out = BytesMut::new();
    p.write(&mut out, None).unwrap();
    let _ = w.write_all(&out).await;
    let _ = w.flush().await;
}

pub async fn broker() -> Broker {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(tls("ca.pem")).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .unwrap();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(tls("server.pem"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let key = PrivateKeyDer::from_pem_file(tls("server.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let ctl = Arc::new(Ctl::default());
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let c = ctl.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let n = {
                let mut conns = c.conns.lock().unwrap();
                conns.push(cmd_tx);
                conns.len()
            };
            let (acceptor, ctl, seen_tx) = (acceptor.clone(), c.clone(), seen_tx.clone());
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(tcp).await else {
                    return;
                };
                serve(n, stream, ctl, seen_tx, cmd_rx).await;
            });
        }
    });
    Broker { port, ctl, seen }
}

async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    n: usize,
    stream: S,
    ctl: Arc<Ctl>,
    seen: mpsc::UnboundedSender<Seen>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
) {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut buf = BytesMut::new();
    let connack = || {
        Packet::ConnAck(ConnAck {
            session_present: false,
            code: ConnectReturnCode::Success,
            properties: None,
        })
    };
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::ConnAck) => write(&mut wr, connack()).await,
                Some(Cmd::Disconnect(code)) => {
                    // Hand-encoded: reason code + an empty property block. (rumqttc's
                    // own encoder writes one byte more than its remaining length,
                    // and its decoder rejects the spec-legal reason-only form.)
                    let _ = wr.write_all(&[0xE0, 0x02, code as u8, 0x00]).await;
                    let _ = wr.flush().await;
                    // Then close the way a broker does (TLS close_notify, FIN).
                    let _ = wr.shutdown().await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    return;
                }
                Some(Cmd::Kill) | None => return,
            },
            r = rd.read_buf(&mut buf) => {
                if !matches!(r, Ok(k) if k > 0) {
                    return;
                }
                loop {
                    let p = match Packet::read(&mut buf, None) {
                        Ok(p) => p,
                        Err(WireError::InsufficientBytes(_)) => break,
                        Err(_) => return,
                    };
                    match p {
                        Packet::Connect(..) => {
                            if !ctl.hold_connack.load(Ordering::SeqCst) {
                                write(&mut wr, connack()).await;
                            }
                        }
                        Packet::Publish(p) => {
                            let topic = String::from_utf8_lossy(&p.topic).into_owned();
                            ctl.publishes.fetch_add(1, Ordering::SeqCst);
                            let ack = p.qos == QoS::AtLeastOnce
                                && !ctl.no_ack.load(Ordering::SeqCst)
                                && ctl.ack_if.lock().unwrap().is_none_or(|f| f(&topic));
                            let _ = seen.send((n, topic, p.qos));
                            if ack {
                                write(&mut wr, Packet::PubAck(PubAck::new(p.pkid, None))).await;
                            }
                        }
                        Packet::Subscribe(s) => {
                            let codes = s
                                .filters
                                .iter()
                                .map(|_| SubscribeReasonCode::Success(QoS::AtLeastOnce))
                                .collect();
                            write(
                                &mut wr,
                                Packet::SubAck(SubAck {
                                    pkid: s.pkid,
                                    return_codes: codes,
                                    properties: None,
                                }),
                            )
                            .await;
                        }
                        Packet::PingReq(_) => {
                            if !ctl.no_pong.load(Ordering::SeqCst) {
                                write(&mut wr, Packet::PingResp(PingResp)).await;
                            }
                        }
                        Packet::Disconnect(_) => return,
                        _ => {}
                    }
                }
            }
        }
    }
}

pub fn client(port: u16, keep_alive_s: u64) -> MqttClient {
    let read = |n: &str| std::fs::read(tls(n)).unwrap();
    let tls = xorgate_cloud::tls::client_config(
        Some(&read("ca.pem")),
        &read("client.pem"),
        &read("client.key"),
    )
    .unwrap();
    let mut cfg = MqttConfig::new("dev-test", "localhost", tls);
    cfg.port = port;
    cfg.keep_alive = Duration::from_secs(keep_alive_s);
    cfg.connect_timeout = Duration::from_secs(5);
    cfg.backoff = BackoffConfig {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
        factor: 2.0,
        jitter: false,
    };
    MqttClient::connect(cfg)
}

/// The next event that is not a `Reconnecting` (those depend on timing).
pub async fn next(events: &mut mpsc::UnboundedReceiver<MqttEvent>) -> MqttEvent {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(15), events.recv())
            .await
            .expect("no MQTT event within 15 s")
            .expect("event stream ended");
        if !matches!(ev, MqttEvent::Reconnecting { .. }) {
            return ev;
        }
    }
}

pub async fn connected(events: &mut mpsc::UnboundedReceiver<MqttEvent>) {
    let ev = next(events).await;
    assert!(matches!(ev, MqttEvent::Connected { .. }), "{ev:?}");
}
