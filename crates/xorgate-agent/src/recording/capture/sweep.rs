//! The I/O around the core sweep plan — port of `writeSessionManifest`
//! (`src/recording/video-segments.ts:114-116`) and `sweepSessionDir`
//! (`:235-332`) @ 89b5c64. The DECISION (which fragments close, anchors,
//! `finalized`, 0-byte reaps) is `xorgate_core::sidecar::plan_sweep`; this
//! file reads the directory, stats the fragments, and executes the plan in
//! fragment order: sidecar `atomicWriteJson` (counted as closed only once
//! written), 0-byte unlink + `[recording] dropped empty fragment …` warning,
//! `[recording] sidecar write failed for …: <node err.message>` on failure.
//!
//! `startTs` comes from the fragment's birth time (`xorgate_hal::fragment_stat`,
//! P10) — the ~2.1 s splitmuxsink GOP lag is REPRODUCED, not corrected (memory
//! `video-sidecar-gop-latency`). Phase 7a: [`sweep_session_dir_timed`] with
//! the run's exact timing (`XORGATE_SIDECAR_TIMING=exact`) takes `startTs`
//! from the capture host's stamps instead (core `plan_sweep_timed`); the
//! stats, the listing and the execution order are the same.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tracing::{error, warn};
use xorgate_core::sidecar::{
    dropped_empty_fragment_warning, plan_sweep_recovering, plan_sweep_timed, segment_seq,
    sidecar_path_for, sidecar_write_failed_error, ClockPair, DirEntry, ExactSweep, PtsSample,
    SessionManifest, SweepOpts, SweepPlan, VideoSessionManifest, SESSION_MANIFEST_NAME,
};
use xorgate_core::types::TimeSource;
use xorgate_hal::atomic_write::{atomic_write_bytes_traced, atomic_write_json};
use xorgate_hal::fragment_stat::stat_fragment;

/// `writeSessionManifest` (`video-segments.ts:114-116`): `atomicWriteJson`
/// of `<dir>/session.json` (compact `JSON.stringify` in interface key order).
pub fn write_session_manifest(dir: &Path, manifest: &VideoSessionManifest) -> std::io::Result<()> {
    atomic_write_json(&dir.join(SESSION_MANIFEST_NAME), manifest)
}

/// `sweepSessionDir`'s options (`video-segments.ts:238-244`).
#[derive(Debug, Clone, Copy)]
pub struct SweepIo {
    pub include_newest: bool,
    pub finalized: Option<bool>,
    pub time_source: Option<TimeSource>,
    /// Read ONCE per sweep (`:263`): every sidecar of this pass shares it.
    pub clock_pair: ClockPair,
}

/// What one pass did (`SweepResult`), plus the reaped 0-byte fragments for
/// the tape's `fs_out delete`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    /// Sidecars written this sweep (mp4 paths), in fragment order.
    pub closed: Vec<PathBuf>,
    /// The newest (active) fragment path; `None` when there are no mp4s or
    /// `include_newest`.
    pub active_path: Option<PathBuf>,
    /// 0-byte fragments unlinked (final sweeps only).
    pub reaped: Vec<PathBuf>,
    /// Phase 7a, `exact` only: closed fragments (seq) deferred this pass
    /// (core `SweepPlan::deferred`); always empty in legacy.
    pub deferred: Vec<u64>,
}

/// The directory as a `String` for the core plan (paths the agent builds are UTF-8).
fn dir_str(dir: &Path) -> String {
    dir.to_string_lossy().into_owned()
}

/// `sweepSessionDir(dir, manifest, opts)` (`video-segments.ts:235-332`).
///
/// Stats exactly the fragments the TS stats: those without a sidecar (the
/// closing candidates) and, for fragment 0's boundary anchor, its successor
/// (`:281-288`). A stat that throws leaves the entry without one (`raced an
/// eviction — skip`).
pub fn sweep_session_dir(dir: &Path, manifest: &SessionManifest, opts: &SweepIo) -> SweepOutcome {
    sweep_session_dir_timed(dir, manifest, opts, None)
}

/// [`sweep_session_dir`] with the run's exact timing (phase-7a); `None` is
/// the legacy sweep, byte for byte.
pub fn sweep_session_dir_timed(
    dir: &Path,
    manifest: &SessionManifest,
    opts: &SweepIo,
    exact: Option<&ExactSweep<'_>>,
) -> SweepOutcome {
    sweep_with(dir, opts, |d, entries, o| {
        plan_sweep_timed(d, manifest, entries, o, exact)
    })
}

/// Crash recovery's sweep with the R-WP2R3-2 shift (core
/// `plan_sweep_recovering`): `samples` are the session's `anchor:"pts"`
/// sidecars found on disk; none = [`sweep_session_dir`], byte for byte.
pub fn sweep_session_dir_recovering(
    dir: &Path,
    manifest: &SessionManifest,
    opts: &SweepIo,
    samples: &[PtsSample],
) -> SweepOutcome {
    sweep_with(dir, opts, |d, entries, o| {
        plan_sweep_recovering(d, manifest, entries, o, samples)
    })
}

/// List + stat exactly as the TS does, plan with `plan`, execute in order.
fn sweep_with(
    dir: &Path,
    opts: &SweepIo,
    plan: impl FnOnce(&str, &[DirEntry], &SweepOpts) -> SweepPlan,
) -> SweepOutcome {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return SweepOutcome::default();
    };
    let names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let json: HashSet<&str> = names
        .iter()
        .filter(|n| n.ends_with(".json"))
        .map(String::as_str)
        .collect();
    let mut frags: Vec<(&str, f64)> = names
        .iter()
        .filter_map(|n| segment_seq(n).map(|s| (n.as_str(), s)))
        .collect();
    frags.sort_by(|a, b| a.1.partial_cmp(&b.1).expect("finite seq"));
    let mut need: HashSet<&str> = frags
        .iter()
        .filter(|(n, _)| !json.contains(sidecar_path_for(n).as_str()))
        .map(|(n, _)| *n)
        .collect();
    if let Some(i) = frags
        .iter()
        .position(|(n, s)| *s == 0.0 && need.contains(n))
    {
        if let Some((next, _)) = frags.get(i + 1) {
            need.insert(next);
        }
    }
    let entries: Vec<DirEntry> = names
        .iter()
        .map(|n| DirEntry {
            name: n.clone(),
            stat: if need.contains(n.as_str()) {
                stat_fragment(&dir.join(n)).ok()
            } else {
                None
            },
        })
        .collect();
    let plan = plan(
        &dir_str(dir),
        &entries,
        &SweepOpts {
            include_newest: opts.include_newest,
            finalized: opts.finalized,
            time_source: opts.time_source,
            clock_pair: opts.clock_pair,
        },
    );
    // Execute in fragment order, as the TS loop interleaves unlinks and writes.
    enum Act<'a> {
        Write(&'a xorgate_core::sidecar::SidecarWrite),
        Reap(&'a str),
    }
    let seq_of = |p: &str| {
        let name = p.rsplit('/').next().unwrap_or(p);
        segment_seq(name).unwrap_or(f64::MAX)
    };
    let mut acts: Vec<(f64, Act<'_>)> = plan
        .writes
        .iter()
        .map(|w| (seq_of(&w.mp4_path), Act::Write(w)))
        .chain(plan.reap.iter().map(|r| (seq_of(r), Act::Reap(r.as_str()))))
        .collect();
    acts.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("finite seq"));
    let mut out = SweepOutcome {
        active_path: plan.active_path.as_deref().map(PathBuf::from),
        deferred: plan.deferred.clone(),
        ..SweepOutcome::default()
    };
    for (_, act) in acts {
        match act {
            Act::Reap(p) => {
                if std::fs::remove_file(p).is_ok() {
                    warn!("{}", dropped_empty_fragment_warning(p));
                    out.reaped.push(PathBuf::from(p));
                }
            }
            Act::Write(w) => {
                match atomic_write_bytes_traced(
                    Path::new(&w.sidecar_path),
                    w.sidecar.to_json().as_bytes(),
                ) {
                    Ok(()) => out.closed.push(PathBuf::from(&w.mp4_path)),
                    Err(e) => error!(
                        "{}",
                        sidecar_write_failed_error(&w.mp4_path, &e.node_message())
                    ),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_core::sidecar::{parse_session_manifest_text, SidecarAnchor, VideoSidecar};

    fn manifest(session: &str) -> VideoSessionManifest {
        parse_session_manifest_text(&format!(
            r#"{{"v":1,"deviceId":"dev-1","streamKey":"cam0","sessionId":"{session}","bootId":"b","timeSource":"ntp","preset":"720p15-1mbps","width":1280,"height":720,"fps":15,"bitrateKbps":1000,"segmentSeconds":60,"anchorTs":1760000000000,"anchorMono":"5"}}"#
        ))
        .unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-sweep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn live_then_final_sweep() {
        let d = tmp("lf");
        let m: SessionManifest = manifest("s1").into();
        write_session_manifest(&d, &manifest("s1")).unwrap();
        std::fs::write(d.join("00000.mp4"), vec![1u8; 100]).unwrap();
        std::fs::write(d.join("00001.mp4"), vec![1u8; 50]).unwrap();
        std::fs::write(d.join("00002.mp4"), b"").unwrap();
        let pair = ClockPair {
            wall_ms: 1_760_000_100_000.0,
            mono_ms: 9_000.0,
        };
        let live = sweep_session_dir(
            &d,
            &m,
            &SweepIo {
                include_newest: false,
                finalized: None,
                time_source: Some(TimeSource::Gps),
                clock_pair: pair,
            },
        );
        assert_eq!(live.closed, [d.join("00000.mp4"), d.join("00001.mp4")]);
        assert_eq!(live.active_path, Some(d.join("00002.mp4")));
        let sc: VideoSidecar =
            serde_json::from_slice(&std::fs::read(d.join("00000.json")).unwrap()).unwrap();
        assert_eq!(sc.time_source, TimeSource::Gps);
        assert!(sc.finalized);
        // Fragment 0 is anchored on fragment 1's birth (or fstat/computed on
        // a fs without btime).
        assert!(matches!(
            sc.anchor,
            SidecarAnchor::Boundary | SidecarAnchor::Computed
        ));
        // Idempotent.
        let again = sweep_session_dir(
            &d,
            &m,
            &SweepIo {
                include_newest: false,
                finalized: None,
                time_source: None,
                clock_pair: pair,
            },
        );
        assert!(again.closed.is_empty());
        // Final sweep: the 0-byte newest is reaped, never sidecar'd.
        let fin = sweep_session_dir(
            &d,
            &m,
            &SweepIo {
                include_newest: true,
                finalized: Some(true),
                time_source: None,
                clock_pair: pair,
            },
        );
        assert!(fin.closed.is_empty());
        assert_eq!(fin.reaped, [d.join("00002.mp4")]);
        assert_eq!(fin.active_path, None);
        assert!(!d.join("00002.mp4").exists() && !d.join("00002.json").exists());
        assert!(sweep_session_dir(
            &d.join("nope"),
            &m,
            &SweepIo {
                include_newest: true,
                finalized: None,
                time_source: None,
                clock_pair: pair
            }
        )
        .closed
        .is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
