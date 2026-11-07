//! Test / rig helpers that are not agent behaviour (`#[doc(hidden)]`).
//!
//! [`write_executable`]: tests that fake a helper binary (`bluetoothctl`,
//! `timedatectl`, `python3`, a KVS master …) write a script and then exec it
//! from a MULTI-THREADED test binary. Writing it in-process (`fs::write` +
//! `chmod`) is racy on Linux: while this thread holds the script's write fd,
//! another test thread may fork; that child inherits the fd (O_CLOEXEC only
//! closes it at the child's exec), and until it execs, the script's inode
//! has a writer, so `execve` of the script fails with ETXTBSY ("Text file
//! busy"). A rename does not help (the leaked fd is on the same inode).
//!
//! The fix: never open the script for writing in THIS process. A short-lived
//! `/bin/sh` child writes it to a temp name beside the target, chmods it and
//! renames it into place; that child is single-threaded and has exited
//! before we return, so no writer can survive anywhere. The rename also makes
//! re-writing a shim that a child is executing right now safe (opening a
//! running text file for write is itself ETXTBSY).

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write `body` to `path` as a 0755 executable without this process ever
/// holding a writable fd to it (see the module doc). Overwrites atomically.
/// A test/rig helper (`#[doc(hidden)]` module), not an agent spawn site.
#[allow(clippy::disallowed_methods)]
pub fn write_executable(path: &Path, body: &str) -> io::Result<()> {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"t="$1.wx.$$" && printf '%s' "$2" > "$t" && chmod 755 "$t" && mv -f "$t" "$1""#)
        .arg("write_executable")
        .arg(path)
        .arg(body)
        // Tests point PATH at their shim dirs only (session_io,
        // collectors_imu): `chmod` / `mv` must not depend on it.
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "write_executable {}: {} {}",
            path.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: test harness spawns, not the agent
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_body_verbatim_mode_0755_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("xg-wx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("fake tool");
        // printf metacharacters, quotes, a backslash and no trailing newline
        // all survive byte for byte.
        let body = "#!/bin/sh\nprintf '%s|%s\\n' '100% \"q\" \\' \"$1\"\nexit 3";
        write_executable(&p, body).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), body);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
        let out = Command::new(&p).arg("x").output().unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "100% \"q\" \\|x\n");
        write_executable(&p, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(Command::new(&p).status().unwrap().success());
        // No temp left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(write_executable(&dir.join("missing/x"), "#!/bin/sh\n").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
