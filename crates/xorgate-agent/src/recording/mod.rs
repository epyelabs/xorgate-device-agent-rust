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

/// The production [`manager::RecordingBackend`]: the real components as
/// they merge — WP3's uploader, WP4's capture supervisor + video orphan
/// recovery; WP1-REBASE: the WP2 recorder/disk/orphan finalize/boot id still
/// delegate to [`pending::Inert`].
pub struct Backend {
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub tape: Arc<dyn TapeSink>,
    /// The agent home (`fs_out` tape paths are relative to it).
    pub home: Option<std::path::PathBuf>,
}

impl manager::RecordingBackend for Backend {
    fn finalize_orphan_segments(&self, t: &Path) -> usize {
        manager::RecordingBackend::finalize_orphan_segments(&pending::Inert, t)
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
    fn boot_id(&self) -> String {
        manager::RecordingBackend::boot_id(&pending::Inert)
    }
    fn disk(
        &self,
        root: &Path,
        retention: &ResolvedRetention,
        on_event: manager::EventFn,
    ) -> Arc<dyn manager::DiskApi> {
        manager::RecordingBackend::disk(&pending::Inert, root, retention, on_event)
    }
    fn recorder(&self, spec: manager::RecorderSpec) -> Arc<dyn manager::RecorderApi> {
        manager::RecordingBackend::recorder(&pending::Inert, spec)
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
        o.tape = self.tape.clone();
        Arc::new(CaptureAdapter(Arc::new(
            capture::VideoCaptureSupervisor::new(o),
        )))
    }
}

/// WP4's supervisor behind [`manager::CaptureApi`].
struct CaptureAdapter(Arc<capture::VideoCaptureSupervisor>);

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
    fn stop_and_wait(&self) -> crate::ctx::BoxFut<'_, ()> {
        Box::pin(async move { self.0.stop_and_wait(STOP_AND_WAIT_GRACE).await })
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

/// WP1-REBASE: the production [`manager::RecordingBackend`] is assembled
/// from the WP2–WP4 components once they merge (the adapters live here).
/// Until then the worker runs this inert backend: no files, no children —
/// exactly the Phase 2 behaviour (recording all-off, media idle).
pub mod pending {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use xorgate_core::recording_config::ResolvedRetention;
    use xorgate_core::types::TimeSource;
    use xorgate_core::video_capture::CaptureCamera;

    use super::manager::*;
    use crate::ctx::BoxFut;

    pub struct Inert;

    impl RecorderApi for Inert {
        fn start(&self) {}
        fn stop(&self) -> Result<(), String> {
            Ok(())
        }
        fn active_path(&self) -> Option<PathBuf> {
            None
        }
    }
    impl DiskApi for Inert {
        fn set_retention(&self, _r: &ResolvedRetention) {}
        fn set_active_path(&self, _owner: &str, _p: Option<PathBuf>) {}
        fn stats(&self) -> BacklogStats {
            BacklogStats::default()
        }
        fn enforce(&self) -> BoxFut<'static, ()> {
            Box::pin(async {})
        }
    }
    impl UploaderApi for Inert {
        fn kick(&self) {}
        fn pause(&self) {}
        fn stop(&self) {}
        fn status(&self) -> UploaderStatus {
            UploaderStatus::default()
        }
    }
    impl CaptureApi for Inert {
        fn start(&self) {}
        fn set_cameras(&self, _c: Vec<CaptureCamera>) -> BoxFut<'_, ()> {
            Box::pin(async {})
        }
        fn stop(&self) {}
        fn stop_and_wait(&self) -> BoxFut<'_, ()> {
            Box::pin(async {})
        }
        fn active_session_ids(&self) -> HashSet<String> {
            HashSet::new()
        }
        fn any_recording(&self) -> bool {
            false
        }
        fn status(&self) -> Vec<CaptureStatus> {
            Vec::new()
        }
    }
    impl TimeSourceApi for Inert {
        fn current(&self) -> TimeSource {
            TimeSource::Unsynced
        }
        fn refresh(&self) -> BoxFut<'_, ()> {
            Box::pin(async {})
        }
    }
    impl RecordingBackend for Inert {
        fn finalize_orphan_segments(&self, _t: &Path) -> usize {
            0
        }
        fn recover_orphan_video_sessions(&self, _v: &Path) -> usize {
            0
        }
        fn boot_id(&self) -> String {
            String::new()
        }
        fn disk(&self, _r: &Path, _ret: &ResolvedRetention, _e: EventFn) -> Arc<dyn DiskApi> {
            Arc::new(Inert)
        }
        fn recorder(&self, _s: RecorderSpec) -> Arc<dyn RecorderApi> {
            Arc::new(Inert)
        }
        fn uploader(&self, _s: UploaderSpec) -> Arc<dyn UploaderApi> {
            Arc::new(Inert)
        }
        fn capture(&self, _s: CaptureSpec) -> Arc<dyn CaptureApi> {
            Arc::new(Inert)
        }
    }
}
