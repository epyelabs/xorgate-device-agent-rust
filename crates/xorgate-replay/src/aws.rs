//! The rig's fake AWS endpoints (phase-3-recording-media.md §Record/replay
//! "Rig additions"), served by the player inside the rig container on `:443`:
//!
//! - **IoT credentials provider** (`GET /role-aliases/<alias>/credentials`,
//!   `ts:src/recording/iot-creds.ts:73-132`): mTLS — the client must present a
//!   cert issued by the rig CA (the agents' `CERT_PATH`/`KEY_PATH`); answers
//!   the FIXED rig credentials;
//! - **S3** (`PUT /<key>` on `<bucket>.s3.<region>.amazonaws.com`,
//!   `ts:src/recording/s3-put.ts`): the SigV4 signature is recomputed here,
//!   independently of both agents' signers, against the fixed rig secret; a
//!   bad signature answers 403 and is logged `sig: "bad: …"` (a FAIL row in
//!   the diff). Bodies are stored per agent (`<run>/<label>/s3/<key>`; mp4
//!   bodies as size + sha256 only) and every PUT is logged in arrival order to
//!   `<run>/<label>/s3.ndjson` as `{w, key, contentType, size, sha256, status,
//!   sig}`.
//!
//! The host names are rig constants: the player writes them into each agent's
//! `kvs.json` (whatever the tape's manifest said), maps them to 127.0.0.1
//! (compose `extra_hosts`, plus `/etc/hosts` for buckets a tape's config
//! names) and the rig network is internal, so nothing can reach real AWS.
//! Trust: the server cert is signed by the rig CA, which the player installs in
//! the container's system store (Rust's platform verifier) and passes to Node
//! as `NODE_EXTRA_CA_CERTS` (P5: no new agent knob).
//!
//! Optional fault windows (`meta.rig.s3Faults: [{fromMs, toMs, status}]`,
//! relative to the agents' start) answer that status instead of 200 — the
//! synth `upload-outage` scenario's retry/backoff input.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use xorgate_hal::tape::wall_now_ms;

/// Rig region (every rig manifest names it).
pub const RIG_REGION: &str = "ca-central-1";
/// Rig IoT credentials endpoint host.
pub const RIG_CREDS_HOST: &str = "c0rig00000000.credentials.iot.ca-central-1.amazonaws.com";
/// Rig role alias.
pub const RIG_ROLE_ALIAS: &str = "xorgate-rig-role-alias";
/// Rig upload bucket.
pub const RIG_BUCKET: &str = "xorgate-rig-media";
/// The fixed credentials the fake provider vends.
pub const RIG_ACCESS_KEY: &str = "ASIARIGFAKE000000000";
pub const RIG_SECRET: &str = "rig/Secret/Key+for+the+xorgate+replay+rig0";
pub const RIG_TOKEN: &str = "rig-session-token-AQoDYXdzEJr";

/// Signed headers `signPut` always sends (`s3-put.ts:60-66`).
const REQUIRED_SIGNED: &[&str] = &[
    "content-type",
    "host",
    "x-amz-content-sha256",
    "x-amz-date",
    "x-amz-security-token",
];

/// A fault window: PUTs arriving within `[from_ms, to_ms)` of the agents'
/// start answer `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub from_ms: i64,
    pub to_ms: i64,
    pub status: u16,
}

/// `meta.rig.s3Faults` of a tape.
pub fn faults_of(rig: Option<&Value>) -> Vec<Fault> {
    rig.and_then(|r| r.get("s3Faults"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            Some(Fault {
                from_ms: f.get("fromMs")?.as_i64()?,
                to_ms: f.get("toMs")?.as_i64()?,
                status: f.get("status")?.as_u64()? as u16,
            })
        })
        .collect()
}

/// Phase 6 P3a: a HOLD window (`meta.rig.s3Holds: [{fromMs, toMs, holdMs,
/// suffix}]`, relative to the agents' start like the fault windows). A PUT
/// whose key ends `suffix` and whose HEADERS arrive inside the window is held
/// `holdMs` BEFORE its body is read: the body sits in the socket buffers, so
/// the PUT is mid-transfer — the sidecar of an mp4 was already PUT (sidecar
/// first, `uploader.ts:168-176` @09da38d) and the mp4 is in flight. After the
/// hold the fake reads the body and probes the connection: if the client is
/// gone (killed at a handoff) the PUT is ABORTED — never stored, never a
/// status — and logged `s3-aborted` in `aws.ndjson` (with the `s3-held`
/// record at the arrival), so the in-flight object deterministically never
/// lands. A live client gets the normal answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub from_ms: i64,
    pub to_ms: i64,
    pub hold_ms: u64,
    pub suffix: String,
}

/// `meta.rig.s3Holds` of a tape.
pub fn holds_of(rig: Option<&Value>) -> Vec<Hold> {
    rig.and_then(|r| r.get("s3Holds"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| {
            Some(Hold {
                from_ms: h.get("fromMs")?.as_i64()?,
                to_ms: h.get("toMs")?.as_i64()?,
                hold_ms: h.get("holdMs")?.as_u64()?,
                suffix: h.get("suffix")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// How long the post-hold probe waits for a sign of life: a dead client
/// shows EOF / an unclean TLS close at once.
const HOLD_PROBE_MS: u64 = 300;

/// Who the fake serves: `(deviceId, label, capture dir)` per agent.
pub struct AwsFake {
    pub agents: Vec<(String, String, PathBuf)>,
    pub faults: Vec<Fault>,
    /// P3a hold windows ([`Hold`]).
    pub holds: Vec<Hold>,
    /// Wall ms the agents were started (fault windows are relative to it);
    /// 0 until set.
    pub start_wall: Arc<Mutex<i64>>,
    pub log: Arc<Mutex<File>>,
    s3_logs: Vec<Mutex<File>>,
}

impl AwsFake {
    pub fn new(
        agents: Vec<(String, String, PathBuf)>,
        faults: Vec<Fault>,
        holds: Vec<Hold>,
        out: &Path,
    ) -> Result<Arc<Self>> {
        let mut s3_logs = Vec::new();
        for (_, _, dir) in &agents {
            s3_logs.push(Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("s3.ndjson"))?,
            ));
        }
        Ok(Arc::new(Self {
            agents,
            faults,
            holds,
            start_wall: Arc::new(Mutex::new(0)),
            log: Arc::new(Mutex::new(File::create(out.join("aws.ndjson"))?)),
            s3_logs,
        }))
    }

    fn note(&self, v: Value) {
        if let Ok(mut f) = self.log.lock() {
            let _ = writeln!(f, "{v}");
        }
    }

    /// P3a: `(hold ms, key, agent label)` when a PUT with this head must be
    /// held ([`Hold`]).
    fn hold_of(&self, head: &Head) -> Option<(u64, String, String)> {
        if head.method != "PUT" || self.holds.is_empty() {
            return None;
        }
        let key = decode_path(head.path.trim_start_matches('/'));
        let started = self.start_wall.lock().map(|g| *g).unwrap_or(0);
        let w = wall_now_ms();
        let h = self.holds.iter().find(|h| {
            started > 0
                && w - started >= h.from_ms
                && w - started < h.to_ms
                && key.ends_with(h.suffix.as_str())
        })?;
        let label = self.agents.get(self.agent_of(&key)?)?.1.clone();
        Some((h.hold_ms, key, label))
    }

    /// The agent a key belongs to: S3 keys embed the deviceId
    /// (`telemetry/v1/<id>/…`, `media-sessions/v1/<id>/…`).
    fn agent_of(&self, key: &str) -> Option<usize> {
        self.agents
            .iter()
            .position(|(id, _, _)| key.split('/').any(|seg| seg == id))
    }
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).expect("hmac key");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// One parsed request.
#[derive(Debug, Default)]
pub struct Req {
    pub method: String,
    /// The raw (encoded) path, without query.
    pub path: String,
    pub query: String,
    /// Lower-case names; repeated headers joined with `,`.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Verify a SigV4 PUT the way S3 does for a single-chunk signed payload
/// (independent of both agents' signers). `Ok(())` or the reason.
pub fn verify_sigv4(req: &Req, secret: &str, access_key: &str, token: &str) -> Result<(), String> {
    let auth = req
        .headers
        .get("authorization")
        .ok_or("no authorization header")?;
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or("not AWS4-HMAC-SHA256")?;
    let mut cred = None;
    let mut signed = None;
    let mut sig = None;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            cred = Some(v);
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed = Some(v);
        } else if let Some(v) = part.strip_prefix("Signature=") {
            sig = Some(v);
        }
    }
    let (cred, signed, sig) = (
        cred.ok_or("no Credential")?,
        signed.ok_or("no SignedHeaders")?,
        sig.ok_or("no Signature")?,
    );
    let mut cparts = cred.splitn(5, '/');
    let (ak, date, region, service, term) = (
        cparts.next().unwrap_or(""),
        cparts.next().unwrap_or(""),
        cparts.next().unwrap_or(""),
        cparts.next().unwrap_or(""),
        cparts.next().unwrap_or(""),
    );
    if ak != access_key {
        return Err(format!("access key {ak:?} is not the rig's"));
    }
    if service != "s3" || term != "aws4_request" {
        return Err(format!("scope {cred:?}"));
    }
    if req.headers.get("x-amz-security-token").map(String::as_str) != Some(token) {
        return Err("x-amz-security-token is not the rig's".into());
    }
    let amz_date = req.headers.get("x-amz-date").ok_or("no x-amz-date")?;
    if !amz_date.starts_with(date) {
        return Err(format!("x-amz-date {amz_date} vs scope date {date}"));
    }
    let names: Vec<&str> = signed.split(';').collect();
    for need in REQUIRED_SIGNED {
        if !names.contains(need) {
            return Err(format!("{need} not signed"));
        }
    }
    let payload = req
        .headers
        .get("x-amz-content-sha256")
        .ok_or("no x-amz-content-sha256")?;
    let body_hash = sha256_hex(&req.body);
    if *payload != body_hash {
        return Err(format!(
            "x-amz-content-sha256 {payload} != body sha256 {body_hash}"
        ));
    }
    if let Some(len) = req.headers.get("content-length") {
        if len.trim() != req.body.len().to_string() {
            return Err(format!("content-length {len} != body {}", req.body.len()));
        }
    }
    let mut canonical_headers = String::new();
    for n in &names {
        let v = req
            .headers
            .get(*n)
            .ok_or_else(|| format!("signed header {n} absent"))?;
        canonical_headers.push_str(&format!("{n}:{}\n", v.trim()));
    }
    let canonical = [
        req.method.as_str(),
        req.path.as_str(),
        req.query.as_str(),
        canonical_headers.as_str(),
        signed,
        payload.as_str(),
    ]
    .join("\n");
    let scope = format!("{date}/{region}/s3/aws4_request");
    let sts = [
        "AWS4-HMAC-SHA256",
        amz_date.as_str(),
        scope.as_str(),
        &sha256_hex(canonical.as_bytes()),
    ]
    .join("\n");
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, "s3");
    let k_signing = hmac(&k_service, "aws4_request");
    let want = hex::encode(hmac(&k_signing, &sts));
    if want != sig {
        return Err(format!("signature mismatch (want {want}, got {sig})"));
    }
    Ok(())
}

/// A request line + headers (the body not read yet).
#[derive(Debug, Default)]
pub struct Head {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: BTreeMap<String, String>,
}

/// The request line + headers of one request; `Ok(None)` on a clean EOF.
async fn read_head<R: tokio::io::AsyncRead + Unpin>(
    r: &mut BufReader<R>,
) -> std::io::Result<Option<Head>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let mut it = line.trim_end().splitn(3, ' ');
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).await? == 0 {
            break;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            headers
                .entry(k)
                .and_modify(|e| {
                    e.push(',');
                    e.push_str(&v)
                })
                .or_insert(v);
        }
    }
    Ok(Some(Head {
        method,
        path,
        query,
        headers,
    }))
}

/// The body of a request whose head was read (Content-Length or chunked).
async fn read_body<R: tokio::io::AsyncRead + Unpin>(
    r: &mut BufReader<R>,
    headers: &BTreeMap<String, String>,
) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        body.resize(len, 0);
        r.read_exact(&mut body).await?;
    } else if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        loop {
            let mut sz = String::new();
            r.read_line(&mut sz).await?;
            let n =
                usize::from_str_radix(sz.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if n == 0 {
                let mut end = String::new();
                r.read_line(&mut end).await?;
                break;
            }
            let mut chunk = vec![0; n];
            r.read_exact(&mut chunk).await?;
            body.extend_from_slice(&chunk);
            let mut crlf = String::new();
            r.read_line(&mut crlf).await?;
        }
    }
    Ok(body)
}

/// After a hold: is the client gone? EOF or a read error (a killed peer
/// sends no TLS close_notify) within [`HOLD_PROBE_MS`] = gone; a timeout
/// (it waits for our answer) or pipelined bytes = alive.
async fn client_gone<R: tokio::io::AsyncRead + Unpin>(r: &mut BufReader<R>) -> bool {
    match tokio::time::timeout(
        std::time::Duration::from_millis(HOLD_PROBE_MS),
        r.fill_buf(),
    )
    .await
    {
        Ok(Ok(buf)) => buf.is_empty(),
        Ok(Err(_)) => true,
        Err(_) => false,
    }
}

fn response(status: u16, reason: &str, ctype: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nx-amz-request-id: RIG0000000000000\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn s3_error(status: u16, code: &str, msg: &str) -> Vec<u8> {
    let reason = match status {
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    response(
        status,
        reason,
        "application/xml",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>{msg}</Message><RequestId>RIG0000000000000</RequestId></Error>"
        )
        .as_bytes(),
    )
}

/// Percent-decode an S3 path (the key as stored).
fn decode_path(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hexv = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hexv(b[i + 1]), hexv(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl AwsFake {
    fn creds(&self, req: &Req, peer_cert: bool) -> Vec<u8> {
        let thing = req
            .headers
            .get("x-amzn-iot-thingname")
            .cloned()
            .unwrap_or_default();
        let want_path = format!("/role-aliases/{RIG_ROLE_ALIAS}/credentials");
        let ok = peer_cert
            && req.method == "GET"
            && req.path == want_path
            && self.agents.iter().any(|(id, _, _)| *id == thing);
        self.note(json!({"w": wall_now_ms(), "k": "creds", "thing": thing, "path": req.path, "mtls": peer_cert, "ok": ok}));
        if !ok {
            return response(
                403,
                "Forbidden",
                "application/json",
                br#"{"message":"rig: wrong role alias, thing name or client cert"}"#,
            );
        }
        let exp = wall_now_ms() + 3_600_000;
        let iso = iso8601(exp);
        let body = json!({"credentials": {
            "accessKeyId": RIG_ACCESS_KEY,
            "secretAccessKey": RIG_SECRET,
            "sessionToken": RIG_TOKEN,
            "expiration": iso,
        }})
        .to_string();
        response(200, "OK", "application/json", body.as_bytes())
    }

    fn s3(&self, req: &Req) -> Vec<u8> {
        let w = wall_now_ms();
        let key = decode_path(req.path.trim_start_matches('/'));
        let ctype = req.headers.get("content-type").cloned().unwrap_or_default();
        let sha = sha256_hex(&req.body);
        let size = req.body.len();
        let host = req.headers.get("host").cloned().unwrap_or_default();
        let bucket = host
            .strip_suffix(&format!(".s3.{RIG_REGION}.amazonaws.com"))
            .unwrap_or("")
            .to_string();
        let sig = if req.method != "PUT" {
            Err(format!("method {}", req.method))
        } else if bucket.is_empty() {
            Err(format!("host {host:?} is not a rig bucket host"))
        } else {
            verify_sigv4(req, RIG_SECRET, RIG_ACCESS_KEY, RIG_TOKEN)
        };
        let started = self.start_wall.lock().map(|g| *g).unwrap_or(0);
        let fault = self
            .faults
            .iter()
            .find(|f| started > 0 && w - started >= f.from_ms && w - started < f.to_ms)
            .map(|f| f.status);
        let (status, resp) = match (&sig, fault) {
            (Err(e), _) => (403, s3_error(403, "SignatureDoesNotMatch", e)),
            (Ok(()), Some(st)) => (st, s3_error(st, "SlowDown", "rig fault window")),
            (Ok(()), None) => (200, response(200, "OK", "application/xml", b"")),
        };
        let rec = json!({
            "w": w, "bucket": bucket, "key": key, "contentType": ctype, "size": size,
            "sha256": sha, "status": status,
            "sig": match &sig { Ok(()) => "ok".to_string(), Err(e) => format!("bad: {e}") },
        });
        match self.agent_of(&key) {
            Some(i) => {
                if let Ok(mut f) = self.s3_logs[i].lock() {
                    let _ = writeln!(f, "{rec}");
                }
                if status == 200 && !key.ends_with(".mp4") {
                    let dst = self.agents[i].2.join("s3").join(&key);
                    if let Some(p) = dst.parent() {
                        let _ = fs::create_dir_all(p);
                    }
                    let _ = fs::write(dst, &req.body);
                }
            }
            None => self.note(json!({"w": w, "k": "s3-unrouted", "rec": rec})),
        }
        resp
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` of epoch ms (the provider's `expiration` shape).
pub fn iso8601(ms: i64) -> String {
    let (y, mo, d, h, mi, s) = crate::rewrite::civil_from_epoch(ms.div_euclid(1000));
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn load_chain(certs: &Path, name: &str) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>> {
    use rustls_pki_types::pem::PemObject;
    Ok(
        rustls_pki_types::CertificateDer::pem_file_iter(certs.join(format!("{name}.crt")))
            .with_context(|| format!("{name}.crt"))?
            .collect::<Result<_, _>>()?,
    )
}

fn server_configs(certs: &Path) -> Result<(Arc<rustls::ServerConfig>, Arc<rustls::ServerConfig>)> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let chain = load_chain(certs, "aws")?;
    let key = || PrivateKeyDer::from_pem_file(certs.join("aws.key"));
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(certs.join("ca.pem"))? {
        roots.add(c?)?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()?;
    let mtls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain.clone(), key()?)?;
    let plain = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key()?)?;
    Ok((Arc::new(mtls), Arc::new(plain)))
}

/// Serve both fakes on `listen` until the process exits. The TLS config is
/// chosen by SNI: the credentials host gets mTLS, anything else plain TLS.
pub async fn serve(fake: Arc<AwsFake>, listen: String, certs: PathBuf) -> Result<()> {
    let (mtls, plain) = server_configs(&certs)?;
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("aws fake: bind {listen}"))?;
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let (fake, mtls, plain) = (fake.clone(), mtls.clone(), plain.clone());
        tokio::spawn(async move {
            let acceptor =
                tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), sock);
            let Ok(start) = acceptor.await else {
                return;
            };
            let sni = start.client_hello().server_name().unwrap_or("").to_string();
            let creds_host = sni.ends_with(".credentials.iot.ca-central-1.amazonaws.com");
            let cfg = if creds_host { mtls } else { plain };
            let tls = match start.into_stream(cfg).await {
                Ok(t) => t,
                Err(e) => {
                    fake.note(json!({"w": wall_now_ms(), "k": "tls-error", "sni": sni, "peer": peer.to_string(), "error": e.to_string()}));
                    return;
                }
            };
            let peer_cert = tls
                .get_ref()
                .1
                .peer_certificates()
                .is_some_and(|c| !c.is_empty());
            let (rd, mut wr) = tokio::io::split(tls);
            let mut rd = BufReader::new(rd);
            loop {
                let head = match read_head(&mut rd).await {
                    Ok(Some(h)) => h,
                    _ => break,
                };
                // P3a: an in-flight PUT held across a handoff ([`Hold`]).
                let held = if creds_host {
                    None
                } else {
                    fake.hold_of(&head)
                };
                if let Some((ms, key, agent)) = &held {
                    fake.note(json!({"w": wall_now_ms(), "k": "s3-held", "key": key, "agent": agent, "holdMs": ms}));
                    tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                }
                let body = match read_body(&mut rd, &head.headers).await {
                    Ok(b) => b,
                    Err(e) => {
                        if let Some((_, key, agent)) = &held {
                            fake.note(json!({"w": wall_now_ms(), "k": "s3-aborted", "key": key, "agent": agent, "why": format!("body: {e}")}));
                        }
                        break;
                    }
                };
                if let Some((_, key, agent)) = &held {
                    if client_gone(&mut rd).await {
                        fake.note(json!({"w": wall_now_ms(), "k": "s3-aborted", "key": key, "agent": agent, "why": "client gone after the body"}));
                        break;
                    }
                    fake.note(json!({"w": wall_now_ms(), "k": "s3-held-released", "key": key, "agent": agent}));
                }
                let req = Req {
                    method: head.method,
                    path: head.path,
                    query: head.query,
                    headers: head.headers,
                    body,
                };
                let resp = if creds_host {
                    fake.creds(&req, peer_cert)
                } else {
                    fake.s3(&req)
                };
                if wr.write_all(&resp).await.is_err() || wr.flush().await.is_err() {
                    break;
                }
                if req
                    .headers
                    .get("connection")
                    .is_some_and(|c| c.eq_ignore_ascii_case("close"))
                {
                    break;
                }
            }
            let _ = wr.shutdown().await;
        });
    }
}

/// Install the rig CA into the container's system trust store (the Rust
/// agent's platform verifier reads `/etc/ssl/certs`). Best effort; logged.
pub fn trust_rig_ca(certs: &Path) -> Result<String> {
    let dst = Path::new("/usr/local/share/ca-certificates/xorgate-replay-rig.crt");
    fs::create_dir_all(dst.parent().unwrap_or(Path::new("/")))?;
    fs::copy(certs.join("ca.pem"), dst)?;
    let out = std::process::Command::new("update-ca-certificates")
        .output()
        .context("update-ca-certificates")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Map extra rig hosts (a tape's bucket names) to 127.0.0.1 in /etc/hosts.
pub fn add_hosts(hosts: &[String]) -> Result<()> {
    let cur = fs::read_to_string("/etc/hosts").unwrap_or_default();
    let mut f = OpenOptions::new().append(true).open("/etc/hosts")?;
    for h in hosts {
        if !cur
            .lines()
            .any(|l| l.split_whitespace().skip(1).any(|n| n == h))
        {
            writeln!(f, "127.0.0.1\t{h}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TS signer (`s3-put.ts` `signPut`) for a fixed request: the AWS
    /// documented example shape, recomputed with the rig secret.
    fn sign(req: &mut Req, secret: &str) {
        let amz = req.headers["x-amz-date"].clone();
        let date = &amz[..8];
        let mut names: Vec<&String> = req
            .headers
            .keys()
            .filter(|k| *k != "content-length")
            .collect();
        names.sort();
        let ch: String = names
            .iter()
            .map(|n| format!("{n}:{}\n", req.headers[*n].trim()))
            .collect();
        let sh = names
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical = format!(
            "PUT\n{}\n\n{ch}\n{sh}\n{}",
            req.path, req.headers["x-amz-content-sha256"]
        );
        let scope = format!("{date}/{RIG_REGION}/s3/aws4_request");
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}",
            sha256_hex(canonical.as_bytes())
        );
        let k = hmac(
            &hmac(
                &hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), RIG_REGION),
                "s3",
            ),
            "aws4_request",
        );
        let sig = hex::encode(hmac(&k, &sts));
        req.headers.insert(
            "authorization".into(),
            format!("AWS4-HMAC-SHA256 Credential={RIG_ACCESS_KEY}/{scope}, SignedHeaders={sh}, Signature={sig}"),
        );
    }

    fn put(body: &[u8]) -> Req {
        let mut h = BTreeMap::new();
        h.insert("content-type".into(), "application/json".into());
        h.insert(
            "host".into(),
            format!("{RIG_BUCKET}.s3.{RIG_REGION}.amazonaws.com"),
        );
        h.insert("x-amz-content-sha256".into(), sha256_hex(body));
        h.insert("x-amz-date".into(), "20260729T193000Z".into());
        h.insert("x-amz-security-token".into(), RIG_TOKEN.into());
        h.insert("content-length".into(), body.len().to_string());
        Req {
            method: "PUT".into(),
            path: "/media-sessions/v1/dev/cam0/s/00000.json".into(),
            query: String::new(),
            headers: h,
            body: body.to_vec(),
        }
    }

    #[test]
    fn sigv4_accepts_the_rig_secret_and_rejects_tampering() {
        let mut r = put(b"{\"v\":1}");
        sign(&mut r, RIG_SECRET);
        assert_eq!(
            verify_sigv4(&r, RIG_SECRET, RIG_ACCESS_KEY, RIG_TOKEN),
            Ok(())
        );
        // wrong secret
        let mut bad = put(b"{\"v\":1}");
        sign(&mut bad, "other");
        assert!(verify_sigv4(&bad, RIG_SECRET, RIG_ACCESS_KEY, RIG_TOKEN)
            .unwrap_err()
            .starts_with("signature mismatch"));
        // body swapped after signing
        let mut swapped = r;
        swapped.body = b"{\"v\":2}".to_vec();
        assert!(
            verify_sigv4(&swapped, RIG_SECRET, RIG_ACCESS_KEY, RIG_TOKEN)
                .unwrap_err()
                .contains("body sha256")
        );
        // a required header left unsigned
        let mut r2 = put(b"x");
        r2.headers.remove("x-amz-security-token");
        sign(&mut r2, RIG_SECRET);
        r2.headers
            .insert("x-amz-security-token".into(), RIG_TOKEN.into());
        assert!(verify_sigv4(&r2, RIG_SECRET, RIG_ACCESS_KEY, RIG_TOKEN)
            .unwrap_err()
            .contains("not signed"));
    }

    #[test]
    fn holds_parse_and_match_only_their_suffix_and_window() {
        let rig = json!({"s3Holds": [{"fromMs": 87_000, "toMs": 95_000, "holdMs": 20_000, "suffix": ".mp4"}]});
        let holds = holds_of(Some(&rig));
        assert_eq!(
            holds,
            vec![Hold {
                from_ms: 87_000,
                to_ms: 95_000,
                hold_ms: 20_000,
                suffix: ".mp4".into()
            }]
        );
        assert!(holds_of(None).is_empty());
        let dir = std::env::temp_dir().join(format!("aws-hold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = AwsFake::new(
            vec![("dev-a".into(), "a".into(), dir.clone())],
            vec![],
            holds,
            &dir,
        )
        .unwrap();
        let head = |path: &str| Head {
            method: "PUT".into(),
            path: path.into(),
            ..Default::default()
        };
        let mp4 = "/media-sessions/v1/dev-a/cam0/s/00007.mp4";
        // Not started yet → never held.
        assert!(fake.hold_of(&head(mp4)).is_none());
        *fake.start_wall.lock().unwrap() = wall_now_ms() - 90_000;
        let (ms, key, agent) = fake.hold_of(&head(mp4)).unwrap();
        assert_eq!((ms, key.as_str(), agent.as_str()), (20_000, &mp4[1..], "a"));
        // NEGATIVE: its sidecar, a GET, another device, outside the window.
        assert!(fake
            .hold_of(&head("/media-sessions/v1/dev-a/cam0/s/00007.json"))
            .is_none());
        let mut get = head(mp4);
        get.method = "GET".into();
        assert!(fake.hold_of(&get).is_none());
        assert!(fake
            .hold_of(&head("/media-sessions/v1/dev-z/cam0/s/00007.mp4"))
            .is_none());
        *fake.start_wall.lock().unwrap() = wall_now_ms() - 96_000;
        assert!(fake.hold_of(&head(mp4)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hold's outcome is decided by the client: killed during the hold →
    /// aborted (mid-body: the body read fails; after the whole body: the
    /// probe sees the close); alive → answered normally.
    #[tokio::test]
    async fn a_held_put_is_aborted_exactly_when_its_client_is_gone() {
        use tokio::io::AsyncWriteExt as _;
        let req = b"PUT /k HTTP/1.1\r\ncontent-length: 3\r\n\r\n";
        // Whole body sent, then the client dies.
        let (mut c, srv) = tokio::io::duplex(1 << 16);
        c.write_all(req).await.unwrap();
        c.write_all(b"abc").await.unwrap();
        drop(c);
        let mut r = BufReader::new(srv);
        let h = read_head(&mut r).await.unwrap().unwrap();
        assert_eq!((h.method.as_str(), h.path.as_str()), ("PUT", "/k"));
        assert_eq!(read_body(&mut r, &h.headers).await.unwrap(), b"abc");
        assert!(client_gone(&mut r).await);
        // Mid-body: the client dies after 1 of 3 bytes.
        let (mut c, srv) = tokio::io::duplex(1 << 16);
        c.write_all(req).await.unwrap();
        c.write_all(b"a").await.unwrap();
        drop(c);
        let mut r = BufReader::new(srv);
        let h = read_head(&mut r).await.unwrap().unwrap();
        assert!(read_body(&mut r, &h.headers).await.is_err());
        // NEGATIVE: a live client waiting for the answer is NOT aborted.
        let (mut c, srv) = tokio::io::duplex(1 << 16);
        c.write_all(req).await.unwrap();
        c.write_all(b"abc").await.unwrap();
        let mut r = BufReader::new(srv);
        let h = read_head(&mut r).await.unwrap().unwrap();
        read_body(&mut r, &h.headers).await.unwrap();
        assert!(!client_gone(&mut r).await);
        drop(c);
    }

    #[test]
    fn faults_and_paths() {
        let rig = json!({"s3Faults": [{"fromMs": 1000, "toMs": 2000, "status": 503}]});
        assert_eq!(
            faults_of(Some(&rig)),
            vec![Fault {
                from_ms: 1000,
                to_ms: 2000,
                status: 503
            }]
        );
        assert_eq!(decode_path("a%20b/c%2Fd"), "a b/c/d");
        assert_eq!(decode_path("x%2"), "x%2");
        assert_eq!(iso8601(1_785_353_400_000), "2026-07-29T19:30:00Z");
    }
}
