//! The `/dev/shm` reaper of the capture supervisor — port of
//! `reapShmSegments` (`src/recording/video-capture.ts:873-891` @ 89b5c64).
//!
//! shmsink names its POSIX shared-memory areas `shmpipe.<pid>.<n>` and
//! unlinks them only in its OWN cleanup, which a SIGKILL skips: without this
//! a camera that stalls repeatedly strands 20 MB (`shm-size`) per kill until
//! reboot (memory `1080p30-stall-bug`, bench 2026-08-06: 104 orphans, 286 MB).
//! Called once a child's exit has been OBSERVED (its pid can no longer be
//! live), matched on that child's own pid prefix only — never another
//! process's segments (the JS agent's live on the same tmpfs).

use std::path::Path;

use xorgate_core::capture_ladder::shm_names_to_reap;

/// Every entry name of `dir` (`readdirSync`), or `None` when it is unreadable
/// (no shm tmpfs: a dev box or a restricted container — nothing to do).
fn list(dir: &Path) -> Option<Vec<String>> {
    let rd = std::fs::read_dir(dir).ok()?;
    Some(
        rd.filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
    )
}

/// Unlink `<dir>/shmpipe.<pid>.*` for a child that has EXITED; returns the
/// names actually removed. `None` pid (spawn failed) → nothing. Unlink
/// errors are ignored (raced another reaper — best effort, like the TS).
pub fn reap_shm_segments(dir: &Path, pid: Option<u32>) -> Vec<String> {
    if pid.is_none() {
        return Vec::new();
    }
    let Some(names) = list(dir) else {
        return Vec::new();
    };
    shm_names_to_reap(&names, pid)
        .into_iter()
        .filter(|n| std::fs::remove_file(dir.join(n)).is_ok())
        .map(str::to_string)
        .collect()
}

/// The `shmpipe.*` names currently in `dir`, sorted (the tape's `shm` record
/// and the bench's orphan audit). Empty when the dir is unreadable.
pub fn shm_pipe_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = list(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.starts_with("shmpipe."))
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `video-capture.test.ts` "reaps the killed child's orphaned shmsink
    /// segments, and only its own" — the HAL half (the supervisor half is in
    /// the agent's capture tests).
    #[test]
    fn reaps_only_the_dead_childs_own_segments() {
        let dir = std::env::temp_dir().join(format!("xg-shm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "shmpipe.4242.    0",
            "shmpipe.4242.    1",
            "shmpipe.9999.    0",
            "lttng-ust-wait-8",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        assert!(reap_shm_segments(&dir, None).is_empty());
        let mut reaped = reap_shm_segments(&dir, Some(4242));
        reaped.sort();
        assert_eq!(reaped, ["shmpipe.4242.    0", "shmpipe.4242.    1"]);
        assert_eq!(shm_pipe_names(&dir), ["shmpipe.9999.    0"]);
        let mut left: Vec<String> = list(&dir).unwrap();
        left.sort();
        assert_eq!(left, ["lttng-ust-wait-8", "shmpipe.9999.    0"]);
        assert!(reap_shm_segments(&dir.join("absent"), Some(1)).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
