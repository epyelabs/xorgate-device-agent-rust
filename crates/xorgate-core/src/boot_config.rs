//! The boot config (`/boot/firmware/config.txt`) as the agent is allowed to
//! read it: the camera `dtoverlay=` lines, and on them ONE parameter the
//! agent may change in this phase, `rotation`.
//!
//! Two layers live here:
//!
//! - The port of the pure half of `src/hardware/boot-config.ts`
//!   (cam-sensor-level-rotate plan): [`parse_overlay_lines`] (`:63-151`), the
//!   device-tree node derivation + big-endian decode of `dtRotationForCamera`
//!   (`:166-179`, the `readFileSync` injected), `resolveBootConfigPath`
//!   (`:54`, `existsSync` injected), `resolveCamToken` (`:304`) and the
//!   reconciler's verdict/state types (`:219-253`).
//! - The camera-line model of the support-other-camera-models plan (Phase 1,
//!   RUST-ONLY; accepted divergences D95-D104): which `[section]` a line sits
//!   under and whether it applies to this board ([`parse_scoped_lines`]),
//!   which port a camera line configures ([`port_of_line`]), "no `rotation=`
//!   token" as its own case ([`rotation_token`], bug B3), the rotation a line
//!   gives with the sensor's default ([`effective_rotation`]), the two
//!   renderers ([`render_overlay_line`], [`line_with_rotation`]) and the ONE
//!   mint of the `xorgate-camcfg` helper call ([`camcfg_call`]).
//!
//! THE RULE: the device-tree rotation in force is the overlay's default
//! rotation plus the `cameraMount` setting (mod 360). The default is the
//! sensor catalog's (`imx290` 0; `imx219`, `imx708`, `imx335` 180). "No
//! token" and "property absent" read as that default, never as 0.
//!
//! Left in the HAL/agent: the file reads, the `dtoverlay -h` probe and the
//! reconciler (`xorgate-agent::ops::camera_mount`).
//!
//! THE SAFETY INVARIANT carries over: nothing here can reboot the device.

use serde::{Deserialize, Serialize};

use crate::camera_catalog::{
    reserved_param_name, valid_overlay_name, valid_overlay_param, Catalog, CONFIG_TXT_LINE_MAX,
};
use crate::hardware_block::DeviceHardwareBlock;
use crate::js;

/// `BOOT_CONFIG_CANDIDATES` (`boot-config.ts:46`): Bookworm first, then the
/// pre-Bookworm path.
pub const BOOT_CONFIG_CANDIDATES: &[&str] = &["/boot/firmware/config.txt", "/boot/config.txt"];

/// `PROC_DEVICE_TREE` (`boot-config.ts:49`): the root of the LIVE device tree.
pub const PROC_DEVICE_TREE: &str = "/proc/device-tree";

/// The camera-line owner: the ONE privileged writer of config.txt the Rust
/// agent calls (`system/xorgate-camcfg`, sudoers `025_xorgate-camcfg`). The
/// JS agent's `xorgate-camrotate` stays installed for a device reverted to
/// JS; the Rust agent no longer calls it (D95).
pub const CAMCFG_HELPER: &str = "/usr/local/bin/xorgate-camcfg";

/// The helper's allow-list of overlay names, installed beside it
/// (`system/xorgate-camcfg.overlays`, generated from the catalog).
pub const CAMCFG_ALLOW_LIST: &str = "/usr/local/bin/xorgate-camcfg.overlays";

/// `resolveBootConfigPath` (`boot-config.ts:54`) over an injected `existsSync`.
pub fn resolve_boot_config_path<'a>(
    candidates: &[&'a str],
    exists: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    candidates.iter().copied().find(|p| exists(p))
}

// --- parsing ----------------------------------------------------------------

/// `OverlayLine` (`boot-config.ts:66`): one `dtoverlay=` line, split into the
/// overlay name and its parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayLine {
    /// 0-based line number in the file.
    pub index: usize,
    /// Overlay name — the first comma-separated token (e.g. `imx290`).
    pub name: String,
    /// The remaining tokens, verbatim (`cam0`, `4lane`, `rotation=180`, …).
    pub params: Vec<String>,
    /// The line as it appears in the file, without its newline.
    pub text: String,
}

/// `parseOverlayLines` (`boot-config.ts:91`): every `dtoverlay=` line in the
/// file, in order. Commented-out lines (`#dtoverlay=…`) are skipped; section
/// conditionals (`[cm4]`, `[all]`, …) are deliberately NOT interpreted —
/// every matching line must carry the same rotation.
pub fn parse_overlay_lines(text: &str) -> Vec<OverlayLine> {
    let mut out = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        let trimmed = js::trim(line);
        let Some(value) = trimmed.strip_prefix("dtoverlay=") else {
            continue;
        };
        let tokens: Vec<String> = value
            .split(',')
            .map(js::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        let Some(name) = tokens.first().cloned() else {
            continue;
        };
        out.push(OverlayLine {
            index,
            name,
            params: tokens[1..].to_vec(),
            text: line.to_string(),
        });
    }
    out
}

/// `overlayCamToken` (`boot-config.ts:108`): `csi0` -> `cam0`
/// (`/^csi(\d+)$/` on the trimmed connector). None for a connector this
/// mapping does not know.
pub fn overlay_cam_token(connector: &str) -> Option<String> {
    let digits = js::trim(connector).strip_prefix("csi")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("cam{digits}"))
}

// --- [section] filters: does a line apply to this board? (Q4) ---------------

/// The board the agent runs on, as far as the config.txt model filters go.
/// From `/proc/device-tree/model` ([`board_from_model`]), the same file the
/// helper reads, so the reader and the writer always agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Board {
    /// `Raspberry Pi Compute Module 4 Rev …`: `[cm4]` and `[pi4]` apply.
    Cm4,
    /// `Raspberry Pi Compute Module 5 [Lite ]Rev …`: `[cm5]` and `[pi5]` apply.
    Cm5,
    /// Anything else (a Compute Module 4S included), or no model file.
    #[default]
    Unknown,
}

/// The board of a device-tree `model` string (NULs and whitespace trimmed).
pub fn board_from_model(model: &str) -> Board {
    let m = model.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if m.starts_with("Raspberry Pi Compute Module 4 Rev") {
        Board::Cm4
    } else if m.starts_with("Raspberry Pi Compute Module 5 Rev")
        || m.starts_with("Raspberry Pi Compute Module 5 Lite Rev")
    {
        Board::Cm5
    } else {
        Board::Unknown
    }
}

/// Whether a config.txt line takes effect on this board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineScope {
    /// No `[section]` above it, or `[all]`, or a model filter that names
    /// this board.
    Applies,
    /// Under a model filter that names the OTHER Compute Module family.
    NotApplies,
    /// Under any other filter, or under a model filter while the board is
    /// unknown. Such a line may or may not take effect: it is never edited.
    Unknown,
}

/// THE SECTION RULE (Q4; `system/xorgate-camcfg` implements the same table):
///
/// | header | effect |
/// |---|---|
/// | none yet, or `[all]` | applies; every filter is cleared |
/// | `[cm4]`, `[pi4]` | applies on a CM4, does not on a CM5, unknown on any other board |
/// | `[cm5]`, `[pi5]` | applies on a CM5, does not on a CM4, unknown on any other board |
/// | anything else (`[none]`, `[pi3]`, `[cm4s]`, `[gpio4=1]`, `[EDID=…]`, `[tryboot]`, a serial…) | unknown, and stays unknown until the next `[all]` |
///
/// A model filter replaces the previous model filter; the other filter
/// kinds add up until `[all]`, which is why "unknown" sticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SectionState {
    scope: LineScope,
    /// A filter this rule does not evaluate is in force (until `[all]`).
    sticky_unknown: bool,
}

impl SectionState {
    const START: Self = Self {
        scope: LineScope::Applies,
        sticky_unknown: false,
    };

    fn enter(self, header: &str, board: Board) -> Self {
        if header == "all" {
            return Self::START;
        }
        if self.sticky_unknown {
            return self;
        }
        let family = match header {
            "cm4" | "pi4" => Some(Board::Cm4),
            "cm5" | "pi5" => Some(Board::Cm5),
            _ => None,
        };
        match (family, board) {
            (Some(_), Board::Unknown) => Self {
                scope: LineScope::Unknown,
                sticky_unknown: false,
            },
            (Some(f), b) => Self {
                scope: if f == b {
                    LineScope::Applies
                } else {
                    LineScope::NotApplies
                },
                sticky_unknown: false,
            },
            (None, _) => Self {
                scope: LineScope::Unknown,
                sticky_unknown: true,
            },
        }
    }
}

/// The `name` of a `[name]` section header line, `None` for any other line.
pub fn section_header(line: &str) -> Option<&str> {
    js::trim(line).strip_prefix('[')?.strip_suffix(']')
}

/// One `dtoverlay=` line with the section it sits under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedLine {
    pub line: OverlayLine,
    pub scope: LineScope,
    /// The header in force (`cm4`), `None` before the first one.
    pub section: Option<String>,
}

/// Every `dtoverlay=` line of the file ([`parse_overlay_lines`]) with its
/// [`LineScope`] on `board`.
pub fn parse_scoped_lines(text: &str, board: Board) -> Vec<ScopedLine> {
    let mut lines = parse_overlay_lines(text).into_iter().peekable();
    let mut out = Vec::new();
    let mut state = SectionState::START;
    let mut section: Option<String> = None;
    for (index, raw) in text.split('\n').enumerate() {
        // A clean `[name]` header; any other line that starts with `[` is
        // taken for a filter this rule cannot evaluate.
        let trimmed = js::trim(raw);
        if trimmed.starts_with('[') {
            let name = section_header(raw).unwrap_or(trimmed);
            state = state.enter(name, board);
            section = Some(name.to_string());
            continue;
        }
        if lines.peek().is_some_and(|l| l.index == index) {
            out.push(ScopedLine {
                line: lines.next().expect("peeked"),
                scope: state.scope,
                section: section.clone(),
            });
        }
    }
    out
}

// --- which port a camera line configures (Q5, Q6) ---------------------------

/// A camera port of the carrier: the only two a camera overlay can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CamPort {
    /// CAM0 / `csi0`: a camera line WITH the `cam0` parameter.
    Cam0,
    /// CAM1 / `csi1`: every other camera line.
    Cam1,
}

impl CamPort {
    /// The `config.txt` token and the helper argument (`cam0` / `cam1`).
    pub fn token(self) -> &'static str {
        match self {
            CamPort::Cam0 => "cam0",
            CamPort::Cam1 => "cam1",
        }
    }

    /// The carrier connector the hardware block names (`csi0` / `csi1`).
    pub fn connector(self) -> &'static str {
        match self {
            CamPort::Cam0 => "csi0",
            CamPort::Cam1 => "csi1",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "cam0" => Some(CamPort::Cam0),
            "cam1" => Some(CamPort::Cam1),
            _ => None,
        }
    }

    pub fn from_connector(connector: &str) -> Option<Self> {
        match connector {
            "csi0" => Some(CamPort::Cam0),
            "csi1" => Some(CamPort::Cam1),
            _ => None,
        }
    }
}

/// The port a CAMERA overlay line configures. The only real port selector
/// is `cam0`: a line with it is CAM0; without it the overlay targets CAM1,
/// WITH OR WITHOUT a `cam1` token (`cam1` is not an overlay parameter; the
/// firmware skips it). Only meaningful for a line that is a camera line.
pub fn port_of_line(line: &OverlayLine) -> CamPort {
    if line.params.iter().any(|p| p == "cam0") {
        CamPort::Cam0
    } else {
        CamPort::Cam1
    }
}

/// Is `line` a camera line the agent may attribute to `port` when it wants
/// to WRITE? (The report is more generous: `camera_truth::boot_config_port_with`.)
///
/// - An overlay IN the catalog: by [`port_of_line`].
/// - An overlay OUTSIDE the catalog (Q6): only with an explicit port token,
///   `cam0` for CAM0, or `cam1` and no `cam0` for CAM1. Without one the
///   agent cannot know the line is a camera's (`dtoverlay=vc4-kms-v3d` has
///   no `cam0` either), so it does not touch it.
pub fn line_is_ports(line: &OverlayLine, port: CamPort, catalog: &Catalog) -> bool {
    if catalog.by_overlay(&line.name).is_some() {
        return port_of_line(line) == port;
    }
    let has = |t: &str| line.params.iter().any(|p| p == t);
    match port {
        CamPort::Cam0 => has("cam0"),
        CamPort::Cam1 => has("cam1") && !has("cam0"),
    }
}

/// The camera lines of `port` that may take effect on this board, in file
/// order: [`LineScope::Applies`] and [`LineScope::Unknown`] lines (a line
/// known not to apply is not the port's).
///
/// WHICH ONE IS "THE PORT'S LINE" (Q5): with exactly one, that one. With
/// several, the LAST is the one reported (the later overlay rewrites the
/// port's endpoint, clock and regulator), and the agent changes nothing on
/// the port until the user has removed the stale ones.
pub fn camera_lines_for_port<'a>(
    lines: &'a [ScopedLine],
    port: CamPort,
    catalog: &Catalog,
) -> Vec<&'a ScopedLine> {
    lines
        .iter()
        .filter(|l| l.scope != LineScope::NotApplies && line_is_ports(&l.line, port, catalog))
        .collect()
}

// --- rotation: the token, and what it means with the overlay default (B3) ---

/// The `rotation=` token of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationToken {
    /// No `rotation=` token: the overlay's own default applies. NOT 0: the
    /// `imx219`, `imx708` and `imx335` overlays default to 180 (bug B3).
    Absent,
    /// Exactly one token whose value is 0 or 180 as the TS reads it
    /// (`Number()`): `rotation=0`, `rotation=180`, and the hand-written
    /// spellings v0.1.1 also took for settled (`0x0`, `00`, `0.0`, `180.0`,
    /// `1.8e2`, an empty value = 0).
    Deg(u16),
    /// A value that is not a number or is another number, or more than one
    /// token: the line needs a rewrite.
    Invalid,
}

/// The [`RotationToken`] of a line.
pub fn rotation_token(line: &OverlayLine) -> RotationToken {
    let mut tokens = line.params.iter().filter(|p| p.starts_with("rotation="));
    let Some(first) = tokens.next() else {
        return RotationToken::Absent;
    };
    if tokens.next().is_some() {
        return RotationToken::Invalid;
    }
    // `Number(token.slice("rotation=".length))` (`boot-config.ts:121`): a
    // line a hand wrote as `rotation=0x0` is not rewritten for its spelling.
    let value = js::number(&first["rotation=".len()..]);
    if value == 0.0 {
        RotationToken::Deg(0)
    } else if value == 180.0 {
        RotationToken::Deg(180)
    } else {
        RotationToken::Invalid
    }
}

/// The device-tree rotation a line gives on the next boot: its token, else
/// `default_rotation` (the overlay's default). `None` = the token is not
/// one the agent understands.
pub fn line_rotation(line: &OverlayLine, default_rotation: u16) -> Option<u16> {
    match rotation_token(line) {
        RotationToken::Absent => Some(default_rotation),
        RotationToken::Deg(d) => Some(d),
        RotationToken::Invalid => None,
    }
}

/// [`line_rotation`] with the catalog's default for the line's overlay.
/// `None` also when the overlay is outside the catalog and the line has no
/// token: its default is then not known here.
pub fn effective_rotation(line: &OverlayLine, catalog: &Catalog) -> Option<u16> {
    match (rotation_token(line), catalog.by_overlay(&line.name)) {
        (RotationToken::Deg(d), _) => Some(d),
        (RotationToken::Absent, Some(s)) => Some(s.default_rotation),
        _ => None,
    }
}

/// THE RULE: the device-tree rotation that shows a camera mounted at
/// `mount_deg` the right way up = the overlay's default + the mount, mod 360.
pub fn rotation_for_mount(default_rotation: u16, mount_deg: u16) -> u16 {
    (default_rotation + mount_deg) % 360
}

/// The inverse of [`rotation_for_mount`]: how the camera is mounted, given a
/// device-tree rotation and the overlay's default.
pub fn mount_of_rotation(default_rotation: u16, rotation: u32) -> u32 {
    (rotation % 360 + 360 - u32::from(default_rotation) % 360) % 360
}

// --- rendering --------------------------------------------------------------

/// Why a line cannot be rendered or edited by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// A token is outside the helper's charset.
    Token(String),
    /// A port token (`cam0`, `cam1`, also `cam0=…`) or a `rotation…` among
    /// the parameters: the renderer writes those itself.
    Reserved(String),
    /// A parameter name given twice.
    Repeated(String),
    /// Longer than the firmware reads.
    TooLong(usize),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Token(t) => write!(
                f,
                "\"{t}\" holds characters the camera line owner does not write"
            ),
            RenderError::Reserved(t) => {
                write!(f, "\"{t}\" is written by the camera line owner itself")
            }
            RenderError::Repeated(t) => write!(f, "the parameter \"{t}\" is given twice"),
            RenderError::TooLong(n) => write!(
                f,
                "the line would be {n} characters; the firmware reads at most {CONFIG_TXT_LINE_MAX}"
            ),
        }
    }
}

/// A camera line built from parts (Phase 2 "set"; the helper builds the
/// same text from the same parts): `dtoverlay=<overlay>,<cam0|cam1>,<params…>,rotation=<deg>`.
/// It ALWAYS carries an explicit `rotation=` (bug B2: on a default-180
/// overlay "no token" is not 0) and an explicit port token (`cam1` is
/// skipped by the firmware and kept for the reader).
pub fn render_overlay_line(
    overlay: &str,
    port: CamPort,
    params: &[String],
    rotation: u16,
) -> Result<String, RenderError> {
    if !valid_overlay_name(overlay) {
        return Err(RenderError::Token(overlay.to_string()));
    }
    let mut out = format!("dtoverlay={overlay},{}", port.token());
    let mut names = std::collections::BTreeSet::new();
    for p in params {
        if !valid_overlay_param(p) {
            return Err(RenderError::Token(p.clone()));
        }
        // A port token or `rotation` in ANY form (`cam0=on` selects CAM0 as
        // surely as `cam0` does): written here, never taken.
        let name = p.split('=').next().unwrap_or(p);
        if reserved_param_name(name) {
            return Err(RenderError::Reserved(p.clone()));
        }
        if !names.insert(name) {
            return Err(RenderError::Repeated(name.to_string()));
        }
        out.push(',');
        out.push_str(p);
    }
    out.push_str(&format!(",rotation={rotation}"));
    if out.len() > CONFIG_TXT_LINE_MAX {
        return Err(RenderError::TooLong(out.len()));
    }
    Ok(out)
}

/// `line` with its rotation set to `rotation` and NOTHING else changed:
/// every other token is kept verbatim and in order. An existing `rotation=`
/// token is replaced where it stands (further ones are dropped); without
/// one the token is appended. This is the text the helper's `rotate` verb
/// writes; `crates/xorgate-agent/tests/camera_mount.rs` holds the two to
/// each other.
pub fn line_with_rotation(line: &OverlayLine, rotation: u16) -> Result<String, RenderError> {
    if !valid_overlay_name(&line.name) {
        return Err(RenderError::Token(line.name.clone()));
    }
    let mut out = format!("dtoverlay={}", line.name);
    let mut placed = false;
    for p in &line.params {
        if p.starts_with("rotation=") {
            if !placed {
                out.push_str(&format!(",rotation={rotation}"));
                placed = true;
            }
            continue;
        }
        if !valid_overlay_param(p) {
            return Err(RenderError::Token(p.clone()));
        }
        out.push(',');
        out.push_str(p);
    }
    if !placed {
        out.push_str(&format!(",rotation={rotation}"));
    }
    if out.len() > CONFIG_TXT_LINE_MAX {
        return Err(RenderError::TooLong(out.len()));
    }
    Ok(out)
}

// --- what the kernel actually booted with -----------------------------------

/// The node-path half of `dtRotationForCamera` (`boot-config.ts:170-171`):
/// libcamera's `/base/…` prefix maps onto `/proc/device-tree`; an empty node
/// or one containing `..` is refused.
pub fn dt_rotation_node(camera_name: &str) -> Option<String> {
    let trimmed = js::trim(camera_name);
    let node = trimmed.strip_prefix("/base/").unwrap_or(trimmed);
    if node.is_empty() || node.contains("..") {
        return None;
    }
    Some(node.to_string())
}

/// The decode half of `dtRotationForCamera` (`boot-config.ts:173-175`): the
/// property must be exactly 4 bytes, read as a big-endian u32.
pub fn dt_rotation_from_bytes(buf: &[u8]) -> Option<u32> {
    let b: [u8; 4] = buf.try_into().ok()?;
    Some(u32::from_be_bytes(b))
}

/// `dtRotationForCamera` (`boot-config.ts:166`) over an injected reader that
/// returns the bytes of `<dtRoot>/<node>/rotation` (None on any error). None
/// when the node or property is missing — the caller must say it could not
/// confirm, never assume 0.
pub fn dt_rotation_for_camera(
    camera_name: &str,
    read_rotation: impl FnOnce(&str) -> Option<Vec<u8>>,
) -> Option<u32> {
    let node = dt_rotation_node(camera_name)?;
    dt_rotation_from_bytes(&read_rotation(&node)?)
}

// --- the reconciler's types + the one pure resolver ------------------------

/// The reconciler's verdict/state types (`boot-config.ts:219-253`) have ONE
/// definition in the crate: `crate::types` (WP1, 7-verdict since the WP1
/// review fixes). Re-exported here so boot-config callers keep one path.
pub use crate::types::{CameraMountCameraState, CameraMountState, CameraMountVerdict};

impl CameraMountVerdict {
    /// The wire string (= the serde name), used where the TS interpolates the
    /// verdict into a status detail (`config-sync.ts:893`).
    pub fn as_str(self) -> &'static str {
        match self {
            CameraMountVerdict::Applied => "applied",
            CameraMountVerdict::PendingReboot => "pending-reboot",
            CameraMountVerdict::Unconfirmed => "unconfirmed",
            CameraMountVerdict::UnknownCamera => "unknown-camera",
            CameraMountVerdict::NoOverlay => "no-overlay",
            CameraMountVerdict::Unsupported => "unsupported",
            CameraMountVerdict::WriteFailed => "write-failed",
        }
    }
}

/// `resolveCamToken` (`boot-config.ts:304`): which `camN` token in config.txt
/// belongs to a stream key. With a hardware block that block is the whole
/// truth (an undeclared key = a camera this device does not have); only a v1
/// device (no block) falls back to "a `camN` stream key stands for itself".
pub fn resolve_cam_token(
    stream_key: &str,
    hardware: Option<&DeviceHardwareBlock>,
) -> Option<String> {
    if let Some(hw) = hardware {
        let entry = hw.video.iter().find(|v| v.key == stream_key)?;
        return overlay_cam_token(&entry.connector);
    }
    let digits = stream_key.strip_prefix("cam")?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| stream_key.to_string())
}

// --- the camera-line owner's call (`system/xorgate-camcfg`) -----------------

/// What one `xorgate-camcfg` call does. The helper's command line
/// (`system/xorgate-camcfg`, "usage") is exactly [`CamCfgOp::args`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CamCfgOp {
    /// `rotate <cam0|cam1> <overlay> <0|180>`: on the port's ONE camera
    /// line, which must name `overlay`, set the `rotation=` token and change
    /// nothing else. The only verb this phase uses.
    Rotate {
        port: CamPort,
        overlay: String,
        rotation: u16,
    },
    /// `set <cam0|cam1> <overlay> [param…] rotation=<0|180>`: make the
    /// port's camera line `dtoverlay=<overlay>,<port>,<param…>,rotation=<deg>`
    /// (replace it, or add it when the port has none), and make
    /// `camera_auto_detect=0`. Phase 2 (the sensor setting).
    Set {
        port: CamPort,
        overlay: String,
        /// The parameters between the port token and `rotation=`.
        params: Vec<String>,
        rotation: u16,
    },
    /// `remove <cam0|cam1>`: delete the port's camera line(s). Phase 2
    /// (a port declared empty).
    Remove { port: CamPort },
}

impl CamCfgOp {
    /// The helper's own arguments.
    pub fn args(&self) -> Vec<String> {
        match self {
            CamCfgOp::Rotate {
                port,
                overlay,
                rotation,
            } => vec![
                "rotate".to_string(),
                port.token().to_string(),
                overlay.clone(),
                rotation.to_string(),
            ],
            CamCfgOp::Set {
                port,
                overlay,
                params,
                rotation,
            } => {
                let mut v = vec!["set".to_string(), port.token().to_string(), overlay.clone()];
                v.extend(params.iter().cloned());
                v.push(format!("rotation={rotation}"));
                v
            }
            CamCfgOp::Remove { port } => vec!["remove".to_string(), port.token().to_string()],
        }
    }
}

/// `system/xorgate-camcfg`'s usage line (stderr, exit 2).
pub const CAMCFG_USAGE: &str = "usage: xorgate-camcfg rotate <cam0|cam1> <overlay> <0|180> | set <cam0|cam1> <overlay> [param...] rotation=<0|180> | remove <cam0|cam1>";

/// The ONLY mint of a `xorgate-camcfg` [`crate::commands::HelperCall`] (I5),
/// with the helper's OWN argument checks applied first, so a value the
/// helper would refuse never reaches sudo:
///
/// - the overlay name is `[a-z0-9][a-z0-9_-]*`, at most 32 characters;
/// - the rotation is 0 or 180;
/// - `set`: the overlay is in the catalog (the helper's allow-list is
///   generated from it), every parameter passes the strict token charset,
///   its NAME is one the catalog lists for that overlay, none is given
///   twice, none is a port token or a `rotation` in any form (`cam0=on`,
///   `rotation-x`), and the rendered line is at most 98 characters.
///
/// `Err` is a sentence for the cloud-visible status detail. (The helper
/// checks the same things again as root: its arguments are not trusted.)
pub fn camcfg_call(op: CamCfgOp, catalog: &Catalog) -> Result<crate::commands::HelperCall, String> {
    let bad_rotation =
        |r: u16| (r != 0 && r != 180).then(|| format!("rotation {r} is not 0 or 180"));
    match &op {
        CamCfgOp::Rotate {
            overlay, rotation, ..
        } => {
            if !valid_overlay_name(overlay) {
                return Err(format!(
                    "the overlay name \"{overlay}\" holds characters the camera line owner does not accept"
                ));
            }
            if let Some(e) = bad_rotation(*rotation) {
                return Err(e);
            }
        }
        CamCfgOp::Set {
            port,
            overlay,
            params,
            rotation,
        } => {
            if let Some(e) = bad_rotation(*rotation) {
                return Err(e);
            }
            let Some(sensor) = catalog.by_overlay(overlay) else {
                return Err(format!(
                    "the overlay \"{overlay}\" is not in this agent build's sensor catalog"
                ));
            };
            render_overlay_line(overlay, *port, params, *rotation).map_err(|e| e.to_string())?;
            // The helper's allow-list names the parameters of each overlay
            // (it is rendered from the same catalog entry).
            let allowed = sensor.param_names();
            if let Some(p) = params
                .iter()
                .find(|p| !allowed.contains(&p.split('=').next().unwrap_or(p)))
            {
                return Err(format!(
                    "\"{p}\" is not a parameter the sensor catalog lists for the {overlay} overlay"
                ));
            }
        }
        CamCfgOp::Remove { .. } => {}
    }
    Ok(crate::commands::HelperCall::mint(
        crate::commands::HelperKind::CamCfg { op },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_catalog::catalog;

    fn line(text: &str) -> OverlayLine {
        parse_overlay_lines(text).remove(0)
    }

    #[test]
    fn camcfg_call_is_the_helpers_command_line() {
        let c = camcfg_call(
            CamCfgOp::Rotate {
                port: CamPort::Cam0,
                overlay: "imx708".into(),
                rotation: 180,
            },
            catalog(),
        )
        .unwrap();
        assert_eq!(
            c.argv(),
            [
                "sudo",
                "-n",
                "/usr/local/bin/xorgate-camcfg",
                "rotate",
                "cam0",
                "imx708",
                "180"
            ]
        );
        assert_eq!(c.timeout_ms(), Some(20_000));
        // An overlay outside the catalog may have its rotation token set
        // (Q6): the name only has to be a clean one.
        let c = camcfg_call(
            CamCfgOp::Rotate {
                port: CamPort::Cam1,
                overlay: "ov5647_x-1".into(),
                rotation: 0,
            },
            catalog(),
        )
        .unwrap();
        assert_eq!(c.helper_args(), ["rotate", "cam1", "ov5647_x-1", "0"]);
        let c = camcfg_call(
            CamCfgOp::Set {
                port: CamPort::Cam1,
                overlay: "imx290".into(),
                params: vec!["clock-frequency=74250000".into()],
                rotation: 0,
            },
            catalog(),
        )
        .unwrap();
        assert_eq!(
            c.helper_args(),
            [
                "set",
                "cam1",
                "imx290",
                "clock-frequency=74250000",
                "rotation=0"
            ]
        );
        let c = camcfg_call(
            CamCfgOp::Remove {
                port: CamPort::Cam0,
            },
            catalog(),
        )
        .unwrap();
        assert_eq!(c.helper_args(), ["remove", "cam0"]);
    }

    #[test]
    fn camcfg_call_refuses_what_the_helper_would_refuse() {
        for overlay in [
            "",
            "IMX290",
            "../../etc/passwd",
            "imx 290",
            "imx290;reboot",
            "imx290,cam0",
            "imx290\ndtoverlay=x",
            "-n",
        ] {
            let e = camcfg_call(
                CamCfgOp::Rotate {
                    port: CamPort::Cam0,
                    overlay: overlay.into(),
                    rotation: 180,
                },
                catalog(),
            )
            .unwrap_err();
            assert!(e.contains("holds characters"), "{overlay:?}: {e}");
        }
        for rotation in [90, 270, 1, 360] {
            for op in [
                CamCfgOp::Rotate {
                    port: CamPort::Cam0,
                    overlay: "imx290".into(),
                    rotation,
                },
                CamCfgOp::Set {
                    port: CamPort::Cam0,
                    overlay: "imx290".into(),
                    params: vec![],
                    rotation,
                },
            ] {
                assert_eq!(
                    camcfg_call(op, catalog()).unwrap_err(),
                    format!("rotation {rotation} is not 0 or 180")
                );
            }
        }
        let set = |overlay: &str, params: &[&str]| {
            camcfg_call(
                CamCfgOp::Set {
                    port: CamPort::Cam0,
                    overlay: overlay.into(),
                    params: params.iter().map(|p| p.to_string()).collect(),
                    rotation: 0,
                },
                catalog(),
            )
        };
        // `set` renders a line: catalog overlays only.
        assert_eq!(
            set("imx477", &[]).unwrap_err(),
            "the overlay \"imx477\" is not in this agent build's sensor catalog"
        );
        assert_eq!(
            set("vc4-kms-v3d", &[]).unwrap_err(),
            "the overlay \"vc4-kms-v3d\" is not in this agent build's sensor catalog"
        );
        for bad in ["a b", "x=1,y=2", "x=$(reboot)", "X=1", "x=", "[all]", "#x"] {
            assert!(
                set("imx290", &[bad])
                    .unwrap_err()
                    .contains("holds characters"),
                "{bad:?}"
            );
        }
        // A port token or a `rotation` in ANY form is written by the owner,
        // never taken: `cam0=on` would select CAM0 on a line filed as CAM1.
        for reserved in [
            "cam0",
            "cam1",
            "cam0=on",
            "cam0=1",
            "cam1=on",
            "rotation=180",
            "rotation",
            "rotation-x=1",
            "rotations",
        ] {
            assert!(
                set("imx290", &[reserved])
                    .unwrap_err()
                    .contains("written by the camera line owner itself"),
                "{reserved:?}"
            );
        }
        // Parameter NAMES are the catalog's, per overlay (the helper's
        // allow-list is rendered from the same entry), each at most once.
        assert!(set("imx290", &["clock-frequency=74250000"]).is_ok());
        assert!(set("imx708", &["link-frequency=447000000"]).is_ok());
        for (overlay, param) in [
            ("imx290", "4lane"),
            ("imx290", "link-frequency=450000000"),
            ("imx708", "clock-frequency=74250000"),
            ("imx219", "vcm"),
            ("imx290", "media-controller=0"),
        ] {
            assert_eq!(
                set(overlay, &[param]).unwrap_err(),
                format!(
                    "\"{param}\" is not a parameter the sensor catalog lists for the {overlay} overlay"
                )
            );
        }
        assert_eq!(
            set(
                "imx290",
                &["clock-frequency=37125000", "clock-frequency=74250000"]
            )
            .unwrap_err(),
            "the parameter \"clock-frequency\" is given twice"
        );
        let names = |id: &str| catalog().by_id(id).unwrap().param_names();
        assert_eq!(names("imx290"), ["clock-frequency"]);
        assert_eq!(names("imx219"), Vec::<&str>::new());
        assert_eq!(names("imx708"), ["link-frequency"]);
        assert_eq!(names("imx335"), ["link-frequency"]);
        // 98 characters is the firmware's limit; 99 is refused. (A single
        // token is at most 32 characters, so the length comes from several.)
        let many =
            |n: usize| -> Vec<String> { (0..n).map(|i| format!("p{i:02}=12345678")).collect() };
        let l = |n: usize| render_overlay_line("imx290", CamPort::Cam0, &many(n), 0);
        assert_eq!(l(5).unwrap().len(), 97);
        assert_eq!(l(6), Err(RenderError::TooLong(110)));
        let exact: Vec<String> = [many(4), vec!["abcdefghijklm".to_string()]].concat();
        let exact = render_overlay_line("imx290", CamPort::Cam0, &exact, 0).unwrap();
        assert_eq!(exact.len(), 98);
        let over: Vec<String> = [many(4), vec!["abcdefghijklmn".to_string()]].concat();
        assert_eq!(
            render_overlay_line("imx290", CamPort::Cam0, &over, 0),
            Err(RenderError::TooLong(99))
        );
        assert_eq!(
            camcfg_call(
                CamCfgOp::Set {
                    port: CamPort::Cam0,
                    overlay: "imx290".into(),
                    params: over,
                    rotation: 0
                },
                catalog()
            )
            .unwrap_err(),
            "the line would be 99 characters; the firmware reads at most 98"
        );
    }

    #[test]
    fn js_number_subset() {
        // The shared `Number(string)` (crate::js), as the TS and
        // `rotation_token` read the value of a `rotation=` token.
        let n = js::number;
        assert_eq!(n(""), 0.0);
        assert_eq!(n("  "), 0.0);
        assert_eq!(n("180"), 180.0);
        assert_eq!(n(" 180 "), 180.0);
        assert_eq!(n("1.8e2"), 180.0);
        assert_eq!(n("0x0"), 0.0);
        assert_eq!(n("0xb4"), 180.0);
        assert!(n("abc").is_nan());
        assert!(n("inf").is_nan());
        assert!(n("nan").is_nan());
        assert!(n("1_80").is_nan());
        assert_eq!(n("Infinity"), f64::INFINITY);
        // review-wp4 NIT 7: no sign after a radix prefix (JS: NaN)
        assert!(n("0x+b4").is_nan());
        assert!(n("0x-b4").is_nan());
    }

    /// B3: "no token" is its own case, and what it means depends on the
    /// overlay.
    #[test]
    fn no_rotation_token_is_the_overlays_default_not_zero() {
        let t = |text: &str| rotation_token(&line(text));
        assert_eq!(t("dtoverlay=imx290,cam0"), RotationToken::Absent);
        assert_eq!(t("dtoverlay=imx708,cam0"), RotationToken::Absent);
        assert_eq!(t("dtoverlay=imx708,cam0,rotation=0"), RotationToken::Deg(0));
        assert_eq!(
            t("dtoverlay=imx290,rotation=180,cam0"),
            RotationToken::Deg(180)
        );
        // The spellings v0.1.1 took for 0 or 180 (the TS `Number()`): one
        // such token is settled, not a line to rewrite.
        for (token, want) in [
            ("rotation=", 0),
            ("rotation=0x0", 0),
            ("rotation=00", 0),
            ("rotation=0.0", 0),
            ("rotation=-0", 0),
            ("rotation=180.0", 180),
            ("rotation=1.8e2", 180),
            ("rotation=0xb4", 180),
        ] {
            assert_eq!(
                t(&format!("dtoverlay=imx290,cam0,{token}")),
                RotationToken::Deg(want),
                "{token}"
            );
        }
        // Not a number, another number, or two tokens: needs a rewrite.
        for bad in [
            "rotation=90",
            "rotation=270",
            "rotation=x",
            "rotation=180deg",
            "rotation=1e3",
            "rotation=0,rotation=180",
            "rotation=180,rotation=180",
        ] {
            assert_eq!(
                t(&format!("dtoverlay=imx290,cam0,{bad}")),
                RotationToken::Invalid,
                "{bad}"
            );
        }
        // Not the token: a parameter that merely starts alike.
        assert_eq!(
            t("dtoverlay=imx290,cam0,rotations=1"),
            RotationToken::Absent
        );

        let c = catalog();
        let eff = |text: &str| effective_rotation(&line(text), c);
        assert_eq!(eff("dtoverlay=imx290,cam0"), Some(0));
        assert_eq!(eff("dtoverlay=imx290,cam1"), Some(0));
        assert_eq!(eff("dtoverlay=imx219,cam1"), Some(180));
        assert_eq!(eff("dtoverlay=imx708,cam0"), Some(180));
        assert_eq!(eff("dtoverlay=imx335,cam0"), Some(180));
        assert_eq!(eff("dtoverlay=imx708,cam0,rotation=0"), Some(0));
        assert_eq!(eff("dtoverlay=imx290,cam0,rotation=180"), Some(180));
        // An overlay outside the catalog: only its token is known here.
        assert_eq!(eff("dtoverlay=imx477,cam0"), None);
        assert_eq!(eff("dtoverlay=imx477,cam0,rotation=0"), Some(0));
        assert_eq!(eff("dtoverlay=imx708,cam0,rotation=90"), None);
    }

    /// THE RULE, both ways, for the four catalog sensors.
    #[test]
    fn device_tree_rotation_is_default_plus_mount() {
        assert_eq!(rotation_for_mount(0, 0), 0);
        assert_eq!(rotation_for_mount(0, 180), 180);
        assert_eq!(rotation_for_mount(180, 0), 180);
        assert_eq!(rotation_for_mount(180, 180), 0);
        for default in [0u16, 180] {
            for mount in [0u16, 180] {
                let rot = rotation_for_mount(default, mount);
                assert_eq!(mount_of_rotation(default, u32::from(rot)), u32::from(mount));
            }
        }
        // A hand-set 90 or 270 reads as itself, never as 0 or 180.
        assert_eq!(mount_of_rotation(0, 90), 90);
        assert_eq!(mount_of_rotation(180, 90), 270);
        assert_eq!(mount_of_rotation(180, 270), 90);
        assert_eq!(mount_of_rotation(180, 540), 0);
    }

    #[test]
    fn a_cam0_token_is_cam0_and_everything_else_is_cam1() {
        let p = |text: &str| port_of_line(&line(text));
        assert_eq!(p("dtoverlay=imx708,cam0"), CamPort::Cam0);
        assert_eq!(p("dtoverlay=imx290,rotation=180,cam0"), CamPort::Cam0);
        assert_eq!(p("dtoverlay=imx219,cam1,rotation=180"), CamPort::Cam1);
        assert_eq!(p("dtoverlay=imx219"), CamPort::Cam1);
        assert_eq!(
            p("dtoverlay=imx290,clock-frequency=74250000"),
            CamPort::Cam1
        );
        // Only the exact token selects the port.
        assert_eq!(p("dtoverlay=imx290,xcam0"), CamPort::Cam1);
        assert_eq!(p("dtoverlay=imx290,cam0=1"), CamPort::Cam1);
        assert_eq!(CamPort::Cam0.token(), "cam0");
        assert_eq!(CamPort::Cam1.connector(), "csi1");
        assert_eq!(CamPort::from_token("cam1"), Some(CamPort::Cam1));
        assert_eq!(CamPort::from_token("cam10"), None);
        assert_eq!(CamPort::from_connector("csi0"), Some(CamPort::Cam0));
        assert_eq!(CamPort::from_connector("csi2"), None);
    }

    /// Q6: a line whose overlay is outside the catalog is a port's camera
    /// line only with an explicit port token.
    #[test]
    fn a_line_outside_the_catalog_needs_an_explicit_port_token() {
        let c = catalog();
        let is = |text: &str, port| line_is_ports(&line(text), port, c);
        assert!(is("dtoverlay=imx219", CamPort::Cam1));
        assert!(!is("dtoverlay=imx219", CamPort::Cam0));
        assert!(is("dtoverlay=imx290,cam0", CamPort::Cam0));
        assert!(is("dtoverlay=imx477,cam0", CamPort::Cam0));
        assert!(is("dtoverlay=imx477,cam1", CamPort::Cam1));
        assert!(!is("dtoverlay=imx477", CamPort::Cam1));
        assert!(!is("dtoverlay=imx477,cam0,cam1", CamPort::Cam1));
        // The lines that are not a camera's are nobody's.
        for other in [
            "dtoverlay=vc4-kms-v3d",
            "dtoverlay=dwc2,dr_mode=host",
            "dtoverlay=googlevoicehat-soundcard",
        ] {
            assert!(!is(other, CamPort::Cam0), "{other}");
            assert!(!is(other, CamPort::Cam1), "{other}");
        }
    }

    #[test]
    fn boards_from_the_device_tree_model() {
        // The three benches, read 2026-11-21 (the file ends in a NUL).
        assert_eq!(
            board_from_model("Raspberry Pi Compute Module 4 Rev 1.1\0"),
            Board::Cm4
        );
        assert_eq!(
            board_from_model("Raspberry Pi Compute Module 5 Rev 1.0\0"),
            Board::Cm5
        );
        assert_eq!(
            board_from_model("Raspberry Pi Compute Module 5 Lite Rev 1.0\0\n"),
            Board::Cm5
        );
        for other in [
            "",
            "\0",
            "Raspberry Pi Compute Module 4S Rev 1.0",
            "Raspberry Pi 5 Model B Rev 1.0",
            "Raspberry Pi 4 Model B Rev 1.5",
            "Raspberry Pi Compute Module 3 Plus Rev 1.0",
            "Compute Module 5 Rev 1.0",
        ] {
            assert_eq!(board_from_model(other), Board::Unknown, "{other:?}");
        }
    }

    /// Q4: the section table, line by line.
    #[test]
    fn section_filters_decide_whether_a_line_applies() {
        const TEXT: &str = "dtoverlay=a\n\
[cm4]\n\
dtoverlay=b\n\
[cm5]\n\
dtoverlay=c\n\
[pi4]\n\
dtoverlay=d\n\
[pi5]\n\
dtoverlay=e\n\
[all]\n\
dtoverlay=f\n\
[none]\n\
dtoverlay=g\n\
[cm5]\n\
dtoverlay=h\n\
[all]\n\
dtoverlay=i\n\
  [gpio4=1]  \r\n\
dtoverlay=j\n\
[pi3]\n\
dtoverlay=k\n\
[all] # not a header\n\
dtoverlay=l\n\
[all]\n\
dtoverlay=m\n\
#[cm4]\n\
dtoverlay=n\n\
[EDID=VSC-TD2220]\n\
[cm4]\n\
dtoverlay=o\n";
        use LineScope::{Applies as A, NotApplies as N, Unknown as U};
        let scopes = |board| -> String {
            parse_scoped_lines(TEXT, board)
                .iter()
                .map(|l| {
                    format!(
                        "{}{}",
                        l.line.name,
                        match l.scope {
                            A => '+',
                            N => '-',
                            U => '?',
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        //                              a  b  c  d  e  f  g  h  i  j  k  l  m  n  o
        assert_eq!(
            scopes(Board::Cm4),
            "a+ b+ c- d+ e- f+ g? h? i+ j? k? l? m+ n+ o?"
        );
        assert_eq!(
            scopes(Board::Cm5),
            "a+ b- c+ d- e+ f+ g? h? i+ j? k? l? m+ n+ o?"
        );
        assert_eq!(
            scopes(Board::Unknown),
            "a+ b? c? d? e? f+ g? h? i+ j? k? l? m+ n+ o?"
        );
        let l = parse_scoped_lines(TEXT, Board::Cm5);
        assert_eq!(l[0].section, None);
        assert_eq!(l[1].section.as_deref(), Some("cm4"));
        assert_eq!(l[9].section.as_deref(), Some("gpio4=1"));
        assert_eq!(l[11].section.as_deref(), Some("[all] # not a header"));
        assert_eq!(section_header(" [cm4]\r"), Some("cm4"));
        assert_eq!(section_header("#[cm4]"), None);
        assert_eq!(section_header("dtoverlay=x"), None);
    }

    #[test]
    fn the_ports_lines_leave_out_what_does_not_apply() {
        let c = catalog();
        let text = "[cm4]\ndtoverlay=imx708,cam0\n[cm5]\ndtoverlay=imx290,cam0\ndtoverlay=imx290,cam1\n[all]\n#dtoverlay=imx219\n";
        let names = |board, port| -> Vec<String> {
            let lines = parse_scoped_lines(text, board);
            camera_lines_for_port(&lines, port, c)
                .iter()
                .map(|l| l.line.text.clone())
                .collect()
        };
        assert_eq!(names(Board::Cm4, CamPort::Cam0), ["dtoverlay=imx708,cam0"]);
        assert_eq!(names(Board::Cm4, CamPort::Cam1), Vec::<String>::new());
        assert_eq!(names(Board::Cm5, CamPort::Cam0), ["dtoverlay=imx290,cam0"]);
        assert_eq!(names(Board::Cm5, CamPort::Cam1), ["dtoverlay=imx290,cam1"]);
        // An unknown board keeps both: either may apply.
        assert_eq!(names(Board::Unknown, CamPort::Cam0).len(), 2);
    }

    #[test]
    fn rendering_always_writes_an_explicit_rotation() {
        let r = |overlay: &str, port, params: &[&str], rot| {
            render_overlay_line(
                overlay,
                port,
                &params.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                rot,
            )
            .unwrap()
        };
        assert_eq!(
            r("imx708", CamPort::Cam0, &[], 180),
            "dtoverlay=imx708,cam0,rotation=180"
        );
        assert_eq!(
            r("imx290", CamPort::Cam1, &[], 0),
            "dtoverlay=imx290,cam1,rotation=0"
        );
        assert_eq!(
            r(
                "imx290",
                CamPort::Cam0,
                &["clock-frequency=74250000", "4lane"],
                0
            ),
            "dtoverlay=imx290,cam0,clock-frequency=74250000,4lane,rotation=0"
        );
        // The catalog's worst case fits the firmware's 98 characters.
        for s in &catalog().sensors {
            let mut params = s.fixed_params.clone();
            for o in &s.options {
                let widest = o.values.iter().max_by_key(|v| v.to_string().len()).unwrap();
                params.push(s.option_token(&o.key, *widest).unwrap());
            }
            let l = render_overlay_line(&s.overlay, CamPort::Cam0, &params, 180).unwrap();
            assert!(l.len() <= 98, "{l}");
        }
    }

    /// "Change only the rotation token: every other token is preserved
    /// verbatim and in order."
    #[test]
    fn rotating_a_line_changes_only_its_rotation_token() {
        let w = |text: &str, rot| line_with_rotation(&line(text), rot).unwrap();
        assert_eq!(
            w("dtoverlay=imx290,cam0", 180),
            "dtoverlay=imx290,cam0,rotation=180"
        );
        assert_eq!(
            w("dtoverlay=imx290,cam0", 0),
            "dtoverlay=imx290,cam0,rotation=0"
        );
        assert_eq!(
            w("dtoverlay=imx708,cam0,rotation=180", 0),
            "dtoverlay=imx708,cam0,rotation=0"
        );
        // Replaced where it stands; unknown tokens and their order kept.
        assert_eq!(
            w(
                "dtoverlay=imx290,clock-frequency=74250000,rotation=0,cam0,4lane,media-controller=0,future-thing",
                180
            ),
            "dtoverlay=imx290,clock-frequency=74250000,rotation=180,cam0,4lane,media-controller=0,future-thing"
        );
        assert_eq!(
            w("dtoverlay=imx708,link-frequency=447000000,cam0", 0),
            "dtoverlay=imx708,link-frequency=447000000,cam0,rotation=0"
        );
        // A second token is dropped, a broken one replaced.
        assert_eq!(
            w("dtoverlay=imx290,rotation=0,cam0,rotation=180", 180),
            "dtoverlay=imx290,rotation=180,cam0"
        );
        assert_eq!(
            w("dtoverlay=imx290,cam0,rotation=90", 0),
            "dtoverlay=imx290,cam0,rotation=0"
        );
        // Whitespace around the tokens is not kept (the helper's form).
        assert_eq!(
            w("  dtoverlay=imx290 , cam0 ,rotation=180  ", 0),
            "dtoverlay=imx290,cam0,rotation=0"
        );
        // A token the helper would not write back is refused, not dropped.
        assert_eq!(
            line_with_rotation(&line("dtoverlay=imx290,cam0,x=a b"), 0),
            Err(RenderError::Token("x=a b".into()))
        );
        assert_eq!(
            line_with_rotation(&line("dtoverlay=IMX290,cam0"), 0),
            Err(RenderError::Token("IMX290".into()))
        );
        let long = format!(
            "dtoverlay=imx290,cam0,{}",
            ["abcdefgh=12345678"; 4].join(",")
        );
        assert_eq!(long.len(), 93);
        assert_eq!(
            line_with_rotation(&line(&long), 180),
            Err(RenderError::TooLong(long.len() + ",rotation=180".len()))
        );
    }

    #[test]
    fn state_serde_and_dt_helpers() {
        let s = r#"{"at":1,"cameras":[{"streamKey":"cam0","desiredDeg":180,"inForceDeg":0,"bootDeg":180,"verdict":"pending-reboot"},{"streamKey":"cam1","desiredDeg":0,"inForceDeg":null,"bootDeg":null,"verdict":"unknown-camera","detail":"this device has no camera \"cam1\""}],"rebootRequired":true,"wrote":false}"#;
        let v: CameraMountState = serde_json::from_str(s).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), s);
        assert_eq!(
            dt_rotation_node("/base/soc/i2c@0/imx290@1a"),
            Some("soc/i2c@0/imx290@1a".into())
        );
        assert_eq!(dt_rotation_node("/base/../../etc/passwd"), None);
        assert_eq!(dt_rotation_node(""), None);
        assert_eq!(dt_rotation_from_bytes(&[0, 0, 0, 180]), Some(180));
        assert_eq!(dt_rotation_from_bytes(&[0, 180]), None);
        assert_eq!(
            resolve_boot_config_path(BOOT_CONFIG_CANDIDATES, |p| p == "/boot/config.txt"),
            Some("/boot/config.txt")
        );
        assert_eq!(
            resolve_boot_config_path(BOOT_CONFIG_CANDIDATES, |_| false),
            None
        );
    }
}
