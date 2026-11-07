//! The `statSync` fields the video sweep reads (`src/recording/video-segments.ts`
//! `:273,:284,:311`, `video-capture.ts:969`) — size, birth time and mtime —
//! with Node's exact arithmetic, plus the boot-frame monotonic clock that
//! `process.hrtime` reads.
//!
//! Birth time (phase-3 P10): Node's `fs.statSync` goes through libuv's
//! `statx(2)` and fills `birthtime` from `stx_btime` WHETHER OR NOT the
//! filesystem set `STATX_BTIME` in `stx_mask` (an unsupported btime reads as
//! the zeroed field → `birthtimeMs: 0`, which `buildSidecar` treats as "no
//! birthtime" → `anchor:"computed"`). Rust's `Metadata::created()` is the same
//! `statx` btime but errors when the mask lacks it; that error maps to `0.0`,
//! so both agents take the same branch. The value is
//! `sec * 1e3 + nsec / 1e6` in `f64`, Node's `msFromTimeSpec`
//! (`lib/internal/fs/utils.js`), so `Math.round(birthtimeMs)` lands on the
//! same integer. This is what REPRODUCES the ~2.1 s sidecar GOP lag (memory
//! `video-sidecar-gop-latency`): splitmuxsink creates each file one GOP
//! after its first frame and the sidecar trusts the file's birth.
//!
//! (libuv's pre-statx fallback copies `ctime` into `birthtime`; every kernel
//! the agent runs on has statx, so that branch is not reproduced.)

use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use xorgate_core::sidecar::FragmentStat;

/// Node's `msFromTimeSpec(sec, nsec)`: `sec * 1e3 + nsec / 1e6`. Times before
/// the epoch are negative like Node's.
pub fn ms_from_system_time(t: SystemTime) -> f64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as f64 * 1e3 + f64::from(d.subsec_nanos()) / 1e6,
        Err(e) => {
            // `t` precedes the epoch: Node keeps (negative sec, positive nsec).
            let d = e.duration();
            let (sec, nsec) = if d.subsec_nanos() == 0 {
                (-(d.as_secs() as f64), 0.0)
            } else {
                (
                    -(d.as_secs() as f64) - 1.0,
                    f64::from(1_000_000_000 - d.subsec_nanos()),
                )
            };
            sec * 1e3 + nsec / 1e6
        }
    }
}

/// `statSync(path)` → `{sizeBytes, birthtimeMs, mtimeMs}` (follows symlinks
/// like `statSync`). A missing birth time is `0.0` (see the module doc).
pub fn stat_fragment(path: &Path) -> io::Result<FragmentStat> {
    let md = std::fs::metadata(path)?;
    Ok(FragmentStat {
        size_bytes: md.len() as f64,
        birthtime_ms: md.created().map(ms_from_system_time).unwrap_or(0.0),
        mtime_ms: md.modified().map(ms_from_system_time).unwrap_or(0.0),
    })
}

/// `statSync(path).size`, `0` when the stat throws (`video-capture.ts:968-972`).
pub fn size_or_zero(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// `process.hrtime.bigint()`: CLOCK_MONOTONIC in nanoseconds — the frame the
/// session's `anchorMono` and every sidecar's `startMono` live in
/// (`session.json.bootId` scopes it, so it must be the kernel's boot-relative
/// clock, not an agent-relative one).
#[allow(unsafe_code)]
pub fn hrtime_ns() -> u128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec we own.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128
}

/// `monotonicMs()` (`boot-policy.ts:60-62`): `hrtime.bigint() / 1_000_000n`.
pub fn monotonic_ms() -> u64 {
    (hrtime_ns() / 1_000_000) as u64
}

#[allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: test harness spawns, not the agent
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn node_ms_arithmetic() {
        let t = UNIX_EPOCH + Duration::new(1_760_000_000, 123_456_789);
        assert_eq!(
            ms_from_system_time(t),
            1_760_000_000.0 * 1e3 + 123_456_789.0 / 1e6
        );
        let before = UNIX_EPOCH - Duration::new(1, 500_000_000);
        assert_eq!(ms_from_system_time(before), -2000.0 + 500.0);
    }

    /// P10: the birth time we read equals what coreutils' `stat --format=%W`
    /// (statx btime, seconds; `0` when the fs has none) reports for the same
    /// file on the CI container's filesystem, and the fallback is `0.0`.
    #[test]
    fn birth_time_matches_stat_w() {
        let dir = std::env::temp_dir().join(format!("xg-fragstat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("00000.mp4");
        std::fs::write(&p, b"abc").unwrap();
        let st = stat_fragment(&p).unwrap();
        assert_eq!(st.size_bytes, 3.0);
        assert!(st.mtime_ms > 1_000_000_000_000.0);
        let out = std::process::Command::new("stat")
            .args(["--format=%W %.9W"])
            .arg(&p)
            .output();
        if let Some(out) = out.ok().filter(|o| o.status.success()) {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut it = text.split_whitespace();
            let secs: i64 = it.next().unwrap().parse().unwrap();
            if secs == 0 {
                assert_eq!(
                    st.birthtime_ms, 0.0,
                    "no btime on this fs → 0 (computed anchor)"
                );
            } else {
                assert_eq!((st.birthtime_ms / 1000.0).floor() as i64, secs);
                // `%.9W` = seconds with nanoseconds (coreutils ≥ 8.32).
                if let Some(frac) = it.next().and_then(|f| f.parse::<f64>().ok()) {
                    assert!((st.birthtime_ms - frac * 1000.0).abs() < 0.001);
                }
            }
        }
        assert_eq!(size_or_zero(&dir.join("nope.mp4")), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hrtime_is_monotonic() {
        let a = hrtime_ns();
        let b = hrtime_ns();
        assert!(a > 0 && b >= a);
        assert!(monotonic_ms() >= (a / 1_000_000) as u64);
    }
}
