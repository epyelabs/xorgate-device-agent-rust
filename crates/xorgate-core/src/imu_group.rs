//! The pure half of the IMU collector — `src/agent/collectors/imu.ts` (138
//! lines): `mapLatest` (`:81-106`), the lifecycle decision when the helper
//! exits (`:68-78`) and every line the collector logs, verbatim (operators
//! grep `[imu] …`). Detection, the Python child and the per-tick
//! `settings.json` re-read are the agent's (`xorgate-agent::collectors::imu`).

use crate::imu::{ImuSample, IMU_ADDRESSES, IMU_I2C_BUS};
use crate::orientation::{apply_mount_correction, quat_to_euler, ImuMount};
use crate::round::round;
use crate::telemetry::{ImuGroup, MetricLeaf};

/// `mapLatest(sample)` (`imu.ts:81-106`) with `loadSettings().imuMount` read
/// for this tick. The mount correction rotates the DERIVED euler only; the
/// quaternion stays the sensor's raw output.
pub fn map_latest(sample: &ImuSample, mount: Option<&ImuMount>) -> ImuGroup {
    let quat = &sample.quat;
    let lin = &sample.linaccel;
    let euler = match mount {
        Some(m) if m.enabled => quat_to_euler(&apply_mount_correction(quat, m)),
        _ => quat_to_euler(quat),
    };
    ImuGroup {
        quat_r: MetricLeaf::new(round(quat.r, 4)),
        quat_i: MetricLeaf::new(round(quat.i, 4)),
        quat_j: MetricLeaf::new(round(quat.j, 4)),
        quat_k: MetricLeaf::new(round(quat.k, 4)),
        euler_roll: MetricLeaf::with_unit(round(euler.roll, 1), "°"),
        euler_pitch: MetricLeaf::with_unit(round(euler.pitch, 1), "°"),
        euler_yaw: MetricLeaf::with_unit(round(euler.yaw, 1), "°"),
        accel_x: MetricLeaf::with_unit(round(lin.x, 3), "m/s^2"),
        accel_y: MetricLeaf::with_unit(round(lin.y, 3), "m/s^2"),
        accel_z: MetricLeaf::with_unit(round(lin.z, 3), "m/s^2"),
    }
}

/// What the collector does when the helper's `done` resolves (`imu.ts:68-78`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderExit {
    /// It had produced samples: a transient drop — reset, re-detect next tick.
    Redetect,
    /// It never produced one: a setup problem — log once and stay inert.
    Park,
}

/// The decision of `imu.ts:69`.
pub fn on_reader_exit(ever_sampled: bool) -> ReaderExit {
    if ever_sampled {
        ReaderExit::Redetect
    } else {
        ReaderExit::Park
    }
}

/// `toHexAddr(addr)` (`lib/format.ts:38-40`): `0x` + lowercase hex, ≥ 2 digits.
pub fn to_hex_addr(addr: u32) -> String {
    format!("0x{addr:02x}")
}

/// `[imu] BNO085 not detected on I2C bus 1 (0x4a/0x4b)` (`imu.ts:49`).
pub fn not_detected_line() -> String {
    let addrs: Vec<String> = IMU_ADDRESSES.iter().map(|a| to_hex_addr(*a)).collect();
    // The TS literal is fixed text; built here from the same constants and
    // pinned by the test below.
    format!(
        "[imu] BNO085 not detected on I2C bus {IMU_I2C_BUS} ({})",
        addrs.join("/")
    )
}

/// `[imu] BNO085 detected at 0x4a — streaming` (`imu.ts:54`).
pub fn detected_line(addr: u32) -> String {
    format!("[imu] BNO085 detected at {} — streaming", to_hex_addr(addr))
}

/// `[imu] reader stream ended; will re-detect` (`imu.ts:70`).
pub const READER_ENDED: &str = "[imu] reader stream ended; will re-detect";

/// `[imu] reader exited before any sample (exit N); IMU disabled. Check: …`
/// (`imu.ts:73-76`); `exitCode ?? "?"` — `None` (signal / spawn failure) → `?`.
pub fn exited_before_sample_line(exit_code: Option<i32>) -> String {
    let code = exit_code.map_or_else(|| "?".to_string(), |c| c.to_string());
    format!(
        "[imu] reader exited before any sample (exit {code}); IMU disabled. \
         Check: i2cdetect -y 1, python3 + adafruit-circuitpython-bno08x, and python/bno085_read.py."
    )
}

/// `streamImuData`'s stderr filter (`hardware/imu.ts:163-166`): a trimmed,
/// non-empty stderr line is surfaced as an error, else dropped.
pub fn stderr_error(line: &str) -> Option<&str> {
    let t = crate::js::trim(line);
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_verbatim() {
        assert_eq!(
            not_detected_line(),
            "[imu] BNO085 not detected on I2C bus 1 (0x4a/0x4b)"
        );
        assert_eq!(
            detected_line(0x4a),
            "[imu] BNO085 detected at 0x4a — streaming"
        );
        assert_eq!(to_hex_addr(0x4b), "0x4b");
        assert_eq!(to_hex_addr(5), "0x05");
        assert_eq!(
            exited_before_sample_line(None),
            "[imu] reader exited before any sample (exit ?); IMU disabled. Check: i2cdetect -y 1, python3 + adafruit-circuitpython-bno08x, and python/bno085_read.py."
        );
        assert!(exited_before_sample_line(Some(2)).contains("(exit 2);"));
    }

    #[test]
    fn exit_decision() {
        assert_eq!(on_reader_exit(true), ReaderExit::Redetect);
        assert_eq!(on_reader_exit(false), ReaderExit::Park);
    }

    #[test]
    fn stderr_filter() {
        assert_eq!(stderr_error("  boom \r"), Some("boom"));
        assert_eq!(stderr_error(" \t"), None);
    }
}
