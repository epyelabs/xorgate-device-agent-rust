//! STUB (Phase 2 WP1) — **WP4 replaces this file wholesale**.
//!
//! Frozen signature (see `phase-2/evidence/wp1.md` "Interfaces for wave B"):
//!
//! ```ignore
//! pub fn start(ctx: &AgentCtx, deps: CommandPlaneDeps) -> Arc<dyn CommandPlane>;
//! ```
//!
//! `handleCommandMessage` (`worker.ts:988-1126`): `on_message` gets every
//! `…/command` payload in broker order; parse/decide/claim run sequentially
//! on the plane's task, execution is spawned (O11). Acks go through
//! `deps.mqtt.publish_acked(…, 3 s)`; `refresh` asks `deps.reports` for two
//! `boot` reports.

use std::sync::Arc;

use tracing::warn;

use crate::ctx::{AgentCtx, CommandPlane, CommandPlaneDeps};

/// The command plane (stub: logs and drops every command; nothing is acked).
pub fn start(_ctx: &AgentCtx, _deps: CommandPlaneDeps) -> Arc<dyn CommandPlane> {
    Arc::new(StubCommandPlane)
}

struct StubCommandPlane;

impl CommandPlane for StubCommandPlane {
    fn on_message(&self, payload: Vec<u8>) {
        warn!(
            "[agent] command received ({} bytes) — not executed (command plane arrives with WP4)",
            payload.len()
        );
    }
}
