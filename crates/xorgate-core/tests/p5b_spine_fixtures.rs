//! Phase 5b WP1 fixtures, dumped from the real TS @ 09da38d by
//! `tools/replay/js/dump-p5b-spine.mts`:
//! `settings/obd.json`, `cloud_config/obd.json`,
//! `hardware_block/peripherals.json`, `commands/p5b-commands.json`,
//! `clock_trust/clock-trust.json`.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::clock_trust::{
    ClockTrustCore, TrustEventLevel, TrustOutput, CLOCK_TRUST_GRACE_MS,
};
use xorgate_core::cloud_config::{merge, parse_cloud_config, CloudConfig};
use xorgate_core::commands::{
    in_process_run_result, parse_command, plan, CommandPlan, DeviceCommandName,
};
use xorgate_core::hardware_block::{hardware_blocks_equal, parse_hardware_block_with};
use xorgate_core::js;
use xorgate_core::session::ProbedTimeSource;
use xorgate_core::settings::{
    load_settings_from, obd_defaults, parse_obd_settings, settings_from_read, SettingsRead,
    OBD_POLL_HZ_MAX, OBD_POLL_HZ_MIN,
};
use xorgate_core::types::SteppedClockSource;

const P5B: &str = common::JS_COMMIT_P5B;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    #[serde(default)]
    op: String,
    input: Value,
    #[serde(default)]
    expected_json: Option<String>,
    #[serde(default)]
    expected: Value,
    #[serde(default)]
    saved_after_gnss: Option<String>,
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default)]
    logs: Vec<String>,
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

fn load(rel: &str) -> common::Tagged<Case> {
    let f = common::fixture::<Case>(rel);
    assert_eq!(f.js_commit, P5B, "{rel}");
    f
}

#[test]
fn settings_obd() {
    let f = load("settings/obd.json");
    for c in &f.cases {
        let got = match c.op.as_str() {
            "parseObdSettings" => parse_obd_settings(&c.input).map(|s| json(&s)),
            "OBD_DEFAULTS" => Some(json(&obd_defaults())),
            "OBD_POLL_HZ" => Some(js::to_json_string(&serde_json::json!([
                OBD_POLL_HZ_MIN,
                OBD_POLL_HZ_MAX
            ]))),
            "loadSettings" => {
                let text = c.input.as_str().unwrap();
                let loaded = load_settings_from(SettingsRead::Text(text));
                // `saveSettings({ ...loaded, gnssAntBias: true })`
                let mut doc = match serde_json::to_value(&loaded).unwrap() {
                    Value::Object(m) => m,
                    _ => unreachable!(),
                };
                doc.insert("gnssAntBias".into(), Value::Bool(true));
                assert_eq!(
                    Some(js::map_to_json_string(&doc)),
                    c.saved_after_gnss,
                    "{}: saved",
                    c.name
                );
                Some(json(&loaded))
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    assert!(f.cases.len() >= 35);
}

#[test]
fn cloud_config_obd() {
    let f = load("cloud_config/obd.json");
    for c in &f.cases {
        match c.op.as_str() {
            "parseCloudConfig" => {
                let got = parse_cloud_config(c.input.as_str().unwrap()).map(|p| json(&p));
                assert_eq!(got, c.expected_json, "{}", c.name);
            }
            "applyCloudConfig" => {
                let mut file: Option<String> = c.input["initial"].as_str().map(str::to_string);
                let steps = c.input["steps"].as_array().unwrap();
                let want = c.expected.as_array().unwrap();
                for (i, (step, w)) in steps.iter().zip(want).enumerate() {
                    let config: CloudConfig = serde_json::from_value(step.clone()).unwrap();
                    let before = match &file {
                        None => settings_from_read(SettingsRead::Absent),
                        Some(t) => settings_from_read(SettingsRead::Text(t)),
                    };
                    let out = merge(&before, &config, "/x/settings.json");
                    assert_eq!(
                        Value::Bool(out.changed),
                        w["changed"],
                        "{} step {i}: changed",
                        c.name
                    );
                    if let Some(doc) = out.document_json() {
                        file = Some(doc);
                    }
                    assert_eq!(
                        file.as_deref().map(Value::from),
                        w["fileAfter"].as_str().map(Value::from),
                        "{} step {i}: file",
                        c.name
                    );
                }
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert!(f.cases.len() >= 10);
}

#[test]
fn hardware_block_peripherals() {
    let f = load("hardware_block/peripherals.json");
    for c in &f.cases {
        match c.op.as_str() {
            "parseHardwareBlock" => {
                let mut warnings = Vec::new();
                let input = js::parsed_key_order(c.input.clone());
                let b = parse_hardware_block_with(&input, &mut |m| warnings.push(m));
                let got = b.map(|b| js::to_json_string(&serde_json::to_value(&b).unwrap()));
                assert_eq!(got, c.expected_json, "{}", c.name);
                assert_eq!(warnings, c.warnings, "{}: warnings", c.name);
            }
            "loadHardwareBlock" => {
                // The HAL's `load_hardware_block` = read + this exact parse;
                // the core part is replayed here over the file text.
                let mut logs = Vec::new();
                let got = match c.input.as_str() {
                    None => None,
                    Some(text) => match serde_json::from_str::<Value>(text) {
                        Err(_) => {
                            logs.push(
                                "$PATH is not valid JSON — ignoring; using legacy hardware behavior"
                                    .to_string(),
                            );
                            None
                        }
                        Ok(v) => {
                            let v = js::parsed_key_order(v);
                            let b = parse_hardware_block_with(&v, &mut |m| logs.push(m));
                            if b.is_none() {
                                logs.push("$PATH failed validation — ignoring; using legacy hardware behavior".to_string());
                            }
                            b
                        }
                    },
                };
                let got = got.map(|b| js::to_json_string(&serde_json::to_value(&b).unwrap()));
                assert_eq!(got, c.expected_json, "{}", c.name);
                assert_eq!(logs, c.logs, "{}: logs", c.name);
            }
            "hardwareBlocksEqual" => {
                let p = |v: &Value| parse_hardware_block_with(v, &mut |_| {});
                let (a, b) = (p(&c.input[0]), p(&c.input[1]));
                let got = json(&hardware_blocks_equal(a.as_ref(), b.as_ref()));
                assert_eq!(Some(got), c.expected_json, "{}", c.name);
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert!(f.cases.len() >= 25);
}

#[test]
fn commands_p5b() {
    let f = load("commands/p5b-commands.json");
    for c in &f.cases {
        match c.op.as_str() {
            "parseCommand" => {
                let got = parse_command(c.input.as_str().unwrap().as_bytes()).map(|p| json(&p));
                assert_eq!(got, c.expected_json, "{}", c.name);
            }
            "executeCommand" => {
                let name = DeviceCommandName::parse(c.input.as_str().unwrap()).unwrap();
                assert!(name.is_in_process(), "{}", c.name);
                let CommandPlan::InProcess(n) = plan(name) else {
                    panic!("{}: reaches sudo", c.name)
                };
                let r = in_process_run_result(n);
                let got = serde_json::json!({
                    "stdout": r.stdout, "stderr": r.stderr, "exitCode": r.exit_code,
                    "failed": r.failed, "notFound": r.not_found,
                });
                let want: Value =
                    serde_json::from_str(c.expected_json.as_deref().unwrap()).unwrap();
                assert_eq!(got, want, "{}", c.name);
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert!(f.cases.len() >= 15);
}

/// Replays one scripted `createClockTrust` trace on [`ClockTrustCore`]
/// (the runtime's `tick()` guard — skip once latched — is modelled here).
#[test]
fn clock_trust_traces() {
    let f = load("clock_trust/clock-trust.json");
    let mut scenarios = 0;
    for c in &f.cases {
        if c.input.is_null() {
            // CLOCK_TRUST_DEFAULTS
            assert_eq!(
                c.expected,
                serde_json::json!({"graceMs": 300_000, "pollMs": 5_000, "slowPollMs": 30_000})
            );
            continue;
        }
        scenarios += 1;
        let answers: Vec<String> = serde_json::from_value(c.input["answers"].clone()).unwrap();
        let grace = c.input["graceMs"].as_u64().unwrap_or(CLOCK_TRUST_GRACE_MS);
        let mut mono = c.input["startMono"].as_u64().unwrap();
        let mut core = ClockTrustCore::new(mono, grace);
        let mut probes = 0usize;
        let steps = c.input["steps"].as_array().unwrap();
        let want = c.expected.as_array().unwrap();
        for (i, (s, w)) in steps.iter().zip(want).enumerate() {
            let mut out: Vec<TrustOutput> = Vec::new();
            let mut result = Value::Null;
            match s["op"].as_str().unwrap() {
                "advance" => mono += s["ms"].as_u64().unwrap(),
                "tick" => {
                    if !core.latched() {
                        probes += 1;
                        let a = &answers[probes.min(answers.len()) - 1];
                        let answer = match a.strip_prefix("throw:") {
                            Some(m) => Err(m.to_string()),
                            None => Ok(serde_json::from_value::<ProbedTimeSource>(Value::from(
                                a.as_str(),
                            ))
                            .unwrap()),
                        };
                        out = core.on_probe(answer, mono);
                    }
                }
                "trusted" => {
                    let (t, o) = core.trusted(mono);
                    out = o;
                    result = Value::Bool(t);
                }
                "status" => {
                    let (st, o) = core.status(mono);
                    out = o;
                    result = serde_json::to_value(&st).unwrap();
                }
                "noteHeld" => core.note_held(),
                "noteStep" => {
                    let src: SteppedClockSource =
                        serde_json::from_value(s["source"].clone()).unwrap();
                    out = core.note_step(src, mono);
                }
                other => panic!("unknown step {other}"),
            }
            let events: Vec<Value> = out
                .into_iter()
                .filter_map(|o| match o {
                    TrustOutput::Event(l, m) => Some(serde_json::json!({
                        "level": match l { TrustEventLevel::Info => "info", TrustEventLevel::Warn => "warn" },
                        "msg": m,
                    })),
                    _ => None,
                })
                .collect();
            assert_eq!(result, w["result"], "{} step {i}: result", c.name);
            assert_eq!(
                Value::from(events),
                w["events"],
                "{} step {i}: events",
                c.name
            );
            assert_eq!(
                Value::from(probes),
                w["probes"],
                "{} step {i}: probes",
                c.name
            );
        }
    }
    assert!(scenarios >= 11);
}
