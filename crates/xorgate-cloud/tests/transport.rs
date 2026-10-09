//! Transport tests for WP5 against local servers (no network):
//! - `IotCredentialsProvider` (port of `test/recording-s3.test.ts`
//!   "caches until near expiry, then refreshes (shared in-flight fetch)");
//! - the real mTLS GET against a local rustls server that REQUIRES a client
//!   cert (`fixtures/s3/tls/`, test-only PKI): path, thing-name header, SNI,
//!   200 / 403 / stall / no-client-cert;
//! - the S3 PUT request half (`put_signed`) against a plain-HTTP server:
//!   headers as signed, un-chunked body, 200 / 403 / idle timeout / abort.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use xorgate_cloud::iot_creds::{
    AwsCredentials, CredsError, IotCredentialsOptions, IotCredentialsProvider,
};
use xorgate_cloud::s3::{https_client, put_signed};
use xorgate_cloud::sigv4::{sign_put, SigningCredentials};

fn tls(name: &str) -> String {
    format!(
        "{}/../../fixtures/s3/tls/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------------------
// recording-s3.test.ts › IotCredentialsProvider

#[tokio::test]
async fn caches_until_near_expiry_then_refreshes_shared_inflight_fetch() {
    let fetches = Arc::new(AtomicU32::new(0));
    let now = Arc::new(Mutex::new(0.0_f64));
    let mut opts =
        IotCredentialsOptions::new("example.invalid", "alias", "thing", "/nope", "/nope");
    opts.refresh_skew_ms = Some(1000.0);
    let n = now.clone();
    opts.now = Some(Arc::new(move || *n.lock().unwrap()));
    let (f, n) = (fetches.clone(), now.clone());
    opts.fetch_fn = Some(Arc::new(move || {
        let (f, n) = (f.clone(), n.clone());
        Box::pin(async move {
            f.fetch_add(1, Ordering::SeqCst);
            // Yield so the second concurrent caller joins the in-flight fetch.
            tokio::task::yield_now().await;
            Ok(AwsCredentials {
                access_key_id: "AKIDEXAMPLE".into(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
                session_token: "TOKEN".into(),
                expiration: *n.lock().unwrap() + 10_000.0,
            })
        })
    }));
    let provider = IotCredentialsProvider::new(opts);

    let (a, b) = tokio::join!(provider.get(), provider.get());
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "concurrent callers share one fetch"
    );
    assert_eq!(a.unwrap(), b.unwrap());

    *now.lock().unwrap() = 5_000.0;
    provider.get().await.unwrap();
    assert_eq!(fetches.load(Ordering::SeqCst), 1, "still fresh");

    *now.lock().unwrap() = 9_500.0; // within the 1 s refresh skew
    provider.get().await.unwrap();
    assert_eq!(fetches.load(Ordering::SeqCst), 2);

    provider.invalidate();
    provider.get().await.unwrap();
    assert_eq!(fetches.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_failed_fetch_rejects_every_waiter_and_is_not_cached() {
    let fetches = Arc::new(AtomicU32::new(0));
    let mut opts =
        IotCredentialsOptions::new("example.invalid", "alias", "thing", "/nope", "/nope");
    let f = fetches.clone();
    opts.fetch_fn = Some(Arc::new(move || {
        let f = f.clone();
        Box::pin(async move {
            f.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Err(CredsError(
                "IoT credentials fetch failed: HTTP 403 nope".into(),
            ))
        })
    }));
    let provider = IotCredentialsProvider::new(opts);
    let (a, b) = tokio::join!(provider.get(), provider.get());
    assert_eq!(
        a.unwrap_err().0,
        "IoT credentials fetch failed: HTTP 403 nope"
    );
    assert_eq!(
        b.unwrap_err().0,
        "IoT credentials fetch failed: HTTP 403 nope"
    );
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    let _ = provider.get().await;
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        2,
        "a rejection is never cached"
    );
}

#[tokio::test]
async fn missing_cert_rejects_with_the_node_enoent_text() {
    let provider = IotCredentialsProvider::new(IotCredentialsOptions::new(
        "example.invalid",
        "alias",
        "thing",
        "/nonexistent/cert.pem",
        "/nonexistent/key.pem",
    ));
    assert_eq!(
        provider.get().await.unwrap_err().0,
        "ENOENT: no such file or directory, open '/nonexistent/cert.pem'"
    );
}

// ---------------------------------------------------------------------------
// mTLS credentials endpoint

#[derive(Clone, Copy)]
enum CredsReply {
    Ok,
    Forbidden,
    Stall,
}

struct Seen {
    head: String,
    sni: Option<String>,
    client_cert: bool,
}

async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf[i + 4..].to_vec();
            return Some((String::from_utf8_lossy(&buf[..i]).into_owned(), rest));
        }
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn respond<S: AsyncWrite + Unpin>(s: &mut S, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(msg.as_bytes()).await;
    let _ = s.shutdown().await;
}

/// One-shot mTLS server: requires a client cert chaining to `ca.pem`.
async fn creds_server(reply: CredsReply) -> (u16, tokio::sync::oneshot::Receiver<Option<Seen>>) {
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
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let Ok(mut tls) = acceptor.accept(tcp).await else {
            let _ = tx.send(None);
            return;
        };
        let (sni, client_cert) = {
            let (_, conn) = tls.get_ref();
            (
                conn.server_name().map(str::to_string),
                conn.peer_certificates().is_some_and(|c| !c.is_empty()),
            )
        };
        let Some((head, _)) = read_head(&mut tls).await else {
            let _ = tx.send(None);
            return;
        };
        let _ = tx.send(Some(Seen {
            head,
            sni,
            client_cert,
        }));
        match reply {
            CredsReply::Ok => {
                respond(
                    &mut tls,
                    "200 OK",
                    r#"{"credentials":{"accessKeyId":"ASIATEST","secretAccessKey":"sekrit","sessionToken":"tok","expiration":"2026-10-09T13:00:00Z"}}"#,
                )
                .await
            }
            CredsReply::Forbidden => {
                respond(&mut tls, "403 Forbidden", r#"{"message":"Access Denied"}"#).await
            }
            CredsReply::Stall => tokio::time::sleep(Duration::from_secs(30)).await,
        }
    });
    (port, rx)
}

fn creds_opts(port: u16, cert: &str, key: &str) -> IotCredentialsOptions {
    let mut o = IotCredentialsOptions::new(
        format!("localhost:{port}"),
        "Xorgate Alias/Dev",
        "a73f7a36-test-thing",
        tls(cert),
        tls(key),
    );
    o.test_roots_pem = Some(std::fs::read(tls("ca.pem")).unwrap());
    o
}

#[tokio::test]
async fn mtls_get_sends_alias_path_thing_header_and_sni() {
    let (port, seen) = creds_server(CredsReply::Ok).await;
    let p = IotCredentialsProvider::new(creds_opts(port, "client.pem", "client.key"));
    let c = p.get().await.unwrap();
    assert_eq!(c.access_key_id, "ASIATEST");
    assert_eq!(c.session_token, "tok");
    assert_eq!(c.expiration, 1_791_550_800_000.0);
    let seen = seen.await.unwrap().unwrap();
    assert!(seen.client_cert, "client cert presented");
    assert_eq!(seen.sni.as_deref(), Some("localhost"));
    let mut lines = seen.head.lines();
    assert_eq!(
        lines.next().unwrap(),
        "GET /role-aliases/Xorgate%20Alias%2FDev/credentials HTTP/1.1"
    );
    let headers: Vec<String> = lines.map(str::to_ascii_lowercase).collect();
    assert!(
        headers.contains(&"x-amzn-iot-thingname: a73f7a36-test-thing".to_string()),
        "{headers:?}"
    );
}

#[tokio::test]
async fn mtls_non_200_is_the_ts_error_text() {
    let (port, _seen) = creds_server(CredsReply::Forbidden).await;
    let p = IotCredentialsProvider::new(creds_opts(port, "client.pem", "client.key"));
    assert_eq!(
        p.get().await.unwrap_err().0,
        r#"IoT credentials fetch failed: HTTP 403 {"message":"Access Denied"}"#
    );
}

#[tokio::test]
async fn mtls_stall_times_out_with_the_ts_text() {
    let (port, _seen) = creds_server(CredsReply::Stall).await;
    let mut o = creds_opts(port, "client.pem", "client.key");
    o.timeout = Some(Duration::from_millis(300));
    let p = IotCredentialsProvider::new(o);
    assert_eq!(
        p.get().await.unwrap_err().0,
        "IoT credentials fetch timed out"
    );
}

#[tokio::test]
async fn mtls_untrusted_client_cert_is_refused() {
    let (port, seen) = creds_server(CredsReply::Ok).await;
    let p = IotCredentialsProvider::new(creds_opts(port, "rogue.pem", "rogue.key"));
    let err = p.get().await.unwrap_err();
    assert!(!err.0.is_empty());
    assert!(seen.await.unwrap().is_none(), "server never saw a request");
}

// ---------------------------------------------------------------------------
// S3 PUT request half

#[derive(Clone, Copy)]
enum PutReply {
    Ok,
    Forbidden,
    StallAfterHead,
}

struct PutSeen {
    head: String,
    body: Vec<u8>,
}

async fn s3_server(reply: PutReply) -> (u16, tokio::sync::oneshot::Receiver<PutSeen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let (head, mut body) = read_head(&mut s).await.unwrap();
        if let PutReply::StallAfterHead = reply {
            let _ = tx.send(PutSeen { head, body });
            tokio::time::sleep(Duration::from_secs(30)).await;
            return;
        }
        let len: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        let mut tmp = vec![0u8; 65536];
        while body.len() < len {
            let n = s.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        let _ = tx.send(PutSeen { head, body });
        match reply {
            PutReply::Ok => respond(&mut s, "200 OK", "").await,
            PutReply::Forbidden => {
                let xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message><Pad>{}</Pad></Error>",
                    "é".repeat(300)
                );
                respond(&mut s, "403 Forbidden", &xml).await
            }
            PutReply::StallAfterHead => unreachable!(),
        }
    });
    (port, rx)
}

fn signed(body: &[u8]) -> xorgate_cloud::sigv4::SignedRequest {
    sign_put(
        "us-east-1",
        "xorgate-dev-recordings",
        "media-sessions/v1/dev/cam0/019a/00000.json",
        body,
        "application/json",
        SigningCredentials {
            access_key_id: "ASIATEST",
            secret_access_key: "sekrit",
            session_token: "tok",
        },
        1_791_504_000_000.0,
    )
}

#[tokio::test]
async fn put_sends_signed_headers_and_an_unchunked_body() {
    let (port, seen) = s3_server(PutReply::Ok).await;
    let body = Bytes::from(vec![0x5a_u8; 300 * 1024]); // several body chunks
    let s = signed(&body);
    let client = https_client().unwrap();
    put_signed(
        &client,
        &format!("http://127.0.0.1:{port}"),
        &s,
        body.clone(),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap();
    let seen = seen.await.unwrap();
    assert_eq!(seen.body, body.to_vec());
    let mut lines = seen.head.lines();
    assert_eq!(
        lines.next().unwrap(),
        "PUT /media-sessions/v1/dev/cam0/019a/00000.json HTTP/1.1"
    );
    let headers: Vec<(String, String)> = lines
        .map(|l| {
            let (k, v) = l.split_once(':').unwrap();
            (k.trim().to_ascii_lowercase(), v.trim().to_string())
        })
        .collect();
    for (k, v) in &s.headers {
        assert!(
            headers.iter().any(|(hk, hv)| hk == k && hv == v),
            "missing {k}: {v} in {headers:?}"
        );
    }
    assert!(
        !headers.iter().any(|(k, _)| k == "transfer-encoding"),
        "{headers:?}"
    );
    assert_eq!(headers.iter().filter(|(k, _)| k == "host").count(), 1);
}

#[tokio::test]
async fn put_non_200_is_the_ts_error_text_sliced_to_300_utf16_units() {
    let (port, _seen) = s3_server(PutReply::Forbidden).await;
    let body = Bytes::from_static(b"{}");
    let s = signed(&body);
    let client = https_client().unwrap();
    let err = put_signed(
        &client,
        &format!("http://127.0.0.1:{port}"),
        &s,
        body,
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message><Pad>{}</Pad></Error>",
        "é".repeat(300)
    );
    let want: String = xml.chars().take(300).collect(); // all BMP: 1 unit per char
    assert_eq!(err.0, format!("S3 PUT failed: HTTP 403 {want}"));
}

#[tokio::test]
async fn put_idle_stall_times_out_with_the_ts_text() {
    let (port, _seen) = s3_server(PutReply::StallAfterHead).await;
    let body = Bytes::from_static(b"{}");
    let s = signed(&body);
    let client = https_client().unwrap();
    let err = put_signed(
        &client,
        &format!("http://127.0.0.1:{port}"),
        &s,
        body,
        Duration::from_millis(300),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, "S3 PUT timed out");
}

#[tokio::test]
async fn put_abort_rejects_with_the_node_abort_text() {
    let (port, seen) = s3_server(PutReply::StallAfterHead).await;
    let body = Bytes::from_static(b"{}");
    let s = signed(&body);
    let client = https_client().unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        let _ = seen.await; // the request is on the wire
        c2.cancel();
    });
    let err = put_signed(
        &client,
        &format!("http://127.0.0.1:{port}"),
        &s,
        body,
        Duration::from_secs(30),
        Some(&cancel),
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, "The operation was aborted");
}
