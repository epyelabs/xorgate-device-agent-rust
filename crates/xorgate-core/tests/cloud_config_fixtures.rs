//! Fixture-driven tests for `xorgate_core::cloud_config`: the parse cases of
//! `test/cloud-config.test.ts` / `config-sync-cellular` / `config-sync`
//! (exact `JSON.stringify`), and the `applyCloudConfig` cases replayed as
//! file-state scripts (`save` / `write` / `unreadable` → `apply`) against
//! [`merge`] with the settings read performed by the test harness.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::cloud_config::*;
use xorgate_core::settings::{
    load_settings_from, parse_settings_text, settings_from_read, SettingsRead,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Step {
    op: String,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    expected_json: Value,
    #[serde(default)]
    doc: Option<Value>,
    #[serde(default)]
    file_after: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    expect: Option<Value>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    steps: Vec<Step>,
}

/// The settings.json as the script sees it.
#[derive(Debug, Clone, PartialEq)]
enum File {
    Absent,
    Unreadable(String),
    Text(String),
}

#[test]
fn cloud_config_suites() {
    let f = common::fixture::<Case>("cloud_config/cloud-config.json");
    let mut parse_steps = 0;
    let mut apply_steps = 0;
    for c in &f.cases {
        let mut file = File::Absent;
        for (i, s) in c.steps.iter().enumerate() {
            let ctx = format!("{} step#{i} {}", c.name, s.op);
            match s.op.as_str() {
                "parseCloudConfig" => {
                    parse_steps += 1;
                    let got = parse_cloud_config(s.payload.as_ref().unwrap())
                        .map(|c| serde_json::to_string(&c).unwrap());
                    let want = s.expected_json.as_str().map(str::to_string);
                    assert_eq!(got, want, "{ctx}");
                }
                "parseCloudMessage" => {
                    parse_steps += 1;
                    let got = parse_cloud_message(s.payload.as_ref().unwrap())
                        .map(|m| serde_json::to_string(&m).unwrap());
                    let want = s.expected_json.as_str().map(str::to_string);
                    assert_eq!(got, want, "{ctx}");
                    // The raw half is the exact delivered object; the config
                    // half must survive a serde round trip (the DesiredSnapshot
                    // keeps both in memory).
                    if let Some(m) = parse_cloud_message(s.payload.as_ref().unwrap()) {
                        let back: CloudConfigMessage =
                            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
                        assert_eq!(back, m, "{ctx}: round trip");
                    }
                }
                "save" => {
                    // `saveSettings(doc)` wrote `JSON.stringify(doc)`; the
                    // fixture carries those bytes.
                    let bytes = s.file_after.clone().unwrap();
                    assert_eq!(
                        serde_json::from_str::<Value>(&bytes).unwrap(),
                        *s.doc.as_ref().unwrap(),
                        "{ctx}: saved bytes are the doc"
                    );
                    file = File::Text(bytes);
                }
                "write" => file = File::Text(s.text.clone().unwrap()),
                "unreadable" => {
                    let File::Text(t) = &file else {
                        panic!("{ctx}: unreadable needs a file")
                    };
                    file = File::Unreadable(t.clone());
                }
                "readable" => {
                    let File::Unreadable(t) = &file else {
                        panic!("{ctx}: readable needs an unreadable file")
                    };
                    file = File::Text(t.clone());
                }
                "apply" => {
                    apply_steps += 1;
                    let config: CloudConfig =
                        serde_json::from_value(s.config.clone().unwrap()).unwrap();
                    let before = match &file {
                        File::Absent => settings_from_read(SettingsRead::Absent),
                        File::Unreadable(_) => settings_from_read(SettingsRead::Unreadable),
                        File::Text(t) => parse_settings_text(t),
                    };
                    let out = merge(&before, &config, "<path>");
                    let expect = s.expect.as_ref().unwrap();
                    if let Some(throws) = expect.get("throws") {
                        assert_eq!(
                            out.refuse_reason.as_deref(),
                            throws.as_str(),
                            "{ctx}: refusal"
                        );
                        assert!(!out.changed, "{ctx}");
                        assert!(out.next.is_none(), "{ctx}");
                        continue;
                    }
                    assert_eq!(out.refuse_reason, None, "{ctx}");
                    assert_eq!(
                        Value::Bool(out.changed),
                        expect["changed"],
                        "{ctx}: changed"
                    );
                    if out.changed {
                        // The writer's bytes (`document_json`: JS number
                        // printing — integration review M1).
                        let bytes = out.document_json().unwrap();
                        // Byte-identical to what the TS `saveSettings` wrote.
                        assert_eq!(
                            Value::String(bytes.clone()),
                            expect["fileAfter"],
                            "{ctx}: file bytes"
                        );
                        // And the typed `next` is what a re-read yields.
                        let next = out.next.as_ref().unwrap();
                        assert_eq!(
                            serde_json::to_string(next).unwrap(),
                            expect["loadedJson"].as_str().unwrap(),
                            "{ctx}: next (parser order)"
                        );
                        file = File::Text(bytes);
                    } else {
                        assert!(out.next.is_none() && out.document.is_none(), "{ctx}");
                        // Unchanged: the file is exactly as it was.
                        let now = match &file {
                            File::Text(t) => Some(t.clone()),
                            File::Absent => None,
                            File::Unreadable(_) => panic!("{ctx}"),
                        };
                        assert_eq!(
                            now.clone().map(Value::String).unwrap_or(Value::Null),
                            expect["fileAfter"],
                            "{ctx}: untouched file"
                        );
                    }
                    // Whatever the file now holds, loadSettings agrees.
                    let loaded = match &file {
                        File::Text(t) => load_settings_from(SettingsRead::Text(t)),
                        File::Absent => load_settings_from(SettingsRead::Absent),
                        File::Unreadable(_) => load_settings_from(SettingsRead::Unreadable),
                    };
                    assert_eq!(
                        serde_json::to_string(&loaded).unwrap(),
                        expect["loadedJson"].as_str().unwrap(),
                        "{ctx}: loadSettings"
                    );
                }
                other => panic!("{ctx}: unknown op {other}"),
            }
        }
    }
    assert_eq!(f.cases.len(), 46);
    assert_eq!(parse_steps, 59);
    assert_eq!(apply_steps, 48);
}
