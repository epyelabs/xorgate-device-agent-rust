//! Graceful shutdown — `shutdown(sig, exitCode)` (`worker.ts:1291-1325`) in
//! the TS order, plus the SIGHUP-ignore handler (`:1327-1343`).
//!
//! Deliberate differences (named in `phase-2/evidence/wp1.md`):
//! - The TS prints `\n[agent] shutting down (SIG)…` (a blank journal line
//!   first); the Rust line has no leading newline.
//! - When the link is down the TS publish of `online:false` sits in mqtt.js's
//!   offline queue and the 2 s timer exits the process; the Rust agent skips
//!   the publish (nothing could carry it) and exits at once with the same code.
//! - `store.setRecording(recording.status())` is Phase 3 (state.json
//!   `recording` stays absent, T6).

use std::future::Future;
use std::time::Duration;

use tracing::{info, warn};
use xorgate_core::telemetry::{ActivityLevel, ConnectionState, StatusPayload};

use crate::ctx::{AgentCtx, ConfigPlane, TelemetrySource};
use crate::mqtt_plane::{MqttHandle, MqttPlane, QoS};

/// The hard exit budget after the final publish (`setTimeout(…, 2000)`, `:1324`).
pub const EXIT_BUDGET: Duration = Duration::from_millis(2000);

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
