//! Fixture-driven tests for `xorgate_core::settings`, `recording_config`,
//! `video_presets`, `config_namespaces` and `settings_migrate`, extracted
//! from `test/settings.test.ts`, `test/recording-settings.test.ts`,
//! `test/config-sync-cellular.test.ts`, `test/camera-mount.test.ts`,
//! `test/orientation.test.ts` (parseImuMount) and `test/settings-migrate.test.ts`.
//!
//! Every expectation is the exact `JSON.stringify` of what the TS produced,
//! so these tests prove the parsers AND the serialisation (invariant 10).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::config_namespaces::{CONFIG_APPLY_CLASSES, CONFIG_NAMESPACE_KEYS};
use xorgate_core::orientation::parse_imu_mount;
use xorgate_core::recording_config::{recording_defaults, resolve_recording_config};
use xorgate_core::settings::*;
use xorgate_core::settings_migrate::{migrate_media_enabled, MediaEnabledMigration};
use xorgate_core::video_presets::{DEFAULT_VIDEO_PRESET, VIDEO_PRESETS};

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

fn opt_json<T: serde::Serialize>(v: &Option<T>) -> Option<String> {
    v.as_ref().map(json)
}

#[test]
fn settings_test_ts_documents() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        input: Option<String>,
        #[serde(rename = "expectedJson")]
        expected_json: String,
        #[serde(rename = "readResult")]
        read_result: String,
    }
    let f = common::fixture::<Case>("settings/settings.json");
    for c in &f.cases {
        let read = match &c.input {
            None => SettingsRead::Absent,
            Some(t) => SettingsRead::Text(t),
        };
        let res = settings_from_read(read);
        let got_result = match &res {
            Ok(_) => "ok".to_string(),
            Err(e) => e.as_str().to_string(),
        };
        assert_eq!(got_result, c.read_result, "{}: readSettingsFile", c.name);
        let loaded = load_settings_from(read);
        assert_eq!(json(&loaded), c.expected_json, "{}: loadSettings", c.name);
        // A loaded document must survive its own round trip untouched
        // (loadSettings ∘ saveSettings is idempotent).
        let again = load_settings_from(SettingsRead::Text(&c.expected_json));
        assert_eq!(again, loaded, "{}: idempotent", c.name);
        // And the derived Deserialize agrees with the allowlist parser on
        // already-clean documents.
        let derived: AgentSettings = serde_json::from_str(&c.expected_json).unwrap();
        assert_eq!(derived, loaded, "{}: derive", c.name);
    }
    assert_eq!(f.cases.len(), 22);
}

#[test]
fn recording_settings_test_ts() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        op: String,
        input: Value,
        #[serde(rename = "expectedJson")]
        expected_json: Option<String>,
    }
    let f = common::fixture::<Case>("settings/recording-settings.json");
    for c in &f.cases {
        let input = if common::is_undefined_marker(&c.input) {
            Value::Null
        } else {
            c.input.clone()
        };
        let got = match c.op.as_str() {
            "parseRecordingSettings" => opt_json(&parse_recording_settings(&input)),
            "resolveRecordingConfig" => {
                let s = parse_recording_settings(&input);
                // The TS passes the raw (already-parsed in practice) object
                // through; here the raw input is a parser-shaped document.
                let s = if input.is_null() {
                    None
                } else {
                    Some(s.unwrap_or_else(|| {
                        serde_json::from_value(input.clone()).expect("RecordingSettings")
                    }))
                };
                Some(json(&resolve_recording_config(s.as_ref())))
            }
            "loadSettings" => Some(json(&load_settings_from(SettingsRead::Text(
                input.as_str().unwrap(),
            )))),
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 48);

    // Module-level constants pinned by the same extraction run.
    let extra = &f.extra;
    assert_eq!(
        json(&recording_defaults()),
        extra["recordingDefaultsJson"].as_str().unwrap(),
        "RECORDING_DEFAULTS"
    );
    assert_eq!(
        json(&TIME_SYNC_DEFAULTS),
        extra["timeSyncDefaultsJson"].as_str().unwrap(),
        "TIME_SYNC_DEFAULTS"
    );
    assert_eq!(
        json(&VIDEO_PRESETS),
        extra["videoPresetsJson"].as_str().unwrap(),
        "VIDEO_PRESETS"
    );
    assert_eq!(extra["defaultVideoPreset"].as_str().unwrap(), DEFAULT_VIDEO_PRESET);
    let classes: serde_json::Map<String, Value> = CONFIG_APPLY_CLASSES
        .iter()
        .map(|(k, c)| (k.to_string(), Value::String(c.as_str().to_string())))
        .collect();
    assert_eq!(
        json(&classes),
        extra["configApplyClassesJson"].as_str().unwrap(),
        "CONFIG_APPLY_CLASSES"
    );
    let keys: Vec<Value> = CONFIG_NAMESPACE_KEYS
        .iter()
        .map(|k| Value::String(k.to_string()))
        .collect();
    assert_eq!(Value::Array(keys), extra["configNamespaceKeys"], "CONFIG_NAMESPACE_KEYS");
}

#[test]
fn namespace_parsers() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        op: String,
        input: Value,
        #[serde(rename = "expectedJson")]
        expected_json: Option<String>,
    }
    let f = common::fixture::<Case>("settings/namespace-parsers.json");
    for c in &f.cases {
        let input = if common::is_undefined_marker(&c.input) {
            Value::Null
        } else {
            c.input.clone()
        };
        let got = match c.op.as_str() {
            "parseCellularSettings" => opt_json(&parse_cellular_settings(&input)),
            "parseCameraMountSettings" => opt_json(&parse_camera_mount_settings(&input)),
            "parseLteRecoverySettings" => opt_json(&parse_lte_recovery_settings(&input)),
            "parseTimeSyncSettings" => opt_json(&parse_time_sync_settings(&input)),
            "parseSimulationSettings" => opt_json(&parse_simulation_settings(&input)),
            // parseImuMount returns `null` (serialised) rather than undefined.
            "parseImuMount" => Some(json(&parse_imu_mount(&input))),
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 51);
}

#[test]
fn settings_migrate_test_ts() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        #[serde(rename = "streamKeys")]
        stream_keys: Vec<String>,
        input: Option<String>,
        expected: MediaEnabledMigration,
        #[serde(rename = "fileAfter")]
        file_after: Option<String>,
    }
    let f = common::fixture::<Case>("settings/settings-migrate.json");
    for c in &f.cases {
        let out = migrate_media_enabled(c.input.as_deref(), &c.stream_keys);
        // The idempotence case runs the migration twice in the TS; replay
        // the second pass over the first pass's written document.
        let (out, written) = if c.name.starts_with("is idempotent") {
            let first = out.save.as_ref().map(json).expect("first pass writes");
            let second = migrate_media_enabled(Some(&first), &c.stream_keys);
            assert_eq!(second.save, None, "{}: second pass must not write", c.name);
            (second, Some(first))
        } else {
            let written = out.save.as_ref().map(json);
            (out, written)
        };
        assert_eq!(out.result, c.expected, "{}: result", c.name);
        // fileAfter is the file content after the TS ran: the written
        // document when it wrote, the untouched input otherwise.
        let after = written.or_else(|| c.input.clone());
        assert_eq!(after, c.file_after, "{}: file after", c.name);
    }
    assert_eq!(f.cases.len(), 10);
}
