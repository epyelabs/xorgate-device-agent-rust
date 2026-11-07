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
//!
//! Phase 7a R-WP2R3-2 (R7 amended for ONE field): a session that an
//! `exact` run left behind holds `anchor:"pts"` sidecars. Their `startTs`
//! and their mp4's birthtime are read here ([`pts_samples`]) and the core
//! shifts the recovered `fstat` rows after them earlier by the session's
//! measured lag (`plan_sweep_recovering`), so the agent-exit / power-loss
//! seam stays contiguous. No `pts` sidecar (legacy mode, untimed runs,
//! every JS-era session) = today's recovery, byte for byte; the extra I/O is
//! reading the session's existing sidecars.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use xorgate_core::clock::Clock;
use xorgate_core::sidecar::{
    read_session_manifest_text, segment_seq, ClockPair, PtsSample, EXACT_ANCHOR,
    SESSION_MANIFEST_NAME,
};
use xorgate_hal::fragment_stat::{monotonic_ms, stat_fragment};

use super::sweep::{sweep_session_dir_recovering, SweepIo};

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

/// The session's `anchor:"pts"` sidecars as recovery samples: the seq from
/// the `NNNNN.json` name, `startTs` from its body, the birthtime of its
/// `NNNNN.mp4`. Unreadable, unparsable, non-`pts` sidecars and a missing
/// mp4 contribute nothing (the core also skips a bogus birthtime).
pub fn pts_samples(dir: &Path, names: &[String]) -> Vec<PtsSample> {
    let pts = serde_json::to_value(EXACT_ANCHOR).ok();
    names
        .iter()
        .filter_map(|n| {
            let stem = n.strip_suffix(".json")?;
            let mp4 = format!("{stem}.mp4");
            let seq = segment_seq(&mp4)?;
            let body: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.join(n)).ok()?).ok()?;
            if body.get("anchor") != pts.as_ref() {
                return None;
            }
            let start_ts = body.get("startTs")?.as_f64()?;
            let birthtime_ms = stat_fragment(&dir.join(&mp4)).ok()?.birthtime_ms;
            Some(PtsSample {
                seq,
                start_ts,
                birthtime_ms,
            })
        })
        .collect()
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
            // R-WP2R3-2: the pts sidecars an exact run left (none = today's sweep).
            let samples = names(&dir)
                .map(|n| pts_samples(&dir, &n))
                .unwrap_or_default();
            // Each sweepSessionDir call defaults its own pair (`:263`).
            let res = sweep_session_dir_recovering(
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
                &samples,
            );
            out.recovered += res.closed.len();
            out.closed.extend(res.closed);
            out.reaped.extend(res.reaped);
        }
    }
    out
}
