//! A supervised long-running child process — the port of `src/lib/exec.ts`
//! `runStream` (`:70-146`) as the IMU helper uses it, kept generic for the
//! Phase 3 media/capture children.
//!
//! - spawned in its OWN process group (`process_group(0)`) so a caller can
//!   signal the whole tree ([`ChildHandle::signal_group`]); `kill_on_drop`
//!   (dropping the [`ChildHandle`] SIGKILLs a child that is still running);
//! - stdout and stderr are drained line by line (split on `\n`, each line
//!   decoded lossily; an unterminated tail at EOF is dropped, exactly like the
//!   TS `buf.split("\n")` / `lines.pop()` loop, which never flushes it);
//! - [`ChildEvent::Done`] is delivered LAST, after both pipes reached EOF
//!   (execa resolves the result only once its stdio streams have ended), so
//!   every stderr line is seen before the exit.
//!
//! [`ChildDone`] carries `exit_code` (`None` when killed by a signal or when
//! the spawn itself failed) and `not_found` (the binary was missing). NOTE the
//! TS quirk the IMU collector reproduces: with execa 9 and `reject:false` a
//! spawn failure RESOLVES with `notFound:false`, so the TS never prints its
//! own "python3 not found" line; this module still reports the truth and the
//! caller decides (see `xorgate-agent::collectors::imu`).

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct ChildSpec {
    /// Program, resolved through `PATH` like execa.
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment, MERGED onto the inherited one (execa's default).
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

impl ChildSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            ..Default::default()
        }
    }

    /// `[program, ...args]` (what the tape's `imu_spawn` records).
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// How the child ended (`RunResult` of `runStream().done`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChildDone {
    /// `exitCode`: `None` when killed by a signal or never spawned.
    pub exit_code: Option<i32>,
    /// The terminating signal, when there was one.
    pub signal: Option<i32>,
    /// `failed`: non-zero exit, signal, or spawn failure.
    pub failed: bool,
    /// The binary itself was missing (ENOENT at spawn).
    pub not_found: bool,
    /// The spawn error text (empty when the child ran).
    pub spawn_error: String,
}

/// One event from the child, in arrival order; `Done` is always last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildEvent {
    Stdout(String),
    Stderr(String),
    Done(ChildDone),
}

/// The running child (`StreamHandle`).
#[derive(Debug)]
pub struct ChildHandle {
    pid: Option<u32>,
    waiter: Option<AbortHandle>,
}

impl ChildHandle {
    /// OS pid (absent when the spawn failed).
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// `stop(signal)` (`child.kill(signal)`): signal the child itself.
    pub fn signal(&self, sig: i32) {
        if let Some(pid) = self.pid {
            // SAFETY: plain kill(2); a stale pid is at worst ESRCH.
            unsafe {
                libc::kill(pid as libc::pid_t, sig);
            }
        }
    }

    /// Signal the child's whole process group (it leads its own group).
    pub fn signal_group(&self, sig: i32) {
        if let Some(pid) = self.pid {
            // SAFETY: plain kill(2) on the negative pgid.
            unsafe {
                libc::kill(-(pid as libc::pid_t), sig);
            }
        }
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // Aborting the waiter drops the tokio Child → kill_on_drop SIGKILLs a
        // child that is still running; a finished one is unaffected.
        if let Some(w) = self.waiter.take() {
            w.abort();
        }
    }
}

/// `runStream(cmd, args, {onStdout, onStderr})`: spawn and stream. Never
/// fails synchronously — a spawn failure is a lone `Done` event.
pub fn spawn(spec: &ChildSpec) -> (ChildHandle, mpsc::UnboundedReceiver<ChildEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(ChildEvent::Done(ChildDone {
                exit_code: None,
                signal: None,
                failed: true,
                not_found: e.kind() == std::io::ErrorKind::NotFound,
                spawn_error: e.to_string(),
            }));
            return (
                ChildHandle {
                    pid: None,
                    waiter: None,
                },
                rx,
            );
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let waiter = tokio::spawn(async move {
        let out = drain(stdout, tx.clone(), ChildEvent::Stdout);
        let err = drain(stderr, tx.clone(), ChildEvent::Stderr);
        let (_, _, status) = tokio::join!(out, err, child.wait());
        let done = match status {
            Ok(st) => {
                use std::os::unix::process::ExitStatusExt;
                ChildDone {
                    exit_code: st.code(),
                    signal: st.signal(),
                    failed: !st.success(),
                    not_found: false,
                    spawn_error: String::new(),
                }
            }
            Err(e) => ChildDone {
                exit_code: None,
                signal: None,
                failed: true,
                not_found: false,
                spawn_error: e.to_string(),
            },
        };
        let _ = tx.send(ChildEvent::Done(done));
    });
    (
        ChildHandle {
            pid,
            waiter: Some(waiter.abort_handle()),
        },
        rx,
    )
}

/// Read a pipe to EOF, emitting each complete `\n`-terminated line.
async fn drain<R: AsyncRead + Unpin>(
    pipe: Option<R>,
    tx: mpsc::UnboundedSender<ChildEvent>,
    wrap: fn(String) -> ChildEvent,
) {
    let Some(mut pipe) = pipe else { return };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                    let _ = tx.send(wrap(text));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn all(mut rx: mpsc::UnboundedReceiver<ChildEvent>) -> Vec<ChildEvent> {
        let mut v = vec![];
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("event within 10 s");
            match ev {
                Some(ChildEvent::Done(d)) => {
                    v.push(ChildEvent::Done(d));
                    return v;
                }
                Some(e) => v.push(e),
                None => return v,
            }
        }
    }

    fn sh(script: &str) -> ChildSpec {
        ChildSpec::new("sh", vec!["-c".into(), script.into()])
    }

    #[tokio::test]
    async fn lines_then_done_and_tail_dropped() {
        let (h, rx) = spawn(&sh(
            "printf 'a\\nb\\n'; printf 'oops\\n' >&2; printf 'tail'; exit 3",
        ));
        assert!(h.pid().is_some());
        let ev = all(rx).await;
        let out: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                ChildEvent::Stdout(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(out, ["a", "b"]);
        assert!(ev.contains(&ChildEvent::Stderr("oops".into())));
        match ev.last().unwrap() {
            ChildEvent::Done(d) => {
                assert_eq!(d.exit_code, Some(3));
                assert!(d.failed && !d.not_found);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_binary_is_not_found() {
        let (h, rx) = spawn(&ChildSpec::new("xorgate-no-such-binary", vec![]));
        assert!(h.pid().is_none());
        let ev = all(rx).await;
        assert_eq!(ev.len(), 1);
        match &ev[0] {
            ChildEvent::Done(d) => {
                assert!(d.not_found && d.failed);
                assert_eq!(d.exit_code, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn signal_kills_and_exit_code_is_none() {
        let (h, rx) = spawn(&sh("echo up; exec sleep 30"));
        let mut rx = rx;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap(),
            Some(ChildEvent::Stdout("up".into()))
        );
        h.signal(libc::SIGKILL);
        let ev = all(rx).await;
        match ev.last().unwrap() {
            ChildEvent::Done(d) => {
                assert_eq!(d.exit_code, None);
                assert_eq!(d.signal, Some(libc::SIGKILL));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn drop_kills_a_running_child() {
        let (h, _rx) = spawn(&sh("exec sleep 30"));
        let pid = h.pid().unwrap() as libc::pid_t;
        drop(h);
        // The waiter task is aborted → kill_on_drop reaps it.
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            // SAFETY: probe only (signal 0).
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
        }
        panic!("child {pid} survived the handle drop");
    }
}
