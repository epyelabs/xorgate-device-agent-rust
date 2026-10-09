//! Device hardware block (cm4-support plan): the projection of this device's
//! model that the cloud tells it about — platform, encoder token,
//! connector/sensor list, LED pin map, rails, IMU part. Port of the pure part
//! of `src/hardware/hardware-config.ts` (`PLATFORMS`, `ENCODERS`, the types,
//! `defaultEncoderForPlatform:62`, `parseHardwareBlock:75`,
//! `hardwareBlocksEqual:159`). `loadHardwareBlock` / `saveHardwareBlock`
//! (`:129-156`, file I/O) stay in the HAL.
//!
//! ABSENT or INVALID block = legacy behavior (the hardcoded CM5 wiring). An
//! unknown enum is a validation FAILURE, not a guess.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::telemetry::{js_num, js_num_opt};

/// `PLATFORMS` (`hardware-config.ts:23`).
pub const PLATFORMS: &[&str] = &["rpi-cm5", "rpi-cm4"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    RpiCm5,
    RpiCm4,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::RpiCm5 => "rpi-cm5",
            Platform::RpiCm4 => "rpi-cm4",
        }
    }
    /// `isPlatform` (`hardware-config.ts:53`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "rpi-cm5" => Some(Platform::RpiCm5),
            "rpi-cm4" => Some(Platform::RpiCm4),
            _ => None,
        }
    }
}

/// `ENCODERS` (`hardware-config.ts:26`).
pub const ENCODERS: &[&str] = &["sw-h264", "hw-h264"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EncoderToken {
    SwH264,
    HwH264,
}

impl EncoderToken {
    pub fn as_str(self) -> &'static str {
        match self {
            EncoderToken::SwH264 => "sw-h264",
            EncoderToken::HwH264 => "hw-h264",
        }
    }
    /// `isEncoder` (`hardware-config.ts:57`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sw-h264" => Some(EncoderToken::SwH264),
            "hw-h264" => Some(EncoderToken::HwH264),
            _ => None,
        }
    }
}

/// `HardwareVideo` (`hardware-config.ts:29`). Key order = the parser's
/// object literal (`:101-110`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareVideo {
    pub key: String,
    /// Carrier connector (csi0, csi1) — NOT a libcamera path.
    pub connector: String,
    /// Sensor part on that connector (imx290).
    pub sensor: String,
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub max_width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub max_height: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "js_num_opt")]
    pub max_fps: Option<f64>,
}

/// `statusLed` pin map.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StatusLedPins {
    #[serde(with = "js_num")]
    pub r: f64,
    #[serde(with = "js_num")]
    pub g: f64,
    #[serde(with = "js_num")]
    pub b: f64,
}

/// `DeviceHardwareBlock` (`hardware-config.ts:42`). `statusLed` and `imu` are
/// written as explicit `null` (the TS type is `X | null`, never optional).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceHardwareBlock {
    pub v: u8,
    pub platform: Platform,
    pub encoder: EncoderToken,
    pub rails: bool,
    pub status_led: Option<StatusLedPins>,
    /// IMU part number, or null when the model declares no IMU.
    pub imu: Option<String>,
    pub video: Vec<HardwareVideo>,
}

/// `defaultEncoderForPlatform` (`hardware-config.ts:62`): the CM5 has no
/// hardware H.264 encoder — software is a SoC fact there.
pub fn default_encoder_for_platform(platform: Platform) -> EncoderToken {
    match platform {
        Platform::RpiCm4 => EncoderToken::HwH264,
        Platform::RpiCm5 => EncoderToken::SwH264,
    }
}

/// `pin` (`hardware-config.ts:66`): a non-negative integer JSON number.
fn pin(v: Option<&Value>) -> Option<f64> {
    let n = v?.as_f64()?;
    (n.fract() == 0.0 && n >= 0.0 && n.is_finite()).then_some(n)
}

/// `parseHardwareBlock` (`hardware-config.ts:75`): None (and the caller logs
/// and falls back to legacy behavior) on ANY unrecognized enum or malformed
/// required field — tolerant only about truly optional extras.
pub fn parse_hardware_block(obj: &Value) -> Option<DeviceHardwareBlock> {
    let Value::Object(o) = obj else {
        return None;
    };
    if o.get("v").and_then(Value::as_f64) != Some(2.0) {
        return None;
    }
    let platform = Platform::parse(o.get("platform")?.as_str()?)?;
    let encoder = match o.get("encoder") {
        None => default_encoder_for_platform(platform),
        Some(e) => EncoderToken::parse(e.as_str()?)?,
    };

    let status_led = match o.get("statusLed") {
        None | Some(Value::Null) => None,
        Some(led) => {
            // A non-object (`{} as Record`) reads every pin as undefined → null → reject.
            let m = led.as_object();
            let get = |k: &str| m.and_then(|m| m.get(k));
            Some(StatusLedPins {
                r: pin(get("r"))?,
                g: pin(get("g"))?,
                b: pin(get("b"))?,
            })
        }
    };

    let video_in: &[Value] = match o.get("video") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    let mut video = Vec::with_capacity(video_in.len());
    for entry in video_in {
        // `!entry || typeof entry !== "object"` — arrays pass but have no
        // string `key`, so they reject on the next line.
        let v = match entry {
            Value::Object(m) => m,
            _ => return None,
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
        let n = |k: &str| v.get(k).and_then(Value::as_f64);
        let key = s("key")?;
        let connector = s("connector")?;
        let sensor = s("sensor")?;
        video.push(HardwareVideo {
            key: key.to_string(),
            connector: connector.to_string(),
            sensor: sensor.to_string(),
            codec: v
                .get("codec")
                .and_then(Value::as_str)
                .unwrap_or("h264")
                .to_string(),
            label: v.get("label").and_then(Value::as_str).map(str::to_string),
            max_width: n("maxWidth"),
            max_height: n("maxHeight"),
            max_fps: n("maxFps"),
        });
    }

    Some(DeviceHardwareBlock {
        v: 2,
        platform,
        encoder,
        rails: o.get("rails") == Some(&Value::Bool(true)),
        status_led,
        imu: o
            .get("imu")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        video,
    })
}

/// `hardwareBlocksEqual` (`hardware-config.ts:159`): structural equality
/// (`JSON.stringify(a) === JSON.stringify(b)`) — gates the persist + re-render
/// on cloud redelivery.
pub fn hardware_blocks_equal(
    a: Option<&DeviceHardwareBlock>,
    b: Option<&DeviceHardwareBlock>,
) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_and_pins() {
        assert_eq!(Platform::parse("jetson"), None);
        assert_eq!(EncoderToken::parse("av1"), None);
        assert_eq!(
            default_encoder_for_platform(Platform::RpiCm5),
            EncoderToken::SwH264
        );
        assert_eq!(pin(Some(&Value::from(12))), Some(12.0));
        assert_eq!(pin(Some(&Value::from(-1))), None);
        assert_eq!(pin(Some(&Value::from(1.5))), None);
        assert_eq!(pin(Some(&Value::from("1"))), None);
        assert_eq!(pin(None), None);
        assert_eq!(PLATFORMS.len(), 2);
        assert_eq!(ENCODERS.len(), 2);
    }
}
