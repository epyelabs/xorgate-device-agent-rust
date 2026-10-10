//! Port of `src/lib/atomicWrite.ts` `atomicWriteJson`: temp file in the SAME
//! directory (`.<base>.tmp.<pid>.<n>`, mode 0600) → write → fsync → rename
//! over the target → fsync the directory. When running as root and the target
//! already exists, the temp file is handed the target's owner + mode before
//! the rename (the "sudo freezes settings.json" fix). Best-effort on the
//! directory fsync, exactly like the TS.
//!
//! [`atomic_write_bytes_traced`] keeps the failing step so callers can print
//! Node's `err.message` for it ([`AtomicWriteError::node_message`]): the TS
//! strings `cloud config apply failed: …`, `uploadBucket persist failed: …`,
//! `hardware block persist failed: …` and the `xorgate set` failure line all
//! embed it, and two of them reach state.json `recentEvents` (review-wp3 #1).
//! Only the `<pid>.<n>` of the temp name can differ from the TS.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::io::{node_fd_message, node_fs_message, node_rename_message};

/// The module-global `tmpCounter` of `atomicWrite.ts:41`.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `(dir, <dir>/.<base>.tmp.<pid>.<n>)` for a write to `path`
/// (`atomicWrite.ts:44-45`), advancing the shared counter.
pub(crate) fn temp_path_for(path: &Path) -> (PathBuf, PathBuf) {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let base = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{base}.tmp.{}.{n}", std::process::id()));
    (dir, tmp)
}

/// The step of an atomic write that failed, with what Node's `err.message`
/// names for it (`atomicWrite.ts:47-60`).
#[derive(Debug)]
pub struct AtomicWriteError {
    /// The failing `*Sync` call: `open`, `write`, `fsync` or `rename`.
    pub syscall: &'static str,
    /// The temp file (`.<base>.tmp.<pid>.<n>`).
    pub tmp: PathBuf,
    /// The target.
    pub dest: PathBuf,
    pub source: io::Error,
}

impl AtomicWriteError {
    /// Node's `err.message` (tsx/node 20+24-probed):
    /// `ENOENT: no such file or directory, open '<dir>/.settings.json.tmp.<pid>.<n>'`,
    /// `EISDIR: illegal operation on a directory, rename '<tmp>' -> '<dest>'`,
    /// `ENOSPC: no space left on device, write` (fd calls carry no path).
    pub fn node_message(&self) -> String {
        match self.syscall {
            "open" => node_fs_message(&self.source, "open", &self.tmp),
            "rename" => node_rename_message(&self.source, &self.tmp, &self.dest),
            fd_call => node_fd_message(&self.source, fd_call),
        }
    }
}

impl std::fmt::Display for AtomicWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.node_message())
    }
}

impl std::error::Error for AtomicWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<AtomicWriteError> for io::Error {
    fn from(e: AtomicWriteError) -> io::Error {
        e.source
    }
}

/// Serialize `value` as compact JSON (`JSON.stringify` layout) and write it
/// atomically to `path`.
pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    atomic_write_bytes(path, &bytes)
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    Ok(atomic_write_bytes_traced(path, bytes)?)
}

/// [`atomic_write_bytes`] that reports which step failed.
pub fn atomic_write_bytes_traced(path: &Path, bytes: &[u8]) -> Result<(), AtomicWriteError> {
    let (dir, tmp) = temp_path_for(path);
    let fail = |syscall: &'static str, tmp: &Path| {
        let (tmp, dest) = (tmp.to_path_buf(), path.to_path_buf());
        move |source| AtomicWriteError {
            syscall,
            tmp,
            dest,
            source,
        }
    };
    // Root only, and only when there is an existing owner to preserve.
    let prior = if is_root() {
        std::fs::metadata(path).ok()
    } else {
        None
    };
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(fail("open", &tmp))?;
        let res = f
            .write_all(bytes)
            .map_err(fail("write", &tmp))
            .and_then(|()| {
                if let Some(m) = &prior {
                    preserve_ownership(&f, m);
                }
                f.sync_all().map_err(fail("fsync", &tmp))
            });
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail("rename", &tmp)(e));
    }
    fsync_dir(&dir);
    Ok(())
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// chown first (clears setuid/setgid bits), then chmod; both best-effort.
fn preserve_ownership(f: &File, prior: &std::fs::Metadata) {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    // SAFETY: fchown/fchmod on an open fd we own; errors are ignored on purpose.
    unsafe {
        let _ = libc::fchown(fd, prior.uid(), prior.gid());
        let _ = libc::fchmod(fd, (prior.mode() & 0o7777) as libc::mode_t);
    }
}

/// Best-effort fsync of a directory so a rename survives a power loss.
pub fn fsync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_compact_json_atomically_with_0600() {
        let dir = std::env::temp_dir().join(format!("xg-aw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("state.json");
        atomic_write_json(&p, &serde_json::json!({"a": 1, "b": [1, 2]})).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), r#"{"a":1,"b":[1,2]}"#);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // No temp turds left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "state.json")
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-awe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The temp name is `<dir>/.<base>.tmp.<pid>.<n>`; returns `<n>`.
    fn tmp_counter_of(msg: &str, dir: &Path, base: &str) -> u64 {
        let prefix = format!("{}/.{base}.tmp.{}.", dir.display(), std::process::id());
        let start = msg.find(&prefix).expect(msg) + prefix.len();
        let digits: String = msg[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().expect(msg)
    }

    /// Node 20 (linux) + 24 (darwin) probe, `aw-probe.mjs` in integration.md:
    /// `ENOENT: no such file or directory, open '/w/nope/.settings.json.tmp.1.0'`.
    #[test]
    fn open_failure_names_the_temp_path() {
        let d = scratch("open");
        let p = d.join("nope").join("settings.json");
        let e = atomic_write_bytes_traced(&p, b"{}").unwrap_err();
        assert_eq!(e.syscall, "open");
        let msg = e.node_message();
        let n = tmp_counter_of(&msg, &d.join("nope"), "settings.json");
        assert_eq!(
            msg,
            format!(
                "ENOENT: no such file or directory, open '{}/nope/.settings.json.tmp.{}.{n}'",
                d.display(),
                std::process::id()
            )
        );
        assert_eq!(e.to_string(), msg);
        assert_eq!(io::Error::from(e).raw_os_error(), Some(libc::ENOENT));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Probe: `EISDIR: illegal operation on a directory, rename
    /// '/w/isdir/.settings.json.tmp.1.2' -> '/w/isdir/settings.json'`; the temp
    /// file is unlinked (`atomicWrite.ts:56-63`).
    #[test]
    fn rename_failure_names_both_paths_and_leaves_no_temp() {
        let d = scratch("rename");
        let p = d.join("settings.json");
        std::fs::create_dir_all(p.join("child")).unwrap();
        let e = atomic_write_bytes_traced(&p, b"{}").unwrap_err();
        assert_eq!(e.syscall, "rename");
        let msg = e.node_message();
        let n = tmp_counter_of(&msg, &d, "settings.json");
        let tmp = format!(
            "{}/.settings.json.tmp.{}.{n}",
            d.display(),
            std::process::id()
        );
        // Linux: EISDIR (a non-empty directory may also give ENOTEMPTY/EEXIST
        // on other kernels; the probe and the CI target are Linux).
        let code = if cfg!(target_os = "linux") {
            "EISDIR: illegal operation on a directory"
        } else {
            msg.split(", rename").next().unwrap()
        };
        assert_eq!(msg, format!("{code}, rename '{tmp}' -> '{}'", p.display()));
        assert!(!Path::new(&tmp).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `writeSync`/`fsyncSync` act on an fd: Node prints no path
    /// (probe: `EBADF: bad file descriptor, write`).
    #[test]
    fn fd_failures_carry_no_path() {
        for (syscall, errno, want) in [
            (
                "write",
                libc::ENOSPC,
                "ENOSPC: no space left on device, write",
            ),
            ("fsync", libc::EIO, "EIO: i/o error, fsync"),
        ] {
            let e = AtomicWriteError {
                syscall,
                tmp: PathBuf::from("/x/.a.tmp.1.0"),
                dest: PathBuf::from("/x/a"),
                source: io::Error::from_raw_os_error(errno),
            };
            assert_eq!(e.node_message(), want);
        }
    }
}
