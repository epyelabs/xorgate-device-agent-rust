//! Fixture-driven tests for `xorgate_core::orientation`, from
//! `test/orientation.test.ts` (11 cases) + `test/imu.test.ts` `quatToEuler`
//! (2 cases) + 2 extras. Each case carries the TS's own f64 results (checked
//! at 1e-9 — libm may differ in the last ulp) AND the vitest `toBeCloseTo`
//! expectation (checked at its `digits`).

mod common;

use serde::Deserialize;
use xorgate_core::orientation::*;

#[derive(Deserialize, Clone, Copy)]
struct Q {
    r: f64,
    i: f64,
    j: f64,
    k: f64,
}

impl From<Q> for Quat {
    fn from(q: Q) -> Self {
        Quat {
            r: q.r,
            i: q.i,
            j: q.j,
            k: q.k,
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
struct E {
    roll: f64,
    pitch: f64,
    yaw: f64,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
struct Deg {
    roll_deg: f64,
    pitch_deg: f64,
    yaw_deg: f64,
}

impl From<Deg> for EulerDeg {
    fn from(d: Deg) -> Self {
        EulerDeg {
            roll_deg: d.roll_deg,
            pitch_deg: d.pitch_deg,
            yaw_deg: d.yaw_deg,
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
struct CloseTo {
    roll: f64,
    pitch: f64,
    yaw: f64,
    digits: i32,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
struct Mount {
    enabled: bool,
    roll_deg: f64,
    pitch_deg: f64,
    yaw_deg: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    op: String,
    euler: Option<serde_json::Value>,
    quat: Option<Q>,
    back: Option<E>,
    close_to: Option<CloseTo>,
    norm2: Option<f64>,
    digits: Option<i32>,
    conj: Option<Q>,
    product: Option<Q>,
    a: Option<Q>,
    b: Option<Q>,
    vehicle: Option<Deg>,
    mount_euler: Option<Deg>,
    sensor: Option<Q>,
    sensor_euler: Option<E>,
    mount: Option<Mount>,
    corrected: Option<Q>,
}

const TS_TOL: f64 = 1e-9;

fn assert_quat_close(got: &Quat, want: &Q, tol: f64, what: &str) {
    for (g, w, n) in [
        (got.r, want.r, "r"),
        (got.i, want.i, "i"),
        (got.j, want.j, "j"),
        (got.k, want.k, "k"),
    ] {
        assert!((g - w).abs() < tol, "{what}.{n}: got {g}, TS {w}");
    }
}

fn assert_euler_close(got: &Euler, want: &E, tol: f64, what: &str) {
    for (g, w, n) in [
        (got.roll, want.roll, "roll"),
        (got.pitch, want.pitch, "pitch"),
        (got.yaw, want.yaw, "yaw"),
    ] {
        assert!((g - w).abs() < tol, "{what}.{n}: got {g}, TS {w}");
    }
}

/// vitest `toBeCloseTo(expected, digits)`: `|a - b| < 10^-digits / 2`.
fn assert_close_to(got: &Euler, c: &CloseTo, what: &str) {
    let tol = 10f64.powi(-c.digits) / 2.0;
    for (g, w, n) in [
        (got.roll, c.roll, "roll"),
        (got.pitch, c.pitch, "pitch"),
        (got.yaw, c.yaw, "yaw"),
    ] {
        assert!((g - w).abs() < tol, "{what}.{n}: got {g}, expected {w} ({} digits)", c.digits);
    }
}

#[test]
fn orientation_test_ts() {
    let f = common::fixture::<Case>("orientation/orientation.json");
    for c in &f.cases {
        let name = c.name.as_str();
        match c.op.as_str() {
            "eulerToQuat->quatToEuler" => {
                let e: Deg = serde_json::from_value(c.euler.clone().unwrap()).unwrap();
                let q = euler_to_quat(&e.into());
                assert_quat_close(&q, &c.quat.unwrap(), TS_TOL, name);
                let back = quat_to_euler(&q);
                assert_euler_close(&back, &c.back.unwrap(), TS_TOL, name);
                assert_close_to(&back, &c.close_to.unwrap(), name);
            }
            "eulerToQuat-unit" => {
                let e: Deg = serde_json::from_value(c.euler.clone().unwrap()).unwrap();
                let q = euler_to_quat(&e.into());
                assert_quat_close(&q, &c.quat.unwrap(), TS_TOL, name);
                let n2 = q.r * q.r + q.i * q.i + q.j * q.j + q.k * q.k;
                let digits = c.digits.unwrap();
                assert!((n2 - 1.0).abs() < 10f64.powi(-digits) / 2.0, "{name}: norm² {n2}");
                assert!((n2 - c.norm2.unwrap()).abs() < TS_TOL, "{name}: norm² vs TS");
            }
            "multiply-conjugate" => {
                let e: Deg = serde_json::from_value(c.euler.clone().unwrap()).unwrap();
                let q = euler_to_quat(&e.into());
                assert_quat_close(&q, &c.quat.unwrap(), TS_TOL, name);
                let conj = quat_conjugate(&q);
                assert_quat_close(&conj, &c.conj.unwrap(), TS_TOL, name);
                let id = quat_multiply(&q, &conj);
                assert_quat_close(&id, &c.product.unwrap(), TS_TOL, name);
                let tol = 10f64.powi(-c.digits.unwrap()) / 2.0;
                assert!((id.r - 1.0).abs() < tol && id.i.abs() < tol && id.j.abs() < tol && id.k.abs() < tol, "{name}");
            }
            "compose" => {
                let a: Quat = c.a.unwrap().into();
                let b: Quat = c.b.unwrap().into();
                let p = quat_multiply(&a, &b);
                assert_quat_close(&p, &c.product.unwrap(), TS_TOL, name);
                let e = quat_to_euler(&p);
                let want: E = serde_json::from_value(c.euler.clone().unwrap()).unwrap();
                assert_euler_close(&e, &want, TS_TOL, name);
                assert_close_to(&e, &c.close_to.unwrap(), name);
            }
            "applyMountCorrection" => {
                let vehicle = euler_to_quat(&c.vehicle.unwrap().into());
                let sensor = quat_multiply(&vehicle, &euler_to_quat(&c.mount_euler.unwrap().into()));
                assert_quat_close(&sensor, &c.sensor.unwrap(), TS_TOL, name);
                assert_euler_close(&quat_to_euler(&sensor), &c.sensor_euler.unwrap(), TS_TOL, name);
                if name.starts_with("un-swaps") {
                    // symptom: swapped
                    assert!(quat_to_euler(&sensor).pitch.abs() > 10.0, "{name}: symptom");
                }
                let m = c.mount.unwrap();
                let mount = ImuMount {
                    enabled: m.enabled,
                    roll_deg: m.roll_deg,
                    pitch_deg: m.pitch_deg,
                    yaw_deg: m.yaw_deg,
                };
                let corrected = apply_mount_correction(&sensor, &mount);
                assert_quat_close(&corrected, &c.corrected.unwrap(), TS_TOL, name);
                let e = quat_to_euler(&corrected);
                let want: E = serde_json::from_value(c.euler.clone().unwrap()).unwrap();
                assert_euler_close(&e, &want, TS_TOL, name);
                assert_close_to(&e, &c.close_to.unwrap(), name);
            }
            "quatToEuler" => {
                let e = quat_to_euler(&c.quat.unwrap().into());
                let want: E = serde_json::from_value(c.euler.clone().unwrap()).unwrap();
                assert_euler_close(&e, &want, TS_TOL, name);
                assert_close_to(&e, &c.close_to.unwrap(), name);
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert_eq!(f.cases.len(), 15);
}
