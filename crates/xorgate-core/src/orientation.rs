//! Quaternion math for the IMU collector: euler conversion + mount correction.
//! Port of `src/lib/orientation.ts` (110 lines, fully pure). Same operation
//! order as the TS so the f64 results agree to the last ulp wherever the libm
//! implementations do (atan2/asin/sin/cos may differ by an ulp between V8 and
//! glibc; the fixture test compares at 1e-9 and at the vitest `toBeCloseTo`
//! digits).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::telemetry::js_num;

/// `Euler` (`orientation.ts:3`): Tait-Bryan ZYX angles in degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Euler {
    /// Rotation about X, degrees.
    #[serde(with = "js_num")]
    pub roll: f64,
    /// Rotation about Y, degrees.
    #[serde(with = "js_num")]
    pub pitch: f64,
    /// Rotation about Z, degrees.
    #[serde(with = "js_num")]
    pub yaw: f64,
}

/// `Quat` (`orientation.ts:12`): r = scalar part.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quat {
    #[serde(with = "js_num")]
    pub r: f64,
    #[serde(with = "js_num")]
    pub i: f64,
    #[serde(with = "js_num")]
    pub j: f64,
    #[serde(with = "js_num")]
    pub k: f64,
}

/// `ImuMount` (`orientation.ts:27`): how the IMU is physically mounted
/// relative to the vehicle, as ZYX euler degrees. Cloud-managed per device
/// (settings.json `imuMount`). Key order = the TS object literal in
/// `parseImuMount` (`:46-51`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImuMount {
    pub enabled: bool,
    #[serde(with = "js_num")]
    pub roll_deg: f64,
    #[serde(with = "js_num")]
    pub pitch_deg: f64,
    #[serde(with = "js_num")]
    pub yaw_deg: f64,
}

/// ZYX euler degrees input shape shared by [`euler_to_quat`] callers
/// (`{ rollDeg, pitchDeg, yawDeg }` in the TS signature).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EulerDeg {
    #[serde(with = "js_num")]
    pub roll_deg: f64,
    #[serde(with = "js_num")]
    pub pitch_deg: f64,
    #[serde(with = "js_num")]
    pub yaw_deg: f64,
}

impl From<ImuMount> for EulerDeg {
    fn from(m: ImuMount) -> Self {
        EulerDeg {
            roll_deg: m.roll_deg,
            pitch_deg: m.pitch_deg,
            yaw_deg: m.yaw_deg,
        }
    }
}

/// `parseImuMount` (`orientation.ts:40`): validate an unknown value as an
/// ImuMount. Angles must be finite numbers (every JSON number is); anything
/// else rejects the whole object. Shared by the settings loader and the
/// cloud-config parser so both sides accept exactly the same shape.
pub fn parse_imu_mount(v: &Value) -> Option<ImuMount> {
    // `!v || typeof v !== "object"` — arrays pass the TS check but have no
    // `enabled` property, so they reject on the next line either way.
    let o = v.as_object()?;
    let enabled = o.get("enabled")?.as_bool()?;
    let angle = |k: &str| o.get(k).and_then(Value::as_f64).filter(|a| a.is_finite());
    let roll_deg = angle("rollDeg")?;
    let pitch_deg = angle("pitchDeg")?;
    let yaw_deg = angle("yawDeg")?;
    Some(ImuMount {
        enabled,
        roll_deg,
        pitch_deg,
        yaw_deg,
    })
}

/// `quatToEuler` (`orientation.ts:59`): unit quaternion → Tait-Bryan ZYX
/// euler degrees. Pitch's asin argument is clamped to [-1, 1] exactly like
/// the TS `Math.max(-1, Math.min(1, …))`.
pub fn quat_to_euler(q: &Quat) -> Euler {
    let Quat { r, i, j, k } = *q;
    let to_deg = 180.0 / std::f64::consts::PI;

    let roll = (2.0 * (r * i + j * k)).atan2(1.0 - 2.0 * (i * i + j * j));
    let sin_pitch = js_max(-1.0, js_min(1.0, 2.0 * (r * j - k * i)));
    let pitch = sin_pitch.asin();
    let yaw = (2.0 * (r * k + i * j)).atan2(1.0 - 2.0 * (j * j + k * k));

    Euler {
        roll: roll * to_deg,
        pitch: pitch * to_deg,
        yaw: yaw * to_deg,
    }
}

/// `eulerToQuat` (`orientation.ts:72`): ZYX euler degrees → unit quaternion,
/// the exact inverse of [`quat_to_euler`]. `Rz(yaw)·Ry(pitch)·Rx(roll)`.
pub fn euler_to_quat(e: &EulerDeg) -> Quat {
    let to_rad = std::f64::consts::PI / 360.0; // half-angles, so deg → rad/2
    let (cr, sr) = ((e.roll_deg * to_rad).cos(), (e.roll_deg * to_rad).sin());
    let (cp, sp) = ((e.pitch_deg * to_rad).cos(), (e.pitch_deg * to_rad).sin());
    let (cy, sy) = ((e.yaw_deg * to_rad).cos(), (e.yaw_deg * to_rad).sin());
    Quat {
        r: cy * cp * cr + sy * sp * sr,
        i: cy * cp * sr - sy * sp * cr,
        j: cy * sp * cr + sy * cp * sr,
        k: sy * cp * cr - cy * sp * sr,
    }
}

/// `quatMultiply` (`orientation.ts:87`): Hamilton product a⊗b.
pub fn quat_multiply(a: &Quat, b: &Quat) -> Quat {
    Quat {
        r: a.r * b.r - a.i * b.i - a.j * b.j - a.k * b.k,
        i: a.r * b.i + a.i * b.r + a.j * b.k - a.k * b.j,
        j: a.r * b.j - a.i * b.k + a.j * b.r + a.k * b.i,
        k: a.r * b.k + a.i * b.j - a.j * b.i + a.k * b.r,
    }
}

/// `quatConjugate` (`orientation.ts:96`).
pub fn quat_conjugate(q: &Quat) -> Quat {
    Quat {
        r: q.r,
        i: -q.i,
        j: -q.j,
        k: -q.k,
    }
}

/// `applyMountCorrection` (`orientation.ts:108`): rotate a sensor-frame
/// orientation into the vehicle frame, `q_wv = q_ws ⊗ q_vs⁻¹`. Purely a
/// frame change — the sensor's factory calibration is untouched.
pub fn apply_mount_correction(q_sensor: &Quat, mount: &ImuMount) -> Quat {
    quat_multiply(
        q_sensor,
        &quat_conjugate(&euler_to_quat(&EulerDeg::from(*mount))),
    )
}

/// `Math.min(a, b)` — NaN-propagating like JS (Rust's `f64::min` drops NaN).
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a < b {
        a
    } else {
        b
    }
}

/// `Math.max(a, b)` — NaN-propagating like JS.
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_level_and_mount_serde_order() {
        let e = quat_to_euler(&Quat {
            r: 1.0,
            i: 0.0,
            j: 0.0,
            k: 0.0,
        });
        assert_eq!((e.roll, e.pitch, e.yaw), (0.0, 0.0, 0.0));
        let m = ImuMount {
            enabled: true,
            roll_deg: 90.0,
            pitch_deg: 0.0,
            yaw_deg: -180.0,
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"enabled":true,"rollDeg":90,"pitchDeg":0,"yawDeg":-180}"#
        );
        assert_eq!(parse_imu_mount(&serde_json::json!({"enabled": true})), None);
        assert_eq!(parse_imu_mount(&serde_json::json!([true, 0, 0, 0])), None);
    }

    #[test]
    fn js_min_max_propagate_nan() {
        assert!(js_min(f64::NAN, 1.0).is_nan());
        assert!(js_max(1.0, f64::NAN).is_nan());
        assert_eq!(js_max(-1.0, js_min(1.0, 2.0)), 1.0);
        assert_eq!(js_max(-1.0, js_min(1.0, -2.0)), -1.0);
    }
}
