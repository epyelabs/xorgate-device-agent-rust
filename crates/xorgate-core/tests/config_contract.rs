//! Port of `test/config-contract.test.ts` (12 `it()` + 19 `it.each` rows)
//! against `fixtures/config-contract.json` — the artifact the CLOUD's config
//! registry generates (`backend/xorgate-core-service: npm run contract:generate`)
//! and the web app imports for its defaults, copied VERBATIM from the JS
//! repo's `test/config-contract.json` (plan Phase 1 acceptance B3).
//!
//! What this enforces (the two ends MUST agree about):
//!   1. defaults — the cloud UI renders them for an unconfigured device;
//!   2. the preset table — a closed set on both sides;
//!   3. clamp vs bounds — the cloud REJECTS out-of-range values while the
//!      device CLAMPS; coherent only while the device's clamp range CONTAINS
//!      the cloud's accepted range (agent-anatomy invariant 13);
//!   4. apply classes — identical namespace → class map.
//!
//! When this fails, fix the side that is wrong and re-run
//! `npm run contract:generate` in the backend; never edit the JSON by hand.

mod common;

use serde_json::Value;
use xorgate_core::config_namespaces::{ApplyClass, CONFIG_APPLY_CLASSES};
use xorgate_core::recording_config::recording_defaults;
use xorgate_core::settings::*;
use xorgate_core::video_presets::{video_preset_names, DEFAULT_VIDEO_PRESET};

fn contract() -> Value {
    serde_json::from_str(&common::fixture_text("config-contract.json")).unwrap()
}

fn recording_schema(c: &Value) -> &Value {
    &c["namespaces"]["recording"]["jsonSchema"]
}

/// `bounds(path)` (`config-contract.test.ts:49-58`).
fn bounds(c: &Value, path: &[&str]) -> (f64, f64) {
    let mut node = recording_schema(c);
    for key in path {
        node = node
            .get("properties")
            .and_then(|p| p.get(key))
            .unwrap_or_else(|| panic!("contract is missing recording.{}", path.join(".")));
    }
    let min = node["minimum"]
        .as_f64()
        .unwrap_or_else(|| panic!("recording.{} has no minimum", path.join(".")));
    let max = node["maximum"]
        .as_f64()
        .unwrap_or_else(|| panic!("recording.{} has no maximum", path.join(".")));
    (min, max)
}

/// `roundTrip(path, value)` (`config-contract.test.ts:61-75`): build a
/// `recording` value carrying one leaf, then read it back post-clamp.
fn round_trip(path: &[&str], value: f64) -> Option<f64> {
    let mut body = Value::Object(Default::default());
    {
        let mut node = &mut body;
        for key in &path[..path.len() - 1] {
            node = node
                .as_object_mut()
                .unwrap()
                .entry(key.to_string())
                .or_insert_with(|| Value::Object(Default::default()));
        }
        node.as_object_mut()
            .unwrap()
            .insert(path[path.len() - 1].to_string(), serde_json::json!(value));
    }
    let parsed = parse_recording_settings(&body)?;
    let mut out = serde_json::to_value(&parsed).unwrap();
    for key in path {
        out = out.get(key)?.clone();
    }
    out.as_f64()
}

#[test]
fn is_the_artifact_version_this_test_understands() {
    let c = contract();
    assert_eq!(c["version"], 1);
    // The retained config message the worker parses (`v` must be 1).
    assert_eq!(c["wireVersion"], 1);
}

#[test]
fn declares_exactly_the_namespaces_the_worker_applies() {
    // No `agent` namespace: `mode` is reported-only and `mediaEnabled` is not
    // a cloud setting (design Q4).
    let c = contract();
    let mut keys: Vec<&str> = c["namespaces"].as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "cameraMount",
            "cellular",
            "gnssAntBias",
            "imuMount",
            "lteRecovery",
            "recording",
            "simulation",
            "timeSync",
        ]
    );
}

#[test]
fn simulation_bounds_survive_the_device_parser() {
    let c = contract();
    let sim = &c["namespaces"]["simulation"];
    assert_eq!(sim["applyClass"], "agent-poll");
    let parsed = parse_simulation_settings(&sim["defaults"]).expect("defaults parse");
    assert_eq!(serde_json::to_value(&parsed).unwrap(), sim["defaults"]);
    let gap = &sim["jsonSchema"]["properties"]["gapSeconds"];
    for v in [gap["minimum"].as_f64().unwrap(), gap["maximum"].as_f64().unwrap()] {
        let got = parse_simulation_settings(&serde_json::json!({ "gapSeconds": v })).unwrap();
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            serde_json::json!({ "gapSeconds": v })
        );
    }
}

#[test]
fn recording_defaults_match_the_published_defaults() {
    let c = contract();
    assert_eq!(
        serde_json::to_value(recording_defaults()).unwrap(),
        c["namespaces"]["recording"]["defaults"]
    );
}

#[test]
fn time_sync_defaults_match_the_published_defaults() {
    let c = contract();
    assert_eq!(
        serde_json::to_value(TIME_SYNC_DEFAULTS).unwrap(),
        c["namespaces"]["timeSync"]["defaults"]
    );
}

#[test]
fn the_preset_table_is_the_same_closed_set() {
    let c = contract();
    let published: Vec<&str> = recording_schema(&c)["properties"]["video"]["properties"]["preset"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let mut a = published.clone();
    a.sort_unstable();
    let mut b = video_preset_names();
    b.sort_unstable();
    assert_eq!(a, b);
    assert!(published.contains(&DEFAULT_VIDEO_PRESET));
    assert_eq!(DEFAULT_VIDEO_PRESET, recording_defaults().video.preset);
}

#[test]
fn gnss_ant_bias_default_matches_the_hardware_safe_state() {
    assert_eq!(contract()["namespaces"]["gnssAntBias"]["defaults"], false);
}

#[test]
fn imu_mount_default_is_the_no_correction_identity() {
    let c = contract();
    assert_eq!(
        c["namespaces"]["imuMount"]["defaults"],
        serde_json::json!({ "enabled": false, "rollDeg": 0, "pitchDeg": 0, "yawDeg": 0 })
    );
}

/// The whole point of the clamp/reject asymmetry: every value the cloud is
/// willing to send must survive the device parser untouched
/// (`config-contract.test.ts:142-166`, 19 `it.each` rows).
#[test]
fn the_device_clamp_range_contains_the_cloud_bounds() {
    const ROWS: &[&[&str]] = &[
        &["telemetry", "rateHz"],
        &["video", "segmentSeconds"],
        &["video", "bootDeferMaxSecs"],
        &["video", "bootDeferMinBudgetMa"],
        &["video", "gpsGateMaxSecs"],
        &["video", "gpsGateNoProgressSecs"],
        &["video", "spawnStaggerSecs"],
        &["powerShedding", "uvSamples"],
        &["powerShedding", "uvWindowSecs"],
        &["powerShedding", "railLowSecs"],
        &["powerShedding", "escalateDwellSecs"],
        &["powerShedding", "escalateDutyPct"],
        &["powerShedding", "restoreSecs"],
        &["powerShedding", "maxShedsPerHour"],
        &["powerShedding", "railLowVolts"],
        &["powerShedding", "railRestoreVolts"],
        &["powerShedding", "cpuTempC"],
        &["powerShedding", "cpuTempRestoreC"],
        &["powerShedding", "cpuTempSecs"],
    ];
    let c = contract();
    for path in ROWS {
        let (min, max) = bounds(&c, path);
        let label = path.join(".");
        assert_eq!(round_trip(path, min), Some(min), "{label}: minimum {min}");
        assert_eq!(round_trip(path, max), Some(max), "{label}: maximum {max}");
    }
    assert_eq!(ROWS.len(), 19);
}

#[test]
fn time_sync_bounds_survive_the_device_parser_too() {
    let c = contract();
    let schema = &c["namespaces"]["timeSync"]["jsonSchema"];
    for key in ["stepThresholdSecs", "maxStepsPerBoot"] {
        let node = &schema["properties"][key];
        for v in [node["minimum"].as_f64().unwrap(), node["maximum"].as_f64().unwrap()] {
            let got = parse_time_sync_settings(&serde_json::json!({ key: v })).unwrap();
            assert_eq!(serde_json::to_value(&got).unwrap(), serde_json::json!({ key: v }), "{key} {v}");
        }
    }
}

#[test]
fn the_thermal_arm_keys_the_cloud_can_send_are_accepted_by_the_device() {
    let c = contract();
    let shed = recording_schema(&c)["properties"]["powerShedding"]["properties"]
        .as_object()
        .unwrap();
    for k in ["cpuTempC", "cpuTempRestoreC", "cpuTempSecs"] {
        assert!(shed.contains_key(k), "{k}");
    }
    let value = serde_json::json!({ "powerShedding": { "cpuTempC": 82, "cpuTempRestoreC": 73, "cpuTempSecs": 20 } });
    let parsed = parse_recording_settings(&value).unwrap();
    // `toEqual` is order-insensitive; the Rust parser emits ints before
    // decimals (parser order), so compare as values.
    assert_eq!(serde_json::to_value(&parsed).unwrap(), value);
}

/// The reported plane tells the cloud HOW a namespace reaches the hardware;
/// that only means anything while both ends classify the namespaces the
/// same way.
#[test]
fn the_agents_apply_classes_match_the_published_ones() {
    let c = contract();
    let published: Vec<(String, String)> = c["namespaces"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, ns)| (k.clone(), ns["applyClass"].as_str().unwrap().to_string()))
        .collect();
    let mut ours: Vec<(String, String)> = CONFIG_APPLY_CLASSES
        .iter()
        .map(|(k, c)| (k.to_string(), c.as_str().to_string()))
        .collect();
    // `toEqual` on objects is key-order-insensitive.
    let mut published_sorted = published.clone();
    published_sorted.sort();
    ours.sort();
    assert_eq!(ours, published_sorted);
    // Every published class is one the agent can represent.
    for (_, c) in &published {
        assert!(serde_json::from_value::<ApplyClass>(Value::String(c.clone())).is_ok(), "{c}");
    }
}
