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

use crate::s3::{error_chain, js_slice_utf16, run_idle, Activity, Interrupted};
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
                    let state = self.state.clone();
                    tokio::spawn(async move {
                        let res = match &opts.fetch_fn {
                            Some(f) => f().await,
                            None => fetch(&opts).await,
                        };
                        // `.finally(() => { this.inflight = null; })` runs
                        // before any awaiting caller resumes.
                        state.lock().unwrap_or_else(|e| e.into_inner()).inflight = None;
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

/// `fetch()` (`iot-creds.ts:73-131`): one mTLS GET.
async fn fetch(opts: &IotCredentialsOptions) -> Result<AwsCredentials, CredsError> {
    // `iot-creds.ts:75-76`: read synchronously, before any network I/O.
    let cert = std::fs::read(&opts.cert_path)
        .map_err(|e| CredsError(node_read_error(&e, &opts.cert_path)))?;
    let key = std::fs::read(&opts.key_path)
        .map_err(|e| CredsError(node_read_error(&e, &opts.key_path)))?;
    let mut pem = cert;
    pem.push(b'\n');
    pem.extend_from_slice(&key);
    let identity = reqwest::Identity::from_pem(&pem).map_err(|e| CredsError(error_chain(&e)))?;
    let mut builder = reqwest::Client::builder().no_proxy().identity(identity);
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
        Ok(Err(e)) => Err(CredsError(error_chain(&e))),
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
                expiration: date_parse(&exp),
            })
        }
        _ => Err(CredsError("IoT credentials response missing fields".into())),
    }
}

/// `Date.parse` for the ECMAScript Date Time String Format (ES2023 §21.4.1.32):
/// `YYYY[-MM[-DD]][THH:mm[:ss[.sss…]]][Z|±HH:mm]`, `±YYYYYY` years. A
/// date-time without an offset is local time in JS; it is read as UTC here
/// (accepted divergence "Date.parse without offset", tolerance T7). Anything
/// else (V8's legacy fallback forms) is `NaN`. AWS vends
/// `2026-10-09T13:00:00Z`. Private until integration (`xorgate_core::js::date_parse`).
fn date_parse(s: &str) -> f64 {
    parse_iso(s).unwrap_or(f64::NAN)
}

fn parse_iso(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let num = |i: &mut usize, n: usize| -> Option<f64> {
        let d = b.get(*i..*i + n)?;
        if !d.iter().all(u8::is_ascii_digit) {
            return None;
        }
        *i += n;
        std::str::from_utf8(d).ok()?.parse::<f64>().ok()
    };
    let year = match b.first()? {
        b'+' | b'-' => {
            let neg = b[0] == b'-';
            i = 1;
            let y = num(&mut i, 6)?;
            if neg && y == 0.0 {
                return None; // -000000 is invalid
            }
            if neg {
                -y
            } else {
                y
            }
        }
        _ => num(&mut i, 4)?,
    };
    let (mut month, mut day) = (1.0, 1.0);
    if b.get(i) == Some(&b'-') {
        i += 1;
        month = num(&mut i, 2)?;
        if b.get(i) == Some(&b'-') {
            i += 1;
            day = num(&mut i, 2)?;
        }
    }
    let (mut h, mut mi, mut sec, mut ms) = (0.0, 0.0, 0.0, 0.0);
    let mut offset_min = 0.0;
    if b.get(i) == Some(&b'T') {
        i += 1;
        h = num(&mut i, 2)?;
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        mi = num(&mut i, 2)?;
        if b.get(i) == Some(&b':') {
            i += 1;
            sec = num(&mut i, 2)?;
            if b.get(i) == Some(&b'.') {
                i += 1;
                let start = i;
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                // Milliseconds = the first three digits (V8 truncates the rest).
                let frac = &s[start..i];
                let mut digits: String = frac.chars().take(3).collect();
                while digits.len() < 3 {
                    digits.push('0');
                }
                ms = digits.parse::<f64>().ok()?;
            }
        }
        match b.get(i) {
            Some(b'Z') => i += 1,
            Some(&c @ (b'+' | b'-')) => {
                i += 1;
                let oh = num(&mut i, 2)?;
                if b.get(i) != Some(&b':') {
                    return None;
                }
                i += 1;
                let om = num(&mut i, 2)?;
                if oh > 23.0 || om > 59.0 {
                    return None;
                }
                offset_min = (oh * 60.0 + om) * if c == b'+' { 1.0 } else { -1.0 };
            }
            _ => {}
        }
    }
    if i != b.len() {
        return None;
    }
    // V8 range-checks the day against 1..=31 only and lets MakeDay roll
    // `2026-02-30` over into March (node 24: 1772409600000).
    if !(1.0..=12.0).contains(&month)
        || !(1.0..=31.0).contains(&day)
        || h > 24.0
        || mi > 59.0
        || sec > 59.0
        || (h == 24.0 && (mi, sec, ms) != (0.0, 0.0, 0.0))
    {
        return None;
    }
    let t =
        xorgate_core::js::date_utc(year, month - 1.0, day, h, mi, sec) + ms - offset_min * 60_000.0;
    (t.abs() <= 8.64e15).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_parse_matches_v8_on_the_iso_forms() {
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

    #[test]
    fn missing_cert_is_the_node_enoent_text() {
        let e = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            node_read_error(&e, std::path::Path::new("/nope")),
            "ENOENT: no such file or directory, open '/nope'"
        );
    }
}
