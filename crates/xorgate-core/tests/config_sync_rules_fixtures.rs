//! `xorgate_core::config_sync_rules` against `fixtures/config_sync_rules/rules.json`:
//! every expectation was OBSERVED on the real `ConfigSync` class @ 89b5c64
//! (driver `phase-2/evidence/wp3-tools/dump-config-sync.mts`). The I/O-shaped
//! ops of the same file (`appliedSummary`, `stateFile`, `bootUpgrade`) are
//! driven by `xorgate-agent/tests/config_message.rs`.

mod common;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_core::config_report::{
    build_config_report, parse_config_sync_state, ApnBlock, ModemBiasSample, NamespaceStatus,
    ReportInput, ReportOrigin, SyncPatch,
};
use xorgate_core::config_sync_rules::*;
use xorgate_core::settings::{parse_settings_object, AgentSettings};

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

fn origin(v: &Value) -> ReportOrigin {
    serde_json::from_value(v.clone()).unwrap()
}

fn settings(v: &Value) -> AgentSettings {
    parse_settings_object(v.as_object().unwrap())
}

#[test]
fn rules_fixture() {
    let f = common::fixture::<Case>("config_sync_rules/rules.json");
    let mut n = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *n.entry(c.op.clone()).or_default() += 1;
        let (i, e) = (&c.input, &c.expected);
        match c.op.as_str() {
            "nextOrigin" => {
                let lp = i["localPending"].as_bool().unwrap();
                let mut pending = None;
                for o in i["seq"].as_array().unwrap() {
                    pending = Some(next_origin(pending, origin(o), lp));
                }
                assert_eq!(pending, Some(origin(&e["origin"])), "{}", c.name);
                assert_eq!(e["reports"], 1, "{}", c.name);
            }
            "needsSettleReport" => {
                let status: IndexMap<String, NamespaceStatus> =
                    serde_json::from_value(i["status"].clone()).unwrap();
                let settle = needs_settle_report(&status);
                assert_eq!(json!(settle), e["settle"], "{}", c.name);
                if settle {
                    // The fixture records the origin the settle report was
                    // PUBLISHED with. A `local` first report that went out
                    // online retired the change (baseline = current,
                    // `:1556-1559`), so the settle timer's `request("local")`
                    // meets an empty diff at publish time and downgrades to
                    // `boot` (`:1527-1533`) — model both steps.
                    let requested = settle_origin(origin(&i["origin"]));
                    let empty = AgentSettings::default();
                    let published = resolve_report_origin(
                        requested,
                        Some(&xorgate_core::config_report::namespaces_of(&empty)),
                        &empty,
                    )
                    .origin;
                    assert_eq!(published, origin(&e["settleOrigin"]), "{}", c.name);
                }
            }
            "modemArrivalTrigger" => {
                let s: ModemBiasSample = serde_json::from_value(i["sample"].clone()).unwrap();
                assert_eq!(json!(modem_arrival_trigger(&s)), e["trigger"], "{}", c.name);
            }
            "apnFingerprint" => {
                let apn: Option<ApnBlock> = serde_json::from_value(i["apn"].clone()).unwrap();
                assert_eq!(
                    json!(apn_fingerprint(apn.as_ref())),
                    e["sample"],
                    "{}",
                    c.name
                );
                let report = build_config_report(
                    &ReportInput {
                        device_id: "d".into(),
                        ts: 1_786_017_600_000,
                        origin: ReportOrigin::Boot,
                        last_applied_rev: 0,
                        settings: AgentSettings::default(),
                        modem: ModemBiasSample {
                            value: Some(true),
                            fresh: true,
                            apn,
                            ..Default::default()
                        },
                        desired: None,
                        obd: None,
                        hints: None,
                        mode: None,
                        camera_mount: None,
                        telemetry_scope: None,
                        changed: None,
                    },
                    &xorgate_core::build_info::BuildInfo {
                        version: "t".into(),
                        channel: "dev-local".into(),
                        built_at: None,
                        package_version: "0.0.1".into(),
                    },
                );
                assert_eq!(
                    json!(effective_apn_fingerprint(&report.effective)),
                    e["effective"],
                    "{}",
                    c.name
                );
                assert!(!modem_seen(&report.effective));
            }
            "publishOutcome" => {
                let mut state = parse_config_sync_state(i["stateFile"].as_str());
                let s = settings(&i["settings"]);
                let res =
                    resolve_report_origin(origin(&i["origin"]), state.namespaces.as_ref(), &s);
                if res.clear_local_pending {
                    state.merge([SyncPatch::LocalPending(false)]);
                }
                assert_eq!(res.origin, origin(&e["origin"]), "{}", c.name);
                assert_eq!(
                    res.changed
                        .clone()
                        .map(Value::Object)
                        .unwrap_or(Value::Null),
                    e["changed"],
                    "{}",
                    c.name
                );
                state.merge(persist_after_publish(
                    res.origin,
                    i["online"].as_bool().unwrap(),
                    &res.current,
                    i["now"].as_i64().unwrap(),
                ));
                assert_eq!(json!(state.to_file_string()), e["stateFile"], "{}", c.name);
            }
            "appliedSummary" | "stateFile" | "bootUpgrade" => {} // agent tests
            other => panic!("unknown op {other}"),
        }
    }
    assert_eq!(n["nextOrigin"], 18);
    assert_eq!(n["needsSettleReport"], 8);
    assert_eq!(n["modemArrivalTrigger"], 5);
    assert_eq!(n["apnFingerprint"], 5);
    assert_eq!(n["publishOutcome"], 7);
}

/// `checkLocalChange` / `noteSettingsWritten` kernels (the class-level
/// behaviour is pinned by the agent's scenario tests; these are the edges).
#[test]
fn local_check_edges() {
    use xorgate_core::config_report::ConfigSyncState;
    use xorgate_core::settings::SettingsReadFailure;
    let mut st = ConfigSyncState::default();
    st.merge([SyncPatch::SettingsHash("h1".into())]);
    // Same hash: nothing (the read is never consulted).
    assert_eq!(
        check_local_change(&st, "h1", &Err(SettingsReadFailure::Unreadable), false, "f"),
        LocalCheck::Unchanged
    );
    // Unreadable: paused, warn once.
    assert_eq!(
        check_local_change(&st, "h2", &Err(SettingsReadFailure::Unreadable), false, "/s.json"),
        LocalCheck::Paused {
            warn: Some(
                "settings.json is unreadable — local-change sync paused (check owner/permissions on /s.json)"
                    .into()
            )
        }
    );
    assert_eq!(
        check_local_change(
            &st,
            "h2",
            &Err(SettingsReadFailure::Invalid),
            true,
            "/s.json"
        ),
        LocalCheck::Paused { warn: None }
    );
    // Never baselined: adopt silently (namespaces, then settingsHash).
    let s = settings(&json!({"gnssAntBias": true}));
    match check_local_change(&st, "h2", &Ok(s.clone()), false, "f") {
        LocalCheck::Persist { patch } => {
            st.merge(patch);
            assert_eq!(
                st.to_file_string(),
                r#"{"lastAppliedRev":0,"lastReportTs":0,"settingsHash":"h2","namespaces":{"gnssAntBias":true}}"#
            );
        }
        other => panic!("{other:?}"),
    }
    // Absent = `{}` = every namespace removed.
    match check_local_change(&st, "", &Err(SettingsReadFailure::Absent), false, "f") {
        LocalCheck::Changed { event, .. } => {
            assert_eq!(event, "local settings change detected: gnssAntBias")
        }
        other => panic!("{other:?}"),
    }
    // noteSettingsWritten refuses unreadable AND invalid, accepts absent.
    assert!(note_settings_written(
        &st,
        &Err(SettingsReadFailure::Invalid),
        &["gnssAntBias"],
        ""
    )
    .is_none());
    let p = note_settings_written(&st, &Err(SettingsReadFailure::Absent), &["gnssAntBias"], "")
        .unwrap();
    st.merge(p);
    assert_eq!(
        st.to_file_string(),
        r#"{"lastAppliedRev":0,"lastReportTs":0,"settingsHash":"","namespaces":{}}"#
    );
}
