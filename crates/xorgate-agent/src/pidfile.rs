//! `agent.pid` — JSON identity of the running worker (`src/daemon/pidfile.ts`
//! `PidRecord`). Written atomically on start, removed on graceful exit. The TS
//! CLI's `status` parses it with defaults for absent keys; `nodeVersion` is
//! omitted here (there is no Node), everything else is kept.

use std::path::{Path, PathBuf};

use serde::Serialize;
use xorgate_core::telemetry::JsNumber;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PidRecord {
    pub pid: u32,
    /// Epoch ms at start — anchors the identity check against process start time.
    pub started_at: i64,
    pub device_id: String,
    pub mode: String,
    /// `JsNumber` so `1.0` prints as `1`, byte-shaped like the TS pidfile (and state.json).
    pub rate_hz: JsNumber,
    pub offline: bool,
    /// Absolute path of the worker binary (the TS writes `dist/worker.js`).
    pub worker: String,
    pub argv: Vec<String>,
}

pub fn pid_path(home: &Path) -> PathBuf {
    home.join("agent.pid")
}

pub fn write(home: &Path, record: &PidRecord) -> std::io::Result<()> {
    xorgate_hal::atomic_write::atomic_write_json(&pid_path(home), record)
}

pub fn remove(home: &Path) {
    if let Err(e) = std::fs::remove_file(pid_path(home)) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("[agent] pidfile remove failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_hz_prints_like_js() {
        let rec = PidRecord {
            pid: 4242,
            started_at: 1_791_513_164_828,
            device_id: "dev".into(),
            mode: "real".into(),
            rate_hz: JsNumber(1.0),
            offline: false,
            worker: "/opt/xorgate-rs/bin/xorgate-rs".into(),
            argv: vec!["run".into()],
        };
        let json = serde_json::to_string(&rec).unwrap();
        // The TS writes `JSON.stringify({... rateHz: 1 ...})` → `"rateHz":1`, never `1.0`.
        assert!(json.contains(r#""rateHz":1,"#), "{json}");
        assert_eq!(
            json,
            r#"{"pid":4242,"startedAt":1791513164828,"deviceId":"dev","mode":"real","rateHz":1,"offline":false,"worker":"/opt/xorgate-rs/bin/xorgate-rs","argv":["run"]}"#
        );
        let rec = PidRecord {
            rate_hz: JsNumber(0.5),
            ..rec
        };
        assert!(serde_json::to_string(&rec)
            .unwrap()
            .contains(r#""rateHz":0.5,"#));
    }
}
