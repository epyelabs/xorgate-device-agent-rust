//! IMU (BNO085 over I2C) — the pure parts of `src/hardware/imu.ts` (204
//! lines): the helper's NDJSON line protocol (`parseImuLine:52`), the
//! `i2cdetect -y N` grid parser (`parseI2cDetect:82`), the candidate-address
//! pick (`findImuAddress:67`) and the detection-result builders
//! (`buildDetect:98`, `unavailableDetect:111`). `detectImu:123` (spawns
//! `i2cdetect`), `streamImuData:150` (spawns the Python helper) and
//! `mockStream:180` are HAL / agent.
//!
//! Invariant 11 (the IMU fields go together) starts here: an
//! [`ImuMessage::Sample`] exists only with all seven numbers present; a
//! sample line missing any of them parses as `None` (the TS cast the parsed
//! object without checking — see the evidence file's ambiguity list).

use serde::{Deserialize, Serialize};

use crate::js;
pub use crate::orientation::Quat;
use crate::telemetry::js_num;

/// `IMU.i2cBus` (`imu.ts:18`): BNO085 on I2C1, sharing the bus with the BMS.
pub const IMU_I2C_BUS: u32 = 1;
/// `IMU.addresses` (`imu.ts:20`): 7-bit address, strap-selectable 0x4A (default) or 0x4B.
pub const IMU_ADDRESSES: [u32; 2] = [0x4a, 0x4b];
/// `IMU.bmsAddress` (`imu.ts:22`): battery charger / BMS on the same bus — reported, not used.
pub const IMU_BMS_ADDRESS: u32 = 0x6b;

/// `ImuDetect` (`imu.ts:25-36`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImuDetect {
    pub bus: u32,
    pub addresses: Vec<u32>,
    /// The BNO085 address actually found (0x4A or 0x4B), or null if absent.
    pub imu_address: Option<u32>,
    pub imu_present: bool,
    pub bms_present: bool,
    /// False when i2cdetect itself is missing/failed (a tooling problem, not "no sensor").
    pub available: bool,
    /// Human-readable reason when `available` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `ImuSample.linaccel` (`imu.ts:42`): linear acceleration, gravity removed, m/s².
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinAccel {
    #[serde(with = "js_num")]
    pub x: f64,
    #[serde(with = "js_num")]
    pub y: f64,
    #[serde(with = "js_num")]
    pub z: f64,
}

/// `ImuSample` (`imu.ts:38-43`): one helper sample, all fields together.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImuSample {
    /// Unit rotation quaternion (r = real/scalar).
    pub quat: Quat,
    pub linaccel: LinAccel,
}

/// `ImuMessage` (`imu.ts:46-49`): a line emitted by `python/bno085_read.py`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ImuMessage {
    Ready {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
        /// The main firmware's identity (agent 0.2.2 readers; D129).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        part: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<u64>,
    },
    Sample {
        quat: Quat,
        linaccel: LinAccel,
    },
    Error {
        error: String,
    },
}

/// `parseImuLine` (`imu.ts:52-64`): one JSON line → a typed message, or
/// `None` for a partial / non-JSON line or an unknown `type`.
pub fn parse_imu_line(line: &str) -> Option<ImuMessage> {
    let t = js::trim(line);
    if !t.starts_with('{') {
        return None;
    }
    serde_json::from_str::<ImuMessage>(t).ok()
}

/// `findImuAddress` (`imu.ts:67-72`): first candidate present on the bus.
pub fn find_imu_address(present: &[u32], candidates: &[u32]) -> Option<u32> {
    candidates
        .iter()
        .copied()
        .find(|addr| present.contains(addr))
}

/// `parseI2cDetect` (`imu.ts:82-96`): the fixed-width grid, 3 chars per
/// column after the `NN:` row label; `--` absent, `UU` claimed by a kernel
/// driver (counts as present), blank reserved, else the hex address. The
/// address is rowBase + columnIndex.
pub fn parse_i2c_detect(stdout: &str) -> Vec<u32> {
    let mut addresses = Vec::new();
    for line in stdout.split('\n') {
        // /^([0-9a-f]{2}):(.*)$/i — `.` stops at a line terminator, so a row
        // with a stray \r (CRLF output) does not match at all.
        let chars: Vec<char> = line.chars().collect();
        if chars.len() < 3
            || !chars[0].is_ascii_hexdigit()
            || !chars[1].is_ascii_hexdigit()
            || chars[2] != ':'
        {
            continue;
        }
        let rest = &chars[3..];
        if rest
            .iter()
            .any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
        {
            continue;
        }
        let base = js::parse_int(&chars[..2].iter().collect::<String>(), 16) as u32;
        for col in 0..16u32 {
            let start = (col * 3) as usize;
            let end = (start + 3).min(rest.len());
            if start >= rest.len() {
                break;
            }
            let cell: String = rest[start..end].iter().collect();
            let cell = js::trim(&cell);
            if cell.is_empty() || cell == "--" {
                continue;
            }
            addresses.push(base + col);
        }
    }
    addresses
}

/// `buildDetect` (`imu.ts:98-108`).
pub fn build_detect(addresses: Vec<u32>) -> ImuDetect {
    let imu_address = find_imu_address(&addresses, &IMU_ADDRESSES);
    ImuDetect {
        bus: IMU_I2C_BUS,
        bms_present: addresses.contains(&IMU_BMS_ADDRESS),
        addresses,
        imu_address,
        imu_present: imu_address.is_some(),
        available: true,
        reason: None,
    }
}

/// `unavailableDetect` (`imu.ts:111-121`): i2cdetect missing/failed (tooling, not sensor).
pub fn unavailable_detect(reason: &str) -> ImuDetect {
    ImuDetect {
        bus: IMU_I2C_BUS,
        addresses: Vec::new(),
        imu_address: None,
        imu_present: false,
        bms_present: false,
        available: false,
        reason: Some(reason.to_string()),
    }
}

/// `detectImu`'s tooling-missing wording (`imu.ts:131`).
pub const I2CDETECT_NOT_FOUND: &str = "i2cdetect not found — install i2c-tools.";
/// `streamImuData`'s interpreter-missing wording (`imu.ts:172`).
pub const PYTHON3_NOT_FOUND: &str =
    "python3 not found — install Python 3 and adafruit-circuitpython-bno08x.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_sample_is_rejected() {
        // TS: `obj as ImuMessage` with quat undefined; Rust refuses (invariant 11).
        assert_eq!(
            parse_imu_line(r#"{"type":"sample","quat":{"r":1,"i":0,"j":0,"k":0}}"#),
            None
        );
        assert_eq!(parse_imu_line(r#"{"type":"error"}"#), None);
        assert!(matches!(
            parse_imu_line(r#"{"type":"ready","address":"0x4b","extra":1}"#),
            Some(ImuMessage::Ready { .. })
        ));
        // Agent 0.2.2's readers add the main firmware's identity (D129).
        assert_eq!(
            parse_imu_line(
                r#"{"type": "ready", "address": "0x4a", "part": 10004563, "version": "3.12.6", "build": 62}"#
            ),
            Some(ImuMessage::Ready {
                address: Some("0x4a".into()),
                part: Some(10_004_563),
                version: Some("3.12.6".into()),
                build: Some(62),
            })
        );
    }

    #[test]
    fn detect_shapes() {
        let d = build_detect(vec![0x4b, 0x6b]);
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"bus":1,"addresses":[75,107],"imuAddress":75,"imuPresent":true,"bmsPresent":true,"available":true}"#
        );
        let u = unavailable_detect(I2CDETECT_NOT_FOUND);
        assert_eq!(
            serde_json::to_string(&u).unwrap(),
            r#"{"bus":1,"addresses":[],"imuAddress":null,"imuPresent":false,"bmsPresent":false,"available":false,"reason":"i2cdetect not found — install i2c-tools."}"#
        );
    }
}
