//! `state.json` — the worker→reader IPC snapshot (`src/daemon/state.ts`
//! `StateSnapshot` + `StateStore`). Written atomically, throttled to one
//! write per 250 ms unless forced (connection/error/shutdown events), with a
//! 20-event ring buffer. The sim host reads `connection / lastWriteAt /
//! lastError / published`; the TS CLI's `status`/`monitor` read the rest.
//!
//! Phase 0 writes every field the TS writes except the optional
//! `mediaEnabled` and `recording` (no media/recording subsystems yet).

use std::path::PathBuf;

use serde::Serialize;
use xorgate_cloud::mqtt::ConnectionState;
use xorgate_core::telemetry::{JsNumber, TelemetryPayload};

pub const STATE_THROTTLE_MS: i64 = 250;
const MAX_EVENTS: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct ActivityEvent {
    pub ts: i64,
    pub level: &'static str,
    pub msg: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub v: u8,
    pub written_by: String,
    pub device_id: String,
    pub mode: String,
    pub rate_hz: JsNumber,
    pub endpoint: String,
    pub offline: bool,
    pub pid: u32,
    pub started_at: i64,
    pub last_write_at: i64,
    pub connection: &'static str,
    pub connection_since: i64,
    pub seq: u64,
    pub published: u64,
    pub publish_errors: u64,
    pub last_publish_at: Option<i64>,
    pub last_error: Option<String>,
    pub latest: Option<TelemetryPayload>,
    pub recent_events: Vec<ActivityEvent>,
}

pub struct StateStore {
    snap: StateSnapshot,
    last_flush_at: i64,
    path: PathBuf,
}

impl StateStore {
    pub fn new(initial: StateSnapshot, path: PathBuf) -> Self {
        Self {
            snap: initial,
            last_flush_at: 0,
            path,
        }
    }

    pub fn snapshot(&self) -> &StateSnapshot {
        &self.snap
    }

    pub fn set_connection(&mut self, connection: ConnectionState, now: i64) {
        let s = connection.as_str();
        if self.snap.connection != s {
            self.snap.connection = s;
            self.snap.connection_since = now;
        }
    }

    pub fn set_latest(&mut self, payload: TelemetryPayload) {
        self.snap.seq = payload.seq;
        self.snap.latest = Some(payload);
    }

    pub fn record_publish(&mut self, now: i64) {
        self.snap.published += 1;
        self.snap.last_publish_at = Some(now);
    }

    pub fn record_publish_error(&mut self, msg: &str, now: i64) {
        self.snap.publish_errors += 1;
        self.snap.last_error = Some(msg.to_string());
        self.event("error", format!("publish error: {msg}"), now);
    }

    pub fn set_error(&mut self, msg: &str, now: i64) {
        self.snap.last_error = Some(msg.to_string());
        self.event("error", msg.to_string(), now);
    }

    pub fn event(&mut self, level: &'static str, msg: String, now: i64) {
        self.snap.recent_events.push(ActivityEvent {
            ts: now,
            level,
            msg,
        });
        if self.snap.recent_events.len() > MAX_EVENTS {
            let drop = self.snap.recent_events.len() - MAX_EVENTS;
            self.snap.recent_events.drain(..drop);
        }
    }

    /// Persist if forced or the throttle window has elapsed. Never panics:
    /// a failed state write is logged and the worker carries on.
    pub fn flush(&mut self, now: i64, force: bool) {
        if !force && now - self.last_flush_at < STATE_THROTTLE_MS {
            return;
        }
        self.snap.last_write_at = now;
        self.last_flush_at = now;
        if let Err(e) = xorgate_hal::atomic_write::atomic_write_json(&self.path, &self.snap) {
            tracing::warn!("[agent] state.json write failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_key_order_matches_the_ts() {
        let s = StateSnapshot {
            v: 1,
            written_by: "xorgate-device-agent-rs@0.0.1".into(),
            device_id: "d".into(),
            mode: "real".into(),
            rate_hz: JsNumber(1.0),
            endpoint: "e".into(),
            offline: false,
            pid: 1,
            started_at: 2,
            last_write_at: 3,
            connection: "connecting",
            connection_since: 2,
            seq: 0,
            published: 0,
            publish_errors: 0,
            last_publish_at: None,
            last_error: None,
            latest: None,
            recent_events: vec![ActivityEvent {
                ts: 2,
                level: "info",
                msg: "agent starting (mode=real)".into(),
            }],
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"v":1,"writtenBy":"xorgate-device-agent-rs@0.0.1","deviceId":"d","mode":"real","rateHz":1,"endpoint":"e","offline":false,"pid":1,"startedAt":2,"lastWriteAt":3,"connection":"connecting","connectionSince":2,"seq":0,"published":0,"publishErrors":0,"lastPublishAt":null,"lastError":null,"latest":null,"recentEvents":[{"ts":2,"level":"info","msg":"agent starting (mode=real)"}]}"#
        );
    }
}
