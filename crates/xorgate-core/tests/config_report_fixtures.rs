//! Fixture-driven tests for `xorgate_core::config_report`, extracted from the
//! pure cases of `test/config-sync.test.ts`, `test/config-sync-cellular.test.ts`
//! and `test/camera-mount.test.ts` ("what the device reports"). Every
//! expectation is the exact `JSON.stringify` the TS produced, so the wire
//! shape of `effective`, `status` and the whole report is proven byte for byte
//! (status `detail` / `reason` wording included — plan README Appendix A).

mod common;

use serde::Deserialize;
use serde_json::{Map, Value};
use xorgate_core::build_info::BuildInfo;
use xorgate_core::cloud_config::parse_cloud_message;
use xorgate_core::config_report::*;
use xorgate_core::settings::AgentSettings;
use xorgate_core::telemetry_scope::TelemetryScopeReport;
use xorgate_core::types::CameraMountState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected_json: Option<String>,
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

fn opt_json<T: serde::Serialize>(v: &Option<T>) -> Option<String> {
    v.as_ref().map(json)
}

/// `undefined` marker or absent key → None; JSON null → Some(Null).
fn present(v: &Value, key: &str) -> Option<Value> {
    match v.get(key) {
        None => None,
        Some(x) if common::is_undefined_marker(x) => None,
        Some(x) => Some(x.clone()),
    }
}

fn from<T: for<'de> Deserialize<'de>>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

fn desired_from(payload: Option<&Value>) -> Option<DesiredSnapshot> {
    let payload = payload?.as_str()?;
    let m = parse_cloud_message(payload).expect("fixture message parses");
    Some(DesiredSnapshot {
        rev: m.config.rev.unwrap_or(0),
        raw: m.raw,
        parsed: m.config,
    })
}

#[test]
fn config_report_suites() {
    let f = common::fixture::<Case>("config_report/config-report.json");
    let build: BuildInfo = from(f.extra["build"].clone());
    let mut ops: std::collections::BTreeMap<String, usize> = Default::default();
    for c in &f.cases {
        *ops.entry(c.op.clone()).or_default() += 1;
        let i = &c.input;
        let got: Option<String> = match c.op.as_str() {
            "buildEffective" => {
                let settings: AgentSettings = from(i["settings"].clone());
                let modem: ModemBiasSample = from(i["modem"].clone());
                let mode = present(i, "mode").map(|m| m.as_str().unwrap().to_string());
                let cam: Option<CameraMountState> =
                    present(i, "cameraMount").filter(|v| !v.is_null()).map(from);
                let scope: Option<TelemetryScopeReport> = present(i, "telemetryScope")
                    .filter(|v| !v.is_null())
                    .map(from);
                Some(json(&build_effective(
                    &settings,
                    &modem,
                    mode.as_deref(),
                    cam.as_ref(),
                    scope.as_ref(),
                    &build,
                )))
            }
            "buildStatus" => {
                let desired = desired_from(i.get("desiredPayload"));
                let effective: Map<String, Value> = from(i["effective"].clone());
                let modem: ModemBiasSample = from(i["modem"].clone());
                let at = i["at"].as_i64().unwrap();
                let hints: ReportHints = present(i, "hints").map(from).unwrap_or_default();
                let cam: Option<CameraMountState> =
                    present(i, "cameraMount").filter(|v| !v.is_null()).map(from);
                Some(json(&build_status(
                    desired.as_ref(),
                    &effective,
                    &modem,
                    at,
                    &hints,
                    cam.as_ref(),
                )))
            }
            "buildConfigReport" => {
                let mut v = i.clone();
                let desired = desired_from(i.get("desiredPayload"));
                let obj = v.as_object_mut().unwrap();
                obj.remove("desiredPayload");
                obj.remove("build");
                obj.insert("desired".into(), serde_json::to_value(&desired).unwrap());
                let input: ReportInput = from(v);
                let report = build_config_report(&input, &build);
                assert!(
                    report.within_cap(),
                    "{}: {} bytes",
                    c.name,
                    report.wire_bytes()
                );
                // The report must survive its own round trip (the agent also
                // parses what it wrote into state files).
                let back: ConfigReport = serde_json::from_str(&report.to_wire()).unwrap();
                assert_eq!(back, report, "{}: round trip", c.name);
                Some(report.to_wire())
            }
            "parseSimTelemetry" => {
                let raw = present(i, "raw").unwrap_or(Value::Null);
                opt_json(&parse_sim_telemetry(&raw))
            }
            "parseUplinkTelemetry" => {
                let raw = present(i, "raw").unwrap_or(Value::Null);
                opt_json(&parse_uplink_telemetry(&raw))
            }
            "parseApnTelemetry" => {
                let raw = present(i, "raw").unwrap_or(Value::Null);
                opt_json(&parse_apn_telemetry(&raw))
            }
            "shouldApplyRev" => {
                let rev = present(i, "rev").map(|r| r.as_u64().unwrap());
                Some(json(&should_apply_rev(
                    rev,
                    i["lastAppliedRev"].as_u64().unwrap(),
                )))
            }
            "namespacesOf" => {
                let settings: AgentSettings = from(i["settings"].clone());
                Some(json(&namespaces_of(&settings)))
            }
            "diffNamespaces" => {
                let base: Map<String, Value> = from(i["base"].clone());
                let next: Map<String, Value> = from(i["next"].clone());
                Some(json(&diff_namespaces(&base, &next)))
            }
            "readModemBias" => {
                let now = i["now"].as_i64().unwrap();
                let telemetry: Option<Value> = i["text"]
                    .as_str()
                    .and_then(|t| serde_json::from_str::<Value>(t).ok());
                Some(json(&modem_bias_from_telemetry(telemetry.as_ref(), now)))
            }
            "loadConfigSyncState" => {
                let state = parse_config_sync_state(i["text"].as_str());
                // The saved form of the parsed state is its wire shape.
                let back: ConfigSyncState = serde_json::from_str(&json(&state)).unwrap();
                assert_eq!(back, state, "{}: round trip", c.name);
                Some(json(&state))
            }
            "ensureConfigSyncBaseline" => {
                let state: ConfigSyncState = from(i["state"].clone());
                let settings: AgentSettings = from(i["settings"].clone());
                let hash = i["settingsHash"].as_str().unwrap();
                let (next, save) = ensure_config_sync_baseline(state, &settings, hash);
                Some(format!(
                    r#"{{"state":{},"save":{}}}"#,
                    next.to_file_string(),
                    save
                ))
            }
            "CONFIG_APPLY_CLASSES" => {
                let classes: Map<String, Value> = CONFIG_APPLY_CLASSES
                    .iter()
                    .map(|(k, c)| (k.to_string(), Value::String(c.as_str().to_string())))
                    .collect();
                Some(json(&classes))
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 147);
    assert_eq!(ops["buildEffective"], 24);
    assert_eq!(ops["buildStatus"], 36);
    assert_eq!(ops["buildConfigReport"], 18);
}
