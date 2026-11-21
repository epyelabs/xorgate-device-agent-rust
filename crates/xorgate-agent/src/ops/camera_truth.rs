//! The per-port camera report (support-other-camera-models plan, Phase 0;
//! RUST-ONLY, no TS counterpart): for every camera the device model
//! declares, what is declared, what config.txt says, what the kernel bound
//! and libcamera listed, whether capture delivers, and one verdict
//! (`xorgate_core::camera_truth`).
//!
//! This module only gathers and publishes. It READS: the hardware block,
//! the last hardware reconcile (the camera listing it already ran), the boot
//! config text, sysfs, the pipeline dir and the capture supervisor's state.
//! It WRITES: `state.json`'s `cameras` (for `xorgate-rs status`) and the
//! snapshot the config report carries as `effective.cameras`. It runs no
//! process, calls no helper, never writes config.txt and cannot reboot.
//!
//! When it runs:
//! - [`CameraTruth::refresh`]: the slow facts (config.txt, the sysfs scan,
//!   which `.gst` files are bridged). At boot and from the worker's 60 s
//!   poll.
//! - [`CameraTruth::tick`]: the verdicts, from the cached slow facts plus
//!   the live reconcile verdicts and capture state. Every heartbeat (1 s):
//!   two short locks and no I/O.
//!
//! When it asks for a config report: only when what the cloud last got
//! (the summary of the block handed to the last report,
//! [`CameraTruth::for_report`]) differs from the current summary, and then
//! after [`REPORT_HOLD_MS`] (20 s), so the two cameras of a device settling
//! a few seconds apart make one report. The summary leaves out everything that
//! moves with the capture ladder (the back-off step, the last error line),
//! and the capture state itself is settled
//! (`camera_truth::{capture_truth, settle_capture}`): a camera in its
//! back-off stays `failing` however many steps it walks, and a streaming
//! camera that is restarted on purpose stays `streaming` through the
//! restart (`RESTART_GRACE_MS`).
//!
//! Lock rule (ConfigSync, `ctx.rs` `report_hints` doc): [`for_report`] is
//! called under ConfigSync's mutex and takes only this module's own mutex,
//! under which nothing calls out (the change callback and the state write
//! run after it is released).
//!
//! [`for_report`]: CameraTruth::for_report

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Map, Value};
use xorgate_core::camera_truth::{
    boot_config_port, capture_truth, derive_port, settle_capture, summarise_port, CaptureLive,
    CaptureTruth, PortFacts, PortNode,
};
use xorgate_core::hardware_block::{DeviceHardwareBlock, Platform};
use xorgate_core::video_capture::{is_bridged_pipeline, pipeline_file_for};

use crate::recording::hooks::{BootConfigTextFn, StreamHardwareSnapshot};

/// How long a changed summary waits before a config report is requested:
/// longer than the capture stable window (15 s) plus the default spawn
/// stagger of the second camera (4 s), so a boot that reported `starting`
/// settles into ONE more report (`streaming` for both cameras), and a flap
/// inside the window asks for nothing.
pub const REPORT_HOLD_MS: u64 = 20_000;

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

/// What [`CameraTruth`] reads.
pub struct CameraTruthOptions {
    /// The hardware block in force.
    pub hardware: Box<dyn Fn() -> Option<DeviceHardwareBlock> + Send + Sync>,
    /// The hardware reconcile's verdicts and its camera listing.
    pub stream_hw: Box<dyn Fn() -> StreamHardwareSnapshot + Send + Sync>,
    /// The boot config text.
    pub boot_config: BootConfigTextFn,
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
    /// [`REPORT_HOLD_MS`] (a test seam).
    pub hold_ms: u64,
}

/// The slow facts, as of the last [`CameraTruth::refresh`].
#[derive(Debug, Clone, Default)]
struct Statics {
    boot_text: Option<String>,
    nodes: Option<Vec<PortNode>>,
    /// Stream key → its `.gst` is the bridged kind (`None` = no `.gst`).
    bridged: HashMap<String, Option<bool>>,
}

/// One camera's settled capture state.
#[derive(Debug, Clone, Copy)]
struct Settled {
    state: CaptureTruth,
    /// Since when the live state has been `starting` / `off` while this
    /// still says `streaming` (the restart grace).
    soft_since: Option<tokio::time::Instant>,
}

#[derive(Default)]
struct St {
    statics: Option<Statics>,
    /// The capture state last derived per stream key (`settle_capture`).
    settled: HashMap<String, Settled>,
    /// The current block and its summary.
    current: Option<(Value, Vec<String>)>,
    /// The summary of the block the last config report carried.
    reported: Option<Vec<String>>,
    /// A report-request timer is pending.
    hold_armed: bool,
}

struct Inner {
    opts: CameraTruthOptions,
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

/// Build the block from the facts: stream key → port report (the block's
/// `video` order; a repeated key keeps its first entry), plus its summary.
/// `None` = nothing to report (no cameras in the model, or no hardware
/// reconcile has completed yet).
fn build_block(
    block: &DeviceHardwareBlock,
    hw: &StreamHardwareSnapshot,
    statics: &Statics,
    capture: &[(String, CaptureLive)],
    settled: &mut HashMap<String, Settled>,
    now: tokio::time::Instant,
) -> Option<(Value, Vec<String>)> {
    if hw.passes == 0 || block.video.is_empty() {
        return None;
    }
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
        let held = !xorgate_core::camera_truth::capture_allowed(
            hw.armed,
            hw.statuses.get(&video.key).copied(),
        );
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
            boot: statics
                .boot_text
                .as_deref()
                .map(|t| boot_config_port(t, &video.connector)),
            nodes: statics.nodes.as_deref(),
            enumerated: hw.cameras.is_some(),
            listed: listed.map(|c| (c.sensor.as_str(), c.camera_name.as_str())),
            capture: state,
            backoff_step: live.map_or(0, |l| l.backoff_step),
            // An `ERROR` line of a camera that streams is not a failure.
            last_error: live
                .filter(|_| failing)
                .and_then(|l| l.last_error.as_deref()),
            unsupervised: statics.bridged.get(&video.key).copied().flatten() == Some(false),
        });
        summary.push(summarise_port(&video.key, &port));
        ports.insert(
            video.key.as_str(),
            serde_json::to_value(&port).expect("a camera port serialises"),
        );
    }
    let map: Map<String, Value> = ports.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    Some((Value::Object(map), summary))
}

impl CameraTruth {
    pub fn new(opts: CameraTruthOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                opts,
                st: Mutex::new(St::default()),
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
        let statics = Statics {
            boot_text: (o.boot_config)(),
            nodes: (o.port_nodes)(block.platform),
            bridged,
        };
        lock(&self.inner.st).statics = Some(statics);
        self.tick();
    }

    /// Re-derive the verdicts from the cached slow facts and the live
    /// state; publish the block when it moved; arm the report request when
    /// the cloud's copy is out of date. No I/O.
    pub fn tick(&self) {
        let o = &self.inner.opts;
        let block = (o.hardware)();
        let hw = (o.stream_hw)();
        let capture = (o.capture)();
        let (publish, arm) = {
            let mut g = lock(&self.inner.st);
            let st = &mut *g;
            let next = match (&block, &st.statics) {
                (Some(b), Some(statics)) => build_block(
                    b,
                    &hw,
                    statics,
                    &capture,
                    &mut st.settled,
                    tokio::time::Instant::now(),
                ),
                _ => None,
            };
            if next.is_none() {
                st.settled.clear();
            }
            let moved = next.as_ref().map(|n| &n.0) != st.current.as_ref().map(|c| &c.0);
            st.current = next;
            let stale = st.current.as_ref().map(|c| &c.1) != st.reported.as_ref();
            let arm = stale && !st.hold_armed;
            if arm {
                st.hold_armed = true;
            }
            (moved.then(|| st.current.as_ref().map(|c| c.0.clone())), arm)
        };
        if let Some(value) = publish {
            (o.publish)(value);
        }
        if arm {
            let inner = self.inner.clone();
            let hold = Duration::from_millis(inner.opts.hold_ms);
            tokio::spawn(async move {
                tokio::time::sleep(hold).await;
                let stale = {
                    let mut st = lock(&inner.st);
                    st.hold_armed = false;
                    st.current.as_ref().map(|c| &c.1) != st.reported.as_ref()
                };
                if stale {
                    (inner.opts.on_change)();
                }
            });
        }
    }

    /// The block for a config report (`effective.cameras`), `None` when
    /// there is none. Marks it as what the cloud has. A snapshot read: safe
    /// under ConfigSync's mutex (module doc).
    pub fn for_report(&self) -> Option<Value> {
        let mut st = lock(&self.inner.st);
        st.reported = st.current.as_ref().map(|c| c.1.clone());
        st.current.as_ref().map(|c| c.0.clone())
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
        }
    }

    struct Rig {
        truth: CameraTruth,
        capture: Arc<Mutex<Vec<(String, CaptureLive)>>>,
        hw: Arc<Mutex<StreamHardwareSnapshot>>,
        published: Arc<Mutex<Vec<Option<Value>>>>,
        changes: Arc<AtomicUsize>,
    }

    fn rig(dir: &std::path::Path) -> Rig {
        let capture: Arc<Mutex<Vec<(String, CaptureLive)>>> = Arc::default();
        let hw = Arc::new(Mutex::new(bench2_hw()));
        let published: Arc<Mutex<Vec<Option<Value>>>> = Arc::default();
        let changes = Arc::new(AtomicUsize::new(0));
        let (c, h, p, n) = (
            capture.clone(),
            hw.clone(),
            published.clone(),
            changes.clone(),
        );
        let truth = CameraTruth::new(CameraTruthOptions {
            hardware: Box::new(|| Some(block())),
            stream_hw: Box::new(move || h.lock().unwrap().clone()),
            boot_config: Arc::new(|| {
                Some("dtoverlay=imx335,cam0\ndtoverlay=imx219,cam1,rotation=180\n".to_string())
            }),
            port_nodes: Arc::new(|_| {
                Some(vec![
                    PortNode {
                        connector: "csi0".into(),
                        node: "imx335@1a".into(),
                        driver: None,
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
            hold_ms: REPORT_HOLD_MS,
        });
        Rig {
            truth,
            capture,
            hw,
            published,
            changes,
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("xg-camtruth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(REPORT_HOLD_MS + 1)).await;
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
        assert_eq!(r.truth.for_report(), Some(v));
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
            r.truth.for_report().unwrap()["cam1"]["capture"]["state"],
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
            r.truth.for_report().unwrap()["cam1"]["capture"]["state"],
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
        assert_eq!(
            r.truth.for_report().unwrap()["cam1"]["capture"]["state"],
            "off"
        );
        set(live(CaptureState::Running, Some(0), false));
        r.truth.tick();
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 3);
        assert_eq!(
            r.truth.for_report().unwrap()["cam1"]["capture"]["state"],
            "streaming"
        );

        // It starts failing: one request …
        let mut l = live(CaptureState::Retrying, None, true);
        l.last_error = Some("ERROR a".into());
        set(l);
        r.truth.tick();
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 4);
        let rep = r.truth.for_report().unwrap();
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
    async fn nothing_before_the_first_reconcile_and_unmanaged_without_a_listing() {
        let dir = scratch("none");
        let r = rig(&dir);
        r.hw.lock().unwrap().passes = 0;
        r.truth.refresh();
        assert_eq!(r.truth.current(), None);
        assert_eq!(r.truth.for_report(), None);
        assert!(r.published.lock().unwrap().is_empty());
        settle().await;
        assert_eq!(r.changes.load(Ordering::SeqCst), 0);

        // A pass that could not enumerate: every port is `unmanaged`.
        {
            let mut hw = r.hw.lock().unwrap();
            hw.passes = 1;
            hw.armed = false;
            hw.statuses.clear();
            hw.cameras = None;
        }
        r.truth.tick();
        let v = r.truth.current().unwrap();
        assert_eq!(v["cam0"]["verdict"], "unmanaged");
        assert_eq!(v["cam1"]["verdict"], "unmanaged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.gst` that is not the bridged kind: the agent's capture never runs
    /// that camera, so it cannot say frames flow.
    #[tokio::test(start_paused = true)]
    async fn a_direct_pipeline_is_unmanaged() {
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
        assert_eq!(v["cam1"]["verdict"], "unmanaged");
        assert_eq!(v["cam1"]["capture"]["state"], "off");
        assert_eq!(v["cam0"]["verdict"], "not-detected");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
