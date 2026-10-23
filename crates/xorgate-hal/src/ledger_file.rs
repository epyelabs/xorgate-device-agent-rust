//! `commands.json` I/O (Phase 2 WP4): the file halves of `loadLedger` and
//! `claimCommand` (`ts:src/agent/commands.ts:167-218`); the decisions are
//! `xorgate_core::commands::{load_ledger, claim}`.
//!
//! The save is `atomicWriteJson` (`ts:src/lib/atomicWrite.ts:43-70`) step for
//! step — same-dir temp `.commands.json.tmp.<pid>.<n>` (0600) → write →
//! (root only: owner/mode of the existing target) → fsync → close → rename
//! → best-effort dir fsync — re-done here instead of calling
//! [`crate::atomic_write`] because the TS ack reason carries the Node
//! `err.message` of the step that failed (`ledger write failed: EACCES:
//! permission denied, open '<tmp>'`, `worker.ts:1015`), so the error must
//! know its syscall and path.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use xorgate_core::commands::{load_ledger, Ledger};

use crate::atomic_write::{fsync_dir, temp_path_for};
use crate::io::{node_fd_message, node_fs_message, node_rename_message};
use crate::read_lossy::{read_text, FileText};

/// `loadLedger(path)` (`commands.ts:172-194`): `readFileSync(path, "utf8")`
/// (lossy), then the pure parse; absent / unreadable / invalid → empty.
pub fn load(path: &Path) -> Ledger {
    match read_text(path) {
        FileText::Text(t) => load_ledger(Some(&t)),
        FileText::Absent | FileText::Unreadable(_) => load_ledger(None),
    }
}

/// `atomicWriteJson(path, ledger)` (`commands.ts:213-216`). `Err` is the Node
/// `err.message` of the failing step.
pub fn save(path: &Path, ledger: &Ledger) -> Result<(), String> {
    let bytes = serde_json::to_vec(ledger).map_err(|e| e.to_string())?;
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    // The same module-global counter as every other atomic write (TS `tmpCounter`).
    let (dir, tmp) = temp_path_for(path);
    let prior = if is_root() {
        std::fs::metadata(path).ok()
    } else {
        None
    };
    // openSync(tmp, "w", 0o600)
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| node_fs_message(&e, "open", &tmp))?;
    // writeSync / fsyncSync inside try{} finally{ closeSync } — no unlink on
    // these failures in the TS either (only a rename failure unlinks).
    write_and_sync(&f, bytes, prior.as_ref())?;
    drop(f);
    // renameSync, unlink the temp on failure.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(node_rename_message(&e, &tmp, path));
    }
    fsync_dir(&dir);
    Ok(())
}

fn write_and_sync(
    mut f: &File,
    bytes: &[u8],
    prior: Option<&std::fs::Metadata>,
) -> Result<(), String> {
    f.write_all(bytes)
        .map_err(|e| node_fd_message(&e, "write"))?;
    if let Some(m) = prior {
        preserve_ownership(f, m);
    }
    f.sync_all().map_err(|e| node_fd_message(&e, "fsync"))
}

#[allow(unsafe_code)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// `preserveOwnership` (`atomicWrite.ts:84-95`): chown first, then chmod;
/// both best-effort.
#[allow(unsafe_code)]
fn preserve_ownership(f: &File, prior: &std::fs::Metadata) {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    // SAFETY: fchown/fchmod on an fd we own; errors ignored on purpose.
    unsafe {
        let _ = libc::fchown(fd, prior.uid(), prior.gid());
        let _ = libc::fchmod(fd, (prior.mode() & 0o7777) as libc::mode_t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use xorgate_core::commands::claim;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-ledger-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn claim_round_trips_through_the_file() {
        let d = scratch("rt");
        let p = d.join("commands.json");
        assert_eq!(load(&p), Ledger::empty());
        let c = claim(&load(&p), "req-1", 1_750_000_000_000);
        save(&p, &c.ledger).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            r#"{"v":1,"executed":[{"requestId":"req-1","at":1750000000000}]}"#
        );
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!claim(&load(&p), "req-1", 1_750_000_000_001).accepted);
        // corrupt / non-UTF-8 → empty, like the TS catch
        std::fs::write(&p, b"{\"v\":1,\xff").unwrap();
        assert_eq!(load(&p), Ledger::empty());
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "commands.json")
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_errors_carry_the_node_message_of_the_failing_step() {
        let d = scratch("err");
        // Home missing → `openSync(tmp)` ENOENT, the path is the temp file.
        let p = d.join("nope").join("commands.json");
        let e = save(&p, &Ledger::empty()).unwrap_err();
        let tmp = format!(
            "{}/.commands.json.tmp.{}.",
            d.join("nope").display(),
            std::process::id()
        );
        assert!(
            e.starts_with(&format!("ENOENT: no such file or directory, open '{tmp}")),
            "{e}"
        );
        // Target is a directory → rename EISDIR, temp unlinked.
        let p = d.join("commands.json");
        std::fs::create_dir_all(p.join("x")).unwrap();
        let e = save(&p, &Ledger::empty()).unwrap_err();
        assert!(
            e.starts_with("EISDIR: illegal operation on a directory, rename '"),
            "{e}"
        );
        assert!(e.ends_with(&format!(" -> '{}'", p.display())), "{e}");
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != "commands.json" && e.file_name() != "nope")
            .collect();
        assert!(leftovers.is_empty());
        assert_eq!(
            node_fd_message(&io::Error::from_raw_os_error(libc::ENOSPC), "write"),
            "ENOSPC: no space left on device, write"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
