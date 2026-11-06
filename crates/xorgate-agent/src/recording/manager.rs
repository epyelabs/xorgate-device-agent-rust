//! `RecordingManager` — 1:1 port of `src/recording/manager.ts` (809 lines,
//! TS @ 89b5c64), the worker's single entry point for on-device recording:
//! resolves the `recording` namespace (5 s settings poll + `reapply` on every
//! config message), starts/stops the telemetry recorder, runs startup crash
//! recovery, drives the disk manager and the uploader, and owns the video
//! lifecycle: per-camera enables, the boot-defer / GPS gate, the load-shed
//! ladder, and THE live-parking choke point (invariant 6).
//!
//! Components (WP2 recorder/disk/time source, WP3 uploader, WP4 capture) are
//! reached through the small traits below ([`RecorderApi`], [`DiskApi`],
//! [`UploaderApi`], [`CaptureApi`], [`TimeSourceApi`]) built by one
//! [`RecordingBackend`]; the production backend adapts the real components,
//! tests inject fakes.
//!
//! # Concurrency (the TS is single-threaded; this keeps its atomicity)
//! - `state` (std `Mutex`) holds everything `apply`/`applyVideo` mutate; every
//!   TS synchronous block runs under ONE lock acquisition, never across an
//!   `.await`.
//! - `slots` (std `Mutex`, leaf) holds the live component handles; it is held
//!   only to clone an `Arc`, never while calling a component.
//! - Component callbacks that re-enter the manager (`onStatus`,
//!   `onSegmentClosed`, `onUploaded`) are posted to the manager's message task,
//!   so a component may call them from inside any method the manager invoked.
//! - Readers (`status`, `video_status`, the hooks) never take `state`: they read
//!   the [`RecordingSnapshot`] `watch` the manager publishes at every change
//!   plus the leaf slots (P9 — `report_hints` runs under ConfigSync's mutex).
//! - The capture stop→start handover chain (`videoTransition`) is one task
//!   draining a FIFO of [`Transition`]s in order.
//! - LOCK ORDER: `state` → `slots`; nothing takes `state` while holding
//!   `slots`. `start_video` builds the capture AND calls `capture.start()`
//!   while holding `state` (the TS continuation after `stopAndWait` is
//!   atomic). That is safe only because no capture callback (`onStatus`,
//!   `onSegmentClosed`, `onActiveSegment`, `onSpawned`, `onEvent`) takes
//!   `state` synchronously: they reach the manager through the message task
//!   or the leaf `slots`. Keep it that way. A capture callback that locks
//!   `state` deadlocks the transition task (review-wp1 N2).
//!
//! # A throwing telemetry finalize (review-wp1 M1)
//! `stopRecorder` → `recorder.stop()` throws (`manager.ts:777-780`) out of
//! `apply()`/`reapply()`. [`RecordingManager::reapply`] returns that `Err`, and
//! each caller does what its TS call site does:
//! - the 5 s settings poll (`setInterval(() => this.reapply())`, `:319`): the
//!   throw is uncaught, so Node exits 1. Here the poll stops and sends the
//!   message on [`RecordingManager::take_fatal`]. The worker logs
//!   `[agent] fatal: <msg>` and exits 1 through its normal shutdown. systemd
//!   restarts it, and the boot orphan finalize completes the stranded segment.
//! - a config message (`worker.ts:881`, inside the `:864-893` try): the
//!   [`PhaseHooks::reapply_recording`](crate::ctx::PhaseHooks::reapply_recording)
//!   `Err` lands in `[agent] cloud config apply failed <msg>` plus the warn
//!   event.
//! - a hardware change (the `.then` at `:845-847`): `[agent] hardware
//!   reconcile failed <msg>`.
//! - shutdown (`:1306`): D29. The TS throw is uncaught and Node exits 1. Rust
//!   logs it and still stops capture and finishes the shutdown.
//!
//! TIME (P8): one `&dyn Clock` (the agent's `AgentClock`) for both policies;
//! the wall clock only stamps `liveBlock.since` and session ids.
//!
//! Not ported (P14): `cycleVideo`, `rotateTelemetrySession`,
//! `forceBootPolicyOff` and the replay-mode video hooks (`manager.ts:373-389`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use indexmap::IndexMap;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use xorgate_cloud::s3::UploadOptions;
use xorgate_core::boot_policy::{
    BootDeferPolicy, BootDeferSources, BootDeferVerdict, LoadShedPolicy, ShedEventLevel, ShedLevel,
    ShedVerdict,
};
use xorgate_core::clock::Clock;
use xorgate_core::hardware_block::EncoderToken;
use xorgate_core::recording_config::{
    resolve_recording_config, ResolvedRecordingConfig, ResolvedRetention,
};
use xorgate_core::recording_flow::{self as flow, CapturePlan, VideoShape};
use xorgate_core::session::mint_session_id;
use xorgate_core::settings::UploadMode;
use xorgate_core::telemetry::{ActivityLevel, LiveBlock, RecordingStatus};
use xorgate_core::types::{PowerSampleReading, TimeSource};
use xorgate_core::video_capture::CaptureCamera;
use xorgate_core::video_presets::{resolve_video_preset, VideoPreset};
use xorgate_hal::settings_io::load_settings;

use crate::ctx::{BoxFut, GroupSample};
use crate::timer::SetInterval;

// ---------------------------------------------------------------------------
// Component seams
// ---------------------------------------------------------------------------

/// `DiskManager.stats()` (`disk.ts`): the backlog gauges (`counters.json`
/// re-read on every call).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BacklogStats {
    pub backlog_bytes: u64,
    pub backlog_segments: u64,
    pub dropped_segments: u64,
}

/// `Uploader.status()` (`uploader.ts:235`) — WP3's type.
pub use super::uploader::UploaderStatus;

/// `CaptureStatus` (`video-capture.ts:266-272`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureStatus {
    pub stream_key: String,
    /// `CaptureState` (`starting` | `running` | `backoff` | `parked` | …).
    pub state: String,
    pub session_id: Option<String>,
    /// True only in record mode once bytes are observed (not in bridge-only).
    pub recording: bool,
}

/// `VideoRecordingStatus` (`manager.ts:131-136`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoRecordingStatus {
    /// True while ≥1 camera is actively capturing to disk (`media.recording`).
    pub recording: bool,
    pub streams: Vec<CaptureStatus>,
}

/// The shared `TimeSourceTracker` (`session.ts:116-157`; WP2 `TimeSourceHandle`).
pub trait TimeSourceApi: Send + Sync {
    /// `current()`: the cached provenance (kicks a background refresh when stale).
    fn current(&self) -> TimeSource;
    /// `refresh()`: one probe; the manager bounds the startup await (2 s).
    fn refresh(&self) -> BoxFut<'_, ()>;
}

/// `TelemetryRecorder` (`telemetry-recorder.ts`; WP2).
pub trait RecorderApi: Send + Sync {
    fn start(&self);
    /// Synchronous finalize of the active segment (clean stop leaves no
    /// orphan). `Err` = the finalize threw (`TelemetryRecorder.stop()`).
    fn stop(&self) -> Result<(), String>;
    fn active_path(&self) -> Option<PathBuf>;
}

/// `DiskManager` (`disk.ts`; WP2).
pub trait DiskApi: Send + Sync {
    fn set_retention(&self, r: &ResolvedRetention);
    fn set_active_path(&self, owner: &str, p: Option<PathBuf>);
    fn stats(&self) -> BacklogStats;
    /// `enforce()` as a JS async call: the SYNCHRONOUS prefix (`disk.ts:114-122`
    /// — claim, counter reload, tree scan, free-bytes probe start) runs NOW,
    /// inside the caller; the returned future is the tail (review-wp2 N1).
    fn enforce(&self) -> BoxFut<'static, ()>;
}

/// `Uploader` (`uploader.ts`; WP3).
pub trait UploaderApi: Send + Sync {
    fn kick(&self);
    /// Abort the in-flight PUT (connectivity lost).
    fn pause(&self);
    fn stop(&self);
    fn status(&self) -> UploaderStatus;
}

/// `VideoCaptureSupervisor` (`video-capture.ts:395-1021`; WP4).
pub trait CaptureApi: Send + Sync {
    fn start(&self);
    /// In-place camera-set change (`setCameras`, Phase 6).
    fn set_cameras(&self, cameras: Vec<CaptureCamera>) -> BoxFut<'_, ()>;
    /// SIGINT every child (not awaited).
    fn stop(&self);
    /// Stop and wait until every child is gone (10 s grace → SIGKILL + reap).
    fn stop_and_wait(&self) -> BoxFut<'_, ()>;
    /// D41 agent-exit cleanup: SIGKILL + wait (≤ `bound` each) every child
    /// still alive, then unlink its shm segments; `(key, segments)` per
    /// child killed. Default: nothing (fakes).
    fn reap_at_exit(&self, _bound: std::time::Duration) -> BoxFut<'_, Vec<(String, Vec<String>)>> {
        Box::pin(async { Vec::new() })
    }
    fn active_session_ids(&self) -> HashSet<String>;
    fn any_recording(&self) -> bool;
    fn status(&self) -> Vec<CaptureStatus>;
}

/// `onEvent(level, msg)`.
pub type EventFn = Arc<dyn Fn(ActivityLevel, &str) + Send + Sync>;
/// A no-argument notification (`onStatus` / `onSegmentClosed` / `onUploaded`).
pub type NotifyFn = Arc<dyn Fn() + Send + Sync>;
/// `sampleGroups()` (the recorder's view of the collectors).
pub type SampleGroupsFn =
    Arc<dyn Fn() -> BoxFut<'static, IndexMap<String, GroupSample>> + Send + Sync>;
/// `onActiveSegment(streamKey, path)`.
pub type ActiveSegmentFn = Arc<dyn Fn(&str, Option<PathBuf>) + Send + Sync>;
/// `setLiveShed(shed)` — the choke point's actuator.
pub type LiveShedFn = Arc<dyn Fn(bool) + Send + Sync>;
/// `setDisabledCameras(keys)`.
pub type DisabledCamerasFn = Arc<dyn Fn(&[String]) + Send + Sync>;
/// `onStatus(status)`.
pub type StatusFn = Arc<dyn Fn(&RecordingStatus) + Send + Sync>;
/// `clockTrusted: () => boolean` (`manager.ts:190-197` @ 09da38d): the clock
/// trust gate (`ClockTrust::trusted`, which may flip to fallback).
pub type ClockTrustedFn = Arc<dyn Fn() -> bool + Send + Sync>;
/// `powerSample()` (the shed policy's ~1 Hz reading).
pub type PowerSampleFn = Arc<dyn Fn() -> BoxFut<'static, Option<PowerSampleReading>> + Send + Sync>;

/// What `startRecorder` (`manager.ts:744-776`) hands `new TelemetryRecorder`.
pub struct RecorderSpec {
    /// The recordings root (WP2 derives `<root>/telemetry/<sessionId>`).
    pub root: PathBuf,
    /// `<telemetry root>/<sessionId>`.
    pub dir: PathBuf,
    pub device_id: String,
    pub session_id: String,
    pub boot_id: String,
    pub rate_hz: u32,
    pub time_source: Arc<dyn TimeSourceApi>,
    pub sample_groups: SampleGroupsFn,
    /// `onSegmentClosed: () => void this.runEnforce()`.
    pub on_segment_closed: NotifyFn,
}

/// What `startVideo` (`manager.ts:702-741`) hands `new VideoCaptureSupervisor`.
pub struct CaptureSpec {
    pub device_id: String,
    pub cameras: Vec<CaptureCamera>,
    pub record: bool,
    pub preset: &'static VideoPreset,
    pub encoder: EncoderToken,
    pub segment_seconds: u32,
    pub stagger_ms: u64,
    pub video_root: PathBuf,
    pub shm_dir: PathBuf,
    pub time_source: Arc<dyn TimeSourceApi>,
    pub on_segment_closed: NotifyFn,
    /// `onActiveSegment(streamKey, path)` → `disk.setActivePath("video:<key>", path)`.
    pub on_active_segment: ActiveSegmentFn,
    pub on_status: NotifyFn,
    pub on_event: EventFn,
    /// `onSpawned(streamKey)` → bounce that stream's KVS master.
    pub on_spawned: Arc<dyn Fn(&str) + Send + Sync>,
}

/// What `buildUploader` (`manager.ts:246-278`) hands `new Uploader`.
pub struct UploaderSpec {
    pub device_id: String,
    /// The recordings root (`telemetryRoot(root)` / `videoRoot(root)` under it).
    pub root: PathBuf,
    /// `put` + `isOnline` (worker `buildUploadOptions`; `isOnline` = MQTT
    /// connected ONLY, `worker.ts:411` — the mode gate is `mode`, review-wp3 S1).
    pub upload: UploadOptions,
    /// `mode: () => this.cfg.upload.mode`.
    pub mode: Arc<dyn Fn() -> UploadMode + Send + Sync>,
    /// `this.videoCapture?.activeSessionIds() ?? new Set()` — the video
    /// source's `isSessionActive` (capture's live sessions are not uploaded).
    pub active_session_ids: super::sources::ActiveSessionIds,
    pub on_status: NotifyFn,
    pub on_event: EventFn,
    /// `onUploaded: () => void this.runEnforce()`.
    pub on_uploaded: NotifyFn,
}

/// Builds the components and runs the crash-recovery passes.
pub trait RecordingBackend: Send + Sync {
    /// `finalizeOrphanSegments(telemetryRoot)` — count finalized.
    fn finalize_orphan_segments(&self, telemetry_root: &Path) -> usize;
    /// `recoverOrphanVideoSessions(videoRoot)` — sidecars backfilled.
    fn recover_orphan_video_sessions(&self, video_root: &Path) -> usize;
    /// Accepted divergence D41 (no TS counterpart): the start-up sweep of
    /// `/dev/shm` — every shmsink area whose owner pid is not a live process
    /// (`xorgate_hal::shm_reaper::sweep_dead_owner_segments`), as
    /// `(name, owner pid)` per segment removed. Default: nothing.
    fn sweep_dead_shm_segments(&self) -> Vec<(String, u32)> {
        Vec::new()
    }
    /// `readBootId()` (stable per process).
    fn boot_id(&self) -> String;
    fn disk(
        &self,
        root: &Path,
        retention: &ResolvedRetention,
        on_event: EventFn,
    ) -> Arc<dyn DiskApi>;
    fn recorder(&self, spec: RecorderSpec) -> Arc<dyn RecorderApi>;
    fn uploader(&self, spec: UploaderSpec) -> Arc<dyn UploaderApi>;
    fn capture(&self, spec: CaptureSpec) -> Arc<dyn CaptureApi>;
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// `RecordingVideoOptions` (`manager.ts:93-129`) minus the replay hooks (P14)
/// and the test-only capture overrides (those live in the test backend).
#[derive(Clone)]
pub struct VideoOpts {
    /// The worker's media-enabled snapshot; false = never spawn capture.
    pub media_enabled: bool,
    /// Bridged streams with a camera identity (`resolveBridgedCameras`),
    /// re-resolved on every apply pass.
    pub cameras: Arc<dyn Fn() -> Vec<CaptureCamera> + Send + Sync>,
    /// The recording encoder from the hardware block, read per apply pass.
    pub encoder: Arc<dyn Fn() -> EncoderToken + Send + Sync>,
    /// `onCaptureSpawned(streamKey)` → `MediaSupervisor::restart_channel`.
    pub on_capture_spawned: Arc<dyn Fn(&str) + Send + Sync>,
    /// `setLiveShed(shed)` → `MediaSupervisor::set_shed` — called ONLY from
    /// the choke point in `apply_video`.
    pub set_live_shed: Option<LiveShedFn>,
    /// `setDisabledCameras(keys)` → `MediaSupervisor::set_disabled_streams`.
    pub set_disabled_cameras: Option<DisabledCamerasFn>,
}

/// `RecordingManagerOptions` (`manager.ts:138-190`).
pub struct RecordingManagerOpts {
    pub device_id: String,
    /// `recordingsRoot()` = `<home>/recordings`.
    pub root: PathBuf,
    /// `shmDir()` = `<home>/shm`.
    pub shm_dir: PathBuf,
    /// settings.json (re-read on the poll and on every `reapply`).
    pub settings_path: PathBuf,
    pub time_source: Arc<dyn TimeSourceApi>,
    /// Segment upload plumbing (CF33: the worker's boot `buildUploadOptions`).
    pub upload: Option<UploadOptions>,
    /// `None` = telemetry-only (no video at all).
    pub video: Option<VideoOpts>,
    pub sample_groups: SampleGroupsFn,
    /// `None` = sim/dev host: shedding inert.
    pub power_sample: Option<PowerSampleFn>,
    /// The boot policy's readers (budget, telemetry.json, GNSS view).
    pub boot_sources: Box<dyn BootDeferSources + Send>,
    /// Called whenever status may have changed.
    pub on_status: StatusFn,
    pub on_event: EventFn,
    /// The ONE monotonic source (P8) for both policies.
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub backend: Arc<dyn RecordingBackend>,
    /// Test overrides (`pollMs`, `shedSampleMs`, `timeProbeTimeoutMs`).
    pub poll_ms: Option<u64>,
    pub shed_sample_ms: Option<u64>,
    pub time_probe_timeout_ms: Option<u64>,
    /// The clock trust gate (Phase 5b, I5): the telemetry recorder is not
    /// started while this returns false; re-evaluated on every settings
    /// poll. `None` = always trusted (sim, tests).
    pub clock_trusted: Option<ClockTrustedFn>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// What the manager publishes for lock-free readers (P9): every field of
/// `status()` the manager owns. Disk and uploader gauges are read live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordingSnapshot {
    pub enabled: bool,
    pub session_id: Option<String>,
    pub boot_defer: Option<String>,
    pub power_shed: Option<String>,
    pub live_block: Option<LiveBlock>,
    /// `this.cfg.telemetry.enabled` — the `clockHold` half `status()` needs.
    pub telemetry_enabled: bool,
}

#[derive(Default)]
struct Slots {
    /// Mirror of `State::recorder` for `runEnforce`'s `this.recorder?.activePath()`,
    /// read without the state lock (enforce runs inside callbacks fired while
    /// the state lock is held, e.g. the finalize of `stopRecorder`).
    recorder: Option<Arc<dyn RecorderApi>>,
    disk: Option<Arc<dyn DiskApi>>,
    uploader: Option<Arc<dyn UploaderApi>>,
    capture: Option<Arc<dyn CaptureApi>>,
    /// The capture `stop()` retired at shutdown, kept for the agent-exit
    /// cleanup ([`RecordingManager::reap_capture_at_exit`]).
    exiting_capture: Option<Arc<dyn CaptureApi>>,
}

struct State {
    cfg: ResolvedRecordingConfig,
    stop_requested: bool,
    recorder: Option<Arc<dyn RecorderApi>>,
    session_id: Option<String>,
    active_rate_hz: Option<u32>,
    /// Effective video tuple EXCLUDING the camera set.
    video_key: String,
    /// Effective camera set (a change alone is applied in place).
    video_cameras_key: String,
    /// Last disabled-camera set published to the live supervisor.
    disabled_cameras_key: String,
    boot_policy: BootDeferPolicy,
    boot_sources: Box<dyn BootDeferSources + Send>,
    boot_defer_status: Option<String>,
    shed_policy: LoadShedPolicy,
    shed_timer: Option<CancellationToken>,
    shed_status: Option<String>,
    shed_level: ShedLevel,
    /// Whether the live KVS masters are parked (shed rung 1 or the gate).
    live_shed: bool,
    live_block: Option<LiveBlock>,
    /// powerShedding fingerprint — a change resets the ladder.
    shed_cfg_key: String,
    last_settings_key: String,
    disk_timer: Option<CancellationToken>,
    /// `clockHoldNoted` (`manager.ts:363` @ 09da38d): one "held" event per hold.
    clock_hold_noted: bool,
}

enum Msg {
    /// A component's `onStatus` → `notifyStatus()`.
    Status,
}

/// One link of the `videoTransition` promise chain (`manager.ts:228`).
enum Transition {
    /// Test/driver seam: resolves once every earlier link has run
    /// (`await manager.videoTransition`).
    Barrier(tokio::sync::oneshot::Sender<()>),
    StopAndWait(Arc<dyn CaptureApi>),
    SetCameras(Arc<dyn CaptureApi>, Vec<CaptureCamera>),
    Replace {
        prev: Option<Arc<dyn CaptureApi>>,
        key: String,
        cameras_key: String,
        cameras: Vec<CaptureCamera>,
        record: bool,
        preset: &'static VideoPreset,
        encoder: EncoderToken,
    },
}

struct Inner {
    device_id: String,
    root: PathBuf,
    shm_dir: PathBuf,
    settings_path: PathBuf,
    time_source: Arc<dyn TimeSourceApi>,
    video: Option<VideoOpts>,
    sample_groups: SampleGroupsFn,
    power_sample: Option<PowerSampleFn>,
    on_status: StatusFn,
    on_event: EventFn,
    clock: Arc<dyn Clock + Send + Sync>,
    backend: Arc<dyn RecordingBackend>,
    clock_trusted: Option<ClockTrustedFn>,
    poll_ms: u64,
    shed_sample_ms: u64,
    time_probe_timeout_ms: u64,

    state: Mutex<State>,
    slots: Mutex<Slots>,
    /// `this.cfg.upload.mode`, readable by the uploader without `state`.
    upload_mode: Mutex<UploadMode>,
    snapshot: watch::Sender<RecordingSnapshot>,
    msgs: mpsc::UnboundedSender<Msg>,
    transitions: mpsc::UnboundedSender<Transition>,
    /// Cancelled by `stop()`: every timer and the message task end.
    cancel: CancellationToken,
    /// An error the TS leaves uncaught (it crashes the process). The worker
    /// takes the receiver and exits non-zero (M1, the 5 s poll path).
    fatal_tx: mpsc::UnboundedSender<String>,
    fatal_rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    /// `Arc::new_cyclic` back-reference for callbacks and timers.
    self_weak: std::sync::Weak<Inner>,
}

/// The recording manager handle (cheap to clone).
#[derive(Clone)]
pub struct RecordingManager {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// 16 random bytes for `mintSessionId` (`randomBytes(16)`, `session.ts:26`).
fn random16() -> [u8; 16] {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut b).is_ok() {
            return b;
        }
    }
    // Fallback (never expected on Linux): time + pid mixed; uniqueness only.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (u128::from(std::process::id()) << 64);
    b.copy_from_slice(&n.to_le_bytes());
    b
}

/// A `setInterval(fn, period)` loop on `token`: first fire one period from
/// now, every next one a period after the previous FIRE (P-WP6-1: Node
/// re-arms from the fire instant; a fixed-rate tokio interval let a 999 ms
/// gap hand the shared 1 s `system.sample()` gate to the recorder). The work
/// is awaited before the next tick is polled; its duration does not move the
/// deadline (only a body longer than the period delays that fire — the TS
/// would overlap it, `void this.shedTick()`).
fn spawn_every(
    period_ms: u64,
    token: CancellationToken,
    mut f: impl FnMut() -> Option<BoxFut<'static, ()>> + Send + 'static,
) {
    tokio::spawn(async move {
        let period = Duration::from_millis(period_ms.max(1));
        let mut i = SetInterval::new(period);
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = i.tick() => {
                    if let Some(fut) = f() {
                        fut.await;
                    }
                }
            }
        }
    });
}

impl RecordingManager {
    /// `new RecordingManager(opts)` (`manager.ts:234-244`): resolve settings,
    /// fingerprint `powerShedding`, build the uploader when upload plumbing
    /// exists. Must be called inside the tokio runtime (it spawns the
    /// message and transition tasks).
    pub fn new(opts: RecordingManagerOpts) -> Self {
        let cfg = resolve_recording_config(load_settings(&opts.settings_path).recording.as_ref());
        let shed_cfg_key = serde_json::to_string(&cfg.power_shedding).unwrap_or_default();
        let clock = opts.clock.clone();
        let state = State {
            boot_policy: BootDeferPolicy::new(clock.as_ref(), None),
            shed_policy: LoadShedPolicy::new(clock.as_ref()),
            boot_sources: opts.boot_sources,
            stop_requested: false,
            recorder: None,
            session_id: None,
            active_rate_hz: None,
            video_key: String::new(),
            video_cameras_key: String::new(),
            disabled_cameras_key: String::new(),
            boot_defer_status: None,
            shed_timer: None,
            shed_status: None,
            shed_level: ShedLevel::Full,
            live_shed: false,
            live_block: None,
            shed_cfg_key,
            last_settings_key: String::new(),
            disk_timer: None,
            clock_hold_noted: false,
            cfg: cfg.clone(),
        };
        let (msgs, msg_rx) = mpsc::unbounded_channel();
        let (transitions, tr_rx) = mpsc::unbounded_channel();
        let (snapshot, _) = watch::channel(RecordingSnapshot::default());
        let (fatal_tx, fatal_rx) = mpsc::unbounded_channel();
        let inner = Arc::new_cyclic(|self_weak| Inner {
            self_weak: self_weak.clone(),
            device_id: opts.device_id,
            root: opts.root,
            shm_dir: opts.shm_dir,
            settings_path: opts.settings_path,
            time_source: opts.time_source,
            video: opts.video,
            sample_groups: opts.sample_groups,
            power_sample: opts.power_sample,
            on_status: opts.on_status,
            on_event: opts.on_event,
            clock,
            backend: opts.backend,
            clock_trusted: opts.clock_trusted,
            poll_ms: opts.poll_ms.unwrap_or(flow::SETTINGS_POLL_MS),
            shed_sample_ms: opts.shed_sample_ms.unwrap_or(flow::SHED_SAMPLE_MS),
            time_probe_timeout_ms: opts
                .time_probe_timeout_ms
                .unwrap_or(flow::TIME_PROBE_TIMEOUT_MS),
            state: Mutex::new(state),
            slots: Mutex::new(Slots::default()),
            upload_mode: Mutex::new(cfg.upload.mode),
            snapshot,
            msgs,
            transitions,
            cancel: CancellationToken::new(),
            fatal_tx,
            fatal_rx: Mutex::new(Some(fatal_rx)),
        });
        tokio::spawn(message_task(inner.clone(), msg_rx));
        tokio::spawn(transition_task(inner.clone(), tr_rx));
        if let Some(upload) = opts.upload {
            let u = inner.build_uploader(upload);
            lock(&inner.slots).uploader = Some(u);
        }
        RecordingManager { inner }
    }

    /// `setUpload` (`manager.ts:280-292`): arm (or re-arm) uploads live —
    /// the uploadBucket backfill. The old uploader is stopped and replaced
    /// (its PUT captured the old bucket); `None` is a no-op.
    pub fn set_upload(&self, upload: Option<UploadOptions>) {
        let Some(upload) = upload else { return };
        let old = lock(&self.inner.slots).uploader.take();
        if let Some(old) = old {
            old.stop();
        }
        let u = self.inner.build_uploader(upload);
        lock(&self.inner.slots).uploader = Some(u.clone());
        u.kick();
    }

    /// `start()` (`manager.ts:294-329`): the D41 `/dev/shm` sweep (Rust
    /// only), crash recovery (only when a previous run created the tree), the
    /// ≤2 s time-source probe, the first apply,
    /// the 5 s settings poll, and the initial uploader kick.
    pub async fn start(&self) {
        let inner = &self.inner;
        // D41: a killed predecessor's capture children strand their 20 MiB
        // shmsink areas until reboot (the TS has no sweep, P7). Before any
        // capture spawns, unlink those whose owner is dead — journal only,
        // never an activity event (no new cloud-visible shape).
        // ASSUMPTION (review-wp5 R-WP5-5): this agent shares the pid namespace
        // of every process that can own a segment in its /dev/shm (the bench
        // host; the rig container). A container that shares /dev/shm across
        // pid namespaces (--ipc=host, a bind-mounted /dev/shm) would see LIVE
        // owners as dead and unlink their segments — Phase 7 containers must
        // revisit this before running the agent that way.
        for (name, pid) in inner.backend.sweep_dead_shm_segments() {
            tracing::info!(
                "[recording] removed stale shm segment {name:?} (owner pid {pid} not running)"
            );
        }
        if inner.root.exists() {
            let finalized = inner
                .backend
                .finalize_orphan_segments(&inner.root.join("telemetry"));
            if finalized > 0 {
                inner.event(
                    ActivityLevel::Info,
                    &flow::orphans_finalized_event(finalized),
                );
            }
            let recovered = inner
                .backend
                .recover_orphan_video_sessions(&inner.root.join("video"));
            if recovered > 0 {
                inner.event(
                    ActivityLevel::Info,
                    &flow::sidecars_backfilled_event(recovered),
                );
            }
            let mut st = lock(&inner.state);
            inner.ensure_disk(&mut st);
        }
        lock(&inner.state).last_settings_key = inner.settings_key();
        // `Promise.race([refresh().catch(), timeout])`: the probe keeps running
        // in the background after the bound.
        let ts = inner.time_source.clone();
        let probe = tokio::spawn(async move { ts.refresh().await });
        let _ =
            tokio::time::timeout(Duration::from_millis(inner.time_probe_timeout_ms), probe).await;
        {
            let mut st = lock(&inner.state);
            if st.stop_requested {
                return;
            }
            // No recorder exists before the first apply, so `stopRecorder`
            // cannot run here. If it ever did, the TS `await recording.start()`
            // would reject at top level (exit 1), which the fatal route matches.
            if let Err(e) = inner.apply(&mut st) {
                let _ = inner.fatal_tx.send(e);
            }
        }
        let weak = Arc::downgrade(inner);
        let poll = inner.cancel.child_token();
        let poll_stop = poll.clone();
        spawn_every(inner.poll_ms, poll, move || {
            if let Some(i) = weak.upgrade() {
                // `setInterval(() => this.reapply())` (`manager.ts:319`): a
                // throw is uncaught. There is no `uncaughtException` handler,
                // only the `unhandledRejection` net at `worker.ts:656`, so
                // Node exits 1. Stop polling and hand the error to the worker.
                if let Err(e) = i.reapply() {
                    poll_stop.cancel();
                    let _ = i.fatal_tx.send(e);
                }
            }
            None
        });
        inner.kick_uploader();
    }

    /// `notifyConnectivity` (`manager.ts:330-334`): online → drain; offline →
    /// abort the in-flight PUT.
    pub fn notify_connectivity(&self, online: bool) {
        let u = lock(&self.inner.slots).uploader.clone();
        if let Some(u) = u {
            if online {
                u.kick();
            } else {
                u.pause();
            }
        }
    }

    /// `reapply()` (`manager.ts:336-349`). `Err` = `stopRecorder`'s finalize
    /// threw. Each caller handles it as its TS call site does (module docs, M1).
    pub fn reapply(&self) -> Result<(), String> {
        self.inner.reapply()
    }

    /// The receiver of errors the TS leaves uncaught (the 5 s poll's
    /// `reapply`). The worker takes it once and exits non-zero on a message.
    pub fn take_fatal(&self) -> Option<mpsc::UnboundedReceiver<String>> {
        lock(&self.inner.fatal_rx).take()
    }

    /// `stop()` (`manager.ts:351-372`): sync shutdown — timers off, uploader
    /// stopped, the active segment finalized, capture SIGINTed.
    pub fn stop(&self) {
        let _ = self.stop_checked();
    }

    /// [`stop`](Self::stop), returning the finalize error (D29, review-wp1
    /// X1): `Err` = the TS `recording.stop()` threw at `worker.ts:1306`, so
    /// the TS process crashed before its `online:false` publish and the
    /// shutdown must end the MQTT link the crash way (Last-Will). Capture is
    /// still stopped and the snapshot published either way.
    pub fn stop_checked(&self) -> Result<(), String> {
        let inner = &self.inner;
        let mut st = lock(&inner.state);
        st.stop_requested = true;
        inner.cancel.cancel();
        if let Some(t) = st.disk_timer.take() {
            t.cancel();
        }
        if let Some(t) = st.shed_timer.take() {
            t.cancel();
        }
        let uploader = lock(&inner.slots).uploader.clone();
        if let Some(u) = uploader {
            u.stop();
        }
        let finalized = inner.stop_recorder(&mut st, flow::STOP_SHUTDOWN);
        if let Err(e) = &finalized {
            // D29 (wp1.md): in the TS the throw escapes `stop()` and the
            // signal handler, so Node exits 1 before `videoCapture.stop()`.
            // Rust logs the same stand-in line as the poll path (`[agent]
            // fatal: <msg>`, in place of Node's uncaught dump) and still
            // stops capture and finishes the local shutdown; the caller ends
            // the MQTT link the crash way (X1).
            tracing::error!("[agent] fatal: {e}");
        }
        let capture = lock(&inner.slots).capture.take();
        if let Some(c) = capture {
            c.stop();
            lock(&inner.slots).exiting_capture = Some(c);
        }
        st.video_key.clear();
        st.video_cameras_key.clear();
        inner.publish(&st);
        finalized
    }

    /// D41 agent-exit cleanup (Rust only): after the shutdown sequence, kill
    /// and reap the capture children the shutdown `stop()` SIGINTed that are
    /// still alive, so none strands its /dev/shm segment. Journal only.
    pub async fn reap_capture_at_exit(&self, bound: Duration) {
        let c = lock(&self.inner.slots).exiting_capture.take();
        let Some(c) = c else { return };
        for (key, segs) in c.reap_at_exit(bound).await {
            tracing::info!(
                "[recording] capture {key} still running at exit: killed, {} shm segment(s) unlinked {segs:?}",
                segs.len()
            );
        }
    }

    /// `videoStatus()` (`manager.ts:390-395`).
    pub fn video_status(&self) -> VideoRecordingStatus {
        let c = lock(&self.inner.slots).capture.clone();
        match c {
            Some(c) => VideoRecordingStatus {
                recording: c.any_recording(),
                streams: c.status(),
            },
            None => VideoRecordingStatus::default(),
        }
    }

    /// `status()` (`manager.ts:397-419`) — lock-free (P9): the published
    /// snapshot + the live disk (counters.json re-read) and uploader gauges.
    pub fn status(&self) -> RecordingStatus {
        self.inner.compose(&self.inner.snapshot.borrow())
    }

    /// The manager-owned half of `status()` as a `watch` (P9).
    pub fn status_watch(&self) -> watch::Receiver<RecordingSnapshot> {
        self.inner.snapshot.subscribe()
    }

    /// Wait until every capture handover queued so far has run (`await
    /// this.videoTransition`). Test/driver seam.
    #[doc(hidden)]
    pub async fn settled(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.inner.transitions.send(Transition::Barrier(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    /// One shed-monitor tick now (`shedTick()`), whether or not the 1 s timer
    /// is armed. Test/driver seam (the fixture drives ticks explicitly).
    #[doc(hidden)]
    pub async fn shed_tick_now(&self) {
        self.inner.clone().shed_tick().await;
    }

    /// The current `videoKey` dedupe string. Test seam.
    #[doc(hidden)]
    pub fn video_key(&self) -> String {
        lock(&self.inner.state).video_key.clone()
    }
}

impl Inner {
    fn self_weak(&self) -> std::sync::Weak<Inner> {
        self.self_weak.clone()
    }

    fn event(&self, level: ActivityLevel, msg: &str) {
        (self.on_event)(level, msg);
    }

    fn settings_key(&self) -> String {
        let s = load_settings(&self.settings_path);
        serde_json::to_string(&s.recording).unwrap_or_default()
    }

    fn kick_uploader(&self) {
        let u = lock(&self.slots).uploader.clone();
        if let Some(u) = u {
            u.kick();
        }
    }

    /// `buildUploader` (`manager.ts:246-278`).
    fn build_uploader(self: &Arc<Self>, upload: UploadOptions) -> Arc<dyn UploaderApi> {
        let mode = {
            let w = Arc::downgrade(self);
            Arc::new(move || {
                w.upgrade()
                    .map(|i| *lock(&i.upload_mode))
                    .unwrap_or(UploadMode::Off)
            })
        };
        let active = {
            let w = Arc::downgrade(self);
            Arc::new(move || {
                let Some(i) = w.upgrade() else {
                    return HashSet::new();
                };
                let c = lock(&i.slots).capture.clone();
                c.map(|c| c.active_session_ids()).unwrap_or_default()
            })
        };
        self.backend.uploader(UploaderSpec {
            device_id: self.device_id.clone(),
            root: self.root.clone(),
            upload,
            mode,
            active_session_ids: active,
            on_status: self.status_notifier(),
            on_event: self.on_event.clone(),
            on_uploaded: self.enforcer(),
        })
    }

    /// `() => this.notifyStatus()` — one hop through the message task.
    fn status_notifier(&self) -> NotifyFn {
        let tx = self.msgs.clone();
        Arc::new(move || {
            let _ = tx.send(Msg::Status);
        })
    }

    /// The telemetry recorder's own `onSegmentClosed`: fired INSIDE the
    /// rotation/finalize under the recorder's lock (WP2), where the TS
    /// `this.recorder.activePath()` is null — so the recorder is not called
    /// back (it would deadlock); the active path is `None`, as in the TS.
    fn recorder_enforcer(&self) -> NotifyFn {
        let w = self.self_weak();
        Arc::new(move || {
            if let Some(i) = w.upgrade() {
                i.run_enforce_active(None);
            }
        })
    }

    /// `() => void this.runEnforce()` — called synchronously by the component.
    fn enforcer(&self) -> NotifyFn {
        let w = self.self_weak();
        Arc::new(move || {
            if let Some(i) = w.upgrade() {
                i.run_enforce();
            }
        })
    }

    /// `reapply()` (`manager.ts:336-349`): a changed namespace re-resolves
    /// and applies; unchanged still re-runs the video diff (the boot-defer
    /// verdict can flip on its own). Deviation: after `stop()` this is a
    /// no-op (the TS process exits; a late config message must not restart
    /// the recorder during the Rust shutdown window).
    fn reapply(&self) -> Result<(), String> {
        let settings = load_settings(&self.settings_path);
        let key = serde_json::to_string(&settings.recording).unwrap_or_default();
        let mut st = lock(&self.state);
        if st.stop_requested {
            return Ok(());
        }
        if key != st.last_settings_key {
            st.last_settings_key = key;
            st.cfg = resolve_recording_config(settings.recording.as_ref());
            *lock(&self.upload_mode) = st.cfg.upload.mode;
            return self.apply(&mut st);
        }
        self.apply_video(&mut st);
        // ... and the clock trust gate releases on its own too (NTP landed)
        // (`manager.ts:356-360` @ 09da38d).
        if st.cfg.telemetry.enabled && st.recorder.is_none() && self.clock_ok(&mut st) {
            self.start_recorder(&mut st);
            self.notify_status(&st);
        }
        Ok(())
    }

    /// `clockOk()` (`manager.ts:365-376` @ 09da38d): the clock trust gate,
    /// with its one "held" info event per hold.
    fn clock_ok(&self, st: &mut State) -> bool {
        let ok = self.clock_trusted.as_ref().is_none_or(|f| f());
        if ok {
            st.clock_hold_noted = false;
            return true;
        }
        if !st.clock_hold_noted {
            st.clock_hold_noted = true;
            self.event(ActivityLevel::Info, flow::CLOCK_HOLD_EVENT);
        }
        false
    }

    /// `apply()` (`manager.ts:424-456`). `Err` = `stopRecorder`'s finalize
    /// threw. The TS exception leaves `apply` before
    /// startRecorder/applyVideo/notify/kick, and so does the `?` here.
    fn apply(&self, st: &mut State) -> Result<(), String> {
        let disk = lock(&self.slots).disk.clone();
        if let Some(d) = disk {
            d.set_retention(&st.cfg.retention);
        }
        let shed_key = serde_json::to_string(&st.cfg.power_shedding).unwrap_or_default();
        if shed_key != st.shed_cfg_key {
            st.shed_cfg_key = shed_key;
            st.shed_policy.reset(self.clock.as_ref());
        }
        let plan = flow::plan_telemetry(
            st.active_rate_hz,
            st.cfg.telemetry.enabled,
            st.cfg.telemetry.rate_hz,
        );
        if let Some(why) = plan.stop {
            self.stop_recorder(st, why)?;
        }
        // `want && !this.recorder && this.clockOk()` (`manager.ts:476` @ 09da38d).
        if plan.start && self.clock_ok(st) {
            self.start_recorder(st);
        }
        self.apply_video(st);
        self.notify_status(st);
        self.kick_uploader();
        Ok(())
    }

    /// `applyVideo()` (`manager.ts:457-594`) — the choke point lives here.
    fn apply_video(&self, st: &mut State) {
        let Some(vo) = self.video.clone() else { return };
        // Per-camera capacity: a disabled camera gets no pipeline at all.
        let disabled = flow::disabled_cameras(&st.cfg.video.cameras);
        let disabled_key = flow::disabled_key(&disabled);
        if disabled_key != st.disabled_cameras_key {
            st.disabled_cameras_key = disabled_key;
            if let Some(f) = &vo.set_disabled_cameras {
                f(&disabled);
            }
            if let Some(msg) = flow::cameras_disabled_event(&disabled) {
                self.event(ActivityLevel::Info, &msg);
            }
        }
        let resolved = if vo.media_enabled {
            (vo.cameras)()
        } else {
            Vec::new()
        };
        let cameras = flow::enabled_cameras(resolved, &st.cfg.video.cameras);
        let shed = self.update_shed_monitor(st, !cameras.is_empty());
        let mut boot_allow = true;
        let mut gate_holds_live = false;
        let mut gate_why = String::new();
        if !cameras.is_empty() {
            let bcfg = flow::boot_defer_config(&st.cfg.video);
            let verdict =
                st.boot_policy
                    .evaluate(&bcfg, self.clock.as_ref(), st.boot_sources.as_mut());
            boot_allow = verdict.allow;
            gate_holds_live = verdict.hold_live;
            gate_why = verdict.why.clone();
            self.note_boot_defer(st, &verdict);
        }
        // THE choke point (invariant 6): one evaluation → the parked boolean
        // AND the published reason; `set_live_shed` is called here and only here.
        let live = flow::decide_live(shed.as_ref(), gate_holds_live, &gate_why);
        if live.parked != st.live_shed {
            st.live_shed = live.parked;
            if let Some(f) = &vo.set_live_shed {
                f(live.parked);
            }
        }
        self.note_live_block(st, live.block.as_ref());
        // Shed rung 2: video recording off; telemetry recording untouched.
        let record = flow::effective_record(st.cfg.video.enabled, boot_allow, shed.as_ref());
        let preset = resolve_video_preset(&st.cfg.video.preset);
        let encoder = (vo.encoder)();
        let key = VideoShape {
            record,
            preset: preset.name.to_string(),
            seg: st.cfg.video.segment_seconds,
            stagger: st.cfg.video.spawn_stagger_secs,
            encoder: encoder.as_str().to_string(),
        }
        .key();
        let cameras_key = flow::cameras_key(&cameras);
        let current = lock(&self.slots).capture.clone();
        let plan = flow::plan_capture(
            &st.video_key,
            &st.video_cameras_key,
            &key,
            &cameras_key,
            cameras.is_empty(),
            record,
            current.is_some(),
        );
        if plan == CapturePlan::Unchanged {
            return;
        }
        st.video_key = key.clone();
        st.video_cameras_key = cameras_key.clone();
        match plan {
            CapturePlan::Unchanged => {}
            CapturePlan::StopAll { warn } => {
                let prev = lock(&self.slots).capture.take();
                if let Some(prev) = prev {
                    let _ = self.transitions.send(Transition::StopAndWait(prev));
                }
                if warn {
                    self.event(ActivityLevel::Warn, flow::NO_BRIDGED_CAMERA_WARNING);
                }
            }
            CapturePlan::InPlace => {
                if record {
                    self.ensure_disk(st);
                }
                if let Some(live) = current {
                    let _ = self.transitions.send(Transition::SetCameras(live, cameras));
                }
            }
            CapturePlan::Replace => {
                if record {
                    self.ensure_disk(st);
                }
                let prev = lock(&self.slots).capture.take();
                let _ = self.transitions.send(Transition::Replace {
                    prev,
                    key,
                    cameras_key,
                    cameras,
                    record,
                    preset,
                    encoder,
                });
            }
        }
    }

    /// `updateShedMonitor` (`manager.ts:595-621`).
    fn update_shed_monitor(&self, st: &mut State, cameras_present: bool) -> Option<ShedVerdict> {
        let armed = flow::shed_armed(
            cameras_present,
            st.cfg.power_shedding.enabled,
            self.power_sample.is_some(),
        );
        if !armed {
            if let Some(t) = st.shed_timer.take() {
                t.cancel();
            }
            st.shed_policy.reset(self.clock.as_ref());
            st.shed_level = ShedLevel::Full;
            if st.shed_status.is_some() {
                st.shed_status = None;
                self.event(ActivityLevel::Info, flow::SHED_DISARMED_EVENT);
                self.notify_status(st);
            }
            return None;
        }
        if st.shed_timer.is_none() {
            let token = self.cancel.child_token();
            st.shed_timer = Some(token.clone());
            let weak = self.self_weak();
            spawn_every(self.shed_sample_ms, token, move || {
                let i = weak.upgrade()?;
                Some(Box::pin(async move { i.shed_tick().await }))
            });
        }
        Some(self.evaluate_shed(st))
    }

    /// One shed-monitor tick (`manager.ts:622-638`): sample, push, evaluate,
    /// and re-run `applyVideo` on a rung change (the same choke point).
    async fn shed_tick(self: Arc<Self>) {
        if lock(&self.state).stop_requested {
            return;
        }
        let reading = match &self.power_sample {
            Some(f) => f().await,
            None => None,
        };
        let mut st = lock(&self.state);
        if let Some(r) = reading {
            st.shed_policy.push(&r, self.clock.as_ref());
        }
        if st.stop_requested {
            return;
        }
        let before = st.shed_level;
        let verdict = self.evaluate_shed(&mut st);
        if verdict.level != before {
            self.apply_video(&mut st);
        }
    }

    /// `evaluateShed` (`manager.ts:639-659`).
    fn evaluate_shed(&self, st: &mut State) -> ShedVerdict {
        let verdict = st
            .shed_policy
            .evaluate(&st.cfg.power_shedding, self.clock.as_ref());
        st.shed_level = verdict.level;
        if let Some(e) = &verdict.event {
            let level = match e.level {
                ShedEventLevel::Info => ActivityLevel::Info,
                ShedEventLevel::Warn => ActivityLevel::Warn,
            };
            self.event(level, &e.msg);
        }
        let status = st.shed_policy.status_string();
        if status != st.shed_status {
            st.shed_status = status;
            self.notify_status(st);
        }
        verdict
    }

    /// `noteLiveBlock` (`manager.ts:660-678`).
    fn note_live_block(&self, st: &mut State, next: Option<&flow::LiveBlockCandidate>) {
        if let Some(v) = flow::note_live_block(st.live_block.as_ref(), next, self.clock.wall_ms()) {
            st.live_block = v;
            self.notify_status(st);
        }
    }

    /// `noteBootDefer` (`manager.ts:679-741`): log + publish once per state change.
    fn note_boot_defer(&self, st: &mut State, verdict: &BootDeferVerdict) {
        let status = flow::boot_defer_status(verdict);
        if st.boot_defer_status.as_deref() == Some(status.as_str()) {
            return;
        }
        st.boot_defer_status = Some(status);
        let msg = flow::boot_defer_event(verdict, &st.cfg.video);
        self.event(ActivityLevel::Info, &msg);
        self.notify_status(st);
    }

    /// `startVideo` (`manager.ts:702-741`) — called from the transition task
    /// with `state` held (the TS continuation is atomic). `capture.start()`
    /// runs under that lock: see LOCK ORDER in the module docs. No capture
    /// callback may take `state` synchronously.
    fn start_video(
        &self,
        st: &State,
        cameras: Vec<CaptureCamera>,
        record: bool,
        preset: &'static VideoPreset,
        encoder: EncoderToken,
    ) {
        let Some(vo) = &self.video else { return };
        let on_active = {
            let w = self.self_weak();
            Arc::new(move |stream_key: &str, path: Option<PathBuf>| {
                let Some(i) = w.upgrade() else { return };
                let d = lock(&i.slots).disk.clone();
                if let Some(d) = d {
                    d.set_active_path(&format!("video:{stream_key}"), path);
                }
            })
        };
        let spawned = vo.on_capture_spawned.clone();
        let capture = self.backend.capture(CaptureSpec {
            device_id: self.device_id.clone(),
            cameras,
            record,
            preset,
            encoder,
            segment_seconds: st.cfg.video.segment_seconds,
            stagger_ms: u64::from(st.cfg.video.spawn_stagger_secs) * 1000,
            video_root: self.root.join("video"),
            shm_dir: self.shm_dir.clone(),
            time_source: self.time_source.clone(),
            on_segment_closed: self.enforcer(),
            on_active_segment: on_active,
            on_status: self.status_notifier(),
            on_event: self.on_event.clone(),
            on_spawned: Arc::new(move |k: &str| spawned(k)),
        });
        lock(&self.slots).capture = Some(capture.clone());
        capture.start();
    }

    /// `startRecorder` (`manager.ts:742-776`): a new session per (re)start.
    fn start_recorder(&self, st: &mut State) {
        let session_id = mint_session_id(self.clock.wall_ms(), random16());
        let rate_hz = st.cfg.telemetry.rate_hz;
        let dir = self.root.join("telemetry").join(&session_id);
        self.ensure_disk(st);
        let recorder = self.backend.recorder(RecorderSpec {
            root: self.root.clone(),
            dir,
            device_id: self.device_id.clone(),
            session_id: session_id.clone(),
            boot_id: self.backend.boot_id(),
            rate_hz,
            time_source: self.time_source.clone(),
            sample_groups: self.sample_groups.clone(),
            on_segment_closed: self.recorder_enforcer(),
        });
        st.session_id = Some(session_id.clone());
        st.active_rate_hz = Some(rate_hz);
        st.recorder = Some(recorder.clone());
        lock(&self.slots).recorder = Some(recorder.clone());
        recorder.start();
        self.event(
            ActivityLevel::Info,
            &flow::telemetry_started_event(&session_id, rate_hz),
        );
    }

    /// `stopRecorder` (`manager.ts:777-785`).
    /// A finalize error ABORTS it (review-wp2 N5): the recorder stays set,
    /// no `stopped` event — as the TS, whose `recorder.stop()` throws before
    /// `this.recorder = null`.
    fn stop_recorder(&self, st: &mut State, why: &str) -> Result<(), String> {
        let Some(r) = st.recorder.clone() else {
            return Ok(());
        };
        r.stop()?;
        st.recorder = None;
        lock(&self.slots).recorder = None;
        st.active_rate_hz = None;
        self.event(ActivityLevel::Info, &flow::telemetry_stopped_event(why));
        st.session_id = None;
        Ok(())
    }

    /// `ensureDisk` (`manager.ts:786-795`): build once, enforce now, then every 60 s.
    fn ensure_disk(&self, st: &mut State) {
        if lock(&self.slots).disk.is_some() {
            return;
        }
        let d = self
            .backend
            .disk(&self.root, &st.cfg.retention, self.on_event.clone());
        lock(&self.slots).disk = Some(d);
        // `void this.runEnforce()`.
        self.run_enforce_from(st.recorder.clone());
        let token = self.cancel.child_token();
        st.disk_timer = Some(token.clone());
        let enforce = self.enforcer();
        spawn_every(flow::DISK_ENFORCE_MS, token, move || {
            enforce();
            None
        });
    }

    /// `runEnforce` (`manager.ts:796-805`): `setActivePath` and the disk's
    /// synchronous scan prefix run NOW, inside the caller (for
    /// `onSegmentClosed` that is the rotation itself, review-wp2 N1); the
    /// enforce tail, `notifyStatus()` and the uploader kick follow on a task.
    fn run_enforce(&self) {
        let rec = lock(&self.slots).recorder.clone();
        self.run_enforce_from(rec);
    }

    /// [`Self::run_enforce`] with `this.recorder` supplied by a caller that
    /// holds the state lock.
    fn run_enforce_from(&self, recorder: Option<Arc<dyn RecorderApi>>) {
        self.run_enforce_active(recorder.and_then(|r| r.active_path()));
    }

    /// `runEnforce` with `this.recorder?.activePath()` already evaluated.
    fn run_enforce_active(&self, active: Option<PathBuf>) {
        let Some(disk) = lock(&self.slots).disk.clone() else {
            return;
        };
        disk.set_active_path("telemetry", active);
        let tail = disk.enforce();
        let Some(me) = self.self_weak().upgrade() else {
            return;
        };
        tokio::spawn(async move {
            tail.await;
            {
                let st = lock(&me.state);
                me.notify_status(&st);
            }
            me.kick_uploader();
        });
    }

    /// The manager-owned snapshot of `st`, published to the watch.
    fn publish(&self, st: &State) -> RecordingSnapshot {
        let snap = RecordingSnapshot {
            enabled: st.recorder.is_some(),
            session_id: st.session_id.clone(),
            boot_defer: st.boot_defer_status.clone(),
            power_shed: st.shed_status.clone(),
            live_block: st.live_block.clone(),
            telemetry_enabled: st.cfg.telemetry.enabled,
        };
        self.snapshot.send_if_modified(|cur| {
            if *cur == snap {
                false
            } else {
                *cur = snap.clone();
                true
            }
        });
        snap
    }

    /// `status()` from a snapshot + the live leaf gauges.
    fn compose(&self, snap: &RecordingSnapshot) -> RecordingStatus {
        let (disk, uploader) = {
            let s = lock(&self.slots);
            (s.disk.clone(), s.uploader.clone())
        };
        let d = disk.map(|d| d.stats()).unwrap_or_default();
        let u = uploader.map(|u| u.status()).unwrap_or_default();
        RecordingStatus {
            enabled: snap.enabled,
            session_id: snap.session_id.clone(),
            backlog_bytes: d.backlog_bytes,
            backlog_segments: d.backlog_segments,
            dropped_segments: d.dropped_segments,
            uploading: u.uploading,
            last_upload_at: u.last_upload_at,
            boot_defer: snap.boot_defer.clone(),
            power_shed: snap.power_shed.clone(),
            // `cfg.telemetry.enabled && recorder === null && !clockTrusted()`
            // (`manager.ts:442-446` @ 09da38d) — the gate is consulted (and
            // may flip to fallback) only when the first two hold.
            clock_hold: snap.telemetry_enabled
                && !snap.enabled
                && !self.clock_trusted.as_ref().is_none_or(|f| f()),
            live_block: snap.live_block.clone(),
        }
    }

    /// `notifyStatus` (`manager.ts:806-808`).
    fn notify_status(&self, st: &State) {
        let snap = self.publish(st);
        let status = self.compose(&snap);
        (self.on_status)(&status);
    }
}

/// The component-callback mailbox (`onStatus` / `runEnforce`).
async fn message_task(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Msg>) {
    loop {
        let msg = tokio::select! {
            _ = inner.cancel.cancelled() => break,
            m = rx.recv() => match m { Some(m) => m, None => break },
        };
        match msg {
            Msg::Status => {
                let st = lock(&inner.state);
                inner.notify_status(&st);
            }
        }
    }
}

/// The `videoTransition` chain (`manager.ts:556-593`): strictly in order;
/// after `stop()` the queued links still run (a `Replace` then finds itself
/// superseded, as in the TS).
async fn transition_task(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Transition>) {
    loop {
        let t = tokio::select! {
            biased;
            m = rx.recv() => match m { Some(t) => t, None => break },
            _ = inner.cancel.cancelled() => match rx.try_recv() {
                Ok(t) => t,
                Err(_) => break,
            },
        };
        match t {
            Transition::Barrier(done) => {
                let _ = done.send(());
            }
            Transition::StopAndWait(c) => c.stop_and_wait().await,
            Transition::SetCameras(c, cams) => c.set_cameras(cams).await,
            Transition::Replace {
                prev,
                key,
                cameras_key,
                cameras,
                record,
                preset,
                encoder,
            } => {
                if let Some(p) = prev {
                    p.stop_and_wait().await;
                }
                let st = lock(&inner.state);
                // Superseded by a newer change while we were stopping.
                if st.video_key != key || st.video_cameras_key != cameras_key {
                    continue;
                }
                inner.start_video(&st, cameras, record, preset, encoder);
            }
        }
    }
}
