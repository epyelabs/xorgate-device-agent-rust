//! The real i2c-dev transport of the Rust BNO085 reader (phase 7c WP3; plan
//! `phase-7c-rust-imu.md` §Design 1 "xorgate-hal, i2c_dev.rs", P5, R4/R5):
//! [`I2cDev`] implements the FROZEN [`I2cOps`] seam exactly as CPython +
//! Adafruit PureIO 1.1.11 drive `/dev/i2c-1` (`smbus.py:140,152-154,
//! 164-170,297-303`):
//!
//! - **open:** `open(path, O_RDWR | O_CLOEXEC)` (CPython `FileIO` "r+b",
//!   `buffering=0`); std's `File::open` is that syscall and retries `EINTR`
//!   as CPython does (PEP 475).
//! - **every op:** `ioctl(fd, I2C_SLAVE = 0x0703, addr)` first (PureIO
//!   re-selects before EVERY transfer, never once per open), then EXACTLY
//!   ONE `read(2)` / `write(2)`. Never `read_exact` / `write_all`: a
//!   `write_all(&[])` makes no syscall at all (that would break busdevice's
//!   zero-length probe), and a `read_exact` would hide i2c-dev's 8192-byte
//!   read clamp, which the client must see as a short read. The only loop is
//!   CPython's own `EINTR` retry of the transfer (`_Py_read`/`_Py_write`);
//!   `fcntl.ioctl` does not retry, and neither does [`I2cDev`].
//! - **`EAGAIN` on the transfer** is NOT an error to CPython: `FileIO.read`/
//!   `write` return `None` → [`ReadOutcome::WouldBlock`] /
//!   [`WriteOutcome::WouldBlock`]. On the ioctl it is an ordinary `OSError`.
//! - **No retries** above that (plan R4): an `EREMOTEIO`/`ETIMEDOUT`/`EIO`
//!   flows into the client's 40-consecutive budget, as in Python.
//! - **Errors carry glibc's `strerror` text** ([`strerror`], XSI
//!   `strerror_r`), which is what CPython's `OSError` prints. nix's
//!   `Errno::desc()` uses the kernel-header wording ("I/O error", "Try
//!   again") and is WRONG for parity (review F14); the pin test compares
//!   errnos 2, 5, 6, 11, 13, 16, 19, 110 and 121 with the oracle image's
//!   Python `os.strerror` table (`fixtures/imu_oracle/errno.json`).
//!
//! Every `unsafe` of the transport is one libc call in its own scoped
//! function below ([`select_device`], [`read_once`], [`write_once`],
//! [`strerror`]): a borrowed live fd, integers by value, and pointers into
//! buffers this module owns for the duration of the call.

use std::ffi::CStr;
use std::fs::OpenOptions;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use xorgate_core::imu_io::{I2cError, I2cOps, I2cStage, ReadOutcome, WriteOutcome, I2C_SLAVE};

/// `/dev/i2c-N` as PureIO's `SMBus` holds it: one fd, opened once.
#[derive(Debug, Default)]
pub struct I2cDev {
    fd: Option<OwnedFd>,
}

impl I2cDev {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `open` succeeded and the fd is still held.
    pub fn is_open(&self) -> bool {
        self.fd.is_some()
    }

    /// The held fd (tests, diagnostics).
    pub fn raw_fd(&self) -> Option<i32> {
        self.fd.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Close the bus fd with ONE `close(2)`: CPython's `SMBus.__del__` at
    /// interpreter teardown, which runs BEFORE the final stdout flush
    /// (review-wp2 F4; orchestrator ruling: close → write the unflushed
    /// text → exit). Returns whether an fd was closed (`false` when the open
    /// failed or the bus was already closed: no syscall then).
    pub fn close(&mut self) -> bool {
        // Dropping the OwnedFd is exactly one close(2).
        self.fd.take().is_some()
    }

    fn fd(&self, stage: I2cStage) -> Result<BorrowedFd<'_>, I2cError> {
        // Unreachable from the client (it opens first, once); EBADF is what
        // the syscall would say on a closed fd.
        self.fd
            .as_ref()
            .map(AsFd::as_fd)
            .ok_or_else(|| err(stage, libc::EBADF))
    }
}

fn err(stage: I2cStage, errno: i32) -> I2cError {
    I2cError {
        stage,
        errno,
        strerror: strerror(errno),
    }
}

impl I2cOps for I2cDev {
    fn open(&mut self, path: &str) -> Result<(), I2cError> {
        // std: openat(AT_FDCWD, path, O_RDWR | O_CLOEXEC), EINTR retried.
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => {
                self.fd = Some(OwnedFd::from(f));
                Ok(())
            }
            Err(e) => Err(err(I2cStage::Open, e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn write(&mut self, address: u8, bytes: &[u8]) -> Result<WriteOutcome, I2cError> {
        let fd = self.fd(I2cStage::SelectDevice)?;
        select_device(fd, address).map_err(|e| err(I2cStage::SelectDevice, e))?;
        match write_once(fd, bytes) {
            Ok(n) => Ok(WriteOutcome::Written(n)),
            Err(libc::EAGAIN) => Ok(WriteOutcome::WouldBlock),
            Err(e) => Err(err(I2cStage::Transfer, e)),
        }
    }

    fn read(&mut self, address: u8, len: usize) -> Result<ReadOutcome, I2cError> {
        let fd = self.fd(I2cStage::SelectDevice)?;
        select_device(fd, address).map_err(|e| err(I2cStage::SelectDevice, e))?;
        match read_once(fd, len) {
            Ok(data) => Ok(ReadOutcome::Data(data)),
            Err(libc::EAGAIN) => Ok(ReadOutcome::WouldBlock),
            Err(e) => Err(err(I2cStage::Transfer, e)),
        }
    }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// `ioctl(fd, I2C_SLAVE, address)` — PureIO's `ioctl(self._device.fileno(),
/// I2C_SLAVE, addr & 0x7F)` (`smbus.py:152-154`). `I2C_SLAVE`, never
/// `I2C_SLAVE_FORCE`: a kernel-claimed address fails `EBUSY` as in Python.
/// No `EINTR` retry (`fcntl.ioctl` has none). `Err(errno)` on failure.
#[allow(unsafe_code)]
pub fn select_device(fd: BorrowedFd<'_>, address: u8) -> Result<(), i32> {
    // SAFETY: a borrowed, live fd and an integer argument passed by value;
    // I2C_SLAVE takes no pointer, so the kernel reads nothing from us.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), I2C_SLAVE as _, libc::c_ulong::from(address)) };
    if rc < 0 {
        Err(last_errno())
    } else {
        Ok(())
    }
}

/// ONE `read(2)` of `len` bytes (CPython `FileIO.read(len)`): the bytes the
/// kernel returned, possibly fewer (i2c-dev clamps a read to 8192). `EINTR`
/// is retried, as CPython's `_Py_read` does; nothing else is.
#[allow(unsafe_code)]
pub fn read_once(fd: BorrowedFd<'_>, len: usize) -> Result<Vec<u8>, i32> {
    let mut buf = vec![0u8; len];
    loop {
        // SAFETY: `buf` is a live, exclusively borrowed allocation of `len`
        // bytes; read(2) writes at most `len` bytes into it.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), len) };
        if n >= 0 {
            buf.truncate(n as usize);
            return Ok(buf);
        }
        match last_errno() {
            libc::EINTR => continue,
            e => return Err(e),
        }
    }
}

/// ONE `write(2)` of exactly `bytes` — a zero-length write INCLUDED, which is
/// a real syscall (busdevice's probe `writeto(addr, b"")`). Returns the
/// kernel's count (PureIO ignores it). `EINTR` is retried, as CPython's
/// `_Py_write` does; nothing else is.
#[allow(unsafe_code)]
pub fn write_once(fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, i32> {
    loop {
        // SAFETY: `bytes` is a live borrowed slice; write(2) reads at most
        // `bytes.len()` bytes from it (none for an empty slice, whose
        // dangling-but-aligned pointer is never dereferenced).
        let n = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        match last_errno() {
            libc::EINTR => continue,
            e => return Err(e),
        }
    }
}

/// glibc's message for `errno`: the text CPython's `OSError` carries
/// (`os.strerror`). XSI `strerror_r` (the `libc` crate binds glibc's
/// `__xpg_strerror_r`), never nix's kernel-header wording.
#[allow(unsafe_code)]
pub fn strerror(errno: i32) -> String {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: `buf` is a live stack buffer of `buf.len()` bytes; strerror_r
    // writes a NUL-terminated message of at most that many bytes into it.
    let rc = unsafe { libc::strerror_r(errno, buf.as_mut_ptr(), buf.len()) };
    // SAFETY: strerror_r NUL-terminates within `buf` whenever it returns 0
    // or EINVAL (an unknown errno: "Unknown error N"); the buffer is
    // zero-initialised, so a NUL exists in every case.
    let text = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    if rc != 0 && text.is_empty() {
        return format!("Unknown error {errno}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::OFlag;
    use std::os::fd::AsFd;

    /// `(syscr, syscw)` of THIS thread (`/proc/thread-self/io`): the test
    /// harness runs tests on several threads, the per-thread counters see
    /// only this one's syscalls.
    fn sysc() -> (u64, u64) {
        let s = std::fs::read_to_string("/proc/thread-self/io").expect("/proc/thread-self/io");
        let get = |k: &str| {
            s.lines()
                .find_map(|l| l.strip_prefix(k))
                .and_then(|v| v.trim().parse::<u64>().ok())
                .expect(k)
        };
        (get("syscr:"), get("syscw:"))
    }

    fn pipe(nonblock: bool) -> (OwnedFd, OwnedFd) {
        let flags = if nonblock {
            OFlag::O_CLOEXEC | OFlag::O_NONBLOCK
        } else {
            OFlag::O_CLOEXEC
        };
        nix::unistd::pipe2(flags).expect("pipe2")
    }

    #[test]
    fn strerror_matches_the_oracle_images_python_os_strerror() {
        // fixtures/imu_oracle/errno.json = Python 3.11.2's os.strerror in the
        // oracle image (bookworm glibc 2.36, the bench's line).
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/imu_oracle/errno.json"))
                .expect("errno.json");
        let table = doc["errno"].as_object().expect("errno object");
        let mut seen = Vec::new();
        for (name, pair) in table {
            let n = pair[0].as_i64().expect("number") as i32;
            let text = pair[1].as_str().expect("text");
            assert_eq!(strerror(n), text, "{name} ({n})");
            seen.push(n);
        }
        for must in [2, 5, 6, 11, 13, 16, 19, 110, 121] {
            assert!(seen.contains(&must), "errno {must} not in the fixture");
        }
        // The kernel-header wording nix's desc() would print is NOT it.
        assert_eq!(strerror(libc::EIO), "Input/output error");
        assert_eq!(strerror(libc::EAGAIN), "Resource temporarily unavailable");
        assert_eq!(strerror(libc::EREMOTEIO), "Remote I/O error");
        assert_eq!(strerror(libc::ETIMEDOUT), "Connection timed out");
        assert_eq!(strerror(4321), "Unknown error 4321");
    }

    #[test]
    fn a_zero_length_write_is_one_real_syscall() {
        let (_r, w) = pipe(false);
        let before = sysc().1;
        assert_eq!(write_once(w.as_fd(), &[]), Ok(0));
        let after = sysc().1;
        assert_eq!(
            after - before,
            1,
            "write(2) with an empty buffer must be issued"
        );
    }

    #[test]
    fn a_write_is_exactly_one_syscall() {
        let (_r, w) = pipe(false);
        let before = sysc().1;
        assert_eq!(write_once(w.as_fd(), b"\x05\x00\x01\x00\x01"), Ok(5));
        assert_eq!(sysc().1 - before, 1);
    }

    #[test]
    fn a_read_is_one_syscall_and_may_be_short() {
        let (r, w) = pipe(false);
        assert_eq!(write_once(w.as_fd(), b"abc"), Ok(3));
        // Reading /proc/thread-self/io is itself counted in syscr: measure
        // that overhead first (two back-to-back samples), then subtract it.
        let a = sysc().0;
        let overhead = sysc().0 - a;
        let before = sysc().0;
        // read_exact would block here waiting for the other 5 bytes.
        assert_eq!(read_once(r.as_fd(), 8), Ok(b"abc".to_vec()));
        assert_eq!(sysc().0 - before - overhead, 1);
    }

    #[test]
    fn eagain_is_reported_as_eagain_by_the_transfer_fns() {
        let (r, w) = pipe(true);
        assert_eq!(read_once(r.as_fd(), 4), Err(libc::EAGAIN));
        // Fill the pipe, then one more write would block.
        let chunk = vec![0u8; 65536];
        while write_once(w.as_fd(), &chunk).is_ok() {}
        assert_eq!(write_once(w.as_fd(), b"x"), Err(libc::EAGAIN));
    }

    #[test]
    fn the_ioctl_is_i2c_slave_and_fails_with_enotty_off_an_i2c_adapter() {
        let (r, _w) = pipe(false);
        // CPython: fcntl.ioctl on a non-i2c fd → [Errno 25] Inappropriate
        // ioctl for device (the oracle selftest's `ioctl error` row).
        assert_eq!(select_device(r.as_fd(), 0x4a), Err(libc::ENOTTY));
        assert_eq!(strerror(libc::ENOTTY), "Inappropriate ioctl for device");
        assert_eq!(I2C_SLAVE, 0x0703);
    }

    #[test]
    fn ops_are_staged_open_select_transfer() {
        let mut bus = I2cDev::new();
        let e = bus.open("/nonexistent/i2c-1").unwrap_err();
        assert_eq!((e.stage, e.errno), (I2cStage::Open, libc::ENOENT));
        assert_eq!(e.strerror, "No such file or directory");
        assert!(!bus.is_open());
        assert!(!bus.close(), "nothing to close after a failed open");

        // /dev/null opens O_RDWR, but the I2C_SLAVE ioctl fails: no transfer.
        bus.open("/dev/null").expect("open /dev/null");
        assert!(bus.is_open());
        let e = bus.write(0x4a, &[]).unwrap_err();
        assert_eq!((e.stage, e.errno), (I2cStage::SelectDevice, libc::ENOTTY));
        let e = bus.read(0x4a, 4).unwrap_err();
        assert_eq!((e.stage, e.errno), (I2cStage::SelectDevice, libc::ENOTTY));
        assert_eq!(
            e.to_py(xorgate_core::imu_io::I2C_BUS_1_PATH).py_str(),
            "[Errno 25] Inappropriate ioctl for device"
        );
        assert!(bus.close());
        assert!(!bus.close());
    }

    #[test]
    fn open_is_o_rdwr_cloexec() {
        let mut bus = I2cDev::new();
        bus.open("/dev/null").expect("open");
        let fd = bus.raw_fd().expect("fd");
        let fl = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).expect("fdinfo");
        let flags = fl
            .lines()
            .find_map(|l| l.strip_prefix("flags:"))
            .map(|v| u32::from_str_radix(v.trim(), 8).expect("octal"))
            .expect("flags");
        assert_eq!(flags & libc::O_ACCMODE as u32, libc::O_RDWR as u32);
        assert_ne!(flags & libc::O_CLOEXEC as u32, 0);
        assert_eq!(
            flags & libc::O_NONBLOCK as u32,
            0,
            "blocking, as CPython's FileIO"
        );
        bus.close();
    }
}
