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
//! Signals ([`ChildHandle::signal`], [`ChildHandle::signal_group`]) are
//! NEVER sent to a bare pid: they are requests to the waiter task, which owns
//! the tokio `Child` and delivers one only after `try_wait()` says the child
//! is still unreaped. An unreaped child (even a zombie) pins its pid and its
//! process-group id, so the number cannot have been recycled; once it is
//! reaped the request is dropped (the TS `child.kill()` is likewise a no-op
//! once Node has seen the exit, `lib/exec.ts:138-144`). WP2 review MUST-FIX 1.
//!
//! [`ChildDone`] carries `exit_code` (`None` when killed by a signal or when
//! the spawn itself failed) and `not_found` (the binary was missing). NOTE the
//! TS quirk the IMU collector reproduces: with execa 9 and `reject:false` a
//! spawn failure RESOLVES with `notFound:false`, so the TS never prints its
//! own "python3 not found" line; this module still reports the truth and the
//! caller decides (see `xorgate-agent::collectors::imu`).

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
    /// `argv[0]` when it must differ from `program` (Phase 7a: the agent
    /// re-execs itself as `xorgate-capture`); `None` = `program` (execa).
    pub arg0: Option<String>,
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

/// Called by the waiter right AFTER its `kill(2)` returned, with the errno
/// (0 = sent). Dropped uncalled when nothing was sent (the child is reaped).
pub type SignalReport = Box<dyn FnOnce(i32) + Send>;

/// A signal request to the waiter: `(signal, whole group?, report)`.
type SignalReq = (i32, bool, Option<SignalReport>);

/// The running child (`StreamHandle`).
#[derive(Debug)]
pub struct ChildHandle {
    pid: Option<u32>,
    ctl: Option<mpsc::UnboundedSender<SignalReq>>,
    delivered: Arc<AtomicUsize>,
    waiter: Option<AbortHandle>,
}

impl ChildHandle {
    /// The pid the child was spawned with (absent when the spawn failed).
    /// Informational only: it may already be reaped and recycled, so never
    /// `kill(2)` it yourself; go through [`Self::signal`].
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// `stop(signal)` (`child.kill(signal)`): signal the child itself, if it
    /// is still unreaped (a no-op afterwards).
    pub fn signal(&self, sig: i32) {
        self.request(sig, false, None);
    }

    /// [`Self::signal`], reporting the `kill(2)` result to `report` once it
    /// returned (review-wp6 M5: the tape records what was really sent). Not
    /// called when nothing is sent.
    pub fn signal_reported(&self, sig: i32, report: SignalReport) {
        self.request(sig, false, Some(report));
    }

    /// Signal the child's whole process group (it leads its own group), if
    /// the leader is still unreaped (a no-op afterwards).
    pub fn signal_group(&self, sig: i32) {
        self.request(sig, true, None);
    }

    /// How many signals were actually delivered (tests / diagnostics).
    pub fn delivered(&self) -> DeliveredSignals {
        DeliveredSignals(self.delivered.clone())
    }

    fn request(&self, sig: i32, group: bool, report: Option<SignalReport>) {
        if let Some(ctl) = &self.ctl {
            // A closed channel = the waiter is done = the child is reaped.
            let _ = ctl.send((sig, group, report));
        }
    }
}

/// A shared count of the signals a [`ChildHandle`] delivered; it outlives the
/// handle.
#[derive(Debug, Clone)]
pub struct DeliveredSignals(Arc<AtomicUsize>);

impl DeliveredSignals {
    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
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

/// The variables systemd puts into the environment of a `Type=notify` unit
/// with `WatchdogSec=` (`systemd.exec(5)`, "Environment Variables in Spawned
/// Processes"). They are the AGENT's: only the main pid may notify
/// (`NotifyAccess=main`), and `WATCHDOG_PID` names it. Phase 6 finding
/// F-P6-ENV: a child that inherited them sent `sd_notify` on every OBD
/// reconnect ("Got notification message from PID …, but reception only
/// permitted for main PID", 56× in 33 min on PROD), and the KVS master's env
/// was JS's 35 keys + these 3 (the JS unit is `Type=simple`: no notify env).
pub const NOTIFY_ENV_KEYS: [&str; 3] = ["NOTIFY_SOCKET", "WATCHDOG_PID", "WATCHDOG_USEC"];

/// THE constructor for every child process the agent (daemon or CLI) starts:
/// `std::process::Command::new(program)` minus [`NOTIFY_ENV_KEYS`]
/// (F-P6-ENV). Raw `Command::new` is a clippy `disallowed-methods` error
/// everywhere else (`clippy.toml`), so no spawn site can forget it.
///
/// - Only the CHILD's environment changes: the agent's own keeps the three
///   keys and goes on notifying the watchdog (`worker.rs` `sd_notify`).
/// - A key is removed only when this process actually has it. With none of
///   them set (no systemd: CLI from a shell, tests, the replay rig) the
///   command is left untouched, so the child inherits `environ` verbatim, as
///   before. When one is set, std materialises the child env with the key
///   removed; std builds such an env from a sorted map, so the child's keys
///   come out in byte order (a [`ChildSpec`] with `env` was already sorted
///   for the same reason).
#[allow(clippy::disallowed_methods)] // the one sanctioned `Command::new`
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    strip_notify_env(&mut cmd, |k| std::env::var_os(k).is_some());
    cmd
}

/// [`std_command`] as a tokio command (`kill_on_drop` etc. still apply).
pub fn command(program: impl AsRef<OsStr>) -> Command {
    Command::from(std_command(program))
}

/// Remove each [`NOTIFY_ENV_KEYS`] entry `present` says the parent has.
fn strip_notify_env(cmd: &mut std::process::Command, present: impl Fn(&str) -> bool) {
    for key in NOTIFY_ENV_KEYS {
        if present(key) {
            cmd.env_remove(key);
        }
    }
}

/// `runStream(cmd, args, {onStdout, onStderr})`: spawn and stream. Never
/// fails synchronously — a spawn failure is a lone `Done` event.
#[allow(unsafe_code)]
pub fn spawn(spec: &ChildSpec) -> (ChildHandle, mpsc::UnboundedReceiver<ChildEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut cmd = command(&spec.program);
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
    if let Some(arg0) = &spec.arg0 {
        cmd.arg0(arg0);
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
                    ctl: None,
                    delivered: Arc::default(),
                    waiter: None,
                },
                rx,
            );
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<SignalReq>();
    let delivered: Arc<AtomicUsize> = Arc::default();
    let counter = delivered.clone();
    let waiter = tokio::spawn(async move {
        let out = drain(stdout, tx.clone(), ChildEvent::Stdout);
        let err = drain(stderr, tx.clone(), ChildEvent::Stderr);
        // The only place a signal reaches the OS: `child` is owned here, and
        // `try_wait()` + `id()` prove it unreaped (pid/pgid still pinned).
        let reap = async {
            loop {
                tokio::select! {
                    st = child.wait() => break st, // cancel safe
                    Some((sig, group, report)) = ctl_rx.recv() => match child.try_wait() {
                        Ok(None) => {
                            if let Some(pid) = child.id() {
                                let target = if group { -(pid as libc::pid_t) } else { pid as libc::pid_t };
                                // SAFETY: plain kill(2) on a pid we have not reaped.
                                let rc = unsafe { libc::kill(target, sig) };
                                let errno = if rc == 0 {
                                    0
                                } else {
                                    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
                                };
                                counter.fetch_add(1, Ordering::SeqCst);
                                if let Some(report) = report {
                                    report(errno);
                                }
                            }
                        }
                        Ok(Some(st)) => break Ok(st),
                        Err(e) => break Err(e),
                    },
                }
            }
        };
        let (_, _, status) = tokio::join!(out, err, reap);
        drop(ctl_rx);
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
            ctl: Some(ctl_tx),
            delivered,
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

    /// review-wp6 M5: the report runs AFTER kill(2) returned, with its errno
    /// and the signal really passed; nothing is reported when nothing is sent.
    #[tokio::test]
    async fn signal_reported_after_the_syscall_with_its_result() {
        let (h, mut rx) = spawn(&sh("echo up; exec sleep 30"));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap(),
            Some(ChildEvent::Stdout("up".into()))
        );
        let (tx, mut got) = mpsc::unbounded_channel();
        let tx2 = tx.clone();
        h.signal_reported(libc::SIGINT, Box::new(move |errno| tx.send(errno).unwrap()));
        let first = tokio::time::timeout(Duration::from_secs(10), got.recv()).await;
        assert_eq!(first.unwrap(), Some(0));
        let ev = all(rx).await;
        assert!(matches!(ev.last(), Some(ChildEvent::Done(d)) if d.signal == Some(libc::SIGINT)));
        // Reaped: the request is dropped, the report never runs.
        h.signal_reported(
            libc::SIGINT,
            Box::new(move |errno| tx2.send(errno).unwrap()),
        );
        let none = tokio::time::timeout(Duration::from_millis(200), got.recv()).await;
        assert!(matches!(none, Ok(None) | Err(_)), "{none:?}");
    }

    #[tokio::test]
    async fn no_signal_reaches_an_exited_child() {
        // MUST-FIX 1: once the child is reaped its pid may be recycled, so a
        // later signal()/signal_group() must deliver nothing.
        let (h, rx) = spawn(&sh("exit 0"));
        let ev = all(rx).await;
        assert!(matches!(ev.last(), Some(ChildEvent::Done(d)) if d.exit_code == Some(0)));
        h.signal(libc::SIGTERM);
        h.signal_group(libc::SIGTERM);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(h.delivered().get(), 0);
    }

    #[tokio::test]
    async fn signal_group_reaches_the_whole_group() {
        let (h, mut rx) = spawn(&sh("sleep 30 & echo up; wait"));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap(),
            Some(ChildEvent::Stdout("up".into()))
        );
        h.signal_group(libc::SIGTERM);
        // Done needs both pipes at EOF: the background sleep holds them, so
        // Done arriving proves the group signal reached it too.
        let ev = all(rx).await;
        assert!(matches!(ev.last(), Some(ChildEvent::Done(d)) if d.signal == Some(libc::SIGTERM)));
        assert_eq!(h.delivered().get(), 1);
    }

    #[tokio::test]
    #[allow(unsafe_code)]
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

    /// F-P6-ENV, pure part (no process-env mutation in this multi-threaded
    /// test binary; the env-set cases run in `tests/child_env.rs`): every
    /// key the parent has is removed, and nothing else is touched.
    #[test]
    fn strip_removes_exactly_the_present_notify_keys() {
        #[allow(clippy::disallowed_methods)]
        let mut cmd = std::process::Command::new("true");
        strip_notify_env(&mut cmd, |_| true);
        let got: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_str().unwrap().to_string(), v.is_none()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("NOTIFY_SOCKET".to_string(), true),
                ("WATCHDOG_PID".to_string(), true),
                ("WATCHDOG_USEC".to_string(), true),
            ],
            "three removals, no other env change"
        );
    }

    /// With none of the keys set (no systemd) the command is untouched, so
    /// the child inherits `environ` verbatim (order included).
    #[test]
    fn strip_is_a_no_op_without_the_keys() {
        #[allow(clippy::disallowed_methods)]
        let mut cmd = std::process::Command::new("true");
        strip_notify_env(&mut cmd, |_| false);
        assert_eq!(cmd.get_envs().count(), 0);
        let mut only_socket = command("true");
        strip_notify_env(only_socket.as_std_mut(), |k| k == "NOTIFY_SOCKET");
        let keys: Vec<_> = only_socket
            .as_std()
            .get_envs()
            .map(|(k, _)| k.to_owned())
            .collect();
        assert!(keys.iter().all(|k| k == "NOTIFY_SOCKET"), "{keys:?}");
    }
}
