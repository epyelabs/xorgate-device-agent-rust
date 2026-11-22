//! The per-port camera report ("tell the truth", support-other-camera-models
//! plan, Phase 0). RUST-ONLY: the JS agent has no counterpart, so nothing
//! here is a port and nothing here may change a JS-parity output.
//!
//! For every camera the device model declares (`hardware.video[]`, one per
//! stream key) the agent reports five things and one verdict:
//!
//! - `declared`: which sensor is supposed to be on the port, and who says
//!   so. Sources in order: the boot config line, then the model's hardware
//!   block. (Phase 2 adds the `cameraSensors` namespace as the FIRST source:
//!   one more [`DeclaredSource`] variant and one more entry in the list
//!   handed to [`resolve_declared`].)
//! - `bootConfig`: what `/boot/firmware/config.txt` says about the port.
//! - `bound`: what the running kernel and libcamera found. "A driver is
//!   bound on the port" and "libcamera lists a camera" are kept apart,
//!   because they differ on real devices (an `imx335` overlay with stock
//!   libcamera: driver bound, no camera listed).
//! - `capture`: whether the capture process delivers.
//! - `verdict` + `detail`: one word and one sentence a developer can act on.
//!
//! The block is carried as `effective.cameras` (reported-only, like `sim` /
//! `uplink` / `apn`) and as `cameras` in state.json (for `xorgate-rs status`).
//!
//! Everything here is pure: the config.txt text, the sysfs scan, the camera
//! listing and the capture supervisor's state are handed in.

use serde::{Deserialize, Serialize};

use crate::boot_config::{
    line_is_ports, parse_scoped_lines, port_of_line, Board, CamPort, LineScope, OverlayLine,
    ScopedLine,
};
use crate::camera_catalog::{catalog, Sensor};
use crate::camera_sensors::LineChange;
use crate::capture_ladder::CaptureState;
use crate::settings::CAMERA_SENSOR_NONE;
use crate::stream_guard::StreamHardwareStatus;

// ---------------------------------------------------------------------------
// The sensor catalog (it replaced Phase 0's temporary overlay table)
// ---------------------------------------------------------------------------

/// How far "a driver is bound" proves which chip is attached: a fact of the
/// sensor catalog (`camera-catalog.json`, `identity`).
pub use crate::camera_catalog::Identity;

/// The catalog entry of an overlay name, `None` for anything else. An
/// overlay OUTSIDE the catalog is never judged: this build knows neither
/// its device-tree node name nor what libcamera lists it as (`imx327` and
/// `imx462` list as `imx290`; `arducam-pivariety` names its node
/// differently), so nothing is inferred from a name that differs.
pub fn camera_overlay(name: &str) -> Option<&'static Sensor> {
    catalog().by_overlay(name)
}

/// The sensor a libcamera listing name stands for: libcamera appends the
/// module variant to some sensors (`imx708_wide`, `imx708_noir`,
/// `imx708_wide_noir`, `imx219_noir`, `imx477_noir`…), and every variant is
/// the same sensor as far as the declaration goes. Used for a sensor the
/// catalog does not hold; a catalog sensor lists its own names.
pub fn sensor_base(listed: &str) -> &str {
    for suffix in ["_wide_noir", "_noir", "_wide"] {
        if let Some(base) = listed.strip_suffix(suffix) {
            if !base.is_empty() {
                return base;
            }
        }
    }
    listed
}

/// Is the camera libcamera lists the sensor that was declared?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorMatch {
    Same,
    Different,
    /// The declared name is outside the catalog and differs from the listed
    /// one: it may be another sensor, or an overlay served by another
    /// driver. This build cannot tell, so it claims nothing.
    Unknown,
}

/// Compare a declared sensor (an overlay name from config.txt, or the
/// model's sensor) with the name libcamera lists.
///
/// - Declared IN the catalog: the same sensor when the listed name is one
///   of the entry's `libcameraNames` (its variants included), or a variant
///   spelling of one of them; otherwise a different one.
/// - Declared OUTSIDE the catalog: the same when the names agree (variant
///   suffixes aside); otherwise [`SensorMatch::Unknown`].
pub fn sensor_match(declared: &str, listed: &str) -> SensorMatch {
    let base = sensor_base(listed);
    if let Some(s) = catalog().by_overlay(declared).or(catalog().by_id(declared)) {
        let same = s.libcamera_names.iter().any(|n| n == listed || n == base);
        return if same {
            SensorMatch::Same
        } else {
            SensorMatch::Different
        };
    }
    if base == declared || base == sensor_base(declared) {
        SensorMatch::Same
    } else {
        SensorMatch::Unknown
    }
}

/// `true` unless the listed camera is KNOWN to be another sensor than the
/// overlay `declared` names ([`SensorMatch::Different`]).
pub fn same_sensor(declared: &str, listed: &str) -> bool {
    sensor_match(declared, listed) != SensorMatch::Different
}

// ---------------------------------------------------------------------------
// config.txt: which line belongs to which port
// ---------------------------------------------------------------------------

/// What config.txt says about one port (the wire `bootConfig`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootConfigPort {
    /// The overlay of the port's line; `null` when no line targets the port.
    /// With several lines the LAST one is reported (the later overlay
    /// rewrites the port's endpoint, clock and regulator).
    pub overlay: Option<String>,
    /// That line as written in the file (trimmed, at most
    /// [`BOOT_LINE_MAX`] characters).
    pub line: Option<String>,
    /// How many camera lines target the port. More than one is a fault.
    pub lines: usize,
}

/// The longest config.txt line the report carries.
pub const BOOT_LINE_MAX: usize = 200;

/// The camera lines of `text` (a config.txt) that target `connector`, for
/// the overlays in the catalog only, with the board unknown: commented
/// lines are skipped, lines under `[all]` or no section count, and so do
/// lines under any other `[section]` (they may apply).
pub fn boot_config_port(text: &str, connector: &str) -> BootConfigPort {
    boot_config_port_on(text, connector, &[], Board::Unknown)
}

/// [`boot_config_port`], also counting a line whose overlay is NOT in the
/// catalog when something else says it is this port's camera line (see
/// [`boot_config_port_on`]).
pub fn boot_config_port_with(text: &str, connector: &str, hints: &[&str]) -> BootConfigPort {
    boot_config_port_on(text, connector, hints, Board::Unknown)
}

/// What config.txt says about `connector` on `board`.
///
/// A line counts as the port's when it may take effect on this board (THE
/// SECTION RULE, `boot_config::parse_scoped_lines`: a line under a model
/// filter that names the other Compute Module family is left out) and it is
/// the port's camera line:
///
/// - its overlay is in the catalog and it targets the port (`cam0` =
///   `csi0`; anything else = `csi1`); or
/// - its overlay is NOT in the catalog, and it carries the port's explicit
///   token (`cam0`, or `cam1` without `cam0`), or its overlay is one of
///   `hints`, the sensor names the kernel bound or libcamera lists on the
///   port. Without this a device on, say, `dtoverlay=ov5647,cam0` would
///   read "no line for this port" and the model would be taken for the
///   declarer.
pub fn boot_config_port_on(
    text: &str,
    connector: &str,
    hints: &[&str],
    board: Board,
) -> BootConfigPort {
    let lines = parse_scoped_lines(text, board);
    let mine = port_camera_lines(&lines, connector, hints);
    BootConfigPort {
        overlay: mine.last().map(|l| l.line.name.clone()),
        line: mine
            .last()
            .map(|l| l.line.text.trim().chars().take(BOOT_LINE_MAX).collect()),
        lines: mine.len(),
    }
}

/// The lines of a parsed config.txt that count as `connector`'s camera
/// lines for the report (see [`boot_config_port_on`]).
fn port_camera_lines<'a>(
    lines: &'a [ScopedLine],
    connector: &str,
    hints: &[&str],
) -> Vec<&'a ScopedLine> {
    lines
        .iter()
        .filter(|l| l.scope != LineScope::NotApplies && line_targets(&l.line, connector, hints))
        .collect()
}

/// The `[section]` the port's reported line sits under, when it is one the
/// agent cannot evaluate for this board ([`LineScope::Unknown`]): the line
/// may or may not take effect, and the agent does not edit it.
pub fn unknown_section_of(
    text: &str,
    connector: &str,
    hints: &[&str],
    board: Board,
) -> Option<String> {
    let lines = parse_scoped_lines(text, board);
    let last = port_camera_lines(&lines, connector, hints).pop()?;
    (last.scope == LineScope::Unknown).then(|| last.section.clone().unwrap_or_default())
}

/// Does `line` configure `connector`'s camera? (See [`boot_config_port_on`].)
fn line_targets(line: &OverlayLine, connector: &str, hints: &[&str]) -> bool {
    let Some(port) = CamPort::from_connector(connector) else {
        return false;
    };
    line_is_ports(line, port, catalog())
        || (camera_overlay(&line.name).is_none()
            && port_of_line(line) == port
            && hints.contains(&line.name.as_str()))
}

// ---------------------------------------------------------------------------
// declared
// ---------------------------------------------------------------------------

/// Who declared the sensor of a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeclaredSource {
    /// The `cameraSensors` setting (Phase 2: the port is managed).
    Config,
    /// The port's `dtoverlay=` line in config.txt.
    BootConfig,
    /// The device model's hardware block (`hardware.video[].sensor`).
    Model,
}

impl DeclaredSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DeclaredSource::Config => "config",
            DeclaredSource::BootConfig => "boot-config",
            DeclaredSource::Model => "model",
        }
    }

    /// How the source reads in a sentence.
    fn words(self) -> &'static str {
        match self {
            DeclaredSource::Config => "the camera sensor setting",
            DeclaredSource::BootConfig => "config.txt",
            DeclaredSource::Model => "the device model",
        }
    }
}

/// The wire `declared`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declared {
    pub sensor: String,
    pub source: DeclaredSource,
}

impl Declared {
    /// Does libcamera list ANOTHER sensor than the declared one?
    ///
    /// - Declared by a config.txt line: only when it is KNOWN to be another
    ///   ([`sensor_match`]). An overlay outside the catalog may be served by
    ///   another driver, so a name that differs proves nothing.
    /// - Declared by the model: the model names a SENSOR, not an overlay, so
    ///   a name the catalog cannot place is compared as a name (the TS
    ///   warning `model expects X …, enumerated Y`, `hardware-reconcile.ts`,
    ///   which the hardware reconcile still logs word for word).
    pub fn differs_from(&self, listed: &str) -> bool {
        match (sensor_match(&self.sensor, listed), self.source) {
            (SensorMatch::Same, _) => false,
            (SensorMatch::Different, _) => true,
            (SensorMatch::Unknown, DeclaredSource::Model) => true,
            (SensorMatch::Unknown, DeclaredSource::BootConfig | DeclaredSource::Config) => false,
        }
    }
}

/// The first source that names a sensor wins. The caller lists the sources
/// in priority order (Phase 0: boot config, model).
pub fn resolve_declared(sources: &[(DeclaredSource, Option<&str>)]) -> Option<Declared> {
    sources.iter().find_map(|(source, sensor)| {
        let s = sensor.map(str::trim).filter(|s| !s.is_empty())?;
        Some(Declared {
            sensor: s.to_string(),
            source: *source,
        })
    })
}

/// The declaration of an UNMANAGED port: the boot config line's overlay,
/// else the model's sensor.
pub fn declared_for(boot: Option<&BootConfigPort>, model_sensor: &str) -> Option<Declared> {
    declared_with(None, boot, model_sensor)
}

/// The declaration of a port: the `cameraSensors` setting when it names a
/// sensor for the port (`none` included), else the boot config line's
/// overlay, else the model's sensor.
pub fn declared_with(
    setting: Option<&str>,
    boot: Option<&BootConfigPort>,
    model_sensor: &str,
) -> Option<Declared> {
    resolve_declared(&[
        (DeclaredSource::Config, setting),
        (
            DeclaredSource::BootConfig,
            boot.and_then(|b| b.overlay.as_deref()),
        ),
        (DeclaredSource::Model, Some(model_sensor)),
    ])
}

// ---------------------------------------------------------------------------
// bound
// ---------------------------------------------------------------------------

/// One sensor node the live device tree holds on a camera port, as the
/// sysfs scan found it (`xorgate-hal::camera_probe`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortNode {
    /// `csi0` / `csi1`.
    pub connector: String,
    /// The device-tree node name, e.g. `imx708@1a`.
    pub node: String,
    /// The kernel driver bound to it, `None` when its probe failed.
    pub driver: Option<String>,
}

/// The sensor a device-tree node name stands for (`imx708@1a` → `imx708`).
pub fn node_sensor(node: &str) -> &str {
    node.split('@').next().unwrap_or(node)
}

/// The port's node to report: a bound one first, else the first found.
pub fn node_for_port<'a>(nodes: &'a [PortNode], connector: &str) -> Option<&'a PortNode> {
    let mut mine = nodes.iter().filter(|n| n.connector == connector);
    let first = mine.next()?;
    if first.driver.is_some() {
        return Some(first);
    }
    Some(mine.find(|n| n.driver.is_some()).unwrap_or(first))
}

/// The wire `bound`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bound {
    /// A kernel driver is bound to a sensor node on this port. `null` = the
    /// agent could not look (no sysfs on this host).
    pub driver: Option<bool>,
    /// The port's sensor node in the live device tree (`imx708@1a`), `null`
    /// when no camera overlay is loaded for the port.
    pub node: Option<String>,
    /// The sensor libcamera lists on this port, `null` when it lists none.
    pub listed: Option<String>,
    /// That camera's libcamera path.
    pub path: Option<String>,
    /// How far `driver: true` proves the chip, for the driver in use.
    pub identity: Option<Identity>,
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

/// The wire `capture.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureTruth {
    /// The capture process is up and has been delivering.
    Streaming,
    /// A capture process was launched and has not failed; not stable yet.
    Starting,
    /// The capture process keeps dying, stalls, or is parked in a back-off.
    Failing,
    /// No capture process: no camera was found on the port, so none is
    /// launched (the stream is held).
    Absent,
    /// No capture process by choice: the camera is disabled, video is off,
    /// the stream's pipeline is not the agent's, or capture has not begun.
    Off,
}

impl CaptureTruth {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureTruth::Streaming => "streaming",
            CaptureTruth::Starting => "starting",
            CaptureTruth::Failing => "failing",
            CaptureTruth::Absent => "absent",
            CaptureTruth::Off => "off",
        }
    }
}

/// After a failure a camera counts as streaming again only once a capture
/// process has been stable this long. Without it a camera that crashes
/// every half minute would flip the report twice a minute.
pub const RECOVER_MS: u64 = 60_000;

/// What the capture supervisor knows about one camera's run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureLive {
    pub state: CaptureState,
    /// Record mode (the child writes fragments) as opposed to bridge-only.
    pub record: bool,
    /// Record mode only: the current child has put bytes on a fragment.
    pub bytes_seen: bool,
    /// A child died or stalled while it was wanted, and no child has been
    /// stable for [`RECOVER_MS`] since.
    pub unstable: bool,
    /// How long the current child has been past the stable window; `None`
    /// when there is no stable child.
    pub stable_ms: Option<u64>,
    /// The start-failure back-off step (D86), 0 outside it.
    pub backoff_step: u32,
    /// The error line of the last child that printed one
    /// ([`first_error_line`]).
    pub last_error: Option<String>,
    /// The last failure was a STALL (the child ran, wrote nothing and the
    /// progress watchdog killed it): the video preset it ran.
    pub stalled_preset: Option<String>,
}

/// Has a run recovered from its failures? (The supervisor clears `unstable`
/// when this holds.)
pub fn capture_recovered(live: &CaptureLive) -> bool {
    live.state == CaptureState::Running
        && live.stable_ms.is_some_and(|ms| ms >= RECOVER_MS)
        && (!live.record || live.bytes_seen)
}

/// The capture state of one camera, from its run (`None` = the supervisor
/// has no run for it) and whether the port was held for lack of a camera.
pub fn capture_truth(live: Option<&CaptureLive>, held_no_camera: bool) -> CaptureTruth {
    let Some(l) = live else {
        return if held_no_camera {
            CaptureTruth::Absent
        } else {
            CaptureTruth::Off
        };
    };
    match l.state {
        CaptureState::Stopped => CaptureTruth::Off,
        CaptureState::Retrying | CaptureState::Failed => CaptureTruth::Failing,
        CaptureState::Starting | CaptureState::Running => {
            if l.unstable && !capture_recovered(l) {
                return CaptureTruth::Failing;
            }
            let delivering = l.state == CaptureState::Running
                && l.stable_ms.is_some()
                && (!l.record || l.bytes_seen);
            if delivering {
                CaptureTruth::Streaming
            } else {
                CaptureTruth::Starting
            }
        }
    }
}

/// How long a camera that was streaming may read `starting` or `off`
/// before the report says so. A planned restart (recording switched on at
/// the end of the boot gate, a preset change, a shed rung released) stops
/// the capture and starts it again within this time.
pub const RESTART_GRACE_MS: u64 = 30_000;

/// The capture state the report carries, given the one it carried before:
/// a camera that was streaming and is merely being restarted stays
/// `streaming` for up to [`RESTART_GRACE_MS`] instead of flashing `off` and
/// `starting` (two reports per restart otherwise). `soft_ms` is how long the
/// live state has been `starting` / `off` since it was last `streaming`.
/// A failure, or a port that lost its camera, shows at once.
pub fn settle_capture(
    prev: Option<CaptureTruth>,
    live: CaptureTruth,
    soft_ms: u64,
) -> CaptureTruth {
    match (prev, live) {
        (Some(CaptureTruth::Streaming), CaptureTruth::Starting | CaptureTruth::Off)
            if soft_ms < RESTART_GRACE_MS =>
        {
            CaptureTruth::Streaming
        }
        _ => live,
    }
}

/// The longest error line the report carries.
pub const LAST_ERROR_MAX: usize = 240;

/// `ERROR` lines of a capture child that are never the cause, as
/// substrings. DATA: extend it with evidence (a real log), never by guess.
///
/// - `Unable to set controls: Device or resource busy`: libcamera, about the
///   OTHER camera of a two-camera device, printed FIRST by every child
///   while that camera streams (bench 2, 2026-11-21: `'imx219 0-0010'` in
///   cam0's child).
/// - `pipeline doesn't want to preroll.`: gst-launch's follow-up to the
///   real error.
pub const ERROR_LINES_SKIPPED: &[&str] = &[
    "Unable to set controls: Device or resource busy",
    "pipeline doesn't want to preroll.",
];

/// Is this child output line an error line worth reporting? libcamera and
/// gst-launch both mark theirs with `ERROR`; the ones on
/// [`ERROR_LINES_SKIPPED`] are not. Returns the line trimmed and cut to
/// [`LAST_ERROR_MAX`] characters.
pub fn error_line(line: &str) -> Option<String> {
    let plain = strip_ansi(line);
    let t = plain.trim();
    if !t.contains("ERROR") || ERROR_LINES_SKIPPED.iter().any(|s| t.contains(s)) {
        return None;
    }
    Some(t.chars().take(LAST_ERROR_MAX).collect())
}

/// `line` without ANSI escape sequences. libcamera colours its log lines
/// when it thinks it writes to a terminal (`ESC[1;31mERROR ESC[1;37mRPI …`,
/// seen on the benches in state.json and in the cloud): the report carries
/// plain text, and the lists above and below match plain text.
pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // `ESC [ … <final byte @..~>` (a CSI sequence); any other escape is
        // `ESC` plus one character.
        if chars.next() == Some('[') {
            for d in chars.by_ref() {
                if ('@'..='~').contains(&d) {
                    break;
                }
            }
        }
    }
    out
}

/// Lines after which a capture child is NOT delivering, even while it stays
/// alive, as substrings. DATA: extend it with evidence, never by guess.
///
/// - `Camera frontend has timed out!`: libcamera got no frame from the
///   sensor. Bench 2, 2026-11-21 (`imx290` overlay on an IMX335): the child
///   printed it one second after start, did not exit, spun at 188 % of a
///   core for 43 s, and the report read `ok` / "detected and streaming".
pub const ERROR_LINES_FATAL: &[&str] = &["Camera frontend has timed out!"];

/// [`error_line`] for a line on [`ERROR_LINES_FATAL`]: the child that
/// printed it counts as failing for the REPORT from that moment (the
/// supervisor's own ladder is not changed by it).
pub fn fatal_error_line(line: &str) -> Option<String> {
    let e = error_line(line)?;
    ERROR_LINES_FATAL.iter().any(|f| e.contains(f)).then_some(e)
}

/// Error lines that say the camera is held by another process (`EBUSY` at
/// libcamera's `acquire()`), as substrings. DATA. Bench 2, 2026-11-21:
/// `Camera '…imx335@1a' is already in use.` while the other camera's
/// capture, started before the libcamera packages were changed, still held
/// the media device.
pub const ERROR_LINES_IN_USE: &[&str] = &["is already in use"];

/// The line that stands for one child's failure: its FIRST error line
/// ([`error_line`]). The later ones are advice ("Please check that your
/// camera sensor connector is attached securely.") and consequences
/// (shmsink's "Failed waiting on fd activity"), so the last line is never
/// the cause on a real log.
pub fn first_error_line<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<String> {
    lines.into_iter().find_map(error_line)
}

/// The wire `capture`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureInfo {
    pub state: CaptureTruth,
    /// The start-failure back-off step, 0 outside the back-off.
    pub backoff_step: u32,
    /// The last error line of a failed capture process, `null` otherwise.
    pub last_error: Option<String>,
}

/// B5: may a capture process be launched for a stream? Not when a reconcile
/// pass that could enumerate cameras (`armed`) found none on the stream's
/// port: the camera name left in `<key>.camera` / `<key>.gst.direct` then
/// belongs to a camera that is not there. Fail open in every other case
/// (no pass yet, enumeration unavailable, a stream the model does not list).
pub fn capture_allowed(armed: bool, status: Option<StreamHardwareStatus>) -> bool {
    !(armed && status == Some(StreamHardwareStatus::NoCamera))
}

// ---------------------------------------------------------------------------
// verdict
// ---------------------------------------------------------------------------

/// The wire `verdict`. Phase 0 produced the first seven; Phase 2 adds
/// `requirements-missing` and `unknown-sensor`. A later phase may add more,
/// so a reader must tolerate a value it does not know. (There is no
/// `unmanaged` verdict: a port without a sensor setting reports as it did
/// before, with `declared.source` `boot-config` or `model`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraVerdict {
    /// The declared sensor is listed and capture is not failing.
    Ok,
    /// A camera is listed, but it is not the declared sensor.
    Mismatch,
    /// config.txt's line for the port names another overlay than the one
    /// the running kernel has loaded: the edit takes effect at the next
    /// reboot.
    RebootRequired,
    /// The port is configured, yet libcamera lists no camera on it.
    NotDetected,
    /// A camera is there, but its capture process keeps failing or stalls.
    NoFrames,
    /// Nothing is configured on the port and nothing is there.
    Empty,
    /// The agent cannot check the port: it cannot list cameras on this
    /// device (and the camera is not streaming), or the stream's pipeline
    /// is not one it supervises. Not a fault by itself.
    Unchecked,
    /// The sensor setting names a sensor whose requirements the device does
    /// not meet (extra packages not installed in the required versions, or
    /// another OS release), and the camera is not listed.
    RequirementsMissing,
    /// The sensor setting names a sensor this build's catalog lacks.
    UnknownSensor,
}

impl CameraVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            CameraVerdict::Ok => "ok",
            CameraVerdict::Mismatch => "mismatch",
            CameraVerdict::RebootRequired => "reboot-required",
            CameraVerdict::NotDetected => "not-detected",
            CameraVerdict::NoFrames => "no-frames",
            CameraVerdict::Empty => "empty",
            CameraVerdict::Unchecked => "unchecked",
            CameraVerdict::RequirementsMissing => "requirements-missing",
            CameraVerdict::UnknownSensor => "unknown-sensor",
        }
    }
}

/// Everything known about one port when the verdict is derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortFacts<'a> {
    pub stream_key: &'a str,
    pub connector: &'a str,
    /// The model's sensor for the port (`hardware.video[].sensor`).
    pub model_sensor: &'a str,
    /// The text of config.txt; `None` = it could not be found or read.
    pub boot_text: Option<&'a str>,
    /// The board, for config.txt's `[section]` filters
    /// (`boot_config::board_from_model`).
    pub board: Board,
    /// The sysfs scan; `None` = it could not run.
    pub nodes: Option<&'a [PortNode]>,
    /// A reconcile pass could list cameras (`false` = `rpicam-hello` is
    /// missing or libcamera is broken).
    pub enumerated: bool,
    /// Cameras were listed AGAIN after the boot listing. Until then "the
    /// driver is bound and libcamera lists nothing" is a boot-time
    /// observation the agent is still re-checking, not a finding.
    pub relisted: bool,
    /// The camera libcamera lists on this port: `(sensor, path)`.
    pub listed: Option<(&'a str, &'a str)>,
    pub capture: CaptureTruth,
    pub backoff_step: u32,
    pub last_error: Option<&'a str>,
    /// The capture fails by STALLING (it runs, writes nothing and the
    /// progress watchdog kills it): the video preset it runs. Worded as a
    /// stall only when `last_error` is `None`.
    pub stalled_preset: Option<&'a str>,
    /// The stream's `.gst` exists and is not the bridged kind: the KVS
    /// master owns the camera and the agent's capture never runs it.
    pub unsupervised: bool,
    /// Why capture is off, when it is (`capture` = `off`).
    pub off_reason: CaptureOff,
    /// What the `cameraSensors` namespace and its machinery add (Phase 2).
    /// `PortSetting::default()` = an unmanaged port with nothing pending:
    /// the report as it was before.
    pub setting: PortSetting<'a>,
}

/// The Phase 2 facts of a port.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortSetting<'a> {
    /// The sensor id the `cameraSensors` setting declares for the port
    /// (`none` = declared empty; an id the catalog lacks included). `None`
    /// = the port is unmanaged.
    pub sensor: Option<&'a str>,
    /// The port's camera line changed since this boot
    /// (`camera_sensors::changed_since_boot`): it waits for a reboot.
    pub line_change: Option<&'a LineChange>,
    /// What the declared sensor needs and the device does not have
    /// (`camera_sensors::missing_requirements`).
    pub missing: Option<&'a str>,
    /// The video preset capture runs on this device.
    pub preset: Option<&'a str>,
}

/// Why a camera has no capture process, as far as the agent knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOff {
    /// The camera is turned off in `recording.video.cameras`.
    Disabled,
    /// Media is off on this device (no capture for any camera).
    VideoOff,
    /// It is wanted and simply has not been started yet.
    NotBegun,
}

/// One camera's report (one value of the wire `effective.cameras` map).
/// Field order is the wire key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraPort {
    pub connector: String,
    pub declared: Option<Declared>,
    pub boot_config: Option<BootConfigPort>,
    pub bound: Bound,
    pub capture: CaptureInfo,
    pub verdict: CameraVerdict,
    /// One sentence a developer can act on.
    pub detail: String,
}

/// How far the driver IN USE on a port proves the chip: the driver bound to
/// the port's node first (that is what runs, whatever config.txt says now),
/// else the boot config's overlay, else what libcamera lists. `None` when
/// no driver is in use there, or the sensor is not in the catalog.
fn identity_of(
    node: Option<&PortNode>,
    boot: Option<&BootConfigPort>,
    listed: Option<&str>,
) -> Option<Identity> {
    let c = catalog();
    let sensor = if let Some(n) = node.filter(|n| n.driver.is_some()) {
        c.by_dt_node(&n.node)
    } else if let Some(o) = boot.and_then(|b| b.overlay.as_deref()) {
        c.by_overlay(o)
    } else {
        listed.and_then(|l| {
            c.by_libcamera_name(l)
                .or_else(|| c.by_libcamera_name(sensor_base(l)))
        })
    };
    sensor.map(|s| s.identity)
}

/// What to add to a sentence about a driver that cannot prove the chip. A
/// driver without any identity check lists a camera on an EMPTY port too.
fn identity_note(identity: Option<Identity>, driver: &str) -> String {
    match identity {
        Some(Identity::None) => format!(
            " The {driver} driver cannot confirm which chip is attached, so the camera on this port may be a different sensor, or nothing may be attached."
        ),
        Some(Identity::Weak) => format!(
            " The {driver} driver only weakly checks which chip is attached, so the camera on this port may be a different sensor."
        ),
        _ => String::new(),
    }
}

/// The facts of one port, resolved once for [`verdict_of`].
struct Resolved<'a> {
    f: &'a PortFacts<'a>,
    boot: Option<&'a BootConfigPort>,
    declared: Option<&'a Declared>,
    /// The port's node to report (a bound one first).
    node: Option<&'a PortNode>,
    /// Every sensor node of the port.
    port_nodes: &'a [&'a PortNode],
    identity: Option<Identity>,
}

impl Resolved<'_> {
    /// The driver the sentences name: the bound driver, else the node's
    /// sensor, else what libcamera lists.
    fn driver(&self) -> &str {
        self.node
            .and_then(|n| n.driver.as_deref())
            .or(self.node.map(|n| node_sensor(&n.node)))
            .or(self.f.listed.map(|(s, _)| sensor_base(s)))
            .unwrap_or("camera")
    }

    /// config.txt was edited and the device not rebooted: the line declares
    /// a sensor whose overlay is not in the running device tree, while
    /// another overlay's node is. Returns that other overlay's sensor.
    fn stale_overlay(&self) -> Option<&str> {
        self.declared
            .filter(|d| d.source != DeclaredSource::Model)?;
        // The overlay of the port's LINE (for an unmanaged port that is the
        // declared sensor itself). Only for an overlay in the catalog:
        // outside it the node's name is not known (`arducam-pivariety` and
        // its like name their node differently), so a name that differs
        // proves nothing.
        let want = &camera_overlay(self.boot?.overlay.as_deref()?)?.dt_node_names;
        let node = self.node?;
        let loaded = self
            .port_nodes
            .iter()
            .any(|n| want.iter().any(|w| w == node_sensor(&n.node)));
        (!loaded).then(|| node_sensor(&node.node))
    }

    /// "This agent build does not know the overlay of the port's line."
    fn unknown_overlay_note(&self) -> String {
        match self.boot.and_then(|b| b.overlay.as_deref()) {
            Some(o) if camera_overlay(o).is_none() => format!(
                " This agent build does not know the {o} overlay; the camera is used as found."
            ),
            _ => String::new(),
        }
    }
}

/// Derive one port's report from its facts. Pure; the table of cases:
///
/// | cameras can be listed | listed on the port | else | verdict |
/// |---|---|---|---|
/// | no | — | capture streaming | `ok` |
/// | no | — | capture failing | `no-frames` |
/// | no | — | otherwise | `unchecked` |
/// | yes | a sensor other than the declared one | — | `mismatch` |
/// | yes | the declared sensor | capture failing or stalling | `no-frames` |
/// | yes | the declared sensor | pipeline not the agent's | `unchecked` |
/// | yes | the declared sensor | otherwise | `ok` |
/// | yes | nothing | a line in config.txt, or a sensor node in the live device tree | `not-detected` |
/// | yes | nothing | config.txt unreadable AND the device tree unreadable | `not-detected` |
/// | yes | nothing | no line (or config.txt unreadable) and no node (or the device tree unreadable), not both unreadable | `empty` |
pub fn derive_port(f: &PortFacts<'_>) -> CameraPort {
    let port_nodes: Vec<&PortNode> = f
        .nodes
        .map(|n| n.iter().filter(|n| n.connector == f.connector).collect())
        .unwrap_or_default();
    let node = f.nodes.and_then(|n| node_for_port(n, f.connector));
    let listed = f.listed.map(|(s, _)| s);
    // What the kernel and libcamera say is on the port: a config.txt line
    // naming one of these is the port's line even when its overlay is not
    // in the catalog.
    let mut hints: Vec<&str> = port_nodes.iter().map(|n| node_sensor(&n.node)).collect();
    hints.extend(listed.map(sensor_base));
    let boot = f
        .boot_text
        .map(|t| boot_config_port_on(t, f.connector, &hints, f.board));
    let declared = declared_with(f.setting.sensor, boot.as_ref(), f.model_sensor);
    let identity = identity_of(node, boot.as_ref(), listed);
    let driver = f.nodes.map(|_| node.is_some_and(|n| n.driver.is_some()));
    let resolved = Resolved {
        f,
        boot: boot.as_ref(),
        declared: declared.as_ref(),
        node,
        port_nodes: &port_nodes,
        identity,
    };
    let (verdict, detail) = setting_verdict(&resolved).unwrap_or_else(|| verdict_of(&resolved));
    let preset = preset_note(f, declared.as_ref(), verdict);
    // What the reconciler knows about the line and the report would
    // otherwise not say: it sits under a section the agent cannot evaluate,
    // so the agent neither knows that it applies nor edits it. The verdict
    // stays about the camera.
    let section = f
        .boot_text
        .and_then(|t| unknown_section_of(t, f.connector, &hints, f.board))
        .map(|name| {
            format!(
                " The camera line of {} in config.txt sits under [{name}], a section the agent cannot evaluate for this board: the agent does not edit that line.",
                f.connector
            )
        })
        .unwrap_or_default();
    let extra_lines = boot
        .as_ref()
        .filter(|b| b.lines > 1)
        .map(|b| match node.filter(|n| n.driver.is_some()) {
            // Which line took effect is what the kernel bound.
            Some(n) => format!(
                " config.txt has {} camera lines for {}; the one in force is the {} line (the kernel bound its driver) and the others did not take effect. Remove them, unless the lines sit under different [section] filters.",
                b.lines,
                f.connector,
                node_sensor(&n.node)
            ),
            None => format!(
                " config.txt has {} camera lines for {}; only one can work unless they sit under different [section] filters: remove the stale one.",
                b.lines, f.connector
            ),
        })
        .unwrap_or_default();
    CameraPort {
        connector: f.connector.to_string(),
        declared,
        bound: Bound {
            driver,
            node: node.map(|n| n.node.clone()),
            listed: listed.map(str::to_string),
            path: f.listed.map(|(_, p)| p.to_string()),
            identity,
        },
        boot_config: boot,
        capture: CaptureInfo {
            state: f.capture,
            backoff_step: f.backoff_step,
            last_error: f.last_error.map(str::to_string),
        },
        verdict,
        detail: format!("{detail}{preset}{section}{extra_lines}"),
    }
}

/// How the preset note of a port's sentence begins: `xorgate-rs status`
/// prints the note under a camera that is otherwise `ok` (it prints no
/// sentence for a healthy camera).
pub const PRESET_NOTE_MARK: &str = "The video preset ";

/// " The video preset … cannot run on this sensor. <the catalog's reason>"
/// for a camera that is there (`ok` / `no-frames`) and whose sensor the
/// catalog knows; else nothing.
fn preset_note(f: &PortFacts<'_>, declared: Option<&Declared>, verdict: CameraVerdict) -> String {
    if !matches!(verdict, CameraVerdict::Ok | CameraVerdict::NoFrames) || f.listed.is_none() {
        return String::new();
    }
    let c = catalog();
    let sensor = declared.and_then(|d| match d.source {
        DeclaredSource::Config => c.by_id(&d.sensor),
        DeclaredSource::BootConfig => c.by_overlay(&d.sensor),
        DeclaredSource::Model => None,
    });
    match (f.setting.preset, sensor) {
        (Some(preset), Some(s)) => s
            .preset_unsupported(preset)
            .map(|reason| {
                format!(" {PRESET_NOTE_MARK}{preset} cannot run on this sensor. {reason}")
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// The verdicts that come from the `cameraSensors` setting and from a line
/// that waits for a reboot, before anything else is looked at. In order:
///
/// 1. `unknown-sensor`: the setting names a sensor this build's catalog
///    lacks. Nothing is written for the port.
/// 2. `requirements-missing`: the declared sensor needs something the
///    device lacks, and the camera is not listed. Ahead of a pending
///    reboot, because a reboot alone would not bring the camera.
/// 3. `reboot-required`: the port's camera line changed since this boot
///    (managed or not, whoever changed it) and nothing else is wrong. A
///    real fault of the camera that the reboot brings back (`no-frames`,
///    `not-detected`, with the overlay unchanged) keeps its own verdict
///    and its sentence says that the line also waits for a reboot.
/// 4. `mismatch`: config.txt does not hold the declared line (yet): the
///    setting is not applied.
/// 5. a port declared empty and applied: `empty`, or `reboot-required`
///    while the running kernel still has a camera there.
fn setting_verdict(r: &Resolved<'_>) -> Option<(CameraVerdict, String)> {
    let f = r.f;
    let (key, port) = (f.stream_key, f.connector);
    let c = catalog();
    let set = f.setting.sensor;
    let sensor = set.and_then(|id| c.by_id(id));
    if let Some(id) = set.filter(|id| *id != CAMERA_SENSOR_NONE && sensor.is_none()) {
        return Some((
            CameraVerdict::UnknownSensor,
            format!(
                "{key} on {port} is set to \"{id}\", a camera sensor this agent build does not know (it knows: {}). Update the agent, or choose another sensor. The camera line in config.txt is left as it is.",
                c.sensors
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    if let (Some(s), Some(missing)) = (sensor, f.setting.missing) {
        let listed_as_declared = f
            .listed
            .is_some_and(|(l, _)| sensor_match(&s.overlay, l) == SensorMatch::Same);
        if !listed_as_declared {
            let reboot = if f.setting.line_change.is_some() {
                " Its camera line in config.txt also waits for a reboot."
            } else {
                ""
            };
            return Some((
                CameraVerdict::RequirementsMissing,
                format!("{port} is set to {}, which {missing}.{reboot}", s.id),
            ));
        }
    }
    if let Some(ch) = f.setting.line_change {
        // A REAL FAULT OUTRANKS A PENDING CHANGE. When the change keeps the
        // overlay (an option or the rotation was rewritten), the camera
        // that runs now is the one the reboot will bring back: if its
        // capture is failing, or nothing answers, that is the verdict, and
        // the sentence ends by saying the line also waits for a reboot.
        // `reboot-required` is for a port with nothing else wrong. (When
        // the overlay itself changes, or the line was added or removed,
        // what runs now is what the reboot replaces: its state is not the
        // new line's fault, and the port reads `reboot-required`.)
        if !ch.overlay {
            let (verdict, detail) = verdict_of(r);
            if matches!(
                verdict,
                CameraVerdict::NoFrames | CameraVerdict::NotDetected
            ) {
                return Some((
                    verdict,
                    format!(
                        "{detail} The camera line of {port} in config.txt also waits for a reboot: it was changed since this device booted."
                    ),
                ));
            }
        }
        let detail = match (&ch.booted, &ch.now) {
            (Some(was), Some(now)) => format!(
                "The camera line of {port} in config.txt was changed since this device booted (it booted with `{was}`, the file now says `{now}`). Reboot to apply."
            ),
            (None, Some(now)) => format!(
                "config.txt has a camera line for {port} that was added since this device booted (`{now}`). Reboot to apply."
            ),
            (Some(was), None) => format!(
                "The camera line of {port} was removed from config.txt since this device booted (it booted with `{was}`); the running kernel still has that camera configured. Reboot to apply."
            ),
            (None, None) => format!(
                "The camera lines of {port} in config.txt were changed since this device booted. Reboot to apply."
            ),
        };
        return Some((CameraVerdict::RebootRequired, detail));
    }
    let set = set?;
    // From here: a managed port whose line did not change since boot.
    let boot = r.boot?;
    const WHY: &str =
        "the setting has not been applied (the status of the camera sensor setting says why)";
    match (sensor, boot.overlay.as_deref()) {
        (Some(s), Some(overlay)) if overlay != s.overlay => Some((
            CameraVerdict::Mismatch,
            format!(
                "{port} is set to {}, but its camera line in config.txt still names {overlay}: {WHY}.",
                s.id
            ),
        )),
        (Some(s), None) => Some((
            CameraVerdict::Mismatch,
            format!(
                "{port} is set to {}, but config.txt has no camera line for it: {WHY}.",
                s.id
            ),
        )),
        (None, Some(overlay)) => Some((
            CameraVerdict::Mismatch,
            format!(
                "{port} is declared empty by the camera sensor setting, but config.txt still has a camera line for it ({overlay}): {WHY}."
            ),
        )),
        (None, None) => {
            debug_assert_eq!(set, CAMERA_SENSOR_NONE);
            if f.listed.is_some() || r.node.is_some() {
                Some((
                    CameraVerdict::RebootRequired,
                    format!(
                        "{port} is declared empty and config.txt has no camera line for it, but the running kernel still has a camera configured there. Reboot to apply."
                    ),
                ))
            } else {
                Some((
                    CameraVerdict::Empty,
                    format!(
                        "{port} is declared empty by the camera sensor setting: no camera is configured on it."
                    ),
                ))
            }
        }
        _ => None,
    }
}

fn verdict_of(r: &Resolved<'_>) -> (CameraVerdict, String) {
    let f = r.f;
    let (key, port) = (f.stream_key, f.connector);
    if !f.enumerated {
        const CANNOT: &str = "the agent cannot list cameras on this device (rpicam-hello is missing or libcamera is broken)";
        return match f.capture {
            // Frames flow: that is the strongest evidence there is.
            CaptureTruth::Streaming => (
                CameraVerdict::Ok,
                format!(
                    "The camera on {port} is streaming. It could not be checked against the declared sensor: {CANNOT}. Install rpicam-apps."
                ),
            ),
            CaptureTruth::Failing => (
                CameraVerdict::NoFrames,
                format!(
                    "The capture of {key} on {port} keeps failing, and {CANNOT}, so the port cannot be checked. Install rpicam-apps and restart the agent."
                ),
            ),
            _ => (
                CameraVerdict::Unchecked,
                format!(
                    "{key} on {port} cannot be checked: {CANNOT}. Install rpicam-apps and restart the agent."
                ),
            ),
        };
    }
    let drv = r.driver();

    if let Some((listed, _)) = f.listed {
        if let Some(d) = r.declared.filter(|d| d.differs_from(listed)) {
            // (b) config.txt edited, not rebooted.
            if let Some(old) = r.stale_overlay() {
                return (
                    CameraVerdict::RebootRequired,
                    format!(
                        "config.txt now declares {} on {port}, but the running kernel still has the {old} overlay loaded. Reboot to apply the config.txt change.",
                        d.sensor
                    ),
                );
            }
            // (c) a driver that cannot prove the chip does not "find" a sensor.
            let found = match r.identity {
                Some(Identity::None) | Some(Identity::Weak) => format!(
                    "the camera there is listed as {listed} by the {drv} driver, which cannot confirm which chip is attached"
                ),
                _ => format!("the camera found there is {listed}"),
            };
            return (
                CameraVerdict::Mismatch,
                match d.source {
                    DeclaredSource::Model => format!(
                        "The device model expects {} on {port}, but {found}. It is used as found.",
                        d.sensor
                    ),
                    DeclaredSource::BootConfig => format!(
                        "{port} is declared as {} by config.txt, but {found}. It is used as found; make the dtoverlay line in config.txt name the sensor that is really attached, then reboot.",
                        d.sensor
                    ),
                    DeclaredSource::Config => format!(
                        "{port} is set to {}, but {found}. It is used as found; choose the sensor that is really attached, then reboot.",
                        d.sensor
                    ),
                },
            );
        }
        let note = identity_note(r.identity, drv);
        let unknown = r.unknown_overlay_note();
        return match f.capture {
            // The stall wording only when the child printed no error of its
            // own: with one (a frame timeout, say), that error is the lead.
            CaptureTruth::Failing => match f.stalled_preset.filter(|_| f.last_error.is_none()) {
                Some(preset) => (
                    CameraVerdict::NoFrames,
                    format!(
                        "{listed} on {port} starts, but its recording stalls at preset {preset}: the capture process runs, writes nothing and is restarted by the progress watchdog. Try another video preset; a stall of this kind is a known fault of some presets and does not point at the cable or the sensor."
                    ),
                ),
                // The camera is held by another process: nothing is wrong
                // with the cable or the line.
                None if f.last_error.is_some_and(|e| {
                    ERROR_LINES_IN_USE.iter().any(|p| e.contains(p))
                }) =>
                {
                    (
                        CameraVerdict::NoFrames,
                        format!(
                            "{listed} is listed on {port} but its capture cannot open it: the camera is in use by another process. The usual causes are another capture or camera program running on this device, or a capture that was started before libcamera was changed. Stop the other program, or restart the agent."
                        ),
                    )
                }
                None => {
                    // Where the user looks: a managed port's line is written
                    // from the camera sensor setting, so that is what to
                    // check (a hand edit of config.txt would be overwritten).
                    // A hand-written or model-derived line: config.txt.
                    let names = match r.declared.map(|d| d.source) {
                        Some(DeclaredSource::Config) => "the camera sensor setting",
                        _ => "the dtoverlay line in config.txt",
                    };
                    (
                        CameraVerdict::NoFrames,
                        format!(
                            "{listed} is listed on {port} but delivers no frames: its capture process keeps failing.{note} Check the ribbon cable and that {names} names the sensor that is really attached."
                        ),
                    )
                }
            },
            _ if f.unsupervised => (
                CameraVerdict::Unchecked,
                format!(
                    "{listed} is detected on {port}, but {key} uses a custom pipeline that the agent does not supervise, so it cannot tell whether frames flow."
                ),
            ),
            CaptureTruth::Streaming => (
                CameraVerdict::Ok,
                format!("{listed} on {port} is detected and streaming.{unknown}"),
            ),
            // The "only weakly checks" caveat is left out here: the camera
            // is listed under the declared sensor's own name (libcamera
            // accepted it), and the caveat would read as a fault on a
            // correct camera that is merely starting. A driver with NO
            // identity check keeps its note (it lists a camera for anything).
            CaptureTruth::Starting => (
                CameraVerdict::Ok,
                format!(
                    "{listed} on {port} is detected; capture is starting.{}{unknown}",
                    if r.identity == Some(Identity::Weak) {
                        ""
                    } else {
                        note.as_str()
                    }
                ),
            ),
            // `Absent` cannot go with a listed camera; worded like `Off`.
            // The one reason the agent knows, and no identity caveat beside
            // an `ok`: "nothing may be attached" under `ok` helps nobody.
            CaptureTruth::Off | CaptureTruth::Absent => (
                CameraVerdict::Ok,
                match f.off_reason {
                    CaptureOff::Disabled => format!(
                        "{listed} on {port} is detected; capture is off: the camera is disabled in settings.{unknown}"
                    ),
                    CaptureOff::VideoOff => format!(
                        "{listed} on {port} is detected; capture is off: video is off on this device.{unknown}"
                    ),
                    CaptureOff::NotBegun => format!(
                        "{listed} on {port} is detected; capture has not begun yet.{unknown}"
                    ),
                },
            ),
        };
    }

    // Nothing listed on the port.
    let has_line = r.boot.is_some_and(|b| b.lines > 0);
    let unknown = r.boot.is_none() && f.nodes.is_none();
    let what = r.declared.map(|d| d.sensor.as_str()).unwrap_or("camera");
    if let Some(old) = r.stale_overlay() {
        return (
            CameraVerdict::RebootRequired,
            format!(
                "config.txt now declares {what} on {port}, but the running kernel still has the {old} overlay loaded, and no camera is listed there. Reboot to apply the config.txt change."
            ),
        );
    }
    if r.node.is_some_and(|n| n.driver.is_some()) {
        if !f.relisted {
            return (
                CameraVerdict::NotDetected,
                format!(
                    "The {drv} driver is bound on {port}, but libcamera did not list a camera there when the agent started. The agent is checking again."
                ),
            );
        }
        // No identity caveat here: the missing libcamera support is the
        // likely cause, and "may be a different sensor" points away from it.
        return (
            CameraVerdict::NotDetected,
            format!(
                "The {drv} driver is bound on {port} but libcamera lists no camera there (checked again after the agent started): libcamera on this device may not support this sensor."
            ),
        );
    }
    let managed = r
        .declared
        .is_some_and(|d| d.source == DeclaredSource::Config);
    if r.node.is_some() {
        if managed {
            return (
                CameraVerdict::NotDetected,
                format!(
                    "{port} is set to {what}, but no {what} answered on this port (the kernel driver did not bind). Check the ribbon cable and the selected sensor."
                ),
            );
        }
        return (
            CameraVerdict::NotDetected,
            format!(
                "{port} is declared as {what} by {}, but no {what} answered on this port (the kernel driver did not bind). Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached, then reboot.",
                r.declared.map(|d| d.source.words()).unwrap_or("config.txt"),
            ),
        );
    }
    if has_line && managed {
        let loaded = if f.nodes.is_some() {
            " its overlay is not loaded in the running kernel. Reboot if the setting was just changed; otherwise check that the overlay file exists on this device."
        } else {
            " no camera was found there. Check the ribbon cable and the selected sensor, then reboot."
        };
        return (
            CameraVerdict::NotDetected,
            format!("{port} is set to {what}, but{loaded}"),
        );
    }
    if has_line {
        let loaded = if f.nodes.is_some() {
            " that overlay is not loaded in the running kernel. Reboot if config.txt was edited; otherwise check the dtoverlay line."
        } else {
            " no camera was found there. Check the ribbon cable and the dtoverlay line, then reboot."
        };
        return (
            CameraVerdict::NotDetected,
            format!("{port} is declared as {what} by config.txt, but{loaded}"),
        );
    }
    if unknown {
        return (
            CameraVerdict::NotDetected,
            format!(
                "No camera was found on {port} (the device model expects {}). Check the ribbon cable and the camera dtoverlay line in config.txt, then reboot.",
                f.model_sensor
            ),
        );
    }
    // What is known to be missing: the line (config.txt was read), or, when
    // config.txt could not be read, the overlay in the running kernel.
    let why = if r.boot.is_some() {
        "config.txt has no camera dtoverlay line for it"
    } else {
        "no camera overlay is loaded for it in the running kernel"
    };
    (
        CameraVerdict::Empty,
        format!(
            "No camera is configured on {port}: {why} (the device model expects {}). Add the line for the attached sensor and reboot, or leave it if the port is unused.",
            f.model_sensor
        ),
    )
}

// ---------------------------------------------------------------------------
// B7: when the port's line must not be edited
// ---------------------------------------------------------------------------

/// B7: why the agent must NOT edit the camera line of a port, or `None`
/// when it may. An edit is for the sensor the line names, so the line is
/// left alone unless the running kernel and libcamera agree that this
/// sensor is what answered on the port. The facts are the camera report's
/// own ([`derive_port`], with the capture state left out: it does not bear
/// on which sensor is attached).
///
/// WHICH VERDICTS BLOCK A WRITE, and why:
///
/// | verdict | write | why |
/// |---|---|---|
/// | `mismatch` | blocked | libcamera lists another sensor than the line names: the line is wrong. Its rotation would be computed for a sensor that is not the one running |
/// | `reboot-required` | blocked | config.txt's line names another OVERLAY than the one the running kernel has loaded (the line was edited, the device not rebooted). The sensor running is the old one; the line is edited once the reboot has made it the running one. The agent's own rotation write never produces this verdict: it changes the `rotation=` token, not the overlay, so a pending rotation does not block the next one |
/// | `not-detected` | blocked | nothing answered as the line's sensor: the driver did not bind (wrong overlay, cable), the overlay is not loaded (edited, not rebooted), or the driver is bound and libcamera lists nothing (the agent is still re-checking, or the sensor needs packages). Bug B7 was exactly this: a rotation written for a wrong overlay |
/// | `empty` | blocked | no line and no node (nothing to edit; reached only when config.txt changed under the pass) |
/// | `ok` | allowed | the line's sensor is listed on the port |
/// | `no-frames` | allowed | the line's sensor IS listed; its capture fails for a reason of its own (a preset stall included), which says nothing about the line |
/// | `unchecked` | allowed, after one more look | the agent cannot list cameras on this device, or the pipeline is not its own. Blocking would take rotation away from devices that have it today (an IMX290 device without `rpicam-hello`). The kernel is still asked, below |
///
/// Where cameras cannot be listed, the kernel's side alone decides, when it
/// can be read: the line's overlay must be the one loaded on the port (not
/// "edited, not rebooted") and its driver must be bound.
///
/// Two camera lines on one port block a write too; the caller sees that in
/// config.txt itself (`boot_config::camera_lines_for_port`).
pub fn rotation_write_block(f: &PortFacts<'_>) -> Option<String> {
    let port = derive_port(&PortFacts {
        capture: CaptureTruth::Off,
        backoff_step: 0,
        last_error: None,
        stalled_preset: None,
        unsupervised: false,
        ..f.clone()
    });
    match port.verdict {
        CameraVerdict::Mismatch
        | CameraVerdict::RebootRequired
        | CameraVerdict::NotDetected
        | CameraVerdict::Empty
        // Not reached for an unmanaged port (they come from the sensor
        // setting, and a managed port's line is not gated here).
        | CameraVerdict::RequirementsMissing
        | CameraVerdict::UnknownSensor => {
            return Some(port.detail);
        }
        CameraVerdict::Ok | CameraVerdict::NoFrames | CameraVerdict::Unchecked => {}
    }
    if f.enumerated {
        return None;
    }
    // Cameras cannot be listed: the kernel's facts, for a catalog overlay.
    let overlay = port.boot_config.as_ref()?.overlay.as_deref()?;
    let sensor = camera_overlay(overlay)?;
    let nodes: Vec<&PortNode> = f
        .nodes?
        .iter()
        .filter(|n| n.connector == f.connector)
        .collect();
    let mine = nodes.iter().find(|n| {
        sensor
            .dt_node_names
            .iter()
            .any(|w| w == node_sensor(&n.node))
    });
    let connector = f.connector;
    match mine {
        Some(n) if n.driver.is_some() => None,
        Some(_) => Some(format!(
            "{connector} is declared as {overlay} by config.txt, but no {overlay} answered on this port (the kernel driver did not bind). Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached, then reboot."
        )),
        None => Some(match nodes.first() {
            Some(other) => format!(
                "config.txt now declares {overlay} on {connector}, but the running kernel still has the {} overlay loaded. Reboot to apply the config.txt change.",
                node_sensor(&other.node)
            ),
            None => format!(
                "{connector} is declared as {overlay} by config.txt, but that overlay is not loaded in the running kernel. Reboot if config.txt was edited; otherwise check the dtoverlay line."
            ),
        }),
    }
}

/// The parts of a port's report worth re-reporting on. The back-off step,
/// the last error line and the sentence are left out: they move with every
/// step of the capture ladder, and a report per step would be churn.
pub fn summarise_port(key: &str, p: &CameraPort) -> String {
    format!(
        "{key}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        p.declared
            .as_ref()
            .map(|d| format!("{}@{}", d.sensor, d.source.as_str()))
            .unwrap_or_default(),
        p.boot_config
            .as_ref()
            .map(|b| format!("{}#{}", b.line.as_deref().unwrap_or(""), b.lines))
            .unwrap_or_else(|| "?".to_string()),
        match p.bound.driver {
            Some(true) => "bound",
            Some(false) => "unbound",
            None => "?",
        },
        p.bound.node.as_deref().unwrap_or(""),
        p.bound.listed.as_deref().unwrap_or(""),
        p.bound.path.as_deref().unwrap_or(""),
        p.capture.state.as_str(),
        p.verdict.as_str(),
        p.connector,
    )
}

/// The word `xorgate-rs status` prints for one camera: the verdict, except
/// that a healthy camera shows what its capture is doing.
pub fn status_word(p: &CameraPort) -> &'static str {
    match p.verdict {
        CameraVerdict::Ok => p.capture.state.as_str(),
        v => v.as_str(),
    }
}

// ---------------------------------------------------------------------------
// re-listing a held port, and the report hold
// ---------------------------------------------------------------------------

/// When cameras are listed again for a held port, in ms after the agent
/// started: three early looks, then one every [`RELIST_EVERY_MS`].
pub const RELIST_AFTER_START_MS: [u64; 3] = [15_000, 30_000, 60_000];
/// The steady period of the re-listing after the early looks.
pub const RELIST_EVERY_MS: u64 = 300_000;

/// Does a port need its cameras listed again? Only a port that is HELD for
/// lack of a camera (B5) while the kernel has a driver bound on it: the
/// boot listing may simply have come too early, and without a re-listing
/// the hold would last until the agent restarts. A port with no bound
/// driver gets none: a driver binds at boot only, so nothing can appear.
pub fn port_needs_relist(held_no_camera: bool, driver_bound: bool) -> bool {
    held_no_camera && driver_bound
}

/// The first re-listing time strictly after `now_ms` (both in ms after the
/// agent started): 15 s, 30 s, 60 s, then every 5 minutes.
pub fn next_relist_ms(now_ms: u64) -> u64 {
    if let Some(t) = RELIST_AFTER_START_MS.iter().find(|t| **t > now_ms) {
        return *t;
    }
    let last = RELIST_AFTER_START_MS[RELIST_AFTER_START_MS.len() - 1];
    let periods = (now_ms - last) / RELIST_EVERY_MS + 1;
    last + periods * RELIST_EVERY_MS
}

/// The longest a changed report waits before it is sent.
pub const REPORT_HOLD_MAX_MS: u64 = 600_000;
/// With no camera report requested for this long, the hold is the base again.
pub const REPORT_HOLD_RESET_MS: u64 = 1_800_000;

/// The hold before the NEXT change is reported, after one was just
/// requested with `current_ms`: doubled, up to [`REPORT_HOLD_MAX_MS`]. A
/// camera that fails and recovers every couple of minutes would otherwise
/// cost two reports per cycle for ever.
pub fn next_report_hold_ms(current_ms: u64) -> u64 {
    current_ms.saturating_mul(2).min(REPORT_HOLD_MAX_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> OverlayLine {
        crate::boot_config::parse_overlay_lines(text).remove(0)
    }

    /// The connector a line counts for in the report, with no hint.
    fn port_of(text: &str) -> Option<&'static str> {
        let l = line(text);
        ["csi0", "csi1"]
            .into_iter()
            .find(|c| line_targets(&l, c, &[]))
    }

    #[test]
    fn a_cam0_token_is_csi0_and_everything_else_is_csi1() {
        // The three shapes found on the benches.
        assert_eq!(port_of("dtoverlay=imx708,cam0"), Some("csi0"));
        assert_eq!(port_of("dtoverlay=imx708,cam0,rotation=180"), Some("csi0"));
        assert_eq!(port_of("dtoverlay=imx290,rotation=180,cam0"), Some("csi0"));
        // csi1 with the fake `cam1` token, and without it.
        assert_eq!(port_of("dtoverlay=imx219,cam1,rotation=180"), Some("csi1"));
        assert_eq!(port_of("dtoverlay=imx219"), Some("csi1"));
        assert_eq!(
            port_of("dtoverlay=imx290,clock-frequency=74250000"),
            Some("csi1")
        );
        // Not a camera overlay: no port at all.
        assert_eq!(port_of("dtoverlay=vc4-kms-v3d"), None);
        assert_eq!(port_of("dtoverlay=dwc2,dr_mode=host"), None);
        // A token that merely contains `cam0` is not the selector.
        assert_eq!(port_of("dtoverlay=imx290,xcam0"), Some("csi1"));
        // A connector that is not a camera port has no line.
        assert!(!line_targets(&line("dtoverlay=imx290,cam0"), "csi2", &[]));
    }

    #[test]
    fn every_catalog_overlay_is_a_port_selector() {
        let names: Vec<&str> = catalog()
            .sensors
            .iter()
            .map(|s| s.overlay.as_str())
            .collect();
        assert_eq!(names, ["imx290", "imx219", "imx708", "imx335"]);
        for o in &names {
            assert_eq!(port_of(&format!("dtoverlay={o},cam0")), Some("csi0"));
            assert_eq!(port_of(&format!("dtoverlay={o}")), Some("csi1"));
        }
        let id = |n: &str| camera_overlay(n).unwrap().identity;
        assert_eq!(id("imx219"), Identity::ChipId);
        assert_eq!(id("imx708"), Identity::ChipId);
        assert_eq!(id("imx335"), Identity::Weak);
        assert_eq!(id("imx290"), Identity::None);
        // The three overlays Phase 0's temporary table also held are outside
        // the catalog: a line of theirs is a port's only with an explicit
        // port token (or a hint), and nothing is known about their driver.
        for o in ["imx327", "imx462", "imx477"] {
            assert!(camera_overlay(o).is_none(), "{o}");
            assert_eq!(port_of(&format!("dtoverlay={o}")), None, "{o}");
            assert_eq!(port_of(&format!("dtoverlay={o},cam0")), Some("csi0"), "{o}");
            assert_eq!(port_of(&format!("dtoverlay={o},cam1")), Some("csi1"), "{o}");
            assert!(line_targets(&line(&format!("dtoverlay={o}")), "csi1", &[o]));
        }
    }

    /// The camera lines of the three benches' config.txt (2026-11-21), with
    /// the surrounding lines that must not be taken for camera lines.
    const BENCH2: &str = "camera_auto_detect=0\ndtoverlay=vc4-kms-v3d\n[cm5]\ndtoverlay=dwc2,dr_mode=host\n[all]\ndtoverlay=imx335,cam0\ndtoverlay=imx219,cam1,rotation=180\n";
    const CM4: &str =
        "camera_auto_detect=0\ndtoverlay=vc4-kms-v3d\n[all]\ndtoverlay=imx708,cam0,rotation=180\n";
    const BENCH1: &str = "dtoverlay=vc4-kms-v3d\n[all]\ndtoverlay=imx290,cam0\ndtoverlay=imx290,cam1\n#dtoverlay=googlevoicehat-soundcard\n";

    #[test]
    fn boot_config_view_per_port() {
        assert_eq!(
            boot_config_port(BENCH2, "csi0"),
            BootConfigPort {
                overlay: Some("imx335".into()),
                line: Some("dtoverlay=imx335,cam0".into()),
                lines: 1
            }
        );
        assert_eq!(
            boot_config_port(BENCH2, "csi1"),
            BootConfigPort {
                overlay: Some("imx219".into()),
                line: Some("dtoverlay=imx219,cam1,rotation=180".into()),
                lines: 1
            }
        );
        assert_eq!(
            boot_config_port(CM4, "csi0").overlay.as_deref(),
            Some("imx708")
        );
        assert_eq!(
            boot_config_port(CM4, "csi1"),
            BootConfigPort {
                overlay: None,
                line: None,
                lines: 0
            }
        );
        assert_eq!(boot_config_port(BENCH1, "csi0").lines, 1);
        assert_eq!(boot_config_port(BENCH1, "csi1").lines, 1);
        // A swap that left two lines on one port: both are counted and the
        // last is reported. A commented line is not counted.
        let two = "dtoverlay=imx290,cam0\r\n#dtoverlay=imx477,cam0\n  dtoverlay=imx708,cam0  \n";
        assert_eq!(
            boot_config_port(two, "csi0"),
            BootConfigPort {
                overlay: Some("imx708".into()),
                line: Some("dtoverlay=imx708,cam0".into()),
                lines: 2
            }
        );
    }

    #[test]
    fn declared_order_is_boot_config_then_model() {
        let boot = boot_config_port(CM4, "csi0");
        assert_eq!(
            declared_for(Some(&boot), "imx290"),
            Some(Declared {
                sensor: "imx708".into(),
                source: DeclaredSource::BootConfig
            })
        );
        // No line for the port, or no config.txt at all: the model.
        let none = boot_config_port(CM4, "csi1");
        for b in [Some(&none), None] {
            assert_eq!(
                declared_for(b, "imx290"),
                Some(Declared {
                    sensor: "imx290".into(),
                    source: DeclaredSource::Model
                })
            );
        }
        assert_eq!(
            resolve_declared(&[(DeclaredSource::Model, Some("  "))]),
            None
        );
        assert_eq!(resolve_declared(&[]), None);
    }

    #[test]
    fn variant_names_are_the_same_sensor_and_an_unknown_overlay_is_not_judged() {
        use SensorMatch::{Different, Same, Unknown};
        for v in ["imx708", "imx708_wide", "imx708_noir", "imx708_wide_noir"] {
            assert_eq!(sensor_match("imx708", v), Same, "{v}");
            assert_eq!(sensor_base(v), "imx708");
        }
        // A variant spelling the catalog does not list is still the sensor.
        assert_eq!(sensor_match("imx219", "imx219_noir"), Same);
        // Two catalog sensors: known to differ.
        assert_eq!(sensor_match("imx290", "imx708"), Different);
        assert_eq!(sensor_match("imx708", "imx219"), Different);
        assert_eq!(sensor_match("imx290", "imx327"), Different);
        assert_eq!(sensor_match("imx335", "imx708_wide"), Different);
        // Declared OUTSIDE the catalog: the same by name, else unknown. The
        // `imx327` / `imx462` overlays list as `imx290`, and this build
        // cannot know that: it claims nothing rather than a mismatch.
        assert_eq!(sensor_match("imx477", "imx477"), Same);
        assert_eq!(sensor_match("imx477", "imx477_noir"), Same);
        assert_eq!(sensor_match("imx327", "imx290"), Unknown);
        assert_eq!(sensor_match("imx462", "imx290"), Unknown);
        assert_eq!(sensor_match("ov5647", "imx708"), Unknown);
        // `same_sensor` is "not known to differ".
        assert!(same_sensor("imx462", "imx290"));
        assert!(same_sensor("imx708", "imx708_wide"));
        assert!(!same_sensor("imx290", "imx708"));
        // The MODEL names a sensor: a name the catalog cannot place is
        // compared as a name (TS parity); a config.txt overlay is not.
        let d = |sensor: &str, source| Declared {
            sensor: sensor.into(),
            source,
        };
        assert!(d("imx477", DeclaredSource::Model).differs_from("imx290"));
        assert!(!d("imx477", DeclaredSource::BootConfig).differs_from("imx290"));
        assert!(!d("imx477", DeclaredSource::Model).differs_from("imx477_noir"));
        assert!(d("imx290", DeclaredSource::Model).differs_from("imx708"));
        assert!(d("imx290", DeclaredSource::BootConfig).differs_from("imx708"));
        assert!(!d("imx708", DeclaredSource::BootConfig).differs_from("imx708_wide"));
        // A bare suffix is not a variant of nothing.
        assert_eq!(sensor_base("_noir"), "_noir");
    }

    /// Q4: a line under a model filter that names the other Compute Module
    /// family is not the port's line; under any other filter it still counts
    /// (it may apply).
    #[test]
    fn section_filters_decide_which_line_is_the_ports() {
        let text = "[cm4]\ndtoverlay=imx708,cam0\n[cm5]\ndtoverlay=imx290,cam0\n[all]\n";
        let on = |board| boot_config_port_on(text, "csi0", &[], board);
        assert_eq!(
            on(Board::Cm4),
            BootConfigPort {
                overlay: Some("imx708".into()),
                line: Some("dtoverlay=imx708,cam0".into()),
                lines: 1
            }
        );
        assert_eq!(on(Board::Cm5).overlay.as_deref(), Some("imx290"));
        assert_eq!(on(Board::Cm5).lines, 1);
        // The board is not known: both may apply, the last is reported.
        assert_eq!(on(Board::Unknown).lines, 2);
        assert_eq!(boot_config_port(text, "csi0").lines, 2);
        let gpio = "dtoverlay=imx290,cam0\n[gpio4=1]\ndtoverlay=imx708,cam0\n";
        assert_eq!(boot_config_port_on(gpio, "csi0", &[], Board::Cm5).lines, 2);
        // The three benches: every camera line sits under `[all]`.
        for board in [Board::Cm4, Board::Cm5, Board::Unknown] {
            assert_eq!(boot_config_port_on(BENCH1, "csi0", &[], board).lines, 1);
            assert_eq!(boot_config_port_on(BENCH1, "csi1", &[], board).lines, 1);
            assert_eq!(boot_config_port_on(BENCH2, "csi1", &[], board).lines, 1);
            assert_eq!(boot_config_port_on(CM4, "csi0", &[], board).lines, 1);
        }
    }

    fn live(state: CaptureState) -> CaptureLive {
        CaptureLive {
            state,
            record: false,
            bytes_seen: false,
            unstable: false,
            stable_ms: None,
            backoff_step: 0,
            last_error: None,
            stalled_preset: None,
        }
    }

    #[test]
    fn capture_truth_table() {
        use CaptureState as S;
        use CaptureTruth as T;
        // No run at all.
        assert_eq!(capture_truth(None, true), T::Absent);
        assert_eq!(capture_truth(None, false), T::Off);
        assert_eq!(capture_truth(Some(&live(S::Stopped)), false), T::Off);
        // A first start that has not failed.
        assert_eq!(capture_truth(Some(&live(S::Starting)), false), T::Starting);
        // Bridge-only: past the stable window = streaming.
        let up = CaptureLive {
            stable_ms: Some(0),
            ..live(S::Running)
        };
        assert_eq!(capture_truth(Some(&up), false), T::Streaming);
        // Record mode needs bytes on a fragment too (the 1080p30 stall goes
        // PLAYING and writes nothing).
        let rec = CaptureLive {
            record: true,
            ..up.clone()
        };
        assert_eq!(capture_truth(Some(&rec), false), T::Starting);
        let rec = CaptureLive {
            bytes_seen: true,
            ..rec
        };
        assert_eq!(capture_truth(Some(&rec), false), T::Streaming);
        // The ladder: every state of a camera that keeps dying is `failing`,
        // the fresh attempt after a park included.
        for s in [S::Retrying, S::Failed] {
            assert_eq!(capture_truth(Some(&live(s)), false), T::Failing);
        }
        let again = CaptureLive {
            unstable: true,
            ..live(S::Starting)
        };
        assert_eq!(capture_truth(Some(&again), false), T::Failing);
        // A crash after the stable window leaves the state `running` with
        // no child: failing, not streaming.
        let crashed = CaptureLive {
            unstable: true,
            ..live(S::Running)
        };
        assert_eq!(capture_truth(Some(&crashed), false), T::Failing);
        // Recovery needs RECOVER_MS of stable running.
        let recovering = CaptureLive {
            unstable: true,
            stable_ms: Some(RECOVER_MS - 1),
            ..live(S::Running)
        };
        assert_eq!(capture_truth(Some(&recovering), false), T::Failing);
        assert!(!capture_recovered(&recovering));
        let recovered = CaptureLive {
            stable_ms: Some(RECOVER_MS),
            ..recovering
        };
        assert_eq!(capture_truth(Some(&recovered), false), T::Streaming);
        assert!(capture_recovered(&recovered));
    }

    #[test]
    fn a_clean_restart_of_a_streaming_camera_is_not_news() {
        use CaptureTruth as T;
        // Inside the grace a restart (`off` while the old capture stops,
        // `starting` while the new one settles) still reads `streaming` …
        for live in [T::Starting, T::Off] {
            assert_eq!(settle_capture(Some(T::Streaming), live, 0), T::Streaming);
            assert_eq!(
                settle_capture(Some(T::Streaming), live, RESTART_GRACE_MS - 1),
                T::Streaming
            );
            // … and past it the report says what is.
            assert_eq!(
                settle_capture(Some(T::Streaming), live, RESTART_GRACE_MS),
                live
            );
        }
        // A failure or a lost camera is never smoothed.
        assert_eq!(
            settle_capture(Some(T::Streaming), T::Failing, 0),
            T::Failing
        );
        assert_eq!(settle_capture(Some(T::Streaming), T::Absent, 0), T::Absent);
        // Only a camera that WAS streaming is.
        assert_eq!(settle_capture(None, T::Starting, 0), T::Starting);
        assert_eq!(settle_capture(Some(T::Off), T::Starting, 0), T::Starting);
        assert_eq!(
            settle_capture(Some(T::Failing), T::Starting, 0),
            T::Starting
        );
        assert_eq!(settle_capture(Some(T::Starting), T::Off, 0), T::Off);
    }

    #[test]
    fn b5_guard_holds_only_a_port_that_enumerated_no_camera() {
        use StreamHardwareStatus as H;
        assert!(!capture_allowed(true, Some(H::NoCamera)));
        // Enumeration never worked: fail open, as before.
        assert!(capture_allowed(false, Some(H::NoCamera)));
        assert!(capture_allowed(false, None));
        for s in [H::Rendered, H::Ok, H::Custom] {
            assert!(capture_allowed(true, Some(s)));
        }
        // A manifest stream the model does not list keeps today's behaviour.
        assert!(capture_allowed(true, None));
    }

    #[test]
    fn error_lines_are_the_ones_marked_error_and_not_on_the_skip_list() {
        assert_eq!(
            error_line("ERROR: from element /GstPipeline:pipeline0/GstLibcameraSrc:libcamerasrc0: Could not find a camera named '/base/soc/i2c0mux/i2c@0/imx708@1a'.\n").as_deref(),
            Some("ERROR: from element /GstPipeline:pipeline0/GstLibcameraSrc:libcamerasrc0: Could not find a camera named '/base/soc/i2c0mux/i2c@0/imx708@1a'.")
        );
        // The skip list: never the cause.
        assert_eq!(error_line("ERROR: pipeline doesn't want to preroll."), None);
        assert_eq!(
            error_line("[0:00:19.158270951] [1114] ERROR V4L2 v4l2_device.cpp:390 'imx219 0-0010': Unable to set controls: Device or resource busy"),
            None
        );
        assert_eq!(error_line("Additional debug info:"), None);
        assert_eq!(
            error_line("[0:00:12.794902750] [739]  INFO Camera camera_manager.cpp:330 libcamera v0.5.2+99-bfd68f78"),
            None
        );
        let long = format!("ERROR {}", "x".repeat(500));
        assert_eq!(error_line(&long).unwrap().chars().count(), LAST_ERROR_MAX);
    }

    /// The REAL stderr of capture children on the benches: the line kept is
    /// the child's first error that is not on the skip list. Keeping the
    /// LAST one would report libcamera's advice ("Alternatively, try another
    /// cable and/or sensor.") or gst-launch's consequence lines; keeping the
    /// plain first one would report the OTHER camera's busy-controls line on
    /// a two-camera device.
    #[test]
    fn real_capture_stderr_keeps_the_cause() {
        let doc: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/camera_truth/capture-stderr.json"
        ))
        .unwrap();
        let cases = doc["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 6);
        let (mut busy_first, mut advice) = (0, 0);
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let lines: Vec<&str> = c["lines"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap())
                .collect();
            assert_eq!(
                first_error_line(lines.iter().copied()).as_deref(),
                c["lastError"].as_str(),
                "{name}"
            );
            let errors: Vec<&&str> = lines.iter().filter(|l| l.contains("ERROR")).collect();
            busy_first += usize::from(errors[0].contains("Device or resource busy"));
            advice += usize::from(errors.iter().any(|l| l.contains("Alternatively, try")));
        }
        assert!(busy_first >= 2, "the fixture holds the two-camera case");
        assert!(advice >= 2, "the fixture holds the frame-timeout advice");
        assert_eq!(first_error_line(["INFO x", "WARN y"]), None);
    }

    fn node(connector: &str, node: &str, driver: Option<&str>) -> PortNode {
        PortNode {
            connector: connector.into(),
            node: node.into(),
            driver: driver.map(str::to_string),
        }
    }

    fn facts<'a>(key: &'a str, connector: &'a str) -> PortFacts<'a> {
        PortFacts {
            stream_key: key,
            connector,
            model_sensor: "imx290",
            boot_text: None,
            board: Board::Unknown,
            nodes: None,
            enumerated: true,
            relisted: false,
            listed: None,
            capture: CaptureTruth::Off,
            backoff_step: 0,
            last_error: None,
            stalled_preset: None,
            unsupervised: false,
            off_reason: CaptureOff::NotBegun,
            setting: PortSetting::default(),
        }
    }

    const IMX290_CM5: (&str, &str) = (
        "imx290",
        "/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a",
    );

    #[test]
    fn verdict_ok_cm4_imx708_declared_in_config_model_says_imx290() {
        let nodes = [node("csi0", "imx708@1a", Some("imx708"))];
        let p = derive_port(&PortFacts {
            boot_text: Some(CM4),
            nodes: Some(&nodes),
            listed: Some(("imx708", "/base/soc/i2c0mux/i2c@0/imx708@1a")),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"connector":"csi0","declared":{"sensor":"imx708","source":"boot-config"},"bootConfig":{"overlay":"imx708","line":"dtoverlay=imx708,cam0,rotation=180","lines":1},"bound":{"driver":true,"node":"imx708@1a","listed":"imx708","path":"/base/soc/i2c0mux/i2c@0/imx708@1a","identity":"chip-id"},"capture":{"state":"streaming","backoffStep":0,"lastError":null},"verdict":"ok","detail":"imx708 on csi0 is detected and streaming."}"#
        );
        assert_eq!(status_word(&p), "streaming");
    }

    #[test]
    fn verdict_ok_wording_follows_capture_and_identity() {
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let base = PortFacts {
            boot_text: Some(BENCH1),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            ..facts("cam0", "csi0")
        };
        let starting = derive_port(&PortFacts {
            capture: CaptureTruth::Starting,
            ..base.clone()
        });
        assert_eq!(starting.verdict, CameraVerdict::Ok);
        assert_eq!(
            starting.detail,
            "imx290 on csi0 is detected; capture is starting. The imx290 driver cannot confirm which chip is attached, so the camera on this port may be a different sensor, or nothing may be attached."
        );
        assert_eq!(starting.bound.identity, Some(Identity::None));
        assert_eq!(status_word(&starting), "starting");
        // Capture off: the ONE reason the agent knows, and no "nothing may
        // be attached" caveat beside an `ok` (bench 1: cam1 disabled by
        // settings read `ok` with that caveat and a list of three reasons).
        for (why, want) in [
            (
                CaptureOff::Disabled,
                "imx290 on csi0 is detected; capture is off: the camera is disabled in settings.",
            ),
            (
                CaptureOff::VideoOff,
                "imx290 on csi0 is detected; capture is off: video is off on this device.",
            ),
            (
                CaptureOff::NotBegun,
                "imx290 on csi0 is detected; capture has not begun yet.",
            ),
        ] {
            let off = derive_port(&PortFacts {
                off_reason: why,
                ..base.clone()
            });
            assert_eq!(off.verdict, CameraVerdict::Ok);
            assert_eq!(off.detail, want);
            assert_eq!(status_word(&off), "off");
        }
        // Streaming proves frames flow: no identity caveat.
        let up = derive_port(&PortFacts {
            capture: CaptureTruth::Streaming,
            ..base
        });
        assert_eq!(up.detail, "imx290 on csi0 is detected and streaming.");
    }

    #[test]
    fn verdict_no_frames_start_failures_and_stalls_read_differently() {
        // A start failure or a frame timeout under the `imx290` driver: the
        // port may hold another sensor, or NOTHING (the single-camera fleet
        // device with two `imx290` lines).
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let failing = PortFacts {
            boot_text: Some(BENCH1),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Failing,
            backoff_step: 2,
            last_error: Some("ERROR RPI pipeline_base.cpp:1408 Camera frontend has timed out!"),
            ..facts("cam0", "csi0")
        };
        let p = derive_port(&failing);
        assert_eq!(p.verdict, CameraVerdict::NoFrames);
        assert_eq!(
            p.detail,
            "imx290 is listed on csi0 but delivers no frames: its capture process keeps failing. The imx290 driver cannot confirm which chip is attached, so the camera on this port may be a different sensor, or nothing may be attached. Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached."
        );
        assert_eq!(p.capture.backoff_step, 2);
        assert_eq!(status_word(&p), "no-frames");
        // The known 1080p30 recording stall on a HEALTHY camera (the child
        // runs and prints NO error): it names the preset and does not send
        // the reader to the ribbon cable.
        let stalled = PortFacts {
            stalled_preset: Some("1080p30-4mbps"),
            last_error: None,
            ..failing.clone()
        };
        let p = derive_port(&stalled);
        assert_eq!(p.verdict, CameraVerdict::NoFrames);
        assert_eq!(
            p.detail,
            "imx290 on csi0 starts, but its recording stalls at preset 1080p30-4mbps: the capture process runs, writes nothing and is restarted by the progress watchdog. Try another video preset; a stall of this kind is a known fault of some presets and does not point at the cable or the sensor."
        );
        // A watchdog kill of a child that DID print an error (a frame
        // timeout): the sentence must not contradict it. The normal
        // no-frames wording, with the error in `capture.lastError`.
        let p = derive_port(&PortFacts {
            stalled_preset: Some("1080p30-4mbps"),
            ..failing
        });
        assert_eq!(p.verdict, CameraVerdict::NoFrames);
        assert_eq!(
            p.capture.last_error.as_deref(),
            Some("ERROR RPI pipeline_base.cpp:1408 Camera frontend has timed out!")
        );
        assert_eq!(
            p.detail,
            "imx290 is listed on csi0 but delivers no frames: its capture process keeps failing. The imx290 driver cannot confirm which chip is attached, so the camera on this port may be a different sensor, or nothing may be attached. Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached."
        );
        // A chip-ID driver gets no identity caveat.
        let nodes = [node("csi1", "imx219@10", Some("imx219"))];
        let hand_written = PortFacts {
            boot_text: Some(BENCH2),
            nodes: Some(&nodes),
            listed: Some((
                "imx219",
                "/base/axi/pcie@1000120000/rp1/i2c@70000/imx219@10",
            )),
            capture: CaptureTruth::Failing,
            ..facts("cam1", "csi1")
        };
        let p = derive_port(&hand_written);
        assert_eq!(
            p.declared.as_ref().map(|d| d.source),
            Some(DeclaredSource::BootConfig)
        );
        assert_eq!(
            p.detail,
            "imx219 is listed on csi1 but delivers no frames: its capture process keeps failing. Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached."
        );
        // The same port MANAGED (its line is written from the camera sensor
        // setting, `declared.source` = `config`): the sentence points at the
        // setting. Sending the user to config.txt would have them edit by
        // hand a line the agent writes.
        let p = derive_port(&PortFacts {
            setting: managed("imx219"),
            ..hand_written
        });
        assert_eq!(p.verdict, CameraVerdict::NoFrames);
        assert_eq!(
            p.declared.as_ref().map(|d| d.source),
            Some(DeclaredSource::Config)
        );
        assert_eq!(
            p.detail,
            "imx219 is listed on csi1 but delivers no frames: its capture process keeps failing. Check the ribbon cable and that the camera sensor setting names the sensor that is really attached."
        );
    }

    #[test]
    fn verdict_not_detected_shapes() {
        // (1) Bench 2: `imx335,cam0`, the sensor does not answer
        // (`probe with driver imx335 failed`): node present, no driver.
        let nodes = [
            node("csi0", "imx335@1a", None),
            node("csi1", "imx219@10", Some("imx219")),
        ];
        let p = derive_port(&PortFacts {
            boot_text: Some(BENCH2),
            nodes: Some(&nodes),
            capture: CaptureTruth::Absent,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::NotDetected);
        assert_eq!(
            p.detail,
            "csi0 is declared as imx335 by config.txt, but no imx335 answered on this port (the kernel driver did not bind). Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached, then reboot."
        );
        assert_eq!(p.bound.driver, Some(false));
        assert_eq!(p.bound.identity, Some(Identity::Weak), "the line's overlay");
        assert_eq!(status_word(&p), "not-detected");

        // (2) CM4: `imx335,cam0` on an IMX708 with stock libcamera: the
        // driver BINDS, libcamera lists nothing. At boot that is only an
        // observation: nothing is concluded about libcamera …
        let nodes = [node("csi0", "imx335@1a", Some("imx335"))];
        let bound = PortFacts {
            boot_text: Some("dtoverlay=imx335,cam0,rotation=180\n"),
            nodes: Some(&nodes),
            capture: CaptureTruth::Absent,
            ..facts("cam0", "csi0")
        };
        let p = derive_port(&bound);
        assert_eq!(p.verdict, CameraVerdict::NotDetected);
        assert_eq!(p.bound.driver, Some(true));
        assert_eq!(
            p.detail,
            "The imx335 driver is bound on csi0, but libcamera did not list a camera there when the agent started. The agent is checking again."
        );
        // … until a second listing has confirmed it.
        let p = derive_port(&PortFacts {
            relisted: true,
            ..bound
        });
        assert_eq!(
            p.detail,
            "The imx335 driver is bound on csi0 but libcamera lists no camera there (checked again after the agent started): libcamera on this device may not support this sensor."
        );

        // (3) A line whose overlay is not in the running device tree, and
        // no other camera node on the port.
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx219\n"),
            nodes: Some(&[]),
            ..facts("cam1", "csi1")
        });
        assert_eq!(p.verdict, CameraVerdict::NotDetected);
        assert_eq!(
            p.detail,
            "csi1 is declared as imx219 by config.txt, but that overlay is not loaded in the running kernel. Reboot if config.txt was edited; otherwise check the dtoverlay line."
        );
        assert_eq!((p.bound.driver, p.bound.node.clone()), (Some(false), None));

        // Neither config.txt nor sysfs could be read: by the model's word.
        let p = derive_port(&facts("cam1", "csi1"));
        assert_eq!(p.verdict, CameraVerdict::NotDetected);
        assert_eq!((p.bound.driver, p.boot_config.clone()), (None, None));
        assert_eq!(
            p.declared,
            Some(Declared {
                sensor: "imx290".into(),
                source: DeclaredSource::Model
            })
        );
    }

    /// config.txt edited, device not rebooted: the port still carries the
    /// OLD overlay's node. The reader is told to reboot, not to fix a line
    /// that is already right; and `bound.identity` is the bound driver's.
    #[test]
    fn an_edit_without_a_reboot_says_reboot_and_identity_is_the_bound_drivers() {
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx708,cam0\n"),
            nodes: Some(&nodes),
            listed: Some(("imx290", "/base/soc/i2c0mux/i2c@0/imx290@1a")),
            capture: CaptureTruth::Failing,
            ..facts("cam0", "csi0")
        });
        // Its own verdict word (the console keys on it), not `mismatch`.
        assert_eq!(p.verdict, CameraVerdict::RebootRequired);
        assert_eq!(status_word(&p), "reboot-required");
        assert_eq!(
            serde_json::to_string(&p.verdict).unwrap(),
            "\"reboot-required\""
        );
        assert_eq!(
            p.detail,
            "config.txt now declares imx708 on csi0, but the running kernel still has the imx290 overlay loaded. Reboot to apply the config.txt change."
        );
        assert_eq!(
            p.boot_config.as_ref().unwrap().overlay.as_deref(),
            Some("imx708")
        );
        assert_eq!(p.bound.node.as_deref(), Some("imx290@1a"));
        assert_eq!(
            p.bound.identity,
            Some(Identity::None),
            "the bound imx290 driver, not the line's imx708"
        );
        // The same edit with nothing listed (the old overlay's probe failed).
        let nodes = [node("csi0", "imx219@10", None)];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx708,cam0\n"),
            nodes: Some(&nodes),
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::RebootRequired);
        assert_eq!(
            p.detail,
            "config.txt now declares imx708 on csi0, but the running kernel still has the imx219 overlay loaded, and no camera is listed there. Reboot to apply the config.txt change."
        );
        // An overlay OUTSIDE the table whose node is named differently
        // (`arducam-pivariety` instantiates `arducam_pivariety@c`): nothing
        // is inferred from the names, so no "reboot to apply".
        let nodes = [node(
            "csi0",
            "arducam_pivariety@c",
            Some("arducam-pivariety"),
        )];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=arducam-pivariety,cam0\n"),
            nodes: Some(&nodes),
            listed: Some((
                "arducam-pivariety",
                "/base/soc/i2c0mux/i2c@0/arducam_pivariety@c",
            )),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert!(!p.detail.contains("Reboot"), "{}", p.detail);
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=arducam-pivariety,cam0\n"),
            nodes: Some(&nodes),
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::NotDetected);
        assert!(!p.detail.contains("Reboot to apply"), "{}", p.detail);
        // An overlay served by the SAME driver is not a stale edit.
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx462,cam0\n"),
            nodes: Some(&nodes),
            listed: Some(("imx290", "/base/soc/i2c0mux/i2c@0/imx290@1a")),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
    }

    #[test]
    fn verdict_empty_no_line_and_no_node() {
        // The CM4's config.txt has no line for csi1.
        let nodes = [node("csi0", "imx708@1a", Some("imx708"))];
        let p = derive_port(&PortFacts {
            boot_text: Some(CM4),
            nodes: Some(&nodes),
            ..facts("cam1", "csi1")
        });
        assert_eq!(p.verdict, CameraVerdict::Empty);
        assert_eq!(
            p.detail,
            "No camera is configured on csi1: config.txt has no camera dtoverlay line for it (the device model expects imx290). Add the line for the attached sensor and reboot, or leave it if the port is unused."
        );
        assert_eq!(
            p.boot_config,
            Some(BootConfigPort {
                overlay: None,
                line: None,
                lines: 0
            })
        );
        assert_eq!(p.bound.identity, None, "no driver is in use on the port");
        assert_eq!(status_word(&p), "empty");
        // config.txt unreadable, the device tree readable and without a
        // sensor node for the port: still `empty`, and the sentence does not
        // claim to know what config.txt holds.
        let p = derive_port(&PortFacts {
            nodes: Some(&nodes),
            ..facts("cam1", "csi1")
        });
        assert_eq!(p.verdict, CameraVerdict::Empty);
        assert_eq!(p.boot_config, None);
        assert_eq!(
            p.detail,
            "No camera is configured on csi1: no camera overlay is loaded for it in the running kernel (the device model expects imx290). Add the line for the attached sensor and reboot, or leave it if the port is unused."
        );
        // config.txt read (no line), the device tree unreadable: `empty` by
        // config.txt's word.
        let p = derive_port(&PortFacts {
            boot_text: Some(CM4),
            ..facts("cam1", "csi1")
        });
        assert_eq!(p.verdict, CameraVerdict::Empty);
        assert_eq!(p.bound.driver, None);
    }

    #[test]
    fn verdict_mismatch_blames_the_right_thing() {
        // The model is the only declarer (config.txt unreadable): the
        // sentence does not point at config.txt.
        let p = derive_port(&PortFacts {
            listed: Some(("imx708_wide", "/base/soc/i2c0mux/i2c@0/imx708@1a")),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Mismatch);
        assert_eq!(
            p.detail,
            "The device model expects imx290 on csi0, but the camera found there is imx708_wide. It is used as found."
        );
        // Two lines on one port, both overlays loaded, a chip-ID driver
        // bound: the last line is declared, the other one found.
        let nodes = [
            node("csi0", "imx219@10", Some("imx219")),
            node("csi0", "imx708@1a", None),
        ];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx219,cam0\ndtoverlay=imx708,cam0\n"),
            nodes: Some(&nodes),
            listed: Some(("imx219", "/base/soc/i2c0mux/i2c@0/imx219@10")),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Mismatch);
        assert_eq!(
            p.detail,
            "csi0 is declared as imx708 by config.txt, but the camera found there is imx219. It is used as found; make the dtoverlay line in config.txt name the sensor that is really attached, then reboot. config.txt has 2 camera lines for csi0; the one in force is the imx219 line (the kernel bound its driver) and the others did not take effect. Remove them, unless the lines sit under different [section] filters."
        );
        assert_eq!(p.bound.node.as_deref(), Some("imx219@10"), "the bound one");
        assert_eq!(p.bound.identity, Some(Identity::ChipId));
        // `imx290,cam0` then `imx708,cam0`: the imx290 driver is the one
        // bound, so the identity is `none` (not the last line's `chip-id`)
        // and "imx290" is not stated as a finding.
        let nodes = [
            node("csi0", "imx290@1a", Some("imx290")),
            node("csi0", "imx708@1a", None),
        ];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx290,cam0\ndtoverlay=imx708,cam0\n"),
            nodes: Some(&nodes),
            listed: Some(("imx290", "/base/soc/i2c0mux/i2c@0/imx290@1a")),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Mismatch);
        assert_eq!(p.bound.identity, Some(Identity::None));
        assert_eq!(
            p.detail,
            "csi0 is declared as imx708 by config.txt, but the camera there is listed as imx290 by the imx290 driver, which cannot confirm which chip is attached. It is used as found; make the dtoverlay line in config.txt name the sensor that is really attached, then reboot. config.txt has 2 camera lines for csi0; the one in force is the imx290 line (the kernel bound its driver) and the others did not take effect. Remove them, unless the lines sit under different [section] filters."
        );
        // A variant of the declared sensor is not a mismatch.
        let p = derive_port(&PortFacts {
            boot_text: Some(CM4),
            listed: Some(("imx708_wide_noir", "/base/soc/i2c0mux/i2c@0/imx708@1a")),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
    }

    /// A camera overlay outside the table (`ov5647`): its line IS the port's
    /// line when it carries `cam0`, or when the kernel / libcamera name the
    /// same sensor on the port. The model does not become the declarer and
    /// nobody is told to fix a line that is right.
    #[test]
    fn an_overlay_outside_the_table_is_still_the_ports_line() {
        let path = "/base/soc/i2c0mux/i2c@0/ov5647@36";
        let nodes = [node("csi0", "ov5647@36", Some("ov5647"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=vc4-kms-v3d\ndtoverlay=ov5647,cam0\n"),
            nodes: Some(&nodes),
            listed: Some(("ov5647", path)),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert_eq!(
            p.detail,
            "ov5647 on csi0 is detected and streaming. This agent build does not know the ov5647 overlay; the camera is used as found."
        );
        assert_eq!(
            p.boot_config,
            Some(BootConfigPort {
                overlay: Some("ov5647".into()),
                line: Some("dtoverlay=ov5647,cam0".into()),
                lines: 1
            })
        );
        assert_eq!(
            p.declared,
            Some(Declared {
                sensor: "ov5647".into(),
                source: DeclaredSource::BootConfig
            })
        );
        assert_eq!(p.bound.identity, None);
        // csi1 has no token: the line counts because the listing names it.
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=ov5647\n"),
            listed: Some(("ov5647", "/base/soc/i2c0mux/i2c@1/ov5647@36")),
            capture: CaptureTruth::Streaming,
            ..facts("cam1", "csi1")
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert_eq!(p.boot_config.as_ref().unwrap().lines, 1);
        // With nothing saying it is a camera, an unknown overlay without a
        // `cam0` token is not taken for one …
        assert_eq!(
            boot_config_port_with("dtoverlay=ov5647\n", "csi1", &[]).lines,
            0
        );
        // … and a non-camera overlay never is, whatever the hints.
        assert_eq!(
            boot_config_port_with(
                "dtoverlay=vc4-kms-v3d\ndtoverlay=dwc2,dr_mode=host\n",
                "csi1",
                &["imx290"]
            )
            .lines,
            0
        );
        // A hint does not move a `cam0` line to csi1, nor a bare one to csi0.
        assert_eq!(
            boot_config_port_with("dtoverlay=ov5647,cam0\n", "csi1", &["ov5647"]).lines,
            0
        );
        assert_eq!(
            boot_config_port_with("dtoverlay=ov5647\n", "csi0", &["ov5647"]).lines,
            0
        );
    }

    #[test]
    fn the_reported_line_is_capped() {
        let long = format!("dtoverlay=imx290,cam0,{}\n", "x".repeat(500));
        let b = boot_config_port(&long, "csi0");
        assert_eq!(b.line.unwrap().chars().count(), BOOT_LINE_MAX);
        assert_eq!(b.overlay.as_deref(), Some("imx290"));
    }

    /// `unchecked` is "the agent cannot check", never a camera that works:
    /// a device without `rpicam-hello` whose camera streams reads `ok`.
    #[test]
    fn verdict_unchecked_and_the_device_that_cannot_list_cameras() {
        let cannot = PortFacts {
            enumerated: false,
            ..facts("cam0", "csi0")
        };
        let p = derive_port(&cannot);
        assert_eq!(p.verdict, CameraVerdict::Unchecked);
        assert_eq!(
            p.detail,
            "cam0 on csi0 cannot be checked: the agent cannot list cameras on this device (rpicam-hello is missing or libcamera is broken). Install rpicam-apps and restart the agent."
        );
        assert_eq!(status_word(&p), "unchecked");
        let p = derive_port(&PortFacts {
            capture: CaptureTruth::Streaming,
            ..cannot.clone()
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert_eq!(
            p.detail,
            "The camera on csi0 is streaming. It could not be checked against the declared sensor: the agent cannot list cameras on this device (rpicam-hello is missing or libcamera is broken). Install rpicam-apps."
        );
        assert_eq!(status_word(&p), "streaming");
        let p = derive_port(&PortFacts {
            capture: CaptureTruth::Failing,
            ..cannot
        });
        assert_eq!(p.verdict, CameraVerdict::NoFrames);
        assert_eq!(
            p.detail,
            "The capture of cam0 on csi0 keeps failing, and the agent cannot list cameras on this device (rpicam-hello is missing or libcamera is broken), so the port cannot be checked. Install rpicam-apps and restart the agent."
        );
        // A pipeline the agent does not supervise.
        let p = derive_port(&PortFacts {
            boot_text: Some(BENCH1),
            listed: Some(IMX290_CM5),
            unsupervised: true,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Unchecked);
        assert_eq!(
            p.detail,
            "imx290 is detected on csi0, but cam0 uses a custom pipeline that the agent does not supervise, so it cannot tell whether frames flow."
        );
        // The wire word.
        assert_eq!(
            serde_json::to_string(&CameraVerdict::Unchecked).unwrap(),
            "\"unchecked\""
        );
    }

    /// The wire shape, one real case per Phase 0 verdict (`ok` is pinned by
    /// `verdict_ok_cm4_imx708_declared_in_config_model_says_imx290`). The
    /// console and the SDK will parse these: a change here is a contract
    /// change of `effective.cameras`.
    #[test]
    fn one_wire_example_per_verdict() {
        let wire = |f: &PortFacts<'_>| serde_json::to_string(&derive_port(f)).unwrap();

        // not-detected: bench 2, `imx335,cam0`, the sensor does not answer.
        let nodes = [
            node("csi0", "imx335@1a", None),
            node("csi1", "imx219@10", Some("imx219")),
        ];
        assert_eq!(
            wire(&PortFacts {
                boot_text: Some(BENCH2),
                nodes: Some(&nodes),
                capture: CaptureTruth::Absent,
                ..facts("cam0", "csi0")
            }),
            r#"{"connector":"csi0","declared":{"sensor":"imx335","source":"boot-config"},"bootConfig":{"overlay":"imx335","line":"dtoverlay=imx335,cam0","lines":1},"bound":{"driver":false,"node":"imx335@1a","listed":null,"path":null,"identity":"weak"},"capture":{"state":"absent","backoffStep":0,"lastError":null},"verdict":"not-detected","detail":"csi0 is declared as imx335 by config.txt, but no imx335 answered on this port (the kernel driver did not bind). Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached, then reboot."}"#
        );

        // no-frames: an IMX708 under `dtoverlay=imx290` (2026-11-20): a
        // camera is listed, the frontend times out, the capture is in its
        // D86 back-off.
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        assert_eq!(
            wire(&PortFacts {
                boot_text: Some("dtoverlay=imx290,cam0,rotation=180\n"),
                nodes: Some(&nodes),
                listed: Some(("imx290", "/base/soc/i2c0mux/i2c@0/imx290@1a")),
                capture: CaptureTruth::Failing,
                backoff_step: 2,
                last_error: Some(
                    "[0:00:13.417539436] [940] ERROR RPI pipeline_base.cpp:1408 Camera frontend has timed out!"
                ),
                ..facts("cam0", "csi0")
            }),
            r#"{"connector":"csi0","declared":{"sensor":"imx290","source":"boot-config"},"bootConfig":{"overlay":"imx290","line":"dtoverlay=imx290,cam0,rotation=180","lines":1},"bound":{"driver":true,"node":"imx290@1a","listed":"imx290","path":"/base/soc/i2c0mux/i2c@0/imx290@1a","identity":"none"},"capture":{"state":"failing","backoffStep":2,"lastError":"[0:00:13.417539436] [940] ERROR RPI pipeline_base.cpp:1408 Camera frontend has timed out!"},"verdict":"no-frames","detail":"imx290 is listed on csi0 but delivers no frames: its capture process keeps failing. The imx290 driver cannot confirm which chip is attached, so the camera on this port may be a different sensor, or nothing may be attached. Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached."}"#
        );

        // mismatch: a swap that left two lines on one port.
        let nodes = [
            node("csi0", "imx219@10", Some("imx219")),
            node("csi0", "imx708@1a", None),
        ];
        assert_eq!(
            wire(&PortFacts {
                boot_text: Some("dtoverlay=imx219,cam0\ndtoverlay=imx708,cam0\n"),
                nodes: Some(&nodes),
                listed: Some(("imx219", "/base/soc/i2c0mux/i2c@0/imx219@10")),
                capture: CaptureTruth::Streaming,
                ..facts("cam0", "csi0")
            }),
            r#"{"connector":"csi0","declared":{"sensor":"imx708","source":"boot-config"},"bootConfig":{"overlay":"imx708","line":"dtoverlay=imx708,cam0","lines":2},"bound":{"driver":true,"node":"imx219@10","listed":"imx219","path":"/base/soc/i2c0mux/i2c@0/imx219@10","identity":"chip-id"},"capture":{"state":"streaming","backoffStep":0,"lastError":null},"verdict":"mismatch","detail":"csi0 is declared as imx708 by config.txt, but the camera found there is imx219. It is used as found; make the dtoverlay line in config.txt name the sensor that is really attached, then reboot. config.txt has 2 camera lines for csi0; the one in force is the imx219 line (the kernel bound its driver) and the others did not take effect. Remove them, unless the lines sit under different [section] filters."}"#
        );

        // empty: a two-camera model on a device whose config.txt has a
        // camera line for csi0 only.
        let nodes = [node("csi0", "imx708@1a", Some("imx708"))];
        assert_eq!(
            wire(&PortFacts {
                boot_text: Some(CM4),
                nodes: Some(&nodes),
                capture: CaptureTruth::Absent,
                ..facts("cam1", "csi1")
            }),
            r#"{"connector":"csi1","declared":{"sensor":"imx290","source":"model"},"bootConfig":{"overlay":null,"line":null,"lines":0},"bound":{"driver":false,"node":null,"listed":null,"path":null,"identity":null},"capture":{"state":"absent","backoffStep":0,"lastError":null},"verdict":"empty","detail":"No camera is configured on csi1: config.txt has no camera dtoverlay line for it (the device model expects imx290). Add the line for the attached sensor and reboot, or leave it if the port is unused."}"#
        );

        // unchecked: a device without `rpicam-hello` (a fresh Lite image),
        // capture not running.
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        assert_eq!(
            wire(&PortFacts {
                boot_text: Some(BENCH1),
                nodes: Some(&nodes),
                enumerated: false,
                ..facts("cam0", "csi0")
            }),
            r#"{"connector":"csi0","declared":{"sensor":"imx290","source":"boot-config"},"bootConfig":{"overlay":"imx290","line":"dtoverlay=imx290,cam0","lines":1},"bound":{"driver":true,"node":"imx290@1a","listed":null,"path":null,"identity":"none"},"capture":{"state":"off","backoffStep":0,"lastError":null},"verdict":"unchecked","detail":"cam0 on csi0 cannot be checked: the agent cannot list cameras on this device (rpicam-hello is missing or libcamera is broken). Install rpicam-apps and restart the agent."}"#
        );
    }

    #[test]
    fn the_relist_schedule_is_three_early_looks_then_every_five_minutes() {
        let mut t = 0;
        let mut times = Vec::new();
        for _ in 0..6 {
            t = next_relist_ms(t);
            times.push(t);
        }
        assert_eq!(times, [15_000, 30_000, 60_000, 360_000, 660_000, 960_000]);
        // Strictly after `now`, wherever `now` falls.
        assert_eq!(next_relist_ms(14_999), 15_000);
        assert_eq!(next_relist_ms(15_000), 30_000);
        assert_eq!(next_relist_ms(60_000), 360_000);
        assert_eq!(next_relist_ms(359_999), 360_000);
        assert_eq!(next_relist_ms(360_000), 660_000);
        assert_eq!(next_relist_ms(5_000_000), 5_160_000);
        // Only a held port with a bound driver is looked at again.
        assert!(port_needs_relist(true, true));
        assert!(
            !port_needs_relist(true, false),
            "a driver binds at boot only"
        );
        assert!(!port_needs_relist(false, true));
    }

    #[test]
    fn the_report_hold_doubles_up_to_ten_minutes() {
        let mut h = 20_000;
        let mut seen = Vec::new();
        for _ in 0..7 {
            h = next_report_hold_ms(h);
            seen.push(h);
        }
        assert_eq!(
            seen,
            [40_000, 80_000, 160_000, 320_000, 600_000, 600_000, 600_000]
        );
    }

    /// Bench 2, 2026-11-21, E-two-lines: `imx335,cam0` … `imx290,cam0`. The
    /// LAST line won in the kernel (the first failed to register with -16)
    /// and the imx290 driver bound. The sentence names the line in force.
    #[test]
    fn two_lines_on_a_port_name_the_one_the_kernel_took() {
        let text =
            "dtoverlay=imx335,cam0\ndtoverlay=imx219,cam1,rotation=180\ndtoverlay=imx290,cam0\n";
        let nodes = [
            node("csi0", "imx290@1a", Some("imx290")),
            node("csi1", "imx219@10", Some("imx219")),
        ];
        let p = derive_port(&PortFacts {
            boot_text: Some(text),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert_eq!(p.boot_config.as_ref().unwrap().lines, 2);
        assert_eq!(
            p.detail,
            "imx290 on csi0 is detected and streaming. config.txt has 2 camera lines for csi0; the one in force is the imx290 line (the kernel bound its driver) and the others did not take effect. Remove them, unless the lines sit under different [section] filters."
        );
        // Nothing bound (or sysfs unreadable): no line is named.
        let p = derive_port(&PortFacts {
            boot_text: Some(text),
            ..facts("cam0", "csi0")
        });
        assert!(
            p.detail.ends_with(" config.txt has 2 camera lines for csi0; only one can work unless they sit under different [section] filters: remove the stale one."),
            "{}",
            p.detail
        );
    }

    /// Bench 2, 2026-11-21, E-heal: the healed camera could not be acquired
    /// (`EBUSY`) while the other camera's old capture held the media device.
    /// The sentence names "in use by another process" and does not send the
    /// reader to the ribbon cable or the dtoverlay line.
    #[test]
    fn a_camera_in_use_by_another_process_is_not_a_cable_fault() {
        let nodes = [node("csi0", "imx335@1a", Some("imx335"))];
        let p = derive_port(&PortFacts {
            boot_text: Some(BENCH2),
            nodes: Some(&nodes),
            listed: Some((
                "imx335",
                "/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a",
            )),
            capture: CaptureTruth::Failing,
            backoff_step: 1,
            last_error: Some("ERROR: from element /GstPipeline:pipeline0/GstLibcameraSrc:libcamerasrc0: Camera '/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a' is already in use."),
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::NoFrames);
        assert_eq!(
            p.detail,
            "imx335 is listed on csi0 but its capture cannot open it: the camera is in use by another process. The usual causes are another capture or camera program running on this device, or a capture that was started before libcamera was changed. Stop the other program, or restart the agent."
        );
        assert!(!p.detail.contains("ribbon") && !p.detail.contains("dtoverlay"));
    }

    /// libcamera colours its log lines; the report carries plain text, and
    /// the skip / fatal lists match the plain text. The coloured lines are
    /// the real ones bench 2 put in state.json and in the cloud.
    #[test]
    fn coloured_lines_are_reported_as_plain_text() {
        let doc: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/camera_truth/capture-stderr.json"
        ))
        .unwrap();
        let coloured = doc["coloured"].as_array().unwrap();
        assert_eq!(coloured.len(), 5);
        for c in coloured {
            let (raw, plain) = (c["raw"].as_str().unwrap(), c["plain"].as_str().unwrap());
            assert!(raw.contains('\u{1b}'), "the fixture keeps the escape bytes");
            assert!(!plain.contains('\u{1b}'));
            assert_eq!(strip_ansi(raw), plain);
            assert_eq!(error_line(raw).as_deref(), Some(plain));
            // The frame timeout is fatal whatever its colours.
            assert_eq!(fatal_error_line(raw).as_deref(), Some(plain));
        }
        // A coloured line on the skip list is still skipped.
        assert_eq!(
            error_line("[0:00:19.1] [1114] \u{1b}[1;31mERROR \u{1b}[1;37mV4L2 \u{1b}[1;34mv4l2_device.cpp:390 \u{1b}[0m'imx219 0-0010': Unable to set controls: Device or resource busy"),
            None
        );
        // Other escapes, a bare ESC at the end, and plain text.
        assert_eq!(
            strip_ansi("a\u{1b}[0mb\u{1b}[38;5;196mc\u{1b}cd\u{1b}"),
            "abcd"
        );
        assert_eq!(strip_ansi("plain"), "plain");
        // Only lines on the fatal list are fatal.
        assert_eq!(
            fatal_error_line("ERROR: from element /GstPipeline:pipeline0/GstShmSink:shmsink0: Failed waiting on fd activity"),
            None
        );
        assert_eq!(
            fatal_error_line("INFO Camera frontend has timed out!"),
            None
        );
    }

    #[test]
    fn the_summary_ignores_the_ladder_and_the_wording() {
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let base = PortFacts {
            boot_text: Some(BENCH1),
            nodes: Some(&nodes),
            listed: Some((
                "imx290",
                "/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a",
            )),
            capture: CaptureTruth::Failing,
            ..facts("cam0", "csi0")
        };
        let a = derive_port(&PortFacts {
            backoff_step: 1,
            last_error: Some("ERROR a"),
            ..base.clone()
        });
        let b = derive_port(&PortFacts {
            backoff_step: 5,
            last_error: Some("ERROR b"),
            ..base.clone()
        });
        assert_ne!(a, b);
        assert_eq!(summarise_port("cam0", &a), summarise_port("cam0", &b));
        // The capture state and the verdict do move it.
        let c = derive_port(&PortFacts {
            capture: CaptureTruth::Streaming,
            ..base
        });
        assert_ne!(summarise_port("cam0", &a), summarise_port("cam0", &c));
    }

    // -----------------------------------------------------------------------
    // Phase 2: the sensor setting in the report
    // -----------------------------------------------------------------------

    const IMX708_CM4: (&str, &str) = ("imx708_wide", "/base/soc/i2c0mux/i2c@0/imx708@1a");

    fn managed<'a>(sensor: &'a str) -> PortSetting<'a> {
        PortSetting {
            sensor: Some(sensor),
            ..Default::default()
        }
    }

    /// A managed port that is applied and booted reports as any healthy
    /// port, with `declared.source` = `config`; the wire's keys and their
    /// order are unchanged.
    #[test]
    fn a_managed_port_declares_by_config_and_keeps_the_wire() {
        let nodes = [node("csi0", "imx708@1a", Some("imx708"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx708,cam0,rotation=180\n"),
            nodes: Some(&nodes),
            listed: Some(IMX708_CM4),
            capture: CaptureTruth::Streaming,
            setting: managed("imx708"),
            ..facts("cam0", "csi0")
        });
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"connector":"csi0","declared":{"sensor":"imx708","source":"config"},"bootConfig":{"overlay":"imx708","line":"dtoverlay=imx708,cam0,rotation=180","lines":1},"bound":{"driver":true,"node":"imx708@1a","listed":"imx708_wide","path":"/base/soc/i2c0mux/i2c@0/imx708@1a","identity":"chip-id"},"capture":{"state":"streaming","backoffStep":0,"lastError":null},"verdict":"ok","detail":"imx708_wide on csi0 is detected and streaming."}"#
        );
        // The same port unmanaged: only `declared.source` differs.
        let u = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx708,cam0,rotation=180\n"),
            nodes: Some(&nodes),
            listed: Some(IMX708_CM4),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        });
        assert_eq!(
            u.declared.as_ref().unwrap().source,
            DeclaredSource::BootConfig
        );
        assert_eq!((u.verdict, &u.detail), (p.verdict, &p.detail));
        assert_eq!(DeclaredSource::Config.as_str(), "config");
        assert_eq!(
            serde_json::to_string(&DeclaredSource::Config).unwrap(),
            r#""config""#
        );
    }

    #[test]
    fn verdict_unknown_sensor() {
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx290,cam0\n"),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Streaming,
            setting: managed("imx999"),
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::UnknownSensor);
        assert_eq!(p.verdict.as_str(), "unknown-sensor");
        assert_eq!(
            p.detail,
            "cam0 on csi0 is set to \"imx999\", a camera sensor this agent build does not know (it knows: imx290, imx219, imx708, imx335). Update the agent, or choose another sensor. The camera line in config.txt is left as it is."
        );
        // What is really there is still reported beside it.
        assert_eq!(p.declared.as_ref().unwrap().sensor, "imx999");
        assert_eq!(p.declared.as_ref().unwrap().source, DeclaredSource::Config);
        assert_eq!(
            p.boot_config.as_ref().unwrap().overlay.as_deref(),
            Some("imx290")
        );
        assert_eq!(p.bound.listed.as_deref(), Some("imx290"));
        assert_eq!(p.capture.state, CaptureTruth::Streaming);
        // Also where cameras cannot be listed.
        let p = derive_port(&PortFacts {
            enumerated: false,
            setting: managed("imx999"),
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::UnknownSensor);
    }

    #[test]
    fn verdict_requirements_missing() {
        const MISSING: &str = "needs extra packages that are not installed in the required versions: libcamera0.5 0.5.2+rpt20250909-1 (installed: 0.5.0+rpt20250429-1), libcamera-ipa 0.5.2+rpt20250909-1 (not installed). The camera will not appear until they are installed";
        // Bench 2 on stock libcamera: the driver binds, libcamera lists nothing.
        let nodes = [node("csi0", "imx335@1a", Some("imx335"))];
        let base = PortFacts {
            boot_text: Some("dtoverlay=imx335,cam0\n"),
            nodes: Some(&nodes),
            relisted: true,
            capture: CaptureTruth::Absent,
            setting: PortSetting {
                missing: Some(MISSING),
                ..managed("imx335")
            },
            ..facts("cam0", "csi0")
        };
        let p = derive_port(&base);
        assert_eq!(p.verdict, CameraVerdict::RequirementsMissing);
        assert_eq!(p.verdict.as_str(), "requirements-missing");
        assert_eq!(p.detail, format!("csi0 is set to imx335, which {MISSING}."));
        // With the line still waiting for a reboot, both are said: a reboot
        // alone would not bring the camera.
        let change = LineChange {
            booted: Some("dtoverlay=imx290,cam0".into()),
            now: Some("dtoverlay=imx335,cam0,rotation=180".into()),
            overlay: true,
            rotation_only: false,
        };
        let p = derive_port(&PortFacts {
            setting: PortSetting {
                line_change: Some(&change),
                ..base.setting.clone()
            },
            ..base.clone()
        });
        assert_eq!(p.verdict, CameraVerdict::RequirementsMissing);
        assert_eq!(
            p.detail,
            format!("csi0 is set to imx335, which {MISSING}. Its camera line in config.txt also waits for a reboot.")
        );
        // The camera IS listed as the declared sensor (other packages do the
        // job): the port is judged as it works, not by the package list.
        let p = derive_port(&PortFacts {
            listed: Some((
                "imx335",
                "/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a",
            )),
            capture: CaptureTruth::Streaming,
            ..base.clone()
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        // An UNMANAGED port never says it (the report as it was).
        let p = derive_port(&PortFacts {
            setting: PortSetting {
                sensor: None,
                ..base.setting.clone()
            },
            ..base
        });
        assert_eq!(p.verdict, CameraVerdict::NotDetected);
        assert_eq!(
            p.detail,
            "The imx335 driver is bound on csi0 but libcamera lists no camera there (checked again after the agent started): libcamera on this device may not support this sensor."
        );
    }

    /// A camera line that changed since this boot waits for a reboot,
    /// whatever changed (the overlay, an option, the rotation), on a
    /// managed and on an unmanaged port alike.
    #[test]
    fn a_line_changed_since_boot_says_reboot_required() {
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let healthy = PortFacts {
            boot_text: Some("dtoverlay=imx290,cam0,rotation=180\n"),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        };
        assert_eq!(derive_port(&healthy).verdict, CameraVerdict::Ok);
        let cases = [
            (
                LineChange {
                    booted: Some("dtoverlay=imx290,cam0".into()),
                    now: Some("dtoverlay=imx290,cam0,rotation=180".into()),
                    overlay: false,
                    rotation_only: false,
                },
                "The camera line of csi0 in config.txt was changed since this device booted (it booted with `dtoverlay=imx290,cam0`, the file now says `dtoverlay=imx290,cam0,rotation=180`). Reboot to apply.",
            ),
            (
                LineChange {
                    booted: None,
                    now: Some("dtoverlay=imx290,cam0,rotation=180".into()),
                    overlay: true,
                    rotation_only: false,
                },
                "config.txt has a camera line for csi0 that was added since this device booted (`dtoverlay=imx290,cam0,rotation=180`). Reboot to apply.",
            ),
            (
                LineChange {
                    booted: Some("dtoverlay=imx290,cam0".into()),
                    now: None,
                    overlay: true,
                    rotation_only: false,
                },
                "The camera line of csi0 was removed from config.txt since this device booted (it booted with `dtoverlay=imx290,cam0`); the running kernel still has that camera configured. Reboot to apply.",
            ),
        ];
        for (change, sentence) in &cases {
            for sensor in [None, Some("imx290")] {
                let p = derive_port(&PortFacts {
                    setting: PortSetting {
                        sensor,
                        line_change: Some(change),
                        ..Default::default()
                    },
                    ..healthy.clone()
                });
                assert_eq!(p.verdict, CameraVerdict::RebootRequired, "{sensor:?}");
                assert_eq!(&p.detail, sentence, "{sensor:?}");
                // The camera that runs meanwhile is still reported.
                assert_eq!(p.capture.state, CaptureTruth::Streaming);
            }
        }
    }

    /// A REAL FAULT OUTRANKS A PENDING CHANGE (review N1). After a rotation
    /// or an option was rewritten, a camera that dies, or that does not
    /// answer, keeps its own verdict; the sentence ends by saying that the
    /// line also waits for a reboot. `reboot-required` is for a port with
    /// nothing else wrong. When the OVERLAY changes, what runs now is what
    /// the reboot replaces, and the port reads `reboot-required`.
    #[test]
    fn a_real_fault_outranks_a_pending_line_change() {
        const ALSO: &str = " The camera line of csi0 in config.txt also waits for a reboot: it was changed since this device booted.";
        let rotated = LineChange {
            booted: Some("dtoverlay=imx290,cam0".into()),
            now: Some("dtoverlay=imx290,cam0,rotation=180".into()),
            overlay: false,
            rotation_only: false,
        };
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let running = PortFacts {
            boot_text: Some("dtoverlay=imx290,cam0,rotation=180\n"),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Streaming,
            setting: PortSetting {
                line_change: Some(&rotated),
                ..Default::default()
            },
            ..facts("cam0", "csi0")
        };
        // Nothing else wrong: reboot-required.
        assert_eq!(derive_port(&running).verdict, CameraVerdict::RebootRequired);
        // The capture is failing: `no-frames`, with the usual sentence and
        // the closing clause. Managed or not; what to check is the camera
        // sensor setting on a managed port, the line in config.txt otherwise.
        for (sensor, names) in [
            (None, "the dtoverlay line in config.txt"),
            (Some("imx290"), "the camera sensor setting"),
        ] {
            let p = derive_port(&PortFacts {
                capture: CaptureTruth::Failing,
                last_error: Some("ERROR: Camera frontend has timed out!"),
                setting: PortSetting {
                    sensor,
                    ..running.setting.clone()
                },
                ..running.clone()
            });
            assert_eq!(p.verdict, CameraVerdict::NoFrames, "{sensor:?}");
            assert_eq!(
                p.detail,
                format!("imx290 is listed on csi0 but delivers no frames: its capture process keeps failing. The imx290 driver cannot confirm which chip is attached, so the camera on this port may be a different sensor, or nothing may be attached. Check the ribbon cable and that {names} names the sensor that is really attached.{ALSO}"),
                "{sensor:?}"
            );
            // The longest this sentence gets stays inside the 500 characters
            // a status text may have.
            assert!(p.detail.encode_utf16().count() <= 500, "{}", p.detail.len());
        }
        // Nothing answers on a line whose overlay did not change (an option
        // was rewritten on a chip-ID sensor that is not there).
        let option = LineChange {
            booted: Some("dtoverlay=imx708,cam0,rotation=180".into()),
            now: Some("dtoverlay=imx708,cam0,link-frequency=447000000,rotation=180".into()),
            overlay: false,
            rotation_only: false,
        };
        let unbound = [node("csi0", "imx708@1a", None)];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx708,cam0,link-frequency=447000000,rotation=180\n"),
            nodes: Some(&unbound),
            listed: None,
            capture: CaptureTruth::Absent,
            setting: PortSetting {
                sensor: Some("imx708"),
                line_change: Some(&option),
                ..Default::default()
            },
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::NotDetected);
        assert_eq!(
            p.detail,
            format!("csi0 is set to imx708, but no imx708 answered on this port (the kernel driver did not bind). Check the ribbon cable and the selected sensor.{ALSO}")
        );
        // The OVERLAY changed (the user is replacing a camera that fails):
        // what runs now is not the new line's fault. reboot-required.
        let swapped = LineChange {
            booted: Some("dtoverlay=imx290,cam0".into()),
            now: Some("dtoverlay=imx708,cam0,rotation=180".into()),
            overlay: true,
            rotation_only: false,
        };
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx708,cam0,rotation=180\n"),
            capture: CaptureTruth::Failing,
            last_error: Some("ERROR: x"),
            setting: PortSetting {
                sensor: Some("imx708"),
                line_change: Some(&swapped),
                ..Default::default()
            },
            ..running.clone()
        });
        assert_eq!(p.verdict, CameraVerdict::RebootRequired);
        // A line ADDED on an empty port: nothing answers yet, by design.
        let added = LineChange {
            booted: None,
            now: Some("dtoverlay=imx219,cam0,rotation=180".into()),
            overlay: true,
            rotation_only: false,
        };
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx219,cam0,rotation=180\n"),
            nodes: Some(&[]),
            listed: None,
            capture: CaptureTruth::Absent,
            setting: PortSetting {
                sensor: Some("imx219"),
                line_change: Some(&added),
                ..Default::default()
            },
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::RebootRequired);
    }

    /// config.txt does not hold the declared line (the reconciler could not
    /// write it, or has not yet): the port says the setting is not applied.
    #[test]
    fn a_setting_that_is_not_applied_is_a_mismatch_with_its_own_sentence() {
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let base = PortFacts {
            boot_text: Some("dtoverlay=imx290,cam0\n"),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Streaming,
            ..facts("cam0", "csi0")
        };
        let say = |f: &PortFacts<'_>| {
            let p = derive_port(f);
            (p.verdict, p.detail)
        };
        assert_eq!(
            say(&PortFacts {
                setting: managed("imx708"),
                ..base.clone()
            }),
            (
                CameraVerdict::Mismatch,
                "csi0 is set to imx708, but its camera line in config.txt still names imx290: the setting has not been applied (the status of the camera sensor setting says why).".to_string()
            )
        );
        assert_eq!(
            say(&PortFacts {
                setting: managed("none"),
                ..base.clone()
            }),
            (
                CameraVerdict::Mismatch,
                "csi0 is declared empty by the camera sensor setting, but config.txt still has a camera line for it (imx290): the setting has not been applied (the status of the camera sensor setting says why).".to_string()
            )
        );
        assert_eq!(
            say(&PortFacts {
                boot_text: Some("dtoverlay=vc4-kms-v3d\n"),
                nodes: Some(&[]),
                listed: None,
                capture: CaptureTruth::Absent,
                setting: managed("imx708"),
                ..base.clone()
            }),
            (
                CameraVerdict::Mismatch,
                "csi0 is set to imx708, but config.txt has no camera line for it: the setting has not been applied (the status of the camera sensor setting says why).".to_string()
            )
        );
        // config.txt unreadable: the setting cannot be compared; the port is
        // judged as found.
        let p = derive_port(&PortFacts {
            boot_text: None,
            setting: managed("imx708"),
            ..base.clone()
        });
        assert_eq!(p.verdict, CameraVerdict::Mismatch);
        assert_eq!(
            p.detail,
            "csi0 is set to imx708, but the camera there is listed as imx290 by the imx290 driver, which cannot confirm which chip is attached. It is used as found; choose the sensor that is really attached, then reboot."
        );
    }

    #[test]
    fn a_port_declared_empty() {
        // Applied and booted: nothing configured, nothing there.
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx290,cam1\n"),
            nodes: Some(&[]),
            capture: CaptureTruth::Absent,
            setting: managed("none"),
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::Empty);
        assert_eq!(
            p.detail,
            "csi0 is declared empty by the camera sensor setting: no camera is configured on it."
        );
        assert_eq!(
            serde_json::to_value(&p.declared).unwrap(),
            serde_json::json!({"sensor": "none", "source": "config"})
        );
        // The line is gone, the kernel still has the camera (the removal
        // was made before the agent first ran in this boot).
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx290,cam1\n"),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Streaming,
            setting: managed("none"),
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::RebootRequired);
        assert_eq!(
            p.detail,
            "csi0 is declared empty and config.txt has no camera line for it, but the running kernel still has a camera configured there. Reboot to apply."
        );
    }

    /// A managed port names the setting, not config.txt, in its sentences,
    /// and tells the user to check the SELECTED sensor.
    #[test]
    fn a_managed_port_that_is_not_detected_points_at_the_selection() {
        // The driver did not bind (a chip-ID sensor that is not there).
        let nodes = [node("csi0", "imx219@10", None)];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx219,cam0,rotation=180\n"),
            nodes: Some(&nodes),
            capture: CaptureTruth::Absent,
            setting: managed("imx219"),
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.verdict, CameraVerdict::NotDetected);
        assert_eq!(
            p.detail,
            "csi0 is set to imx219, but no imx219 answered on this port (the kernel driver did not bind). Check the ribbon cable and the selected sensor."
        );
        // The overlay is not loaded at all.
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx219,cam0,rotation=180\n"),
            nodes: Some(&[]),
            capture: CaptureTruth::Absent,
            setting: managed("imx219"),
            ..facts("cam0", "csi0")
        });
        assert_eq!(
            (p.verdict, p.detail.as_str()),
            (
                CameraVerdict::NotDetected,
                "csi0 is set to imx219, but its overlay is not loaded in the running kernel. Reboot if the setting was just changed; otherwise check that the overlay file exists on this device."
            )
        );
        // The line is the setting's, the kernel still runs another overlay
        // (an edit the boot snapshot did not see): reboot.
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx708,cam0,rotation=180\n"),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Streaming,
            setting: managed("imx708"),
            ..facts("cam0", "csi0")
        });
        assert_eq!(
            (p.verdict, p.detail.as_str()),
            (
                CameraVerdict::RebootRequired,
                "config.txt now declares imx708 on csi0, but the running kernel still has the imx290 overlay loaded. Reboot to apply the config.txt change."
            )
        );
    }

    /// The running video preset against the catalog's `unsupportedPresets`:
    /// said in the port's sentence, the verdict stays about the camera.
    #[test]
    fn a_preset_the_sensor_cannot_run_is_named() {
        const REASON: &str = " The video preset 1080p30-4mbps cannot run on this sensor. The IMX335's full-resolution mode tops out at 29.99 frames per second, so a 30 fps 1080p stream cannot start. Use 1080p15-2mbps or a 720p preset.";
        let nodes = [node("csi0", "imx335@1a", Some("imx335"))];
        let listed = Some((
            "imx335",
            "/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a",
        ));
        let base = PortFacts {
            boot_text: Some("dtoverlay=imx335,cam0\n"),
            nodes: Some(&nodes),
            listed,
            capture: CaptureTruth::Failing,
            last_error: Some("ERROR: could not negotiate"),
            setting: PortSetting {
                preset: Some("1080p30-4mbps"),
                ..managed("imx335")
            },
            ..facts("cam0", "csi0")
        };
        let p = derive_port(&base);
        assert_eq!(p.verdict, CameraVerdict::NoFrames);
        assert!(p.detail.ends_with(REASON), "{}", p.detail);
        // While it is only starting, the note is already there.
        let p = derive_port(&PortFacts {
            capture: CaptureTruth::Starting,
            last_error: None,
            ..base.clone()
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert_eq!(
            p.detail,
            format!("imx335 on csi0 is detected; capture is starting.{REASON}")
        );
        // An unmanaged port whose line names the sensor gets it too.
        let p = derive_port(&PortFacts {
            setting: PortSetting {
                sensor: None,
                ..base.setting.clone()
            },
            ..base.clone()
        });
        assert!(p.detail.ends_with(REASON), "{}", p.detail);
        // A preset the sensor can run, a sensor without a list, no preset
        // known, and a port with no camera: nothing is added.
        for (preset, sensor, text) in [
            (Some("720p15-1mbps"), "imx335", "dtoverlay=imx335,cam0\n"),
            (None, "imx335", "dtoverlay=imx335,cam0\n"),
        ] {
            let p = derive_port(&PortFacts {
                boot_text: Some(text),
                setting: PortSetting {
                    preset,
                    ..managed(sensor)
                },
                ..base.clone()
            });
            assert!(
                !p.detail.contains("video preset"),
                "{preset:?}: {}",
                p.detail
            );
        }
        let nodes290 = [node("csi0", "imx290@1a", Some("imx290"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx290,cam0\n"),
            nodes: Some(&nodes290),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Streaming,
            last_error: None,
            setting: PortSetting {
                preset: Some("1080p30-4mbps"),
                ..Default::default()
            },
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.detail, "imx290 on csi0 is detected and streaming.");
    }

    /// The "only weakly checks" caveat is not put on a correct camera that
    /// is merely starting; a driver with no identity check keeps its note.
    #[test]
    fn the_weak_identity_caveat_is_not_in_the_starting_sentence() {
        let nodes = [node("csi0", "imx335@1a", Some("imx335"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx335,cam0\n"),
            nodes: Some(&nodes),
            listed: Some((
                "imx335",
                "/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a",
            )),
            capture: CaptureTruth::Starting,
            ..facts("cam0", "csi0")
        });
        assert_eq!(p.detail, "imx335 on csi0 is detected; capture is starting.");
        let nodes = [node("csi0", "imx290@1a", Some("imx290"))];
        let p = derive_port(&PortFacts {
            boot_text: Some("dtoverlay=imx290,cam0\n"),
            nodes: Some(&nodes),
            listed: Some(IMX290_CM5),
            capture: CaptureTruth::Starting,
            ..facts("cam0", "csi0")
        });
        assert_eq!(
            p.detail,
            "imx290 on csi0 is detected; capture is starting. The imx290 driver cannot confirm which chip is attached, so the camera on this port may be a different sensor, or nothing may be attached."
        );
    }

    /// Bench finding (Phase 1b run): with a port's line under a section
    /// the agent cannot evaluate, the reconciler refused to touch it while
    /// the report read it as the port's line and said nothing. The report
    /// now says what the reconciler knows; the verdict stays about the
    /// camera.
    #[test]
    fn a_line_under_a_section_the_agent_cannot_evaluate_is_named() {
        let text =
            "dtoverlay=imx335,cam0\n[x-unknown]\ndtoverlay=imx219,cam1,rotation=180\n[all]\n";
        let nodes = [node("csi1", "imx219@10", Some("imx219"))];
        let base = PortFacts {
            boot_text: Some(text),
            board: Board::Cm5,
            nodes: Some(&nodes),
            listed: Some((
                "imx219",
                "/base/axi/pcie@1000120000/rp1/i2c@70000/imx219@10",
            )),
            capture: CaptureTruth::Streaming,
            ..facts("cam1", "csi1")
        };
        let p = derive_port(&base);
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert_eq!(p.boot_config.as_ref().unwrap().lines, 1);
        assert_eq!(
            p.detail,
            "imx219 on csi1 is detected and streaming. The camera line of csi1 in config.txt sits under [x-unknown], a section the agent cannot evaluate for this board: the agent does not edit that line."
        );
        // The same on a managed port.
        let p = derive_port(&PortFacts {
            setting: managed("imx219"),
            ..base.clone()
        });
        assert_eq!(p.verdict, CameraVerdict::Ok);
        assert!(
            p.detail.ends_with("the agent does not edit that line."),
            "{}",
            p.detail
        );
        // A section the agent CAN evaluate says nothing: this board's, and
        // the other family's (whose line is not the port's at all).
        for (section, lines) in [("cm5", 1), ("all", 1), ("cm4", 0)] {
            let text = format!("[{section}]\ndtoverlay=imx219,cam1,rotation=180\n[all]\n");
            let p = derive_port(&PortFacts {
                boot_text: Some(&text),
                ..base.clone()
            });
            assert_eq!(p.boot_config.as_ref().unwrap().lines, lines, "{section}");
            assert!(
                !p.detail.contains("cannot evaluate"),
                "{section}: {}",
                p.detail
            );
        }
        // With the board unknown every model filter is one it cannot evaluate.
        let p = derive_port(&PortFacts {
            boot_text: Some("[cm5]\ndtoverlay=imx219,cam1\n"),
            board: Board::Unknown,
            ..base.clone()
        });
        assert!(
            p.detail
                .contains("sits under [cm5], a section the agent cannot evaluate"),
            "{}",
            p.detail
        );
        assert_eq!(
            unknown_section_of(text, "csi1", &[], Board::Cm5).as_deref(),
            Some("x-unknown")
        );
        assert_eq!(unknown_section_of(text, "csi0", &[], Board::Cm5), None);
    }
}
