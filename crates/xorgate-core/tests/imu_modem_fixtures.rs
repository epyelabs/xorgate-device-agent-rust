//! Fixture-driven tests for `xorgate_core::imu` (`test/imu.test.ts`, the 5
//! non-quaternion `it`; `quatToEuler` is WP1's orientation fixture) and
//! `xorgate_core::modem::find_modem_line` (`test/modem.test.ts`, 2 `it`;
//! `readModemTelemetry` and `createLteCollector` read files — HAL / Phase 2),
//! plus the per-field tolerance of the daemon's telemetry.json
//! (`fixtures/modem/telemetry-tolerance.json`): each raw file text is driven
//! through `ModemTelemetry::from_json` and every consumer reduction in core
//! (`read_modem_clock`, `lte_group`, the LED's `status`, the raw
//! `gnss_ant_bias`), against what the real TS consumers produced.

mod common;

use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_core::clock_step::read_modem_clock;
use xorgate_core::imu::*;
use xorgate_core::modem::*;
use xorgate_core::types::ModemTelemetry;

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

#[test]
fn modem_telemetry_tolerance() {
    let f = common::fixture::<Case>("modem/telemetry-tolerance.json");
    for c in &f.cases {
        assert_eq!(c.op, "modemTelemetryConsumers");
        let (now, raw) = (
            c.input["now"].as_i64().unwrap(),
            c.input["raw"].as_str().unwrap(),
        );
        let e = &c.expected;
        let t = ModemTelemetry::from_json(raw);
        // `if (!t)` (collectors/lte.ts:65).
        assert_eq!(
            Value::Bool(t.is_none()),
            e["noTelemetry"],
            "{}: noTelemetry",
            c.name
        );
        // clock-step.ts:138-153.
        assert_eq!(
            serde_json::to_value(read_modem_clock(now, t.as_ref())).unwrap(),
            e["readModemClock"],
            "{}: readModemClock",
            c.name
        );
        // createLteCollector().read() → `mapTelemetry(t) ?? {}` (lte.ts:62-73).
        let lte = t
            .as_ref()
            .and_then(lte_group)
            .map_or_else(|| json!({}), |g| serde_json::to_value(g).unwrap());
        assert_eq!(lte, e["lteRead"], "{}: lteRead", c.name);
        // led-state.ts:183.
        assert_eq!(
            serde_json::to_value(t.as_ref().and_then(|t| t.status.as_deref())).unwrap(),
            e["ledLteStatus"],
            "{}: ledLteStatus",
            c.name
        );
        // config-sync.ts:513: the raw value WP4's readModemBias port maps.
        let bias = t.as_ref().and_then(|t| t.gnss_ant_bias.clone());
        if common::is_undefined_marker(&e["gnssAntBiasRaw"]) {
            assert_eq!(bias, None, "{}: gnssAntBiasRaw", c.name);
        } else {
            assert_eq!(
                bias.as_ref(),
                Some(&e["gnssAntBiasRaw"]),
                "{}: gnssAntBiasRaw",
                c.name
            );
        }
    }
    assert_eq!(f.cases.len(), 17);
}

#[test]
fn modem_telemetry_typed_view_keeps_every_good_field() {
    // The full snapshot of the tolerance fixture, as the typed view sees it.
    let f = common::fixture::<Case>("modem/telemetry-tolerance.json");
    let full = f
        .cases
        .iter()
        .find(|c| c.name.starts_with("full real-shaped"))
        .unwrap();
    let t = ModemTelemetry::from_json(full.input["raw"].as_str().unwrap()).unwrap();
    assert_eq!(t.timestamp.as_deref(), Some("2026-07-29T19:29:50Z"));
    assert_eq!(
        (t.csq, t.rssi_dbm, t.csq_interval),
        (Some(18.0), Some(-77.0), Some(30.0))
    );
    assert_eq!(
        (t.reg, t.iface.as_deref(), t.ip.as_deref()),
        (Some(1.0), Some("usb0"), Some("10.164.20.3"))
    );
    assert_eq!(
        (t.xtra_valid_min, t.gnss_ant_bias),
        (Some(4210.0), Some(json!(1)))
    );
    let g = t.gnss.as_ref().unwrap();
    assert_eq!(
        g.last_engine_restart.as_ref().unwrap().reason.as_deref(),
        Some("post-reset")
    );
    assert_eq!(g.no_fix_since, None);
    let r = t.recovery.as_ref().unwrap();
    assert_eq!(
        r.last_reset.as_ref().unwrap().path.as_deref(),
        Some("ladder")
    );
    assert_eq!(r.last_gate_skip, None);
    assert_eq!(
        t.sim.as_ref().unwrap().iccid.as_deref(),
        Some("8912230000507537488")
    );
    assert_eq!(
        t.cellular.as_ref().unwrap().apn_applied.as_deref(),
        Some("sp.mb.com")
    );
    assert_eq!(t.uplink.as_ref().unwrap().modem_only, Some(1.0));

    // Null nested `ts` (iso_or_null) keeps the rest of each block.
    let nulls = f
        .cases
        .iter()
        .find(|c| c.name.starts_with("null nested ts"))
        .unwrap();
    let t = ModemTelemetry::from_json(nulls.input["raw"].as_str().unwrap()).unwrap();
    assert_eq!(t.csq, None);
    let restart = t
        .gnss
        .as_ref()
        .unwrap()
        .last_engine_restart
        .clone()
        .unwrap();
    assert_eq!(
        (restart.ts, restart.reason.as_deref()),
        (None, Some("wedge"))
    );
    let skip = t.recovery.as_ref().unwrap().last_gate_skip.clone().unwrap();
    assert_eq!((skip.ts, skip.reason.as_deref()), (None, Some("cooldown")));
    assert_eq!(t.gnss_ant_bias, Some(Value::Null));

    // `Deserialize` goes through the same builder; a non-object is an error there.
    let via_serde: ModemTelemetry =
        serde_json::from_str(nulls.input["raw"].as_str().unwrap()).unwrap();
    assert_eq!(via_serde, t);
    assert!(serde_json::from_str::<ModemTelemetry>("[1,2,3]").is_err());
}
