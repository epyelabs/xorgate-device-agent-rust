//! The worker-side closures the recording manager consumes
//! (`worker.ts:439-501`): `sampleGroups`, `powerSample`, the boot policy's
//! readers (with the `gnss` view) and the capture camera set / encoder.
//!
//! The recorder samples through these whether or not MQTT is connected, so
//! the gps collector's `sample()` — and with it `gnss-sky.json`'s 15 s
//! cadence — keeps running through an outage (CF34).

use std::sync::Arc;

use tracing::warn;
use xorgate_core::boot_policy::BootDeferSources;
use xorgate_core::hardware_block::EncoderToken;
use xorgate_core::video_capture::{resolve_bridged_cameras, CaptureCamera, KvsChannel};
use xorgate_hal::boot_sources::FileBootSources;
use xorgate_hal::kvs_manifest_io::load_manifest;
use xorgate_hal::read_lossy::read_text;

use super::manager::{PowerSampleFn, SampleGroupsFn};
use crate::ctx::{AgentCtx, TelemetrySource};

/// `sampleGroups: () => source.sampleGroups()` (`worker.ts:476`).
pub fn sample_groups_fn(source: Arc<dyn TelemetrySource>) -> SampleGroupsFn {
    Arc::new(move || {
        let s = source.clone();
        Box::pin(async move { s.sample_groups().await })
    })
}

/// `powerSample: source.samplePower ? () => source.samplePower!() : undefined`
/// (`worker.ts:479`): the sim source has no sampler → shedding inert.
pub fn power_sample_fn(source: Arc<dyn TelemetrySource>, real: bool) -> Option<PowerSampleFn> {
    real.then(|| -> PowerSampleFn {
        Arc::new(move || {
            let s = source.clone();
            Box::pin(async move { s.sample_power().await })
        })
    })
}

/// `new BootDeferPolicy({ gnss: source.gnssView })` (`manager.ts:237` +
/// `worker.ts:482`): device-tree budget, `LTE_TELEMETRY_FILE`, GNSS view
/// (`None` from a source without a gps collector — the gate never arms).
pub fn boot_sources(
    ctx: &AgentCtx,
    source: Arc<dyn TelemetrySource>,
) -> Box<dyn BootDeferSources + Send> {
    Box::new(FileBootSources::new(
        ctx.paths.lte_telemetry.clone(),
        ctx.tape.clone(),
        Some(Arc::new(move || source.gnss_view())),
    ))
}

/// `cameras: () => resolveBridgedCameras(pipelineDir, loadManifest(...)?.channels ?? [])`
/// (`worker.ts:446-450`), re-resolved on every apply pass; each skipped
/// bridged stream warns `[video-capture] …` (`video-capture.ts:105-108`).
pub fn cameras_fn(ctx: &AgentCtx) -> Arc<dyn Fn() -> Vec<CaptureCamera> + Send + Sync> {
    let pipelines = ctx.paths.pipelines.display().to_string();
    let manifest = ctx.paths.kvs_manifest.clone();
    Arc::new(move || {
        let channels: Vec<KvsChannel> = load_manifest(&manifest)
            .map(|m| {
                m.channels
                    .into_iter()
                    .map(|c| KvsChannel {
                        stream_key: c.stream_key,
                        channel_name: c.channel_name,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (cams, warnings) = resolve_bridged_cameras(&pipelines, &channels, &|p: &str| {
            read_text(std::path::Path::new(p)).ok()
        });
        for w in warnings {
            warn!("{w}");
        }
        cams
    })
}

/// `encoder: () => hardwareBlock?.encoder ?? "sw-h264"` (`worker.ts:453`).
pub fn encoder_fn(ctx: &AgentCtx) -> Arc<dyn Fn() -> EncoderToken + Send + Sync> {
    let hw = ctx.hardware.clone();
    Arc::new(move || {
        hw.borrow()
            .as_ref()
            .map(|b| b.encoder)
            .unwrap_or(EncoderToken::SwH264)
    })
}
