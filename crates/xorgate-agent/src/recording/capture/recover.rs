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
//! Phase 7a R-WP2R3-2 / R-WP2R4-1 (R7 amended for ONE field): a session
//! that an `exact` run left behind holds `anchor:"pts"` sidecars. The core
//! shifts the recovered `fstat` rows so the first one starts exactly where
//! the pts sidecar before it ends (end anchor), else by the session's
//! median lag (`plan_sweep_recovering_lazy`): the agent-exit / power-loss
//! seam stays contiguous. No `pts` sidecar (legacy mode, untimed runs,
//! every JS-era session) = today's recovery, byte for byte.
//!
//! I/O (R-WP2R4-4, [`recovery_samples`]): nothing extra unless the recovery
//! plan writes a sidecar. Then ONE read: the sidecar of the fragment just
//! before the first recovered one (P). P `pts` → the end anchor needs
//! nothing else. P an unshifted legacy row (`fstat` with `startTs` =
//! birthtime, `boundary`, `computed`) → the session has no exact fragment
//! below it, stop. Only P missing, unparsable, or a SHIFTED `fstat` (a live
//! fallback inside an exact session) costs a scan of the session's
//! sidecars for the median fallback.

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

/// A sidecar file's bytes (`std::fs::read`; injected so tests count reads).
pub type SidecarReader<'a> = dyn FnMut(&Path) -> Option<Vec<u8>> + 'a;

/// One sidecar as recovery reads it: its parsed body (None = unreadable or
/// unparsable) and its mp4's birthtime (`NaN` when the stat fails).
fn read_sidecar(
    dir: &Path,
    json: &str,
    read: &mut SidecarReader<'_>,
) -> (Option<serde_json::Value>, f64) {
    let body = read(&dir.join(json)).and_then(|b| serde_json::from_slice(&b).ok());
    let mp4 = format!("{}.mp4", json.strip_suffix(".json").unwrap_or(json));
    let birth = stat_fragment(&dir.join(mp4)).map_or(f64::NAN, |s| s.birthtime_ms);
    (body, birth)
}

/// A parsed `anchor:"pts"` sidecar as a sample.
fn as_pts_sample(json: &str, body: &serde_json::Value, birthtime_ms: f64) -> Option<PtsSample> {
    let pts = serde_json::to_value(EXACT_ANCHOR).ok();
    if body.get("anchor") != pts.as_ref() {
        return None;
    }
    let seq = segment_seq(&format!("{}.mp4", json.strip_suffix(".json")?))?;
    Some(PtsSample {
        seq,
        start_ts: body.get("startTs")?.as_f64()?,
        duration_ms: body
            .get("durationMs")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::NAN),
        birthtime_ms,
    })
}

/// Every `anchor:"pts"` sidecar of the session (the median fallback's scan).
pub fn pts_samples(dir: &Path, names: &[String]) -> Vec<PtsSample> {
    scan_pts(dir, names, &mut |p: &Path| std::fs::read(p).ok())
}

fn scan_pts(dir: &Path, names: &[String], read: &mut SidecarReader<'_>) -> Vec<PtsSample> {
    names
        .iter()
        .filter(|n| n.ends_with(".json") && segment_seq(&n.replace(".json", ".mp4")).is_some())
        .filter_map(|n| {
            let (body, birth) = read_sidecar(dir, n, read);
            as_pts_sample(n, &body?, birth)
        })
        .collect()
}

/// R-WP2R4-4: the samples the core needs for first recovered seq `k`,
/// reading as little as possible (see the module doc): P = `k − 1`'s
/// sidecar first; a scan only when P is missing, unparsable, or a shifted
/// `fstat` row.
pub fn recovery_samples(
    dir: &Path,
    names: &[String],
    k: f64,
    read: &mut SidecarReader<'_>,
) -> Vec<PtsSample> {
    if k < 1.0 {
        return Vec::new(); // fragment 0: nothing below it
    }
    let p = format!("{:05}.json", (k - 1.0) as u64);
    if !names.contains(&p) {
        return scan_pts(dir, names, read);
    }
    let (body, birth) = read_sidecar(dir, &p, read);
    let Some(body) = body else {
        return scan_pts(dir, names, read);
    };
    if let Some(sample) = as_pts_sample(&p, &body, birth) {
        if sample.duration_ms.is_finite() {
            return vec![sample];
        }
        return scan_pts(dir, names, read); // P's numbers unusable: median fallback
    }
    // P is a legacy row. Shifted (fstat with startTs < birthtime) means a
    // live fallback inside an exact session: scan. Otherwise no exact
    // fragment exists below it in this session.
    let shifted = body.get("anchor").and_then(|a| a.as_str()) == Some("fstat")
        && body
            .get("startTs")
            .and_then(|v| v.as_f64())
            .is_some_and(|st| birth.is_finite() && st < xorgate_core::round::js_math_round(birth));
    if shifted {
        scan_pts(dir, names, read)
    } else {
        Vec::new()
    }
}

/// `recoverOrphanVideoSessions(videoRoot, activeSessionIds)`.
pub fn recover_orphan_video_sessions(video_root: &Path, opts: &RecoverOpts<'_>) -> RecoverResult {
    recover_orphan_video_sessions_with(video_root, opts, &mut |p: &Path| std::fs::read(p).ok())
}

/// [`recover_orphan_video_sessions`] with the sidecar reader injected
/// (tests count the reads, R-WP2R4-4).
pub fn recover_orphan_video_sessions_with(
    video_root: &Path,
    opts: &RecoverOpts<'_>,
    read: &mut SidecarReader<'_>,
) -> RecoverResult {
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
            // R-WP2R3-2 / R-WP2R4-4: the pts sidecars an exact run left, read
            // only when recovery writes something (none = today's sweep).
            let samples = |k: f64| {
                names(&dir)
                    .map(|n| recovery_samples(&dir, &n, k, &mut *read))
                    .unwrap_or_default()
            };
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
                samples,
            );
            out.recovered += res.closed.len();
            out.closed.extend(res.closed);
            out.reaped.extend(res.reaped);
        }
    }
    out
}
