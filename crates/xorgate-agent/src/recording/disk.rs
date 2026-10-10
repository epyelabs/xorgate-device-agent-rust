//! Recording disk manager — port of `src/recording/disk.ts` (251 lines):
//! quota + eviction over the whole recordings tree (telemetry AND video, one
//! generic scan). Decisions are core's [`xorgate_core::disk_plan`]; the walk
//! and `counters.json` I/O are `xorgate-hal::{disk_scan, counters_file}`.
//!
//! Policy: enforce `retention.maxBytes` over the tree AND
//! `retention.minFreeBytes` of filesystem headroom; under pressure delete the
//! oldest CLOSED segments first (mtime across the whole tree) and count each
//! into the persistent `droppedSegments`. Active segments (registered by
//! their writers) and unknown file types are never touched. Driven by the
//! manager on rotation events + a slow (60 s) timer (WP1).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use xorgate_core::disk_plan::{
    active_entry, active_entry_owned_by, active_entry_path, backlog_after,
    counter_persist_failed_error, evictable_order, evicted_warning, eviction_failed_error,
    sidecar_of_mp4, BacklogStats, EvictionPass,
};
use xorgate_core::recording_config::ResolvedRetention;
use xorgate_hal::counters_file::{load_dropped, save_dropped};
use xorgate_hal::disk_scan::{prune_empty_session_dirs, scan};
use xorgate_hal::io::{node_fs_message, NullTape, TapeSink};

use crate::ctx::BoxFut;

/// `freeBytes()` (`DiskManagerOptions.freeBytes`): filesystem free bytes, or
/// `None` when unknown (the floor then never applies).
pub type FreeBytesFn = Arc<dyn Fn() -> BoxFut<'static, Option<f64>> + Send + Sync>;

struct DiskState {
    retention: ResolvedRetention,
    /// `this.dropped` — a JS number (the file's value, incremented).
    dropped: f64,
    /// `this.lastBacklog` — `(segments, bytes)`.
    last_backlog: (u64, u64),
    /// `this.activePaths` — `"<owner> <path>"` entries.
    active: Vec<String>,
}

/// `class DiskManager` (`disk.ts:61-251`). Cheap to share behind an `Arc`.
pub struct DiskManager {
    root: PathBuf,
    free_bytes: FreeBytesFn,
    state: Mutex<DiskState>,
    enforcing: AtomicBool,
    tape: Arc<dyn TapeSink>,
}

/// Clears `enforcing` on every exit of `enforce()` (its `finally`).
struct EnforceGuard<'a>(&'a AtomicBool);

impl Drop for EnforceGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The default probe (`disk.ts:73-78`): `readDisk(root)` → `totalBytes -
/// usedBytes` (df semantics: root-reserved blocks count as used), `None`
/// when the root cannot be stat'ed.
pub fn default_free_bytes(root: PathBuf) -> FreeBytesFn {
    Arc::new(move || {
        let root = root.clone();
        Box::pin(async move {
            let d = xorgate_hal::system::read_disk(&root.to_string_lossy())?;
            Some(d.total_bytes - d.used_bytes)
        })
    })
}

impl DiskManager {
    /// `new DiskManager({root, retention})` (`disk.ts:70-80`): the counter is
    /// loaded from `counters.json` at once. The free-bytes probe is `df` on
    /// the root; [`DiskManager::with_free_bytes`] injects one (tests).
    pub fn new(root: PathBuf, retention: ResolvedRetention) -> Self {
        let free = default_free_bytes(root.clone());
        Self::with_free_bytes(root, retention, free)
    }

    /// `new DiskManager({root, retention, freeBytes})`.
    pub fn with_free_bytes(root: PathBuf, retention: ResolvedRetention, free_bytes: FreeBytesFn) -> Self {
        let dropped = load_dropped(&root);
        Self {
            root,
            free_bytes,
            state: Mutex::new(DiskState {
                retention,
                dropped,
                last_backlog: (0, 0),
                active: Vec::new(),
            }),
            enforcing: AtomicBool::new(false),
            tape: Arc::new(NullTape),
        }
    }

    /// Attach the record/replay sink (eviction deletes are `fs_out` rows).
    pub fn with_tape(mut self, tape: Arc<dyn TapeSink>) -> Self {
        self.tape = tape;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DiskState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `setRetention` (`disk.ts:82-84`).
    pub fn set_retention(&self, retention: ResolvedRetention) {
        self.lock().retention = retention;
    }

    /// `setActivePath(owner, path)` (`disk.ts:87-92`): writers register their
    /// active (open) file so eviction can never touch it; `None` clears the
    /// owner's registration.
    pub fn set_active_path(&self, owner: &str, path: Option<&Path>) {
        let mut st = self.lock();
        st.active.retain(|e| !active_entry_owned_by(e, owner));
        if let Some(p) = path {
            let p = p.to_string_lossy();
            if !p.is_empty() {
                let entry = active_entry(owner, &p);
                if !st.active.contains(&entry) {
                    st.active.push(entry);
                }
            }
        }
    }

    /// `stats()` (`disk.ts:94-106`): re-syncs `droppedSegments` from the
    /// persisted counter so an external reset (`xorgate recording
    /// reset-counters`, a separate process) shows live.
    pub fn stats(&self) -> BacklogStats {
        let dropped = load_dropped(&self.root);
        let mut st = self.lock();
        st.dropped = dropped;
        BacklogStats {
            backlog_segments: st.last_backlog.0,
            backlog_bytes: st.last_backlog.1,
            dropped_segments: dropped.max(0.0) as u64,
        }
    }

    /// `enforce()` (`disk.ts:113-168`): scan, refresh backlog stats, evict
    /// oldest closed segments while over quota or under the free floor.
    /// Serialized: an overlapping call (rotation event during the slow
    /// timer's run) returns at once.
    pub async fn enforce(&self) {
        if self
            .enforcing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let _guard = EnforceGuard(&self.enforcing);
        // Adopt the persisted counter as the base so an external reset
        // landing between ticks is never clobbered (Phase 6.5).
        let dropped = load_dropped(&self.root);
        self.lock().dropped = dropped;
        let files = scan(&self.root);
        let free = (self.free_bytes)().await;

        // From here on the TS is synchronous: retention and the active set
        // are read AFTER the await (`disk.ts:124-135`).
        let mut st = self.lock();
        let retention = st.retention.clone();
        let active: HashSet<String> = st
            .active
            .iter()
            .map(|e| active_entry_path(e).to_string())
            .collect();
        let evictable = evictable_order(&files, &active);
        let mut pass = EvictionPass::new(&files, free);
        let mut deleted: HashSet<usize> = HashSet::new();
        for &i in &evictable {
            if !pass.under_pressure(&retention) {
                break;
            }
            let f = &files[i];
            let path = Path::new(&f.path);
            if let Err(e) = std::fs::remove_file(path) {
                let msg = node_fs_message(&e, "unlink", path);
                tracing::error!("{}", eviction_failed_error(&f.path, &msg));
                continue;
            }
            deleted.insert(i);
            pass.evicted(f.size);
            st.dropped += 1.0;
            if let Some(sidecar) = sidecar_of_mp4(&f.path) {
                // no sidecar yet — fine
                let _ = std::fs::remove_file(&sidecar);
            }
        }
        if !deleted.is_empty() {
            tracing::warn!("{}", evicted_warning(deleted.len()));
            if let Err(e) = save_dropped(&self.root, st.dropped) {
                tracing::error!("{}", counter_persist_failed_error(&e.node_message()));
            }
            prune_empty_session_dirs(&self.root);
        }
        st.last_backlog = backlog_after(&files, &evictable, &deleted);
        let _ = &self.tape;
    }
}
