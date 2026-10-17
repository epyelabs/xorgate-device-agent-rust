//! `counters.json` at the recordings root (`src/recording/disk.ts:226-250`):
//! `loadCounters()` re-reads it on every `stats()` / `enforce()` (so an
//! external `xorgate recording reset-counters` applies live) and
//! `saveCounters()` writes `{"v":1,"droppedSegments":N}` atomically. The
//! parse rule and the bytes are core's [`xorgate_core::disk_plan`].

use std::path::{Path, PathBuf};

use xorgate_core::disk_plan::{counters_json, parse_counters, COUNTER_FILE};

use crate::atomic_write::{atomic_write_bytes_traced, AtomicWriteError};

/// `counterPath()` (`disk.ts:226-228`).
pub fn counter_path(root: &Path) -> PathBuf {
    root.join(COUNTER_FILE)
}

/// `loadCounters().droppedSegments` (`disk.ts:230-242`): bytes read then
/// decoded lossily (D4); absent/unreadable/corrupt → 0.
pub fn load_dropped(root: &Path) -> f64 {
    let text = crate::read_lossy::read_text(&counter_path(root)).ok();
    parse_counters(text.as_deref())
}

/// `saveCounters()` body (`disk.ts:244-250`): `atomicWriteJson(path, {v: 1,
/// droppedSegments})`. The caller prints `[recording] counter persist
/// failed: <message>` on an error.
pub fn save_dropped(root: &Path, dropped: f64) -> Result<(), AtomicWriteError> {
    atomic_write_bytes_traced(&counter_path(root), counters_json(dropped).as_bytes())
}
