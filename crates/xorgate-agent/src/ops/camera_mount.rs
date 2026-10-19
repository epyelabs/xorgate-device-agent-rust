//! Camera-mount reconciler — the runtime half of `ts:src/hardware/boot-config.ts`
//! @89b5c64 (`applyRotationViaHelper` `:280-302`, `reconcileCameraMount`
//! `:329-457`) and of the worker's `reconcileCameraMountNow` (`worker.ts:150-205`),
//! Phase 4 WP3 (I7).
//!
//! It makes the boot config's overlay `rotation` parameter agree with the
//! `cameraMount` namespace and reports the three rotations (desired / boot /
//! in force) per managed camera. THE SAFETY INVARIANT (`boot-config.ts:13-18`,
//! invariant 4): this module can only ask for a reboot (`rebootRequired`) —
//! the only privileged call it can make is the `xorgate-camrotate` helper
//! argv minted by `boot_config::camrotate_call`; a unit test scans this
//! file's code for any reboot / power-off / systemctl identifier.
//!
//! Runs are NOT single-flight (the TS poll and the config-path kick may
//! overlap): the last pass to finish owns the state, as in the TS.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tracing::{info, warn};
use xorgate_core::boot_config::{
    boot_config_camera, camrotate_call, resolve_cam_token, BOOT_CONFIG_CANDIDATES, PROC_DEVICE_TREE,
};
use xorgate_core::clock::Clock;
use xorgate_core::clock_step::EventLevel;
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::js;
use xorgate_core::settings::CameraMountSettings;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::types::{
    CameraMountCameraState, CameraMountState, CameraMountVerdict, CameraRotationDeg,
};
use xorgate_hal::boot_config_io::{
    probe_overlay_rotation, read_boot_config, read_in_force, resolve_boot_config_path_fs,
    OverlayProbeResult,
};
use xorgate_hal::io::TapeSink;
use xorgate_hal::privileged::{HelperArgv, PrivilegedRunner};

use crate::ctx::BoxFut;
use crate::state::StateHandle;

/// `OverlayProbe` (`boot-config.ts:193`): does this sensor's overlay declare
/// a `rotation` parameter? Production: [`DtoverlayProbe`].
pub trait OverlayProbe: Send + Sync + 'static {
    fn probe<'a>(&'a self, sensor: &'a str) -> BoxFut<'a, OverlayProbeResult>;
}

/// `applyRotation`'s answer (`boot-config.ts:276`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationResult {
    pub ok: bool,
    pub changed: bool,
    pub detail: String,
}

/// `applyRotation(sensor, camToken, deg)` (`boot-config.ts:272-276`).
/// Production: [`HelperRotationWriter`].
pub trait RotationWriter: Send + Sync + 'static {
    fn apply<'a>(
        &'a self,
        sensor: &'a str,
        cam_token: &'a str,
        deg: CameraRotationDeg,
    ) -> BoxFut<'a, RotationResult>;
}

/// `probeOverlayRotation` (`boot-config.ts:206-215`): `dtoverlay -h <sensor>`.
pub struct DtoverlayProbe {
    pub tape: Arc<dyn TapeSink>,
}

impl OverlayProbe for DtoverlayProbe {
    fn probe<'a>(&'a self, sensor: &'a str) -> BoxFut<'a, OverlayProbeResult> {
        Box::pin(probe_overlay_rotation(sensor, &*self.tape))
    }
}

/// `applyRotationViaHelper` (`boot-config.ts:280-302`): `sudo -n
/// /usr/local/bin/xorgate-camrotate <sensor> <camN> <deg>` (20 s) through
/// [`PrivilegedRunner::run_helper`]. The helper's exit contract: 0 =
/// changed, 3 = already correct, anything else = failed with
/// `stderr.trim() || stdout.trim() || exit <code>`; `notFound` →
/// `sudo not found`. Arguments the helper would refuse never reach sudo:
/// `camrotate_call`'s refusal is the helper's own usage line, so the
/// `detail` is the one the TS reports after the helper exits 2.
pub struct HelperRotationWriter {
    pub runner: Arc<dyn PrivilegedRunner>,
}

impl RotationWriter for HelperRotationWriter {
    fn apply<'a>(
        &'a self,
        sensor: &'a str,
        cam_token: &'a str,
        deg: CameraRotationDeg,
    ) -> BoxFut<'a, RotationResult> {
        Box::pin(async move {
            let call = match camrotate_call(sensor, cam_token, deg) {
                Ok(c) => c,
                Err(detail) => {
                    return RotationResult {
                        ok: false,
                        changed: false,
                        detail,
                    }
                }
            };
            let res = self.runner.run_helper(&HelperArgv::from(&call)).await;
            if res.not_found {
                return RotationResult {
                    ok: false,
                    changed: false,
                    detail: "sudo not found".to_string(),
                };
            }
            if res.exit_code == Some(3) {
                return RotationResult {
                    ok: true,
                    changed: false,
                    detail: String::new(),
                };
            }
            if !res.failed {
                return RotationResult {
                    ok: true,
                    changed: true,
                    detail: String::new(),
                };
            }
            let (e, o) = (js::trim(&res.stderr), js::trim(&res.stdout));
            let detail = if !e.is_empty() {
                e.to_string()
            } else if !o.is_empty() {
                o.to_string()
            } else {
                match res.exit_code {
                    Some(c) => format!("exit {c}"),
                    None => "exit null".to_string(),
                }
            };
            RotationResult {
                ok: false,
                changed: false,
                detail,
            }
        })
    }
}

/// Which boot config the pass reads (`opts.bootConfigPath`, `boot-config.ts:264`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootConfigPath {
    /// `undefined`: `resolveBootConfigPath()` over [`BOOT_CONFIG_CANDIDATES`].
    Resolve,
    /// A fixed path, or `None` = "no config.txt on this device".
    Fixed(Option<String>),
}

/// `ReconcileCameraMountOptions` (`boot-config.ts:255-278`), owned.
pub struct ReconcileCameraMountOptions {
    /// The `cameraMount` namespace as settings.json holds it.
    pub settings: Option<CameraMountSettings>,
    /// Model facts (stream key → connector/sensor); see `resolve_cam_token`.
    pub hardware: Option<DeviceHardwareBlock>,
    /// Where the hardware reconciler caches each camera's identity.
    pub pipeline_dir: String,
    pub boot_config_path: BootConfigPath,
    /// `opts.dtRoot ?? PROC_DEVICE_TREE`.
    pub dt_root: PathBuf,
    /// `(opts.now ?? Date.now)()` — read once, stamps `at`.
    pub now: i64,
    pub log: Arc<dyn Fn(EventLevel, &str) + Send + Sync>,
    pub probe: Arc<dyn OverlayProbe>,
    pub apply_rotation: Arc<dyn RotationWriter>,
}

/// `reconcileCameraMount(opts)` (`boot-config.ts:329-457`), line by line.
/// `None` = the namespace is ABSENT: nothing is read, compared or written
/// (an installer's hand-set `rotation=180` survives on an unconfigured
/// device). Idempotent: the helper runs only when config.txt disagrees.
///
/// Every `detail` below is CLOUD-VISIBLE (the report's `cameraMount`) and
/// verbatim TS, including ``run `sudo xorgate update` `` (Q9).
pub async fn reconcile_camera_mount(o: ReconcileCameraMountOptions) -> Option<CameraMountState> {
    let cameras = o.settings.as_ref()?.cameras.as_ref()?;
    let at = o.now;
    let mut keys: Vec<&String> = cameras.keys().collect();
    keys.sort();
    if keys.is_empty() {
        return Some(CameraMountState {
            at,
            cameras: vec![],
            reboot_required: false,
            wrote: false,
        });
    }
    let path: Option<String> = match &o.boot_config_path {
        BootConfigPath::Resolve => {
            resolve_boot_config_path_fs(BOOT_CONFIG_CANDIDATES).map(str::to_string)
        }
        BootConfigPath::Fixed(p) => p.clone(),
    };
    let mut out: Vec<CameraMountCameraState> = Vec::new();
    let mut wrote = false;

    for stream_key in keys {
        let desired = cameras[stream_key].rotation_deg;
        let desired_deg = f64::from(desired.degrees());
        let in_force_deg = read_in_force(&o.pipeline_dir, stream_key, &o.dt_root);
        let push = |out: &mut Vec<CameraMountCameraState>,
                    boot_deg: Option<f64>,
                    verdict: CameraMountVerdict,
                    detail: Option<String>| {
            out.push(CameraMountCameraState {
                stream_key: stream_key.clone(),
                desired_deg: desired,
                in_force_deg,
                boot_deg,
                verdict,
                detail,
            })
        };

        let Some(cam_token) = resolve_cam_token(stream_key, o.hardware.as_ref()) else {
            push(
                &mut out,
                None,
                CameraMountVerdict::UnknownCamera,
                Some(format!("this device has no camera \"{stream_key}\"")),
            );
            continue;
        };

        let Some(path) = path.as_deref() else {
            push(
                &mut out,
                None,
                CameraMountVerdict::NoOverlay,
                Some(
                    "no config.txt on this device (looked in /boot/firmware and /boot)".to_string(),
                ),
            );
            continue;
        };

        let text = match read_boot_config(path) {
            Ok(t) => t,
            Err(message) => {
                push(
                    &mut out,
                    None,
                    CameraMountVerdict::WriteFailed,
                    Some(format!("cannot read {path}: {message}")),
                );
                continue;
            }
        };

        let view = boot_config_camera(&text, &cam_token);
        let Some(sensor) = view.sensor.clone() else {
            push(
                &mut out,
                None,
                CameraMountVerdict::NoOverlay,
                Some(format!(
                    "no dtoverlay line for {cam_token} in {path} — the camera overlay is missing"
                )),
            );
            continue;
        };

        if view.rotation != Some(desired_deg) {
            // Support gate BEFORE any edit (Q4): truth from the box, not a list.
            let support = o.probe.probe(&sensor).await;
            if !support.ok || !support.supports {
                let detail = if support.ok {
                    format!("the {sensor} overlay on this device has no \"rotation\" parameter")
                } else {
                    format!("could not ask dtoverlay whether {sensor} supports rotation")
                };
                push(
                    &mut out,
                    view.rotation,
                    CameraMountVerdict::Unsupported,
                    Some(detail),
                );
                continue;
            }

            let res = o.apply_rotation.apply(&sensor, &cam_token, desired).await;
            if !res.ok {
                push(
                    &mut out,
                    view.rotation,
                    CameraMountVerdict::WriteFailed,
                    Some(format!(
                        "could not edit {path}: {} (the agent needs the xorgate-camrotate helper — run `sudo xorgate update`)",
                        res.detail
                    )),
                );
                (o.log)(
                    EventLevel::Warn,
                    &format!(
                        "{stream_key}: boot config rotation edit failed — {}",
                        res.detail
                    ),
                );
                continue;
            }
            if res.changed {
                wrote = true;
                (o.log)(
                    EventLevel::Info,
                    &format!(
                        "{stream_key}: {sensor}/{cam_token} rotation set to {}° in {path} — REBOOT required before the image changes (the agent never reboots itself)",
                        desired.degrees()
                    ),
                );
            }
        }

        let boot_deg = Some(desired_deg);
        match in_force_deg {
            None => push(
                &mut out,
                boot_deg,
                CameraMountVerdict::Unconfirmed,
                Some(
                    "could not read the rotation currently in force from the device tree"
                        .to_string(),
                ),
            ),
            Some(f) => push(
                &mut out,
                boot_deg,
                if f == desired_deg {
                    CameraMountVerdict::Applied
                } else {
                    CameraMountVerdict::PendingReboot
                },
                None,
            ),
        }
    }

    let reboot_required = out
        .iter()
        .any(|c| c.verdict == CameraMountVerdict::PendingReboot);
    Some(CameraMountState {
        at,
        cameras: out,
        reboot_required,
        wrote,
    })
}

/// `summariseCameraMount(state)` (`worker.ts:202-205`): the parts worth
/// re-reporting on — `[streamKey, desiredDeg, inForceDeg, bootDeg, verdict]`
/// per camera, the timestamp and details ignored. `None` ≙ `undefined`.
pub type CameraMountSummary =
    Option<Vec<(String, u32, Option<f64>, Option<f64>, CameraMountVerdict)>>;

pub fn summarise_camera_mount(state: Option<&CameraMountState>) -> CameraMountSummary {
    state.map(|s| {
        s.cameras
            .iter()
            .map(|c| {
                (
                    c.stream_key.clone(),
                    c.desired_deg.degrees(),
                    c.in_force_deg,
                    c.boot_deg,
                    c.verdict,
                )
            })
            .collect()
    })
}

/// What [`CameraMountReconciler::now`] reads on every pass (`worker.ts:171-182`).
pub struct CameraMountReconcilerOptions {
    /// `loadSettings().cameraMount` — read per pass.
    pub settings: Box<dyn Fn() -> Option<CameraMountSettings> + Send + Sync>,
    /// The worker's current `hardwareBlock` — read per pass.
    pub hardware: Box<dyn Fn() -> Option<DeviceHardwareBlock> + Send + Sync>,
    /// `config.media.pipelineDir`.
    pub pipeline_dir: String,
    /// Production: [`BootConfigPath::Resolve`].
    pub boot_config_path: BootConfigPath,
    /// Production: [`PROC_DEVICE_TREE`] ([`default_dt_root`]).
    pub dt_root: PathBuf,
    pub probe: Arc<dyn OverlayProbe>,
    pub apply_rotation: Arc<dyn RotationWriter>,
    /// The worker's `log` (`worker.ts:176-181`): see [`camera_mount_event_sink`].
    pub log: Arc<dyn Fn(EventLevel, &str) + Send + Sync>,
    pub clock: Arc<dyn Clock + Send + Sync>,
}

/// `/proc/device-tree` as a path.
pub fn default_dt_root() -> PathBuf {
    PathBuf::from(PROC_DEVICE_TREE)
}

/// `reconcileCameraMountNow` + `cameraMountState` (`worker.ts:162-200`). The
/// worker (WP6) builds it in REAL mode only (`worker.ts:171`) and calls
/// [`now`](Self::now) at boot (awaited, `:432`), on every cloud config
/// message (not awaited, `:889`) and from the 60 s poll (`:648-651`).
#[derive(Clone)]
pub struct CameraMountReconciler {
    inner: Arc<ReconcilerInner>,
}

struct ReconcilerInner {
    opts: CameraMountReconcilerOptions,
    /// `cameraMountState`: snapshot-only access (safe under ConfigSync's mutex).
    state: Mutex<Option<CameraMountState>>,
    /// `requestConfigReport?.("boot")`.
    on_change: Box<dyn Fn() + Send + Sync>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl CameraMountReconciler {
    /// `on_change` = `requestConfigReport("boot")`: called when the summary
    /// moved (`worker.ts:191-193`) — "reboot required" must not wait for the
    /// hourly heartbeat.
    pub fn new(opts: CameraMountReconcilerOptions, on_change: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(ReconcilerInner {
                opts,
                state: Mutex::new(None),
                on_change,
            }),
        }
    }

    /// One `reconcileCameraMountNow()` pass. A failure (a panic here — the
    /// TS `catch`) never touches the rest of the agent: it logs
    /// `[agent] camera mount reconcile failed: <msg>` (console only) and
    /// leaves the previous state in place.
    pub fn now(&self) -> BoxFut<'static, ()> {
        let inner = self.inner.clone();
        Box::pin(async move {
            // Everything inside the TS `try` — the settings read included —
            // runs in its own task so a panic is observable (`JoinError`).
            let task = inner.clone();
            let pass = tokio::spawn(async move {
                let o = &task.opts;
                let opts = ReconcileCameraMountOptions {
                    settings: (o.settings)(),
                    hardware: (o.hardware)(),
                    pipeline_dir: o.pipeline_dir.clone(),
                    boot_config_path: o.boot_config_path.clone(),
                    dt_root: o.dt_root.clone(),
                    now: o.clock.wall_ms(),
                    log: o.log.clone(),
                    probe: o.probe.clone(),
                    apply_rotation: o.apply_rotation.clone(),
                };
                reconcile_camera_mount(opts).await
            });
            let next = match pass.await {
                Ok(n) => n,
                Err(e) => {
                    let msg = if e.is_panic() {
                        panic_message(e.into_panic())
                    } else {
                        e.to_string()
                    };
                    warn!("[agent] camera mount reconcile failed: {msg}");
                    return;
                }
            };
            let changed = {
                let mut st = lock(&inner.state);
                let before = summarise_camera_mount(st.as_ref());
                let after = summarise_camera_mount(next.as_ref());
                *st = next;
                before != after
            };
            if changed {
                (inner.on_change)();
            }
        })
    }

    /// `cameraMountState` (the report's `cameraMount`); `None` = not
    /// reconciled yet, or the namespace is absent.
    pub fn state(&self) -> Option<CameraMountState> {
        lock(&self.inner.state).clone()
    }
}

fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// The worker's reconciler `log` (`worker.ts:176-181`): `[camera-mount] <msg>`
/// on the console (warn → `console.warn`), then `store.event(level,
/// "camera mount: <msg>", t)` + `store.flush(t, true)` with one `Date.now()`.
pub fn camera_mount_event_sink(
    state: StateHandle,
    clock: Arc<dyn Clock + Send + Sync>,
) -> Arc<dyn Fn(EventLevel, &str) + Send + Sync> {
    Arc::new(move |level, msg| {
        let t = clock.wall_ms();
        let lvl = match level {
            EventLevel::Warn => {
                warn!("[camera-mount] {msg}");
                ActivityLevel::Warn
            }
            EventLevel::Info => {
                info!("[camera-mount] {msg}");
                ActivityLevel::Info
            }
        };
        state.note(lvl, format!("camera mount: {msg}"), t);
    })
}
