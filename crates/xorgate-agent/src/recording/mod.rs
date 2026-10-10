//! On-device recording (Phase 3): telemetry recorder, segments, disk
//! manager, uploader + segment sources, video capture, and the manager that
//! owns them. Module list: append-only, alphabetical (shared file).

pub mod capture;
pub mod sources;
pub mod uploader;
