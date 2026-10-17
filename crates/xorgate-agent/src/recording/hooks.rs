//! `RecordingHooks` — the Phase 3 [`PhaseHooks`] (every recording/media call
//! site of `src/worker.ts` @ 89b5c64). The LED and camera-mount hooks stay
//! the Phase 2 no-ops (Phase 4).
//!
//! | hook | TS |
//! |---|---|
//! | `notify_connectivity` | `recording.notifyConnectivity(…)` (connect `:1157`, close `:1283`) |
//! | `recording_status` | `recording.status()` in the tick (`:1209`) |
//! | `overlay_media` | the tick's media overlay (`:1200-1221`) |
//! | `reapply_recording` | `recording.reapply()` on every config message (`:878`) |
//! | `rearm_upload` | `recording.setUpload(buildUploadOptions())` (`:801`) |
//! | `hardware_changed` | `reconcileHardwareNow()` → bounce changed → `reapply()` (`:839-846`) |
//! | `reconcile_hardware_boot` | `await reconcileHardwareNow()` (`:427`) |
//! | `recording_start` | `await recording.start()` (`:529`) |
//! | `note_recording_status` | `noteRecordingStatus(…)` (`:577-579`, the LED cache) |
//! | `final_recording_status` | `store.setRecording(recording.status())` (`:1307`) |
//! | `report_hints` | ConfigSync `hints()` (`:719-726`) |
//! | `recording_stop` | `recording.stop()` (`:1306`) |
//!
//! P9: nothing here takes the manager's state lock — `report_hints` (called
//! under ConfigSync's mutex) reads only the manager's published `watch`
//! snapshot; `recording_status` reads the snapshot + the leaf gauges.

use std::sync::{Arc, Mutex, OnceLock};

use xorgate_cloud::s3::UploadOptions;
use xorgate_core::clock::Clock;
use xorgate_core::config_report::ReportHints;
use xorgate_core::telemetry::{RecordingStatus, TelemetryMedia};

use indexmap::IndexMap;
use tokio::sync::watch;
use tracing::{info, warn};
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::stream_guard::StreamHardwareStatus;
use xorgate_core::telemetry::{ActivityLevel, RunMode};

use super::manager::RecordingManager;
use crate::ctx::{AgentCtx, BoxFut, MediaControl, PhaseHooks};

/// `reconcileHardwareNow()` (`worker.ts:133-150`): re-render pipelines
/// against the hardware block; the stream keys whose `.gst` changed.
pub trait HardwareReconciler: Send + Sync {
    fn reconcile(&self) -> BoxFut<'_, Vec<String>>;
}

/// `buildUploadOptions()` (`worker.ts:389-413`) — re-run on every re-arm, so
/// the manifest is re-read and the `segment upload disabled (…)` line may be
/// logged again (CF39, review-wp3 #4).
pub type UploadBuilder = Arc<dyn Fn() -> Option<UploadOptions> + Send + Sync>;

/// `streamHardware` + `streamGuardArmed` (`worker.ts:129-130`): the reconcile
/// verdicts the media spawn guard reads. Armed only after a pass that could
/// enumerate cameras.
#[derive(Default)]
pub struct StreamHardware {
    inner: Mutex<(bool, IndexMap<String, StreamHardwareStatus>)>,
}

impl StreamHardware {
    /// `streamGuardArmed`.
    pub fn armed(&self) -> bool {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).0
    }
    /// `streamHardware.get(key)`.
    pub fn status(&self, key: &str) -> Option<StreamHardwareStatus> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.1.get(key).copied()
    }
}

/// `config.media.streamGuard` (`worker.ts:205-218`, replay branch out — P14).
/// Runs under the supervisor's lock: it reads only the hardware watch, the
/// verdict map and the pipeline dir — never the supervisor.
pub fn stream_guard_fn(
    block: Arc<watch::Sender<Option<DeviceHardwareBlock>>>,
    hw: Arc<StreamHardware>,
    media: &crate::media::MediaConfig,
) -> crate::media::GuardFn {
    let dir = std::path::PathBuf::from(&media.pipeline_dir);
    let explicit = media.pipeline_path.clone().map(std::path::PathBuf::from);
    Arc::new(move |key: &str| {
        let has_block = block.borrow().is_some();
        let (armed, status) = (hw.armed(), hw.status(key));
        crate::media::stream_guard_verdict(has_block, armed, status, || {
            crate::media::resolve_channel_pipeline(&dir, key, explicit.as_deref()).is_some()
        })
    })
}

/// `reconcileHardwareNow()` (`worker.ts:132-150`): real, online, with a
/// hardware block only; logs `[hardware] …` + a `hardware: …` state event
/// (flushed); arms the guard when enumeration worked; returns the streams
/// whose `.gst` changed.
pub struct MediaReconciler {
    pub ctx: AgentCtx,
    pub stream_hw: Arc<StreamHardware>,
    /// `config.media.pipelineDir`.
    pub pipeline_dir: std::path::PathBuf,
    /// `rpicam-hello --list-cameras` — [`crate::media::real_list_exec`] (the
    /// `exec` tape record) in the worker; a fake in tests.
    pub list_exec: crate::media::ListExec,
}

impl HardwareReconciler for MediaReconciler {
    fn reconcile(&self) -> BoxFut<'_, Vec<String>> {
        Box::pin(async move {
            let cfg = &self.ctx.env.config;
            let block = self.ctx.hardware.borrow().clone();
            let Some(block) = block else {
                return Vec::new();
            };
            if cfg.mode != RunMode::Real || cfg.offline {
                return Vec::new();
            }
            let (state, clock) = (self.ctx.state.clone(), self.ctx.clock.clone());
            let log: crate::media::ReconcileLog = Arc::new(move |level, msg: &str| {
                let t = clock.wall_ms();
                let level = match level {
                    crate::media::ReconcileLevel::Warn => {
                        warn!("[hardware] {msg}");
                        ActivityLevel::Warn
                    }
                    crate::media::ReconcileLevel::Info => {
                        info!("[hardware] {msg}");
                        ActivityLevel::Info
                    }
                };
                // `store.event(level, …); store.flush(t, true)` = `note`.
                state.note(level, format!("hardware: {msg}"), t);
            });
            let shm = self.ctx.paths.home.join("shm");
            let res = crate::media::reconcile_hardware(
                &block,
                crate::media::ReconcileOpts {
                    pipeline_dir: self.pipeline_dir.clone(),
                    // `shmSocketPathFor` (`recording/config.ts:153-155`).
                    shm_socket_path_for: Arc::new(move |key: &str| {
                        shm.join(format!("{key}.sock")).display().to_string()
                    }),
                    list_exec: Some(self.list_exec.clone()),
                    log: Some(log),
                },
            )
            .await;
            if res.cameras.is_some() {
                let mut g = self
                    .stream_hw
                    .inner
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                // `streamHardware.clear(); set(...)…; streamGuardArmed = true`.
                *g = (true, res.statuses.clone());
            }
            res.changed_streams
        })
    }
}

/// The media collector's surface (`collectors/index.ts:146-176`) over WP5's supervisor.
impl MediaControl for crate::media::MediaSupervisor {
    fn read(&self) -> TelemetryMedia {
        crate::media::MediaSupervisor::read(self)
    }
    fn stop(&self) {
        crate::media::MediaSupervisor::stop(self)
    }
    fn restart_channel(&self, key: &str) {
        crate::media::MediaSupervisor::restart_channel(self, key)
    }
    fn set_shed(&self, shed: bool) {
        crate::media::MediaSupervisor::set_shed(self, shed)
    }
    fn set_disabled_streams(&self, keys: &[String]) {
        crate::media::MediaSupervisor::set_disabled_streams(self, keys)
    }
}

/// `lastRecordingStatus` staleness for the LED (`worker.ts:581-587`).
const LED_STATUS_STALE_MS: i64 = 2000;

/// The Phase 3 hooks.
pub struct RecordingHooks {
    manager: OnceLock<RecordingManager>,
    media: Option<Arc<dyn MediaControl>>,
    hardware: Option<Arc<dyn HardwareReconciler>>,
    upload_builder: UploadBuilder,
    clock: Arc<dyn Clock + Send + Sync>,
    /// `lastRecordingStatus` (`worker.ts:574`).
    led_status: Mutex<Option<(i64, RecordingStatus)>>,
}

impl RecordingHooks {
    /// The hooks exist before the manager (the worker installs them in the
    /// ctx before the hardware reconcile that precedes `new RecordingManager`).
    pub fn new(
        media: Option<Arc<dyn MediaControl>>,
        hardware: Option<Arc<dyn HardwareReconciler>>,
        upload_builder: UploadBuilder,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Self {
        Self {
            manager: OnceLock::new(),
            media,
            hardware,
            upload_builder,
            clock,
            led_status: Mutex::new(None),
        }
    }

    /// Install the manager (`const recording = new RecordingManager(…)`, `:439`).
    pub fn install(&self, manager: RecordingManager) {
        let _ = self.manager.set(manager);
    }

    pub fn manager(&self) -> Option<&RecordingManager> {
        self.manager.get()
    }

    /// `recordingStatusForLed()` (`worker.ts:581-587`): the tick's cached
    /// value unless ≥ 2 s old (Phase 4's LED is the consumer).
    pub fn recording_status_for_led(&self) -> RecordingStatus {
        let now = self.clock.wall_ms();
        let mut g = self.led_status.lock().unwrap_or_else(|p| p.into_inner());
        match &*g {
            Some((at, v)) if now - *at < LED_STATUS_STALE_MS => v.clone(),
            _ => {
                let v = self.recording_status();
                *g = Some((now, v.clone()));
                v
            }
        }
    }
}

/// The tick's media overlay (`worker.ts:1200-1221`): `media.recording` =
/// any capture recording; streams under capture supervision gain their
/// disk-recording flag; untouched streams stay byte-identical.
pub fn overlay(
    mut media: TelemetryMedia,
    video: &super::manager::VideoRecordingStatus,
) -> TelemetryMedia {
    media.recording.value = video.recording;
    for s in &mut media.streams {
        if video.streams.iter().any(|v| v.stream_key == s.stream_key) {
            s.recording = Some(
                video
                    .streams
                    .iter()
                    .any(|v| v.recording && v.stream_key == s.stream_key),
            );
        }
    }
    media
}

impl PhaseHooks for RecordingHooks {
    fn notify_connectivity(&self, online: bool) {
        if let Some(m) = self.manager() {
            m.notify_connectivity(online);
        }
    }

    fn recording_status(&self) -> RecordingStatus {
        match self.manager() {
            Some(m) => m.status(),
            None => RecordingStatus::all_off(),
        }
    }

    fn overlay_media(&self, media: TelemetryMedia) -> TelemetryMedia {
        let video = self.manager().map(|m| m.video_status()).unwrap_or_default();
        overlay(media, &video)
    }

    fn reapply_recording(&self) {
        if let Some(m) = self.manager() {
            m.reapply();
        }
    }

    fn rearm_upload(&self) {
        if let Some(m) = self.manager() {
            m.set_upload((self.upload_builder)());
        }
    }

    /// `void reconcileHardwareNow().then(changed => bounce; reapply)`.
    fn hardware_changed(&self) {
        let (Some(hw), Some(m)) = (self.hardware.clone(), self.manager().cloned()) else {
            return;
        };
        let media = self.media.clone();
        tokio::spawn(async move {
            let changed = hw.reconcile().await;
            if let Some(media) = &media {
                for key in &changed {
                    media.restart_channel(key);
                }
            }
            m.reapply();
        });
    }

    fn reconcile_hardware_boot(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if let Some(hw) = &self.hardware {
                let _ = hw.reconcile().await;
            }
        })
    }

    fn recording_start(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if let Some(m) = self.manager() {
                m.start().await;
            }
        })
    }

    fn note_recording_status(&self, status: &RecordingStatus) {
        let now = self.clock.wall_ms();
        *self.led_status.lock().unwrap_or_else(|p| p.into_inner()) = Some((now, status.clone()));
    }

    fn final_recording_status(&self) -> Option<RecordingStatus> {
        self.manager().map(|m| m.status())
    }

    /// `hints` (`worker.ts:719-726`) from the published snapshot only (P9).
    fn report_hints(&self) -> ReportHints {
        let Some(m) = self.manager() else {
            return ReportHints::default();
        };
        let snap = m.status_watch().borrow().clone();
        ReportHints {
            boot_defer: snap.boot_defer,
            power_shed: snap.power_shed,
            // `clockStepper?.status().armed` — Phase 4.
            clock_stepper_armed: None,
        }
    }

    fn recording_stop(&self) {
        if let Some(m) = self.manager() {
            m.stop();
        }
    }
}
