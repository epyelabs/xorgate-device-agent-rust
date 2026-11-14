//! Fixture-driven tests for `xorgate_core::config_report`, extracted from the
//! pure cases of `test/config-sync.test.ts`, `test/config-sync-cellular.test.ts`
//! and `test/camera-mount.test.ts` ("what the device reports"). Every
//! expectation is the exact `JSON.stringify` the TS produced, so the wire
//! shape of `effective`, `status` and the whole report is proven byte for byte
//! (status `detail` / `reason` wording included — plan README Appendix A).
//! A bare `Map<String, Value>` result is printed with
//! `js::map_to_json_string` — the form it takes inside the report /
//! config-sync.json (serde_json's own formatter prints `0.000005` as `5e-6`;
//! integration review M1).

mod common;

use serde::Deserialize;
use serde_json::{Map, Value};
use xorgate_core::build_info::BuildInfo;
use xorgate_core::cloud_config::parse_cloud_message;
use xorgate_core::config_report::*;
use xorgate_core::js;
use xorgate_core::settings::{load_settings_from, AgentSettings, SettingsRead};
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

/// D85 (Phase 8 D9; `accepted-divergences.md`): the Rust agent appends
/// `"runtime":"rust"` as the LAST key of every `effective.agent` block; the JS
/// goldens carry no such key. Assert it sits exactly there (once per `agent`
/// block, nowhere else) and remove it, so everything else is still compared
/// byte for byte with the JS. Returns the stripped text and the strip count.
fn strip_d85(got: &str) -> (String, usize) {
    let caps = serde_json::to_string(AGENT_CAPABILITIES).unwrap();
    let with = format!(r#""capabilities":{caps},"runtime":"rust"}}"#);
    let without = format!(r#""capabilities":{caps}}}"#);
    let n = got.matches(&with).count();
    assert_eq!(
        n,
        got.matches(r#""agent":{"#).count(),
        "D85: one marker per agent block: {got}"
    );
    assert_eq!(
        n,
        got.matches(r#""runtime""#).count(),
        "D85: no marker elsewhere: {got}"
    );
    (got.replace(&with, &without), n)
}

#[test]
fn config_report_suites() {
    let f = common::fixture::<Case>("config_report/config-report.json");
    let build: BuildInfo = from(f.extra["build"].clone());
    let mut ops: std::collections::BTreeMap<String, usize> = Default::default();
    let mut d85 = 0;
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
                let obd = present(i, "obd").filter(|v| !v.is_null());
                Some(js::map_to_json_string(&build_effective(
                    &settings,
                    &modem,
                    mode.as_deref(),
                    cam.as_ref(),
                    scope.as_ref(),
                    obd.as_ref(),
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
                let settings_opt: Option<AgentSettings> = present(i, "settings")
                    .map(|v| load_settings_from(SettingsRead::Text(&v.to_string())));
                Some(json(&build_status(
                    desired.as_ref(),
                    &effective,
                    &modem,
                    at,
                    &hints,
                    cam.as_ref(),
                    settings_opt.as_ref(),
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
                Some(js::map_to_json_string(&namespaces_of(&settings)))
            }
            "diffNamespaces" => {
                let base: Map<String, Value> = from(i["base"].clone());
                let next: Map<String, Value> = from(i["next"].clone());
                Some(js::map_to_json_string(&diff_namespaces(&base, &next)))
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
                // The saved form of the parsed state is its wire shape: a
                // re-read writes the same bytes. (Byte-, not value-equality:
                // an integer above 2^53 parses exactly but prints as the JS
                // double, which then re-reads as that double.)
                let back: ConfigSyncState = serde_json::from_str(&json(&state)).unwrap();
                assert_eq!(json(&back), json(&state), "{}: round trip", c.name);
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
            "unheldNamespaces" => {
                let d = desired_from(i.get("desiredPayload")).expect("payload");
                let settings = load_settings_from(SettingsRead::Text(&i["settings"].to_string()));
                let baseline: Option<Map<String, Value>> = present(i, "baseline").map(from);
                Some(json(&unheld_namespaces(
                    &d.parsed,
                    &settings,
                    baseline.as_ref(),
                )))
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
        let got = match c.op.as_str() {
            "buildEffective" | "buildConfigReport" => got.map(|g| {
                let (g, n) = strip_d85(&g);
                d85 += n;
                g
            }),
            _ => got,
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    // Every JS golden `agent` block (12 at 09da38d) got the D85 marker.
    assert_eq!(d85, 12, "D85 markers stripped");
    // Phase 1's 155 cases (re-evaluated @ 09da38d) + the [p5b] ones.
    assert_eq!(f.cases.len(), 155 + 39, "{}", f.cases.len());
    assert!(ops["buildEffective"] >= 26 + 3);
    assert!(ops["buildStatus"] >= 36 + 26);
    assert!(ops["buildConfigReport"] >= 19 + 2);
    assert!(ops["unheldNamespaces"] >= 7);
}
