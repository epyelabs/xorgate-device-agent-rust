//! The real client (`xorgate_cloud::mqtt`) against a local MQTT 5 stub broker
//! over mTLS (`fixtures/s3/tls/`, test-only PKI), on real time:
//! - mqtt.js offline semantics (review-wp1 I-b, `client.js:1068-1096,1165-1255`):
//!   an unacknowledged QoS 1 publish is resent after the reconnect and its
//!   `publish_acked` waiter resolves on the RESENT packet's PUBACK; publishes
//!   made while down are sent after the CONNACK — the store (QoS 1) first,
//!   then the queue (QoS 0, `queueQoSZero`) — and a tracked QoS 0 completes then;
//! - the ack-tracker race (review-wp1 #2): publishes racing a link drop never
//!   bind to the wrong PUBACK;
//! - a broker DISCONNECT surfaces before the close (#7);
//! - an unanswered ping ends as `Keepalive timeout` (#3).

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
use xorgate_cloud::mqtt::{AckOutcome, MqttClient, MqttConfig, MqttEvent, MqttPublisher, QoS};

fn tls(name: &str) -> String {
    format!(
        "{}/../../fixtures/s3/tls/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// What the broker saw: (connection number, topic, qos).
type Seen = (usize, String, QoS);

/// Which QoS 1 topics the broker PUBACKs.
type AckFilter = fn(&str) -> bool;

enum Cmd {
    /// Drop the socket without a word (a dead link).
    Kill,
    /// Send DISCONNECT with this reason, then close.
    Disconnect(DisconnectReasonCode),
    /// Send the CONNACK held back by `hold_connack`.
    ConnAck,
}

#[derive(Default)]
struct Ctl {
    /// PUBACK QoS 1 publishes whose topic passes this test (None = all).
    ack_if: Mutex<Option<AckFilter>>,
    /// Withhold every PUBACK.
    no_ack: AtomicBool,
    /// Ignore PINGREQ.
    no_pong: AtomicBool,
    /// Wait for `Cmd::ConnAck` before answering CONNECT.
    hold_connack: AtomicBool,
    conns: Mutex<Vec<mpsc::UnboundedSender<Cmd>>>,
    /// Publishes received so far, all connections.
    publishes: AtomicUsize,
}

struct Broker {
    port: u16,
    ctl: Arc<Ctl>,
    seen: mpsc::UnboundedReceiver<Seen>,
}

impl Broker {
    fn cmd(&self, conn: usize, c: Cmd) {
        let tx = self.ctl.conns.lock().unwrap()[conn - 1].clone();
        let _ = tx.send(c);
    }

    async fn next_seen(&mut self) -> Seen {
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

async fn broker() -> Broker {
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

fn client(port: u16, keep_alive_s: u64) -> MqttClient {
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
async fn next(events: &mut mpsc::UnboundedReceiver<MqttEvent>) -> MqttEvent {
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

async fn connected(events: &mut mpsc::UnboundedReceiver<MqttEvent>) {
    let ev = next(events).await;
    assert!(matches!(ev, MqttEvent::Connected { .. }), "{ev:?}");
}

#[tokio::test]
async fn unacked_and_offline_publishes_are_resent_after_reconnect() {
    let mut b = broker().await;
    let (p, mut events, end) = client(b.port, 60).split();
    connected(&mut events).await;

    // Connected: a PUBACKed publish resolves `Acked`.
    let t = Duration::from_secs(10);
    assert_eq!(
        p.publish_acked("a/1", false, b"1".to_vec(), t).await,
        AckOutcome::Acked
    );
    assert_eq!(b.next_seen().await, (1, "a/1".into(), QoS::AtLeastOnce));

    // a/2 is written but never PUBACKed on connection 1, which then dies.
    b.ctl.no_ack.store(true, Ordering::SeqCst);
    b.ctl.hold_connack.store(true, Ordering::SeqCst);
    let p2 = p.clone();
    let waiter =
        tokio::spawn(async move { p2.publish_acked("a/2", false, b"2".to_vec(), t).await });
    assert_eq!(b.next_seen().await, (1, "a/2".into(), QoS::AtLeastOnce));
    b.cmd(1, Cmd::Kill);
    let ev = next(&mut events).await;
    assert!(matches!(ev, MqttEvent::Disconnected { .. }), "{ev:?}");

    // While down (connection 2 holds its CONNACK): a QoS 1 and a QoS 0 publish.
    while b.ctl.conns.lock().unwrap().len() < 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!p.is_connected());
    p.publish("a/3", QoS::AtLeastOnce, false, b"3".to_vec())
        .unwrap();
    let qos0 = p.publish_tracked("t/0", QoS::AtMostOnce, false, b"0".to_vec());
    b.ctl.no_ack.store(false, Ordering::SeqCst);
    b.cmd(2, Cmd::ConnAck);
    connected(&mut events).await;

    // The store (a/2 resent, then a/3) before the queue (t/0).
    assert_eq!(b.next_seen().await, (2, "a/2".into(), QoS::AtLeastOnce));
    assert_eq!(b.next_seen().await, (2, "a/3".into(), QoS::AtLeastOnce));
    assert_eq!(b.next_seen().await, (2, "t/0".into(), QoS::AtMostOnce));
    assert_eq!(waiter.await.unwrap(), AckOutcome::Acked);
    assert_eq!(qos0.await.unwrap(), AckOutcome::Acked);
    assert!(end.shutdown(Duration::from_secs(2)).await);
}

/// review-wp1 #2: publishes racing a link drop. The broker PUBACKs only even
/// topics; whatever the interleaving with the drop, an odd waiter must never
/// be resolved by an even PUBACK, and every even one must resolve `Acked`.
#[tokio::test]
async fn publishes_racing_a_drop_never_bind_to_the_wrong_ack() {
    let mut b = broker().await;
    *b.ctl.ack_if.lock().unwrap() = Some(|t: &str| {
        t.rsplit('/')
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| n % 2 == 0)
    });
    let (p, mut events, end) = client(b.port, 60).split();
    connected(&mut events).await;

    let t = Duration::from_secs(4);
    let mut tasks = Vec::new();
    for w in 0..4u32 {
        let p: MqttPublisher = p.clone();
        tasks.push(tokio::spawn(async move {
            let mut out = Vec::new();
            for i in 0..40u32 {
                let n = w * 1000 + i;
                let p = p.clone();
                out.push((
                    n,
                    tokio::spawn(async move {
                        p.publish_acked(&format!("r/{n}"), false, vec![], t).await
                    }),
                ));
                tokio::task::yield_now().await;
            }
            out
        }));
    }
    // Kill the link once some publishes have landed, while others still race in.
    while b.ctl.publishes.load(Ordering::SeqCst) < 30 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    b.cmd(1, Cmd::Kill);

    let mut checked = 0;
    for task in tasks {
        for (n, h) in task.await.unwrap() {
            let outcome = h.await.unwrap();
            if n % 2 == 0 {
                assert_eq!(outcome, AckOutcome::Acked, "r/{n}");
            } else {
                assert_eq!(outcome, AckOutcome::TimedOut, "r/{n}");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 160);
    while b.seen.try_recv().is_ok() {}
    assert!(end.shutdown(Duration::from_secs(2)).await);
}

/// review-wp1 #7: a broker DISCONNECT is surfaced, then the close (no error).
#[tokio::test]
async fn server_disconnect_is_surfaced_before_the_close() {
    let b = broker().await;
    let (_p, mut events, end) = client(b.port, 60).split();
    connected(&mut events).await;
    b.cmd(1, Cmd::Disconnect(DisconnectReasonCode::SessionTakenOver));
    match next(&mut events).await {
        MqttEvent::ServerDisconnect { reason } => assert_eq!(reason, "SessionTakenOver"),
        ev => panic!("{ev:?}"),
    }
    match next(&mut events).await {
        MqttEvent::Disconnected { emits_error, .. } => assert!(!emits_error),
        ev => panic!("{ev:?}"),
    }
    connected(&mut events).await; // and it reconnects
    assert!(end.shutdown(Duration::from_secs(2)).await);
}

/// review-wp1 #3: a ping left unanswered ends the link as mqtt.js's
/// `Keepalive timeout`, with the `error` event (keepalive 5 s: ~10 s).
#[tokio::test]
async fn unanswered_ping_is_a_keepalive_timeout() {
    let b = broker().await;
    b.ctl.no_pong.store(true, Ordering::SeqCst);
    let (_p, mut events, end) = client(b.port, 5).split();
    connected(&mut events).await;
    match next(&mut events).await {
        MqttEvent::Disconnected {
            reason,
            emits_error,
        } => {
            assert_eq!(reason, "Keepalive timeout");
            assert!(emits_error);
        }
        ev => panic!("{ev:?}"),
    }
    assert!(end.shutdown(Duration::from_secs(2)).await);
}
