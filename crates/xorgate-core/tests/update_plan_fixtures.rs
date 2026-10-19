//! Fixture-driven tests for `xorgate_core::update_plan` (Phase 4 WP4) against
//! `fixtures/update_plan/update.json`, dumped by
//! `tools/replay/js/dump-update-plan.mts` from the REAL `src/commands/update.ts`
//! @ 89b5c64 (`parseEnvFile`, `serviceUser`, the three `install*Sudoers`, and
//! the uid / resolution / download-failure half of `updateCommand`).
//!
//! Where the Rust rail deliberately differs (Q9 operator hints, the `-rs`
//! artifact, the `024` grant replacing `022`) the test applies exactly the
//! documented substitution to the TS output and compares the rest verbatim.

mod common;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;
use xorgate_core::update_plan::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

const TS_PREFIX: &str = "[xorgate] ";
const TS_FAIL: &str = "[xorgate] update failed: ";

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// The TS `022` body → the Rust `024` body (O3/Q11): the three documented
/// substitutions, nothing else.
fn ts_022_to_024(ts: &str) -> String {
    ts.replacen("# Installed by xorgate. ", "# Installed by xorgate-rs. ", 1)
        .replacen(
            "# else. See device-agent src/agent/commands.ts + systemd/xorgate-updater.service.\n",
            "# else. See device-agent-rust crates/xorgate-hal/src/privileged.rs + systemd/xorgate-updater-rs.service.\n",
            1,
        )
        .replacen(
            " --no-block xorgate-updater.service\n",
            " --no-block xorgate-updater-rs.service\n",
            1,
        )
}

#[test]
fn update_ts() {
    let f = common::fixture::<Case>("update_plan/update.json");
    let mut counts: IndexMap<String, usize> = IndexMap::new();
    for c in &f.cases {
        *counts.entry(c.op.clone()).or_default() += 1;
        match c.op.as_str() {
            "parseEnvFile" => {
                let text = c.input.as_str().unwrap_or("");
                let got = parse_env_file(text);
                let want: IndexMap<String, String> =
                    serde_json::from_value(c.expected.clone()).unwrap();
                assert_eq!(
                    got.iter().collect::<Vec<_>>(),
                    want.iter().collect::<Vec<_>>(),
                    "{}",
                    c.name
                );
            }
            "serviceUser" => {
                let got = c.input.as_str().and_then(parse_service_user);
                assert_eq!(got, s(&c.expected), "{}", c.name);
            }
            "sudoers" => {
                let grant = match c.input["grant"].as_str().unwrap() {
                    "clock" => SudoersGrant::Clock,
                    "camrotate" => SudoersGrant::CamRotate,
                    "update" => SudoersGrant::UpdateRs,
                    g => panic!("{g}"),
                };
                let user = s(&c.input["user"]);
                let visudo_ok = c.input["visudoExit"] == 0;
                let ts_body = s(&c.expected["body"]);
                let ts_logs: Vec<String> =
                    serde_json::from_value(c.expected["logs"].clone()).unwrap();
                match grant_user(user.as_deref()) {
                    None => {
                        assert_eq!(ts_body, None, "{}", c.name);
                        assert!(ts_logs.is_empty(), "{}", c.name);
                    }
                    Some(u) if visudo_ok => {
                        let ts_body = ts_body.expect("TS wrote the grant");
                        let want = if grant == SudoersGrant::UpdateRs {
                            ts_022_to_024(&ts_body)
                        } else {
                            ts_body
                        };
                        assert_eq!(grant.body(u), want, "{}", c.name);
                        assert!(ts_logs.is_empty(), "{}", c.name);
                    }
                    Some(_) => {
                        assert_eq!(ts_body, None, "{}", c.name);
                        assert_eq!(
                            ts_logs,
                            [format!("{TS_PREFIX}{}", grant.refused_warning())],
                            "{}",
                            c.name
                        );
                    }
                }
            }
            "updateCommand" => check_update_command(c),
            op => panic!("unknown op {op}"),
        }
    }
    assert_eq!(counts.get("parseEnvFile"), Some(&24));
    assert_eq!(counts.get("serviceUser"), Some(&12));
    assert_eq!(counts.get("sudoers"), Some(&24));
    assert_eq!(counts.get("updateCommand"), Some(&16));
}

fn check_update_command(c: &Case) {
    let inp = &c.input;
    let exp = &c.expected;
    let err: Vec<String> = serde_json::from_value(exp["err"].clone()).unwrap();
    let out: Vec<String> = serde_json::from_value(exp["out"].clone()).unwrap();
    assert_eq!(exp["exit"], 1, "{}: every dumped run fails", c.name);
    assert_eq!(err.len(), 1, "{}", c.name);
    let ts_fail = err[0].strip_prefix(TS_FAIL).expect("TS fail prefix");

    if inp["uid"] != 0 {
        assert_eq!(
            NOT_ROOT_MESSAGE,
            ts_fail.replacen("sudo xorgate update", "sudo xorgate-rs update", 1),
            "{}",
            c.name
        );
        return;
    }

    let argv: Vec<String> = serde_json::from_value(inp["argv"].clone()).unwrap();
    let env: IndexMap<String, String> = serde_json::from_value(inp["env"].clone()).unwrap();
    let file = inp["agentEnv"]
        .as_str()
        .map(parse_env_file)
        .unwrap_or_default();
    let got = resolve_source(
        flag_value(&argv, "--from"),
        flag_value(&argv, "--tag"),
        &|k| env.get(k).cloned(),
        &file,
    );
    match got {
        Err(msg) => {
            assert_eq!(msg, ts_fail, "{}", c.name);
            assert!(out.is_empty(), "{}", c.name);
            assert_eq!(exp["curlArgv"], Value::Null, "{}", c.name);
        }
        Ok(src) => {
            // The TS URL names the JS artifact; the resolved bucket + version
            // are what both rails share.
            let ts_url = format!("{}/xorgate-agent-{}.tar.gz", src.bucket_url, src.version);
            assert_eq!(
                out,
                [format!("{TS_PREFIX}downloading {ts_url}")],
                "{}",
                c.name
            );
            let curl: Vec<String> = serde_json::from_value(exp["curlArgv"].clone()).unwrap();
            assert_eq!(curl, curl_args(&ts_url, "<tgz>"), "{}", c.name);
            let code = inp["curlExit"].as_i64().map(|c| c as i32);
            assert_eq!(
                download_failed_message(inp["curlStderr"].as_str().unwrap(), code),
                ts_fail,
                "{}",
                c.name
            );
            // The Rust artifact for the same resolution.
            assert_eq!(
                artifact_url(&src, &tarball_name(&src.version)),
                format!("{}/xorgate-agent-rs-{}.tar.gz", src.bucket_url, src.version)
            );
        }
    }
}
