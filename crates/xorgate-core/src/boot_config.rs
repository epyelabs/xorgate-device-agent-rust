//! The boot config (`/boot/firmware/config.txt`) as the agent is allowed to
//! read it: ONE parameter, `rotation`, on ONE kind of line, the camera
//! `dtoverlay=`. Port of the pure half of `src/hardware/boot-config.ts`
//! (cam-sensor-level-rotate plan): the overlay-line parsers (`:63-151`), the
//! device-tree node derivation + big-endian decode of `dtRotationForCamera`
//! (`:166-179`, the `readFileSync` injected), `resolveBootConfigPath` (`:54`,
//! `existsSync` injected), `resolveCamToken` (`:304`) and the reconciler's
//! verdict/state types (`:219-253`).
//!
//! Left in the HAL/agent (Phase 4): `cameraNameFor` (`:182`, reads the
//! pipeline dir), `probeOverlayRotation` (`:206`, spawns `dtoverlay -h`),
//! `applyRotationViaHelper` (`:280`, `sudo -n xorgate-camrotate`) and
//! `reconcileCameraMount` (`:329-457`), whose control flow interleaves those
//! three I/O seams per camera.
//!
//! THE SAFETY INVARIANT carries over: nothing here can reboot the device.

use serde::{Deserialize, Serialize};

use crate::hardware_block::DeviceHardwareBlock;
use crate::js;
use crate::telemetry::js_num_opt;

/// `BOOT_CONFIG_CANDIDATES` (`boot-config.ts:46`): Bookworm first, then the
/// pre-Bookworm path.
pub const BOOT_CONFIG_CANDIDATES: &[&str] = &["/boot/firmware/config.txt", "/boot/config.txt"];

/// `PROC_DEVICE_TREE` (`boot-config.ts:49`): the root of the LIVE device tree.
pub const PROC_DEVICE_TREE: &str = "/proc/device-tree";

/// `CAMROTATE_HELPER` (`boot-config.ts:52`): the privileged writer.
pub const CAMROTATE_HELPER: &str = "/usr/local/bin/xorgate-camrotate";

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

/// `rotationOfLine` (`boot-config.ts:118`): the rotation a line declares —
/// the value of its `rotation=` token, or 0 when it has none (the overlay's
/// own default). Anything unparseable is None so the caller rewrites the line
/// rather than trusting it.
pub fn rotation_of_line(line: &OverlayLine) -> Option<f64> {
    let Some(token) = line.params.iter().find(|p| p.starts_with("rotation=")) else {
        return Some(0.0);
    };
    let value = js::number(&token["rotation=".len()..]);
    (value == 0.0 || value == 180.0).then_some(value)
}

/// `overlayLinesForCamera` (`boot-config.ts:126`): the `dtoverlay=` lines that
/// configure one camera connector.
pub fn overlay_lines_for_camera<'a>(
    lines: &'a [OverlayLine],
    cam_token: &str,
) -> Vec<&'a OverlayLine> {
    lines
        .iter()
        .filter(|l| l.params.iter().any(|p| p == cam_token))
        .collect()
}

/// `BootConfigCameraView` (`boot-config.ts:130`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BootConfigCameraView {
    /// The overlay (sensor) name the boot config uses for this camera.
    pub sensor: Option<String>,
    /// Rotation in the boot config; null when there is no line, or lines disagree.
    #[serde(with = "js_num_opt")]
    pub rotation: Option<f64>,
    /// How many `dtoverlay=` lines mention this camera.
    pub lines: usize,
}

/// `bootConfigCamera` (`boot-config.ts:144`): what the boot config says about
/// one camera. Disagreeing duplicate lines report `rotation: None`, which the
/// reconciler treats as "needs a write".
pub fn boot_config_camera(text: &str, cam_token: &str) -> BootConfigCameraView {
    let lines = parse_overlay_lines(text);
    let matches = overlay_lines_for_camera(&lines, cam_token);
    if matches.is_empty() {
        return BootConfigCameraView {
            sensor: None,
            rotation: None,
            lines: 0,
        };
    }
    let rotations: Vec<Option<f64>> = matches.iter().map(|l| rotation_of_line(l)).collect();
    let first = rotations[0];
    let agreed = if rotations.iter().all(|r| r.is_some() && *r == first) {
        first
    } else {
        None
    };
    BootConfigCameraView {
        sensor: Some(matches[0].name.clone()),
        rotation: agreed,
        lines: matches.len(),
    }
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

/// `system/xorgate-camrotate:34` — its usage line (stderr, exit 2).
pub const CAMROTATE_USAGE: &str = "usage: xorgate-camrotate <sensor> <camN> <0|180>";

/// The ONLY mint of a `xorgate-camrotate` [`crate::commands::HelperCall`]
/// (I5): `applyRotationViaHelper(sensor, camToken, deg)`'s argv
/// (`boot-config.ts:285`) with the helper's own argument checks
/// (`system/xorgate-camrotate:39-50`) applied first:
/// sensor `'' | *[!a-z0-9_-]*` → usage; cam must be `cam[0-9]`; deg 0|180
/// (the type already guarantees it).
///
/// `Err` = [`CAMROTATE_USAGE`], the helper's stderr for those arguments, so
/// the reconciler's cloud-visible `could not edit <path>: <detail> …` reads
/// exactly as when the TS let the helper refuse them (D33; an overlay name such as
/// `IMX290` or a connector `csi10` → `cam10`). The sudo call is skipped.
pub fn camrotate_call(
    sensor: &str,
    cam_token: &str,
    deg: crate::types::CameraRotationDeg,
) -> Result<crate::commands::HelperCall, String> {
    let sensor_ok = !sensor.is_empty()
        && sensor
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    let cam = cam_token.as_bytes();
    let cam_ok = cam.len() == 4 && cam.starts_with(b"cam") && cam[3].is_ascii_digit();
    if !sensor_ok || !cam_ok {
        return Err(CAMROTATE_USAGE.to_string());
    }
    Ok(crate::commands::HelperCall::mint(
        crate::commands::HelperKind::CamRotate {
            sensor: sensor.to_string(),
            cam_token: cam_token.to_string(),
            deg: deg.degrees() as u16,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camrotate_call_validates_like_the_helper() {
        use crate::types::CameraRotationDeg::{Deg0, Deg180};
        let c = camrotate_call("imx290", "cam0", Deg180).unwrap();
        assert_eq!(
            c.argv(),
            [
                "sudo",
                "-n",
                "/usr/local/bin/xorgate-camrotate",
                "imx290",
                "cam0",
                "180"
            ]
        );
        assert_eq!(c.timeout_ms(), Some(20_000));
        assert_eq!(
            camrotate_call("ov5647_x-1", "cam9", Deg0)
                .unwrap()
                .helper_args()[2],
            "0"
        );
        for (s, cam) in [
            ("", "cam0"),
            ("IMX290", "cam0"),
            ("../../etc/passwd", "cam0"),
            ("imx 290", "cam0"),
            ("imx290;reboot", "cam0"),
            ("imx290", "cam10"),
            ("imx290", "cam"),
            ("imx290", "csi0"),
            ("imx290", "camx"),
            ("imx290", ""),
        ] {
            assert_eq!(
                camrotate_call(s, cam, Deg180).unwrap_err(),
                CAMROTATE_USAGE,
                "{s:?} {cam:?}"
            );
        }
    }

    #[test]
    fn js_number_subset() {
        // `rotation=` goes through the shared `Number(string)` (crate::js).
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

    #[test]
    fn rotation_token_edge_cases() {
        let line = |p: &[&str]| OverlayLine {
            index: 0,
            name: "imx290".into(),
            params: p.iter().map(|s| s.to_string()).collect(),
            text: String::new(),
        };
        assert_eq!(rotation_of_line(&line(&["cam0"])), Some(0.0));
        assert_eq!(rotation_of_line(&line(&["cam0", "rotation="])), Some(0.0));
        assert_eq!(rotation_of_line(&line(&["rotation=180"])), Some(180.0));
        assert_eq!(rotation_of_line(&line(&["rotation=90"])), None);
        assert_eq!(rotation_of_line(&line(&["rotation=x"])), None);
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
