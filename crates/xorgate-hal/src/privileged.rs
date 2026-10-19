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

use std::time::Duration;

use xorgate_core::commands::{
    in_process_run_result, plan, CommandPlan, DeviceCommandName, HelperCall, PrivilegedCall,
    RunResult,
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
    /// The TS argv verbatim (`commands.ts:252, 256`). Test-only: the Rust
    /// agent never runs it (O3 swaps the updater unit; review-wp4 #5).
    #[cfg(test)]
    pub(crate) fn ts_exact(call: &PrivilegedCall) -> Self {
        Self { argv: call.argv() }
    }

    /// What the Rust agent runs: the TS argv with the updater unit swapped
    /// for [`RS_UPDATER_UNIT`] (O3). `reboot`/`poweroff` are unchanged.
    pub fn for_rust_agent(call: &PrivilegedCall) -> Self {
        let argv = call
            .argv()
            .into_iter()
            .map(|w| {
                if w == JS_UPDATER_UNIT {
                    RS_UPDATER_UNIT
                } else {
                    w
                }
            })
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
pub async fn execute_command(
    command: DeviceCommandName,
    runner: &dyn PrivilegedRunner,
) -> RunResult {
    match plan(command) {
        CommandPlan::InProcess(c) => in_process_run_result(c),
        CommandPlan::Privileged(call) => runner.run(&PrivilegedArgv::for_rust_agent(&call)).await,
    }
}

/// The exact argv of one Phase 4 sudo helper run (I5): built only from a
/// core [`HelperCall`] (whose only mints are `clock_step::setclock_call` and
/// `boot_config::camrotate_call`). Private fields, like [`PrivilegedArgv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperArgv {
    argv: Vec<String>,
    timeout: Option<Duration>,
}

impl From<&HelperCall> for HelperArgv {
    fn from(call: &HelperCall) -> Self {
        Self {
            argv: call.argv(),
            timeout: call.timeout_ms().map(Duration::from_millis),
        }
    }
}

impl HelperArgv {
    /// `["sudo", "-n", <helper>, args…]`.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn program(&self) -> &str {
        &self.argv[0]
    }

    pub fn args(&self) -> Vec<&str> {
        self.argv[1..].iter().map(String::as_str).collect()
    }

    /// `run(…, { timeoutMs })` — 20 s for `xorgate-camrotate`, none for
    /// `xorgate-setclock`.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}

/// Runs a privileged argv to completion; never fails (inspect the result).
pub trait PrivilegedRunner: Send + Sync + 'static {
    fn run<'a>(&'a self, call: &'a PrivilegedArgv) -> RunFut<'a>;
    /// One sudo helper run (I5): the clock stepper's `xorgate-setclock` and
    /// the camera-mount reconciler's `xorgate-camrotate`.
    fn run_helper<'a>(&'a self, call: &'a HelperArgv) -> RunFut<'a>;
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

    fn run_helper<'a>(&'a self, call: &'a HelperArgv) -> RunFut<'a> {
        Box::pin(async move {
            let args = call.args();
            match call.timeout() {
                None => run_argv(call.program(), &args).await,
                Some(t) => run_argv_timeout(call.program(), &args, t).await,
            }
        })
    }
}

/// [`run_argv`] with execa's `timeout` (`boot-config.ts:285-287`
/// `{ timeoutMs: 20_000 }`): on expiry the child is killed (SIGKILL on drop;
/// execa sends SIGTERM) and the result reads like execa's timed-out
/// `reject:false` resolution — `failed`, no exit code — so the caller's
/// `stderr || stdout || exit <code>` detail is `exit null`. Ambiguity (wp3.md
/// A3): execa would keep any output captured before the kill; a 20 s
/// overrun of a one-pass awk helper is not worth reproducing that for.
async fn run_argv_timeout(program: &str, args: &[&str], timeout: Duration) -> RunResult {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(o)) => RunResult {
            failed: !o.status.success(),
            not_found: false,
            exit_code: o.status.code().map(i64::from),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Ok(Err(_)) | Err(_) => RunResult {
            failed: true,
            not_found: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
        },
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

/// Reports every settled run to the tape as a `sudo` record (I9: "every
/// `PrivilegedRunner` run — `PrivilegedArgv` + `HelperArgv` — when it
/// settles"): full argv (`["sudo","-n",…]`) + code/stdout/stderr/not-found.
/// Wrap the production [`SudoRunner`] with it; with no tape configured the
/// hook is a no-op.
pub struct TapedRunner {
    inner: Arc<dyn PrivilegedRunner>,
    tape: Arc<dyn crate::io::TapeSink>,
}

impl TapedRunner {
    pub fn new(inner: Arc<dyn PrivilegedRunner>, tape: Arc<dyn crate::io::TapeSink>) -> Self {
        Self { inner, tape }
    }
}

fn tape_result(r: &RunResult) -> crate::exec::RunResult {
    crate::exec::RunResult {
        stdout: r.stdout.clone(),
        stderr: r.stderr.clone(),
        exit_code: r.exit_code.map(|c| c as i32),
        failed: r.failed,
        not_found: r.not_found,
        spawn_failed: false,
    }
}

impl PrivilegedRunner for TapedRunner {
    fn run<'a>(&'a self, call: &'a PrivilegedArgv) -> RunFut<'a> {
        Box::pin(async move {
            let r = self.inner.run(call).await;
            self.tape.sudo(call.argv(), &tape_result(&r));
            r
        })
    }

    fn run_helper<'a>(&'a self, call: &'a HelperArgv) -> RunFut<'a> {
        Box::pin(async move {
            let r = self.inner.run_helper(call).await;
            let argv: Vec<&str> = call.argv().iter().map(String::as_str).collect();
            self.tape.sudo(&argv, &tape_result(&r));
            r
        })
    }
}

/// Test runner: records every argv and answers from a script (default: ok).
/// Helper runs ([`PrivilegedRunner::run_helper`]) are recorded separately
/// ([`RecordingRunner::helper_calls`]) and answer from the SAME FIFO queue.
#[derive(Debug, Default)]
pub struct RecordingRunner {
    calls: Mutex<Vec<Vec<&'static str>>>,
    helper_calls: Mutex<Vec<Vec<String>>>,
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
        self.answers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(r);
    }

    pub fn calls(&self) -> Vec<Vec<&'static str>> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Every [`HelperArgv`] run so far (`["sudo", "-n", <helper>, …]`).
    pub fn helper_calls(&self) -> Vec<Vec<String>> {
        self.helper_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn next_answer(&self) -> RunResult {
        let mut q = self.answers.lock().unwrap_or_else(|p| p.into_inner());
        if q.is_empty() {
            Self::ok()
        } else {
            q.remove(0)
        }
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
        let r = self.next_answer();
        Box::pin(async move { r })
    }

    fn run_helper<'a>(&'a self, call: &'a HelperArgv) -> RunFut<'a> {
        self.helper_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(call.argv().to_vec());
        if let Some(t) = &self.timeline {
            t.lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(format!("exec {}", call.argv().join(" ")));
        }
        let r = self.next_answer();
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
            [
                "sudo",
                "-n",
                "systemctl",
                "start",
                "--no-block",
                "xorgate-updater-rs.service"
            ]
        );
        assert_eq!(
            PrivilegedArgv::ts_exact(&call(DeviceCommandName::Update)).argv(),
            [
                "sudo",
                "-n",
                "systemctl",
                "start",
                "--no-block",
                "xorgate-updater.service"
            ]
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
    async fn helper_argv_comes_only_from_a_minted_call() {
        let call = xorgate_core::clock_step::setclock_call(1_785_326_700_000.0).unwrap();
        let a = HelperArgv::from(&call);
        assert_eq!(
            a.argv(),
            [
                "sudo",
                "-n",
                "/usr/local/bin/xorgate-setclock",
                "1785326700"
            ]
        );
        assert_eq!((a.program(), a.timeout()), ("sudo", None));
        let cam = xorgate_core::boot_config::camrotate_call(
            "imx290",
            "cam1",
            xorgate_core::types::CameraRotationDeg::Deg0,
        )
        .unwrap();
        let b = HelperArgv::from(&cam);
        assert_eq!(
            b.args(),
            [
                "-n",
                "/usr/local/bin/xorgate-camrotate",
                "imx290",
                "cam1",
                "0"
            ]
        );
        assert_eq!(b.timeout(), Some(Duration::from_secs(20)));

        let r = RecordingRunner::new();
        r.answer(RunResult {
            failed: true,
            not_found: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "sudo: a password is required\n".into(),
        });
        assert!(r.run_helper(&a).await.failed);
        assert!(!r.run_helper(&b).await.failed); // queue empty → ok
        assert_eq!(r.helper_calls(), [a.argv().to_vec(), b.argv().to_vec()]);
        assert!(r.calls().is_empty());
    }

    #[tokio::test]
    async fn taped_runner_records_every_settled_run() {
        type SudoRec = (Vec<String>, Option<i32>, String);
        #[derive(Default)]
        struct Rec(Mutex<Vec<SudoRec>>);
        impl crate::io::TapeSink for Rec {
            fn sudo(&self, argv: &[&str], res: &crate::exec::RunResult) {
                self.0.lock().unwrap().push((
                    argv.iter().map(|a| a.to_string()).collect(),
                    res.exit_code,
                    res.stderr.clone(),
                ));
            }
        }
        let inner = Arc::new(RecordingRunner::new());
        inner.answer(RunResult {
            failed: true,
            not_found: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "sudo: a password is required\n".into(),
        });
        let tape = Arc::new(Rec::default());
        let r = TapedRunner::new(inner.clone(), tape.clone());
        let clock = xorgate_core::clock_step::setclock_call(1_785_326_700_000.0).unwrap();
        assert!(r.run_helper(&HelperArgv::from(&clock)).await.failed);
        assert!(
            !r.run(&PrivilegedArgv::for_rust_agent(&call(
                DeviceCommandName::Update
            )))
            .await
            .failed
        );
        let got = tape.0.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0],
            (
                vec![
                    "sudo".to_string(),
                    "-n".into(),
                    "/usr/local/bin/xorgate-setclock".into(),
                    "1785326700".into()
                ],
                Some(1),
                "sudo: a password is required\n".to_string()
            )
        );
        assert_eq!(got[1].0.last().unwrap(), "xorgate-updater-rs.service");
        assert_eq!(inner.helper_calls().len(), 1);
        assert_eq!(inner.calls().len(), 1);
    }

    #[tokio::test]
    async fn helper_timeout_reads_like_execa_timed_out() {
        let r = run_argv_timeout(
            "sh",
            &["-c", "echo partial; sleep 5"],
            Duration::from_millis(100),
        )
        .await;
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
        let r = run_argv_timeout("sh", &["-c", "exit 3"], Duration::from_secs(5)).await;
        assert_eq!((r.exit_code, r.failed), (Some(3), true));
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
        let r = run_argv(
            "sh",
            &["-c", "echo 'sudo: a password is required' >&2; exit 1"],
        )
        .await;
        assert_eq!(r.stderr, "sudo: a password is required\n");
        assert_eq!(r.exit_code, Some(1));
        assert!(r.failed && !r.not_found);
        let r = run_argv("sh", &["-c", "exit 3"]).await;
        assert_eq!(
            (r.exit_code, r.stderr.as_str(), r.failed),
            (Some(3), "", true)
        );
        let r = run_argv("sh", &["-c", "exit 0"]).await;
        assert!(!r.failed);
    }
}
