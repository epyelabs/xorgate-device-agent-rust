//! Generic segment uploader (`src/recording/uploader.ts` @ `89b5c64`): a
//! single-flight queue that drains closed recording segments to S3 while the
//! device is online and `recording.upload.mode === "auto"`.
//!
//! Rules (README shared contracts), all as the TS:
//! - one transfer at a time; sources by priority (telemetry 0, video 1),
//!   oldest-first within a source;
//! - a video unit PUTs its sidecar first, then the mp4, under ONE abort
//!   token; the local files are deleted only after both PUTs resolve (ENOENT
//!   tolerated);
//! - pause on disconnect aborts the in-flight PUT (both files stay);
//! - backoff 2 s → ×2 → 5 min on failure, reset on success / pause.
//!
//! The drain runs on its own tokio task (the recorder is never blocked) and
//! every interval is the tokio monotonic clock (P8); the wall clock only
//! stamps `lastUploadAt`. No uploader lock is held while a caller-supplied
//! closure (`is_online`, `mode`, `on_*`) runs (P9).
//!
//! Deviation (tolerance T4, one tick): `kick()` marks the drain running
//! synchronously (so a second kick is a no-op, as in the TS) but its first
//! pick happens when the spawned task first runs, not inside the `kick()`
//! call as V8's synchronous async-function prefix does.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use xorgate_cloud::s3::{ObjectPut, PutError, UploadOptions};
use xorgate_core::clock::Clock;
use xorgate_core::settings::UploadMode;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::upload_step::{
    failure_event, may_continue, may_kick, on_failure, plan_retry, skip_unrecognized_event,
    FailureStep, BACKOFF_MAX_MS, BACKOFF_MIN_MS,
};
use xorgate_hal::io::TapeSink;

pub use super::sources::{
    telemetry_source, video_source, ActiveSessionIds, SegmentSource, SidecarRef, UploadSegment,
    VideoSourceOpts,
};

/// `UploaderStatus` (`uploader.ts:52-55`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UploaderStatus {
    pub uploading: bool,
    /// Epoch ms of the last confirmed upload+delete.
    pub last_upload_at: Option<i64>,
}

pub type OnStatus = Arc<dyn Fn(UploaderStatus) + Send + Sync>;
pub type OnUploaded = Arc<dyn Fn(&UploadSegment, &str) + Send + Sync>;
pub type OnEvent = Arc<dyn Fn(ActivityLevel, &str) + Send + Sync>;
pub type Gate = Arc<dyn Fn() -> bool + Send + Sync>;
pub type ModeFn = Arc<dyn Fn() -> UploadMode + Send + Sync>;

/// `UploaderOptions` (`uploader.ts:57-71`).
pub struct UploaderOpts {
    pub sources: Vec<Arc<dyn SegmentSource>>,
    /// `PutFn`: one object PUT, `cancel` = the unit's abort signal.
    pub put: Arc<dyn ObjectPut>,
    /// MQTT-connected gate (`worker.ts:411`), read before/during every unit.
    pub is_online: Gate,
    /// Live `recording.upload.mode` (`manager.ts:261`). Kept apart from
    /// `is_online` on purpose: a failure is a "pause" only when OFFLINE
    /// (`uploader.ts:200`; wp3.md A1).
    pub mode: ModeFn,
    pub on_status: Option<OnStatus>,
    /// After each confirmed upload+delete (manager: refresh backlog / enforce).
    pub on_uploaded: Option<OnUploaded>,
    pub on_event: Option<OnEvent>,
    /// Test knobs (`backoffMinMs`/`backoffMaxMs`).
    pub backoff_min_ms: Option<u64>,
    pub backoff_max_ms: Option<u64>,
    /// `Date.now()` for `lastUploadAt` only.
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub tape: Arc<dyn TapeSink>,
}

#[derive(Default)]
struct State {
    running: bool,
    stopped: bool,
    uploading: bool,
    last_upload_at: Option<i64>,
    /// The in-flight unit's abort token (`this.aborter`).
    aborter: Option<CancellationToken>,
    backoff_ms: u64,
    /// `this.retryTimer`: generation + the sleeping task.
    retry_timer: Option<(u64, tokio::task::AbortHandle)>,
    retry_gen: u64,
    /// Segment paths with unparseable names / unreadable files — never retried.
    skipped: HashSet<PathBuf>,
}

struct Shared {
    opts: UploaderOpts,
    min_ms: u64,
    max_ms: u64,
    /// `[...opts.sources].sort((a, b) => a.priority - b.priority)` (stable).
    sources: Vec<Arc<dyn SegmentSource>>,
    state: Mutex<State>,
    /// `true` while a drain runs (`settled()` waits for `false`).
    running_tx: watch::Sender<bool>,
    rt: tokio::runtime::Handle,
}

/// The single-flight uploader. Build it inside a tokio runtime.
pub struct Uploader {
    inner: Arc<Shared>,
}

impl Uploader {
    /// `new Uploader(opts)` (`uploader.ts:89-93`).
    pub fn new(opts: UploaderOpts) -> Self {
        let min_ms = opts.backoff_min_ms.unwrap_or(BACKOFF_MIN_MS);
        let max_ms = opts.backoff_max_ms.unwrap_or(BACKOFF_MAX_MS);
        let mut sources = opts.sources.clone();
        sources.sort_by_key(|s| s.priority());
        Self {
            inner: Arc::new(Shared {
                opts,
                min_ms,
                max_ms,
                sources,
                state: Mutex::new(State {
                    backoff_ms: min_ms,
                    ..State::default()
                }),
                running_tx: watch::channel(false).0,
                rt: tokio::runtime::Handle::current(),
            }),
        }
    }

    /// `status()` (`uploader.ts:95-97`).
    pub fn status(&self) -> UploaderStatus {
        self.inner.status()
    }

    /// `kick()` (`uploader.ts:103-111`): start (or continue) draining. Cheap
    /// and idempotent — on connect, segment close, settings change, slow timer.
    pub fn kick(&self) {
        self.inner.kick();
    }

    /// `pause()` (`uploader.ts:114-121`): reset the backoff for the reconnect,
    /// clear the retry timer, abort the in-flight unit.
    pub fn pause(&self) {
        self.inner.pause();
    }

    /// `stop()` (`uploader.ts:123-126`).
    pub fn stop(&self) {
        self.inner.lock().stopped = true;
        self.inner.pause();
    }

    /// `settled()` (`uploader.ts:132-134`): await the current drain (none
    /// running → immediately). Does not wait for an armed retry timer.
    pub async fn settled(&self) {
        let mut rx = self.inner.running_tx.subscribe();
        let _ = rx.wait_for(|running| !running).await;
    }
}

impl Drop for Uploader {
    /// The manager replaces an uploader with `stop()` first (`manager.ts:282`);
    /// a dropped one must not keep a retry timer alive either.
    fn drop(&mut self) {
        let timer = self.inner.lock().retry_timer.take();
        if let Some((_, h)) = timer {
            h.abort();
        }
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn status(&self) -> UploaderStatus {
        let st = self.lock();
        UploaderStatus {
            uploading: st.uploading,
            last_upload_at: st.last_upload_at,
        }
    }

    fn online(&self) -> bool {
        (self.opts.is_online)()
    }

    fn auto(&self) -> bool {
        (self.opts.mode)() == UploadMode::Auto
    }

    fn event(&self, level: ActivityLevel, msg: &str) {
        if let Some(f) = &self.opts.on_event {
            f(level, msg);
        }
    }

    fn kick(self: &Arc<Self>) {
        {
            let st = self.lock();
            if st.stopped || st.running {
                return;
            }
        }
        let (online, auto) = (self.online(), self.auto());
        let timer = {
            let mut st = self.lock();
            if !may_kick(st.stopped, st.running, online, auto) {
                return;
            }
            st.running = true;
            // Under the lock, so a finishing drain's `false` cannot overtake it.
            self.running_tx.send_replace(true);
            st.retry_timer.take()
        };
        if let Some((_, h)) = timer {
            h.abort(); // clearTimeout
        }
        let me = self.clone();
        self.rt.spawn(async move { me.drain().await });
    }

    fn pause(&self) {
        let (timer, aborter) = {
            let mut st = self.lock();
            st.backoff_ms = self.min_ms;
            (st.retry_timer.take(), st.aborter.clone())
        };
        if let Some((_, h)) = timer {
            h.abort();
        }
        if let Some(a) = aborter {
            a.cancel();
        }
    }

    /// `setUploading(v)` (`uploader.ts:235-239`): `onStatus` only on an edge.
    fn set_uploading(&self, v: bool) {
        let status = {
            let mut st = self.lock();
            if st.uploading == v {
                return;
            }
            st.uploading = v;
            UploaderStatus {
                uploading: st.uploading,
                last_upload_at: st.last_upload_at,
            }
        };
        if let Some(f) = &self.opts.on_status {
            f(status);
        }
    }

    /// `next()` (`uploader.ts:226-233`): the first unskipped segment of the
    /// first source (by priority) that has one. Sources are listed lazily —
    /// the video scan has side effects (reaps) and runs only when every
    /// higher-priority source is drained.
    fn next(&self) -> Option<(Arc<dyn SegmentSource>, UploadSegment)> {
        for source in &self.sources {
            let list = source.list();
            let st = self.lock();
            if let Some(seg) = list.into_iter().find(|s| !st.skipped.contains(&s.path)) {
                return Some((source.clone(), seg));
            }
        }
        None
    }

    fn skip(&self, path: &Path) {
        self.lock().skipped.insert(path.to_path_buf());
    }

    /// `drain()` (`uploader.ts:136-213`).
    ///
    /// The catch's `scheduleRetry()` is armed in the same critical section
    /// that clears `running` (after the finally's `setUploading(false)`): a
    /// JS timer cannot fire before the synchronous finally completes, and a
    /// tokio timer on another worker must not find the drain still "running"
    /// and drop the retry. The observable order — event, then `onStatus` —
    /// is the TS one.
    async fn drain(self: Arc<Self>) {
        let mut retry = false;
        if let Err(err) = self.drain_units().await {
            // The catch (`:198-206`).
            let stopped = self.lock().stopped;
            let online = self.online();
            let step = on_failure(stopped, online);
            let backoff = self.lock().backoff_ms;
            let (level, msg) = failure_event(&step, backoff, &err.0);
            self.event(level, &msg);
            retry = step == FailureStep::Retry;
        }
        // finally (`:207-210`).
        self.set_uploading(false);
        let mut st = self.lock();
        st.running = false;
        if retry {
            self.schedule_retry(&mut st);
        }
        self.running_tx.send_replace(false);
    }

    async fn drain_units(&self) -> Result<(), PutError> {
        loop {
            let stopped = self.lock().stopped;
            if !may_continue(stopped, self.online(), self.auto()) {
                return Ok(());
            }
            let Some((source, seg)) = self.next() else {
                return Ok(());
            };

            let Some(key) = source.s3_key_for(&seg) else {
                self.skip(&seg.path);
                self.event(ActivityLevel::Warn, &skip_unrecognized_event(&seg.name));
                continue;
            };

            // `this.aborter = new AbortController()` — armed BEFORE the reads
            // (the TS reads synchronously, so a pause() can only land during
            // the PUT; here the async read is an extra await point, and a
            // pause() landing in it must still abort this unit).
            let token = CancellationToken::new();
            self.lock().aborter = Some(token.clone());

            let bodies = read_unit(&seg).await;
            let Some((body, sidecar_body)) = bodies else {
                // Evicted or already gone — not an error, just move on.
                self.lock().aborter = None;
                self.skip(&seg.path);
                continue;
            };

            self.set_uploading(true);
            let res = self
                .put_unit(&source, &seg, &key, body, sidecar_body, &token)
                .await;
            self.lock().aborter = None;
            res?;

            let _ = std::fs::remove_file(&seg.path); // deleted concurrently (eviction) — fine
            if let Some(sc) = &seg.sidecar {
                let _ = std::fs::remove_file(&sc.path);
            }
            {
                let mut st = self.lock();
                st.last_upload_at = Some(self.opts.clock.wall_ms());
                st.backoff_ms = self.min_ms;
            }
            self.set_uploading(false);
            source.on_uploaded(&seg);
            if let Some(f) = &self.opts.on_uploaded {
                f(&seg, &key);
            }
        }
    }

    /// The unit's PUTs (`uploader.ts:167-179`): sidecar (if any) then main.
    async fn put_unit(
        &self,
        source: &Arc<dyn SegmentSource>,
        seg: &UploadSegment,
        key: &str,
        body: Bytes,
        sidecar_body: Option<Bytes>,
        token: &CancellationToken,
    ) -> Result<(), PutError> {
        if let (Some(sc), Some(sb)) = (&seg.sidecar, sidecar_body) {
            let r = self
                .opts
                .put
                .put(&sc.s3_key, sb.clone(), &sc.content_type, token)
                .await;
            self.tape_put(&sc.s3_key, &sc.content_type, &sb, &r);
            r?;
        }
        let ct = source.content_type().to_string();
        let r = self.opts.put.put(key, body.clone(), &ct, token).await;
        self.tape_put(key, &ct, &body, &r);
        r
    }

    /// The `s3_put` tape record (phase-3 §Record/replay), one per PUT.
    fn tape_put(&self, key: &str, content_type: &str, body: &Bytes, res: &Result<(), PutError>) {
        let _ = (key, content_type, body, res, &self.opts.tape);
    }

    /// `scheduleRetry()` (`uploader.ts:215-224`), with the state lock held.
    fn schedule_retry(self: &Arc<Self>, st: &mut State) {
        let Some(plan) = plan_retry(
            st.retry_timer.is_some(),
            st.stopped,
            st.backoff_ms,
            self.max_ms,
        ) else {
            return;
        };
        st.backoff_ms = plan.next_backoff_ms;
        st.retry_gen += 1;
        let gen = st.retry_gen;
        let weak: Weak<Self> = Arc::downgrade(self);
        let task = self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(plan.delay_ms)).await;
            let Some(me) = weak.upgrade() else { return };
            {
                let mut st = me.lock();
                // A clearTimeout that raced the wake-up wins.
                if st.retry_timer.as_ref().map(|(g, _)| *g) != Some(gen) {
                    return;
                }
                st.retry_timer = None;
            }
            me.kick();
        });
        st.retry_timer = Some((gen, task.abort_handle()));
    }
}

/// `readFileSync(seg.path)` + `readFileSync(seg.sidecar.path)`; any failure
/// is `None` (`uploader.ts:154-161`).
async fn read_unit(seg: &UploadSegment) -> Option<(Bytes, Option<Bytes>)> {
    let body = tokio::fs::read(&seg.path).await.ok()?;
    let sidecar = match &seg.sidecar {
        Some(sc) => Some(Bytes::from(tokio::fs::read(&sc.path).await.ok()?)),
        None => None,
    };
    Some((Bytes::from(body), sidecar))
}

/// `buildUploader(upload)` (`manager.ts:246-272`) minus the manager's own
/// callbacks, which the caller passes: the telemetry source over
/// `<root>/telemetry` and the video source over `<root>/video` (registered
/// even on camera-less hosts — it lists nothing without a tree and still
/// drains leftovers), then `extra_sources`; `put`/`is_online` from the
/// worker's [`UploadOptions`] (`xorgate_cloud::s3::build_upload_options`,
/// rebuilt on every re-arm — never a cached manifest, review-wp3 #4).
pub struct BuildUploader {
    pub device_id: String,
    /// The recordings root (`recordingsRoot()`).
    pub root: PathBuf,
    pub upload: UploadOptions,
    pub extra_sources: Vec<Arc<dyn SegmentSource>>,
    pub mode: ModeFn,
    pub active_session_ids: ActiveSessionIds,
    pub on_status: Option<OnStatus>,
    pub on_event: Option<OnEvent>,
    pub on_uploaded: Option<OnUploaded>,
    pub backoff_min_ms: Option<u64>,
    pub backoff_max_ms: Option<u64>,
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub tape: Arc<dyn TapeSink>,
}

/// See [`BuildUploader`].
pub fn build_uploader(b: BuildUploader) -> Uploader {
    let mut sources = vec![
        telemetry_source(&b.device_id, &b.root.join("telemetry")),
        video_source(VideoSourceOpts {
            device_id: b.device_id.clone(),
            video_root: b.root.join("video"),
            active_session_ids: b.active_session_ids,
            now_ms: None,
        }),
    ];
    sources.extend(b.extra_sources);
    Uploader::new(UploaderOpts {
        sources,
        put: b.upload.put,
        is_online: b.upload.is_online,
        mode: b.mode,
        on_status: b.on_status,
        on_uploaded: b.on_uploaded,
        on_event: b.on_event,
        backoff_min_ms: b.backoff_min_ms,
        backoff_max_ms: b.backoff_max_ms,
        clock: b.clock,
        tape: b.tape,
    })
}
