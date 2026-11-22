//! The per-port camera report (support-other-camera-models plan, Phase 0;
//! RUST-ONLY, no TS counterpart): for every camera the device model
//! declares, what is declared, what config.txt says, what the kernel bound
//! and libcamera listed, whether capture delivers, and one verdict
//! (`xorgate_core::camera_truth`).
//!
//! This module gathers and publishes. It READS: the hardware block, the
//! last hardware reconcile (the camera listing it already ran), the boot
//! config text, sysfs, the pipeline dir and the capture supervisor's state.
//! It WRITES: `state.json`'s `cameras` (for `xorgate-rs status`) and the
//! snapshot the config report carries as `effective.cameras`. It never
//! writes config.txt, calls no helper and cannot reboot. The one thing it
//! sets in motion is the re-listing below.
//!
//! When it runs:
//! - [`CameraTruth::refresh`]: the slow facts (config.txt, the sysfs scan,
//!   which `.gst` files are bridged). At boot and from the worker's 60 s
//!   poll.
//! - [`CameraTruth::tick`]: the verdicts, from the cached slow facts plus
//!   the live reconcile verdicts and capture state. Every heartbeat (1 s):
//!   two short locks and no I/O.
//!
//! Re-listing (the B5 hold must heal without an agent restart). A stream
//! held because its port listed no camera would stay held for ever: cameras
//! are listed at boot and on a hardware-block change only. So while a port
//! is held AND the kernel has a driver bound on it (the boot listing may
//! simply have come too early), `tick` asks for the hardware reconcile to
//! run again (`CameraTruthOptions::recheck`, the `hardware_changed` path)
//! 15 s, 30 s and 60 s after the agent started, then every 5 minutes
//! (`camera_truth::next_relist_ms`). When the camera then lists, every
//! capture already running is restarted once (a capture started before
//! libcamera gained the camera keeps the media device, and the new camera
//! could never be acquired), then the stream starts as it would at boot. A port with no bound driver is never
//! re-listed: a driver binds at boot only. Each re-listing is one
//! `rpicam-hello --list-cameras` run, which the JS agent does not make.
//! Until one has run, a "driver bound, nothing listed" port is reported as
//! an observation the agent is still checking, not as a libcamera finding.
//!
//! When it asks for a config report: only when what the cloud last got (the
//! summary of the block of the last report that was SENT,
//! [`CameraTruth::for_report`] + [`CameraTruth::report_sent`]) differs from
//! the current summary, and then after a hold: [`REPORT_HOLD_MS`] (20 s), so
//! the two cameras of a device settling a few seconds apart make one
//! report; doubled after every request up to 10 minutes
//! (`camera_truth::next_report_hold_ms`), so a camera that fails and
//! recovers every couple of minutes costs a report per 10 minutes, not two
//! per cycle; back to 20 s after 30 quiet minutes. The summary leaves out
//! everything that moves with the capture ladder (the back-off step, the
//! last error line), and the capture state itself is settled
//! (`camera_truth::{capture_truth, settle_capture}`): a camera in its
//! back-off stays `failing` however many steps it walks, and a streaming
//! camera that is restarted on purpose stays `streaming` through the
//! restart (`RESTART_GRACE_MS`).
//!
//! Lock rule (ConfigSync, `ctx.rs` `report_hints` doc): [`for_report`] and
//! [`report_sent`] are called under ConfigSync's mutex and take only this
//! module's own mutex, under which nothing calls out (the change callback,
//! the re-listing and the state write run after it is released).
//!
//! [`for_report`]: CameraTruth::for_report
//! [`report_sent`]: CameraTruth::report_sent

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Map, Value};
use xorgate_core::boot_config::CamPort;
use xorgate_core::camera_catalog::catalog;
use xorgate_core::camera_sensors::{
    changed_since_boot, declared_id, missing_requirements, required_package_names, HostFacts,
    LineChange,
};
use xorgate_core::camera_truth::{
    capture_allowed, capture_truth, derive_port, next_relist_ms, next_report_hold_ms,
    node_for_port, port_needs_relist, settle_capture, summarise_port, CaptureLive, CaptureOff,
    CaptureTruth, PortFacts, PortNode, PortSetting, RELIST_AFTER_START_MS, REPORT_HOLD_RESET_MS,
};
use xorgate_core::settings::CameraSensorsSettings;
use xorgate_core::hardware_block::{DeviceHardwareBlock, Platform};
use xorgate_core::telemetry::RunMode;
use xorgate_core::video_capture::{is_bridged_pipeline, pipeline_file_for};

use crate::recording::hooks::{BootConfigTextFn, StreamHardwareSnapshot};

/// How long a changed summary waits before a config report is requested,
/// to begin with: longer than the capture stable window (15 s) plus the
/// default spawn stagger of the second camera (4 s). The first hold is
/// armed at the first change AFTER the boot report went out (never
/// before: the boot report carries the block as it is then), so a boot
/// that reported `starting` settles into ONE more report (`streaming` for
/// both cameras), and a flap inside the window asks for nothing.
pub const REPORT_HOLD_MS: u64 = 20_000;

/// Where the camera report exists: where the hardware reconcile runs (real
/// mode, online). In sim and replay mode and with `--offline` there is no
/// `effective.cameras` and no `cameras` key in state.json.
pub fn camera_report_enabled(mode: RunMode, offline: bool) -> bool {
    mode == RunMode::Real && !offline
}

/// The sysfs scan (`xorgate_hal::camera_probe::scan_port_nodes`); `None` =
/// it could not look.
pub type PortNodesFn = Arc<dyn Fn(Platform) -> Option<Vec<PortNode>> + Send + Sync>;

/// The real [`PortNodesFn`]: `/sys/bus/i2c/devices` + `/proc/device-tree`.
pub fn real_port_nodes() -> PortNodesFn {
    Arc::new(|platform| {
        xorgate_hal::camera_probe::scan_port_nodes(
            platform,
            std::path::Path::new(xorgate_hal::camera_probe::SYS_I2C_DEVICES),
            std::path::Path::new(xorgate_core::boot_config::PROC_DEVICE_TREE),
        )
    })
}

/// Reads the board (`xorgate_hal::boot_config_io::read_board`).
pub type BoardFn = Arc<dyn Fn() -> xorgate_core::boot_config::Board + Send + Sync>;

/// The real [`BoardFn`]: `/proc/device-tree/model`.
pub fn real_board() -> BoardFn {
    Arc::new(|| {
        xorgate_hal::boot_config_io::read_board(std::path::Path::new(
            xorgate_core::boot_config::PROC_DEVICE_TREE,
        ))
    })
}

/// Why a stream's capture is not wanted (`None` = it is wanted).
pub type CaptureOffFn = Box<dyn Fn(&str) -> Option<CaptureOff> + Send + Sync>;

/// What [`CameraTruth`] reads, and the two things it may ask for.
pub struct CameraTruthOptions {
    /// The hardware block in force.
    pub hardware: Box<dyn Fn() -> Option<DeviceHardwareBlock> + Send + Sync>,
    /// The hardware reconcile's verdicts and its camera listing.
    pub stream_hw: Box<dyn Fn() -> StreamHardwareSnapshot + Send + Sync>,
    /// The boot config text.
    pub boot_config: BootConfigTextFn,
    /// The board (`/proc/device-tree/model`), for config.txt's `[section]`
    /// filters: [`real_board`].
    pub board: BoardFn,
    /// The sysfs scan.
    pub port_nodes: PortNodesFn,
    /// `config.media.pipelineDir`.
    pub pipeline_dir: String,
    /// The capture supervisor's per-camera state
    /// (`RecordingManager::capture_health`); empty while none runs.
    pub capture: Box<dyn Fn() -> Vec<(String, CaptureLive)> + Send + Sync>,
    /// Where the block goes for `xorgate-rs status` (`StateHandle::set_cameras`).
    pub publish: Box<dyn Fn(Option<Value>) + Send + Sync>,
    /// "The cloud's copy is out of date": request a config report.
    pub on_change: Box<dyn Fn() + Send + Sync>,
    /// "List the cameras again" (`PhaseHooks::hardware_recheck`): the
    /// hardware reconcile through the `hardware_changed` path, quiet.
    pub recheck: Box<dyn Fn() + Send + Sync>,
    /// Why this stream's capture is not wanted, if it is not: the camera is
    /// disabled in `recording.video.cameras`, or media is off. `None` =
    /// wanted. A held port nobody wants is not re-listed, and the reason is
    /// what the sentence of an `ok` camera with capture off says. Read on
    /// [`CameraTruth::refresh`] (it may do I/O).
    pub capture_off: CaptureOffFn,
    /// [`REPORT_HOLD_MS`] (a test seam).
    pub hold_ms: u64,
    /// What the `cameraSensors` namespace adds (Phase 2). Read on
    /// [`CameraTruth::refresh`].
    pub setting: SettingSources,
}

/// The Phase 2 inputs of the report: the sensor setting, the lines as
/// booted, what the device has, and the video preset in use.
pub struct SettingSources {
    /// The `cameraSensors` namespace as settings.json holds it.
    pub sensors: Box<dyn Fn() -> Option<CameraSensorsSettings> + Send + Sync>,
    /// Each port's camera line as first seen in this boot (shared with the
    /// camera reconciler); `None` = not tracked.
    pub boot_lines: Option<Arc<crate::ops::camera_mount::BootLineStore>>,
    /// The device's OS release, architecture and the installed versions of
    /// the packages named (`xorgate_hal::camera_host::host_facts`). Asked
    /// only while a managed port names a sensor.
    pub host: Box<dyn Fn(&[String]) -> HostFacts + Send + Sync>,
    /// The video preset capture runs (`recording.video.preset`).
    pub preset: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

impl SettingSources {
    /// No sensor setting, nothing tracked: the report as it was before
    /// Phase 2.
    pub fn none() -> Self {
        Self {
            sensors: Box::new(|| None),
            boot_lines: None,
            host: Box::new(|_| HostFacts::default()),
            preset: Box::new(|| None),
        }
    }
}

/// The slow facts, as of the last [`CameraTruth::refresh`].
#[derive(Debug, Clone, Default)]
struct Statics {
    boot_text: Option<String>,
    /// The board, for config.txt's `[section]` filters.
    board: xorgate_core::boot_config::Board,
    nodes: Option<Vec<PortNode>>,
    /// Stream key → its `.gst` is the bridged kind (`None` = no `.gst`).
    bridged: HashMap<String, Option<bool>>,
    /// The streams whose capture is not wanted, and why (`capture_off`).
    unwanted: HashMap<String, CaptureOff>,
    /// Stream key → the sensor id the `cameraSensors` setting declares.
    setting: HashMap<String, String>,
    /// Stream key → its port's camera line changed since this boot.
    changed: HashMap<String, LineChange>,
    /// Stream key → what its declared sensor needs and the device lacks.
    missing: HashMap<String, String>,
    /// The video preset capture runs.
    preset: Option<String>,
}

/// One camera's settled capture state.
#[derive(Debug, Clone, Copy)]
struct Settled {
    state: CaptureTruth,
    /// Since when the live state has been `starting` / `off` while this
    /// still says `streaming` (the restart grace).
    soft_since: Option<tokio::time::Instant>,
}

struct St {
    statics: Option<Statics>,
    /// The capture state last derived per stream key (`settle_capture`).
    settled: HashMap<String, Settled>,
    /// The current block and its summary.
    current: Option<(Value, Vec<String>)>,
    /// The summary of the block handed to the report being built.
    offered: Option<Vec<String>>,
    /// The summary of the block the last SENT config report carried.
    reported: Option<Vec<String>>,
    /// Some config report has been sent. Before the first one (the boot
    /// report, at connect) nothing is "out of date": that report will carry
    /// the block as it is then, so no hold is armed until it has gone.
    sent_once: bool,
    /// A report-request timer is pending.
    hold_armed: bool,
    /// The hold of the next request (grows with every request).
    hold_ms: u64,
    /// When the last report was requested.
    last_request: Option<tokio::time::Instant>,
    /// The next re-listing, in ms after `Inner::started`.
    next_relist_ms: u64,
}

struct Inner {
    opts: CameraTruthOptions,
    started: tokio::time::Instant,
    st: Mutex<St>,
}

/// The per-port camera report. Cheap to clone.
#[derive(Clone)]
pub struct CameraTruth {
    inner: Arc<Inner>,
}

fn lock(m: &Mutex<St>) -> MutexGuard<'_, St> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// What one evaluation produced.
struct Built {
    /// The block and its summary; `None` = nothing to report (no cameras in
    /// the model, or no hardware reconcile has completed yet).
    block: Option<(Value, Vec<String>)>,
    /// Some port is held for lack of a camera while a driver is bound on it.
    needs_relist: bool,
}

/// Build the block from the facts: stream key → port report (the block's
/// `video` order; a repeated key keeps its first entry), plus its summary.
fn build_block(
    block: &DeviceHardwareBlock,
    hw: &StreamHardwareSnapshot,
    statics: &Statics,
    capture: &[(String, CaptureLive)],
    settled: &mut HashMap<String, Settled>,
    now: tokio::time::Instant,
) -> Built {
    if hw.passes == 0 || block.video.is_empty() {
        return Built {
            block: None,
            needs_relist: false,
        };
    }
    // The boot listing has been repeated at least once.
    let relisted = hw.passes >= 2;
    let mut needs_relist = false;
    let mut ports: IndexMap<&str, Value> = IndexMap::new();
    let mut summary = Vec::new();
    for video in &block.video {
        if ports.contains_key(video.key.as_str()) {
            continue;
        }
        // First camera per connector wins, as in the hardware reconcile.
        let listed = hw.cameras.as_deref().and_then(|cams| {
            cams.iter()
                .find(|c| c.connector.as_deref() == Some(video.connector.as_str()))
        });
        let live = capture
            .iter()
            .find(|(k, _)| *k == video.key)
            .map(|(_, l)| l);
        let held = !capture_allowed(hw.armed, hw.statuses.get(&video.key).copied());
        let driver_bound = statics
            .nodes
            .as_deref()
            .and_then(|n| node_for_port(n, &video.connector))
            .is_some_and(|n| n.driver.is_some());
        needs_relist |=
            port_needs_relist(held, driver_bound) && !statics.unwanted.contains_key(&video.key);
        let live_state = capture_truth(live, held);
        let prev = settled.get(&video.key).copied();
        // The restart grace: how long a camera that was streaming has read
        // `starting` / `off`.
        let soft = prev.is_some_and(|p| p.state == CaptureTruth::Streaming)
            && matches!(live_state, CaptureTruth::Starting | CaptureTruth::Off);
        let soft_since = soft.then(|| prev.and_then(|p| p.soft_since).unwrap_or(now));
        let soft_ms = soft_since.map_or(0, |s| now.duration_since(s).as_millis() as u64);
        let state = settle_capture(prev.map(|p| p.state), live_state, soft_ms);
        settled.insert(
            video.key.clone(),
            Settled {
                state,
                soft_since: soft_since.filter(|_| state == CaptureTruth::Streaming),
            },
        );
        let failing = state == CaptureTruth::Failing;
        let port = derive_port(&PortFacts {
            stream_key: &video.key,
            connector: &video.connector,
            model_sensor: &video.sensor,
            boot_text: statics.boot_text.as_deref(),
            board: statics.board,
            nodes: statics.nodes.as_deref(),
            enumerated: hw.cameras.is_some(),
            relisted,
            listed: listed.map(|c| (c.sensor.as_str(), c.camera_name.as_str())),
            capture: state,
            backoff_step: live.map_or(0, |l| l.backoff_step),
            // An `ERROR` line of a camera that streams is not a failure.
            last_error: live
                .filter(|_| failing)
                .and_then(|l| l.last_error.as_deref()),
            stalled_preset: live
                .filter(|_| failing)
                .and_then(|l| l.stalled_preset.as_deref()),
            unsupervised: statics.bridged.get(&video.key).copied().flatten() == Some(false),
            off_reason: statics
                .unwanted
                .get(&video.key)
                .copied()
                .unwrap_or(CaptureOff::NotBegun),
            setting: PortSetting {
                sensor: statics.setting.get(&video.key).map(String::as_str),
                line_change: statics.changed.get(&video.key),
                missing: statics.missing.get(&video.key).map(String::as_str),
                preset: statics.preset.as_deref(),
            },
        });
        // The sentence of a "driver bound, nothing listed" port changes when
        // the re-listing has confirmed it, and so does the summary; a stall
        // reads differently from a start failure.
        summary.push(format!(
            "{}|{}|{}|{}|{}|{}",
            summarise_port(&video.key, &port),
            u8::from(relisted && driver_bound && listed.is_none()),
            u8::from(
                failing
                    && live.is_some_and(|l| l.stalled_preset.is_some() && l.last_error.is_none())
            ),
            // The reason an `off` capture is off is part of its sentence.
            statics.unwanted.get(&video.key).map_or(0, |r| *r as u8 + 1),
            // So is what a declared sensor still needs, and a preset the
            // sensor cannot run.
            statics.missing.get(&video.key).map_or("", String::as_str),
            u8::from(port.detail.contains(" The video preset ")),
        ));
        ports.insert(
            video.key.as_str(),
            serde_json::to_value(&port).expect("a camera port serialises"),
        );
    }
    let map: Map<String, Value> = ports.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    Built {
        block: Some((Value::Object(map), summary)),
        needs_relist,
    }
}

impl CameraTruth {
    pub fn new(opts: CameraTruthOptions) -> Self {
        let hold_ms = opts.hold_ms;
        Self {
            inner: Arc::new(Inner {
                opts,
                started: tokio::time::Instant::now(),
                st: Mutex::new(St {
                    statics: None,
                    settled: HashMap::new(),
                    current: None,
                    offered: None,
                    reported: None,
                    sent_once: false,
                    hold_armed: false,
                    hold_ms,
                    last_request: None,
                    next_relist_ms: RELIST_AFTER_START_MS[0],
                }),
            }),
        }
    }

    /// Re-read the slow facts (config.txt, sysfs, which `.gst` files are
    /// bridged), then [`tick`](Self::tick). Small synchronous reads.
    pub fn refresh(&self) {
        let o = &self.inner.opts;
        let Some(block) = (o.hardware)() else {
            // No model: forget the slow facts; `tick` clears the block.
            lock(&self.inner.st).statics = None;
            self.tick();
            return;
        };
        let bridged = block
            .video
            .iter()
            .map(|v| {
                let path = pipeline_file_for(&o.pipeline_dir, &v.key);
                let gst = xorgate_hal::read_lossy::read_text(std::path::Path::new(&path)).ok();
                (
                    v.key.clone(),
                    gst.as_deref().map(|g| is_bridged_pipeline(Some(g))),
                )
            })
            .collect();
        let boot_text = (o.boot_config)();
        let board = (o.board)();
        // The sensor setting: the id each managed port names.
        let setting: HashMap<String, String> = (o.setting.sensors)()
            .and_then(|s| s.cameras)
            .map(|cams| {
                cams.iter()
                    .filter_map(|(k, entry)| Some((k.clone(), declared_id(entry)?)))
                    .collect()
            })
            .unwrap_or_default();
        // A port whose line differs from the one this boot started with.
        let changed: HashMap<String, LineChange> = match (&boot_text, &o.setting.boot_lines) {
            (Some(text), Some(store)) => {
                let booted = store.booted(text, board, catalog());
                block
                    .video
                    .iter()
                    .filter_map(|v| {
                        let port = CamPort::from_connector(&v.connector)?;
                        let ch = changed_since_boot(&booted, text, board, port, catalog())?;
                        Some((v.key.clone(), ch))
                    })
                    .collect()
            }
            _ => HashMap::new(),
        };
        // What a declared sensor needs and the device lacks. The device is
        // asked only while some managed port names a catalog sensor.
        let declared: Vec<(&String, &xorgate_core::camera_catalog::Sensor)> = setting
            .iter()
            .filter_map(|(k, id)| Some((k, catalog().by_id(id)?)))
            .collect();
        let missing: HashMap<String, String> = if declared.is_empty() {
            HashMap::new()
        } else {
            let mut names: Vec<String> = declared
                .iter()
                .flat_map(|(_, s)| required_package_names(s, catalog()))
                .collect();
            names.sort();
            names.dedup();
            let host = (o.setting.host)(&names);
            declared
                .iter()
                .filter_map(|(k, s)| Some(((*k).clone(), missing_requirements(s, catalog(), &host)?)))
                .collect()
        };
        let statics = Statics {
            boot_text,
            board,
            nodes: (o.port_nodes)(block.platform),
            bridged,
            unwanted: block
                .video
                .iter()
                .filter_map(|v| Some((v.key.clone(), (o.capture_off)(&v.key)?)))
                .collect(),
            setting,
            changed,
            missing,
            preset: (o.setting.preset)(),
        };
        lock(&self.inner.st).statics = Some(statics);
        self.tick();
    }

    /// Re-derive the verdicts from the cached slow facts and the live
    /// state; publish the block when it moved; arm the report request when
    /// the cloud's copy is out of date; ask for a re-listing when one is
    /// due. No I/O.
    pub fn tick(&self) {
        let o = &self.inner.opts;
        let block = (o.hardware)();
        let hw = (o.stream_hw)();
        let capture = (o.capture)();
        let now = tokio::time::Instant::now();
        let since_start = now.duration_since(self.inner.started).as_millis() as u64;
        let (publish, arm, relist) = {
            let mut g = lock(&self.inner.st);
            let st = &mut *g;
            let built = match (&block, &st.statics) {
                (Some(b), Some(statics)) => {
                    build_block(b, &hw, statics, &capture, &mut st.settled, now)
                }
                _ => Built {
                    block: None,
                    needs_relist: false,
                },
            };
            if built.block.is_none() {
                st.settled.clear();
            }
            let moved = built.block.as_ref().map(|n| &n.0) != st.current.as_ref().map(|c| &c.0);
            st.current = built.block;
            let stale = st.sent_once && st.current.as_ref().map(|c| &c.1) != st.reported.as_ref();
            let arm = (stale && !st.hold_armed).then(|| {
                st.hold_armed = true;
                // A long quiet spell: the hold starts over.
                if st.last_request.is_none_or(|t| {
                    now.duration_since(t).as_millis() as u64 >= REPORT_HOLD_RESET_MS
                }) {
                    st.hold_ms = o.hold_ms;
                }
                st.hold_ms
            });
            // A re-listing is due only while a held port has a bound driver;
            // a time that passed while none was needed is not made up for.
            let relist = built.needs_relist && since_start >= st.next_relist_ms;
            if since_start >= st.next_relist_ms {
                st.next_relist_ms = next_relist_ms(since_start);
            }
            (
                moved.then(|| st.current.as_ref().map(|c| c.0.clone())),
                arm,
                relist,
            )
        };
        if let Some(value) = publish {
            (o.publish)(value);
        }
        if relist {
            (o.recheck)();
        }
        if let Some(hold_ms) = arm {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(hold_ms)).await;
                let stale = {
                    let mut st = lock(&inner.st);
                    st.hold_armed = false;
                    let stale = st.current.as_ref().map(|c| &c.1) != st.reported.as_ref();
                    if stale {
                        st.last_request = Some(tokio::time::Instant::now());
                        st.hold_ms = next_report_hold_ms(st.hold_ms);
                    }
                    stale
                };
                if stale {
                    (inner.opts.on_change)();
                }
            });
        }
    }

    /// The block for a config report (`effective.cameras`), `None` when
    /// there is none. It counts as what the cloud has once
    /// [`report_sent`](Self::report_sent) follows. A snapshot read: safe
    /// under ConfigSync's mutex (module doc).
    pub fn for_report(&self) -> Option<Value> {
        let mut st = lock(&self.inner.st);
        st.offered = st.current.as_ref().map(|c| c.1.clone());
        st.current.as_ref().map(|c| c.0.clone())
    }

    /// The report built from the last [`for_report`](Self::for_report) was
    /// handed to the MQTT client (sent, or queued for the next connect). A
    /// report whose publish failed is never marked, so the next hold asks
    /// again.
    pub fn report_sent(&self) {
        let mut st = lock(&self.inner.st);
        st.reported = st.offered.clone();
        st.sent_once = true;
    }

    /// The current block without marking it reported (tests, diagnostics).
    pub fn current(&self) -> Option<Value> {
        lock(&self.inner.st).current.as_ref().map(|c| c.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use xorgate_core::cameras::EnumeratedCamera;
    use xorgate_core::capture_ladder::CaptureState;
    use xorgate_core::hardware_block::parse_hardware_block;
    use xorgate_core::stream_guard::StreamHardwareStatus;

    use super::*;

    /// The bench 2 hardware block (CM5, two cameras, the model says imx290).
    fn block() -> DeviceHardwareBlock {
        parse_hardware_block(&serde_json::json!({
            "v": 2, "platform": "rpi-cm5", "encoder": "sw-h264", "rails": true,
            "statusLed": {"r": 12, "g": 21, "b": 16}, "imu": "bno085",
            "video": [
                {"key": "cam0", "connector": "csi0", "sensor": "imx290", "codec": "h264"},
                {"key": "cam1", "connector": "csi1", "sensor": "imx290", "codec": "h264"}
            ]
        }))
        .unwrap()
    }

    const CAM1_PATH: &str = "/base/axi/pcie@1000120000/rp1/i2c@70000/imx219@10";

    /// Bench 2 on 2026-11-21: cam0 `imx335` silent, cam1 `imx219` listed.
    fn bench2_hw() -> StreamHardwareSnapshot {
        StreamHardwareSnapshot {
            armed: true,
            statuses: [
                ("cam0".to_string(), StreamHardwareStatus::NoCamera),
                ("cam1".to_string(), StreamHardwareStatus::Ok),
            ]
            .into_iter()
            .collect(),
            passes: 1,
            cameras: Some(vec![EnumeratedCamera {
                camera_name: CAM1_PATH.into(),
                sensor: "imx219".into(),
                connector: Some("csi1".into()),
            }]),
        }
    }

    fn live(state: CaptureState, stable_ms: Option<u64>, unstable: bool) -> CaptureLive {
        CaptureLive {
            state,
            record: false,
            bytes_seen: false,
            unstable,
            stable_ms,
            backoff_step: 0,
            last_error: None,
            stalled_preset: None,
        }
    }

    struct Rig {
        truth: CameraTruth,
        capture: Arc<Mutex<Vec<(String, CaptureLive)>>>,
        hw: Arc<Mutex<StreamHardwareSnapshot>>,
        published: Arc<Mutex<Vec<Option<Value>>>>,
        changes: Arc<AtomicUsize>,
        /// The driver bound on csi0's `imx335@1a` (`None` = its probe failed).
        cam0_driver: Arc<Mutex<Option<String>>>,
        /// When (ms after the rig was built) a re-listing was asked for.
        rechecks: Arc<Mutex<Vec<u64>>>,
        /// The stream keys whose capture is not wanted.
        unwanted: Arc<Mutex<Vec<String>>>,
    }

    /// What a config report does: read the block, then mark it sent.
    fn reported(r: &Rig) -> Option<Value> {
        let v = r.truth.for_report();
        r.truth.report_sent();
        v
    }

    fn rig(dir: &std::path::Path) -> Rig {
        let capture: Arc<Mutex<Vec<(String, CaptureLive)>>> = Arc::default();
        let hw = Arc::new(Mutex::new(bench2_hw()));
        let published: Arc<Mutex<Vec<Option<Value>>>> = Arc::default();
        let changes = Arc::new(AtomicUsize::new(0));
        let cam0_driver: Arc<Mutex<Option<String>>> = Arc::default();
        let rechecks: Arc<Mutex<Vec<u64>>> = Arc::default();
        let (c, h, p, n) = (
            capture.clone(),
            hw.clone(),
            published.clone(),
            changes.clone(),
        );
        let (d, rc, h2) = (cam0_driver.clone(), rechecks.clone(), hw.clone());
        let unwanted: Arc<Mutex<Vec<String>>> = Arc::default();
        let uw = unwanted.clone();
        let t0 = tokio::time::Instant::now();
        let truth = CameraTruth::new(CameraTruthOptions {
            hardware: Box::new(|| Some(block())),
            stream_hw: Box::new(move || h.lock().unwrap().clone()),
            boot_config: Arc::new(|| {
                Some("dtoverlay=imx335,cam0\ndtoverlay=imx219,cam1,rotation=180\n".to_string())
            }),
            board: Arc::new(|| xorgate_core::boot_config::Board::Unknown),
            port_nodes: Arc::new(move |_| {
                Some(vec![
                    PortNode {
                        connector: "csi0".into(),
                        node: "imx335@1a".into(),
                        driver: d.lock().unwrap().clone(),
                    },
                    PortNode {
                        connector: "csi1".into(),
                        node: "imx219@10".into(),
                        driver: Some("imx219".into()),
                    },
                ])
            }),
            pipeline_dir: dir.display().to_string(),
            capture: Box::new(move || c.lock().unwrap().clone()),
            publish: Box::new(move |v| p.lock().unwrap().push(v)),
            on_change: Box::new(move || {
                n.fetch_add(1, Ordering::SeqCst);
            }),
            // A re-listing: note when, and count the pass it stands for.
            recheck: Box::new(move || {
                rc.lock().unwrap().push(t0.elapsed().as_millis() as u64);
                h2.lock().unwrap().passes += 1;
            }),
            capture_off: Box::new(move |k| {
                uw.lock()
                    .unwrap()
                    .iter()
                    .any(|u| u == k)
                    .then_some(CaptureOff::Disabled)
            }),
            hold_ms: REPORT_HOLD_MS,
            setting: SettingSources::none(),
        });
        Rig {
            truth,
            capture,
            hw,
            published,
            changes,
            cam0_driver,
            rechecks,
            unwanted,
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("xg-camtruth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Let any pending report hold run out (the hold grows up to 10 min).
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(
            xorgate_core::camera_truth::REPORT_HOLD_MAX_MS + 1,
        ))
        .await;
        tokio::task::yield_now().await;
    }

    /// Bench 2 as the plan found it: cam0 not detected (and held: capture
    /// `absent`), cam1 an imx219 the model calls imx290, streaming.
    #[tokio::test(start_paused = true)]
    async fn bench2_block_shape_and_verdicts() {
        let dir = scratch("bench2");
        let r = rig(&dir);
        *r.capture.lock().unwrap() = vec![(
            "cam1".to_string(),
            live(CaptureState::Running, Some(20_000), false),
        )];
        r.truth.refresh();
        let v = r.truth.current().expect("a block");
        assert_eq!(
            v.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["cam0", "cam1"]
        );
        assert_eq!(
            serde_json::to_string(&v["cam0"]).unwrap(),
            r#"{"connector":"csi0","declared":{"sensor":"imx335","source":"boot-config"},"bootConfig":{"overlay":"imx335","line":"dtoverlay=imx335,cam0","lines":1},"bound":{"driver":false,"node":"imx335@1a","listed":null,"path":null,"identity":"weak"},"capture":{"state":"absent","backoffStep":0,"lastError":null},"verdict":"not-detected","detail":"csi0 is declared as imx335 by config.txt, but no imx335 answered on this port (the kernel driver did not bind). Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached, then reboot."}"#
        );
        assert_eq!(
            serde_json::to_string(&v["cam1"]).unwrap(),
            r#"{"connector":"csi1","declared":{"sensor":"imx219","source":"boot-config"},"bootConfig":{"overlay":"imx219","line":"dtoverlay=imx219,cam1,rotation=180","lines":1},"bound":{"driver":true,"node":"imx219@10","listed":"imx219","path":"/base/axi/pcie@1000120000/rp1/i2c@70000/imx219@10","identity":"chip-id"},"capture":{"state":"streaming","backoffStep":0,"lastError":null},"verdict":"ok","detail":"imx219 on csi1 is detected and streaming."}"#
        );
        // state.json got exactly this block, once.
        assert_eq!(r.published.lock().unwrap().clone(), vec![Some(v.clone())]);
        // The report carries it.
        assert_eq!(reported(&r), Some(v));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cloud is asked for a report only when its copy is out of date,
    /// after the hold, and a walk down the capture ladder asks for nothing.
    #[tokio::test(start_paused = true)]
    async fn reports_on_change_not_on_every_ladder_step() {
        let dir = scratch("churn");
        let r = rig(&dir);
        let set = |l: CaptureLive| *r.capture.lock().unwrap() = vec![("cam1".to_string(), l)];

        // Boot: capture starting; the boot report carries `starting`.
        set(live(CaptureState::Starting, None, false));
        r.truth.refresh();
        assert_eq!(
            reported(&r).unwrap()["cam1"]["capture"]["state"],
            "starting"
        );
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 0, "the cloud is current");

        // It goes stable: ONE request, after the hold.
        set(live(CaptureState::Running, Some(0), false));
        r.truth.tick();
        r.truth.tick();
        assert_eq!(r.changes.load(Ordering::SeqCst), 0, "held");
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 1);
        assert_eq!(
            reported(&r).unwrap()["cam1"]["capture"]["state"],
            "streaming"
        );

        // A planned restart of a streaming camera (recording switched on):
        // the old capture stops (no run: `off`), the new one starts and is
        // stable 19 s later. Still `streaming` throughout, nothing asked.
        *r.capture.lock().unwrap() = vec![];
        for _ in 0..3 {
            r.truth.tick();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        set(live(CaptureState::Starting, None, false));
        for _ in 0..19 {
            r.truth.tick();
            assert_eq!(
                r.truth.current().unwrap()["cam1"]["capture"]["state"],
                "streaming"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        set(live(CaptureState::Running, Some(0), false));
        r.truth.tick();
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 1, "a restart is not news");

        // A camera that really goes off (disabled in settings) is reported
        // once the restart grace is over, and back when it returns.
        *r.capture.lock().unwrap() = vec![];
        for _ in 0..29 {
            r.truth.tick();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(
            r.truth.current().unwrap()["cam1"]["capture"]["state"],
            "streaming",
            "inside the grace"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        r.truth.tick();
        assert_eq!(
            r.truth.current().unwrap()["cam1"]["capture"]["state"],
            "off"
        );
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 2);
        assert_eq!(reported(&r).unwrap()["cam1"]["capture"]["state"], "off");
        set(live(CaptureState::Running, Some(0), false));
        r.truth.tick();
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 3);
        assert_eq!(
            reported(&r).unwrap()["cam1"]["capture"]["state"],
            "streaming"
        );

        // It starts failing: one request …
        let mut l = live(CaptureState::Retrying, None, true);
        l.last_error = Some("ERROR a".into());
        set(l);
        r.truth.tick();
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 4);
        let rep = reported(&r).unwrap();
        assert_eq!(rep["cam1"]["verdict"], "no-frames");
        assert_eq!(rep["cam1"]["capture"]["lastError"], "ERROR a");
        let published_before = r.published.lock().unwrap().len();

        // … and the whole ladder after it (retry, start, park, back-off
        // steps 1..6, a new error line each time) asks for NOTHING more,
        // while state.json keeps following.
        for step in 0..6u32 {
            for state in [
                CaptureState::Starting,
                CaptureState::Retrying,
                CaptureState::Failed,
            ] {
                let mut l = live(state, None, true);
                l.backoff_step = step;
                l.last_error = Some(format!("ERROR step {step}"));
                set(l);
                r.truth.tick();
                settle().await;
            }
        }
        assert_eq!(r.changes.load(Ordering::SeqCst), 4, "no report churn");
        assert!(r.published.lock().unwrap().len() > published_before);
        assert_eq!(
            r.truth.current().unwrap()["cam1"]["capture"]["backoffStep"],
            5
        );

        // A flap inside the hold window (failing → streaming → failing)
        // asks for nothing either.
        set(live(CaptureState::Running, Some(70_000), false));
        r.truth.tick();
        let mut l = live(CaptureState::Failed, None, true);
        l.backoff_step = 5;
        set(l);
        r.truth.tick();
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No hardware reconcile yet, or no model: no block at all (the key is
    /// absent from the report and from state.json).
    #[tokio::test(start_paused = true)]
    async fn nothing_before_the_first_reconcile_and_unchecked_without_a_listing() {
        let dir = scratch("none");
        let r = rig(&dir);
        r.hw.lock().unwrap().passes = 0;
        r.truth.refresh();
        assert_eq!(r.truth.current(), None);
        assert_eq!(reported(&r), None);
        assert!(r.published.lock().unwrap().is_empty());
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 0);

        // A pass that could not enumerate: every port is `unchecked`.
        {
            let mut hw = r.hw.lock().unwrap();
            hw.passes = 1;
            hw.armed = false;
            hw.statuses.clear();
            hw.cameras = None;
        }
        r.truth.tick();
        let v = r.truth.current().unwrap();
        assert_eq!(v["cam0"]["verdict"], "unchecked");
        assert_eq!(v["cam1"]["verdict"], "unchecked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.gst` that is not the bridged kind: the agent's capture never runs
    /// that camera, so it cannot say frames flow.
    #[tokio::test(start_paused = true)]
    async fn a_direct_pipeline_is_unchecked() {
        let dir = scratch("direct");
        std::fs::write(dir.join("cam1.gst"), "libcamerasrc ! my own ! appsink\n").unwrap();
        std::fs::write(
            dir.join("cam0.gst"),
            "shmsrc socket-path=/x/cam0.sock ! appsink\n",
        )
        .unwrap();
        let r = rig(&dir);
        r.truth.refresh();
        let v = r.truth.current().unwrap();
        assert_eq!(v["cam1"]["verdict"], "unchecked");
        assert_eq!(v["cam1"]["capture"]["state"], "off");
        assert_eq!(v["cam0"]["verdict"], "not-detected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The B5 hold heals without an agent restart: a held port with a bound
    /// driver is listed again 15 s, 30 s and 60 s after the start, then
    /// every 5 minutes; a held port with NO bound driver never is.
    #[tokio::test(start_paused = true)]
    async fn a_held_port_with_a_bound_driver_is_relisted_on_schedule() {
        let dir = scratch("relist");
        let r = rig(&dir);
        r.truth.refresh();
        // cam0 is held, its probe failed (no driver): nothing to wait for.
        for _ in 0..400 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            r.truth.tick();
        }
        assert!(r.rechecks.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);

        // The same port with the driver BOUND and nothing listed (the CM4
        // `imx335` case, or a listing that came too early).
        let dir = scratch("relist-bound");
        let r = rig(&dir);
        *r.cam0_driver.lock().unwrap() = Some("imx335".into());
        r.truth.refresh();
        // Boot: an observation, still being checked.
        assert_eq!(
            r.truth.current().unwrap()["cam0"]["detail"],
            "The imx335 driver is bound on csi0, but libcamera did not list a camera there when the agent started. The agent is checking again."
        );
        assert_eq!(reported(&r).unwrap()["cam0"]["verdict"], "not-detected");
        for i in 0..700 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            r.truth.tick();
            if i == 39 {
                // The 15 s re-listing confirmed it: the sentence says so,
                // and the cloud was asked to take it (once: the report
                // that follows is what the cloud then has).
                assert_eq!(
                    r.truth.current().unwrap()["cam0"]["detail"],
                    "The imx335 driver is bound on csi0 but libcamera lists no camera there (checked again after the agent started): libcamera on this device may not support this sensor."
                );
                assert_eq!(r.changes.load(Ordering::SeqCst), 1);
                reported(&r);
            }
        }
        assert_eq!(
            r.rechecks.lock().unwrap().clone(),
            [15_000, 30_000, 60_000, 360_000, 660_000]
        );
        // The later re-listings found the same: nothing more to report.
        assert_eq!(r.changes.load(Ordering::SeqCst), 1);

        // The camera lists (the re-listing found it): the hold is over, and
        // so is the re-listing.
        {
            let mut hw = r.hw.lock().unwrap();
            hw.statuses
                .insert("cam0".to_string(), StreamHardwareStatus::Rendered);
            hw.cameras.as_mut().unwrap().push(EnumeratedCamera {
                camera_name: "/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a".into(),
                sensor: "imx335".into(),
                connector: Some("csi0".into()),
            });
        }
        let before = r.rechecks.lock().unwrap().len();
        for _ in 0..700 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            r.truth.tick();
        }
        assert_eq!(r.rechecks.lock().unwrap().len(), before);
        assert_eq!(r.truth.current().unwrap()["cam0"]["verdict"], "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A port that starts being held late (a hardware-block change) is not
    /// re-listed in a burst for the schedule points that already passed.
    #[tokio::test(start_paused = true)]
    async fn schedule_points_that_passed_unneeded_are_not_made_up_for() {
        let dir = scratch("relist-late");
        let r = rig(&dir);
        r.truth.refresh();
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            r.truth.tick();
        }
        *r.cam0_driver.lock().unwrap() = Some("imx335".into());
        r.truth.refresh();
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            r.truth.tick();
        }
        assert_eq!(r.rechecks.lock().unwrap().clone(), [360_000]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hold doubles with every request (20 s, 40 s, 80 s … 10 min) and
    /// starts over after 30 quiet minutes; a report that was built but not
    /// sent does not count as the cloud's copy.
    #[tokio::test(start_paused = true)]
    async fn the_hold_grows_with_repeats_and_a_failed_publish_is_asked_again() {
        let dir = scratch("hold");
        let r = rig(&dir);
        let set = |l: CaptureLive| *r.capture.lock().unwrap() = vec![("cam1".to_string(), l)];
        let up = || live(CaptureState::Running, Some(0), false);
        let down = || live(CaptureState::Retrying, None, true);
        let wait = |ms: u64| tokio::time::sleep(Duration::from_millis(ms));
        set(up());
        r.truth.refresh();
        reported(&r);

        // Change 1: asked after 20 s.
        set(down());
        r.truth.tick();
        wait(19_000).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 0);
        wait(1_100).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 1);
        reported(&r);
        // Change 2: 40 s.
        set(up());
        r.truth.tick();
        wait(39_000).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 1);
        wait(1_100).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 2);
        reported(&r);
        // Change 3: 80 s.
        set(down());
        r.truth.tick();
        wait(79_000).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 2);
        wait(1_100).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 3);

        // The report of change 3 was built but its publish FAILED: not
        // marked, so the next hold (160 s) asks again for the same block.
        let _ = r.truth.for_report();
        r.truth.tick();
        wait(159_000).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 3);
        wait(1_100).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 4);
        reported(&r);

        // 30 quiet minutes: the hold is 20 s again.
        wait(xorgate_core::camera_truth::REPORT_HOLD_RESET_MS).await;
        set(up());
        r.truth.tick();
        wait(19_000).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 4);
        wait(1_100).await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A two-camera boot makes ONE settle report, not two: no hold is armed
    /// before the boot report has gone (it carries the block as it is
    /// then), and the first hold, armed at the first change after it,
    /// outlasts the second camera's stagger.
    #[tokio::test(start_paused = true)]
    async fn a_two_camera_boot_settles_in_one_report() {
        let dir = scratch("boot2");
        let r = rig(&dir);
        {
            // Both cameras listed.
            let mut hw = r.hw.lock().unwrap();
            hw.statuses
                .insert("cam0".to_string(), StreamHardwareStatus::Ok);
            hw.cameras.as_mut().unwrap().push(EnumeratedCamera {
                camera_name: "/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a".into(),
                sensor: "imx335".into(),
                connector: Some("csi0".into()),
            });
        }
        let state = |t: u64, spawn: u64| {
            if t < spawn {
                None
            } else if t < spawn + 15 {
                Some(live(CaptureState::Starting, None, false))
            } else {
                Some(live(
                    CaptureState::Running,
                    Some((t - spawn - 15) * 1000),
                    false,
                ))
            }
        };
        r.truth.refresh();
        let mut asked_at = Vec::new();
        for t in 1..=120u64 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            // cam0 spawns at 1 s, cam1 4 s later (the default stagger).
            *r.capture.lock().unwrap() = [("cam0", state(t, 1)), ("cam1", state(t, 5))]
                .into_iter()
                .filter_map(|(k, l)| l.map(|l| (k.to_string(), l)))
                .collect();
            r.truth.tick();
            let before = r.changes.load(Ordering::SeqCst);
            tokio::task::yield_now().await;
            if t == 3 {
                // The boot report (MQTT connected): both still `starting`.
                assert_eq!(before, 0, "nothing is asked before the boot report");
                let v = reported(&r).unwrap();
                assert_eq!(v["cam0"]["capture"]["state"], "starting");
                assert_eq!(v["cam1"]["capture"]["state"], "off");
            }
            if r.changes.load(Ordering::SeqCst) > asked_at.len() {
                asked_at.push(t);
                // The report that answers it.
                let v = reported(&r).unwrap();
                assert_eq!(v["cam0"]["capture"]["state"], "streaming");
                assert_eq!(v["cam1"]["capture"]["state"], "streaming");
            }
        }
        // cam1 `starting` at 5 s is the first change after the boot report:
        // the hold runs to 25 s, by when both cameras stream (16 s, 20 s).
        assert_eq!(asked_at, [25], "ONE settle report");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A held port whose capture nobody wants (the camera is disabled, or
    /// media is off) is not re-listed; it is once it is wanted again.
    #[tokio::test(start_paused = true)]
    async fn an_unwanted_held_port_is_not_relisted() {
        let dir = scratch("unwanted");
        let r = rig(&dir);
        *r.cam0_driver.lock().unwrap() = Some("imx335".into());
        r.unwanted.lock().unwrap().push("cam0".to_string());
        r.truth.refresh();
        for _ in 0..400 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            r.truth.tick();
        }
        assert!(r.rechecks.lock().unwrap().is_empty());
        // Enabled again: the 60 s refresh sees it, the schedule resumes.
        r.unwanted.lock().unwrap().clear();
        r.truth.refresh();
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            r.truth.tick();
        }
        assert_eq!(r.rechecks.lock().unwrap().clone(), [660_000]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_exists_in_real_online_mode_only() {
        assert!(camera_report_enabled(RunMode::Real, false));
        assert!(!camera_report_enabled(RunMode::Real, true), "--offline");
        assert!(!camera_report_enabled(RunMode::Sim, false));
        assert!(!camera_report_enabled(RunMode::Sim, true));
        assert!(!camera_report_enabled(RunMode::Replay, false));
        assert!(!camera_report_enabled(RunMode::Replay, true));
    }
}
