//! The NMEA tty reader — the I/O half of `src/hardware/gps.ts` `streamGps`
//! (`:46-80`): open the SIM7600 GNSS port read-only and hand every chunk that
//! arrives to the caller, then report `error` / `close` exactly where Node's
//! `fs.ReadStream` emits them.
//!
//! Decision O1: the port is opened `O_RDONLY | O_NOCTTY | O_CLOEXEC` with NO
//! termios change (Node's `createReadStream` changes none; `O_NOCTTY` removes
//! the controlling-tty SIGHUP hazard). The open and the reads run on a
//! dedicated blocking thread that feeds an unbounded channel:
//!
//! - the OPEN blocks like Node's (libuv threadpool) open does — a FIFO
//!   without a writer never completes, which is exactly the "silent, never
//!   errors, never closes" fd the collector's watchdog exists for;
//! - reads are `poll(2)` (250 ms slices) + `read(2)`, so [`NmeaStream::stop`]
//!   (= `stream.destroy()`) takes effect within one slice and the fd is
//!   closed by the thread itself (a dead tty never strands a thread blocked
//!   in `read`).
//!
//! Event order mirrors Node: data chunks…, then on EOF `close`; on a read or
//! open error `error` then `close`; after `stop()` a final `close`.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::io::node_fs_message;

/// Poll slice: the longest a `stop()` waits before the fd is closed.
const POLL_SLICE_MS: i32 = 250;
/// `fs.ReadStream`'s default `highWaterMark`.
const READ_CHUNK: usize = 64 * 1024;

/// One stream event (`data` / `error` / `close`).
#[derive(Debug)]
pub enum TtyEvent {
    /// Raw bytes as one `read(2)` returned them (Node decodes per chunk).
    Data(Vec<u8>),
    /// An open or read failure, already in the TS wording
    /// ([`xorgate_core::gps_group::describe_error`] applied by the caller
    /// from `code` + `message`).
    Error(TtyError),
    /// The stream ended (EOF, error, or `stop()`).
    Close,
}

/// A failure as Node would have reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtyError {
    /// `err.code` (`ENOENT`, `EACCES`, `EIO`, …) when the errno is known.
    pub code: Option<String>,
    /// `err.message` (`EIO: i/o error, read`, `EBUSY: …, open '/dev/x'`).
    pub message: String,
}

/// The stop handle (`GpsStreamHandle.stop` = `stream.destroy()`).
#[derive(Debug, Clone)]
pub struct NmeaStream {
    stop: Arc<AtomicBool>,
}

impl NmeaStream {
    /// A handle that controls nothing (tests / fakes).
    pub fn detached() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `stream.destroy()`: the reader thread closes the fd within one poll
    /// slice and emits a final `Close`.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether `stop()` was called (lets fakes observe it).
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// Something that opens an NMEA stream — the real tty ([`TtyOpener`]) or a
/// test fake.
pub trait NmeaOpener: Send + Sync + 'static {
    fn open(&self, port: &Path) -> (NmeaStream, mpsc::UnboundedReceiver<TtyEvent>);
}

/// The production opener: [`open`].
#[derive(Debug, Default, Clone, Copy)]
pub struct TtyOpener;

impl NmeaOpener for TtyOpener {
    fn open(&self, port: &Path) -> (NmeaStream, mpsc::UnboundedReceiver<TtyEvent>) {
        open(port)
    }
}

/// `createReadStream(port)` on a reader thread. Never fails synchronously:
/// open failures arrive as `Error` + `Close`, like Node's async open.
pub fn open(port: &Path) -> (NmeaStream, mpsc::UnboundedReceiver<TtyEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = NmeaStream::detached();
    let stop = handle.stop.clone();
    let port: PathBuf = port.to_path_buf();
    let spawned = std::thread::Builder::new().name("nmea-tty".into()).spawn({
        let tx = tx.clone();
        move || reader_thread(&port, &stop, &tx)
    });
    if let Err(e) = spawned {
        // Thread exhaustion: report it like any other stream error.
        let _ = tx.send(TtyEvent::Error(TtyError {
            code: None,
            message: e.to_string(),
        }));
        let _ = tx.send(TtyEvent::Close);
    }
    (handle, rx)
}

/// errnos a vanished / misbehaving tty can raise that [`node_fs_message`]'s
/// table does not carry (libuv `uv_strerror` text).
fn extra_errno(errno: i32) -> Option<(&'static str, &'static str)> {
    Some(match errno {
        libc::ENODEV => ("ENODEV", "no such device"),
        libc::ENXIO => ("ENXIO", "no such device or address"),
        libc::EINVAL => ("EINVAL", "invalid argument"),
        libc::EAGAIN => ("EAGAIN", "resource temporarily unavailable"),
        _ => return None,
    })
}

/// Node's `err` for a failed `open` (`CODE: desc, open '<path>'`) or `read`
/// (`CODE: desc, read` — Node names no path for a read).
fn node_error(err: &io::Error, syscall: &str, path: Option<&Path>) -> TtyError {
    let (code, message) = match err.raw_os_error().and_then(extra_errno) {
        Some((code, desc)) => (Some(code.to_string()), format!("{code}: {desc}, {syscall}")),
        None => {
            let msg = node_fs_message(err, syscall, Path::new(""));
            let code = msg
                .split_once(": ")
                .map(|(c, _)| c)
                .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_uppercase()))
                .map(str::to_string);
            let msg = msg.strip_suffix(" ''").map(str::to_string).unwrap_or(msg);
            (code, msg)
        }
    };
    let message = match path {
        Some(p) => format!("{message} '{}'", p.display()),
        None => message,
    };
    TtyError { code, message }
}

fn reader_thread(port: &Path, stop: &AtomicBool, tx: &mpsc::UnboundedSender<TtyEvent>) {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(port);
    let mut file = match opened {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.send(TtyEvent::Error(node_error(&e, "open", Some(port))));
            let _ = tx.send(TtyEvent::Close);
            return;
        }
    };
    let fd = file.as_raw_fd();
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        if stop.load(Ordering::SeqCst) || tx.is_closed() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for an fd this thread owns.
        let rc = unsafe { libc::poll(&mut pfd, 1, POLL_SLICE_MS) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let _ = tx.send(TtyEvent::Error(node_error(&e, "read", None)));
            break;
        }
        if rc == 0 {
            continue; // slice elapsed, nothing to read
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match file.read(&mut buf) {
            Ok(0) => break, // EOF → 'end' + 'close'
            Ok(n) => {
                if tx.send(TtyEvent::Data(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                let _ = tx.send(TtyEvent::Error(node_error(&e, "read", None)));
                break;
            }
        }
    }
    drop(file);
    let _ = tx.send(TtyEvent::Close);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-nmea-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn collect(mut rx: mpsc::UnboundedReceiver<TtyEvent>) -> (Vec<u8>, Vec<TtyError>) {
        let mut data = vec![];
        let mut errs = vec![];
        while let Some(ev) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("event within 5 s")
        {
            match ev {
                TtyEvent::Data(d) => data.extend(d),
                TtyEvent::Error(e) => errs.push(e),
                TtyEvent::Close => return (data, errs),
            }
        }
        panic!("channel closed without Close");
    }

    #[tokio::test]
    async fn file_reads_to_eof_then_closes() {
        let d = scratch("file");
        let f = d.join("nmea.txt");
        std::fs::write(&f, "$GPGGA,1\n$GPRMC,2\n").unwrap();
        let (_h, rx) = open(&f);
        let (data, errs) = collect(rx).await;
        assert_eq!(data, b"$GPGGA,1\n$GPRMC,2\n");
        assert!(errs.is_empty());
    }

    #[tokio::test]
    async fn absent_port_is_error_then_close() {
        let (_h, rx) = open(Path::new("/nonexistent/xorgate-no-such-nmea-port"));
        let (data, errs) = collect(rx).await;
        assert!(data.is_empty());
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code.as_deref(), Some("ENOENT"));
        assert_eq!(
            errs[0].message,
            "ENOENT: no such file or directory, open '/nonexistent/xorgate-no-such-nmea-port'"
        );
    }

    #[tokio::test]
    async fn fifo_streams_chunks_and_stop_closes() {
        let d = scratch("fifo");
        let fifo = d.join("nmea.fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (h, mut rx) = open(&fifo);
        // The reader's open blocks until a writer appears (like Node's).
        let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        w.write_all(b"$GPGGA,a\n").unwrap();
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(TtyEvent::Data(b))) => assert_eq!(b, b"$GPGGA,a\n"),
            other => panic!("expected data, got {other:?}"),
        }
        h.stop();
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(TtyEvent::Close)) => {}
            other => panic!("expected close after stop, got {other:?}"),
        }
        drop(w);
    }

    #[test]
    fn node_error_wording() {
        let e = io::Error::from_raw_os_error(libc::EIO);
        let r = node_error(&e, "read", None);
        assert_eq!(r.message, "EIO: i/o error, read");
        assert_eq!(r.code.as_deref(), Some("EIO"));
        let e = io::Error::from_raw_os_error(libc::ENODEV);
        let r = node_error(&e, "open", Some(Path::new("/dev/ttyUSB1")));
        assert_eq!(r.message, "ENODEV: no such device, open '/dev/ttyUSB1'");
        let e = io::Error::from_raw_os_error(libc::EACCES);
        let r = node_error(&e, "open", Some(Path::new("/dev/ttyUSB1")));
        assert_eq!(r.code.as_deref(), Some("EACCES"));
        assert_eq!(r.message, "EACCES: permission denied, open '/dev/ttyUSB1'");
    }
}
