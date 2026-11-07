//! The `/dev/shm` reaper of the capture supervisor — port of
//! `reapShmSegments` (`src/recording/video-capture.ts:873-891` @ 89b5c64).
//!
//! shmsink names its POSIX shared-memory areas `shmpipe.<pid>.<n>` and
//! unlinks them only in its OWN cleanup, which a SIGKILL skips: without this
//! a camera that stalls repeatedly strands 20 MB (`shm-size`) per kill until
//! reboot (memory `1080p30-stall-bug`, bench 2026-08-06: 104 orphans, 286 MB).
//! Called once a child's exit has been OBSERVED (its pid can no longer be
//! live), matched on that child's own pid only — never another process's
//! segments (the JS agent's live on the same tmpfs). shmsink space-pads the
//! pid (`shmpipe. 1035.    0`), which the TS prefix match misses; Rust
//! compares the trimmed pid (D41, `capture_ladder::shm_owner_pid`), and adds
//! a start-up sweep of dead owners' segments ([`sweep_dead_owner_segments`]).

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

/// Is `pid` a live process (`kill(pid, 0)`: success or EPERM = it exists)?
/// pid 0 and pids past `i32::MAX` are never "dead" (kill would mean a group).
pub fn pid_alive(pid: u32) -> bool {
    pid_alive_with(pid, |p| nix::sys::signal::kill(p, None))
}

/// [`pid_alive`] over an injectable `kill(pid, 0)` probe (the test seam that
/// pins the EPERM branch, review-wp5 R-WP5-6: the suites run as root, where
/// EPERM never happens; production is `User=root` too, so the branch is
/// defensive — a live process of another user must never look dead).
fn pid_alive_with(pid: u32, probe: impl Fn(nix::unistd::Pid) -> nix::Result<()>) -> bool {
    use nix::errno::Errno;
    let Ok(raw) = i32::try_from(pid) else {
        return true;
    };
    if raw == 0 {
        return true;
    }
    match probe(nix::unistd::Pid::from_raw(raw)) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// The start-up sweep (accepted divergence D41; the TS has none, P7): unlink
/// every shmsink area of `dir` whose OWNER pid is not a live process — the
/// segments a killed predecessor's children (an agent SIGKILL, the OOM
/// killer, a systemd stop that killed gst first) left behind. Keyed on
/// "owner is dead", never on "owner is ours": a live owner's segments (the JS
/// agent's on a shared bench, a sibling agent in the rig) are never touched,
/// and pid reuse can only make it SKIP a stale segment. Returns
/// `(name, owner pid)` per segment actually removed.
pub fn sweep_dead_owner_segments(dir: &Path, alive: &dyn Fn(u32) -> bool) -> Vec<(String, u32)> {
    let Some(mut names) = list(dir) else {
        return Vec::new();
    };
    names.sort();
    names
        .into_iter()
        .filter_map(|n| {
            let pid = xorgate_core::capture_ladder::shm_owner_pid(&n)?;
            (!alive(pid) && std::fs::remove_file(dir.join(&n)).is_ok()).then_some((n, pid))
        })
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

#[allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: test harness spawns, not the agent
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

    /// D41: the exit reaper finds shmsink's REAL (space-padded) names.
    #[test]
    fn reaps_a_padded_four_digit_pid() {
        let dir = std::env::temp_dir().join(format!("xg-shm-pad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "shmpipe. 4242.    0",
            "shmpipe. 4242.    1",
            "shmpipe.14242.    0",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let mut reaped = reap_shm_segments(&dir, Some(4242));
        reaped.sort();
        assert_eq!(reaped, ["shmpipe. 4242.    0", "shmpipe. 4242.    1"]);
        assert_eq!(shm_pipe_names(&dir), ["shmpipe.14242.    0"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// R-WP5-6: the probe's answer decides: Ok and EPERM (alive, another
    /// user's) are alive; ESRCH (and any other error) is dead; pid 0 and
    /// out-of-range pids never reach the probe.
    #[test]
    fn pid_alive_treats_eperm_as_alive() {
        use nix::errno::Errno;
        assert!(pid_alive_with(4242, |_| Ok(())));
        assert!(pid_alive_with(4242, |_| Err(Errno::EPERM)));
        assert!(!pid_alive_with(4242, |_| Err(Errno::ESRCH)));
        let never = |_| -> nix::Result<()> { panic!("probed") };
        assert!(pid_alive_with(0, never));
        assert!(pid_alive_with(u32::MAX, never));
    }

    /// D41 start-up sweep: only segments whose owner pid is DEAD go; a live
    /// owner's (ours, or another agent's) and non-shmsink names stay.
    #[test]
    fn startup_sweep_removes_only_dead_owners_segments() {
        let dir = std::env::temp_dir().join(format!("xg-shm-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let me = std::process::id();
        // A child that has exited and been reaped: its pid is dead.
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let dead = c.id();
        c.wait().unwrap();
        assert!(pid_alive(me) && !pid_alive(dead));
        let live = format!("shmpipe.{me:>5}.    0");
        let gone0 = format!("shmpipe.{dead:>5}.    0");
        let gone1 = format!("shmpipe.{dead:>5}.    1");
        for n in [
            live.as_str(),
            gone0.as_str(),
            gone1.as_str(),
            "shmpipe.x.    0",
            "lttng-ust-wait-8",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let removed = sweep_dead_owner_segments(&dir, &pid_alive);
        assert_eq!(removed, [(gone0.clone(), dead), (gone1.clone(), dead)]);
        let mut left = list(&dir).unwrap();
        left.sort();
        let mut want = vec![
            live.clone(),
            "lttng-ust-wait-8".to_string(),
            "shmpipe.x.    0".to_string(),
        ];
        want.sort();
        assert_eq!(left, want);
        // Every owner alive → nothing; an unreadable dir → nothing.
        assert!(sweep_dead_owner_segments(&dir, &|_| true).is_empty());
        assert!(sweep_dead_owner_segments(&dir.join("absent"), &|_| false).is_empty());
        // pid 0 / out of range are never "dead".
        assert!(pid_alive(0) && pid_alive(u32::MAX));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
