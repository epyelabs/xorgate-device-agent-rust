//! Graceful shutdown — `shutdown(sig, exitCode)` (`worker.ts:1291-1325`) in
//! the TS order, plus the SIGHUP-ignore handler (`:1327-1343`).
//!
//! Deliberate differences (named in `phase-2/evidence/wp1.md`):
//! - The TS prints `\n[agent] shutting down (SIG)…` (a blank journal line
//!   first); the Rust line has no leading newline.
//! - When the link is down the TS publish of `online:false` sits in mqtt.js's
//!   offline queue and the 2 s timer exits the process; the Rust agent skips
//!   the publish (nothing could carry it) and exits at once with the same code.
//! - `store.setRecording(recording.status())` (`:1307`) is the
//!   `final_recording_status` hook: `None` in Phase 2, so state.json
//!   `recording` stays absent (T6).

use std::future::Future;
use std::time::Duration;

use tracing::{error, info, warn};
use xorgate_core::telemetry::{ActivityLevel, ConnectionState, StatusPayload};

use crate::ctx::{AgentCtx, ConfigPlane, TelemetrySource};
use crate::mqtt_plane::{MqttHandle, MqttPlane, QoS};

/// The hard exit budget after the final publish (`setTimeout(…, 2000)`, `:1324`).
pub const EXIT_BUDGET: Duration = Duration::from_millis(2000);

/// The `sig` label of a shutdown forced by the MQTT loop task ending on its
/// own (`[agent] shutting down (mqtt event loop ended)…`).
pub const LOOP_ENDED: &str = "mqtt event loop ended";

/// The `sig` label of a shutdown forced by an error the TS leaves uncaught in
/// the recording manager (the 5 s settings poll's `reapply`, review-wp1 M1).
pub const RECORDING_FATAL: &str = "recording settings apply failed";

/// The recording manager's 5 s poll hit a throwing telemetry finalize. In the
/// TS this is an uncaught exception inside `setInterval` (`manager.ts:319`).
/// There is no `uncaughtException` handler, only the `unhandledRejection` net
/// at `worker.ts:656`, so Node prints the error and exits 1. systemd's
/// `Restart=always` restarts the agent, and the boot `finalizeOrphanSegments`
/// completes the stranded segment. Rust logs `[agent] fatal: <msg>`, the
/// stand-in for Node's uncaught dump (as A7 does for the boot reconcile),
/// then runs the normal [`Shutdown`] and exits with this code.
pub fn on_recording_fatal(msg: &str) -> i32 {
    error!("[agent] fatal: {msg}");
    1
}

/// The MQTT loop task ended without a shutdown (it ends only after the
/// cancel, so this is a panic inside it). Rust-only — mqtt.js has no such
/// path (review-wp1 #10a, review-wp1-followup #4): log, `lastError`, flush,
/// then the worker runs the normal [`Shutdown`] (pidfile removal, an
/// `online:false` attempt — the dead loop cannot carry it, the LWT does —
/// the ≤ 2 s budget) and exits with this code, so systemd's
/// `Restart=always` restarts a cleanly stopped process. `panic="abort"` is
/// O10's call at the gate, not made here.
pub fn on_mqtt_loop_ended(ctx: &AgentCtx) -> i32 {
    let t = ctx.now_ms();
    error!("[agent] mqtt event loop ended unexpectedly");
    ctx.state.set_error(LOOP_ENDED, t);
    ctx.state.flush(t, true);
    1
}

/// The SIGHUP handler (`worker.ts:1339-1343`), wording verbatim.
pub fn on_sighup(ctx: &AgentCtx) {
    let now = ctx.now_ms();
    warn!("[agent] SIGHUP ignored (tty hangup — likely modem USB drop/re-enumeration)");
    ctx.state.note(
        ActivityLevel::Warn,
        "SIGHUP ignored (likely modem USB re-enumeration)",
        now,
    );
}

/// What `shutdown` touches, borrowed from the worker.
pub struct Shutdown<'a> {
    pub ctx: &'a AgentCtx,
    pub mqtt: &'a MqttHandle,
    pub plane: &'a mut MqttPlane,
    pub config: &'a dyn ConfigPlane,
    pub source: &'a dyn TelemetrySource,
    /// Whether ending the client closes a stream (mqtt.js emits `close`): true
    /// for the real client, false for the `--offline` stub, whose `end()` just
    /// calls back (`mqtt.ts:97-100`).
    pub link_closes: bool,
}

impl Shutdown<'_> {
    /// Run the TS shutdown sequence; `end` is the client's graceful end
    /// (queue DISCONNECT behind the final publish, wait ≤ `budget`; returns
    /// whether it completed). Returns the process exit code.
    pub async fn run<F, Fut>(self, sig: &str, exit_code: i32, end: F) -> i32
    where
        F: FnOnce(Duration) -> Fut,
        Fut: Future<Output = bool>,
    {
        let ctx = self.ctx;
        // `if (shuttingDown) return; shuttingDown = true;` — the token also
        // stops the publish loop, the heartbeat and every subsystem timer.
        ctx.cancel.cancel();
        let now = ctx.now_ms();
        info!("[agent] shutting down ({sig})…");
        ctx.hooks.led_stop();
        self.config.stop();
        ctx.hooks.recording_stop();
        if let Some(status) = ctx.hooks.final_recording_status() {
            ctx.state.set_recording(status);
        }
        self.source.stop();
        if ctx.env.self_managed {
            if let Err(e) = crate::pidfile::remove_pidfile(&ctx.paths.pid) {
                warn!("[agent] pidfile remove failed: {e}");
            }
        }
        ctx.state.set_connection(ConnectionState::Closed, now);
        ctx.state
            .note(ActivityLevel::Info, format!("shutting down ({sig})"), now);

        let was_connected = self.plane.is_connected() && self.mqtt.connected();
        if was_connected {
            let cfg = &ctx.env.config;
            let offline = serde_json::to_vec(&StatusPayload::new(&cfg.device_id, now, false))
                .unwrap_or_default();
            if let Err(e) = self
                .mqtt
                .publish_raw(&cfg.topics.status, offline, QoS::AtMostOnce)
            {
                warn!("[agent] final status publish not queued: {e}");
            }
        }
        let clean = end(EXIT_BUDGET).await;
        if was_connected && self.link_closes {
            // mqtt.js emits `close` when end() closes the stream.
            self.plane.on_close();
            if clean {
                info!("[agent] disconnected cleanly (online:false sent)");
            } else {
                warn!(
                    "[agent] disconnect not confirmed within {} ms — exiting anyway",
                    EXIT_BUDGET.as_millis()
                );
            }
        }
        exit_code
    }
}
