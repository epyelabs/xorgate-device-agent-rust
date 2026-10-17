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
/// they merge — WP3's uploader now; WP1-REBASE: the WP2 recorder/disk/time
/// source/orphan passes and the WP4 capture supervisor still delegate to
/// [`pending::Inert`].
pub struct Backend {
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub tape: Arc<dyn TapeSink>,
}

impl manager::RecordingBackend for Backend {
    fn finalize_orphan_segments(&self, t: &Path) -> usize {
        manager::RecordingBackend::finalize_orphan_segments(&pending::Inert, t)
    }
    fn recover_orphan_video_sessions(&self, v: &Path) -> usize {
        manager::RecordingBackend::recover_orphan_video_sessions(&pending::Inert, v)
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
    fn capture(&self, spec: manager::CaptureSpec) -> Arc<dyn manager::CaptureApi> {
        manager::RecordingBackend::capture(&pending::Inert, spec)
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
        fn stop(&self) {}
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
        fn enforce(&self) -> BoxFut<'_, ()> {
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
