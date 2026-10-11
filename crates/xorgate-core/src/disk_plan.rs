//! The decisions of the recording disk manager, `src/recording/disk.ts`
//! (251 lines): which scanned files are segments, which are evictable, the
//! oldest-first eviction walk under `retention.maxBytes` / `minFreeBytes`,
//! the backlog numbers, and the `counters.json` bytes.
//!
//! Policy (`disk.ts:7-12`): enforce `retention.maxBytes` over the whole
//! recordings tree AND `retention.minFreeBytes` of filesystem headroom; under
//! pressure delete the oldest CLOSED segments first (mtime order across the
//! tree — telemetry and video interleave chronologically) and count each
//! into the persistent `droppedSegments`. The active segments (registered by
//! their writers) and unknown file types are never touched.
//!
//! Out of core: the recursive scan, `statSync`, `unlinkSync`, the
//! `df`-backed free-bytes probe, `rmdirSync` of drained session dirs and
//! the atomic `counters.json` write (`xorgate-hal::{disk_scan,
//! counters_file}`, the agent's `recording::disk`).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::recording_config::ResolvedRetention;
use crate::telemetry::JsNumber;

/// `CLOSED_SUFFIXES` (`disk.ts:25`): a CLOSED (evictable, uploadable)
/// segment. Video `.mp4`s count from the moment splitmuxsink creates them —
/// the one still being written is protected via `setActivePath`.
pub const CLOSED_SUFFIXES: [&str; 2] = [".jsonl.gz", ".mp4"];

/// `COUNTER_FILE` (`disk.ts:28`): persistent counters at the recordings root.
pub const COUNTER_FILE: &str = "counters.json";

/// `SESSION_MANIFEST` (`disk.ts:31`): per-session bookkeeping, never scanned.
pub const SESSION_MANIFEST: &str = "session.json";

/// `ScannedFile` (`disk.ts:46-51`). `mtime_ms` is Node's `stat.mtimeMs`
/// (`sec * 1e3 + nsec / 1e6`, a double).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedFile {
    pub path: String,
    pub size: u64,
    pub mtime_ms: f64,
    pub closed: bool,
}

/// `BacklogStats` (`disk.ts:38-44`), typed like the payload's
/// [`crate::telemetry::RecordingStatus`] counters. `dropped_segments` is the
/// `counters.json` value as an integer: the agent and the `reset-counters`
/// CLI only ever write integers; a hand-edited fraction would print
/// truncated here (the file itself keeps the exact JS number).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BacklogStats {
    /// Closed segments awaiting upload.
    pub backlog_segments: u64,
    pub backlog_bytes: u64,
    /// Lifetime count of segments evicted under disk pressure.
    pub dropped_segments: u64,
}

/// The scan's file filter (`disk.ts:183`): `counters.json` and
/// `session.json` are skipped at ANY depth (only regular files reach here).
pub fn is_scanned_name(name: &str) -> bool {
    name != COUNTER_FILE && name != SESSION_MANIFEST
}

/// `CLOSED_SUFFIXES.some((s) => e.name.endsWith(s))` (`disk.ts:190`).
pub fn is_closed_name(name: &str) -> bool {
    CLOSED_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// The separator of the `activePaths` entries: a NUL byte (`disk.ts:89,91,
/// 125` hold a literal `\0` — the reason the file reads as binary to grep).
pub const ACTIVE_SEP: char = '\0';

/// `setActivePath` (`disk.ts:87-92`) keeps `"<owner>\0<path>"` strings;
/// `enforce` strips everything up to the first NUL (`disk.ts:124-126`).
/// This is that set's member text.
pub fn active_entry(owner: &str, path: &str) -> String {
    format!("{owner}{ACTIVE_SEP}{path}")
}

/// `p.slice(p.indexOf("\0") + 1)` (`disk.ts:125`).
pub fn active_entry_path(entry: &str) -> &str {
    match entry.find(ACTIVE_SEP) {
        Some(i) => &entry[i + 1..],
        None => entry,
    }
}

/// `p.startsWith(`${owner}\0`)` (`disk.ts:89`): the entries a new
/// registration for `owner` replaces.
pub fn active_entry_owned_by(entry: &str, owner: &str) -> bool {
    entry.starts_with(&format!("{owner}{ACTIVE_SEP}"))
}

/// `files.filter((f) => f.closed && !active.has(f.path)).sort((a, b) =>
/// a.mtimeMs - b.mtimeMs)` (`disk.ts:127-129`): indices into `files`,
/// oldest first. `Array.prototype.sort` is stable, so ties keep scan order
/// (the scan walks directories in byte order of names, like libuv's
/// `scandir`).
pub fn evictable_order(files: &[ScannedFile], active: &HashSet<String>) -> Vec<usize> {
    let mut idx: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.closed && !active.contains(&f.path))
        .map(|(i, _)| i)
        .collect();
    idx.sort_by(|&a, &b| {
        files[a]
            .mtime_ms
            .partial_cmp(&files[b].mtime_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx
}

/// The running totals of one `enforce()` pass (`disk.ts:121-153`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvictionPass {
    /// `total` — bytes of every scanned file (closed or not).
    pub total: f64,
    /// `free` — the free-bytes probe (`null` = unknown: the floor never applies).
    pub free: Option<f64>,
}

impl EvictionPass {
    /// `let total = files.reduce((a, f) => a + f.size, 0); let free = await
    /// this.freeBytesFn();` (`disk.ts:121-122`).
    pub fn new(files: &[ScannedFile], free: Option<f64>) -> Self {
        Self {
            total: files.iter().map(|f| f.size as f64).sum(),
            free,
        }
    }

    /// `overQuota || underFloor` (`disk.ts:133-135`): keep evicting.
    pub fn under_pressure(&self, retention: &ResolvedRetention) -> bool {
        let over_quota = self.total > retention.max_bytes;
        let under_floor = self.free.is_some_and(|f| f < retention.min_free_bytes);
        over_quota || under_floor
    }

    /// A successful `unlinkSync(f.path)` (`disk.ts:142-144`): `total -=
    /// size; if (free !== null) free += size`. (The sidecar removed with an
    /// `.mp4` is NOT credited — it was never a scanned closed file's size.)
    pub fn evicted(&mut self, size: u64) {
        self.total -= size as f64;
        if let Some(f) = self.free.as_mut() {
            *f += size as f64;
        }
    }
}

/// `f.path.replace(/\.mp4$/, ".json")` (`disk.ts:148`): the sidecar evicted
/// with its fragment; `None` for a non-`.mp4` path.
pub fn sidecar_of_mp4(path: &str) -> Option<String> {
    path.strip_suffix(".mp4").map(|stem| format!("{stem}.json"))
}

/// The whole eviction walk assuming every unlink succeeds (the fixture
/// view of `disk.ts:131-153`): the indices evicted, in order.
pub fn plan_eviction(
    files: &[ScannedFile],
    active: &HashSet<String>,
    free: Option<f64>,
    retention: &ResolvedRetention,
) -> Vec<usize> {
    let mut pass = EvictionPass::new(files, free);
    let mut out = Vec::new();
    for i in evictable_order(files, active) {
        if !pass.under_pressure(retention) {
            break;
        }
        pass.evicted(files[i].size);
        out.push(i);
    }
    out
}

/// `this.lastBacklog` (`disk.ts:160-164`): the evictable files that were
/// not deleted — count and bytes.
pub fn backlog_after(
    files: &[ScannedFile],
    evictable: &[usize],
    deleted: &HashSet<usize>,
) -> (u64, u64) {
    let remaining = evictable.iter().filter(|i| !deleted.contains(i));
    remaining.fold((0, 0), |(n, b), &i| (n + 1, b + files[i].size))
}

/// `loadCounters()` (`disk.ts:230-242`) over the file text (`None` = absent
/// or unreadable): `droppedSegments` when it is a JS number `>= 0`, else 0
/// (absent/corrupt → start at 0).
pub fn parse_counters(text: Option<&str>) -> f64 {
    let Some(text) = text else {
        return 0.0;
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(obj)) => match obj.get("droppedSegments") {
            Some(Value::Number(n)) => n.as_f64().filter(|v| *v >= 0.0).unwrap_or(0.0),
            _ => 0.0,
        },
        _ => 0.0,
    }
}

/// `JSON.stringify({ v: 1, droppedSegments: this.dropped })` — the bytes
/// `saveCounters()` hands `atomicWriteJson` (`disk.ts:246`).
pub fn counters_json(dropped: f64) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Counters {
        v: u8,
        dropped_segments: JsNumber,
    }
    serde_json::to_string(&Counters {
        v: 1,
        dropped_segments: JsNumber(dropped),
    })
    .expect("counters serialise")
}

/// `[recording] disk pressure: evicted ${deleted.size} oldest segment(s)` (`disk.ts:155`, `console.warn`).
pub fn evicted_warning(count: usize) -> String {
    format!("[recording] disk pressure: evicted {count} oldest segment(s)")
}

/// `[recording] eviction failed for ${f.path}: ${message}` (`disk.ts:139`, `console.error`).
pub fn eviction_failed_error(path: &str, message: &str) -> String {
    format!("[recording] eviction failed for {path}: {message}")
}

/// `[recording] counter persist failed: ${message}` (`disk.ts:248`, `console.error`).
pub fn counter_persist_failed_error(message: &str) -> String {
    format!("[recording] counter persist failed: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, size: u64, mtime: f64, closed: bool) -> ScannedFile {
        ScannedFile {
            path: path.into(),
            size,
            mtime_ms: mtime,
            closed,
        }
    }

    fn ret(max: f64, min_free: f64) -> ResolvedRetention {
        ResolvedRetention {
            max_bytes: max,
            min_free_bytes: min_free,
        }
    }

    #[test]
    fn names_and_entries() {
        assert!(is_closed_name("seg-1-00000.jsonl.gz"));
        assert!(is_closed_name("00001.mp4"));
        assert!(!is_closed_name("seg-1-00000.jsonl"));
        assert!(!is_closed_name("00001.json"));
        assert!(!is_scanned_name("counters.json"));
        assert!(!is_scanned_name("session.json"));
        assert!(is_scanned_name("x.json"));
        let e = active_entry("video:cam0", "/r/a b.mp4");
        assert_eq!(e, "video:cam0\0/r/a b.mp4");
        assert_eq!(active_entry_path(&e), "/r/a b.mp4");
        assert!(active_entry_owned_by(&e, "video:cam0"));
        assert!(!active_entry_owned_by(&e, "video:cam"));
        assert_eq!(
            sidecar_of_mp4("/v/00001.mp4").as_deref(),
            Some("/v/00001.json")
        );
        assert_eq!(sidecar_of_mp4("/t/seg.jsonl.gz"), None);
    }

    #[test]
    fn oldest_first_until_relieved() {
        let files = vec![
            f("/r/t/s2/c", 1000, 300.0, true),
            f("/r/t/s1/a", 1000, 100.0, true),
            f("/r/t/s1/active.jsonl", 5000, 50.0, false),
            f("/r/t/s1/b", 1000, 200.0, true),
        ];
        let active = HashSet::new();
        assert_eq!(evictable_order(&files, &active), vec![1, 3, 0]);
        // total 8000 > 7000: one eviction (the oldest closed) is enough.
        assert_eq!(
            plan_eviction(&files, &active, None, &ret(7000.0, 0.0)),
            vec![1]
        );
        // Free floor: 500 < 1000 until one eviction credits 1000.
        assert_eq!(
            plan_eviction(&files, &active, Some(500.0), &ret(1e9, 1000.0)),
            vec![1]
        );
        let ev = evictable_order(&files, &active);
        let deleted: HashSet<usize> = [1].into_iter().collect();
        assert_eq!(backlog_after(&files, &ev, &deleted), (2, 2000));
    }

    #[test]
    fn counters_text() {
        assert_eq!(counters_json(0.0), r#"{"v":1,"droppedSegments":0}"#);
        assert_eq!(counters_json(3.0), r#"{"v":1,"droppedSegments":3}"#);
        assert_eq!(counters_json(2.5), r#"{"v":1,"droppedSegments":2.5}"#);
        assert_eq!(parse_counters(None), 0.0);
        assert_eq!(parse_counters(Some("{")), 0.0);
        assert_eq!(parse_counters(Some("null")), 0.0);
        assert_eq!(parse_counters(Some(r#"{"droppedSegments":-1}"#)), 0.0);
        assert_eq!(parse_counters(Some(r#"{"droppedSegments":"3"}"#)), 0.0);
        assert_eq!(parse_counters(Some(r#"{"v":1,"droppedSegments":7}"#)), 7.0);
        assert_eq!(parse_counters(Some(r#"[1]"#)), 0.0);
    }
}
