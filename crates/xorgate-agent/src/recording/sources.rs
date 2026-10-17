//! The uploader's segment sources: closed telemetry segments
//! (`src/recording/telemetry-source.ts`) and closed video fragment pairs
//! (`src/recording/video-source.ts`) @ `89b5c64`. The key/name rules are pure
//! (`xorgate_core::upload_keys`); this file owns the directory walks and the
//! video scan's housekeeping (orphan sidecars, drained-session reap).
//!
//! The walks are synchronous like the TS `readdirSync`/`statSync` (a few
//! small directories per pick); the uploader calls them from its own task.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xorgate_core::sidecar::SESSION_MANIFEST_NAME;
use xorgate_core::upload_keys::{
    is_fragment_name, js_string_cmp, json_to_mp4, mp4_to_json, ms_from_timespec,
    reaped_by_session_reap, should_reap_drained_session, telemetry_s3_key, video_key_for,
    video_listing_cmp, video_s3_key, SIDECAR_CONTENT_TYPE, TELEMETRY_CLOSED_SUFFIX,
    TELEMETRY_CONTENT_TYPE, TELEMETRY_PRIORITY, VIDEO_CONTENT_TYPE, VIDEO_PRIORITY,
};

/// The companion object PUT immediately before the main file
/// (`UploadSegment.sidecar`, `uploader.ts:30`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarRef {
    pub path: PathBuf,
    pub s3_key: String,
    pub content_type: String,
}

/// `UploadSegment` (`uploader.ts:19-31`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSegment {
    /// Absolute path of the closed segment file.
    pub path: PathBuf,
    /// `<session>/<file>` (telemetry) or `<streamKey>/<session>/<file>` (video).
    pub name: String,
    pub sidecar: Option<SidecarRef>,
}

/// `SegmentSource` (`uploader.ts:33-43`).
///
/// Frozen names (phase-3 §Interfaces): `priority`, `list` (= `listClosed`),
/// `on_uploaded`. `kind`, `content_type` and `s3_key_for` are the TS members
/// the drain needs (`uploader.ts:145,176`); `on_uploaded` has no TS
/// counterpart (the manager's hook is `UploaderOpts::on_uploaded`) and is a
/// no-op for both real sources.
pub trait SegmentSource: Send + Sync {
    /// `"telemetry"` | `"video"`.
    fn kind(&self) -> &str;
    /// Lower uploads first across sources.
    fn priority(&self) -> u8;
    /// Main-object content type.
    fn content_type(&self) -> &str;
    /// Closed segments, oldest-first; re-scanned before every pick.
    fn list(&self) -> Vec<UploadSegment>;
    /// S3 key, or `None` when the name does not parse (the uploader skips it).
    fn s3_key_for(&self, seg: &UploadSegment) -> Option<String>;
    /// After the unit's PUTs + local deletes (no-op by default).
    fn on_uploaded(&self, _seg: &UploadSegment) {}
}

/// `readdirSync(dir)` as strings (node decodes names as UTF-8 with U+FFFD,
/// and joins THAT string back into paths — so a non-UTF-8 name is listed and
/// later fails its read, exactly like the TS).
fn read_dir_names(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        out.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(out)
}

/// `readdirSync(dir).sort()` (UTF-16 code-unit order).
fn read_dir_sorted(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut names = read_dir_names(dir)?;
    names.sort_by(|a, b| js_string_cmp(a, b));
    Ok(names)
}

/// `statSync(p).isDirectory()` (follows symlinks); errors are `None`.
fn is_dir(p: &Path) -> Option<bool> {
    std::fs::metadata(p).ok().map(|m| m.is_dir())
}

/// `statSync(p).mtimeMs`.
fn mtime_ms(p: &Path) -> Option<f64> {
    std::fs::metadata(p)
        .ok()
        .map(|m| ms_from_timespec(m.mtime(), m.mtime_nsec()))
}

struct TelemetrySource {
    device_id: String,
    root: PathBuf,
}

impl SegmentSource for TelemetrySource {
    fn kind(&self) -> &str {
        "telemetry"
    }

    fn priority(&self) -> u8 {
        TELEMETRY_PRIORITY
    }

    fn content_type(&self) -> &str {
        TELEMETRY_CONTENT_TYPE
    }

    /// `listClosed()` (`telemetry-source.ts:21-44`): every `*.jsonl.gz` under
    /// each session dir, sessions and names in plain sorted order (UUIDv7 +
    /// zero-padded seq ⇒ chronological). The active `.jsonl` never matches.
    fn list(&self) -> Vec<UploadSegment> {
        let Ok(sessions) = read_dir_sorted(&self.root) else {
            return Vec::new(); // no tree yet
        };
        let mut out = Vec::new();
        for session in sessions {
            let dir = self.root.join(&session);
            if is_dir(&dir) != Some(true) {
                continue;
            }
            let Ok(names) = read_dir_sorted(&dir) else {
                continue;
            };
            for name in names {
                if !name.ends_with(TELEMETRY_CLOSED_SUFFIX) {
                    continue;
                }
                out.push(UploadSegment {
                    path: dir.join(&name),
                    name: format!("{session}/{name}"),
                    sidecar: None,
                });
            }
        }
        out
    }

    /// `s3KeyFor` (`telemetry-source.ts:45-51`).
    fn s3_key_for(&self, seg: &UploadSegment) -> Option<String> {
        telemetry_s3_key(&self.device_id, &seg.name)
    }
}

/// `createTelemetrySource(deviceId, telemetryRoot)` (`telemetry-source.ts:16`).
pub fn telemetry_source(device_id: &str, telemetry_root: &Path) -> Arc<dyn SegmentSource> {
    Arc::new(TelemetrySource {
        device_id: device_id.to_string(),
        root: telemetry_root.to_path_buf(),
    })
}

/// `isSessionActive` input: the session ids a live capture daemon owns.
pub type ActiveSessionIds = Arc<dyn Fn() -> HashSet<String> + Send + Sync>;

/// `createVideoSource(deviceId, videoRoot, opts)` inputs (`video-source.ts:37-48`).
pub struct VideoSourceOpts {
    pub device_id: String,
    pub video_root: PathBuf,
    /// `isSessionActive(id)` = `active_session_ids().contains(id)` (the
    /// manager reads the CURRENT capture supervisor each call, `manager.ts:255`).
    pub active_session_ids: ActiveSessionIds,
    /// `now` override (tests); default `Date.now()`.
    pub now_ms: Option<Arc<dyn Fn() -> f64 + Send + Sync>>,
}

struct VideoSource {
    device_id: String,
    root: PathBuf,
    active: ActiveSessionIds,
    now_ms: Arc<dyn Fn() -> f64 + Send + Sync>,
}

/// `reapSession(dir, names)` (`video-source.ts:59-74`): unlink the manifest +
/// every `.json`, then `rmdir` (non-json stragglers keep the dir for the disk
/// manager). Best-effort.
fn reap_session(dir: &Path, names: &[String]) {
    for n in names {
        if reaped_by_session_reap(n) {
            let _ = std::fs::remove_file(dir.join(n));
        }
    }
    let _ = std::fs::remove_dir(dir);
}

impl SegmentSource for VideoSource {
    fn kind(&self) -> &str {
        "video"
    }

    fn priority(&self) -> u8 {
        VIDEO_PRIORITY // telemetry (0) always drains first
    }

    fn content_type(&self) -> &str {
        VIDEO_CONTENT_TYPE
    }

    /// `listClosed()` (`video-source.ts:80-173`), in TS order per session dir:
    /// orphan sidecars unlinked; a dir with no mp4 reaped when inactive and
    /// its manifest |age| ≥ 10 min; else one entry per sidecar'd fragment.
    /// Sorted by mp4 mtime, then name.
    fn list(&self) -> Vec<UploadSegment> {
        let Ok(stream_keys) = read_dir_sorted(&self.root) else {
            return Vec::new(); // no video tree yet
        };
        let mut out: Vec<(UploadSegment, f64)> = Vec::new();
        for stream_key in stream_keys {
            let stream_dir = self.root.join(&stream_key);
            if is_dir(&stream_dir) != Some(true) {
                continue;
            }
            let Ok(sessions) = read_dir_sorted(&stream_dir) else {
                continue;
            };
            for session_id in sessions {
                let dir = stream_dir.join(&session_id);
                if is_dir(&dir) != Some(true) {
                    continue;
                }
                // `readdirSync(dir)` — unsorted, as the TS (order is irrelevant:
                // the output is sorted below).
                let Ok(names) = read_dir_names(&dir) else {
                    continue;
                };
                // `new Set(names.filter(n => n.endsWith(".mp4")))`, in readdir order.
                let mut mp4s: Vec<&String> = Vec::new();
                for n in &names {
                    if n.ends_with(".mp4") && !mp4s.contains(&n) {
                        mp4s.push(n);
                    }
                }

                // Orphan sidecars (`:111-120`).
                for n in &names {
                    if !n.ends_with(".json") || n == SESSION_MANIFEST_NAME {
                        continue;
                    }
                    let pair = json_to_mp4(n);
                    if !mp4s.iter().any(|m| **m == pair) {
                        let _ = std::fs::remove_file(dir.join(n));
                    }
                }

                if mp4s.is_empty() {
                    let active = (self.active)().contains(&session_id);
                    // `statSync(manifest).mtimeMs` only for an inactive session.
                    if !active {
                        let manifest = mtime_ms(&dir.join(SESSION_MANIFEST_NAME));
                        if should_reap_drained_session(false, (self.now_ms)(), manifest) {
                            reap_session(&dir, &names);
                        }
                    }
                    continue;
                }

                for mp4 in mp4s {
                    if !is_fragment_name(mp4) {
                        continue;
                    }
                    let sidecar_name = mp4_to_json(mp4);
                    if !names.contains(&sidecar_name) {
                        continue; // active or not yet swept
                    }
                    let mp4_path = dir.join(mp4);
                    let Some(mtime) = mtime_ms(&mp4_path) else {
                        continue; // raced an eviction
                    };
                    let s3_key = video_key_for(&self.device_id, &stream_key, &session_id, mp4);
                    out.push((
                        UploadSegment {
                            path: mp4_path,
                            name: format!("{stream_key}/{session_id}/{mp4}"),
                            // unparseable name → s3_key_for also None; uploader skips
                            sidecar: s3_key.map(|k| SidecarRef {
                                path: dir.join(&sidecar_name),
                                s3_key: mp4_to_json(&k),
                                content_type: SIDECAR_CONTENT_TYPE.to_string(),
                            }),
                        },
                        mtime,
                    ));
                }
            }
        }
        // V8's sort is stable; so is `sort_by`.
        out.sort_by(|(a, am), (b, bm)| video_listing_cmp(*am, &a.name, *bm, &b.name));
        out.into_iter().map(|(s, _)| s).collect()
    }

    /// `s3KeyFor` (`video-source.ts:174-178`).
    fn s3_key_for(&self, seg: &UploadSegment) -> Option<String> {
        video_s3_key(&self.device_id, &seg.name)
    }
}

/// `createVideoSource(deviceId, videoRoot, opts)` (`video-source.ts:44-180`).
pub fn video_source(opts: VideoSourceOpts) -> Arc<dyn SegmentSource> {
    Arc::new(VideoSource {
        device_id: opts.device_id,
        root: opts.video_root,
        active: opts.active_session_ids,
        now_ms: opts
            .now_ms
            .unwrap_or_else(|| Arc::new(|| crate::now_ms() as f64)),
    })
}
