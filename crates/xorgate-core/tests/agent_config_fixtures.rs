//! `fixtures/agent_config/load-config.json`: the real `loadConfig()`
//! (`src/agent/config.ts:197-308`) dumped by tsx over argv / env /
//! settings.json cases (mode precedence, `??` vs `||`, `Number()` parsing,
//! `path.resolve`, the error texts) and replayed through
//! `xorgate_core::agent_config::load_config`.

mod common;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_core::agent_config::{load_config, AgentConfig, ConfigInputs};
use xorgate_core::settings::parse_settings_text;

#[derive(Deserialize)]
struct Input {
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    #[serde(rename = "settingsJson")]
    settings_json: Option<String>,
    cwd: String,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    input: Input,
    expected: Value,
}

/// The TS `AgentConfig` minus `creds` and `media.streamGuard`, as JSON.
fn view(c: &AgentConfig) -> Value {
    let mut media = json!({
        "enabled": c.media.enabled,
        "binaryPath": c.media.binary_path,
        "libDir": c.media.lib_dir,
        "manifestPath": c.media.manifest_path,
        "source": c.media.source,
        "mediaType": c.media.media_type,
        "pipelineDir": c.media.pipeline_dir,
        "logLevel": c.media.log_level,
        "idleTeardownSecs": c.media.idle_teardown_secs,
        "certPath": c.media.cert_path,
        "keyPath": c.media.key_path,
        "caCertPath": c.media.ca_cert_path,
    });
    if let Some(p) = &c.media.pipeline_path {
        media["pipelinePath"] = json!(p);
    }
    json!({
        "deviceId": c.device_id,
        "endpoint": c.endpoint,
        "mode": c.mode.as_str(),
        "rateHz": c.rate_hz,
        "intervalMs": c.interval_ms,
        "mqttKeepaliveSec": c.mqtt_keepalive_sec,
        "offline": c.offline,
        "telemetryTopic": c.topics.telemetry,
        "statusTopic": c.topics.status,
        "configTopic": c.topics.config,
        "configReportedTopic": c.topics.config_reported,
        "commandTopic": c.topics.command,
        "commandResponseTopic": c.topics.command_response,
        "smsTopic": c.topics.sms,
        "certPath": c.cert_path,
        "keyPath": c.key_path,
        "caPath": c.ca_path,
        "hardwarePath": c.hardware_path,
        "replay": {
            "assetsDir": c.replay.assets_dir,
            "defaultScenarioId": c.replay.default_scenario_id,
        },
        "media": media,
    })
}

/// Numbers compare as f64 (the TS prints `rateHz: 1`, serde_json `1.0`).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same(v, w))
        }
        _ => a == b,
    }
}

#[test]
fn load_config_matches_the_ts() {
    let f = common::fixture::<Case>("agent_config/load-config.json");
    for c in &f.cases {
        let env = |k: &str| c.input.env.get(k).cloned();
        let settings_mode = c
            .input
            .settings_json
            .as_deref()
            .and_then(|t| parse_settings_text(t).ok())
            .and_then(|s| s.mode);
        let got = load_config(&ConfigInputs {
            argv: &c.input.argv,
            env: &env,
            settings_mode,
            cwd: &c.input.cwd,
        });
        match (&got, c.expected.get("ok"), c.expected.get("error")) {
            (Ok(cfg), Some(want), None) => {
                let have = view(cfg);
                assert!(
                    same(&have, want),
                    "{}:\n have {have:#}\n want {want:#}",
                    c.name
                );
            }
            (Err(msg), None, Some(want)) => assert_eq!(msg, want, "{}", c.name),
            _ => panic!("{}: got {got:?}, expected {}", c.name, c.expected),
        }
    }
    assert!(f.cases.len() >= 40);
}
