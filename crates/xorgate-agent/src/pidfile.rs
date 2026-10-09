//! `agent.pid` — JSON identity of the running worker (`src/daemon/pidfile.ts`
//! `PidRecord`). Written atomically on start, removed on graceful exit. The TS
//! CLI's `status` parses it with defaults for absent keys; `nodeVersion` is
//! omitted here (there is no Node), everything else is kept.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PidRecord {
    pub pid: u32,
    /// Epoch ms at start — anchors the identity check against process start time.
    pub started_at: i64,
    pub device_id: String,
    pub mode: String,
    pub rate_hz: f64,
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
