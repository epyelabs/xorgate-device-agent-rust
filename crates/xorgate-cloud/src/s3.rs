//! S3 PutObject over HTTPS — the transport half of `src/recording/s3-put.ts`
//! (`s3Put` :105-135) behind the `ObjectPut` seam the Phase 3 uploader
//! consumes (`PutFn`, `src/recording/uploader.ts:45-50`), plus the worker's
//! `buildUploadOptions` (`src/worker.ts:389-413`).
//!
//! NOT wired to any uploader in Phase 2 (scope boundary: recording is Phase
//! 3); `build_upload_options` is ready for that call site.
//!
//! Timeouts are node:https `timeout` semantics — a socket IDLE timeout, not a
//! total deadline: a 30 MB segment over a slow LTE link may take minutes as
//! long as bytes keep moving. [`run_idle`] reproduces that: activity is every
//! request-body chunk hyper pulls (it pulls only when the socket accepts more),
//! the response head, and every response-body chunk.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use xorgate_core::kvs_manifest::{upload_bucket_decision, KvsManifest};

use crate::iot_creds::{AwsCredentials, IotCredentialsOptions, IotCredentialsProvider};
use crate::sigv4::{sign_put, SignedRequest};

/// `s3Put` default `timeoutMs` (`s3-put.ts:23-24`, `:115`).
pub const S3_PUT_TIMEOUT: Duration = Duration::from_millis(60_000);

/// Request-body chunk size handed to hyper (one activity tick per chunk).
const BODY_CHUNK: usize = 64 * 1024;

/// A rejected PUT: `Display` is the TS `Error.message` (`S3 PUT failed: HTTP
/// 403 …`, `S3 PUT timed out`, `The operation was aborted`, a creds error, or
/// a transport error — see wp5.md ambiguity A4 for the transport wording).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PutError(pub String);

pub type PutFuture<'a> = Pin<Box<dyn Future<Output = Result<(), PutError>> + Send + 'a>>;

/// The uploader seam — `PutFn` (`uploader.ts:45-50`): one object PUT,
/// resolved on success, rejected with a message otherwise; `cancel` is the
/// uploader's `AbortSignal` (pause()/stop()).
pub trait ObjectPut: Send + Sync {
    fn put<'a>(
        &'a self,
        key: &'a str,
        body: Bytes,
        content_type: &'a str,
        cancel: &'a CancellationToken,
    ) -> PutFuture<'a>;
}

/// The `put` closure of `buildUploadOptions` (`worker.ts:406-410`): creds
/// from the shared provider, then one signed PUT into `bucket`.
pub struct S3Put {
    pub region: String,
    pub bucket: String,
    pub creds: IotCredentialsProvider,
    client: Mutex<Option<reqwest::Client>>,
}

impl S3Put {
    pub fn new(
        region: impl Into<String>,
        bucket: impl Into<String>,
        creds: IotCredentialsProvider,
    ) -> Self {
        Self {
            region: region.into(),
            bucket: bucket.into(),
            creds,
            client: Mutex::new(None),
        }
    }

    /// One pooled client, built on first use so construction can never fail
    /// (`buildUploadOptions` cannot); a build failure surfaces as a PUT error.
    fn client(&self) -> Result<reqwest::Client, PutError> {
        let mut slot = self.client.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = &*slot {
            return Ok(c.clone());
        }
        let c = https_client().map_err(|e| PutError(error_chain(&e)))?;
        *slot = Some(c.clone());
        Ok(c)
    }
}

impl ObjectPut for S3Put {
    /// `worker.ts:406-410`: `const credentials = await creds.get(); await
    /// s3Put({ region, bucket, key, body, contentType, credentials, signal })`.
    /// The creds fetch is NOT abortable (the TS passes no signal to it).
    fn put<'a>(
        &'a self,
        key: &'a str,
        body: Bytes,
        content_type: &'a str,
        cancel: &'a CancellationToken,
    ) -> PutFuture<'a> {
        Box::pin(async move {
            let credentials = self.creds.get().await.map_err(|e| PutError(e.0))?;
            let client = self.client()?;
            s3_put(
                &client,
                S3PutOptions {
                    region: &self.region,
                    bucket: &self.bucket,
                    key,
                    body,
                    content_type,
                    credentials: &credentials,
                    cancel: Some(cancel),
                    timeout: None,
                    now_ms: None,
                },
            )
            .await
        })
    }
}

/// The client node:https behaves like: no env proxy, HTTP/1.1, platform roots;
/// redirects are never followed (`https.request` does not, `s3-put.ts:109`;
/// reqwest would re-issue a 303 as a GET — review-wp5 #4); idle pooled
/// sockets close after 5 s like Node ≥ 19's `globalAgent` (`keepAlive`,
/// `timeout: 5000`), so a server-closed socket is not reused (review-wp5 #6).
pub fn https_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(5))
        .build()
}

/// `S3PutOptions` (`s3-put.ts:14-27`).
pub struct S3PutOptions<'a> {
    pub region: &'a str,
    pub bucket: &'a str,
    /// Object key, no leading slash.
    pub key: &'a str,
    pub body: Bytes,
    pub content_type: &'a str,
    pub credentials: &'a AwsCredentials,
    pub cancel: Option<&'a CancellationToken>,
    /// Idle timeout; default [`S3_PUT_TIMEOUT`].
    pub timeout: Option<Duration>,
    /// Epoch-ms clock override for tests.
    pub now_ms: Option<f64>,
}

fn wall_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// `s3Put` (`s3-put.ts:105-135`): sign (`Date.now()` at call time), then PUT
/// to `https://<host><path>`; `Ok` on exactly HTTP 200, else
/// `S3 PUT failed: HTTP <status> <body[0..300]>`.
pub async fn s3_put(client: &reqwest::Client, opts: S3PutOptions<'_>) -> Result<(), PutError> {
    let signed = sign_put(
        opts.region,
        opts.bucket,
        opts.key,
        &opts.body,
        opts.content_type,
        opts.credentials.signing(),
        opts.now_ms.unwrap_or_else(wall_now_ms),
    );
    let base = format!("https://{}", signed.host);
    put_signed(
        client,
        &base,
        &signed,
        opts.body,
        opts.timeout.unwrap_or(S3_PUT_TIMEOUT),
        opts.cancel,
    )
    .await
}

/// The request half of `s3Put` against `base` (the virtual host in
/// production; a local test server in tests). Every signed header is sent as
/// signed, `content-length` included, so the body goes out un-chunked.
#[doc(hidden)]
pub async fn put_signed(
    client: &reqwest::Client,
    base: &str,
    signed: &SignedRequest,
    body: Bytes,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> Result<(), PutError> {
    let activity = Arc::new(Activity::new());
    let mut req = client.put(format!("{base}{}", signed.path));
    for (k, v) in &signed.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let reader = TrackingReader {
        inner: std::io::Cursor::new(body),
        activity: activity.clone(),
    };
    let stream = tokio_util::io::ReaderStream::with_capacity(reader, BODY_CHUNK);
    let req = req.body(reqwest::Body::wrap_stream(stream));
    let act = activity.clone();
    let request = async move {
        let res = req.send().await?;
        act.touch();
        let status = res.status().as_u16();
        let body = read_body(res, &act).await?;
        Ok::<_, reqwest::Error>((status, body))
    };
    match run_idle(request, &activity, timeout, cancel).await {
        Err(Interrupted::TimedOut) => Err(PutError("S3 PUT timed out".into())),
        Err(Interrupted::Aborted) => Err(PutError("The operation was aborted".into())),
        Ok(Err(e)) => Err(PutError(error_chain(&e))),
        Ok(Ok((200, _))) => Ok(()),
        Ok(Ok((status, body))) => Err(PutError(format!(
            "S3 PUT failed: HTTP {status} {}",
            js_slice_utf16(&String::from_utf8_lossy(&body), 300)
        ))),
    }
}

/// `RecordingUploadOptions` minus the test-only knobs (`manager.ts:81-91`):
/// the PUT seam and the MQTT-connected gate.
#[derive(Clone)]
pub struct UploadOptions {
    pub put: Arc<dyn ObjectPut>,
    /// `isOnline: () => store.snapshot().connection === "connected"`.
    pub is_online: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// Inputs of `buildUploadOptions` (`worker.ts:389-413`) that the TS reads from
/// `config` / `process.env`.
pub struct UploadEnv<'a> {
    /// `config.offline`.
    pub offline: bool,
    /// `process.env.XORGATE_UPLOAD_BUCKET` (empty counts as unset: `||`).
    pub env_upload_bucket: Option<&'a str>,
    /// `config.deviceId` (the IoT thing name).
    pub thing_name: &'a str,
    pub cert_path: &'a std::path::Path,
    pub key_path: &'a std::path::Path,
}

/// `buildUploadOptions` (`worker.ts:389-413`), side effects in TS order:
/// offline → `None` (no manifest read, no log); then `load_manifest()`
/// (`xorgate_hal::kvs_manifest_io::load_manifest(config.media.manifestPath)`);
/// a missing manifest or bucket logs `[agent] segment upload disabled (…)`
/// and returns `None`; otherwise a fresh `IotCredentialsProvider` (manifest
/// endpoint + alias, `thingName = deviceId`) behind an [`S3Put`].
pub fn build_upload_options(
    env: &UploadEnv<'_>,
    load_manifest: impl FnOnce() -> Option<KvsManifest>,
    is_online: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Option<UploadOptions> {
    if env.offline {
        return None;
    }
    let manifest = load_manifest();
    let bucket = match upload_bucket_decision(manifest.as_ref(), env.env_upload_bucket) {
        Ok(bucket) => bucket,
        Err(reason) => {
            tracing::info!("[agent] segment upload disabled ({reason})");
            return None;
        }
    };
    let manifest = manifest?;
    let creds = IotCredentialsProvider::new(IotCredentialsOptions::new(
        manifest.credential_endpoint.clone(),
        manifest.role_alias.clone(),
        env.thing_name,
        env.cert_path,
        env.key_path,
    ));
    Some(UploadOptions {
        put: Arc::new(S3Put::new(manifest.region, bucket, creds)),
        is_online,
    })
}

// ---------------------------------------------------------------------------
// node:https-like plumbing shared with `iot_creds`.

/// Last-activity stamp for the idle watchdog (monotonic, tokio clock).
///
/// Known edges vs Node's socket `timeout` (review-wp5 #5, accepted): DNS,
/// TCP connect and the TLS handshake are not counted as activity (Node
/// resets on handshake bytes; hyper exposes no connect hook), and the last
/// body pull precedes the kernel hand-off by at most hyper's write buffer
/// (~408 KB). Both are negligible against the 15 s / 60 s budgets.
pub(crate) struct Activity {
    last: Mutex<Instant>,
}

impl Activity {
    pub(crate) fn new() -> Self {
        Self {
            last: Mutex::new(Instant::now()),
        }
    }

    pub(crate) fn touch(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    fn last(&self) -> Instant {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Resolves once nothing has touched `self` for `timeout`.
    async fn idle(&self, timeout: Duration) {
        loop {
            let deadline = self.last() + timeout;
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

pub(crate) enum Interrupted {
    /// `req.on("timeout")` → `req.destroy(new Error("… timed out"))`.
    TimedOut,
    /// `signal` aborted (also when already aborted at call time).
    Aborted,
}

/// Run `fut` under a node:https-style idle timeout and an optional abort
/// signal; dropping `fut` tears the connection down like `req.destroy()`.
pub(crate) async fn run_idle<T>(
    fut: impl Future<Output = T>,
    activity: &Activity,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> Result<T, Interrupted> {
    let cancelled = async {
        match cancel {
            Some(c) => c.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        _ = cancelled => Err(Interrupted::Aborted),
        _ = activity.idle(timeout) => Err(Interrupted::TimedOut),
        v = fut => Ok(v),
    }
}

/// Drain a response body, touching `activity` per chunk.
pub(crate) async fn read_body(
    mut res: reqwest::Response,
    activity: &Activity,
) -> reqwest::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        activity.touch();
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// A request body that stamps `activity` each time hyper pulls a chunk.
struct TrackingReader {
    inner: std::io::Cursor<Bytes>,
    activity: Arc<Activity>,
}

impl AsyncRead for TrackingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if r.is_ready() {
            self.activity.touch();
        }
        r
    }
}

/// An error and its `source()` chain joined with `": "` — the closest
/// stable rendering of reqwest/hyper/rustls failures (node prints e.g.
/// `getaddrinfo ENOTFOUND host`; wording differs, ambiguity A4).
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        let t = s.to_string();
        if !out.contains(&t) {
            out.push_str(": ");
            out.push_str(&t);
        }
        src = s.source();
    }
    out
}

/// `String.prototype.slice(0, n)`: the first `n` UTF-16 code units. A
/// surrogate pair cut in half decodes to U+FFFD where V8 would keep a lone
/// surrogate (accepted divergence "cut-at-64 surrogate", tolerance T7).
pub(crate) fn js_slice_utf16(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_counts_utf16_units() {
        assert_eq!(js_slice_utf16("héllo", 2), "hé");
        assert_eq!(js_slice_utf16("a😀b", 3), "a😀");
        assert_eq!(js_slice_utf16("a😀b", 2), "a\u{FFFD}");
        assert_eq!(js_slice_utf16("ab", 300), "ab");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timer_restarts_on_activity() {
        let a = Arc::new(Activity::new());
        let a2 = a.clone();
        let start = Instant::now();
        let work = async move {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_secs(40)).await;
                a2.touch();
            }
            "done"
        };
        // 5 × 40 s of steady activity under a 60 s idle timeout completes…
        assert!(matches!(
            run_idle(work, &a, Duration::from_secs(60), None).await,
            Ok("done")
        ));
        assert_eq!(start.elapsed(), Duration::from_secs(200));
        // …while 61 s of silence times out at exactly 60 s.
        let a = Activity::new();
        let start = Instant::now();
        let stall = tokio::time::sleep(Duration::from_secs(61));
        assert!(matches!(
            run_idle(stall, &a, Duration::from_secs(60), None).await,
            Err(Interrupted::TimedOut)
        ));
        assert_eq!(start.elapsed(), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn pre_aborted_signal_rejects_immediately() {
        let c = CancellationToken::new();
        c.cancel();
        let a = Activity::new();
        assert!(matches!(
            run_idle(
                std::future::pending::<()>(),
                &a,
                Duration::from_secs(60),
                Some(&c)
            )
            .await,
            Err(Interrupted::Aborted)
        ));
    }
}
