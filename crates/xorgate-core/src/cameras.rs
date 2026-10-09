//! Camera enumeration parsers. Port of the pure half of
//! `src/hardware/cameras.ts` (92 lines; cm4-support plan): the agent asks
//! libcamera what is attached and maps each identity path onto the carrier
//! connector (`csi0`/`csi1`) the hardware block names, per platform.
//!
//! Out of core: `enumerateCameras` (`cameras.ts:74-92`, runs
//! `rpicam-hello --list-cameras`) — the HAL runs the tool and feeds its
//! stdout to [`parse_camera_list`]; the "cannot enumerate vs zero cameras"
//! rule (`:81-90`) is ported here as [`enumeration_recognizable`].
//!
//! Platform → connector mapping (bench-verified identities, `cameras.ts:9-15`):
//! - rpi-cm5 (RP1): `/i2c@88000/` = csi0 (CAM0), `/i2c@70000/` = csi1 (CAM1).
//!   NOTE the i2c addresses do NOT sort numerically in connector order.
//! - rpi-cm4 (Unicam): `/i2c@0/` = csi0, `/i2c@1/` = csi1.

use serde::{Deserialize, Serialize};

use crate::hardware_block::Platform;

/// `EnumeratedCamera` (`cameras.ts:19-26`). `connector` serialises as
/// `null` when unknown — the TS object always carries the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnumeratedCamera {
    /// libcamera identity path, e.g. `/base/soc/i2c0mux/i2c@0/imx290@1a`.
    pub camera_name: String,
    /// Sensor model libcamera reports (`imx290`).
    pub sensor: String,
    /// Carrier connector this path maps to (`csi0`/`csi1`), or `None` when unknown.
    pub connector: Option<String>,
}

/// `CM5_CONNECTORS` (`cameras.ts:28-31`), in lookup order.
const CM5_CONNECTORS: &[(&str, &str)] = &[("/i2c@88000/", "csi0"), ("/i2c@70000/", "csi1")];
/// `CM4_CONNECTORS` (`cameras.ts:33-36`).
const CM4_CONNECTORS: &[(&str, &str)] = &[("/i2c@0/", "csi0"), ("/i2c@1/", "csi1")];

/// `connectorForPath` (`cameras.ts:38-44`): first table entry whose needle
/// the path contains. Every platform other than `rpi-cm5` uses the CM4 table
/// (the TS `platform === "rpi-cm5" ? CM5 : CM4`).
pub fn connector_for_path(platform: Platform, camera_name: &str) -> Option<&'static str> {
    let table = match platform {
        Platform::RpiCm5 => CM5_CONNECTORS,
        Platform::RpiCm4 => CM4_CONNECTORS,
    };
    table
        .iter()
        .find(|(needle, _)| camera_name.contains(needle))
        .map(|(_, connector)| *connector)
}

/// JS `\s` (ECMAScript WhiteSpace + LineTerminator), which differs from
/// `char::is_whitespace` at U+0085 (JS: no) and U+FEFF (JS: yes).
pub(crate) fn js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Hand-rolled equivalent of the TS line regex
/// `/^\s*\d+\s*:\s*(\S+)\s.*\((\S+)\)\s*$/` (`cameras.ts:54`), returning
/// `(sensor, cameraName)`. `\d` is ASCII-only in JS; `\s` is [`js_ws`].
///
/// Backtracking semantics reproduced: `(\S+)\s` forces the sensor to be the
/// whole first token after the colon followed by at least one whitespace
/// char; the greedy `.*` picks the RIGHTMOST `(` such that everything from
/// it to the final `)` (after trailing whitespace) is non-whitespace and
/// non-empty.
fn match_camera_line(line: &str) -> Option<(&str, &str)> {
    let s = line.trim_start_matches(js_ws);
    let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let s = s[digits..].trim_start_matches(js_ws);
    let s = s.strip_prefix(':')?;
    let s = s.trim_start_matches(js_ws);
    let sensor_len = s.len() - s.trim_start_matches(|c: char| !js_ws(c)).len();
    if sensor_len == 0 {
        return None;
    }
    let sensor = &s[..sensor_len];
    let mut rest = s[sensor_len..].chars();
    // The mandatory single `\s` after the sensor token.
    if !rest.next().is_some_and(js_ws) {
        return None;
    }
    let rest = rest.as_str();
    // `\((\S+)\)\s*$`: strip trailing whitespace, require a final `)`.
    let body = rest.trim_end_matches(js_ws).strip_suffix(')')?;
    // Rightmost `(` whose tail is a non-empty run of non-whitespace.
    let open = body.rfind('(')?;
    let name = &body[open + 1..];
    if name.is_empty() || name.chars().any(js_ws) {
        return None;
    }
    Some((sensor, name))
}

/// `parseCameraList` (`cameras.ts:51-61`): parse `rpicam-hello --list-cameras`
/// output. Camera lines look like
/// `0 : imx290 [1920x1080 12-bit RGGB] (/base/soc/i2c0mux/i2c@0/imx290@1a)`;
/// indented mode-detail lines are ignored (they carry no `N :` prefix).
pub fn parse_camera_list(output: &str, platform: Platform) -> Vec<EnumeratedCamera> {
    output
        .split('\n')
        .filter_map(match_camera_line)
        .map(|(sensor, camera_name)| EnumeratedCamera {
            camera_name: camera_name.to_string(),
            sensor: sensor.to_string(),
            connector: connector_for_path(platform, camera_name).map(str::to_string),
        })
        .collect()
}

/// The decision inside `enumerateCameras` (`cameras.ts:81-90`) when the
/// listing parsed to ZERO cameras: only an error WITHOUT any recognizable
/// listing output means "cannot enumerate" (broken libcamera stack); a
/// fresh install prints `No cameras available!` and exits non-zero, which is
/// a real empty result. `text` is `stdout + stderr` as the TS concatenates them.
pub fn enumeration_recognizable(text: &str) -> bool {
    text.contains("Available cameras") || text.to_lowercase().contains("no cameras available")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_regex_edge_cases() {
        assert_eq!(
            match_camera_line(
                "0 : imx290 [1920x1080 12-bit RGGB] (/base/soc/i2c0mux/i2c@0/imx290@1a)"
            ),
            Some(("imx290", "/base/soc/i2c0mux/i2c@0/imx290@1a"))
        );
        // Indented mode lines have no leading `N :`.
        assert_eq!(
            match_camera_line("    Modes: 'SRGGB10_CSI2P' : 1280x720 [30.00 fps]"),
            None
        );
        assert_eq!(match_camera_line("Available cameras"), None);
        // Sensor token must be followed by whitespace; name = rightmost `(`.
        assert_eq!(match_camera_line("1 : foo(bar)"), None);
        assert_eq!(match_camera_line("1 : foo (x) (a(b))"), Some(("foo", "b)")));
        assert_eq!(match_camera_line("1 : foo ()"), None);
        assert_eq!(match_camera_line("1 : foo (a b)"), None);
        assert_eq!(
            match_camera_line("  7:imx708 x (/p)\r"),
            Some(("imx708", "/p"))
        );
    }

    #[test]
    fn zero_camera_recognition() {
        assert!(enumeration_recognizable("No cameras available!"));
        assert!(enumeration_recognizable("Available cameras\n-----\n"));
        assert!(!enumeration_recognizable("libcamera: broken"));
    }

    #[test]
    fn connector_serialises_null_when_unknown() {
        let c = EnumeratedCamera {
            camera_name: "/weird".into(),
            sensor: "x".into(),
            connector: None,
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"cameraName":"/weird","sensor":"x","connector":null}"#
        );
    }
}
