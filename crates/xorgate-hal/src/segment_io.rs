//! The fd I/O of `class SegmentWriter` (`src/recording/segments.ts:156-191`):
//! executes the [`SegmentOp`]s core's [`SegmentWriterCore`] emits, in order,
//! under the S2 contract (review-wp6 S2, CF35): the FIRST failed op stops
//! the batch, core is rewound with [`SegmentWriterCore::op_failed`] so it
//! agrees with the disk, and the error (Node's `err.message`) goes back to
//! the caller — the recorder's `[recording] tick failed: …` line.
//!
//! Per op (TS line → Rust):
//! - `Open` — `mkdirSync(dir, {recursive})` (`:157`) → `create_dir_all`;
//!   `openSync(path, "w", 0o600)` (`:161`) → create+truncate mode 0600; the
//!   header `writeSync` (`:166`).
//! - `Append` — `writeSync(fd, json + "\n")` (`:172`); no per-line fsync
//!   (a power cut may cost the tail of the ACTIVE segment only).
//! - `Close` — `fsyncSync` + `closeSync` (`:179-180`), then `unlinkSync`
//!   for a header-only segment (`:185-188`) or [`gzip_finalize`] (`:189`).
//!
//! One deviation, on a path that cannot fail in practice: when the plain
//! fsync throws, the TS keeps its fd and `path` (the throw precedes `fd =
//! null`) while Rust has already dropped (= closed) the handle; core treats
//! it as a failed `Close` (segment closed). See evidence wp2 "ambiguities".

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use xorgate_core::segments::{GzipFinalizeStep, OpFailure, SegmentOp, SegmentWriterCore};

use crate::gzip_finalize::{gzip_finalize, StepError};
use crate::io::{node_fd_message, node_fs_message};

/// Where a fault hook may fail the executor (test seam for disk-full
/// scenarios; production passes none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoPoint {
    /// `mkdirSync` of the session dir.
    Mkdir,
    /// `openSync` of a new plain segment.
    Create,
    /// `writeSync` of a header (on open) or of an appended line.
    Write,
    /// A step of the close / gzip finalize.
    Close(GzipFinalizeStep),
}

/// The fault hook type.
pub type FaultHook = Box<dyn FnMut(IoPoint, &Path) -> io::Result<()> + Send + Sync>;

/// What the executor did, in order — the recorder maps these to the tape's
/// `fs_out` kinds and to `onSegmentClosed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentEvent {
    /// A new plain segment was created (its header written or not).
    Created { path: PathBuf },
    /// The plain segment's fd was fsynced and closed.
    Closed { path: PathBuf },
    /// A header-only segment was unlinked instead of shipped.
    Deleted { path: PathBuf },
    /// `<plain>.gz` is in place (plain removed); `size` = gz bytes.
    Gzipped { gz: PathBuf, size: u64 },
}

/// A failed op: Node's `err.message` for the failing call.
#[derive(Debug)]
pub struct SegmentIoError {
    pub failure: OpFailure,
    pub source: io::Error,
    pub message: String,
}

impl std::fmt::Display for SegmentIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SegmentIoError {}

/// The fd half of `SegmentWriter`: at most one open plain segment.
#[derive(Default)]
pub struct SegmentExecutor {
    open: Option<(String, File)>,
    fault: Option<FaultHook>,
}

impl std::fmt::Debug for SegmentExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentExecutor")
            .field("open", &self.open.as_ref().map(|(p, _)| p))
            .finish()
    }
}

impl SegmentExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// A test executor whose `fault` hook may fail any [`IoPoint`].
    pub fn with_fault(fault: FaultHook) -> Self {
        Self {
            open: None,
            fault: Some(fault),
        }
    }

    /// The path of the open plain segment's fd, if any.
    pub fn open_path(&self) -> Option<&str> {
        self.open.as_ref().map(|(p, _)| p.as_str())
    }

    fn check(&mut self, at: IoPoint, p: &Path) -> io::Result<()> {
        match self.fault.as_mut() {
            Some(f) => f(at, p),
            None => Ok(()),
        }
    }

    /// Execute one call's `ops` in order (S2): on the first failure, rewind
    /// `core` and return the error; `events` gets what happened before it.
    pub fn execute(
        &mut self,
        core: &mut SegmentWriterCore,
        ops: &[SegmentOp],
        events: &mut Vec<SegmentEvent>,
    ) -> Result<(), SegmentIoError> {
        for (i, op) in ops.iter().enumerate() {
            if let Err(e) = self.execute_one(op, events) {
                core.op_failed(ops, i, e.failure);
                return Err(e);
            }
        }
        Ok(())
    }

    fn execute_one(
        &mut self,
        op: &SegmentOp,
        events: &mut Vec<SegmentEvent>,
    ) -> Result<(), SegmentIoError> {
        match op {
            SegmentOp::Open { path, line } => self.open_segment(path, line, events),
            SegmentOp::Append { line, .. } => self.append(line, OpFailure::Append),
            SegmentOp::Close { plain, gz } => self.close(plain, gz.is_some(), events),
        }
    }

    /// `open(now)` (`segments.ts:156-167`).
    fn open_segment(
        &mut self,
        path: &str,
        line: &str,
        events: &mut Vec<SegmentEvent>,
    ) -> Result<(), SegmentIoError> {
        let p = Path::new(path);
        let dir = p.parent().unwrap_or(Path::new("."));
        self.check(IoPoint::Mkdir, dir)
            .and_then(|_| std::fs::create_dir_all(dir))
            .map_err(|e| SegmentIoError {
                failure: OpFailure::OpenMkdir,
                message: node_fs_message(&e, "mkdir", dir),
                source: e,
            })?;
        self.check(IoPoint::Create, p).map_err(|e| SegmentIoError {
            failure: OpFailure::OpenCreate,
            message: node_fs_message(&e, "open", p),
            source: e,
        })?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(p)
            .map_err(|e| SegmentIoError {
                failure: OpFailure::OpenCreate,
                message: node_fs_message(&e, "open", p),
                source: e,
            })?;
        self.open = Some((path.to_string(), file));
        events.push(SegmentEvent::Created {
            path: p.to_path_buf(),
        });
        self.append(line, OpFailure::OpenHeader)
    }

    /// `append(json)` (`segments.ts:169-174`): no-op without an fd.
    fn append(&mut self, line: &str, failure: OpFailure) -> Result<(), SegmentIoError> {
        let Some(path) = self.open.as_ref().map(|(p, _)| PathBuf::from(p)) else {
            return Ok(());
        };
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        let res = self.check(IoPoint::Write, &path).and_then(|_| {
            let (_, f) = self.open.as_mut().expect("checked above");
            f.write_all(&buf)
        });
        res.map_err(|e| SegmentIoError {
            failure,
            message: node_fd_message(&e, "write"),
            source: e,
        })
    }

    /// `rotate()` (`segments.ts:176-191`) after core cleared its `path`.
    fn close(
        &mut self,
        plain: &str,
        has_samples: bool,
        events: &mut Vec<SegmentEvent>,
    ) -> Result<(), SegmentIoError> {
        let p = PathBuf::from(plain);
        let close_err = |e: io::Error, msg: String| SegmentIoError {
            failure: OpFailure::Close,
            message: msg,
            source: e,
        };
        if let Some((_, file)) = self.open.take() {
            let synced = self
                .check(IoPoint::Close(GzipFinalizeStep::FsyncPlain), &p)
                .and_then(|_| file.sync_all());
            drop(file); // closeSync
            if let Err(e) = synced {
                let msg = node_fd_message(&e, "fsync");
                return Err(close_err(e, msg));
            }
            events.push(SegmentEvent::Closed { path: p.clone() });
        }
        if !has_samples {
            // A header-only segment carries no data — dropped, not shipped.
            self.check(IoPoint::Close(GzipFinalizeStep::UnlinkHeaderOnly), &p)
                .and_then(|_| std::fs::remove_file(&p))
                .map_err(|e| {
                    let msg = node_fs_message(&e, "unlink", &p);
                    close_err(e, msg)
                })?;
            events.push(SegmentEvent::Deleted { path: p });
            return Ok(());
        }
        let mut hook = |step: GzipFinalizeStep, at: &Path| -> io::Result<()> {
            match self.fault.as_mut() {
                Some(f) => f(IoPoint::Close(step), at),
                None => Ok(()),
            }
        };
        let gz = gzip_finalize(&p, None, Some(&mut hook)).map_err(|e: StepError| SegmentIoError {
            failure: OpFailure::Close,
            message: e.message,
            source: e.source,
        })?;
        let size = std::fs::metadata(&gz).map(|m| m.len()).unwrap_or(0);
        events.push(SegmentEvent::Gzipped { gz, size });
        Ok(())
    }
}
