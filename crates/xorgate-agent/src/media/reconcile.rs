//! Hardware reconciler (cm4-support plan, Phase 2) — the port of
//! `ts:src/media/hardware-reconcile.ts` `reconcileHardware` (:77-182), plus
//! `resolveChannelPipeline` (`ts:src/media/pipeline.ts:79-87`).
//!
//! Enumerate cameras (`rpicam-hello --list-cameras`), pair them to the
//! block's `video` entries by CONNECTOR, and write/refresh the pipeline trio
//! per matched camera from the encoder token's templates:
//!
//! - `<key>.gst` — bridged shmsrc consumer the KVS master loads; replaced
//!   only when ABSENT or a RECOGNIZED render (an operator's pipeline is
//!   reported `custom`, never clobbered);
//! - `<key>.camera` — this boot's enumerated identity, always ground-truthed;
//! - `<key>.gst.direct` — the direct pipeline backup, same overwrite rule as
//!   the `.gst` (but an unrecognized one is silently left, not `custom`).
//!
//! Files are read as bytes → lossy UTF-8 (`readFileSync(p, "utf8")`, D4) and
//! written with a plain `writeFileSync` (not atomic, like the TS). Content is
//! compared after JS `trim()`.
//!
//! Two deliberate differences from the TS (support-other-camera-models plan,
//! Phase 0; both leave a healthy IMX290 device byte-identical):
//!
//! - B6: a `.gst.direct` is ours when it is one of our renders FOR THE
//!   CAMERA NAME IT CARRIES (`own_direct_render_camera`), not only for the
//!   camera enumerated now. So after a sensor change the previous sensor's
//!   render is replaced instead of being left behind with a dead
//!   `camera-name=`. An operator's pipeline is still never overwritten.
//! - The mismatch warning compares the enumerated sensor with the DECLARED
//!   one (the port's config.txt line when [`ReconcileOpts::boot_config`] is
//!   given, else the model), and libcamera's variant names
//!   (`imx708_wide`…) are the same sensor. The camera is used anyway, as
//!   before.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use xorgate_core::camera_truth::{
    boot_config_port_on, declared_for, sensor_base, DeclaredSource,
};
use xorgate_core::cameras::EnumeratedCamera;
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::js::trim as js_trim;
use xorgate_core::pipeline_builder::{
    build_bridged_kvs_pipeline, build_direct_kvs_pipeline, is_recognized_bridged_render,
    own_direct_render_camera,
};
use xorgate_core::stream_guard::StreamHardwareStatus;
use xorgate_core::video_capture::{camera_file_for, direct_backup_for, pipeline_file_for};
use xorgate_hal::camera_list::{enumerate_from, run_list_cameras};
use xorgate_hal::exec::RunResult;
use xorgate_hal::io::{null_tape, TapeSink};

use crate::ctx::BoxFut;

/// The `log(level, msg)` callback's level (`"info" | "warn"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileLevel {
    Info,
    Warn,
}

impl ReconcileLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
        }
    }
}

/// `opts.log` — WP1 passes the worker's closure (`[hardware] …` line + a
/// `hardware: …` state event + flush, `worker.ts:137-142`).
pub type ReconcileLog = Arc<dyn Fn(ReconcileLevel, &str) + Send + Sync>;

/// `opts.listExec` — the `rpicam-hello --list-cameras` run (test seam).
pub type ListExec = Arc<dyn Fn() -> BoxFut<'static, RunResult> + Send + Sync>;

/// The real `defaultExec` (`cameras.ts:66-67`), reporting each run to the tape
/// as an `exec` record.
pub fn real_list_exec(tape: Arc<dyn TapeSink>) -> ListExec {
    Arc::new(move || {
        let tape = tape.clone();
        Box::pin(async move { run_list_cameras(tape.as_ref()).await })
    })
}

/// `ReconcileOptions` (`hardware-reconcile.ts:61-67`).
#[derive(Clone)]
pub struct ReconcileOpts {
    pub pipeline_dir: PathBuf,
    /// `shmSocketPathFor(streamKey)` (`recording/config.ts`; WP1 passes it).
    pub shm_socket_path_for: Arc<dyn Fn(&str) -> String + Send + Sync>,
    /// `None` = the real exec without a tape.
    pub list_exec: Option<ListExec>,
    /// `None` = `console[level](`[hardware] ${msg}`)`.
    pub log: Option<ReconcileLog>,
    /// The text of `/boot/firmware/config.txt` (Rust-only): when given, a
    /// port's `dtoverlay=` line declares its sensor ahead of the model for
    /// the mismatch warning. `None` = the model alone, as the TS has it.
    pub boot_config: Option<String>,
    /// The board (`/proc/device-tree/model`), for the `[section]` filters
    /// of that text.
    pub board: xorgate_core::boot_config::Board,
}

/// `ReconcileResult` (`hardware-reconcile.ts:52-59`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileResult {
    /// Verdict per stream key, in the block's `video` order (`Map`).
    pub statuses: IndexMap<String, StreamHardwareStatus>,
    /// Streams whose `.gst` CONTENT changed — their KVS masters must be bounced.
    pub changed_streams: Vec<String>,
    /// What enumeration saw (`None` = could not enumerate).
    pub cameras: Option<Vec<EnumeratedCamera>>,
}

/// `readIfExists` (`hardware-reconcile.ts:69-75`).
fn read_if_exists(p: &str) -> Option<String> {
    std::fs::read(p)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `reconcileHardware(block, opts)` (`hardware-reconcile.ts:77-182`).
///
/// # Errors
///
/// The FIRST failed `mkdirSync` (`:108`) / `writeFileSync` (`:146,160,170`)
/// ends the pass with `Err(<Node err.message>)`, e.g.
/// `EACCES: permission denied, open '<dir>/cam1.camera'` — exactly where the
/// TS throws. Nothing is logged for the failure here (no `[hardware]` line,
/// so no `hardware: …` state event), no partial [`ReconcileResult`] is
/// returned (no `rendered` status, no `changed_streams` entry), and files
/// written earlier in the pass stay on disk, as in the TS. Lines already
/// logged before the failure stay logged.
///
/// Caller contracts (WP1, `worker.ts`):
/// - BOOT (`:427`, top-level `await reconcileHardwareNow()`, before the
///   `unhandledRejection` net at `:656`): the error propagates and the
///   process exits non-zero (systemd restarts it).
/// - CLOUD hardware-block change (`:843-850`): catch it and log
///   `error!("[agent] hardware reconcile failed {msg}")` (console.error's
///   space join); `streamHardware`/guard arming are NOT updated, no master is
///   bounced and `recording.reapply()` is skipped.
pub async fn reconcile_hardware(
    block: &DeviceHardwareBlock,
    opts: ReconcileOpts,
) -> Result<ReconcileResult, String> {
    let log: ReconcileLog = opts.log.clone().unwrap_or_else(|| {
        Arc::new(|level, msg| match level {
            ReconcileLevel::Warn => tracing::warn!("[hardware] {msg}"),
            ReconcileLevel::Info => tracing::info!("[hardware] {msg}"),
        })
    });
    let mut out = ReconcileResult::default();

    let exec = opts
        .list_exec
        .clone()
        .unwrap_or_else(|| real_list_exec(null_tape()));
    let listing = exec().await;
    let Some(cameras) = enumerate_from(block.platform, &listing) else {
        log(
            ReconcileLevel::Warn,
            "camera enumeration unavailable (rpicam-hello missing or libcamera broken) — leaving pipelines untouched",
        );
        return Ok(out);
    };

    // First camera per connector wins.
    let mut by_connector: IndexMap<&str, &EnumeratedCamera> = IndexMap::new();
    for cam in &cameras {
        if let Some(c) = cam.connector.as_deref() {
            by_connector.entry(c).or_insert(cam);
        }
    }
    let unmapped: Vec<&str> = cameras
        .iter()
        .filter(|c| c.connector.is_none())
        .map(|c| c.camera_name.as_str())
        .collect();
    if !unmapped.is_empty() {
        log(
            ReconcileLevel::Warn,
            &format!(
                "enumerated camera(s) on unrecognized path(s): {}",
                unmapped.join(", ")
            ),
        );
    }

    let dir = path_str(&opts.pipeline_dir);
    std::fs::create_dir_all(&opts.pipeline_dir)
        .map_err(|e| xorgate_hal::io::node_fs_message(&e, "mkdir", &opts.pipeline_dir))?;
    let write = |p: &str, content: &str| -> Result<(), String> {
        std::fs::write(p, content)
            .map_err(|e| xorgate_hal::io::node_fs_message(&e, "open", Path::new(p)))
    };

    for video in &block.video {
        let Some(cam) = by_connector.get(video.connector.as_str()).copied() else {
            out.statuses
                .insert(video.key.clone(), StreamHardwareStatus::NoCamera);
            log(
                ReconcileLevel::Warn,
                &format!(
                    "{}: no camera enumerated on {} (expected {}) — stream held down until one appears (check the ribbon + overlays, reboot after install)",
                    video.key, video.connector, video.sensor
                ),
            );
            continue;
        };
        // Declared = the port's config.txt line, then the model. (The
        // enumerated sensor is the hint that makes a line with an overlay
        // outside the catalog count as this port's line.)
        let boot = opts
            .boot_config
            .as_deref()
            .map(|t| {
                boot_config_port_on(
                    t,
                    &video.connector,
                    &[sensor_base(&cam.sensor)],
                    opts.board,
                )
            });
        if let Some(d) = declared_for(boot.as_ref(), &video.sensor)
            .filter(|d| d.differs_from(&cam.sensor))
        {
            let who = match d.source {
                DeclaredSource::Model => "model expects",
                DeclaredSource::BootConfig => "config.txt declares",
            };
            log(
                ReconcileLevel::Warn,
                &format!(
                    "{}: {who} {} on {}, enumerated {} — using it anyway",
                    video.key, d.sensor, video.connector, cam.sensor
                ),
            );
        }

        let sock = (opts.shm_socket_path_for)(&video.key);
        let gst_path = pipeline_file_for(&dir, &video.key);
        let cam_path = camera_file_for(&dir, &video.key);
        let direct_path = direct_backup_for(&dir, &video.key);

        let desired_gst = format!("{}\n", build_bridged_kvs_pipeline(block.encoder, &sock));
        let desired_cam = format!("{}\n", cam.camera_name);
        let desired_direct = format!(
            "{}\n",
            build_direct_kvs_pipeline(block.encoder, &video.key, &cam.camera_name)
        );

        let mut wrote = false;
        let mut custom = false;

        let current_gst = read_if_exists(&gst_path);
        if current_gst
            .as_deref()
            .is_none_or(|c| is_recognized_bridged_render(c, &sock))
        {
            if current_gst.as_deref().map(js_trim) != Some(js_trim(&desired_gst)) {
                write(&gst_path, &desired_gst)?;
                out.changed_streams.push(video.key.clone());
                wrote = true;
            }
        } else {
            custom = true;
            log(
                ReconcileLevel::Info,
                &format!(
                    "{}: .gst is not a recognized render (operator-customized?) — leaving it alone",
                    video.key
                ),
            );
        }

        // .camera is a cache of THIS boot's enumeration — always ground-truthed.
        if read_if_exists(&cam_path).as_deref().map(js_trim) != Some(js_trim(&desired_cam)) {
            write(&cam_path, &desired_cam)?;
            wrote = true;
        }

        // B6: ours for whatever camera the file names (see the module doc).
        let current_direct = read_if_exists(&direct_path);
        if current_direct
            .as_deref()
            .is_none_or(|c| own_direct_render_camera(c, &video.key).is_some())
            && current_direct.as_deref().map(js_trim) != Some(js_trim(&desired_direct))
        {
            write(&direct_path, &desired_direct)?;
            wrote = true;
        }

        let status = if custom {
            StreamHardwareStatus::Custom
        } else if wrote {
            StreamHardwareStatus::Rendered
        } else {
            StreamHardwareStatus::Ok
        };
        // `statuses.set(key, …)`: a repeated key keeps its first position.
        out.statuses.insert(video.key.clone(), status);
        if wrote && !custom {
            log(
                ReconcileLevel::Info,
                &format!(
                    "{}: pipelines rendered ({}, {})",
                    video.key,
                    block.encoder.as_str(),
                    cam.camera_name
                ),
            );
        }
    }

    out.cameras = Some(cameras);
    Ok(out)
}

/// `resolveChannelPipeline(pipelineDir, streamKey, globalFallback)`
/// (`pipeline.ts:79-87`): `<dir>/<key>.gst` when it exists (`existsSync`,
/// directories included), else the global fallback, else `None` (the runtime
/// then uses its built-in default for the source).
pub fn resolve_channel_pipeline(
    pipeline_dir: &Path,
    key: &str,
    global: Option<&Path>,
) -> Option<PathBuf> {
    let per_channel = PathBuf::from(pipeline_file_for(&path_str(pipeline_dir), key));
    if per_channel.exists() {
        return Some(per_channel);
    }
    global.map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `test/pipeline.test.ts` › resolveChannelPipeline (2 it()s, CF44).
    #[test]
    fn pipeline_test_ts_resolve_channel_pipeline() {
        let dir = std::env::temp_dir().join(format!("xg-rcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // "prefers a per-stream <streamKey>.gst file"
        let f = dir.join("cam0.gst");
        std::fs::write(&f, "videotestsrc ! fakesink\n").unwrap();
        assert_eq!(
            resolve_channel_pipeline(&dir, "cam0", None),
            Some(f.clone())
        );
        // "falls back to the global pipeline, then undefined"
        assert_eq!(
            resolve_channel_pipeline(&dir, "cam1", Some(Path::new("/global.gst"))),
            Some(PathBuf::from("/global.gst"))
        );
        assert_eq!(resolve_channel_pipeline(&dir, "cam1", None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
