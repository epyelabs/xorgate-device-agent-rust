//! HAL seams every subsystem shares (Phase 2 WP1, frozen for wave B):
//!
//! - [`TapeSink`] — the record/replay hook (`phase-2-cloud-plane.md`
//!   §Record/replay). Every HAL reader reports the raw input it just read,
//!   AT THE POINT OF READING, through one method per tape kind. Without a tape
//!   the sink is [`NullTape`], whose methods are empty: a hook costs one
//!   virtual call. The gz file writer is WP7's (`xorgate_hal::tape`); the
//!   agent obtains it through [`open_file_tape`], the one function WP7 fills.
//! - [`node_fs_message`] — the `err.message` Node prints for a failed fs call
//!   (`ENOENT: no such file or directory, open '/x'`), for the TS strings that
//!   embed it (`failed to read creds (…)`, `ledger write failed: …`, …);
//!   [`node_fd_message`] / [`node_rename_message`] are its fd-call and
//!   `rename` shapes.

use std::io;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value};

/// The record/replay sink. Default methods are no-ops so a sink implements
/// only what it records; [`NullTape`] implements nothing.
///
/// Who calls what (the tape table of the phase file):
///
/// | method | kind | caller |
/// |---|---|---|
/// | [`nmea`](Self::nmea) | `nmea` | WP2 tty reader, per line, before parsing |
/// | [`imu`](Self::imu) / [`imu_spawn`](Self::imu_spawn) / [`imu_exit`](Self::imu_exit) | `imu`, `imu_spawn`, `imu_exit` | WP2 python child |
/// | [`i2cdetect`](Self::i2cdetect) | `i2cdetect` | WP2 detection |
/// | [`lte_file`](Self::lte_file) | `lte_file` | [`crate::modem_file`] (every read; the writer de-duplicates) |
/// | [`exec`](Self::exec) | `exec` | WP2 vcgencmd runs (every run; the writer de-duplicates per argv) |
/// | [`dt`](Self::dt) | `dt` | WP2 `max_current` read (once) |
/// | [`mqtt_in`](Self::mqtt_in) | `mqtt_in` | WP1 MQTT plane, every inbound publish |
/// | [`conn`](Self::conn) | `conn` | WP1 MQTT plane, connect / close |
/// | [`cli`](Self::cli) | `cli` | WP6 `xorgate-rs set …` |
/// | [`clock`](Self::clock) | `clock` | WP1 spine, every 10 s while a tape is open |
/// | [`proc_read`](Self::proc_read) | `proc` | optional, Rust-only replay of `/proc`/`/sys` |
pub trait TapeSink: Send + Sync + 'static {
    /// True when a tape is being written (lets a caller skip building an
    /// expensive argument; never needed for correctness).
    fn enabled(&self) -> bool {
        false
    }
    /// One NMEA line off the GNSS tty, as split on `\n` (a trailing `\r` kept).
    fn nmea(&self, _line: &str) {}
    /// One stdout line of the IMU helper.
    fn imu(&self, _line: &str) {}
    /// The IMU helper was spawned with this argv.
    fn imu_spawn(&self, _argv: &[String]) {}
    /// The IMU helper exited (`None` = killed by a signal / spawn error).
    fn imu_exit(&self, _code: Option<i32>) {}
    /// `i2cdetect -y 1` stdout, or `None` when the tool is absent (`notFound`).
    fn i2cdetect(&self, _stdout: Option<&str>) {}
    /// The full LTE telemetry file bytes, or `None` when absent/unreadable.
    fn lte_file(&self, _bytes: Option<&[u8]>) {}
    /// A tool run: argv (program first), stdout, exit code (`None` = timeout / not found).
    fn exec(&self, _argv: &[&str], _stdout: &str, _code: Option<i32>) {}
    /// `/proc/device-tree/chosen/power/max_current` bytes, or `None` when absent.
    fn dt(&self, _max_current: Option<&[u8]>) {}
    /// An inbound MQTT publish (retained redeliveries included).
    fn mqtt_in(&self, _topic: &str, _payload: &[u8], _retain: bool, _qos: u8) {}
    /// MQTT plane transition: `true` = connected, `false` = closed.
    fn conn(&self, _connected: bool) {}
    /// `xorgate-rs …` argv (without the program name) run while taping.
    fn cli(&self, _argv: &[String]) {}
    /// Periodic clock sample: wall ms + monotonic ms (any fixed origin).
    fn clock(&self, _wall_ms: i64, _mono_ms: u64) {}
    /// Optional Rust-only replay of a `/proc` / `/sys` read.
    fn proc_read(&self, _path: &str, _text: &str) {}
    /// Flush and close (graceful shutdown). Idempotent.
    fn close(&self) {}
}

/// The default sink: records nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullTape;

impl TapeSink for NullTape {}

/// A shareable no-op sink.
pub fn null_tape() -> Arc<dyn TapeSink> {
    Arc::new(NullTape)
}

/// What the agent knows at start that a tape's meta line carries
/// (`{"v":1,"k":"meta",deviceId,build,startWall,hardware,kvs:{channels},env}`).
/// The writer computes the redacted `env` block itself.
#[derive(Debug, Clone, Default)]
pub struct TapeStart {
    pub device_id: String,
    /// `xorgate-rs -v` text.
    pub build: String,
    /// Wall ms at `t = 0`.
    pub start_wall: i64,
    /// `hardware.json` as parsed at start (`Null` when absent).
    pub hardware: Value,
    /// The kvs.json manifest's `channels` at start (empty when absent).
    pub kvs_channels: Vec<Value>,
    /// The agent's home files at start, `name -> text | null`
    /// (`settings.json`, `config-sync.json`, `commands.json`).
    pub home_files: Option<Map<String, Value>>,
}

/// Open the gz NDJSON tape at `path` (`XORGATE_RS_TAPE`). **WP7 fills this
/// body** with its `tape::FileTape`; until then the agent logs the error and
/// runs with [`NullTape`].
pub fn open_file_tape(path: &Path, _start: &TapeStart) -> io::Result<Arc<dyn TapeSink>> {
    Err(io::Error::other(format!(
        "tape writer not built into this binary (WP7); {} not opened",
        path.display()
    )))
}

/// Node's `err.message` for a failed fs call: `CODE: description, syscall
/// 'path'` (libuv's `uv_strerror` text). Errors without a known errno fall
/// back to Rust's own text after the same prefix shape.
pub fn node_fs_message(err: &io::Error, syscall: &str, path: &Path) -> String {
    let (code, desc) = match err.raw_os_error().and_then(errno_name) {
        Some(pair) => pair,
        None => return format!("{err}, {syscall} '{}'", path.display()),
    };
    format!("{code}: {desc}, {syscall} '{}'", path.display())
}

/// Node's `err.message` for a syscall on an fd, which names no path
/// (`writeSync`, `fsyncSync`): `ENOSPC: no space left on device, write`.
pub fn node_fd_message(err: &io::Error, syscall: &str) -> String {
    match err.raw_os_error().and_then(errno_name) {
        Some((code, desc)) => format!("{code}: {desc}, {syscall}"),
        None => format!("{err}, {syscall}"),
    }
}

/// Node's `err.message` for a failed `renameSync(from, to)`:
/// `EISDIR: illegal operation on a directory, rename '<from>' -> '<to>'`.
pub fn node_rename_message(err: &io::Error, from: &Path, to: &Path) -> String {
    format!(
        "{} -> '{}'",
        node_fs_message(err, "rename", from),
        to.display()
    )
}

/// errno → (Node code, libuv description) for the errors a file read/write
/// on a device can plausibly hit.
fn errno_name(errno: i32) -> Option<(&'static str, &'static str)> {
    Some(match errno {
        libc::ENOENT => ("ENOENT", "no such file or directory"),
        libc::EACCES => ("EACCES", "permission denied"),
        libc::EPERM => ("EPERM", "operation not permitted"),
        libc::EISDIR => ("EISDIR", "illegal operation on a directory"),
        libc::ENOTDIR => ("ENOTDIR", "not a directory"),
        libc::EEXIST => ("EEXIST", "file already exists"),
        libc::EROFS => ("EROFS", "read-only file system"),
        libc::ENOSPC => ("ENOSPC", "no space left on device"),
        libc::EIO => ("EIO", "i/o error"),
        libc::EMFILE => ("EMFILE", "too many open files"),
        libc::EBUSY => ("EBUSY", "resource busy or locked"),
        libc::ENAMETOOLONG => ("ENAMETOOLONG", "name too long"),
        libc::ELOOP => ("ELOOP", "too many symbolic links encountered"),
        libc::EXDEV => ("EXDEV", "cross-device link not permitted"),
        libc::EBADF => ("EBADF", "bad file descriptor"),
        libc::ENOTEMPTY => ("ENOTEMPTY", "directory not empty"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_messages() {
        let e = io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(
            node_fs_message(&e, "open", Path::new("/etc/xorgate/cert.crt")),
            "ENOENT: no such file or directory, open '/etc/xorgate/cert.crt'"
        );
        let e = io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(
            node_fs_message(&e, "open", Path::new("/x")),
            "EACCES: permission denied, open '/x'"
        );
    }

    #[test]
    fn null_tape_is_inert() {
        let t = null_tape();
        assert!(!t.enabled());
        t.nmea("$GPGGA");
        t.mqtt_in("a", b"b", true, 1);
        t.close();
        assert!(open_file_tape(Path::new("/nope"), &TapeStart::default()).is_err());
    }
}
