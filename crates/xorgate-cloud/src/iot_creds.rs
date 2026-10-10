//! IoT credential-provider client — port of `src/recording/iot-creds.ts`
//! (:15-132). Exchanges the device's X.509 cert for short-lived AWS
//! credentials at `https://<credentialEndpoint>/role-aliases/<alias>/credentials`
//! (mTLS, header `x-amzn-iot-thingname`, SNI = the full endpoint host).
//!
//! Caching (`iot-creds.ts:53-66`): creds are vended for 1 h; they are reused
//! until within `refresh_skew_ms` (default 5 min) of expiry. Concurrent
//! callers share ONE in-flight fetch, which runs to completion even if every
//! caller goes away (a JS promise is not cancelled by its awaiters).
//!
//! Transport: reqwest 0.13 over rustls/aws-lc-rs with the platform verifier
//! (Debian's Mozilla bundle — node:https uses its bundled Mozilla roots), no
//! env proxies (node:https ignores `HTTPS_PROXY`), HTTP/1.1 only. The cert and
//! key are re-read on every fetch (~hourly) so a cert rotation needs no
//! restart (`iot-creds.ts:74-76`).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;

use crate::s3::{
    error_chain, js_slice_utf16, node_transport_error, run_idle, Activity, Interrupted,
};
use crate::sigv4::SigningCredentials;

/// `DEFAULT_SKEW_MS` (`iot-creds.ts:41`).
pub const DEFAULT_SKEW_MS: f64 = 5.0 * 60.0 * 1000.0;
/// `DEFAULT_TIMEOUT_MS` (`iot-creds.ts:42`) — a node:https `timeout`, i.e. a
/// socket IDLE timeout, mirrored by [`run_idle`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(15_000);

/// `AwsCredentials` (`iot-creds.ts:15-21`). `expiration` is epoch ms as an
/// f64: `Date.parse` of the vended string, `NaN` when unparseable (which
/// makes every `get()` re-fetch, exactly as in the TS).
#[derive(Clone, PartialEq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
    pub expiration: f64,
}

impl AwsCredentials {
    pub fn signing(&self) -> SigningCredentials<'_> {
        SigningCredentials {
            access_key_id: &self.access_key_id,
            secret_access_key: &self.secret_access_key,
            session_token: &self.session_token,
        }
    }
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("session_token", &"<redacted>")
            .field("expiration", &self.expiration)
            .finish()
    }
}

/// A rejected `get()`: `Display` is the TS `Error.message` verbatim (it ends
/// up in the uploader's `upload failed, retrying in Ns: <msg>` event).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CredsError(pub String);

pub type CredsFuture = Pin<Box<dyn Future<Output = Result<AwsCredentials, CredsError>> + Send>>;
/// `fetchFn` (`iot-creds.ts:35-36`): injectable transport for tests.
pub type FetchFn = Arc<dyn Fn() -> CredsFuture + Send + Sync>;
/// `now` (`iot-creds.ts:37-38`): injectable epoch-ms clock for tests.
pub type NowFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// `IotCredentialsOptions` (`iot-creds.ts:23-39`).
#[derive(Clone)]
pub struct IotCredentialsOptions {
    /// Host (no scheme), e.g. `cXXXX.credentials.iot.us-east-1.amazonaws.com`.
    pub credential_endpoint: String,
    pub role_alias: String,
    /// IoT thing name (== deviceId), sent as `x-amzn-iot-thingname`.
    pub thing_name: String,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub refresh_skew_ms: Option<f64>,
    pub timeout: Option<Duration>,
    pub fetch_fn: Option<FetchFn>,
    pub now: Option<NowFn>,
    /// Tests only: trust exactly these PEM roots instead of the platform's
    /// (the local mTLS test server). Never set in the agent.
    #[doc(hidden)]
    pub test_roots_pem: Option<Vec<u8>>,
}

impl IotCredentialsOptions {
    pub fn new(
        credential_endpoint: impl Into<String>,
        role_alias: impl Into<String>,
        thing_name: impl Into<String>,
        cert_path: impl Into<PathBuf>,
        key_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            credential_endpoint: credential_endpoint.into(),
            role_alias: role_alias.into(),
            thing_name: thing_name.into(),
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            refresh_skew_ms: None,
            timeout: None,
            fetch_fn: None,
            now: None,
            test_roots_pem: None,
        }
    }
}

type Flight = watch::Receiver<Option<Result<AwsCredentials, CredsError>>>;

#[derive(Default)]
struct State {
    cached: Option<AwsCredentials>,
    inflight: Option<Flight>,
}

/// Clears `State::inflight` when dropped — but only while it still holds THIS
/// fetch's channel. The TS `.finally` always runs; a Rust task can instead
/// panic or be cancelled, and a handle left behind with its sender gone would
/// fail every later `get()` with "IoT credentials fetch aborted" instead of
/// starting a fresh fetch.
struct InflightGuard {
    state: Arc<Mutex<State>>,
    flight: Flight,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st
            .inflight
            .as_ref()
            .is_some_and(|f| f.same_channel(&self.flight))
        {
            st.inflight = None;
        }
    }
}

/// `IotCredentialsProvider` (`iot-creds.ts:44-132`). Cheap to clone (shared state).
#[derive(Clone)]
pub struct IotCredentialsProvider {
    opts: Arc<IotCredentialsOptions>,
    state: Arc<Mutex<State>>,
}

fn wall_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

impl IotCredentialsProvider {
    pub fn new(opts: IotCredentialsOptions) -> Self {
        Self {
            opts: Arc::new(opts),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// `get()` (`iot-creds.ts:53-66`): current creds, fetching/refreshing as
    /// needed. The freshness test is on the WALL clock, as in the TS: the
    /// expiry is an absolute server time (see wp5.md ambiguity A3).
    pub async fn get(&self) -> Result<AwsCredentials, CredsError> {
        let now = self.opts.now.as_ref().map_or_else(wall_now_ms, |f| f());
        let skew = self.opts.refresh_skew_ms.unwrap_or(DEFAULT_SKEW_MS);
        let mut rx = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = &st.cached {
                // `this.cached.expiration - now > skew` (false for NaN).
                if c.expiration - now > skew {
                    return Ok(c.clone());
                }
            }
            match &st.inflight {
                Some(rx) => rx.clone(),
                None => {
                    let (tx, rx) = watch::channel(None);
                    st.inflight = Some(rx.clone());
                    let opts = self.opts.clone();
                    // Moved into the task: dropped on completion, on a panic
                    // in the fetch, and when the runtime cancels the task
                    // (even before its first poll), so the shared handle can
                    // never outlive its sender (review-wp5 SHOULD-FIX 2).
                    let guard = InflightGuard {
                        state: self.state.clone(),
                        flight: rx.clone(),
                    };
                    tokio::spawn(async move {
                        let res = match &opts.fetch_fn {
                            Some(f) => f().await,
                            None => fetch(&opts).await,
                        };
                        // `.finally(() => { this.inflight = null; })` runs
                        // before any awaiting caller resumes.
                        drop(guard);
                        let _ = tx.send(Some(res));
                    });
                    rx
                }
            }
        };
        let res = match rx.wait_for(Option::is_some).await {
            Ok(v) => v
                .clone()
                .unwrap_or_else(|| Err(CredsError("IoT credentials fetch aborted".into()))),
            Err(_) => Err(CredsError("IoT credentials fetch aborted".into())),
        };
        let creds = res?;
        // `this.cached = creds` (every awaiting caller stores it).
        self.state.lock().unwrap_or_else(|e| e.into_inner()).cached = Some(creds.clone());
        Ok(creds)
    }

    /// `invalidate()` (`iot-creds.ts:68-71`): drop the cache so the next
    /// `get()` re-fetches.
    pub fn invalidate(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).cached = None;
    }
}

/// `encodeURIComponent` (ES2023 §19.2.6.5): everything but
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )` is UTF-8 percent-encoded (uppercase hex).
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `readFileSync(path)` error text as node prints it
/// (`ENOENT: no such file or directory, open '<path>'`) for the errnos a
/// cert/key read can realistically hit; anything else falls back to the OS text.
fn node_read_error(err: &std::io::Error, path: &std::path::Path) -> String {
    let p = path.display();
    match err.raw_os_error() {
        Some(libc_errno::ENOENT) => format!("ENOENT: no such file or directory, open '{p}'"),
        Some(libc_errno::EACCES) => format!("EACCES: permission denied, open '{p}'"),
        Some(libc_errno::EISDIR) => "EISDIR: illegal operation on a directory, read".to_string(),
        Some(libc_errno::ENOTDIR) => format!("ENOTDIR: not a directory, open '{p}'"),
        _ => format!("{err}, open '{p}'"),
    }
}

/// The Linux errno values `node_read_error` names (asm-generic, all arches).
mod libc_errno {
    pub const ENOENT: i32 = 2;
    pub const EACCES: i32 = 13;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
}

/// The OpenSSL 3 text node's `tls.createSecureContext` throws from
/// `https.request({ cert, key })` (`iot-creds.ts:78-86`) for the PEM
/// failures recognisable without a parser (CF32; probed on node v20.20.2):
/// the cert is checked first (`setCerts` before `setKey`), so a cert with no
/// certificate block is `no start line` whatever the key holds, and a key
/// with no private-key block (empty, garbage, a certificate) is the DECODER
/// `unsupported`. A block that is present but corrupt keeps the reqwest text
/// (tolerance T8).
fn node_pem_error(cert: &[u8], key: &[u8]) -> Option<&'static str> {
    let has = |hay: &[u8], needles: &[&str]| {
        needles
            .iter()
            .any(|n| hay.windows(n.len()).any(|w| w == n.as_bytes()))
    };
    if !has(
        cert,
        &[
            "-----BEGIN CERTIFICATE-----",
            "-----BEGIN TRUSTED CERTIFICATE-----",
            "-----BEGIN X509 CERTIFICATE-----",
        ],
    ) {
        return Some("error:0480006C:PEM routines::no start line");
    }
    if !has(key, &["PRIVATE KEY-----"]) {
        return Some("error:1E08010C:DECODER routines::unsupported");
    }
    None
}

/// `fetch()` (`iot-creds.ts:73-131`): one mTLS GET.
async fn fetch(opts: &IotCredentialsOptions) -> Result<AwsCredentials, CredsError> {
    // `iot-creds.ts:75-76`: read synchronously, before any network I/O.
    let cert = std::fs::read(&opts.cert_path)
        .map_err(|e| CredsError(node_read_error(&e, &opts.cert_path)))?;
    let key = std::fs::read(&opts.key_path)
        .map_err(|e| CredsError(node_read_error(&e, &opts.key_path)))?;
    if let Some(msg) = node_pem_error(&cert, &key) {
        return Err(CredsError(msg.into()));
    }
    let mut pem = cert;
    pem.push(b'\n');
    pem.extend_from_slice(&key);
    let identity = reqwest::Identity::from_pem(&pem).map_err(|e| CredsError(error_chain(&e)))?;
    // `https.request` never follows a redirect (`iot-creds.ts:78`; review-wp5 #4).
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .identity(identity);
    if let Some(roots) = &opts.test_roots_pem {
        let certs = reqwest::Certificate::from_pem_bundle(roots)
            .map_err(|e| CredsError(error_chain(&e)))?;
        builder = builder.tls_certs_only(certs);
    }
    let client = builder.build().map_err(|e| CredsError(error_chain(&e)))?;

    let url = format!(
        "https://{}/role-aliases/{}/credentials",
        opts.credential_endpoint,
        encode_uri_component(&opts.role_alias)
    );
    let activity = Arc::new(Activity::new());
    let act = activity.clone();
    let request = async move {
        let res = client
            .get(url)
            .header("x-amzn-iot-thingname", &opts.thing_name)
            .send()
            .await?;
        act.touch();
        let status = res.status().as_u16();
        let body = crate::s3::read_body(res, &act).await?;
        Ok::<_, reqwest::Error>((status, body))
    };
    let timeout = opts.timeout.unwrap_or(DEFAULT_TIMEOUT);
    match run_idle(request, &activity, timeout, None).await {
        Err(Interrupted::TimedOut) => Err(CredsError("IoT credentials fetch timed out".into())),
        Err(Interrupted::Aborted) => Err(CredsError("The operation was aborted".into())),
        Ok(Err(e)) => Err(CredsError(node_transport_error(&e))),
        Ok(Ok((status, body))) => parse_credentials_response(status, &body),
    }
}

/// JS truthiness of a JSON value (`!c.accessKeyId` etc.).
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// The response half of `fetch()` (`iot-creds.ts:88-124`), pure: status +
/// raw body → creds or the TS rejection text.
/// - non-200 → `IoT credentials fetch failed: HTTP <status> <body[0..200]>`
///   (`slice` counts UTF-16 units);
/// - body not JSON → `IoT credentials response unparseable: <parser message>`
///   (serde_json's wording, not V8's — ambiguity A4);
/// - `null` body → the V8 TypeError text, which the TS catch wraps the same way;
/// - any of the four fields falsy → `IoT credentials response missing fields`.
pub fn parse_credentials_response(status: u16, body: &[u8]) -> Result<AwsCredentials, CredsError> {
    let body = String::from_utf8_lossy(body);
    if status != 200 {
        return Err(CredsError(format!(
            "IoT credentials fetch failed: HTTP {status} {}",
            js_slice_utf16(&body, 200)
        )));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|e| CredsError(format!("IoT credentials response unparseable: {e}")))?;
    if parsed.is_null() {
        return Err(CredsError(
            "IoT credentials response unparseable: Cannot read properties of null (reading 'credentials')"
                .into(),
        ));
    }
    let c = parsed.get("credentials").filter(|c| c.is_object());
    let field = |k: &str| c.and_then(|c| c.get(k));
    if !truthy(field("accessKeyId"))
        || !truthy(field("secretAccessKey"))
        || !truthy(field("sessionToken"))
        || !truthy(field("expiration"))
    {
        return Err(CredsError("IoT credentials response missing fields".into()));
    }
    // AWS always vends strings; a truthy non-string is treated as missing
    // (the TS would carry it through `String()` coercion) — ambiguity A5.
    let s = |k: &str| field(k).and_then(Value::as_str).map(str::to_string);
    match (
        s("accessKeyId"),
        s("secretAccessKey"),
        s("sessionToken"),
        s("expiration"),
    ) {
        (Some(access_key_id), Some(secret_access_key), Some(session_token), Some(exp)) => {
            Ok(AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token,
                expiration: xorgate_core::js::date_parse(&exp),
            })
        }
        _ => Err(CredsError("IoT credentials response missing fields".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_parse_matches_v8_on_the_iso_forms() {
        // The vended `expiration` goes through `xorgate_core::js::date_parse`.
        use xorgate_core::js::date_parse;
        // Values printed by node 24 `Date.parse(…)`.
        assert_eq!(date_parse("2026-10-09T13:00:00Z"), 1_791_550_800_000.0);
        assert_eq!(
            date_parse("2026-10-09T13:00:00.123+02:00"),
            1_791_543_600_123.0
        );
        assert_eq!(date_parse("2028-02-29T23:59:59.9999Z"), 1_835_481_599_999.0);
        assert_eq!(date_parse("2026-10-09"), 1_791_504_000_000.0);
        assert!(date_parse("garbage").is_nan());
        assert_eq!(date_parse("2026-02-30T00:00:00Z"), 1_772_409_600_000.0);
        assert_eq!(date_parse("2026-10-09T24:00:00Z"), 1_791_590_400_000.0);
        assert_eq!(date_parse("2026-10-09T13:00Z"), 1_791_550_800_000.0);
        assert_eq!(date_parse("+002026-10-09T13:00:00Z"), 1_791_550_800_000.0);
        assert!(date_parse("2026-10-32").is_nan());
        assert!(date_parse("").is_nan());
    }

    #[test]
    fn encode_uri_component_keeps_the_js_unreserved_set() {
        assert_eq!(
            encode_uri_component("Xorgate Device/Creds!~*'()é"),
            "Xorgate%20Device%2FCreds!~*'()%C3%A9"
        );
    }

    #[test]
    fn response_parse_wording() {
        let ok = br#"{"credentials":{"accessKeyId":"AK","secretAccessKey":"S","sessionToken":"T","expiration":"2026-10-09T13:00:00Z"}}"#;
        let c = parse_credentials_response(200, ok).unwrap();
        assert_eq!(c.access_key_id, "AK");
        assert_eq!(c.expiration, 1_791_550_800_000.0);
        assert!(!format!("{c:?}").contains("\"S\""), "secrets redacted");

        let long = "x".repeat(250);
        assert_eq!(
            parse_credentials_response(403, long.as_bytes())
                .unwrap_err()
                .0,
            format!("IoT credentials fetch failed: HTTP 403 {}", "x".repeat(200))
        );
        assert_eq!(
            parse_credentials_response(500, b"").unwrap_err().0,
            "IoT credentials fetch failed: HTTP 500 "
        );
        for body in [
            &br#"{"credentials":{"accessKeyId":"AK","secretAccessKey":"S","sessionToken":"","expiration":"x"}}"#[..],
            br#"{}"#,
            br#"[]"#,
            br#"42"#,
            br#"{"credentials":"str"}"#,
        ] {
            assert_eq!(
                parse_credentials_response(200, body).unwrap_err().0,
                "IoT credentials response missing fields"
            );
        }
        assert_eq!(
            parse_credentials_response(200, b"null").unwrap_err().0,
            "IoT credentials response unparseable: Cannot read properties of null (reading 'credentials')"
        );
        assert!(parse_credentials_response(200, b"not json")
            .unwrap_err()
            .0
            .starts_with("IoT credentials response unparseable: "));
        // Unparseable expiration → NaN (always re-fetch), not an error.
        let nan = br#"{"credentials":{"accessKeyId":"AK","secretAccessKey":"S","sessionToken":"T","expiration":"soon"}}"#;
        assert!(parse_credentials_response(200, nan)
            .unwrap()
            .expiration
            .is_nan());
    }

    fn provider_with(fetch_fn: FetchFn) -> IotCredentialsProvider {
        let mut o = IotCredentialsOptions::new("h", "a", "t", "/nope/c", "/nope/k");
        o.fetch_fn = Some(fetch_fn);
        o.now = Some(Arc::new(|| 0.0));
        IotCredentialsProvider::new(o)
    }

    fn creds() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AK".into(),
            secret_access_key: "S".into(),
            session_token: "T".into(),
            expiration: 3_600_000.0,
        }
    }

    #[tokio::test]
    async fn a_panicked_fetch_does_not_wedge_later_gets() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let p = provider_with(Arc::new(move || {
            let n = c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    panic!("fetch task panics (test)");
                }
                Ok(creds())
            })
        }));
        // The caller awaiting the panicked fetch sees it fail...
        assert_eq!(
            p.get().await.unwrap_err().0,
            "IoT credentials fetch aborted"
        );
        assert!(p.state.lock().unwrap().inflight.is_none());
        // ...and the next call starts a fresh fetch instead of re-reading the
        // dead handle.
        assert_eq!(p.get().await.unwrap(), creds());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_cancelled_fetch_does_not_wedge_later_gets() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let p = provider_with(Arc::new(move || {
            let n = c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    std::future::pending::<()>().await;
                }
                Ok(creds())
            })
        }));
        // First runtime: the fetch starts and hangs; the caller gives up and
        // the runtime shuts down, cancelling the spawned fetch task.
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let p1 = p.clone();
            rt.block_on(async move {
                let r = tokio::time::timeout(Duration::from_millis(20), p1.get()).await;
                assert!(r.is_err(), "first fetch never completes");
            });
            assert!(p.state.lock().unwrap().inflight.is_some());
        }
        assert!(p.state.lock().unwrap().inflight.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Second runtime: a later get() refetches.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(rt.block_on(p.get()).unwrap(), creds());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn missing_cert_is_the_node_enoent_text() {
        let e = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            node_read_error(&e, std::path::Path::new("/nope")),
            "ENOENT: no such file or directory, open '/nope'"
        );
    }
}
