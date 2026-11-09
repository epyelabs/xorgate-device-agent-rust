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
//! - Crash-equivalent exits (review-wp1 X1): where the TS process dies on an
//!   uncaught throw — the 5 s poll's finalize ([`RECORDING_FATAL`],
//!   `manager.ts:319`) or `recording.stop()` throwing in the signal handler
//!   (D29, `worker.ts:1306`) — it never reaches the `online:false` publish
//!   (`:1317-1321`) and its socket closes without a DISCONNECT, so the broker
//!   fires the Last-Will (`mqtt.ts:44-47`). Rust runs the local teardown but
//!   skips the publish and ends the client with [`ClientEnd::Abort`]. The
//!   rule is keyed on "the TS would have crashed", not on the exit code
//!   (D29 exits 0; the TS replay `shutdown(cmd, code)` is a graceful non-zero).

use std::future::Future;
use std::time::Duration;

use tracing::{error, info, warn};
use xorgate_core::telemetry::{ActivityLevel, ConnectionState, StatusPayload};

use crate::ctx::{AgentCtx, ConfigPlane, TelemetrySource};
use crate::mqtt_plane::{MqttHandle, MqttPlane, QoS};

/// A shutdown asked for from inside the agent: the replay container exit
/// (`shutdown(\`${cmd} command\`, code)`, `worker.ts:1256-1264` @ 09da38d),
/// which runs the normal graceful [`Shutdown`] and exits with `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitRequest {
    /// The `shutting down (<reason>)…` label.
    pub reason: String,
    pub code: i32,
}

/// The ctx-owned sender half ([`AgentCtx::exit_requests`]); the worker's main
/// loop holds the receiver. Cheap to clone. A send after the receiver is gone
/// (shutdown already under way) is dropped, like a second TS `shutdown()`.
#[derive(Debug, Clone)]
pub struct ExitRequests(tokio::sync::mpsc::UnboundedSender<ExitRequest>);

impl ExitRequests {
    pub fn channel() -> (Self, tokio::sync::mpsc::UnboundedReceiver<ExitRequest>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self(tx), rx)
    }
    /// Ask the worker to shut down; false when nobody listens.
    pub fn request(&self, reason: &str, code: i32) -> bool {
        self.0
            .send(ExitRequest {
                reason: reason.to_string(),
                code,
            })
            .is_ok()
    }
}

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
/// then runs the [`Shutdown`] local teardown and exits with this code. Like
/// the TS crash it sends no `online:false` and no DISCONNECT: the link is
/// dropped and the broker fires the Last-Will (review-wp1 X1).
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

/// How [`Shutdown::run_with`] asks the caller to end the MQTT client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientEnd {
    /// `client.end()` (`worker.ts:1322`): queue DISCONNECT behind the final
    /// `online:false`, wait ≤ the budget; the broker discards the will.
    Graceful(Duration),
    /// The TS would have crashed (X1): drop the connection WITHOUT a
    /// DISCONNECT so the broker publishes the Last-Will.
    Abort,
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
    /// [`run_with`](Self::run_with) for a caller whose `end` is the graceful
    /// end only (budget in, completed out). On a crash-equivalent exit `end`
    /// is not called; dropping it leaves the link to close with the process.
    pub async fn run<F, Fut>(self, sig: &str, exit_code: i32, end: F) -> i32
    where
        F: FnOnce(Duration) -> Fut,
        Fut: Future<Output = bool>,
    {
        self.run_with(sig, exit_code, |how| async move {
            match how {
                ClientEnd::Graceful(budget) => end(budget).await,
                ClientEnd::Abort => true,
            }
        })
        .await
    }

    /// Run the TS shutdown sequence; `end` ends the client as asked (see
    /// [`ClientEnd`]; returns whether a graceful end completed in budget).
    /// Returns the process exit code.
    pub async fn run_with<F, Fut>(self, sig: &str, exit_code: i32, end: F) -> i32
    where
        F: FnOnce(ClientEnd) -> Fut,
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
        // X1: graceful only when the TS reaches `:1317` — not after an
        // uncaught recording throw (RECORDING_FATAL) and not when the
        // finalize here throws (D29; the hook already logged `[agent] fatal:`).
        let finalized = ctx.hooks.recording_stop().is_ok();
        let graceful = finalized && sig != RECORDING_FATAL;
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
        if was_connected && graceful {
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
        let clean = end(if graceful {
            ClientEnd::Graceful(EXIT_BUDGET)
        } else {
            ClientEnd::Abort
        })
        .await;
        if was_connected && self.link_closes {
            // mqtt.js emits `close` when end() closes the stream.
            self.plane.on_close();
            if !graceful {
                warn!(
                    "[agent] connection dropped without DISCONNECT (Last-Will left to the broker)"
                );
            } else if clean {
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
