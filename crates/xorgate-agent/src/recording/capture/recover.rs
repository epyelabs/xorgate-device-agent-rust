//! Startup crash recovery for the video tree — port of
//! `recoverOrphanVideoSessions` (`src/recording/video-segments.ts:341-375`
//! @ 89b5c64).
//!
//! Every session dir not in `active_session_ids` belongs to a previous run:
//! sidecar-backfill all of its fragments (`includeNewest: true`, so the
//! newest is tagged `finalized:false` and 0-byte fragments are dropped).
//! Sessions without a readable `session.json` are left untouched. The
//! manifest is read with the LOOSE TS rule (`read_session_manifest_text`:
//! `v === 1`, string `sessionId`, numeric `anchorTs`), NOT the typed parser,
//! so a manifest the TS accepts with odd or missing fields is still swept
//! and its oddities ride into the sidecars (CF28 / WP6 review S1). Input
//! bytes are decoded lossily (D4), like `readFileSync(…, "utf8")`.
//!
//! The manager (WP1) emits `recording: backfilled N video sidecar(s) from
//! previous run` from the returned count (`manager.ts:304-307`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use xorgate_core::clock::Clock;
use xorgate_core::sidecar::{read_session_manifest_text, ClockPair, SESSION_MANIFEST_NAME};
use xorgate_hal::fragment_stat::monotonic_ms;

use super::sweep::{sweep_session_dir, SweepIo};

/// Options of [`recover_orphan_video_sessions`].
pub struct RecoverOpts<'a> {
    /// `activeSessionIds` (default empty: the manager calls it before any
    /// capture spawns, `manager.ts:302-304`).
    pub active_session_ids: &'a HashSet<String>,
    /// `Date.now()` for each session's sweep clock pair (the mono half is
    /// the boot-frame `monotonicMs()`).
    pub clock: &'a dyn Clock,
}

/// What recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverResult {
    /// The TS return value: sidecars written.
    pub recovered: usize,
    /// The mp4s that got a sidecar (upload/disk cue; tape `fs_out`).
    pub closed: Vec<PathBuf>,
    /// 0-byte fragments dropped.
    pub reaped: Vec<PathBuf>,
}

/// `fs.readdirSync(dir)` names; `None` when it throws.
fn names(dir: &Path) -> Option<Vec<String>> {
    Some(
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
    )
}

/// `statSync(p).isDirectory()` (follows symlinks); a throw is `false`.
fn is_dir(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

/// `recoverOrphanVideoSessions(videoRoot, activeSessionIds)`.
pub fn recover_orphan_video_sessions(video_root: &Path, opts: &RecoverOpts<'_>) -> RecoverResult {
    let mut out = RecoverResult::default();
    let Some(stream_keys) = names(video_root) else {
        return out; // no video tree yet
    };
    for stream_key in stream_keys {
        let stream_dir = video_root.join(&stream_key);
        if !is_dir(&stream_dir) {
            continue;
        }
        let Some(sessions) = names(&stream_dir) else {
            continue;
        };
        for session_id in sessions {
            if opts.active_session_ids.contains(&session_id) {
                continue;
            }
            let dir = stream_dir.join(&session_id);
            if !is_dir(&dir) {
                continue;
            }
            let Some(manifest) = std::fs::read(dir.join(SESSION_MANIFEST_NAME))
                .ok()
                .and_then(|b| read_session_manifest_text(&String::from_utf8_lossy(&b)))
            else {
                continue;
            };
            // Each sweepSessionDir call defaults its own pair (`:263`).
            let res = sweep_session_dir(
                &dir,
                &manifest,
                &SweepIo {
                    include_newest: true,
                    finalized: None,
                    time_source: None,
                    clock_pair: ClockPair {
                        wall_ms: opts.clock.wall_ms() as f64,
                        mono_ms: monotonic_ms() as f64,
                    },
                },
            );
            out.recovered += res.closed.len();
            out.closed.extend(res.closed);
            out.reaped.extend(res.reaped);
        }
    }
    out
}
