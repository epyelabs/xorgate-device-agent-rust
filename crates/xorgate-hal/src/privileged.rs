//! The privileged edge of the command plane (Phase 2 WP4): `executeCommand`'s
//! `run("sudo", […])` (`ts:src/agent/commands.ts:229-257` over
//! `ts:src/lib/exec.ts:22-47`).
//!
//! **Invariant 4** (agent-anatomy §10): the only argv this module can run is
//! a [`PrivilegedArgv`], and the only way to build one is from a core
//! [`PrivilegedCall`] — which only `xorgate_core::commands::plan` mints for
//! a cloud command. Tests never run sudo: they inject a [`RecordingRunner`].
//!
//! **O3** (`phase-2-cloud-plane.md`): the Rust agent's `update` starts
//! `xorgate-updater-rs.service`, never the JS agent's updater. The core plan
//! stays TS-exact (`xorgate-updater.service`, its fixture); the substitution
//! happens here, in [`PrivilegedArgv::for_rust_agent`]. Until Phase 4
//! installs that unit and its exact-args sudoers grant, `sudo -n` refuses it
//! and the ack is `failed` with sudo's stderr.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use xorgate_core::commands::{
    in_process_run_result, plan, CommandPlan, DeviceCommandName, PrivilegedCall, RunResult,
};

/// The JS agent's updater unit (`commands.ts:252`), as the core plan names it.
pub const JS_UPDATER_UNIT: &str = "xorgate-updater.service";
/// O3: the Rust agent's updater unit (README §6; installed by Phase 4).
pub const RS_UPDATER_UNIT: &str = "xorgate-updater-rs.service";

/// A boxed `Send` future (object-safe trait method without a crate).
pub type RunFut<'a> = Pin<Box<dyn Future<Output = RunResult> + Send + 'a>>;

/// The exact argv handed to the OS. Private field: built only from a core
/// [`PrivilegedCall`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedArgv {
    argv: Vec<&'static str>,
}

impl PrivilegedArgv {
    /// The TS argv verbatim (`commands.ts:252, 256`).
    pub fn ts_exact(call: &PrivilegedCall) -> Self {
        Self { argv: call.argv() }
    }

    /// What the Rust agent runs: the TS argv with the updater unit swapped
    /// for [`RS_UPDATER_UNIT`] (O3). `reboot`/`poweroff` are unchanged.
    pub fn for_rust_agent(call: &PrivilegedCall) -> Self {
        let argv = call
            .argv()
            .into_iter()
            .map(|w| if w == JS_UPDATER_UNIT { RS_UPDATER_UNIT } else { w })
            .collect();
        Self { argv }
    }

    /// `[program, args…]`.
    pub fn argv(&self) -> &[&'static str] {
        &self.argv
    }

    pub fn program(&self) -> &'static str {
        self.argv[0]
    }

    pub fn args(&self) -> &[&'static str] {
        &self.argv[1..]
    }
}

/// `executeCommand(command, runner)` (`commands.ts:229-257`) for the Rust
/// agent: `refresh`/`read_sms` never reach the runner (the in-process
/// `RunResult`); the rest run [`PrivilegedArgv::for_rust_agent`] (O3).
pub async fn execute_command(command: DeviceCommandName, runner: &dyn PrivilegedRunner) -> RunResult {
    match plan(command) {
        CommandPlan::InProcess(c) => in_process_run_result(c),
        CommandPlan::Privileged(call) => runner.run(&PrivilegedArgv::for_rust_agent(&call)).await,
    }
}

/// Runs a privileged argv to completion; never fails (inspect the result).
pub trait PrivilegedRunner: Send + Sync + 'static {
    fn run<'a>(&'a self, call: &'a PrivilegedArgv) -> RunFut<'a>;
}

/// The production runner: `run("sudo", ["-n", …])` through `PATH`, no
/// timeout, stdout/stderr kept verbatim (`stripFinalNewline: false`).
///
/// A spawn failure (sudo missing, EACCES) maps to what `lib/exec.ts run()`
/// ACTUALLY returns under execa 9.6.1 with `reject: false`: execa resolves
/// (does not throw) with `stderr: ""` and no exit code, so the TS `catch`
/// branch — the one that would set `notFound` — is never reached and the
/// ack reason is `exit null`, not `sudo not found`
/// (`phase-2/evidence/wp4-tools/probe-exec.mts`; ambiguity 1 in wp4.md).
#[derive(Debug, Default, Clone, Copy)]
pub struct SudoRunner;

impl PrivilegedRunner for SudoRunner {
    fn run<'a>(&'a self, call: &'a PrivilegedArgv) -> RunFut<'a> {
        Box::pin(run_argv(call.program(), call.args()))
    }
}

/// `lib/exec.ts run(cmd, args)` (`exec.ts:22-47`, execa 9.6.1 `reject:false`,
/// no timeout, `stripFinalNewline:false`): stdout/stderr decoded as UTF-8
/// (lossy), `failed` = non-zero or signalled, `exitCode` null when signalled.
/// The child is NOT killed if the caller goes away (a `systemctl reboot` in
/// flight when shutdown starts must finish).
async fn run_argv(program: &str, args: &[&str]) -> RunResult {
    let out = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    match out {
        Ok(o) => RunResult {
            failed: !o.status.success(),
            not_found: false,
            exit_code: o.status.code().map(i64::from),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        // Spawn failure (ENOENT, EACCES): execa resolves `{stdout:"",
        // stderr:"", exitCode:undefined, failed:true}` and `run()` keeps
        // `notFound: false` (its `catch` is never reached).
        Err(_) => RunResult {
            failed: true,
            not_found: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

/// Test runner: records every argv and answers from a script (default: ok).
#[derive(Debug, Default)]
pub struct RecordingRunner {
    calls: Mutex<Vec<Vec<&'static str>>>,
    answers: Mutex<Vec<RunResult>>,
    /// Shared ordered log (`exec <argv>`), when the test wants one timeline.
    timeline: Option<Arc<Mutex<Vec<String>>>>,
}

impl RecordingRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Also push `exec <argv joined>` onto a shared timeline.
    pub fn with_timeline(timeline: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            timeline: Some(timeline),
            ..Self::default()
        }
    }

    /// Queue the next answer (FIFO); when the queue is empty, calls succeed.
    pub fn answer(&self, r: RunResult) {
        self.answers.lock().unwrap_or_else(|p| p.into_inner()).push(r);
    }

    pub fn calls(&self) -> Vec<Vec<&'static str>> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// `{stdout:"", stderr:"", exitCode:0, failed:false, notFound:false}`.
    pub fn ok() -> RunResult {
        RunResult {
            failed: false,
            not_found: false,
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

impl PrivilegedRunner for RecordingRunner {
    fn run<'a>(&'a self, call: &'a PrivilegedArgv) -> RunFut<'a> {
        self.calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(call.argv().to_vec());
        if let Some(t) = &self.timeline {
            t.lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(format!("exec {}", call.argv().join(" ")));
        }
        let mut q = self.answers.lock().unwrap_or_else(|p| p.into_inner());
        let r = if q.is_empty() {
            Self::ok()
        } else {
            q.remove(0)
        };
        Box::pin(async move { r })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(c: DeviceCommandName) -> PrivilegedCall {
        match plan(c) {
            CommandPlan::Privileged(p) => p,
            CommandPlan::InProcess(_) => panic!("{c} is in-process"),
        }
    }

    #[test]
    fn o3_swaps_only_the_updater_unit() {
        assert_eq!(
            PrivilegedArgv::for_rust_agent(&call(DeviceCommandName::Update)).argv(),
            ["sudo", "-n", "systemctl", "start", "--no-block", "xorgate-updater-rs.service"]
        );
        assert_eq!(
            PrivilegedArgv::ts_exact(&call(DeviceCommandName::Update)).argv(),
            ["sudo", "-n", "systemctl", "start", "--no-block", "xorgate-updater.service"]
        );
        assert_eq!(
            PrivilegedArgv::for_rust_agent(&call(DeviceCommandName::Reboot)).argv(),
            ["sudo", "-n", "systemctl", "reboot"]
        );
        assert_eq!(
            PrivilegedArgv::for_rust_agent(&call(DeviceCommandName::PowerOff)).argv(),
            ["sudo", "-n", "systemctl", "poweroff"]
        );
    }

    #[tokio::test]
    async fn spawn_failure_reads_like_execa_reject_false() {
        // A binary that does not exist on PATH: execa 9.6.1 resolves
        // `{stdout:"",stderr:"",exitCode:null,failed:true,notFound:false}`.
        let r = run_argv("definitely-not-a-binary-xyz", &["-n"]).await;
        assert_eq!(
            r,
            RunResult {
                failed: true,
                not_found: false,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            }
        );
        assert_eq!(xorgate_core::commands::failure_reason(&r), "exit null");
    }

    #[tokio::test]
    async fn nonzero_exit_keeps_stderr_verbatim() {
        let r = run_argv("sh", &["-c", "echo 'sudo: a password is required' >&2; exit 1"]).await;
        assert_eq!(r.stderr, "sudo: a password is required\n");
        assert_eq!(r.exit_code, Some(1));
        assert!(r.failed && !r.not_found);
        let r = run_argv("sh", &["-c", "exit 3"]).await;
        assert_eq!((r.exit_code, r.stderr.as_str(), r.failed), (Some(3), "", true));
        let r = run_argv("sh", &["-c", "exit 0"]).await;
        assert!(!r.failed);
    }
}
