//! `gzipFinalize` (`src/recording/segments.ts:198-213`): gzip a plain
//! segment into `<plain>.gz` through `${gz}.tmp.${pid}` (write → fsync →
//! close → rename → fsync dir) and delete the plain file — the step list is
//! core's [`GZIP_FINALIZE_STEPS`] / [`ORPHAN_GZIP_STEPS`] (review-wp6 S3).
//!
//! Gzip bytes (P6): `flate2` at level 6 with the header fields node's
//! `zlib.gzipSync` writes (mtime 0, XFL 0, OS 3 = Unix). The deflate stream
//! itself may differ from zlib's (miniz_oxide backend) — accepted divergence
//! D24: the decompressed bytes are identical and the backend only ever reads
//! the decompressed stream.
//!
//! Errors carry Node's `err.message` for the failing `*Sync` call, because
//! the TS prints it (`[recording] tick failed: …`, `orphan finalize failed
//! for …: …`).
//!
//! [`GZIP_FINALIZE_STEPS`]: xorgate_core::segments::GZIP_FINALIZE_STEPS
//! [`ORPHAN_GZIP_STEPS`]: xorgate_core::segments::ORPHAN_GZIP_STEPS

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use flate2::{Compression, GzBuilder};
use xorgate_core::segments::{gz_temp_path, GzipFinalizeStep};

use crate::io::{node_fd_message, node_fs_message, node_rename_message};

/// A failed step and Node's `err.message` for it.
#[derive(Debug)]
pub struct StepError {
    pub step: GzipFinalizeStep,
    pub source: io::Error,
    /// What `(err as Error).message` reads in the TS.
    pub message: String,
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// A test seam: called before every step with the path it acts on; an `Err`
/// fails that step exactly as the syscall would (disk-full scenarios).
pub type StepFault<'a> = &'a mut dyn FnMut(GzipFinalizeStep, &Path) -> io::Result<()>;

/// node `zlib.gzipSync(data)`: default level (6), header mtime 0, OS 3.
pub fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    let mut enc = GzBuilder::new()
        .mtime(0)
        .operating_system(3)
        .write(Vec::with_capacity(data.len() / 4 + 64), Compression::new(6));
    enc.write_all(data).expect("in-memory gzip");
    enc.finish().expect("in-memory gzip")
}

fn fs_err(step: GzipFinalizeStep, e: io::Error, syscall: &str, path: &Path) -> StepError {
    StepError {
        step,
        message: node_fs_message(&e, syscall, path),
        source: e,
    }
}

fn fd_err(step: GzipFinalizeStep, e: io::Error, syscall: &str) -> StepError {
    StepError {
        step,
        message: node_fd_message(&e, syscall),
        source: e,
    }
}

/// `readFileSync(plain)` (`segments.ts:199`): `open '<path>'` on an open
/// failure, a bare `read` on a read failure (e.g. EISDIR), as Node names them.
pub fn read_file_node(path: &Path, step: GzipFinalizeStep) -> Result<Vec<u8>, StepError> {
    let mut f = File::open(path).map_err(|e| fs_err(step, e, "open", path))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| fd_err(step, e, "read"))?;
    Ok(buf)
}

/// `gzipFinalize(plain, content?)` (`segments.ts:198-213`): returns the
/// `.gz` path. `content = None` reads the plain file first (the rotation
/// path); `Some` is the orphan sweep's torn-line-trimmed bytes. On a failure
/// the temp file is closed but NOT removed (the TS `finally` only closes);
/// the next boot's orphan sweep unlinks it (`.gz.tmp.`).
pub fn gzip_finalize(
    plain: &Path,
    content: Option<&[u8]>,
    fault: Option<StepFault<'_>>,
) -> Result<PathBuf, StepError> {
    let mut fault = fault;
    let mut check = |step: GzipFinalizeStep, p: &Path| -> io::Result<()> {
        match fault.as_mut() {
            Some(f) => f(step, p),
            None => Ok(()),
        }
    };
    let owned;
    let data: &[u8] = match content {
        Some(c) => c,
        None => {
            check(GzipFinalizeStep::ReadPlain, plain)
                .map_err(|e| fs_err(GzipFinalizeStep::ReadPlain, e, "open", plain))?;
            owned = read_file_node(plain, GzipFinalizeStep::ReadPlain)?;
            &owned
        }
    };
    let plain_s = plain.to_string_lossy();
    let gz = PathBuf::from(format!("{plain_s}.gz"));
    let tmp = PathBuf::from(gz_temp_path(&gz.to_string_lossy(), std::process::id()));

    check(GzipFinalizeStep::OpenTemp, &tmp)
        .map_err(|e| fs_err(GzipFinalizeStep::OpenTemp, e, "open", &tmp))?;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| fs_err(GzipFinalizeStep::OpenTemp, e, "open", &tmp))?;
    // try { writeSync; fsyncSync } finally { closeSync } — the drop closes.
    let body = gzip_bytes(data);
    check(GzipFinalizeStep::WriteGzip, &tmp)
        .and_then(|_| f.write_all(&body))
        .map_err(|e| fd_err(GzipFinalizeStep::WriteGzip, e, "write"))?;
    check(GzipFinalizeStep::FsyncTemp, &tmp)
        .and_then(|_| f.sync_all())
        .map_err(|e| fd_err(GzipFinalizeStep::FsyncTemp, e, "fsync"))?;
    drop(f);

    check(GzipFinalizeStep::RenameTemp, &tmp)
        .and_then(|_| std::fs::rename(&tmp, &gz))
        .map_err(|e| StepError {
            step: GzipFinalizeStep::RenameTemp,
            message: node_rename_message(&e, &tmp, &gz),
            source: e,
        })?;
    // fsyncDir(dirname(plain)): best effort, never fails the finalize.
    crate::atomic_write::fsync_dir(plain.parent().unwrap_or(Path::new(".")));
    check(GzipFinalizeStep::UnlinkPlain, plain)
        .and_then(|_| std::fs::remove_file(plain))
        .map_err(|e| fs_err(GzipFinalizeStep::UnlinkPlain, e, "unlink", plain))?;
    Ok(gz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_gzip_header() {
        let gz = gzip_bytes(b"{\"a\":1}\n");
        // 1f 8b, CM 8, FLG 0, MTIME 0, XFL 0 (level 6), OS 3.
        assert_eq!(&gz[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3]);
        let mut d = flate2::read::GzDecoder::new(&gz[..]);
        let mut out = String::new();
        d.read_to_string(&mut out).unwrap();
        assert_eq!(out, "{\"a\":1}\n");
    }
}
