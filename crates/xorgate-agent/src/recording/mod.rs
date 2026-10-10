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
