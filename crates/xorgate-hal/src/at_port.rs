//! The I/O half of `src/hardware/sms-modem.ts` @ 89b5c64 (Phase 4 WP2, I3):
//! port resolution by USB identity, the serial transport, the AT session, the
//! read-only store sweep and the cross-process lock. The pure half (stores,
//! constants, parsers, payload) is `xorgate_core::sms`; PDU decoding is
//! `xorgate_core::sms_pdu`; the single-flight reader is
//! `xorgate_agent::ops::sms::SmsReader`.
//!
//! Why interface 05 and never `/dev/ttyUSB2` (`sms-modem.ts:4-9`):
//! `modem/sim7600-lte.sh` (root) owns `/dev/ttyUSB2` (interface 04) with no
//! lock — its guarantee is "one process, one loop", and a second writer there
//! would interleave with its probes. The modem exposes a second, independent
//! AT parser on interface 05, which nothing else uses. So the ONLY paths this
//! module opens are what [`resolve_sms_port`] returns: the `ttyUSB*` whose
//! USB interface is `05` on vendor `1e0e`, or the `XORGATE_SMS_PORT`
//! override. No path literal for a modem tty exists in this file.
//!
//! Non-destructive by construction (`sms-modem.ts:11-16`): reads use
//! `AT+CMGRO` (SIMCom "read only"), which leaves REC UNREAD as is; the
//! commands that would mark read / delete / write are never sent (a unit
//! test greps this file's literals). The only settings touched are `CMGF`
//! and `CPMS` mem1; both are read first and restored.
//!
//! Privacy (contract §6): nothing here logs.

use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use xorgate_core::clock::Clock;
use xorgate_core::js;
use xorgate_core::sms::{
    is_final_result_line, is_no_sim, parse_cmgf_query, parse_cmgro_header, parse_cpms_query,
    parse_cpms_set, SmsReadStatus, SmsSweep, StorageCounts, StoreCount, SIMCOM_VENDOR_ID,
    SMS_COMMAND_TIMEOUT_MS, SMS_INTERFACE_NUMBER, SMS_PORT_ENV, SMS_SWEEP_BUDGET_MS, SWEEP_STORES,
};
use xorgate_core::sms_pdu::{assemble_messages, SmsStorage, StoredPdu};

use crate::io::{null_tape, TapeSink};

/// A boxed `Send` future (object-safe async trait methods without a crate).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// `SmsReadError` (`sms-modem.ts:113-120`): a whole-sweep failure with the
/// status the payload carries (never `Ok`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsReadError {
    pub status: SmsReadStatus,
    pub message: String,
}

impl SmsReadError {
    pub fn new(status: SmsReadStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SmsReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SmsReadError {}

/// What [`AtSession::command`] can fail with: the TS `AtTimeoutError`
/// (`sms-modem.ts:201, 257`: `timeout on <cmd up to '='>`) or the error a
/// transport write threw (`transport closed`, Node's `EIO: …, write`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtError {
    Timeout(String),
    Transport(String),
}

impl AtError {
    /// How `SmsReader.runSweep`'s catch maps an error that escaped the sweep
    /// (`sms-modem.ts:533-535`): an `AtTimeoutError` → `error` + its message;
    /// anything else → `error` + `message.slice(0, 120)`.
    fn into_read_error(self) -> SmsReadError {
        match self {
            AtError::Timeout(m) => SmsReadError::new(SmsReadStatus::Error, m),
            AtError::Transport(m) => SmsReadError::new(SmsReadStatus::Error, slice_utf16(&m, 120)),
        }
    }
}

/// `s.slice(0, n)` over UTF-16 code units (a split surrogate becomes U+FFFD
/// on the way out, as Node prints it).
fn slice_utf16(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

// ---------------------------------------------------------------------------
// Port resolution (`sms-modem.ts:62-99`)
// ---------------------------------------------------------------------------

/// `ResolvePortOptions` (`sms-modem.ts:62-66`): the sysfs/dev roots (tests
/// inject a fake tree) and the `XORGATE_SMS_PORT` value (`env` in the TS).
#[derive(Debug, Clone)]
pub struct ResolvePortOptions {
    /// `/sys/class/tty`.
    pub sys_class_tty: PathBuf,
    /// `/dev`.
    pub dev_dir: PathBuf,
    /// `env.XORGATE_SMS_PORT`, untrimmed.
    pub port_override: Option<String>,
}

impl ResolvePortOptions {
    /// The defaults (`sms-modem.ts:77, 80-81`): the real sysfs, `/dev`, and
    /// the process environment.
    pub fn system() -> Self {
        Self {
            sys_class_tty: PathBuf::from("/sys/class/tty"),
            dev_dir: PathBuf::from("/dev"),
            port_override: std::env::var_os(SMS_PORT_ENV)
                .map(|v| v.to_string_lossy().into_owned()),
        }
    }
}

/// `resolveSmsPort` (`sms-modem.ts:76-99`): the `ttyUSB*` whose USB
/// interface is 05 on a SIMCom (`1e0e`) device, or the `XORGATE_SMS_PORT`
/// override (trimmed, non-empty, wins outright). `None` when there is no such
/// port or the sysfs dir is unreadable.
///
/// By identity, not name: ttyUSB numbering follows enumeration order, and a
/// modem that re-enumerates in another composition would otherwise point us
/// at the daemon's port (interface 04, `/dev/ttyUSB2` on the bench).
pub fn resolve_sms_port(opts: &ResolvePortOptions) -> Option<PathBuf> {
    if let Some(o) = opts.port_override.as_deref() {
        let t = js::trim(o);
        if !t.is_empty() {
            return Some(PathBuf::from(t));
        }
    }
    let dir = std::fs::read_dir(&opts.sys_class_tty).ok()?;
    // `readdirSync(sys).filter((n) => /^ttyUSB\d+$/.test(n))`
    let mut names: Vec<String> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.strip_prefix("ttyUSB")
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    // `names.sort((a, b) => Number(a.slice(6)) - Number(b.slice(6)))` (stable).
    names.sort_by(|a, b| {
        let na: f64 = a[6..].parse().unwrap_or(f64::NAN);
        let nb: f64 = b[6..].parse().unwrap_or(f64::NAN);
        na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
    });
    let sys = opts.sys_class_tty.to_string_lossy();
    for n in &names {
        // The kernel resolves `device/..` after following the `device`
        // symlink — the same string paths the TS reads.
        let ifnum = read_attr(&format!("{sys}/{n}/device/../bInterfaceNumber"));
        let vid = read_attr(&format!("{sys}/{n}/device/../../idVendor"));
        if let (Some(ifnum), Some(vid)) = (ifnum, vid) {
            if js::trim(&ifnum) == SMS_INTERFACE_NUMBER
                && js::trim(&vid).to_lowercase() == SIMCOM_VENDOR_ID
            {
                return Some(PathBuf::from(format!(
                    "{}/{n}",
                    opts.dev_dir.to_string_lossy()
                )));
            }
        }
    }
    None
}

/// `readFileSync(p, "utf8")`, `None` on any error.
fn read_attr(p: &str) -> Option<String> {
    std::fs::read(p)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

// ---------------------------------------------------------------------------
// Transport (`sms-modem.ts:105-191`)
// ---------------------------------------------------------------------------

/// Byte-oriented serial I/O (`AtTransport`, `sms-modem.ts:106-111`),
/// injectable so the sweep is testable with a fake modem.
///
/// The TS transport pushes split lines to one consumer; here the session
/// pulls raw chunks and splits them itself (same `\r\n|\r|\n` split, same
/// trim, same blank drop — [`LineSplitter`]).
///
/// **`read_chunk` must be cancel-safe**: the session drops it on its
/// per-command timeout and polls it once to drain stale lines. A transport
/// whose reader stopped (the TS read loop's `break`) never resolves again,
/// so the session times out — as the TS does.
pub trait AtTransport: Send {
    /// `write(data)`: one command line, `cmd\r`, latin1 (ASCII here).
    fn write<'a>(&'a mut self, data: &'a [u8]) -> BoxFut<'a, io::Result<()>>;
    /// The next chunk the far side sent.
    fn read_chunk(&mut self) -> BoxFut<'_, io::Result<Vec<u8>>>;
    /// Stop the reader and release the port. Idempotent; never fails.
    fn close(&mut self) -> BoxFut<'_, ()>;
}

/// The `stty` argv of `openSerialTransport` (`sms-modem.ts:139`): raw, no
/// echo, `min 0 time 1` so every read returns within 100 ms and the reader
/// can notice `close()`. Program first.
pub fn stty_argv(path: &Path) -> Vec<String> {
    let p = path.to_string_lossy().into_owned();
    ["stty", "-F", &p, "raw", "-echo", "min", "0", "time", "1"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `run("stty", …, { timeoutMs: 3000 })` (`sms-modem.ts:139`).
const STTY_TIMEOUT: Duration = Duration::from_millis(3000);

/// `openSerialTransport` (`sms-modem.ts:131-191`) with no tape.
pub async fn open_serial_transport(path: &Path) -> Result<Box<dyn AtTransport>, SmsReadError> {
    open_serial_transport_taped(path, null_tape()).await
}

/// `openSerialTransport` (`sms-modem.ts:131-191`): the node must already be
/// a character device (never created); `stty -F <p> raw -echo min 0 time 1`
/// (3 s; stderr `/busy/i` → `modem_busy "sms port busy"`, `/no such/i` →
/// `no_modem "sms port vanished"`, else `error "stty failed: <120 units>"`);
/// then `open(O_RDWR | O_NOCTTY)` (+ `O_CLOEXEC`, as libuv opens). The
/// termios state is NOT restored on close (TS parity; nothing else uses the
/// port). The `stty` run is reported to the tape as `exec_argv_stderr`.
pub async fn open_serial_transport_taped(
    path: &Path,
    tape: Arc<dyn TapeSink>,
) -> Result<Box<dyn AtTransport>, SmsReadError> {
    // `statSync(path)` follows symlinks; `isCharacterDevice()`.
    let st = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Err(SmsReadError::new(SmsReadStatus::NoModem, "sms port not present")),
    };
    if !st.file_type().is_char_device() {
        return Err(SmsReadError::new(
            SmsReadStatus::NoModem,
            "sms port is not a character device",
        ));
    }
    let argv = stty_argv(path);
    let stty = run_stty(&argv).await;
    {
        let a: Vec<&str> = argv.iter().map(String::as_str).collect();
        tape.exec_with_stderr(&a, &stty.stdout, &stty.stderr, stty.code);
    }
    if stty.failed {
        if contains_ci(&stty.stderr, "busy") {
            return Err(SmsReadError::new(SmsReadStatus::ModemBusy, "sms port busy"));
        }
        if contains_ci(&stty.stderr, "no such") {
            return Err(SmsReadError::new(SmsReadStatus::NoModem, "sms port vanished"));
        }
        return Err(SmsReadError::new(
            SmsReadStatus::Error,
            format!("stty failed: {}", slice_utf16(js::trim(&stty.stderr), 120)),
        ));
    }
    let p = path.to_path_buf();
    let opened = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(&p)
    })
    .await
    .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));
    let file = match opened {
        Ok(f) => Arc::new(f),
        Err(e) => return Err(open_error(&e)),
    };
    Ok(Box::new(SerialTransport::start(file)))
}

/// The `open()` catch of `openSerialTransport` (`sms-modem.ts:148-154`).
fn open_error(e: &io::Error) -> SmsReadError {
    match e.raw_os_error() {
        Some(libc::ENOENT | libc::ENODEV | libc::ENXIO) => {
            SmsReadError::new(SmsReadStatus::NoModem, "sms port vanished")
        }
        Some(libc::EBUSY) => SmsReadError::new(SmsReadStatus::ModemBusy, "sms port busy"),
        Some(libc::EACCES) => {
            SmsReadError::new(SmsReadStatus::Error, "permission denied on sms port")
        }
        errno => SmsReadError::new(
            SmsReadStatus::Error,
            format!(
                "open failed: {}",
                errno.and_then(errno_code).unwrap_or("unknown")
            ),
        ),
    }
}

/// libuv's error name for the errnos an `open(2)` of a tty can return
/// (`err.code`); anything else is the TS's `"unknown"` fallback.
fn errno_code(errno: i32) -> Option<&'static str> {
    Some(match errno {
        libc::EPERM => "EPERM",
        libc::EIO => "EIO",
        libc::EINTR => "EINTR",
        libc::EAGAIN => "EAGAIN",
        libc::ENOMEM => "ENOMEM",
        libc::EISDIR => "EISDIR",
        libc::ENOTDIR => "ENOTDIR",
        libc::EINVAL => "EINVAL",
        libc::ENFILE => "ENFILE",
        libc::EMFILE => "EMFILE",
        libc::ENOTTY => "ENOTTY",
        libc::ETXTBSY => "ETXTBSY",
        libc::EROFS => "EROFS",
        libc::ELOOP => "ELOOP",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::EFAULT => "EFAULT",
        _ => return None,
    })
}

/// `/x/i.test(s)` for an ASCII needle.
fn contains_ci(s: &str, needle: &str) -> bool {
    s.to_ascii_lowercase().contains(needle)
}

struct SttyResult {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    failed: bool,
}

/// `run("stty", args, { timeoutMs: 3000 })` with execa's `reject: false`
/// result shape (`lib/exec.ts:22-48`): a spawn failure or a timeout is
/// `failed` with an EMPTY `stderr` (nothing was captured), so it lands on
/// `stty failed: ` — not on the busy / vanished branches.
async fn run_stty(argv: &[String]) -> SttyResult {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(STTY_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => SttyResult {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code(),
            failed: !out.status.success(),
        },
        Ok(Err(_)) | Err(_) => SttyResult {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            failed: true,
        },
    }
}

/// The opened tty: one blocking reader thread (the TS libuv read loop,
/// `sms-modem.ts:159-174`) feeding a channel; writes on the blocking pool.
struct SerialTransport {
    file: Arc<File>,
    closed: Arc<AtomicBool>,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Fires when the reader thread has exited (`await loop`).
    done: Option<oneshot::Receiver<()>>,
}

impl SerialTransport {
    fn start(file: Arc<File>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let (f, c) = (file.clone(), closed.clone());
        std::thread::Builder::new()
            .name("xorgate-sms-tty".into())
            .spawn(move || {
                reader_loop(&f, &c, &tx);
                drop(f);
                let _ = done_tx.send(());
            })
            .ok();
        Self {
            file,
            closed,
            rx,
            done: Some(done_rx),
        }
    }
}

/// `while (!closed) { read 4096; n === 0 → continue (VTIME tick); error →
/// break }` (`sms-modem.ts:159-174`). `EINTR` is retried, as libuv does.
fn reader_loop(f: &File, closed: &AtomicBool, tx: &mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    let mut r: &File = f;
    while !closed.load(Ordering::SeqCst) {
        match r.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break, // node vanished (modem reset): the session times out
        }
    }
}

impl AtTransport for SerialTransport {
    /// `if (closed) throw new SmsReadError("error", "transport closed");
    /// await fh.write(data, null, "latin1")` (`sms-modem.ts:177-180`).
    fn write<'a>(&'a mut self, data: &'a [u8]) -> BoxFut<'a, io::Result<()>> {
        Box::pin(async move {
            if self.closed.load(Ordering::SeqCst) {
                return Err(io::Error::other("transport closed"));
            }
            let f = self.file.clone();
            let data = data.to_vec();
            tokio::task::spawn_blocking(move || {
                let mut w: &File = &f;
                w.write_all(&data)
                    .map_err(|e| io::Error::other(crate::io::node_fd_message(&e, "write")))
            })
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e.to_string())))
        })
    }

    fn read_chunk(&mut self) -> BoxFut<'_, io::Result<Vec<u8>>> {
        Box::pin(async move {
            match self.rx.recv().await {
                Some(c) => Ok(c),
                // The reader stopped: nothing more ever arrives.
                None => std::future::pending().await,
            }
        })
    }

    /// `closed = true; await loop; await fh.close()` (`sms-modem.ts:184-189`):
    /// the fd closes when the last `Arc` (ours, after the thread's) drops.
    fn close(&mut self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if self.closed.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Some(done) = self.done.take() {
                let _ = done.await;
            }
        })
    }
}

impl Drop for SerialTransport {
    /// A transport dropped without `close()` still stops its reader (it exits
    /// within one VTIME tick and drops its handle on the fd).
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// AT session (`sms-modem.ts:193-270`)
// ---------------------------------------------------------------------------

/// `AtResponse` (`sms-modem.ts:193-199`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AtResponse {
    pub ok: bool,
    /// Final result line (`OK`, `ERROR`, `+CMS ERROR: 321`, …).
    #[serde(rename = "final")]
    pub final_line: String,
    /// Lines that belong to this command.
    pub lines: Vec<String>,
}

/// The transport's line splitter (`sms-modem.ts:169-172`): chunks are
/// decoded latin1, appended, split on `\r\n|\r|\n`; the last part stays
/// buffered; each other part is JS-trimmed and kept when non-blank.
#[derive(Debug, Default)]
pub struct LineSplitter {
    buf: String,
}

impl LineSplitter {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        // `chunk.toString("latin1", 0, n)`
        self.buf.extend(chunk.iter().map(|&b| b as char));
        let mut out = Vec::new();
        let mut part = String::new();
        let mut it = self.buf.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\r' => {
                    if it.peek() == Some(&'\n') {
                        it.next();
                    }
                    out.push(std::mem::take(&mut part));
                }
                '\n' => out.push(std::mem::take(&mut part)),
                c => part.push(c),
            }
        }
        self.buf = part;
        out.into_iter()
            .map(|l| js::trim(&l).to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// The command in flight (`AtSession.pending`, `sms-modem.ts:213-219`).
struct Pending<'p> {
    prefix: Option<&'p str>,
    with_pdu: bool,
    lines: Vec<String>,
    expect_pdu: bool,
}

/// `AtSession` (`sms-modem.ts:212-270`): serialises AT commands over a
/// transport and keeps only the lines that belong to the command in flight.
/// Everything else — `+CMTI:`, `+SMS FULL`, `RDY`, `SMS DONE`, echoes — is
/// dropped: a response line must start with the caller's `prefix`, and for
/// `+CMGRO` the hex line right after the header is taken as its PDU. Lines
/// that arrive while no command is in flight are dropped too (the TS
/// `if (!p) return`): complete lines buffered before a write and the rest of
/// the chunk after a final result line.
pub struct AtSession<'t> {
    transport: &'t mut dyn AtTransport,
    timeout: Duration,
    split: LineSplitter,
    tape: Arc<dyn TapeSink>,
    port: String,
}

impl<'t> AtSession<'t> {
    /// `new AtSession(transport, timeoutMs = 3000)`.
    pub fn new(transport: &'t mut dyn AtTransport, timeout_ms: u64) -> Self {
        Self {
            transport,
            timeout: Duration::from_millis(timeout_ms),
            split: LineSplitter::default(),
            tape: null_tape(),
            port: String::new(),
        }
    }

    /// Report `at_tx` / `at_rx` (I9) for `port` to `tape`.
    pub fn with_tape(mut self, port: &str, tape: Arc<dyn TapeSink>) -> Self {
        self.port = port.to_string();
        self.tape = tape;
        self
    }

    /// Lines that arrived while nothing was in flight: dropped. Polls the
    /// transport once per chunk without waiting (cancel-safe `read_chunk`).
    async fn drain(&mut self) {
        loop {
            let polled = {
                let mut fut = self.transport.read_chunk();
                std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx))).await
            };
            match polled {
                Poll::Ready(Ok(chunk)) => {
                    tape_rx(&*self.tape, &self.port, &chunk);
                    let _ = self.split.push(&chunk);
                }
                Poll::Ready(Err(_)) | Poll::Pending => return,
            }
        }
    }

    /// `command(cmd, prefix = null, withPdu = false)` (`sms-modem.ts:250-269`):
    /// send `cmd\r` and wait for its final result line; the timer is armed
    /// before the write; `timeout on <cmd up to the first '='>` after
    /// `timeoutMs`; a failed write rejects with the write's error.
    pub async fn command(
        &mut self,
        cmd: &str,
        prefix: Option<&str>,
        with_pdu: bool,
    ) -> Result<AtResponse, AtError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let timeout_err = || AtError::Timeout(format!("timeout on {}", cmd.split('=').next().unwrap_or("")));
        self.drain().await;
        let line = format!("{cmd}\r");
        tape_tx(&*self.tape, &self.port, &line);
        match tokio::time::timeout_at(deadline, self.transport.write(line.as_bytes())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(AtError::Transport(e.to_string())),
            Err(_) => return Err(timeout_err()),
        }
        let mut p = Pending {
            prefix,
            with_pdu,
            lines: Vec::new(),
            expect_pdu: false,
        };
        loop {
            let chunk = match tokio::time::timeout_at(deadline, self.transport.read_chunk()).await
            {
                Ok(Ok(c)) => c,
                Ok(Err(_)) => {
                    // A dead reader: nothing more arrives; the timer decides.
                    tokio::time::sleep_until(deadline).await;
                    return Err(timeout_err());
                }
                Err(_) => return Err(timeout_err()),
            };
            tape_rx(&*self.tape, &self.port, &chunk);
            for l in self.split.push(&chunk) {
                if let Some(r) = on_line(&mut p, l) {
                    // The rest of this chunk arrives with nothing pending.
                    return Ok(r);
                }
            }
        }
    }
}

/// `onLine` (`sms-modem.ts:228-247`); `Some` = the final result line.
fn on_line(p: &mut Pending<'_>, line: String) -> Option<AtResponse> {
    if is_final_result_line(&line) {
        return Some(AtResponse {
            ok: line == "OK",
            final_line: line,
            lines: std::mem::take(&mut p.lines),
        });
    }
    if p.expect_pdu {
        p.expect_pdu = false;
        if line.bytes().all(|b| b.is_ascii_hexdigit()) {
            p.lines.push(line);
            return None;
        }
    }
    if let Some(prefix) = p.prefix {
        if line.starts_with(prefix) {
            p.lines.push(line);
            if p.with_pdu {
                p.expect_pdu = true;
            }
        }
    }
    None
}

/// I9 `at_tx{port, line}` — the WP6 base adds the sink method; until then a
/// no-op seam with the final call shape.
fn tape_tx(tape: &dyn TapeSink, port: &str, line: &str) {
    let _ = (tape, port, line);
}

/// I9 `at_rx{port, hex}` (see [`tape_tx`]).
fn tape_rx(tape: &dyn TapeSink, port: &str, chunk: &[u8]) {
    let _ = (tape, port, chunk);
}

// ---------------------------------------------------------------------------
// The sweep (`sms-modem.ts:300-417`)
// ---------------------------------------------------------------------------

/// `SweepOptions` (`sms-modem.ts:300-306`). `clock.mono_ms()` is the TS
/// `now` for the budget (invariant 2: intervals on the monotonic clock).
#[derive(Clone)]
pub struct SweepOptions {
    /// Per-command timeout (plan: 3 s).
    pub command_timeout_ms: u64,
    /// Whole-sweep budget before returning `partial` (plan: 20 s).
    pub budget_ms: u64,
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// The port the transport is on (the tape's `at_tx.port`).
    pub port: String,
    pub tape: Arc<dyn TapeSink>,
}

impl SweepOptions {
    /// The TS defaults (3 s per command, 20 s budget).
    pub fn new(clock: Arc<dyn Clock + Send + Sync>) -> Self {
        Self {
            command_timeout_ms: SMS_COMMAND_TIMEOUT_MS,
            budget_ms: SMS_SWEEP_BUDGET_MS,
            clock,
            port: String::new(),
            tape: null_tape(),
        }
    }
}

/// `sweepTransport` (`sms-modem.ts:315-417`): `ATE0` → `AT+CMGF?` →
/// `AT+CPMS?` → (`AT+CMGF=0` unless already 0) → per store of
/// `SWEEP_STORES`: budget check, `AT+CPMS="<X>"`, `AT+CMGRO=<i>` from 0
/// until `used` found or `total` reached → restore `AT+CPMS="<mem1>"` (only
/// once mem1 changed) + `AT+CMGF=<orig>` (only when changed and the original
/// was readable), restore errors ignored.
///
/// Does not close the transport (the reader stamps the sweep, then closes).
/// The returned `SmsSweep` carries `status: Ok`, `error: None` and zero
/// stamps — `SmsReader` stamps `finishedAt` / `durationMs`. Every failure is
/// already in its `SmsReader` shape (`sms-modem.ts:533-535`): a silent port
/// → `modem_busy "no response on sms port"`; `+CMS ERROR: 310`-class →
/// `no_sim "sim not ready"`; an escaped `AtTimeoutError` → `error` + its
/// text; a transport error → `error` + its first 120 units.
pub async fn sweep_transport(
    transport: &mut dyn AtTransport,
    opts: SweepOptions,
) -> Result<SmsSweep, SmsReadError> {
    let clock = opts.clock.clone();
    let now = move || clock.mono_ms() as i64;
    let start = now();
    let budget = opts.budget_ms as i64;
    let mut at =
        AtSession::new(transport, opts.command_timeout_ms).with_tape(&opts.port, opts.tape.clone());

    match at.command("ATE0", None, false).await {
        Ok(_) => {}
        Err(AtError::Timeout(_)) => {
            return Err(SmsReadError::new(
                SmsReadStatus::ModemBusy,
                "no response on sms port",
            ))
        }
        Err(e) => return Err(e.into_read_error()),
    }

    let cmgf = at
        .command("AT+CMGF?", Some("+CMGF:"), false)
        .await
        .map_err(AtError::into_read_error)?;
    // `cmgf.ok ? Number(/\+CMGF:\s*(\d)/.exec(lines[0] ?? "")?.[1] ?? NaN) : NaN`
    let orig_cmgf: Option<u8> = if cmgf.ok {
        cmgf.lines.first().and_then(|l| parse_cmgf_query(l))
    } else {
        None
    };
    let cpms = at
        .command("AT+CPMS?", Some("+CPMS:"), false)
        .await
        .map_err(AtError::into_read_error)?;
    if !cpms.ok {
        if is_no_sim(&cpms.final_line) {
            return Err(SmsReadError::new(SmsReadStatus::NoSim, "sim not ready"));
        }
        return Err(SmsReadError::new(
            SmsReadStatus::Error,
            format!("CPMS? failed: {}", cpms.final_line),
        ));
    }
    let triples = parse_cpms_query(cpms.lines.first().map(String::as_str).unwrap_or(""));
    let Some(first) = triples.first() else {
        return Err(SmsReadError::new(
            SmsReadStatus::Error,
            "unparseable CPMS? answer",
        ));
    };
    let orig_mem1 = first.mem.clone();

    let mut storage = StorageCounts::new();
    // mem1..mem3 can repeat a store; any mention gives its counts.
    for t in &triples {
        if let Some(s) = SmsStorage::parse(&t.mem) {
            storage.insert(
                s,
                StoreCount {
                    used: t.used,
                    total: t.total,
                },
            );
        }
    }

    let mut records: Vec<StoredPdu> = Vec::new();
    let mut partial = false;
    let mut mem1_changed = false;
    let mut cmgf_changed = false;
    // The `try` block; its `finally` restores below.
    let outcome: Result<(), SmsReadError> = async {
        if orig_cmgf != Some(0) {
            let set = at
                .command("AT+CMGF=0", None, false)
                .await
                .map_err(AtError::into_read_error)?;
            if !set.ok {
                return Err(SmsReadError::new(
                    SmsReadStatus::Error,
                    format!("CMGF=0 failed: {}", set.final_line),
                ));
            }
            cmgf_changed = true;
        }
        'scan: for mem in SWEEP_STORES {
            if now() - start > budget {
                partial = true;
                break;
            }
            let known = storage.get(&mem).copied();
            if known.is_some_and(|k| k.used == 0) {
                continue;
            }
            let sel = at
                .command(&format!("AT+CPMS=\"{}\"", mem.as_str()), Some("+CPMS:"), false)
                .await
                .map_err(AtError::into_read_error)?;
            // Set only once the select answered (a thrown timeout skips it).
            mem1_changed = true;
            if !sel.ok {
                // SM missing on a SIM-less modem is not fatal while ME is readable.
                if mem == SmsStorage::SM && is_no_sim(&sel.final_line) {
                    continue;
                }
                return Err(SmsReadError::new(
                    SmsReadStatus::Error,
                    format!("CPMS=\"{}\" failed: {}", mem.as_str(), sel.final_line),
                ));
            }
            let counts = sel
                .lines
                .first()
                .and_then(|l| parse_cpms_set(l))
                .or(known);
            let Some(counts) = counts else { continue };
            storage.insert(mem, counts);
            if counts.used == 0 {
                continue;
            }
            let mut found: u64 = 0;
            // Indexes are 0-based on the SIM7600; one extra probe covers a
            // 1-based store at negligible cost (an empty slot answers ERROR).
            let mut i: u64 = 0;
            while i <= counts.total && found < counts.used {
                if now() - start > budget {
                    partial = true;
                    break 'scan;
                }
                let r = match at
                    .command(&format!("AT+CMGRO={i}"), Some("+CMGRO:"), true)
                    .await
                {
                    Ok(r) => r,
                    Err(AtError::Timeout(_)) => {
                        partial = true;
                        break 'scan;
                    }
                    Err(e) => return Err(e.into_read_error()),
                };
                if !r.ok || r.lines.len() < 2 {
                    i += 1;
                    continue; // empty slot
                }
                let hm = parse_cmgro_header(&r.lines[0]);
                found += 1;
                records.push(StoredPdu {
                    storage: mem,
                    index: i as i64,
                    stat: hm.map(|(s, _)| s as i64).unwrap_or(1),
                    length: hm.map(|(_, l)| l as usize),
                    pdu: r.lines[1].clone(),
                });
                i += 1;
            }
        }
        Ok(())
    }
    .await;
    // `finally`: restore what the daemon and any other user of the modem
    // expect. Errors here must not mask the sweep's own outcome.
    if mem1_changed {
        let _ = at
            .command(&format!("AT+CPMS=\"{orig_mem1}\""), Some("+CPMS:"), false)
            .await;
    }
    if cmgf_changed {
        if let Some(orig) = orig_cmgf {
            let _ = at.command(&format!("AT+CMGF={orig}"), None, false).await;
        }
    }
    outcome?;

    let assembled = assemble_messages(&records);
    Ok(SmsSweep {
        status: SmsReadStatus::Ok,
        error: None,
        finished_at: 0,
        duration_ms: 0,
        storage,
        skipped: assembled.skipped as u64,
        partial,
        messages: assembled.messages,
    })
}

// ---------------------------------------------------------------------------
// Cross-process lock (`sms-modem.ts:425-470`)
// ---------------------------------------------------------------------------

/// A held `sms-reader.lock` (or the "can't lock: proceed unlocked" no-op).
/// Dropping it is the TS release fn: `unlinkSync(path)`, errors ignored.
#[derive(Debug)]
pub struct SmsLock {
    path: Option<PathBuf>,
}

impl SmsLock {
    /// The no-op release (`() => {}`): no lock path (tests), or the lock
    /// file could not be created for a reason other than `EEXIST`.
    pub fn unlocked() -> Self {
        Self { path: None }
    }

    /// True when this holds a real lock file.
    pub fn is_held(&self) -> bool {
        self.path.is_some()
    }
}

impl Drop for SmsLock {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// `acquireSmsLock` (`sms-modem.ts:431-470`): `wx` create (mode 0644) +
/// our pid; a lock whose pid is gone is stale and taken over (two
/// attempts). `None` when a live process holds it (`kill(pid, 0)` succeeds
/// or fails `EPERM`). Any other create/write error proceeds unlocked.
pub fn acquire_sms_lock(path: &Path) -> Option<SmsLock> {
    for _attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(path)
        {
            Ok(mut f) => {
                // `writeSync(fd, String(process.pid)); closeSync(fd)`; a throw
                // here lands in the catch → not EEXIST → the no-op release.
                return match f.write_all(std::process::id().to_string().as_bytes()) {
                    Ok(()) => Some(SmsLock {
                        path: Some(path.to_path_buf()),
                    }),
                    Err(_) => Some(SmsLock::unlocked()),
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return Some(SmsLock::unlocked()), // can't lock: proceed unlocked
        }
        // `Number(readFileSync(path, "utf8").trim())`; a failed read = NaN.
        let pid = std::fs::read(path)
            .map(|b| xorgate_core::sms_cli::js_string_to_number(js::trim(&String::from_utf8_lossy(&b))))
            .unwrap_or(f64::NAN);
        if pid_alive(pid) {
            return None;
        }
        let _ = std::fs::remove_file(path); // someone else may have cleaned it
    }
    None
}

/// `Number.isInteger(pid) && pid > 0` then `process.kill(pid, 0)`: alive
/// unless the call fails with anything but `EPERM`. Node rejects a pid
/// that is not an int32 (`ERR_INVALID_ARG_TYPE`) → not alive.
fn pid_alive(pid: f64) -> bool {
    if !(pid.is_finite() && pid.fract() == 0.0 && pid > 0.0) {
        return false;
    }
    if pid > f64::from(i32::MAX) {
        return false;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(e) => e == nix::errno::Errno::EPERM,
    }
}
