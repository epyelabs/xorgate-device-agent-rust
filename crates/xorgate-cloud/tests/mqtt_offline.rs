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

#[path = "support/stub_broker.rs"]
mod stub_broker;

use std::sync::atomic::Ordering;
use std::time::Duration;

use rumqttc::v5::mqttbytes::v5::DisconnectReasonCode;
use stub_broker::*;
use xorgate_cloud::mqtt::{AckOutcome, MqttEvent, MqttPublisher, QoS};

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

/// review-wp1-followup #2: the broker side vanishes WITHOUT TLS close_notify
/// (the stub's `Kill` drops the stream: FIN, no alert). Node 20/24 + mqtt.js
/// emit `close`/`end` only (`integration-tools/fin-probe.mjs`), so no `error`.
#[tokio::test]
async fn tls_fin_without_close_notify_is_not_an_error() {
    let b = broker().await;
    let (_p, mut events, end) = client(b.port, 60).split();
    connected(&mut events).await;
    b.cmd(1, Cmd::Kill);
    match next(&mut events).await {
        MqttEvent::Disconnected {
            reason,
            emits_error,
        } => {
            assert!(reason.contains("close_notify"), "{reason}");
            assert!(!emits_error, "{reason}");
        }
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
