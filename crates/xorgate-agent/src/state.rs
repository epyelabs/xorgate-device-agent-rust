//! `state.json` — the worker→reader IPC snapshot. Port of
//! `src/daemon/state.ts:19-128`: the `StateStore` (atomic write, one write
//! per 250 ms unless forced, a 20-event ring) behind a cloneable
//! [`StateHandle`], plus the reader half (`readState`, `staleThresholdMs`,
//! `isFresh`) the CLI uses. The snapshot type is `xorgate_core::telemetry::
//! StateSnapshot` (key order = the worker's initial literal).
//!
//! The sim host reads `connection / lastWriteAt / lastError / published`;
//! `xorgate status` / `xorgate-rs status` read the rest. `mediaEnabled` is
//! written (`!offline`, `config.ts:254`); `recording` stays absent until
//! Phase 3 sets it (T6).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use xorgate_core::telemetry::{
    ActivityEvent, ActivityLevel, ConnectionState, RecordingStatus, StateSnapshot, TelemetryPayload,
};

/// `STATE_THROTTLE_MS` (`state.ts:19`).
pub const STATE_THROTTLE_MS: i64 = 250;
/// `MAX_EVENTS` (`state.ts:20`).
pub const MAX_EVENTS: usize = 20;

/// `StateStore` (`state.ts:69-128`).
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

    /// `setConnection` (`state.ts:85-90`): `connectionSince` moves only on a change.
    pub fn set_connection(&mut self, connection: ConnectionState, now: i64) {
        if self.snap.connection != connection {
            self.snap.connection = connection;
            self.snap.connection_since = now;
        }
    }

    /// `setRecording` (`state.ts:92`).
    pub fn set_recording(&mut self, recording: RecordingStatus) {
        self.snap.recording = Some(recording);
    }

    /// `setObd` (`state.ts:94-98` @ 09da38d): `None` removes the key.
    pub fn set_obd(&mut self, obd: Option<xorgate_core::telemetry::ObdStatus>) {
        self.snap.obd = obd;
    }

    /// `setLatest` (`state.ts:96-99`).
    pub fn set_latest(&mut self, payload: TelemetryPayload) {
        self.snap.seq = payload.seq;
        self.snap.latest = Some(payload);
    }

    /// `recordPublish` (`state.ts:101-104`).
    pub fn record_publish(&mut self, now: i64) {
        self.snap.published += 1;
        self.snap.last_publish_at = Some(now);
    }

    /// `recordPublishError` (`state.ts:106-110`).
    pub fn record_publish_error(&mut self, msg: &str, now: i64) {
        self.snap.publish_errors += 1;
        self.snap.last_error = Some(msg.to_string());
        self.event(ActivityLevel::Error, format!("publish error: {msg}"), now);
    }

    /// `setError` (`state.ts:112-115`).
    pub fn set_error(&mut self, msg: &str, now: i64) {
        self.snap.last_error = Some(msg.to_string());
        self.event(ActivityLevel::Error, msg.to_string(), now);
    }

    /// `event` (`state.ts:117-122`): append, keep the newest 20.
    pub fn event(&mut self, level: ActivityLevel, msg: String, now: i64) {
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

    /// `flush` (`state.ts:124-128`): persist if forced or the throttle
    /// window elapsed. The TS lets a write error throw into its caller (and,
    /// from a timer, the process); here it is logged and the worker carries
    /// on — a state file is never worth the agent.
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

/// Cloneable, thread-safe handle on the one [`StateStore`] (frozen for wave B).
///
/// The low-level methods mirror the TS store 1:1 and take the caller's `now`
/// (the TS stamps `Date.now()` once per handler and reuses it). [`note`]
/// is the idiom every TS event site uses — `store.event(level, msg, t);
/// store.flush(t, true);` — in one call.
///
/// [`note`]: StateHandle::note
#[derive(Clone)]
pub struct StateHandle {
    inner: Arc<Mutex<StateStore>>,
}

impl StateHandle {
    pub fn new(initial: StateSnapshot, path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateStore::new(initial, path))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, StateStore> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `store.event(level, msg, now); store.flush(now, true)`.
    pub fn note(&self, level: ActivityLevel, msg: impl Into<String>, now: i64) {
        let mut s = self.lock();
        s.event(level, msg.into(), now);
        s.flush(now, true);
    }

    /// `store.event(level, msg, now)` (no flush).
    pub fn event(&self, level: ActivityLevel, msg: impl Into<String>, now: i64) {
        self.lock().event(level, msg.into(), now);
    }

    pub fn flush(&self, now: i64, force: bool) {
        self.lock().flush(now, force);
    }

    pub fn set_connection(&self, connection: ConnectionState, now: i64) {
        self.lock().set_connection(connection, now);
    }

    pub fn set_error(&self, msg: &str, now: i64) {
        self.lock().set_error(msg, now);
    }

    pub fn record_publish(&self, now: i64) {
        self.lock().record_publish(now);
    }

    pub fn record_publish_error(&self, msg: &str, now: i64) {
        self.lock().record_publish_error(msg, now);
    }

    pub fn set_latest(&self, payload: TelemetryPayload) {
        self.lock().set_latest(payload);
    }

    /// `store.setObd(obd)` (Phase 5b).
    pub fn set_obd(&self, obd: Option<xorgate_core::telemetry::ObdStatus>) {
        self.lock().set_obd(obd);
    }

    pub fn set_recording(&self, recording: RecordingStatus) {
        self.lock().set_recording(recording);
    }

    /// `store.snapshot().connection`.
    pub fn connection(&self) -> ConnectionState {
        self.lock().snapshot().connection
    }

    /// `store.snapshot().connection === "connected"` — the TS `online()` /
    /// `isOnline()` gates.
    pub fn is_connected(&self) -> bool {
        self.connection() == ConnectionState::Connected
    }

    /// `store.snapshot().published`.
    pub fn published(&self) -> u64 {
        self.lock().snapshot().published
    }

    /// A copy of the whole snapshot (tests, diagnostics).
    pub fn snapshot(&self) -> StateSnapshot {
        self.lock().snapshot().clone()
    }
}

// --- reader half (state.ts:22-62) -------------------------------------------

/// `isStateSnapshot` (`state.ts:23-37`): the fields the readers dereference.
pub fn is_state_snapshot(v: &Value) -> bool {
    let Some(o) = v.as_object() else {
        return false;
    };
    o.get("v").and_then(Value::as_f64) == Some(1.0)
        && o.get("deviceId").is_some_and(Value::is_string)
        && o.get("lastWriteAt").is_some_and(Value::is_number)
        && o.get("connection").is_some_and(Value::is_string)
        && o.get("connectionSince").is_some_and(Value::is_number)
        && o.get("seq").is_some_and(Value::is_number)
        && o.get("published").is_some_and(Value::is_number)
        && o.get("recentEvents").is_some_and(Value::is_array)
}

/// `readState` (`state.ts:40-52`): the snapshot as JSON, or `None` when
/// missing / unreadable / not JSON / failing the structural gate. Kept as a
/// `Value` (the TS returns the parsed object as-is), so a reader sees exactly
/// what either agent wrote.
pub fn read_state(path: &Path) -> Option<Value> {
    let text = xorgate_hal::read_lossy::read_text(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    is_state_snapshot(&v).then_some(v)
}

/// `staleThresholdMs` (`state.ts:55-57`).
pub fn stale_threshold_ms(interval_ms: f64, throttle_ms: f64) -> f64 {
    (interval_ms * 3.0 + throttle_ms).max(3000.0)
}

/// `isFresh` (`state.ts:59-61`).
pub fn is_fresh(last_write_at: f64, now: f64, interval_ms: f64) -> bool {
    now - last_write_at < stale_threshold_ms(interval_ms, STATE_THROTTLE_MS as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_core::telemetry::{JsNumber, RunMode};

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// `snap()` of state.test.ts:22-42.
    fn snap() -> StateSnapshot {
        StateSnapshot {
            v: 1,
            written_by: "test".into(),
            device_id: "d".into(),
            mode: RunMode::Sim,
            media_enabled: None,
            rate_hz: JsNumber(1.0),
            endpoint: "offline".into(),
            offline: true,
            pid: 1,
            started_at: 0,
            last_write_at: 0,
            connection: ConnectionState::Connected,
            connection_since: 0,
            seq: 0,
            published: 0,
            publish_errors: 0,
            last_publish_at: None,
            last_error: None,
            latest: None,
            recent_events: vec![],
            recording: None,
            obd: None,
        }
    }

    /// state.test.ts:45 "staleThresholdMs has a 3s floor and scales with interval".
    #[test]
    fn stale_threshold_floor_and_scale() {
        assert_eq!(stale_threshold_ms(1000.0, 250.0), 3250.0);
        assert_eq!(stale_threshold_ms(100.0, 250.0), 3000.0);
    }

    /// state.test.ts:50 "isFresh just inside / just outside the window".
    #[test]
    fn is_fresh_window() {
        let now = 1_000_000.0;
        assert!(is_fresh(now, now, 1000.0));
        assert!(is_fresh(now - 3000.0, now, 1000.0));
        assert!(!is_fresh(now - 5000.0, now, 1000.0));
    }

    /// state.test.ts:59 "writes valid JSON and leaves no temp files".
    #[test]
    fn writes_valid_json_without_temp_files() {
        let h = home("write");
        let p = h.join("state.json");
        let s = StateSnapshot { seq: 7, ..snap() };
        xorgate_hal::atomic_write::atomic_write_json(&p, &s).unwrap();
        assert_eq!(read_state(&p).unwrap()["seq"], 7);
        let leftovers: Vec<_> = std::fs::read_dir(&h)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&h);
    }

    /// state.test.ts:67 "readState returns null on missing, garbage, and schema-mismatch".
    #[test]
    fn read_state_rejects_missing_garbage_mismatch() {
        let h = home("read");
        let p = h.join("state.json");
        assert!(read_state(&p).is_none());
        xorgate_hal::atomic_write::atomic_write_json(&p, &serde_json::json!({"not": "a snapshot"}))
            .unwrap();
        assert!(read_state(&p).is_none());
        xorgate_hal::atomic_write::atomic_write_json(
            &p,
            &serde_json::json!({"v": 1, "deviceId": "d"}),
        )
        .unwrap();
        assert!(read_state(&p).is_none());
        std::fs::write(&p, "{garbage").unwrap();
        assert!(read_state(&p).is_none());
        let _ = std::fs::remove_dir_all(&h);
    }

    /// state.test.ts:78 "only flushes once per throttle window unless forced".
    #[test]
    fn throttles_unless_forced() {
        let h = home("throttle");
        let p = h.join("state.json");
        let store = StateHandle::new(snap(), p.clone());
        let last = || read_state(&p).unwrap()["lastWriteAt"].as_i64().unwrap();
        store.flush(1000, true);
        assert_eq!(last(), 1000);
        store.record_publish(1100);
        store.flush(1100, false);
        assert_eq!(last(), 1000);
        store.flush(1300, false);
        assert_eq!(last(), 1300);
        let _ = std::fs::remove_dir_all(&h);
    }

    /// state.test.ts:91 "trims the event ring buffer".
    #[test]
    fn trims_the_event_ring() {
        let h = home("ring");
        let p = h.join("state.json");
        let store = StateHandle::new(snap(), p.clone());
        for i in 0..30 {
            store.event(ActivityLevel::Info, format!("e{i}"), i);
        }
        store.flush(10_000, true);
        let back = read_state(&p).unwrap();
        let ev = back["recentEvents"].as_array().unwrap();
        assert_eq!(ev.len(), 20);
        assert_eq!(ev[0]["msg"], "e10");
        let _ = std::fs::remove_dir_all(&h);
    }

    /// Serde exactness: key order of the worker's initial literal
    /// (`worker.ts:356-377`), `mediaEnabled` after `mode`, `recording` absent.
    #[test]
    fn initial_snapshot_bytes() {
        let s = StateSnapshot {
            written_by: "xorgate-device-agent-rs@0.0.1".into(),
            mode: RunMode::Real,
            media_enabled: Some(true),
            endpoint: "e".into(),
            offline: false,
            started_at: 2,
            last_write_at: 3,
            connection: ConnectionState::Connecting,
            connection_since: 2,
            recent_events: vec![ActivityEvent {
                ts: 2,
                level: ActivityLevel::Info,
                msg: "agent starting (mode=real)".into(),
            }],
            ..snap()
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"v":1,"writtenBy":"xorgate-device-agent-rs@0.0.1","deviceId":"d","mode":"real","mediaEnabled":true,"rateHz":1,"endpoint":"e","offline":false,"pid":1,"startedAt":2,"lastWriteAt":3,"connection":"connecting","connectionSince":2,"seq":0,"published":0,"publishErrors":0,"lastPublishAt":null,"lastError":null,"latest":null,"recentEvents":[{"ts":2,"level":"info","msg":"agent starting (mode=real)"}]}"#
        );
    }
}
