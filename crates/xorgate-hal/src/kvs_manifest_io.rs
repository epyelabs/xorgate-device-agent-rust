//! File half of `src/media/manifest.ts`: `loadManifest` (:44-50) and
//! `persistUploadBucket` (:99-115). Decisions live in
//! `xorgate_core::kvs_manifest`; this module only reads (bytes → lossy UTF-8,
//! like `readFileSync(path, "utf8")`) and writes through `atomic_write`.
//!
//! Synchronous on purpose, like the TS (`readFileSync`/`atomicWriteJson`):
//! kvs.json is a few hundred bytes and is touched at boot, on a config
//! message carrying `uploadBucket`, and when the uploader is (re)built.

use std::path::Path;

use xorgate_core::kvs_manifest::{
    parse_manifest_text, persist_upload_bucket_decision, KvsManifest, PersistDecision,
};

use crate::atomic_write::{atomic_write_bytes_traced, AtomicWriteError};

/// `readFileSync(path, "utf8")`: `None` where it would throw (absent,
/// unreadable, a directory, …).
fn read_text(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// `loadManifest(path)` (`manifest.ts:44-84`): never fails; `None` for a
/// missing/corrupt/incomplete manifest.
pub fn load_manifest(path: &Path) -> Option<KvsManifest> {
    parse_manifest_text(&read_text(path)?)
}

/// `persistUploadBucket`'s return value (`"applied" | "unchanged" | "no-manifest"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistOutcome {
    Applied,
    Unchanged,
    NoManifest,
}

impl PersistOutcome {
    /// The TS string literal.
    pub fn as_str(self) -> &'static str {
        match self {
            PersistOutcome::Applied => "applied",
            PersistOutcome::Unchanged => "unchanged",
            PersistOutcome::NoManifest => "no-manifest",
        }
    }
}

/// `persistUploadBucket(path, bucket)` (`manifest.ts:99-115`). A read failure
/// is `NoManifest`; a WRITE failure is returned as `Err` exactly where
/// `atomicWriteJson` throws (the worker's `uploadBucket persist failed: …`
/// catch, `worker.ts:806-810`, is the caller's — WP3; it prints
/// [`AtomicWriteError::node_message`]).
pub fn persist_upload_bucket(
    path: &Path,
    bucket: &str,
) -> Result<PersistOutcome, AtomicWriteError> {
    match persist_upload_bucket_decision(read_text(path).as_deref(), bucket) {
        PersistDecision::NoManifest => Ok(PersistOutcome::NoManifest),
        PersistDecision::Unchanged => Ok(PersistOutcome::Unchanged),
        PersistDecision::Apply { text } => {
            atomic_write_bytes_traced(path, text.as_bytes())?;
            Ok(PersistOutcome::Applied)
        }
    }
}
