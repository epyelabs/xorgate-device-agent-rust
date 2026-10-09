//! STUB (Phase 2 WP1) — **WP3 replaces this file wholesale** (and adds
//! `sync.rs`, `message.rs` beside it).
//!
//! Frozen signatures (see `phase-2/evidence/wp1.md` "Interfaces for wave B"):
//!
//! ```ignore
//! pub fn boot(ctx: &AgentCtx);
//! pub fn boot_upload_options(ctx: &AgentCtx);
//! pub fn start(ctx: &AgentCtx, deps: ConfigPlaneDeps) -> Arc<dyn ConfigPlane>;
//! ```
//!
//! - `boot` = `ensureConfigSyncBaseline()` + `migrateMediaEnabled(…)` with
//!   its two log lines (`worker.ts:92-105`), called right after the home
//!   exists and BEFORE the hardware block is loaded.
//! - `boot_upload_options` = the `buildUploadOptions()` call the TS makes
//!   while constructing the RecordingManager (`worker.ts:439`): Phase 2 only
//!   needs its `[agent] segment upload disabled (…)` log line (CF33).
//! - `start` = `new ConfigSync({...})` + `startWatcher()` + `startApnWatch()`
//!   (`worker.ts:700-746`), called after the MQTT client exists and before
//!   any message can arrive. Config messages reach `on_message` in broker
//!   order and must be handled strictly sequentially (O11); `on_connect` is
//!   `request("boot") + startHeartbeat()`; `deps.report_rx` carries every
//!   `ReportRequester::request` made elsewhere.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::info;
use xorgate_core::config_report::ReportOrigin;

use crate::ctx::{AgentCtx, ConfigPlane, ConfigPlaneDeps};

/// Boot-time settings.json upgrades (stub: none).
pub fn boot(_ctx: &AgentCtx) {}

/// The boot-time `buildUploadOptions()` (stub: silent).
pub fn boot_upload_options(_ctx: &AgentCtx) {}

/// The config plane (stub: logs and drops every message, drains report requests).
pub fn start(ctx: &AgentCtx, deps: ConfigPlaneDeps) -> Arc<dyn ConfigPlane> {
    let mut rx: mpsc::UnboundedReceiver<ReportOrigin> = deps.report_rx;
    let cancel = ctx.cancel.clone();
    ctx.spawn("config_plane_stub", async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                r = rx.recv() => if r.is_none() { break },
            }
        }
    });
    Arc::new(StubConfigPlane)
}

struct StubConfigPlane;

impl ConfigPlane for StubConfigPlane {
    fn on_connect(&self) {}

    fn on_message(&self, payload: Vec<u8>) {
        info!(
            "[agent] config message received ({} bytes) — not applied (config plane arrives with WP3)",
            payload.len()
        );
    }

    fn stop(&self) {}
}
