//! Camera-mount reconciler — the runtime half of `ts:src/hardware/boot-config.ts`
//! @89b5c64 (`applyRotationViaHelper` `:280-302`, `reconcileCameraMount`
//! `:329-457`) and of the worker's `reconcileCameraMountNow` (`worker.ts:150-205`),
//! Phase 4 WP3 (I7), reworked in Phase 1 of the support-other-camera-models
//! plan (accepted divergences D95-D104, D106).
//!
//! It makes the boot config's camera overlay line agree with the
//! `cameraMount` namespace and reports the three rotations (desired / boot /
//! in force) per managed camera. `cameraMount` means HOW THE CAMERA IS
//! MOUNTED, for every sensor:
//!
//! ```text
//! device-tree rotation = the overlay's default rotation + cameraMount (mod 360)
//! ```
//!
//! The default is the sensor catalog's (`imx290` 0; `imx219`, `imx708`,
//! `imx335` 180), so on an IMX290 the two numbers are the same and nothing
//! changed for it. The three reported rotations are all MOUNT rotations
//! (the wire is unchanged): 0 = upright as mounted.
//!
//! What it never does:
//! - write when the port's line already gives the wanted rotation (an
//!   explicit `rotation=` token is added only when the line is written);
//! - edit a line it cannot stand behind (bug B7): two camera lines on the
//!   port, a line under a `[section]` it cannot evaluate, or a port where
//!   the kernel and libcamera do not confirm the line's sensor
//!   (`camera_truth::rotation_write_block`);
//! - rotate a sensor that cannot flip (`imx335`);
//! - keep retrying a write that fails ([`RETRY_FAILED_WRITE_MS`]).
//!
//! THE SAFETY INVARIANT (`boot-config.ts:13-18`, invariant 4): this module
//! can only ask for a reboot (`rebootRequired`) — the only privileged call
//! it can make is the `xorgate-camcfg` helper argv minted by
//! `boot_config::camcfg_call`; a unit test scans this file's code for any
//! reboot / power-off / systemctl identifier.
//!
//! Runs are NOT single-flight (the TS poll and the config-path kick may
//! overlap): the last pass to finish owns the state, as in the TS. The
//! helper serialises its own writes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tracing::{info, warn};
use xorgate_core::boot_config::{
    camcfg_call, camera_lines_for_port, line_rotation, line_with_rotation, mount_of_rotation,
    parse_scoped_lines, resolve_cam_token, rotation_for_mount, Board, CamCfgOp, CamPort, LineScope,
    ScopedLine, BOOT_CONFIG_CANDIDATES, CAMCFG_ALLOW_LIST, CAMCFG_HELPER, PROC_DEVICE_TREE,
};
use xorgate_core::camera_catalog::Catalog;
use xorgate_core::camera_truth::{
    rotation_write_block, CaptureOff, CaptureTruth, PortFacts, PortNode,
};
use xorgate_core::cameras::EnumeratedCamera;
use xorgate_core::clock::Clock;
use xorgate_core::clock_step::EventLevel;
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::js;
use xorgate_core::settings::CameraMountSettings;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::types::{CameraMountCameraState, CameraMountState, CameraMountVerdict};
use xorgate_hal::boot_config_io::{
    camera_name_for, dt_rotation_state, probe_overlay_rotation, read_board, read_boot_config,
    resolve_boot_config_path_fs, DtRotation, OverlayProbeResult,
};
use xorgate_hal::io::TapeSink;
use xorgate_hal::privileged::{HelperArgv, PrivilegedRunner};

use crate::ctx::BoxFut;
use crate::ops::clock_stepper::EventSink;
use crate::state::StateHandle;

/// A write that failed is not tried again before this long has passed (the
/// 60 s poll would otherwise run `sudo` against a missing grant once a
/// minute for ever). A change of what is asked is tried at once.
pub const RETRY_FAILED_WRITE_MS: i64 = 600_000;

/// `OverlayProbe` (`boot-config.ts:193`): does this overlay declare a
/// `rotation` parameter, and with which default? Asked only for an overlay
/// OUTSIDE the sensor catalog (D96). Production: [`DtoverlayProbe`].
pub trait OverlayProbe: Send + Sync + 'static {
    fn probe<'a>(&'a self, overlay: &'a str) -> BoxFut<'a, OverlayProbeResult>;
}

/// `applyRotation`'s answer (`boot-config.ts:276`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationResult {
    pub ok: bool,
    pub changed: bool,
    pub detail: String,
}

/// The camera-line writer: set the `rotation=` token of `port`'s line,
/// which names `overlay`. Production: [`HelperRotationWriter`].
pub trait RotationWriter: Send + Sync + 'static {
    /// `None` when the writer can be used; else why not, as a sentence for
    /// the status (the helper is not installed yet: the update window).
    fn unavailable(&self) -> Option<String>;
    fn apply<'a>(
        &'a self,
        port: CamPort,
        overlay: &'a str,
        rotation: u16,
    ) -> BoxFut<'a, RotationResult>;
}

/// `probeOverlayRotation` (`boot-config.ts:206-215`): `dtoverlay -h <overlay>`.
pub struct DtoverlayProbe {
    pub tape: Arc<dyn TapeSink>,
}

impl OverlayProbe for DtoverlayProbe {
    fn probe<'a>(&'a self, overlay: &'a str) -> BoxFut<'a, OverlayProbeResult> {
        Box::pin(probe_overlay_rotation(overlay, &*self.tape))
    }
}

/// The camera-line owner through sudo: `sudo -n /usr/local/bin/xorgate-camcfg
/// rotate <cam0|cam1> <overlay> <0|180>` (20 s) through
/// [`PrivilegedRunner::run_helper`]. The helper's exit contract: 0 =
/// changed, 3 = already as asked, anything else = failed with
/// `stderr.trim() || stdout.trim() || exit <code>`; `notFound` →
/// `sudo not found`. Arguments the helper would refuse never reach sudo
/// (`camcfg_call`).
///
/// The JS agent calls `xorgate-camrotate <sensor> <camN> <deg>` here
/// (`boot-config.ts:280-302`); the Rust agent no longer does (D95).
pub struct HelperRotationWriter {
    pub runner: Arc<dyn PrivilegedRunner>,
    pub catalog: &'static Catalog,
    /// Are the helper and its allow-list installed? (They arrive with the
    /// SECOND `xorgate-rs update` after this build, or with a re-run of
    /// `install-rs.sh`: the first update is run by the previous build's
    /// updater, which does not know them.) Production: [`helper_installed`].
    pub installed: Box<dyn Fn() -> bool + Send + Sync>,
}

/// Production: `/usr/local/bin/xorgate-camcfg` and its allow-list exist.
pub fn helper_installed() -> bool {
    std::path::Path::new(CAMCFG_HELPER).is_file()
        && std::path::Path::new(CAMCFG_ALLOW_LIST).is_file()
}

/// The status sentence of a device in the update window.
pub const HELPER_NOT_INSTALLED: &str = "the xorgate-camcfg helper is not installed on this device yet — run `sudo xorgate-rs update` once more";

impl RotationWriter for HelperRotationWriter {
    fn unavailable(&self) -> Option<String> {
        (!(self.installed)()).then(|| HELPER_NOT_INSTALLED.to_string())
    }

    fn apply<'a>(
        &'a self,
        port: CamPort,
        overlay: &'a str,
        rotation: u16,
    ) -> BoxFut<'a, RotationResult> {
        Box::pin(async move {
            let op = CamCfgOp::Rotate {
                port,
                overlay: overlay.to_string(),
                rotation,
            };
            let call = match camcfg_call(op, self.catalog) {
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

/// What the kernel and libcamera say is on the camera ports, as of this
/// pass: the inputs of the B7 gate (`camera_truth::rotation_write_block`).
#[derive(Debug, Clone, Default)]
pub struct PortEvidence {
    /// The camera listing of the last hardware reconcile pass; `None` = it
    /// could not list cameras (or no pass has completed).
    pub cameras: Option<Vec<EnumeratedCamera>>,
    /// Cameras were listed again after the boot listing.
    pub relisted: bool,
    /// The sysfs scan (`xorgate_hal::camera_probe`); `None` = it could not
    /// look.
    pub nodes: Option<Vec<PortNode>>,
}

/// Reads the [`PortEvidence`] of a pass, given the hardware block in force.
pub type EvidenceFn = Box<dyn Fn(Option<&DeviceHardwareBlock>) -> PortEvidence + Send + Sync>;

/// What a reconciler remembers between passes.
#[derive(Debug, Default)]
pub struct WriteMemo {
    /// Per stream key: the helper arguments of the write that failed, when
    /// (wall ms) and its status detail.
    failed: HashMap<String, (Vec<String>, i64, String)>,
    /// Warnings already logged (`<key>|<reason>`): logged once, not on
    /// every 60 s pass.
    warned: HashSet<String>,
    /// `dtoverlay -h` answers of this agent run, per overlay name (a
    /// failed probe included).
    probes: HashMap<String, OverlayProbeResult>,
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
    pub log: Arc<EventSink>,
    pub probe: Arc<dyn OverlayProbe>,
    pub apply_rotation: Arc<dyn RotationWriter>,
    /// The sensor catalog (`camera_catalog::catalog()`).
    pub catalog: &'static Catalog,
    /// What is on the ports (the B7 gate), read only when a line is about
    /// to be edited (no sysfs scan on a pass that writes nothing, or with
    /// the namespace absent); `PortEvidence::default()` = the agent could
    /// not look at all, and the gate stays open.
    pub evidence: Arc<dyn Fn() -> PortEvidence + Send + Sync>,
    pub memo: Arc<Mutex<WriteMemo>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// What one camera overlay means for the rotation.
#[derive(Debug, Clone, Copy)]
struct OverlayFacts {
    /// The device-tree rotation of a line with no `rotation=` token.
    default_rotation: u16,
    can_flip: bool,
    /// The overlay is in the sensor catalog.
    catalog: bool,
    /// Outside the catalog and `dtoverlay -h` could not be asked.
    probe_failed: bool,
}

/// The rotation IN FORCE for an enumerated camera, as a MOUNT rotation:
/// what the live device tree says at the camera's node, minus the default
/// of the sensor that node belongs to. A node WITHOUT a `rotation` property
/// is read as the kernel reads it: rotation 0 (an `imx290` node on some
/// kernels, where that is mount 0; on a default-180 sensor it is mount
/// 180). It is never "unreadable". `None` = no camera was enumerated for
/// the stream, or its node cannot be read.
fn in_force_mount(
    o: &ReconcileCameraMountOptions,
    stream_key: &str,
    line_default: Option<u16>,
) -> Option<f64> {
    let name = camera_name_for(&o.pipeline_dir, stream_key)?;
    let node = name.rsplit('/').next().unwrap_or(&name);
    let default = o
        .catalog
        .by_dt_node(node)
        .map(|s| s.default_rotation)
        .or(line_default)
        .unwrap_or(0);
    match dt_rotation_state(&name, &o.dt_root) {
        DtRotation::Value(v) => Some(f64::from(mount_of_rotation(default, v))),
        // As the kernel reads it: no property = rotation 0.
        DtRotation::Absent => Some(f64::from(mount_of_rotation(default, 0))),
        DtRotation::Unknown => None,
    }
}

/// `reconcileCameraMount(opts)` (`boot-config.ts:329-457`), reworked (module
/// docs). `None` = the namespace is ABSENT: nothing is read, compared or
/// written (an installer's hand-set `rotation=180` survives on an
/// unconfigured device). Idempotent: the helper runs only when the port's
/// line gives another rotation than the one wanted.
///
/// Every `detail` below is CLOUD-VISIBLE (the report's `cameraMount`).
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
    let board = read_board(&o.dt_root);
    let mut out: Vec<CameraMountCameraState> = Vec::new();
    let mut wrote = false;

    for stream_key in keys {
        let desired = cameras[stream_key].rotation_deg;
        let mount = desired.degrees() as u16;
        let desired_deg = f64::from(desired.degrees());
        let push = |out: &mut Vec<CameraMountCameraState>,
                    in_force_deg: Option<f64>,
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
                in_force_mount(&o, stream_key, None),
                None,
                CameraMountVerdict::UnknownCamera,
                Some(format!("this device has no camera \"{stream_key}\"")),
            );
            continue;
        };

        let Some(path) = path.as_deref() else {
            push(
                &mut out,
                in_force_mount(&o, stream_key, None),
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
                    in_force_mount(&o, stream_key, None),
                    None,
                    CameraMountVerdict::WriteFailed,
                    Some(format!("cannot read {path}: {message}")),
                );
                continue;
            }
        };

        // The port's camera lines that may take effect on this board. A
        // port other than cam0 / cam1 has none: no camera overlay can
        // target it.
        let scoped = parse_scoped_lines(&text, board);
        let port = CamPort::from_token(&cam_token);
        let lines: Vec<&ScopedLine> = port
            .map(|p| camera_lines_for_port(&scoped, p, o.catalog))
            .unwrap_or_default();
        let (Some(port), Some(last)) = (port, lines.last().copied()) else {
            push(
                &mut out,
                in_force_mount(&o, stream_key, None),
                None,
                CameraMountVerdict::NoOverlay,
                Some(format!(
                    "no dtoverlay line for {cam_token} in {path} — the camera overlay is missing"
                )),
            );
            continue;
        };

        // The port's line (the last one, Q5) and what its overlay means.
        let overlay = last.line.name.clone();
        let facts = overlay_facts(&o, &overlay).await;
        let wanted = rotation_for_mount(facts.default_rotation, mount);
        let boot_deg = line_rotation(&last.line, facts.default_rotation)
            .map(|r| f64::from(mount_of_rotation(facts.default_rotation, u32::from(r))));
        let in_force_deg = in_force_mount(&o, stream_key, Some(facts.default_rotation));

        // A sensor that cannot flip: its picture is as mounted, whatever
        // the line says. Mount 0 is what it shows; 180 cannot be had.
        if !facts.can_flip {
            lock(&o.memo).failed.remove(stream_key);
            // Mount 0 is the only picture it has; reported only where a
            // camera was actually read on the port.
            let shown = in_force_deg.map(|_| 0.0);
            if mount == 0 {
                push(&mut out, shown, shown, CameraMountVerdict::Applied, None);
            } else {
                let detail = if facts.catalog {
                    format!(
                        "the {overlay} sensor cannot be rotated: mount the camera the right way up"
                    )
                } else {
                    format!("the {overlay} overlay on this device has no \"rotation\" parameter")
                };
                push(
                    &mut out,
                    shown,
                    shown,
                    CameraMountVerdict::Unsupported,
                    Some(detail),
                );
            }
            continue;
        }

        // Every line of the port already gives the wanted rotation: nothing
        // to write (an IMX290 device with token-less lines and mount 0).
        let mut settled = true;
        for l in &lines {
            let f = if l.line.name == overlay {
                facts
            } else {
                overlay_facts(&o, &l.line.name).await
            };
            let want = rotation_for_mount(f.default_rotation, mount);
            settled &= f.can_flip && line_rotation(&l.line, f.default_rotation) == Some(want);
        }

        if !settled {
            // Why the line is not edited, when it is not (B7 and its kin).
            let blocked: Option<String> = if facts.probe_failed {
                Some(format!(
                    "could not ask dtoverlay whether {overlay} supports rotation"
                ))
            } else if lines.len() > 1 {
                Some(format!(
                    "config.txt has {} camera lines for {cam_token}; the agent changes a rotation only when exactly one line configures the port: remove the stale ones, then reboot",
                    lines.len()
                ))
            } else if last.scope == LineScope::Unknown {
                Some(format!(
                    "the camera line for {cam_token} in {path} sits under [{}], a section the agent cannot evaluate for this board: it is left as it is; edit it by hand",
                    last.section.as_deref().unwrap_or("")
                ))
            } else if let Some(why) = write_block(&o, stream_key, &text, board) {
                Some(format!(
                    "the camera line for {cam_token} is left as it is, because the camera on this port is not confirmed: {why}"
                ))
            } else {
                line_with_rotation(&last.line, wanted).err().map(|e| {
                    format!("the camera line for {cam_token} in {path} cannot be rewritten: {e}")
                })
            };
            if let Some(detail) = blocked {
                lock(&o.memo).failed.remove(stream_key);
                push(
                    &mut out,
                    in_force_deg,
                    boot_deg,
                    CameraMountVerdict::Unsupported,
                    Some(detail),
                );
                continue;
            }

            // The update window: this build runs, the helper is not there yet.
            if let Some(why) = o.apply_rotation.unavailable() {
                warn_once(&o, stream_key, &why);
                push(
                    &mut out,
                    in_force_deg,
                    boot_deg,
                    CameraMountVerdict::WriteFailed,
                    Some(format!("could not edit {path}: {why}")),
                );
                continue;
            }

            // A write that failed a moment ago is not repeated every pass.
            let asked = CamCfgOp::Rotate {
                port,
                overlay: overlay.clone(),
                rotation: wanted,
            }
            .args();
            let recent = lock(&o.memo)
                .failed
                .get(stream_key)
                .filter(|(args, when, _)| {
                    *args == asked && (at - when).abs() < RETRY_FAILED_WRITE_MS
                })
                .map(|(_, _, detail)| detail.clone());
            if let Some(detail) = recent {
                push(
                    &mut out,
                    in_force_deg,
                    boot_deg,
                    CameraMountVerdict::WriteFailed,
                    Some(detail),
                );
                continue;
            }

            let res = o.apply_rotation.apply(port, &overlay, wanted).await;
            if !res.ok {
                // The update hint only where an update is the remedy: sudo
                // itself refused the call (no grant: "sudo: a password is
                // required"; no helper: "sudo: …: command not found"). A
                // refusal by the helper (exit 4, 5) or a failed write is not
                // cured by an update.
                let detail = if res.detail.starts_with("sudo:") {
                    format!(
                        "could not edit {path}: {} (the agent needs the xorgate-camcfg helper — run `sudo xorgate-rs update`)",
                        res.detail
                    )
                } else {
                    format!("could not edit {path}: {}", res.detail)
                };
                lock(&o.memo)
                    .failed
                    .insert(stream_key.clone(), (asked, at, detail.clone()));
                push(
                    &mut out,
                    in_force_deg,
                    boot_deg,
                    CameraMountVerdict::WriteFailed,
                    Some(detail),
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
                // On a default-0 overlay (IMX290) the rotation written IS
                // the mount: the TS sentence, verbatim.
                let what = if facts.default_rotation == 0 {
                    format!("rotation set to {mount}°")
                } else {
                    format!(
                        "mount set to {mount}° (rotation={wanted} written: the {overlay} overlay defaults to {})",
                        facts.default_rotation
                    )
                };
                (o.log)(
                    EventLevel::Info,
                    &format!(
                        "{stream_key}: {overlay}/{cam_token} {what} in {path} — REBOOT required before the image changes (the agent never reboots itself)"
                    ),
                );
            }
        }
        {
            let mut memo = lock(&o.memo);
            memo.failed.remove(stream_key);
            let mine = format!("{stream_key}|");
            memo.warned.retain(|w| !w.starts_with(&mine));
        }

        let boot_deg = Some(desired_deg);
        // The overlay's default could not be asked (outside the catalog,
        // `dtoverlay -h` failing): 0 was assumed, as the TS always did. An
        // "in force" that agrees is reported; one that does not is not
        // turned into "reboot required" on the strength of an assumption.
        if facts.probe_failed && in_force_deg != Some(desired_deg) {
            push(
                &mut out,
                in_force_deg,
                boot_deg,
                CameraMountVerdict::Unsupported,
                Some(format!(
                    "could not ask dtoverlay whether {overlay} supports rotation"
                )),
            );
            continue;
        }
        match in_force_deg {
            None => push(
                &mut out,
                None,
                boot_deg,
                CameraMountVerdict::Unconfirmed,
                Some(
                    "could not read the rotation currently in force from the device tree"
                        .to_string(),
                ),
            ),
            Some(f) => push(
                &mut out,
                in_force_deg,
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

/// What an overlay means for the rotation: the catalog's entry, else what
/// `dtoverlay -h` says (Q6: an overlay outside the catalog is never
/// rendered by the agent; its rotation token may be set only when the
/// overlay takes the parameter). The default of such an overlay is the one
/// its help text states, else 0 (the TS assumption).
async fn overlay_facts(o: &ReconcileCameraMountOptions, overlay: &str) -> OverlayFacts {
    if let Some(s) = o.catalog.by_overlay(overlay) {
        return OverlayFacts {
            default_rotation: s.default_rotation,
            can_flip: s.can_flip,
            catalog: true,
            probe_failed: false,
        };
    }
    let cached = lock(&o.memo).probes.get(overlay).copied();
    let answer = match cached {
        Some(a) => a,
        None => {
            // Asked once per agent run, whatever the answer (a failing
            // `dtoverlay -h` is not run again every 60 s).
            let a = o.probe.probe(overlay).await;
            lock(&o.memo).probes.insert(overlay.to_string(), a);
            a
        }
    };
    OverlayFacts {
        default_rotation: answer.default_rotation.unwrap_or(0),
        can_flip: !answer.ok || answer.supports,
        catalog: false,
        probe_failed: !answer.ok,
    }
}

/// The B7 gate for one stream (`camera_truth::rotation_write_block`), over
/// this pass's evidence. `None` = the line may be edited. A stream the
/// hardware block does not list has no port to judge (a v1 device), and a
/// pass with no evidence at all cannot judge: both stay open.
fn write_block(
    o: &ReconcileCameraMountOptions,
    stream_key: &str,
    text: &str,
    board: Board,
) -> Option<String> {
    let video = o
        .hardware
        .as_ref()?
        .video
        .iter()
        .find(|v| v.key == stream_key)?;
    let evidence = (o.evidence)();
    let listed = evidence.cameras.as_deref().and_then(|cams| {
        cams.iter()
            .find(|c| c.connector.as_deref() == Some(video.connector.as_str()))
    });
    rotation_write_block(&PortFacts {
        stream_key,
        connector: &video.connector,
        model_sensor: &video.sensor,
        boot_text: Some(text),
        board,
        nodes: evidence.nodes.as_deref(),
        enumerated: evidence.cameras.is_some(),
        relisted: evidence.relisted,
        listed: listed.map(|c| (c.sensor.as_str(), c.camera_name.as_str())),
        capture: CaptureTruth::Off,
        backoff_step: 0,
        last_error: None,
        stalled_preset: None,
        unsupervised: false,
        off_reason: CaptureOff::NotBegun,
    })
}

/// One warning per (stream, reason), not one per pass.
fn warn_once(o: &ReconcileCameraMountOptions, stream_key: &str, why: &str) {
    if lock(&o.memo).warned.insert(format!("{stream_key}|{why}")) {
        (o.log)(
            EventLevel::Warn,
            &format!("{stream_key}: boot config rotation edit failed — {why}"),
        );
    }
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
    /// The sensor catalog (`camera_catalog::catalog()`).
    pub catalog: &'static Catalog,
    /// What the kernel and libcamera say is on the ports, read per pass
    /// (the B7 gate). Production: the hardware reconcile's listing + the
    /// sysfs scan.
    pub evidence: EvidenceFn,
    /// The worker's `log` (`worker.ts:176-181`): see [`camera_mount_event_sink`].
    pub log: Arc<EventSink>,
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
    /// Failed writes, logged warnings and probe answers, across passes.
    memo: Arc<Mutex<WriteMemo>>,
    /// `cameraMountState`: snapshot-only access (safe under ConfigSync's mutex).
    state: Mutex<Option<CameraMountState>>,
    /// `requestConfigReport?.("boot")`.
    on_change: Box<dyn Fn() + Send + Sync>,
}

impl CameraMountReconciler {
    /// `on_change` = `requestConfigReport("boot")`: called when the summary
    /// moved (`worker.ts:191-193`) — "reboot required" must not wait for the
    /// hourly heartbeat.
    pub fn new(opts: CameraMountReconcilerOptions, on_change: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(ReconcilerInner {
                opts,
                memo: Arc::default(),
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
                let hardware = (o.hardware)();
                let (reader, block) = (task.clone(), hardware.clone());
                let opts = ReconcileCameraMountOptions {
                    settings: (o.settings)(),
                    evidence: Arc::new(move || (reader.opts.evidence)(block.as_ref())),
                    hardware,
                    catalog: o.catalog,
                    memo: task.memo.clone(),
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
) -> Arc<EventSink> {
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
