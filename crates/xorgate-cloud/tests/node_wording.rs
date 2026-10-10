//! CF32 (review-wp5 MUST-FIX 1): non-HTTP S3 / IoT-credentials failures
//! carry node:https's `err.message` text, because it reaches the state.json
//! event `upload failed, retrying in Ns: <msg>` (`uploader.ts:204`). Expected
//! strings were printed by node v20.20.2 for the same failure classes
//! (phase-3/evidence/wp3.md, probe in node:20-bookworm).

use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use xorgate_cloud::iot_creds::{IotCredentialsOptions, IotCredentialsProvider};
use xorgate_cloud::s3::{https_client, put_signed};
use xorgate_cloud::sigv4::{sign_put, SigningCredentials, SignedRequest};

fn signed(body: &[u8]) -> SignedRequest {
    sign_put(
        "us-east-1",
        "xorgate-dev-recordings",
        "telemetry/v1/dev/019a/1-00000.jsonl.gz",
        body,
        "application/gzip",
        SigningCredentials {
            access_key_id: "ASIATEST",
            secret_access_key: "sekrit",
            session_token: "tok",
        },
        1_791_504_000_000.0,
    )
}

async fn put_to(base: &str) -> String {
    let body = Bytes::from_static(b"payload");
    let s = signed(&body);
    let client = https_client().unwrap();
    put_signed(&client, base, &s, body, Duration::from_secs(10), None)
        .await
        .unwrap_err()
        .0
}

/// Read one request head (+ a short body) off `s`.
async fn read_request(s: &mut tokio::net::TcpStream) {
    let mut buf = vec![0u8; 8192];
    let mut got = Vec::new();
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut buf).await.unwrap();
        if n == 0 {
            return;
        }
        got.extend_from_slice(&buf[..n]);
    }
    // The 7-byte body follows the head (possibly already in `got`).
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn refused_connect_is_connect_econnrefused_ip_port() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    assert_eq!(
        put_to(&format!("http://127.0.0.1:{port}")).await,
        format!("connect ECONNREFUSED 127.0.0.1:{port}")
    );
}

#[tokio::test]
async fn server_close_without_a_response_is_socket_hang_up() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_request(&mut s).await;
        let _ = s.shutdown().await;
    });
    assert_eq!(put_to(&format!("http://127.0.0.1:{port}")).await, "socket hang up");
}

#[tokio::test]
async fn reset_after_the_request_is_read_econnreset() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_request(&mut s).await;
        s.set_zero_linger().unwrap();
        drop(s); // RST
    });
    assert_eq!(put_to(&format!("http://127.0.0.1:{port}")).await, "read ECONNRESET");
}

#[tokio::test]
async fn tcp_close_during_the_tls_handshake_is_nodes_disconnect_text() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let _ = s.shutdown().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    assert_eq!(
        put_to(&format!("https://127.0.0.1:{port}")).await,
        "Client network socket disconnected before secure TLS connection was established"
    );
}

#[tokio::test]
async fn unresolvable_host_is_getaddrinfo_with_the_host() {
    let got = put_to("http://nonexistent-host.invalid").await;
    // NXDOMAIN on a networked host; EAI_AGAIN in a sandbox without DNS.
    assert!(
        got == "getaddrinfo ENOTFOUND nonexistent-host.invalid"
            || got == "getaddrinfo EAI_AGAIN nonexistent-host.invalid",
        "{got}"
    );
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("xg-cloud-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tls(name: &str) -> String {
    format!("{}/../../fixtures/s3/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

async fn creds_err(cert: &std::path::Path, key: &std::path::Path) -> String {
    let p = IotCredentialsProvider::new(IotCredentialsOptions::new(
        "127.0.0.1:1",
        "alias",
        "thing",
        cert,
        key,
    ));
    p.get().await.unwrap_err().0
}

#[tokio::test]
async fn pem_failures_are_openssl3_texts_cert_first() {
    let d = scratch("pem");
    let garbage = d.join("garbage");
    std::fs::write(&garbage, "garbage").unwrap();
    let empty = d.join("empty");
    std::fs::write(&empty, "").unwrap();
    let cert = std::path::PathBuf::from(tls("client.pem"));
    let key = std::path::PathBuf::from(tls("client.key"));
    const NO_START: &str = "error:0480006C:PEM routines::no start line";
    const UNSUPPORTED: &str = "error:1E08010C:DECODER routines::unsupported";
    assert_eq!(creds_err(&garbage, &key).await, NO_START);
    assert_eq!(creds_err(&empty, &key).await, NO_START);
    assert_eq!(creds_err(&key, &key).await, NO_START); // a key where the cert goes
    assert_eq!(creds_err(&garbage, &garbage).await, NO_START); // cert checked first
    assert_eq!(creds_err(&cert, &garbage).await, UNSUPPORTED);
    assert_eq!(creds_err(&cert, &empty).await, UNSUPPORTED);
    assert_eq!(creds_err(&cert, &cert).await, UNSUPPORTED); // a cert where the key goes
    let _ = std::fs::remove_dir_all(&d);
}
