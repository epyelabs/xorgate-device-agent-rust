//! On-device recording (Phase 3): telemetry recorder, segments, disk
//! manager, uploader + segment sources, video capture, and the manager that
//! owns them. Module list: append-only, alphabetical (shared file).
//!
//! The recording spine (WP1 — [`manager`], [`hooks`], [`policy_sources`])
//! composes the telemetry recorder, disk manager and time source (WP2), the
//! uploader (WP3) and the video capture supervisor (WP4).

pub mod capture;
pub mod disk;
pub mod hooks;
pub mod manager;
pub mod policy_sources;
pub mod sources;
pub mod telemetry;
pub mod time_source;
pub mod uploader;

pub use hooks::RecordingHooks;
pub use manager::{RecordingManager, RecordingManagerOpts, VideoOpts};

use std::path::Path;
use std::sync::Arc;

use xorgate_core::clock::Clock;
use xorgate_core::recording_config::ResolvedRetention;
use xorgate_hal::io::TapeSink;

/// The production [`manager::RecordingBackend`]: WP2's recorder, disk
/// manager, orphan finalize and boot id; WP3's uploader; WP4's capture
/// supervisor + video orphan recovery.
pub struct Backend {
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub tape: Arc<dyn TapeSink>,
    /// The agent home (`fs_out` tape paths are relative to it).
    pub home: Option<std::path::PathBuf>,
    /// The ONE tracker (`createTimeSourceTracker()`, `manager.ts:205`) the
    /// manager's probe, every recorder header and the capture share.
    pub time_source: time_source::TimeSourceHandle,
}

impl manager::RecordingBackend for Backend {
    /// `finalizeOrphanSegments(telemetryRoot)` (`segments.ts:224-270`, WP2).
    fn finalize_orphan_segments(&self, t: &Path) -> usize {
        telemetry::finalize_orphan_segments_taped(t, self.tape.as_ref())
    }
    /// `recoverOrphanVideoSessions(videoRoot)` (`video-segments.ts:341-375`,
    /// WP4): runs before any capture spawns (`manager.ts:302-304`), so no
    /// session is active; the result is reported to the tape.
    fn recover_orphan_video_sessions(&self, v: &Path) -> usize {
        let none = std::collections::HashSet::new();
        let res = capture::recover_orphan_video_sessions(
            v,
            &capture::RecoverOpts {
                active_session_ids: &none,
                clock: self.clock.as_ref(),
            },
        );
        capture::tape_sweep_outcome(
            self.tape.as_ref(),
            self.home.as_deref(),
            &res.closed,
            &res.reaped,
        );
        res.recovered
    }
    /// D41: the start-up `/dev/shm` sweep of dead owners' shmsink areas.
    fn sweep_dead_shm_segments(&self) -> Vec<(String, u32)> {
        xorgate_hal::shm_reaper::sweep_dead_owner_segments(
            Path::new(xorgate_core::capture_ladder::SHM_SEGMENT_DIR),
            &xorgate_hal::shm_reaper::pid_alive,
        )
    }
    /// `readBootId()` (`session.ts:49-65`, cached per process).
    fn boot_id(&self) -> String {
        xorgate_hal::boot_id::read_boot_id()
    }
    /// `new DiskManager({ root, retention })` (`manager.ts:788`). `disk.ts`
    /// emits no state event, so `on_event` is unused (WP2 interface note).
    fn disk(
        &self,
        root: &Path,
        retention: &ResolvedRetention,
        _on_event: manager::EventFn,
    ) -> Arc<dyn manager::DiskApi> {
        Arc::new(DiskAdapter(Arc::new(
            disk::DiskManager::new(root.to_path_buf(), retention.clone())
                .with_tape(self.tape.clone()),
        )))
    }
    /// `new TelemetryRecorder({...})` (`manager.ts:748-768`).
    fn recorder(&self, spec: manager::RecorderSpec) -> Arc<dyn manager::RecorderApi> {
        let closed = spec.on_segment_closed;
        Arc::new(telemetry::TelemetryRecorder::new(
            telemetry::TelemetryRecorderOpts {
                root: spec.root,
                device_id: spec.device_id,
                session_id: spec.session_id,
                boot_id: spec.boot_id,
                rate_hz: spec.rate_hz,
                time_source: self.time_source.clone(),
                sample_groups: spec.sample_groups,
                // `onSegmentClosed: () => { void this.runEnforce(); }`.
                on_segment_closed: Some(Arc::new(move |_gz: &Path| closed())),
                on_active_path: None,
                clock: self.clock.clone(),
                tape: self.tape.clone(),
                segment_max_bytes: None,
                segment_max_age_ms: None,
                native_cadence_hz: None,
                mono_ns: None,
                executor: None,
            },
        ))
    }
    /// `buildUploader` (`manager.ts:246-272`) → WP3's [`uploader::build_uploader`].
    fn uploader(&self, spec: manager::UploaderSpec) -> Arc<dyn manager::UploaderApi> {
        let on_status = spec.on_status;
        let on_uploaded = spec.on_uploaded;
        Arc::new(uploader::build_uploader(uploader::BuildUploader {
            device_id: spec.device_id,
            root: spec.root,
            upload: spec.upload,
            extra_sources: Vec::new(),
            mode: spec.mode,
            active_session_ids: spec.active_session_ids,
            // `onStatus: () => this.notifyStatus()`.
            on_status: Some(Arc::new(move |_| on_status())),
            on_event: Some(spec.on_event),
            // `onUploaded: () => void this.runEnforce()`.
            on_uploaded: Some(Arc::new(move |_, _| on_uploaded())),
            backoff_min_ms: None,
            backoff_max_ms: None,
            clock: self.clock.clone(),
            tape: self.tape.clone(),
        }))
    }
    /// `new VideoCaptureSupervisor({...})` (`manager.ts:702-741`, WP4).
    fn capture(&self, spec: manager::CaptureSpec) -> Arc<dyn manager::CaptureApi> {
        // `timeSource: () => this.timeSource.current()` — the SYNC tracker read
        // that kicks a background re-probe when stale (review-wp4 N1).
        let ts = spec.time_source;
        let mut o = capture::VideoCaptureOpts::new(
            spec.device_id,
            spec.cameras,
            spec.record,
            spec.preset.clone(),
            u64::from(spec.segment_seconds),
            spec.video_root,
            spec.shm_dir,
            self.boot_id(),
            Arc::new(move || ts.current()),
            self.clock.clone(),
        );
        o.encoder = Some(spec.encoder);
        // Phase 7a: `XORGATE_CAPTURE_BACKEND` (unset = gst-launch, today's).
        o.backend = capture::backend_from_env();
        // Phase 7a WP2: `XORGATE_SIDECAR_TIMING` (unset = legacy, today's);
        // `exact` with gst-launch warns once and stays legacy.
        o.sidecar_timing = capture::sidecar_timing_from_env(o.backend.emits_timing());
        o.stagger_ms = spec.stagger_ms;
        o.home = self.home.clone();
        let closed = spec.on_segment_closed;
        o.on_segment_closed = Some(Arc::new(move |_mp4: &Path| closed()));
        let active = spec.on_active_segment;
        o.on_active_path = Some(Arc::new(move |key: &str, p: Option<&Path>| {
            active(key, p.map(Path::to_path_buf))
        }));
        o.on_status = Some(spec.on_status);
        o.on_event = Some(spec.on_event);
        o.on_spawned = Some(spec.on_spawned);
        o.replay = spec.replay;
        o.tape = self.tape.clone();
        Arc::new(CaptureAdapter(Arc::new(
            capture::VideoCaptureSupervisor::new(o),
        )))
    }
}

/// WP4's supervisor behind [`manager::CaptureApi`].
struct CaptureAdapter(Arc<capture::VideoCaptureSupervisor>);

/// D41 agent-exit cleanup: the TOTAL wait for every still-running capture
/// child after their SIGKILLs (well inside systemd's TimeoutStopSec).
pub const CAPTURE_EXIT_REAP_BOUND: std::time::Duration = std::time::Duration::from_millis(500);

/// `stopAndWait(graceMs = 10_000)` (`video-capture.ts:516`).
const STOP_AND_WAIT_GRACE: std::time::Duration = std::time::Duration::from_millis(10_000);

impl manager::CaptureApi for CaptureAdapter {
    fn start(&self) {
        self.0.start()
    }
    fn set_cameras(
        &self,
        cameras: Vec<xorgate_core::video_capture::CaptureCamera>,
    ) -> crate::ctx::BoxFut<'_, ()> {
        Box::pin(async move { self.0.set_cameras(&cameras).await })
    }
    fn stop(&self) {
        self.0.stop()
    }
    fn cycle_all(&self) {
        self.0.cycle_all()
    }
    fn stop_and_wait(&self) -> crate::ctx::BoxFut<'_, ()> {
        Box::pin(async move { self.0.stop_and_wait(STOP_AND_WAIT_GRACE).await })
    }
    fn exit_kill(&self) -> Vec<capture::ExitKilled> {
        self.0.exit_kill()
    }
    fn exit_finish(&self, killed: &[capture::ExitKilled]) -> Vec<(String, Vec<String>)> {
        self.0.exit_finish(killed)
    }
    fn active_session_ids(&self) -> std::collections::HashSet<String> {
        self.0.active_session_ids()
    }
    fn any_recording(&self) -> bool {
        self.0.any_recording()
    }
    fn status(&self) -> Vec<manager::CaptureStatus> {
        self.0
            .status()
            .into_iter()
            .map(|s| manager::CaptureStatus {
                stream_key: s.stream_key,
                state: serde_json::to_value(s.state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                session_id: s.session_id,
                recording: s.recording,
            })
            .collect()
    }
}

/// WP2's `DiskManager` behind [`manager::DiskApi`].
struct DiskAdapter(Arc<disk::DiskManager>);

impl manager::DiskApi for DiskAdapter {
    fn set_retention(&self, r: &ResolvedRetention) {
        self.0.set_retention(r.clone());
    }
    fn set_active_path(&self, owner: &str, p: Option<std::path::PathBuf>) {
        self.0.set_active_path(owner, p.as_deref());
    }
    fn stats(&self) -> manager::BacklogStats {
        let s = self.0.stats();
        manager::BacklogStats {
            backlog_bytes: s.backlog_bytes,
            backlog_segments: s.backlog_segments,
            dropped_segments: s.dropped_segments,
        }
    }
    /// The sync prefix (`disk.ts:114-122`) now, the tail as the future.
    fn enforce(&self) -> crate::ctx::BoxFut<'static, ()> {
        let pass = self.0.enforce_scan();
        let dm = self.0.clone();
        Box::pin(async move {
            if let Some(p) = pass {
                dm.enforce_finish(p).await;
            }
        })
    }
}

impl manager::RecorderApi for telemetry::TelemetryRecorder {
    fn start(&self) {
        telemetry::TelemetryRecorder::start(self)
    }
    fn stop(&self) -> Result<(), String> {
        telemetry::TelemetryRecorder::stop(self).map_err(|e| e.to_string())
    }
    fn active_path(&self) -> Option<std::path::PathBuf> {
        telemetry::TelemetryRecorder::active_path(self)
    }
}

impl manager::TimeSourceApi for time_source::TimeSourceHandle {
    fn current(&self) -> xorgate_core::types::TimeSource {
        time_source::TimeSourceHandle::current(self)
    }
    fn refresh(&self) -> crate::ctx::BoxFut<'_, ()> {
        Box::pin(async move {
            time_source::TimeSourceHandle::refresh(self).await;
        })
    }
}

impl manager::UploaderApi for uploader::Uploader {
    fn kick(&self) {
        uploader::Uploader::kick(self)
    }
    fn pause(&self) {
        uploader::Uploader::pause(self)
    }
    fn stop(&self) {
        uploader::Uploader::stop(self)
    }
    fn status(&self) -> manager::UploaderStatus {
        uploader::Uploader::status(self)
    }
}
