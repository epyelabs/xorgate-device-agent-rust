//! Bounded subprocess runs — the port of `src/lib/exec.ts` `run()` +
//! `commandExists()`: nothing throws; the caller inspects `failed`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    /// Non-zero exit, spawn failure, or timeout.
    pub failed: bool,
    /// The binary itself was missing (ENOENT) rather than exiting non-zero.
    pub not_found: bool,
    /// The spawn itself failed (any errno); `exit_code: None` alone also
    /// covers a child killed by a signal.
    pub spawn_failed: bool,
}

/// Run `cmd args…` to completion with an optional timeout. Never errors.
pub async fn run(cmd: &str, args: &[&str], timeout: Option<Duration>) -> RunResult {
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.kill_on_drop(true);
    let fut = command.output();
    let output = match timeout {
        Some(t) => match tokio::time::timeout(t, fut).await {
            Ok(r) => r,
            Err(_) => {
                return RunResult {
                    stderr: format!("{cmd}: timed out after {} ms", t.as_millis()),
                    failed: true,
                    ..Default::default()
                }
            }
        },
        None => fut.await,
    };
    match output {
        Ok(out) => {
            let code = out.status.code();
            RunResult {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                exit_code: code,
                failed: !out.status.success(),
                not_found: false,
                spawn_failed: false,
            }
        }
        Err(err) => RunResult {
            stderr: err.to_string(),
            failed: true,
            not_found: err.kind() == std::io::ErrorKind::NotFound,
            spawn_failed: true,
            ..Default::default()
        },
    }
}

/// `which cmd` without spawning `which`: first executable match on `$PATH`.
pub fn command_exists(cmd: &str) -> bool {
    find_in_path(cmd).is_some()
}

pub fn find_in_path(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        let p = Path::new(cmd);
        return is_executable(p).then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(cmd))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}
