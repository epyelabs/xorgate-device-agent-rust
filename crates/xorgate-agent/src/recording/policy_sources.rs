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

use super::hooks::StreamHardware;
use super::manager::{PowerSampleFn, SampleGroupsFn};
use crate::ctx::{AgentCtx, TelemetrySource};

/// `sampleGroups: async () => withLedSampleGroups(await
/// source.sampleGroups(), ledTelemetryNow())` (`worker.ts:540-545` @
/// 09da38d): the LED leaves join the recorded `system` group, reusing its
/// `updatedMono`; with no system sample a group is synthesised with a fresh
/// mono stamp (CF73). `led` is read at sample time (late-bound).
pub fn sample_groups_with_led_fn(
    source: Arc<dyn TelemetrySource>,
    led: Arc<dyn Fn() -> Option<xorgate_core::led_state::LedTelemetry> + Send + Sync>,
) -> SampleGroupsFn {
    Arc::new(move || {
        let (s, led) = (source.clone(), led.clone());
        Box::pin(async move {
            xorgate_core::led_telemetry::with_led_sample_groups(
                s.sample_groups().await,
                led(),
                |fields| crate::ctx::GroupSample {
                    fields,
                    updated_mono: tokio::time::Instant::now(),
                },
            )
        })
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
///
/// B5 (support-other-camera-models Phase 0, RUST-ONLY): a stream whose port
/// enumerated NO camera is dropped from the set (`stream_hw`,
/// [`StreamHardware::capture_allowed`]). The TS resolves the camera name
/// from the files alone (`<key>.camera`, else the name inside
/// `<key>.gst.direct`), and the reconciler leaves both in place when no
/// camera enumerates, so it launches a capture for a camera that is not
/// there, from the PREVIOUS sensor's name. The filter sits here because this
/// closure is the only source of the capture daemon's camera set
/// (`RecordingManager::apply_video`). `None` = no reconcile verdicts (the
/// tests that drive the TS behaviour): no filter.
pub fn cameras_fn(
    ctx: &AgentCtx,
    stream_hw: Option<Arc<StreamHardware>>,
) -> Arc<dyn Fn() -> Vec<CaptureCamera> + Send + Sync> {
    let pipelines = ctx.paths.pipelines.display().to_string();
    let manifest = ctx.paths.kvs_manifest.clone();
    let settings = ctx.paths.settings.clone();
    // The keys held on the last pass, so the hold is logged once, not on
    // every apply pass.
    let held_before: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
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
        let Some(hw) = &stream_hw else { return cams };
        let (cams, held) = hold_cameras_without_a_sensor(cams, |key| hw.capture_allowed(key));
        let mut before = held_before.lock().unwrap_or_else(|p| p.into_inner());
        let new: Vec<&String> = held.iter().filter(|k| !before.contains(k)).collect();
        if !new.is_empty() {
            // Read only when a hold is about to be said.
            let empty = xorgate_core::camera_sensors::declared_empty_keys(
                xorgate_hal::settings_io::load_settings(&settings)
                    .camera_sensors
                    .as_ref(),
            );
            for key in new {
                if empty.contains(key) {
                    tracing::info!("{}", capture_declared_empty_line(key));
                } else if let Some(sentence) = hw.requirement(key) {
                    warn!("{}", capture_held_requirement_line(key, &sentence));
                } else {
                    warn!("{}", capture_held_warning(key));
                }
            }
        }
        *before = held;
        cams
    })
}

/// B5: split the resolved camera set into the cameras that may be launched
/// and the stream keys held because their port has no camera.
pub fn hold_cameras_without_a_sensor(
    cams: Vec<CaptureCamera>,
    allowed: impl Fn(&str) -> bool,
) -> (Vec<CaptureCamera>, Vec<String>) {
    let (keep, held): (Vec<CaptureCamera>, Vec<CaptureCamera>) =
        cams.into_iter().partition(|c| allowed(&c.stream_key));
    (keep, held.into_iter().map(|c| c.stream_key).collect())
}

/// B5: the line logged once when a stream's capture is held.
pub fn capture_held_warning(stream_key: &str) -> String {
    format!(
        "[video-capture] {stream_key}: no camera enumerated on its port — capture not started (the camera name left in {stream_key}.camera / {stream_key}.gst.direct is stale)"
    )
}

/// The hold of a stream whose declared sensor's requirements the device
/// does not meet (D128): the port's `requirements-missing` sentence
/// (`camera_truth::requirements_sentence`) in place of the stale-name
/// warning, which pointed at hardware.
pub fn capture_held_requirement_line(stream_key: &str, sentence: &str) -> String {
    format!(
        "[video-capture] {stream_key}: no camera enumerated on its port — capture not started: {sentence}"
    )
}

/// The hold of a stream whose port the `cameraSensors` setting declares
/// empty: on purpose, so no stale-name warning.
pub fn capture_declared_empty_line(stream_key: &str) -> String {
    format!(
        "[video-capture] {stream_key}: its port is declared empty by the camera sensor setting — capture not started"
    )
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
