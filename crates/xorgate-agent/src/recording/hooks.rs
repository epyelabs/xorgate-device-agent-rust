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
//! | `reconcile_hardware_boot` (fallible — fatal at boot) | `await reconcileHardwareNow()` (`:427`) |
//! | `recording_start` | `await recording.start()` (`:529`) |
//! | `note_recording_status` | `noteRecordingStatus(…)` (`:577-579`, the LED cache) |
//! | `final_recording_status` | `store.setRecording(recording.status())` (`:1307`) |
//! | `report_hints` | ConfigSync `hints()` (`:719-726`) |
//! | `recording_stop` (fallible — a failed finalize ends the link the crash way, X1) | `recording.stop()` (`:1306`) |
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
use xorgate_core::cameras::EnumeratedCamera;
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::stream_guard::StreamHardwareStatus;
use xorgate_core::telemetry::{ActivityLevel, RunMode};

use super::manager::RecordingManager;
use crate::ctx::{AgentCtx, BoxFut, MediaControl, PhaseHooks};

/// `reconcileHardwareNow()` (`worker.ts:133-150`): re-render pipelines
/// against the hardware block; the stream keys whose `.gst` changed. `Err`
/// = the pass threw (a failed mkdir/write, `hardware-reconcile.ts` throws):
/// fatal at boot, logged and skipped on a cloud change (review-wp5 M1).
pub trait HardwareReconciler: Send + Sync {
    fn reconcile(&self) -> BoxFut<'_, Result<Vec<String>, String>>;
    /// Rust-only (support-other-camera-models Phase 0): the same pass, run
    /// again for a port that is held for lack of a camera while a driver is
    /// bound on it. It differs from [`reconcile`](Self::reconcile) in its
    /// journal only: a line the previous pass already logged is not logged
    /// again (a re-check every five minutes must not repeat "no camera
    /// enumerated …" for ever). Default: the plain pass.
    fn recheck(&self) -> BoxFut<'_, Result<Vec<String>, String>> {
        self.reconcile()
    }
}

/// `buildUploadOptions()` (`worker.ts:389-413`) — re-run on every re-arm, so
/// the manifest is re-read and the `segment upload disabled (…)` line may be
/// logged again (CF39, review-wp3 #4).
pub type UploadBuilder = Arc<dyn Fn() -> Option<UploadOptions> + Send + Sync>;

/// What the last hardware reconcile passes found (a copy of
/// [`StreamHardware`]'s content).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamHardwareSnapshot {
    /// `streamGuardArmed`: some pass could enumerate cameras.
    pub armed: bool,
    /// `streamHardware`: the verdicts of the last pass that could enumerate.
    pub statuses: IndexMap<String, StreamHardwareStatus>,
    /// Camera report (Rust-only): how many passes have completed.
    pub passes: u64,
    /// Camera report (Rust-only): what the LAST completed pass enumerated;
    /// `None` = it could not enumerate (or no pass has completed).
    pub cameras: Option<Vec<EnumeratedCamera>>,
}

/// `streamHardware` + `streamGuardArmed` (`worker.ts:129-130`): the reconcile
/// verdicts the media spawn guard reads. Armed only after a pass that could
/// enumerate cameras.
#[derive(Default)]
pub struct StreamHardware {
    inner: Mutex<StreamHardwareSnapshot>,
    /// The `[hardware]` lines of the last pass (what a re-check must not
    /// repeat).
    last_lines: Mutex<Vec<String>>,
}

impl StreamHardware {
    /// `streamGuardArmed`.
    pub fn armed(&self) -> bool {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).armed
    }
    /// `streamHardware.get(key)`.
    pub fn status(&self, key: &str) -> Option<StreamHardwareStatus> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.statuses.get(key).copied()
    }
    /// Camera report: everything the passes found, in one consistent copy.
    pub fn snapshot(&self) -> StreamHardwareSnapshot {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    /// B5: may the capture daemon launch this stream's camera?
    /// (`camera_truth::capture_allowed` over the last verdicts.)
    pub fn capture_allowed(&self, key: &str) -> bool {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        xorgate_core::camera_truth::capture_allowed(g.armed, g.statuses.get(key).copied())
    }
}

/// Reads the text of the boot config (`None` = no file, or unreadable).
pub type BootConfigTextFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// The real [`BootConfigTextFn`]: `/boot/firmware/config.txt`, then
/// `/boot/config.txt`. Read-only.
pub fn real_boot_config_text() -> BootConfigTextFn {
    Arc::new(xorgate_hal::boot_config_io::read_boot_config_text)
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
    /// Rust-only: the boot config text for the mismatch warning's
    /// "declared" sensor ([`real_boot_config_text`] in the worker). `None` =
    /// the model alone, as the TS has it.
    pub boot_config: Option<BootConfigTextFn>,
}

impl MediaReconciler {
    /// One pass. `quiet` = a re-check: lines the previous pass logged are
    /// dropped, and a stream that got its camera says so once.
    async fn pass(&self, quiet: bool) -> Result<Vec<String>, String> {
        let cfg = &self.ctx.env.config;
        let block = self.ctx.hardware.borrow().clone();
        let Some(block) = block else {
            return Ok(Vec::new());
        };
        if cfg.mode != RunMode::Real || cfg.offline {
            return Ok(Vec::new());
        }
        let (state, clock) = (self.ctx.state.clone(), self.ctx.clock.clone());
        let emit = move |level: crate::media::ReconcileLevel, msg: &str| {
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
        };
        let before: Vec<String> = if quiet {
            self.stream_hw
                .last_lines
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        } else {
            Vec::new()
        };
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let log: crate::media::ReconcileLog = {
            let (lines, emit) = (lines.clone(), emit.clone());
            Arc::new(move |level, msg: &str| {
                lines
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(msg.to_string());
                if !before.iter().any(|l| l == msg) {
                    emit(level, msg);
                }
            })
        };
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
                boot_config: self.boot_config.as_ref().and_then(|f| f()),
            },
        )
        // A throw (first failed mkdir/write, Node's message) leaves the
        // verdicts untouched (TS parity, review-wp5 M1).
        .await?;
        let healed: Vec<String> = {
            let mut g = self
                .stream_hw
                .inner
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            // Streams that were held for lack of a camera and now have one.
            let healed = if res.cameras.is_some() {
                res.statuses
                    .iter()
                    .filter(|(k, st)| {
                        **st != StreamHardwareStatus::NoCamera
                            && g.armed
                            && g.statuses.get(*k) == Some(&StreamHardwareStatus::NoCamera)
                    })
                    .map(|(k, _)| k.clone())
                    .collect()
            } else {
                Vec::new()
            };
            if res.cameras.is_some() {
                // `streamHardware.clear(); set(...)…; streamGuardArmed = true`.
                g.armed = true;
                g.statuses = res.statuses.clone();
            }
            // Camera report: what THIS pass saw (the guard's verdicts
            // above stay those of the last pass that could enumerate).
            g.passes += 1;
            g.cameras = res.cameras.clone();
            healed
        };
        *self
            .stream_hw
            .last_lines
            .lock()
            .unwrap_or_else(|p| p.into_inner()) =
            lines.lock().unwrap_or_else(|p| p.into_inner()).clone();
        // Rust-only: the hold of B5 ended. Said once, on a re-check only
        // (a boot or hardware-change pass keeps the TS's lines exactly).
        if quiet {
            for key in &healed {
                emit(
                    crate::media::ReconcileLevel::Info,
                    &format!("{key}: a camera is enumerated now — starting the stream"),
                );
            }
        }
        Ok(res.changed_streams)
    }
}

impl HardwareReconciler for MediaReconciler {
    fn reconcile(&self) -> BoxFut<'_, Result<Vec<String>, String>> {
        Box::pin(self.pass(false))
    }
    fn recheck(&self) -> BoxFut<'_, Result<Vec<String>, String>> {
        Box::pin(self.pass(true))
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
    fn set_source_absent(&self, key: &str, absent: bool, seq: u64) {
        crate::media::MediaSupervisor::set_source_absent(self, key, absent, seq)
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

    /// `void reconcileHardwareNow().then(changed => bounce; reapply)
    /// .catch(err => console.error("[agent] hardware reconcile failed", msg))`
    /// (`worker.ts:843-850`): a failed pass bounces nothing and does not
    /// reapply. `recheck` = the Rust-only quiet re-listing of a held port.
    fn reconcile_then_reapply(&self, recheck: bool) {
        let (Some(hw), Some(m)) = (self.hardware.clone(), self.manager().cloned()) else {
            return;
        };
        let media = self.media.clone();
        tokio::spawn(async move {
            let pass = if recheck {
                hw.recheck().await
            } else {
                hw.reconcile().await
            };
            let changed = match pass {
                Ok(c) => c,
                Err(e) => {
                    // `console.error(a, b)` joins its arguments with one space.
                    tracing::error!("[agent] hardware reconcile failed {e}");
                    return;
                }
            };
            if let Some(media) = &media {
                for key in &changed {
                    media.restart_channel(key);
                }
            }
            if let Err(e) = m.reapply() {
                tracing::error!("[agent] hardware reconcile failed {e}");
            }
        });
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

    /// `recording.reapply()` inside the config message's try
    /// (`worker.ts:881`): a throwing finalize propagates into the
    /// `cloud config apply failed` handler (review-wp1 M1).
    fn reapply_recording(&self) -> Result<(), String> {
        match self.manager() {
            Some(m) => m.reapply(),
            None => Ok(()),
        }
    }

    fn rearm_upload(&self) {
        if let Some(m) = self.manager() {
            m.set_upload((self.upload_builder)());
        }
    }

    /// `void reconcileHardwareNow().then(changed => bounce; reapply)
    /// .catch(err => console.error("[agent] hardware reconcile failed", msg))`
    /// (`worker.ts:843-850`): a failed pass bounces nothing and does not reapply.
    /// A throwing `recording.reapply()` inside the `.then` rejects the same
    /// chain, so it lands in the same `.catch` line (review-wp1 M1).
    fn hardware_changed(&self) {
        self.reconcile_then_reapply(false);
    }

    /// Rust-only: the re-listing of a held port (`HardwareReconciler::recheck`),
    /// through the SAME path as a hardware-block change: reconcile, bounce
    /// the changed masters, `reapply()` (which starts the capture of a
    /// stream that now has a camera, as at boot).
    fn hardware_recheck(&self) {
        self.reconcile_then_reapply(true);
    }

    /// The boot `await reconcileHardwareNow()` (`worker.ts:427`): a throw is
    /// a top-level-await rejection in the TS (before the `unhandledRejection`
    /// net of `:656`) — the process dies non-zero and systemd restarts it.
    /// The worker propagates this `Err` the same way (review-wp5 M1).
    fn reconcile_hardware_boot(&self) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async move {
            match &self.hardware {
                Some(hw) => hw.reconcile().await.map(|_| ()),
                None => Ok(()),
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
            // `clockTrust?.status().state` — Phase 5b (`OpsHooks`).
            clock_trust: None,
        }
    }

    fn recording_stop(&self) -> Result<(), String> {
        match self.manager() {
            Some(m) => m.stop_checked(),
            None => Ok(()),
        }
    }
}
