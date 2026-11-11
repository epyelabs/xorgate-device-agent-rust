//! Phase 7c WP3: the REAL `xorgate-rs __imu-reader` process (`argv[0]
//! xorgate-imu`, spawned as the collector spawns it) driven through every C4
//! transcript (`fixtures/imu_oracle/c4/*.imutx`, written by the WP2 oracle
//! from the UNMODIFIED `bno085_read.py` on the bench's Python stack).
//!
//! There is no BNO085 and no `i2c-stub` in a container, so the binary is
//! built with the TEST-ONLY `imu-test-bus` feature (this crate's own
//! dev-dependency; release/rig builds never have it): with
//! `XORGATE_IMU_TEST_BUS` set its bus ops, `time.monotonic()` and INIT sleeps
//! go to the server below, which answers them from the transcript in the
//! strict (virtual-clock) mode of C4. Everything else is the real process:
//! argument parsing, `PR_SET_NAME`, SIGINT at its default disposition
//! through init and BLOCKED from the loop start (`sigtimedwait` at the REAL
//! 50 ms interval wait), default SIGTERM, one `write(2)` per stdout chunk,
//! EPIPE → exit 1, and the teardown order: close the bus, THEN write the
//! unflushed text, THEN exit (orchestrator ruling on review-wp2 F4).
//!
//! Signals are REAL: a transcript `signal` is sent with kill(2) when the
//! reader asks for the op the oracle's helper never got to (SIGTERM, or
//! SIGINT during init); a SIGINT at the loop's interval sleep (`clock sleep
//! sigint:true`) is sent right after the reply to the iteration's last op —
//! SIGINT is blocked there, so it is taken at the interval wait.
//!
//! Per transcript: no op/clock mismatch; stdout BYTE-equal to the oracle's
//! (D80 dumps filtered); stdout at the device `close` = exactly the lines the
//! oracle wrote before its `close`; the exit code / signal; empty stderr.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::Value;
use xorgate_core::imu_lockstep::strip_d80;

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");
const ENV: &str = "XORGATE_IMU_TEST_BUS";
const INTERVAL_BITS: u64 = 0x3fa999999999999a; // 0.05

fn c4_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/imu_oracle/c4")
}

fn errno_table() -> BTreeMap<String, i32> {
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/imu_oracle/errno.json"),
        )
        .unwrap(),
    )
    .unwrap();
    doc["errno"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v[0].as_i64().unwrap() as i32))
        .collect()
}

fn load(name: &str) -> Vec<Value> {
    std::fs::read_to_string(c4_dir().join(name))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn op(r: &Value) -> &str {
    r["op"].as_str().unwrap_or("")
}

/// How a run should be perturbed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Opts {
    /// Drop the read end of the reader's stdout before it writes anything.
    close_stdout: bool,
    /// Send a SIGINT when the reader asks for the FIRST loop op (after
    /// `ready`), BEFORE replying: SIGINT is blocked from the loop start, so
    /// the iteration completes and the reader exits 0 at the interval wait.
    sigint_at_first_loop_op: bool,
    /// Check `/proc/<pid>/{comm,cmdline}` at the first request.
    check_identity: bool,
}

#[derive(Debug, Default)]
struct Outcome {
    problems: Vec<String>,
    stdout: Vec<u8>,
    stderr: String,
    code: Option<i32>,
    signal: Option<i32>,
    closed: bool,
    /// stdout the reader had written when it asked to close the bus.
    stdout_at_close: Option<Vec<u8>>,
    signals_sent: Vec<&'static str>,
    identity: Option<(String, String)>,
}

static N: AtomicUsize = AtomicUsize::new(0);

fn sig_of(name: &str) -> (Signal, &'static str) {
    match name {
        "SIGINT" => (Signal::SIGINT, "SIGINT"),
        "SIGTERM" => (Signal::SIGTERM, "SIGTERM"),
        "SIGKILL" => (Signal::SIGKILL, "SIGKILL"),
        other => panic!("signal {other}"),
    }
}

struct Server<'a> {
    recs: &'a [Value],
    i: usize,
    errno: &'a BTreeMap<String, i32>,
    pid: Pid,
    out: Outcome,
    killed: bool,
    sigint_in_init: bool,
    stdout: Arc<Mutex<Vec<u8>>>,
    first_loop_op: Option<usize>,
    sent_early_sigint: bool,
    /// A perturbed run: the reader may close the bus at any point (its
    /// stdout is not the oracle's then).
    relaxed: bool,
}

impl Server<'_> {
    fn skip_stdout(&mut self) {
        while self.i < self.recs.len() && matches!(op(&self.recs[self.i]), "stdout" | "meta") {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<&Value> {
        self.skip_stdout();
        self.recs.get(self.i)
    }

    fn is_local_interval_sleep(r: &Value) -> bool {
        op(r) == "clock"
            && r["fn"] == "sleep"
            && r["bits"].as_u64() == Some(INTERVAL_BITS)
            && r["sigint"] != true
    }

    fn signal(&mut self, name: &str) {
        let (sig, n) = sig_of(name);
        let _ = kill(self.pid, sig);
        self.out.signals_sent.push(n);
    }

    fn errno(&self, r: &Value) -> Option<String> {
        let name = r["errno"].as_str()?;
        if name == "EAGAIN" {
            return Some("EAGAIN".into());
        }
        Some(
            self.errno
                .get(name)
                .unwrap_or_else(|| panic!("errno {name}"))
                .to_string(),
        )
    }

    /// `Some(reply)`, or `None` = no reply (the reader is being killed).
    fn handle(&mut self, req: &str) -> Result<Option<String>, String> {
        let parts: Vec<&str> = req.split(' ').collect();
        let kind = parts[0];
        if kind == "close" && self.relaxed {
            self.out.stdout_at_close = Some(self.stdout.lock().unwrap().clone());
            self.out.closed = true;
            return Ok(Some("ok".into()));
        }
        // Interval sleeps happen in the reader (real sigtimedwait): skip them.
        if kind != "sleep" {
            while matches!(self.peek(), Some(r) if Self::is_local_interval_sleep(r)) {
                self.i += 1;
            }
        }
        if Some(self.i) == self.first_loop_op && !self.sent_early_sigint {
            self.sent_early_sigint = true;
            self.signal("SIGINT");
        }
        // A signal the oracle's helper got INSTEAD of this op.
        if let Some(r) = self.peek() {
            if op(r) == "signal" {
                let s = r["sig"].as_str().unwrap().to_string();
                self.i += 1;
                self.signal(&s);
                self.killed = true;
                return Ok(None);
            }
        }
        let Some(r) = self.peek().cloned() else {
            return Err(format!("reader asked {req:?} past the transcript's end"));
        };
        let at = self.i;
        let mismatch = |what: &str| Err(format!("record {at}: reader {what}; transcript {r}"));
        match kind {
            "open" => {
                if op(&r) != "open" || r["path"].as_str() != parts.get(1).copied() {
                    return mismatch(req);
                }
                self.i += 1;
                Ok(Some(match self.errno(&r) {
                    Some(e) => format!("err open {e}"),
                    None => "ok".into(),
                }))
            }
            "write" | "read" => {
                let addr: u64 = parts[1].parse().unwrap();
                if op(&r) != "ioctl" || r["req"] != 1795 || r["arg"].as_u64() != Some(addr) {
                    return mismatch(req);
                }
                self.i += 1;
                if let Some(e) = self.errno(&r) {
                    return Ok(Some(format!("err sel {e}")));
                }
                if let Some(s) = self.peek() {
                    if op(s) == "signal" {
                        let s = s["sig"].as_str().unwrap().to_string();
                        self.i += 1;
                        self.signal(&s);
                        self.killed = true;
                        return Ok(None);
                    }
                }
                let Some(t) = self.peek().cloned() else {
                    return Err(format!("reader asked {req:?} past the transcript's end"));
                };
                let at = self.i;
                let ok = if kind == "write" {
                    let hex = if parts[2] == "-" { "" } else { parts[2] };
                    op(&t) == "write" && t["hex"].as_str() == Some(hex)
                } else {
                    op(&t) == "read" && t["len"].as_u64() == parts[2].parse::<u64>().ok()
                };
                if !ok {
                    return Err(format!("record {at}: reader {req}; transcript {t}"));
                }
                self.i += 1;
                Ok(Some(match self.errno(&t).as_deref() {
                    Some("EAGAIN") => "eagain".into(),
                    Some(e) => format!("err xfer {e}"),
                    None if kind == "write" => format!("ok {}", t["ret"].as_i64().unwrap()),
                    None => {
                        let h = t["hex"].as_str().unwrap();
                        format!("data {}", if h.is_empty() { "-" } else { h })
                    }
                }))
            }
            "close" => {
                if op(&r) != "close" {
                    return mismatch(req);
                }
                // What the oracle's helper wrote before its close …
                let want = wanted_stdout(&self.recs[..at]);
                // … must be ALL the reader wrote before asking to close: give
                // the pipe time to deliver anything written earlier.
                std::thread::sleep(Duration::from_millis(300));
                let got = self.stdout.lock().unwrap().clone();
                self.out.stdout_at_close = Some(got.clone());
                if got != want {
                    return Err(format!(
                        "stdout at close: {} bytes, oracle {} bytes before its close",
                        got.len(),
                        want.len()
                    ));
                }
                self.i += 1;
                self.out.closed = true;
                Ok(Some("ok".into()))
            }
            "mono" => {
                if op(&r) != "clock" || r["fn"] != "monotonic" {
                    return mismatch(req);
                }
                self.i += 1;
                Ok(Some(format!("mono {}", r["bits"].as_u64().unwrap())))
            }
            "sleep" => {
                let bits: u64 = parts[1].parse().unwrap();
                if op(&r) != "clock" || r["fn"] != "sleep" || r["bits"].as_u64() != Some(bits) {
                    return mismatch(req);
                }
                self.i += 1;
                if r["sigint"] == true {
                    // SIGINT in an INIT sleep: default disposition → death.
                    if matches!(self.peek(), Some(s) if op(s) == "signal") {
                        self.i += 1;
                    }
                    self.signal("SIGINT");
                    self.killed = true;
                    self.sigint_in_init = true;
                    return Ok(None);
                }
                Ok(Some("ok".into()))
            }
            other => Err(format!("unknown request {other:?}")),
        }
    }

    /// After a reply: a SIGINT at the loop's interval sleep is sent now
    /// (SIGINT is blocked in the loop; the reader takes it at the wait).
    fn after_reply(&mut self) {
        if let Some(r) = self.peek() {
            if op(r) == "clock"
                && r["fn"] == "sleep"
                && r["bits"].as_u64() == Some(INTERVAL_BITS)
                && r["sigint"] == true
            {
                self.i += 1;
                if matches!(self.peek(), Some(s) if op(s) == "signal") {
                    self.i += 1;
                }
                self.signal("SIGINT");
            }
        }
    }

    /// The reader is gone: only what Python alone does may be left.
    fn finish(&mut self, relaxed: bool) {
        if relaxed {
            return;
        }
        loop {
            let Some(r) = self.peek().cloned() else { break };
            match op(&r) {
                _ if Self::is_local_interval_sleep(&r) => self.i += 1,
                "exit" => self.i += 1,
                // D83(a): CPython closes the bus while unwinding a SIGINT
                // during init; the Rust reader dies by the default action.
                "close" if self.sigint_in_init => self.i += 1,
                _ => {
                    self.out
                        .problems
                        .push(format!("record {}: left over: {r}", self.i));
                    break;
                }
            }
        }
    }
}

/// The oracle's stdout in `recs`, D80 dumps filtered, as bytes.
fn wanted_stdout(recs: &[Value]) -> Vec<u8> {
    let lines: Vec<(usize, String)> = recs
        .iter()
        .enumerate()
        .filter(|(_, r)| op(r) == "stdout")
        .map(|(k, r)| (k, r["line"].as_str().unwrap().to_string()))
        .collect();
    strip_d80(&lines)
        .into_iter()
        .flat_map(|(_, l)| (l + "\n").into_bytes())
        .collect()
}

fn spawn_reader(sock: &Path) -> Child {
    let mut cmd = xorgate_hal::child::std_command(BIN);
    cmd.arg0("xorgate-imu")
        .args(["__imu-reader", "--address", "0x4a"])
        .env(ENV, sock)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    cmd.spawn().expect("spawn xorgate-rs")
}

/// Run the real reader against one transcript.
fn run_one(recs: &[Value], errno: &BTreeMap<String, i32>, opts: Opts) -> Outcome {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let sock = std::env::temp_dir().join(format!("xg-imu-{}-{n}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = spawn_reader(&sock);
    let pid = Pid::from_raw(child.id() as i32);
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let out_reader = {
        let pipe = child.stdout.take().unwrap();
        if opts.close_stdout {
            drop(pipe);
            None
        } else {
            let buf = stdout.clone();
            Some(std::thread::spawn(move || {
                let mut pipe = pipe;
                let mut chunk = [0u8; 4096];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(k) => buf.lock().unwrap().extend_from_slice(&chunk[..k]),
                    }
                }
            }))
        }
    };
    let err_reader = {
        let mut pipe = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = pipe.read_to_string(&mut s);
            s
        })
    };
    let first_loop_op = if opts.sigint_at_first_loop_op {
        let ready = recs
            .iter()
            .position(|r| op(r) == "stdout" && r["line"].as_str().unwrap().contains("\"ready\""))
            .expect("a ready line");
        Some(ready + recs[ready..].iter().position(|r| op(r) == "ioctl").unwrap())
    } else {
        None
    };
    let mut srv = Server {
        recs,
        i: 0,
        errno,
        pid,
        out: Outcome::default(),
        killed: false,
        sigint_in_init: false,
        stdout: stdout.clone(),
        first_loop_op,
        sent_early_sigint: false,
        relaxed: opts.close_stdout || opts.sigint_at_first_loop_op,
    };
    // Accept (the reader connects after parsing its args).
    let deadline = Instant::now() + Duration::from_secs(20);
    let conn = loop {
        match listener.accept() {
            Ok((c, _)) => break Some(c),
            Err(_) if Instant::now() < deadline => {
                if let Ok(Some(_)) = child.try_wait() {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break None,
        }
    };
    if let Some(conn) = conn {
        conn.set_nonblocking(false).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut w: UnixStream = conn.try_clone().unwrap();
        let mut r = BufReader::new(conn);
        let mut first = true;
        loop {
            let mut line = String::new();
            match r.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    srv.out.problems.push(format!("socket: {e}"));
                    break;
                }
            }
            if first && opts.check_identity {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
                srv.out.identity = Some((comm.trim().to_string(), cmd.trim().to_string()));
            }
            first = false;
            match srv.handle(line.trim_end()) {
                Ok(Some(reply)) => {
                    if writeln!(w, "{reply}").is_err() {
                        break;
                    }
                    srv.after_reply();
                }
                Ok(None) => {} // being killed: no reply
                Err(p) => {
                    srv.out.problems.push(p);
                    let _ = kill(pid, Signal::SIGKILL);
                    break;
                }
            }
        }
    }
    // The reader must end by itself (or by the signal sent).
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            srv.out.problems.push("the reader did not exit".into());
            let _ = child.kill();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if let Some(t) = out_reader {
        t.join().unwrap();
    }
    srv.out.stderr = err_reader.join().unwrap();
    srv.out.stdout = stdout.lock().unwrap().clone();
    srv.out.code = status.code();
    srv.out.signal = status.signal();
    let relaxed = opts.close_stdout || opts.sigint_at_first_loop_op;
    srv.finish(relaxed);
    let _ = std::fs::remove_file(&sock);
    srv.out
}

fn signal_name(n: i32) -> String {
    Signal::try_from(n)
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|_| n.to_string())
}

/// Every problem of a full-transcript run.
fn verdict(recs: &[Value], o: &Outcome) -> Vec<String> {
    let mut p = o.problems.clone();
    let want = wanted_stdout(recs);
    if o.stdout != want {
        p.push(format!(
            "stdout differs: reader {:?} vs oracle {:?}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&want)
        ));
    }
    let exit = recs
        .iter()
        .rev()
        .find(|r| op(r) == "exit")
        .expect("exit record");
    let want_code = exit["code"].as_i64().map(|c| c as i32);
    let want_sig = exit["signal"].as_str().map(str::to_string);
    let got_sig = o.signal.map(signal_name);
    if (want_code, want_sig.clone()) != (o.code, got_sig.clone()) {
        p.push(format!(
            "exit: reader code {:?} signal {:?}; oracle code {want_code:?} signal {want_sig:?}",
            o.code, got_sig
        ));
    }
    if !o.stderr.is_empty() {
        p.push(format!("stderr: {:?}", o.stderr));
    }
    // A normal exit after a successful open closes the bus (CPython's
    // SMBus.__del__); a death by signal or a failed open does not.
    let oracle_closes = recs.iter().any(|r| op(r) == "close") && want_sig.is_none();
    if oracle_closes != o.closed {
        p.push(format!(
            "close: reader {} vs oracle {oracle_closes}",
            o.closed
        ));
    }
    p
}

fn transcripts() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(c4_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".imutx"))
        .collect();
    v.sort();
    v
}

/// Process-level C4: all transcripts, 8 at a time.
#[test]
fn the_real_reader_process_reproduces_every_c4_transcript() {
    let names = transcripts();
    assert!(names.len() >= 72, "{} transcripts", names.len());
    let errno = Arc::new(errno_table());
    let queue = Arc::new(Mutex::new(names.clone()));
    let results = Arc::new(Mutex::new(Vec::new()));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (queue, results, errno) = (queue.clone(), results.clone(), errno.clone());
            std::thread::spawn(move || loop {
                let Some(name) = queue.lock().unwrap().pop() else {
                    return;
                };
                let recs = load(&name);
                let o = run_one(&recs, &errno, Opts::default());
                let p = verdict(&recs, &o);
                results.lock().unwrap().push((name, p, o.signals_sent));
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    let results = results.lock().unwrap();
    assert_eq!(results.len(), names.len());
    let mut failed = Vec::new();
    let mut sigs = BTreeMap::new();
    for (name, p, sent) in results.iter() {
        for s in sent {
            *sigs.entry(*s).or_insert(0) += 1;
        }
        if !p.is_empty() {
            failed.push(format!("{name}: {}", p.join("; ")));
        }
    }
    assert!(
        failed.is_empty(),
        "{} FAIL:\n{}",
        failed.len(),
        failed.join("\n")
    );
    // the signal paths were really exercised
    assert!(sigs.get("SIGINT").copied().unwrap_or(0) >= 40, "{sigs:?}");
    assert!(sigs.get("SIGTERM").copied().unwrap_or(0) >= 3, "{sigs:?}");
}

/// The teardown order on a normal exit (review-wp2 F4 ruling): `_data_ready`
/// lines still buffered at a loop SIGINT go out AFTER the bus is closed.
#[test]
fn close_then_flush_then_exit() {
    let errno = errno_table();
    let recs = load("s16b_sigint_buffered_exit.imutx");
    let o = run_one(&recs, &errno, Opts::default());
    assert_eq!(verdict(&recs, &o), Vec::<String>::new());
    let at_close = String::from_utf8(o.stdout_at_close.clone().unwrap()).unwrap();
    let all = String::from_utf8(o.stdout.clone()).unwrap();
    assert!(
        !at_close.contains("Byte count is 0x7FFF/0xFFFF; Error?"),
        "{at_close}"
    );
    assert!(
        all.ends_with("Byte count is 0x7FFF/0xFFFF; Error?\nSequence number is 0xFF; Error?\n"),
        "{all}"
    );
    assert_eq!(o.code, Some(0));
}

/// SIGINT during init: the default disposition (death by SIGINT, no close,
/// no stdout, no traceback text: D83(a)); SIGTERM mid-loop: death by SIGTERM
/// and the buffered lines are lost, as in CPython.
#[test]
fn signal_deaths() {
    let errno = errno_table();
    for (name, sig) in [
        ("s16c_sigint_in_init.imutx", libc::SIGINT),
        ("s29b_sigterm_in_init.imutx", libc::SIGTERM),
        ("s29_sigterm_midloop.imutx", libc::SIGTERM),
    ] {
        let recs = load(name);
        let o = run_one(&recs, &errno, Opts::default());
        assert_eq!(verdict(&recs, &o), Vec::<String>::new(), "{name}");
        assert_eq!((o.code, o.signal), (None, Some(sig)), "{name}");
        assert!(!o.closed, "{name}: no close on a signal death");
    }
}

/// A SIGINT that lands mid-I/O in the loop stays pending: the iteration
/// completes (its sample is written), then exit 0 at the interval wait, with
/// the teardown close (D83(b)).
#[test]
fn a_loop_sigint_mid_io_is_taken_at_the_interval_wait() {
    let errno = errno_table();
    let recs = load("s01_clean_init.imutx");
    let o = run_one(
        &recs,
        &errno,
        Opts {
            sigint_at_first_loop_op: true,
            ..Opts::default()
        },
    );
    assert!(o.problems.is_empty(), "{:?}", o.problems);
    assert_eq!((o.code, o.signal), (Some(0), None), "{o:?}");
    assert!(o.closed);
    let text = String::from_utf8(o.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with("{\"type\": \"ready\""), "{text}");
    assert!(lines[1].starts_with("{\"type\": \"sample\""), "{text}");
}

/// EPIPE on stdout (the agent is gone): exit 1 (Python 120: D83 note).
#[test]
fn epipe_exits_1() {
    let errno = errno_table();
    for name in ["s22a_open_enoent.imutx", "s05_probe_nack.imutx"] {
        let recs = load(name);
        let o = run_one(
            &recs,
            &errno,
            Opts {
                close_stdout: true,
                ..Opts::default()
            },
        );
        assert!(o.problems.is_empty(), "{name}: {:?}", o.problems);
        assert_eq!((o.code, o.signal), (Some(1), None), "{name}: {o:?}");
        assert!(o.stderr.is_empty(), "{name}: {:?}", o.stderr);
    }
}

/// `argv[0]` and `comm` both read `xorgate-imu` (PR_SET_NAME): what WP4's
/// SHTP-client count (`pgrep -xc xorgate-imu`) matches.
#[test]
fn the_reader_names_itself_xorgate_imu() {
    let errno = errno_table();
    let recs = load("s23a_probe_zero_write.imutx");
    let o = run_one(
        &recs,
        &errno,
        Opts {
            check_identity: true,
            ..Opts::default()
        },
    );
    assert_eq!(verdict(&recs, &o), Vec::<String>::new());
    let (comm, cmdline) = o.identity.unwrap();
    assert_eq!(comm, "xorgate-imu");
    assert_eq!(cmdline, "xorgate-imu __imu-reader --address 0x4a");
}

/// argparse-compatible errors and help, through the real dispatch (no bus:
/// nothing is opened).
#[test]
fn argparse_exits() {
    let run = |args: &[&str]| {
        let mut cmd = xorgate_hal::child::std_command(BIN);
        cmd.arg0("xorgate-imu")
            .arg("__imu-reader")
            .args(args)
            .env_remove(ENV)
            .stdin(Stdio::null());
        cmd.output().unwrap()
    };
    let o = run(&["--help"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("usage: xorgate-imu [-h]"));
    let o = run(&["--bogus"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&o.stderr),
        "usage: xorgate-imu [-h] [--address ADDRESS] [--interval INTERVAL]\n\
         xorgate-imu: error: unrecognized arguments: --bogus\n"
    );
    assert!(o.stdout.is_empty());
    let o = run(&["--address", "zz"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&o.stderr),
        "ValueError: invalid literal for int() with base 10: 'zz'\n"
    );
}
