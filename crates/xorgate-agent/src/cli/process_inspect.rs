//! PID-reuse-resistant identity of the agent named by `agent.pid` — port of
//! `src/daemon/process.ts:16-166` minus `spawnDetached` (the self-managed
//! `xorgate start` daemon is dropped, README §3.3).
//!
//! A bare `kill(pid, 0)` only proves *some* process owns the pid; the command
//! line from `ps -o etime=,command=` (or, without it, the elapsed time vs the
//! pidfile's `startedAt`) corroborates that it is the worker the record
//! describes. `ps` is found through `PATH` like the TS `execa("ps")`; the
//! runner is injectable for tests.
//!
//! Both agents classify as `verified`: the JS pidfile's `worker` is
//! `/opt/xorgate/dist/worker.js` (in the `node …/worker.js` command line), the
//! Rust one's is the binary path (`/opt/xorgate-rs/bin/xorgate-rs run`) — C10.

use std::process::Stdio;

use crate::pidfile::ParsedPidRecord;

/// `ProcRunner`'s result (`process.ts:16-19`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcRun {
    pub stdout: String,
    pub failed: bool,
    pub not_found: bool,
}

/// `ProcRunner` (`process.ts:16`): run a command to completion, never throw.
pub trait ProcRunner {
    fn run(&self, cmd: &str, args: &[&str]) -> ProcRun;
}

/// The default runner (`process.ts:21-22` → `lib/exec.ts run`): `failed` on a
/// spawn error or a non-zero exit, `not_found` when the binary is missing.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl ProcRunner for SystemRunner {
    fn run(&self, cmd: &str, args: &[&str]) -> ProcRun {
        match xorgate_hal::child::std_command(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(out) => ProcRun {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                failed: !out.status.success(),
                not_found: false,
            },
            Err(e) => ProcRun {
                stdout: String::new(),
                failed: true,
                not_found: e.kind() == std::io::ErrorKind::NotFound,
            },
        }
    }
}

/// `IDENTITY_TOLERANCE_SEC` (`process.ts:25`).
pub const IDENTITY_TOLERANCE_SEC: f64 = 5.0;
/// `CLOCK_STEP_GUARD_SEC` (`process.ts:34`).
pub const CLOCK_STEP_GUARD_SEC: f64 = 3600.0;

/// `ProcInfo` (`process.ts:36-43`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProcInfo {
    pub exists: bool,
    pub elapsed_sec: Option<f64>,
    pub command: Option<String>,
}

/// `Identity` (`process.ts:45`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    Verified,
    Reused,
    Unverified,
}

/// `signal(pid, 0)`'s classification (`process.ts:48-58`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signalled {
    Sent,
    Gone,
    Eperm,
}

/// `signal(pid, 0)` (`process.ts:48-58`). A pid beyond `i32::MAX` cannot name
/// a process (and `kill(2)` would read it as a process group): `Gone`. Errors
/// other than ESRCH/EPERM cannot occur for signal 0 on a positive pid; they
/// read as `Gone` instead of the TS throw.
pub fn signal0(pid: u32) -> Signalled {
    let Ok(raw) = i32::try_from(pid) else {
        return Signalled::Gone;
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None) {
        Ok(()) => Signalled::Sent,
        Err(nix::errno::Errno::EPERM) => Signalled::Eperm,
        Err(_) => Signalled::Gone,
    }
}

/// `isAlive` (`process.ts:61-64`): alive regardless of whose it is.
pub fn is_alive(pid: u32) -> bool {
    matches!(signal0(pid), Signalled::Sent | Signalled::Eperm)
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `parseEtimeSeconds` (`process.ts:86-109`): `[[DD-]HH:]MM:SS` → seconds.
pub fn parse_etime_seconds(raw: &str) -> Option<f64> {
    let s = xorgate_core::js::trim(raw);
    if s.is_empty() {
        return None;
    }
    let mut days = 0.0;
    let mut rest = s;
    if let Some(dash) = s.find('-') {
        let d = &s[..dash];
        if !all_digits(d) {
            return None;
        }
        days = xorgate_core::system::js_number(d);
        rest = &s[dash + 1..];
    }
    let tokens: Vec<&str> = rest.split(':').collect();
    if !tokens.iter().all(|t| all_digits(t)) {
        return None;
    }
    let parts: Vec<f64> = tokens
        .iter()
        .map(|t| xorgate_core::system::js_number(t))
        .collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => (0.0, *m, *s),
        _ => return None,
    };
    Some(days * 86400.0 + h * 3600.0 + m * 60.0 + sec)
}

/// `inspectProcess` (`process.ts:112-128`): `ps -o etime=,command= -p <pid>`.
pub fn inspect_process(pid: u32, runner: &dyn ProcRunner) -> ProcInfo {
    let pid_s = pid.to_string();
    let r = runner.run("ps", &["-o", "etime=,command=", "-p", &pid_s]);
    let none = ProcInfo {
        exists: false,
        elapsed_sec: None,
        command: None,
    };
    if r.not_found || r.failed {
        return none;
    }
    let line = xorgate_core::js::trim(&r.stdout);
    if line.is_empty() {
        return none;
    }
    let (etime_tok, command) = match line.find(' ') {
        None => (line, None),
        Some(sp) => (
            &line[..sp],
            Some(xorgate_core::js::trim(&line[sp + 1..]).to_string()),
        ),
    };
    ProcInfo {
        exists: true,
        elapsed_sec: parse_etime_seconds(etime_tok),
        command,
    }
}

/// `looksLikeWorker` (`process.ts:131-136`).
pub fn looks_like_worker(command: &str, worker: &str) -> bool {
    if !worker.is_empty() && command.contains(worker) {
        return true;
    }
    let base = worker.rsplit('/').next().unwrap_or(worker);
    if !base.is_empty() && command.contains(base) {
        return true;
    }
    command.contains("worker")
}

/// `classifyIdentity` (`process.ts:148-166`); `tolerance_sec` defaults to
/// [`IDENTITY_TOLERANCE_SEC`].
pub fn classify_identity(
    record: &ParsedPidRecord,
    info: &ProcInfo,
    now: f64,
    tolerance_sec: f64,
) -> Identity {
    // `if (info.command)` — an empty command line is falsy.
    if let Some(cmd) = info.command.as_deref().filter(|c| !c.is_empty()) {
        return if looks_like_worker(cmd, &record.worker) {
            Identity::Verified
        } else {
            Identity::Reused
        };
    }
    let Some(elapsed) = info.elapsed_sec else {
        return Identity::Unverified;
    };
    let expected_age_sec = (now - record.started_at) / 1000.0;
    if elapsed + tolerance_sec < expected_age_sec {
        if expected_age_sec - elapsed > CLOCK_STEP_GUARD_SEC {
            return Identity::Unverified;
        }
        return Identity::Reused;
    }
    Identity::Verified
}

#[allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: test harness spawns, not the agent
#[cfg(test)]
mod tests {
    //! `test/process.test.ts` (11 it()s).
    use super::*;

    fn record(started_at: f64, worker: &str) -> ParsedPidRecord {
        ParsedPidRecord {
            pid: 1,
            started_at,
            device_id: "d".into(),
            mode: "sim".into(),
            rate_hz: 1.0,
            offline: true,
            worker: worker.into(),
            argv: vec![],
            node_version: "v20".into(),
        }
    }

    fn info(elapsed: Option<f64>, command: Option<&str>) -> ProcInfo {
        ProcInfo {
            exists: true,
            elapsed_sec: elapsed,
            command: command.map(str::to_string),
        }
    }

    /// A pid that just exited and was reaped (`helpers.ts deadPid`).
    pub(crate) fn dead_pid() -> u32 {
        let mut c = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    #[test]
    fn parses_mm_ss_hh_mm_ss_dd_hh_mm_ss() {
        assert_eq!(parse_etime_seconds("00:42"), Some(42.0));
        assert_eq!(parse_etime_seconds("   00:00"), Some(0.0));
        assert_eq!(parse_etime_seconds("01:02:03"), Some(3723.0));
        assert_eq!(
            parse_etime_seconds("2-03:04:05"),
            Some((2 * 86400 + 3 * 3600 + 4 * 60 + 5) as f64)
        );
    }

    #[test]
    fn returns_null_on_junk_and_empty_segments() {
        for s in ["", "nope", "::", ":30", "-3:04:05"] {
            assert_eq!(parse_etime_seconds(s), None, "{s:?}");
        }
    }

    #[test]
    fn reports_the_test_process_alive() {
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn reports_a_reaped_pid_dead() {
        let pid = dead_pid();
        assert_eq!(signal0(pid), Signalled::Gone);
        assert!(!is_alive(pid));
    }

    const NOW: f64 = 1_000_000.0;

    #[test]
    fn verifies_when_the_command_line_references_our_worker_regardless_of_age() {
        let rec = record(NOW - 100_000.0, "/x/dist/worker.js");
        assert_eq!(
            classify_identity(
                &rec,
                &info(Some(1.0), Some("node /x/dist/worker.js --offline")),
                NOW,
                IDENTITY_TOLERANCE_SEC
            ),
            Identity::Verified
        );
    }

    #[test]
    fn flags_reuse_when_the_command_line_is_some_other_program() {
        let rec = record(NOW - 5_000.0, "/x/dist/worker.js");
        assert_eq!(
            classify_identity(
                &rec,
                &info(Some(9999.0), Some("vim notes.txt")),
                NOW,
                IDENTITY_TOLERANCE_SEC
            ),
            Identity::Reused
        );
    }

    #[test]
    fn falls_back_to_the_age_heuristic_when_ps_gave_no_command() {
        let rec = record(NOW - 10_000.0, "/x/worker.js");
        assert_eq!(
            classify_identity(&rec, &info(Some(6.0), None), NOW, 5.0),
            Identity::Verified
        );
        assert_eq!(
            classify_identity(&rec, &info(Some(4.0), None), NOW, 5.0),
            Identity::Reused
        );
    }

    #[test]
    fn is_unverified_when_ps_gave_neither_command_nor_elapsed_time() {
        assert_eq!(
            classify_identity(
                &record(0.0, "/x/worker.js"),
                &info(None, None),
                NOW,
                IDENTITY_TOLERANCE_SEC
            ),
            Identity::Unverified
        );
    }

    #[test]
    fn treats_an_implausibly_large_age_gap_as_unverified_not_reused() {
        let rec = record(NOW - 7_200_000.0, "/x/worker.js");
        assert_eq!(
            classify_identity(&rec, &info(Some(30.0), None), NOW, 5.0),
            Identity::Unverified
        );
    }

    struct Fixed(ProcRun);
    impl ProcRunner for Fixed {
        fn run(&self, cmd: &str, args: &[&str]) -> ProcRun {
            assert_eq!(cmd, "ps");
            assert_eq!(args, ["-o", "etime=,command=", "-p", "1234"]);
            self.0.clone()
        }
    }

    #[test]
    fn inspect_process_parses_etime_and_command() {
        let r = Fixed(ProcRun {
            stdout: "   00:05 node /x/worker.js\n".into(),
            failed: false,
            not_found: false,
        });
        assert_eq!(
            inspect_process(1234, &r),
            ProcInfo {
                exists: true,
                elapsed_sec: Some(5.0),
                command: Some("node /x/worker.js".into())
            }
        );
    }

    #[test]
    fn inspect_process_degrades_to_unverified_when_ps_is_missing() {
        let r = Fixed(ProcRun {
            stdout: String::new(),
            failed: true,
            not_found: true,
        });
        let i = inspect_process(1234, &r);
        assert!(!i.exists);
        assert_eq!(i.elapsed_sec, None);
    }

    /// C10 both directions: each agent's real pidfile `worker` vs the
    /// other's-style command line seen by `ps`.
    #[test]
    fn both_agents_classify_verified() {
        let js = record(NOW, "/opt/xorgate/dist/worker.js");
        let rs = record(NOW, "/opt/xorgate-rs/bin/xorgate-rs");
        let js_cmd = info(Some(10.0), Some("node /opt/xorgate/dist/worker.js"));
        let rs_cmd = info(Some(10.0), Some("/opt/xorgate-rs/bin/xorgate-rs run"));
        assert_eq!(
            classify_identity(&js, &js_cmd, NOW, 5.0),
            Identity::Verified
        );
        assert_eq!(
            classify_identity(&rs, &rs_cmd, NOW, 5.0),
            Identity::Verified
        );
        // a recycled pid running something else is still caught
        assert_eq!(
            classify_identity(&rs, &info(Some(10.0), Some("sshd: pi")), NOW, 5.0),
            Identity::Reused
        );
    }
}
