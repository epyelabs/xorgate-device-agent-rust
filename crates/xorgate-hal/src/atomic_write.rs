//! Port of `src/lib/atomicWrite.ts` `atomicWriteJson`: temp file in the SAME
//! directory (`.<base>.tmp.<pid>.<n>`, mode 0600) → write → fsync → rename
//! over the target → fsync the directory. When running as root and the target
//! already exists, the temp file is handed the target's owner + mode before
//! the rename (the "sudo freezes settings.json" fix). Best-effort on the
//! directory fsync, exactly like the TS.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Serialize `value` as compact JSON (`JSON.stringify` layout) and write it
/// atomically to `path`.
pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    atomic_write_bytes(path, &bytes)
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let base = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{base}.tmp.{}.{n}", std::process::id()));
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
            .open(&tmp)?;
        let res = (|| {
            f.write_all(bytes)?;
            if let Some(m) = &prior {
                preserve_ownership(&f, m);
            }
            f.sync_all()
        })();
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    fsync_dir(dir);
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
}
