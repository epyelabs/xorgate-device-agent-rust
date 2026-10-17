//! The filesystem walks of the recording disk manager
//! (`src/recording/disk.ts:170-224`): the recursive `scan()` and
//! `pruneEmptySessionDirs()`. The decisions over what they return are core's
//! [`xorgate_core::disk_plan`].
//!
//! Directory order: `readdirSync` goes through libuv's `scandir`, which sorts
//! entries by `strcmp` — so entries are walked in byte order of their names
//! here too (it decides the order of equal-mtime files in the eviction walk).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use xorgate_core::disk_plan::{is_closed_name, is_scanned_name, ScannedFile};

/// `readdirSync(dir, { withFileTypes: true })` sorted like libuv: `(name,
/// file type)` pairs, or `None` when the directory cannot be read. The
/// type is the dirent's (`lstat`-like: a symlink is neither file nor dir).
fn read_dir_sorted(dir: &Path) -> Option<Vec<(std::ffi::OsString, fs::FileType)>> {
    let rd = fs::read_dir(dir).ok()?;
    let mut out: Vec<_> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_type().ok().map(|t| (e.file_name(), t)))
        .collect();
    out.sort_by(|a, b| {
        use std::os::unix::ffi::OsStrExt;
        a.0.as_bytes().cmp(b.0.as_bytes())
    });
    Some(out)
}

/// Node's `stat.mtimeMs`: `sec * 1e3 + nsec / 1e6` as a double.
pub fn mtime_ms(meta: &fs::Metadata) -> f64 {
    meta.mtime() as f64 * 1e3 + meta.mtime_nsec() as f64 / 1e6
}

/// `scan(dir)` (`disk.ts:170-197`): every regular file under `root` except
/// `counters.json` / `session.json` (at any depth), with `statSync` size and
/// mtime; `closed` by suffix. An unreadable/absent directory contributes
/// nothing (tree absent — nothing recorded yet); a file that vanished
/// between readdir and stat is skipped (raced a rotation/unlink).
pub fn scan(root: &Path) -> Vec<ScannedFile> {
    let mut out = Vec::new();
    scan_into(root, &mut out);
    out
}

fn scan_into(dir: &Path, out: &mut Vec<ScannedFile>) {
    let Some(entries) = read_dir_sorted(dir) else {
        return;
    };
    for (name, ty) in entries {
        let path = dir.join(&name);
        if ty.is_dir() {
            scan_into(&path, out);
            continue;
        }
        let name = name.to_string_lossy();
        if !ty.is_file() || !is_scanned_name(&name) {
            continue;
        }
        // statSync follows symlinks; only regular dirents reach here anyway.
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        out.push(ScannedFile {
            path: path.to_string_lossy().into_owned(),
            size: meta.len(),
            mtime_ms: mtime_ms(&meta),
            closed: is_closed_name(&name),
        });
    }
}

/// `pruneEmptySessionDirs()` (`disk.ts:200-224`): for each `<root>/<kind>`
/// directory (`statSync` — follows symlinks), `rmdirSync` every entry; it
/// fails (ENOTEMPTY/ENOTDIR) unless the entry is an empty directory —
/// exactly what is wanted. Best effort throughout. Note the depth: video
/// sessions live at `video/<cam>/<sid>`, so only a drained `<cam>` dir is
/// ever removed there (TS parity).
pub fn prune_empty_session_dirs(root: &Path) {
    let Some(kinds) = read_dir_sorted(root) else {
        return;
    };
    for (kind, _) in kinds {
        let kind_dir = root.join(kind);
        if !fs::metadata(&kind_dir).map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(sessions) = read_dir_sorted(&kind_dir) else {
            continue;
        };
        for (s, _) in sessions {
            let _ = fs::remove_dir(kind_dir.join(s));
        }
    }
}
