//! Fixture-driven tests for `xorgate_core::imu` (`test/imu.test.ts`, the 5
//! non-quaternion `it`; `quatToEuler` is WP1's orientation fixture) and
//! `xorgate_core::modem::find_modem_line` (`test/modem.test.ts`, 2 `it`;
//! `readModemTelemetry` and `createLteCollector` read files — HAL / Phase 2).

mod common;

use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_core::imu::*;
use xorgate_core::modem::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

#[test]
fn imu_test_ts() {
    let f = common::fixture::<Case>("imu/imu.json");
    let mut n = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *n.entry(c.op.clone()).or_default() += 1;
        let got: Value = match c.op.as_str() {
            "parseImuLine" => {
                serde_json::to_value(parse_imu_line(c.input.as_str().unwrap())).unwrap()
            }
            "parseI2cDetect" => {
                serde_json::to_value(parse_i2c_detect(c.input.as_str().unwrap())).unwrap()
            }
            "findImuAddress" => {
                let present: Vec<u32> = serde_json::from_value(c.input["present"].clone()).unwrap();
                let candidates: Vec<u32> = if common::is_undefined_marker(&c.input["candidates"]) {
                    IMU_ADDRESSES.to_vec()
                } else {
                    serde_json::from_value(c.input["candidates"].clone()).unwrap()
                };
                serde_json::to_value(find_imu_address(&present, &candidates)).unwrap()
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 16);
    assert_eq!(n["parseImuLine"], 9);
    assert_eq!(n["parseI2cDetect"], 3);
    assert_eq!(n["findImuAddress"], 4);
    assert_eq!(
        f.extra["imuConstants"],
        json!({ "i2cBus": IMU_I2C_BUS, "addresses": IMU_ADDRESSES, "bmsAddress": IMU_BMS_ADDRESS })
    );
}

#[test]
fn modem_test_ts() {
    let f = common::fixture::<Case>("modem/modem.json");
    for c in &f.cases {
        assert_eq!(c.op, "findModemLine");
        let stdout = c.input["stdout"].as_str().unwrap();
        let got = if common::is_undefined_marker(&c.input["vendorId"]) {
            find_modem_line_default(stdout)
        } else {
            let products: Vec<String> =
                serde_json::from_value(c.input["productIds"].clone()).unwrap();
            let refs: Vec<&str> = products.iter().map(String::as_str).collect();
            find_modem_line(stdout, c.input["vendorId"].as_str().unwrap(), &refs)
        };
        assert_eq!(serde_json::to_value(got).unwrap(), c.expected, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 5);
}
