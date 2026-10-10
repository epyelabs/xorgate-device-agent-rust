//! Phase 3 WP5 — live media: the KVS WebRTC master supervisor and the
//! hardware reconcile that renders the pipelines it loads.
//!
//! - [`config`] — `MediaConfig` (`ts:src/agent/config.ts:244-275`, P4 install-root defaults);
//! - [`supervisor`] — `createMediaCollector` (`ts:src/agent/collectors/media.ts`):
//!   one `xorgate-kvs-webrtc` master per manifest channel, decisions in
//!   `xorgate_core::media_ladder`;
//! - [`reconcile`] — `reconcileHardware` (`ts:src/media/hardware-reconcile.ts`)
//!   and `resolveChannelPipeline` (`ts:src/media/pipeline.ts:79-87`).
//!
//! The spawn guard's rule is `xorgate_core::stream_guard::stream_guard_verdict`;
//! WP1 builds the closure (`worker.ts:209-220`) over the reconcile statuses.

pub mod config;
pub mod reconcile;
pub mod supervisor;

pub use config::MediaConfig;
pub use reconcile::{
    real_list_exec, reconcile_hardware, resolve_channel_pipeline, ListExec, ReconcileLevel,
    ReconcileLog, ReconcileOpts, ReconcileResult,
};
pub use supervisor::{GuardFn, MasterBackend, MasterProc, MediaSupervisor, RealMasterBackend};
pub use xorgate_core::stream_guard::{stream_guard_verdict, StreamGuard, StreamHardwareStatus};
