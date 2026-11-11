//! The I/O seams of the Rust BNO085 reader, FROZEN by phase 7c WP1 for WP2
//! (the `bno08x_client` state machine, its scripted transport and virtual
//! clock, the lockstep runner) and WP3 (the HAL `i2c_dev` and the
//! `__imu-reader` subcommand). Plan `phase-7c-rust-imu.md` §Design 1.
//!
//! The traits mirror what CPython does below the Python driver, one call
//! per Python-level op, so a transcript (`*.imutx`: `open|ioctl|write|read`)
//! lines up op-for-op:
//!
//! - **open:** PureIO's `open("/dev/i2c-1", "r+b", buffering=0)`
//!   (`Adafruit_PureIO/smbus.py:140`), i.e. `open(2)` with `O_RDWR|O_CLOEXEC`.
//! - **write / read:** PureIO's `write_bytes` / `read_bytes`
//!   (`smbus.py:164-170,297-303`): `ioctl(fd, I2C_SLAVE=0x0703, addr & 0x7F)`
//!   (`:152-154`), then EXACTLY ONE `write(2)` / `read(2)` — a zero-length
//!   write included (busdevice's probe, `i2c_device.py:168`), never a
//!   `write_all`/`read_exact` loop (review F2). The ioctl is part of the op:
//!   PureIO issues it before every transfer, and an ioctl failure means no
//!   transfer (`I2cStage::SelectDevice`).
//! - **EAGAIN on the transfer** is NOT an error to CPython: `FileIO.read`
//!   and `FileIO.write` return `None` (`ReadOutcome::WouldBlock`,
//!   `WriteOutcome::WouldBlock`). The client maps a read's `None` to
//!   `TypeError("'NoneType' object is not subscriptable")` and treats a
//!   write's as sent (plan P5). EAGAIN on the ioctl is an ordinary `OSError`.
//! - **A short read** is reported as the bytes actually read (i2c-dev clamps
//!   a read to 8192 bytes); Blinka's copy loop then raises `IndexError`
//!   ([`crate::shtp::ShtpBuffer::fill`]).
//! - **Errors carry glibc's `strerror` text** (the HAL fills it with
//!   `strerror_r`, the scripted transport from the oracle-printed table), so
//!   the client can print `[Errno N] <strerror>` without any OS call.
//!
//! The clock mirrors CPython's `time` module: `monotonic()` returns float
//! SECONDS (CPython 3.11 `_PyTime_AsSecondsDouble`: `ns as f64 / 1e9`) so the
//! client's `elapsed < timeout` tests use the same IEEE arithmetic as the
//! driver (`_elapsed`, `__init__.py:218-219`) on the same virtual times;
//! `sleep()` is `time.sleep()`.
//!
//! Signals: [`ImuClock::sleep`] is the init-phase sleep (SIGINT keeps its
//! default disposition there, so a SIGINT kills the process; D83).
//! [`ImuClock::interval_wait`] is ONLY the helper's loop sleep
//! (`bno085_read.py:88,97`), where the loop's `except KeyboardInterrupt:
//! return 0` (`:98-99`) applies; the implementation reports a SIGINT as
//! [`IntervalWait::Interrupted`] (WP3: SIGINT blocked from the loop start,
//! the wait done with a timed signal wait). No signal API is used here.

/// Which syscall of an op failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cStage {
    /// `open(2)` of the bus device.
    Open,
    /// `ioctl(fd, I2C_SLAVE, addr)` before the transfer.
    SelectDevice,
    /// The single `read(2)` / `write(2)`.
    Transfer,
}

/// A failed syscall: `OSError(errno, strerror)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I2cError {
    pub stage: I2cStage,
    pub errno: i32,
    /// glibc `strerror(errno)` ("Input/output error", "Remote I/O error", …).
    pub strerror: String,
}

/// The result of one `read(2)` that did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The bytes read; fewer than requested only at i2c-dev's 8192 clamp
    /// (or whatever the kernel returned).
    Data(Vec<u8>),
    /// `EAGAIN` on the transfer: CPython `FileIO.read` returned `None`.
    WouldBlock,
}

/// The result of one `write(2)` that did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// `write(2)` returned this count (PureIO ignores it).
    Written(usize),
    /// `EAGAIN` on the transfer: CPython `FileIO.write` returned `None`;
    /// PureIO treats the bytes as sent (`smbus.py:303`).
    WouldBlock,
}

/// The i2c-dev bus as the Python stack drives it (see the module docs).
/// Exactly one syscall sequence per call; no retries anywhere (plan R4).
pub trait I2cOps {
    /// `open(path, O_RDWR | O_CLOEXEC)` — once, before any other op
    /// (`smbus.py:140`). `path` is `/dev/i2c-1` on the bench.
    fn open(&mut self, path: &str) -> Result<(), I2cError>;

    /// `ioctl(I2C_SLAVE, address)` then ONE `write(2)` of exactly `bytes`
    /// (zero length included). `address` is already `addr & 0x7F`.
    fn write(&mut self, address: u8, bytes: &[u8]) -> Result<WriteOutcome, I2cError>;

    /// `ioctl(I2C_SLAVE, address)` then ONE `read(2)` of `len` bytes.
    fn read(&mut self, address: u8, len: usize) -> Result<ReadOutcome, I2cError>;
}

/// What the loop's interval wait ended with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalWait {
    /// The full interval passed.
    Elapsed,
    /// A SIGINT arrived (Python: `KeyboardInterrupt` → `return 0`).
    Interrupted,
}

/// CPython's `time` module as the driver and helper use it.
pub trait ImuClock {
    /// `time.monotonic()` in float seconds.
    fn monotonic(&mut self) -> f64;

    /// `time.sleep(secs)` during init (`__init__.py:512,1054,1056,1062`):
    /// not interruptible from the client's point of view.
    fn sleep(&mut self, secs: f64);

    /// `time.sleep(args.interval)` inside the helper's loop
    /// (`bno085_read.py:88,97`) — the only interruptible wait.
    fn interval_wait(&mut self, secs: f64) -> IntervalWait;
}

impl<T: I2cOps + ?Sized> I2cOps for &mut T {
    fn open(&mut self, path: &str) -> Result<(), I2cError> {
        (**self).open(path)
    }
    fn write(&mut self, address: u8, bytes: &[u8]) -> Result<WriteOutcome, I2cError> {
        (**self).write(address, bytes)
    }
    fn read(&mut self, address: u8, len: usize) -> Result<ReadOutcome, I2cError> {
        (**self).read(address, len)
    }
}

impl<T: ImuClock + ?Sized> ImuClock for &mut T {
    fn monotonic(&mut self) -> f64 {
        (**self).monotonic()
    }
    fn sleep(&mut self, secs: f64) {
        (**self).sleep(secs)
    }
    fn interval_wait(&mut self, secs: f64) -> IntervalWait {
        (**self).interval_wait(secs)
    }
}

/// The bus device PureIO opens for bus 1 (`smbus.py:140`, `f"/dev/i2c-{bus}"`).
pub const I2C_BUS_1_PATH: &str = "/dev/i2c-1";

/// `I2C_SLAVE` (`smbus.py`, `<linux/i2c-dev.h>`).
pub const I2C_SLAVE: u32 = 0x0703;

/// The ioctl argument PureIO passes: `addr & 0x7F` (`smbus.py:154`).
pub fn select_device_arg(address: i64) -> u8 {
    (address & 0x7F) as u8
}

impl I2cError {
    /// The Python exception the op raised: `OSError(errno, strerror)`, with
    /// the path for an `open` failure (`[Errno 13] Permission denied:
    /// '/dev/i2c-1'`). An `open` ENOENT never surfaces as this text: Blinka
    /// turns it into the bus-missing `ValueError`
    /// ([`crate::py_fmt::BUS_MISSING_BCM2712_PROVISIONAL`]).
    pub fn to_py(&self, path: &str) -> crate::py_fmt::PyExc {
        match self.stage {
            I2cStage::Open => crate::py_fmt::PyExc::os_path(self.errno, &self.strerror, path),
            _ => crate::py_fmt::PyExc::os(self.errno, &self.strerror),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_render_like_python_oserror() {
        let e = I2cError {
            stage: I2cStage::Open,
            errno: 13,
            strerror: "Permission denied".into(),
        };
        assert_eq!(
            e.to_py(I2C_BUS_1_PATH).py_str(),
            "[Errno 13] Permission denied: '/dev/i2c-1'"
        );
        let e = I2cError {
            stage: I2cStage::SelectDevice,
            errno: 16,
            strerror: "Device or resource busy".into(),
        };
        assert_eq!(
            e.to_py(I2C_BUS_1_PATH).py_str(),
            "[Errno 16] Device or resource busy"
        );
        assert_eq!(select_device_arg(0x14a), 0x4a);
    }
}
