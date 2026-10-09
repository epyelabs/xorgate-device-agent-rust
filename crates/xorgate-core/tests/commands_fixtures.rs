//! Fixture-driven tests for `xorgate_core::commands`, from `test/commands.test.ts`
//! (20 `it()`: parse 8 + isExpired 2 + exit codes 1 as P tables; claim 5 as
//! ledger scripts replayed on the file-backed `claimCommand`; execute 4 as
//! argv/plan tables) plus the worker's ack byte-shape and failure-reason
//! wording (`worker.ts:953-968, 1118-1120`) — 83 cases with extras.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::commands::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    #[serde(rename = "expectedJson")]
    expected_json: String,
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

/// `JSON.stringify(JSON.parse(text))`, or `JSON.stringify(text)` when the
/// file is not JSON (the extractor's fallback for a corrupt ledger).
fn stringify_file(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => json(&v),
        Err(_) => json(&text),
    }
}

#[test]
fn commands_test_ts() {
    let f = common::fixture::<Case>("commands/commands.json");
    assert_eq!(f.extra["MAX_TTL_MS"], MAX_TTL_MS);
    let mut ops = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *ops.entry(c.op.clone()).or_default() += 1;
        let got = match c.op.as_str() {
            "parseCommand" => {
                let cmd = parse_command(c.input.as_str().unwrap().as_bytes());
                if let Some(cmd) = &cmd {
                    let again: DeviceCommand = serde_json::from_str(&json(cmd)).unwrap();
                    assert_eq!(&again, cmd, "{}: round trip", c.name);
                }
                json(&cmd)
            }
            "isExpired" => {
                let cmd = parse_command(c.input["payload"].as_str().unwrap().as_bytes()).unwrap();
                json(&is_expired(&cmd, c.input["now"].as_i64().unwrap()))
            }
            "claim" => {
                #[derive(Deserialize)]
                struct Step {
                    #[serde(rename = "requestId")]
                    request_id: String,
                    now: i64,
                }
                let text = c.input["ledgerText"].as_str().map(str::to_string);
                let steps: Vec<Step> = serde_json::from_value(c.input["steps"].clone()).unwrap();
                // `persisted` mirrors the file on disk: the TS writes only on
                // an accepted claim and re-reads the file on every call.
                let mut persisted: Option<String> = text;
                let mut results = Vec::new();
                for (i, s) in steps.iter().enumerate() {
                    let ledger = load_ledger(persisted.as_deref());
                    let out = claim(&ledger, &s.request_id, s.now);
                    if out.accepted {
                        let again: Ledger = serde_json::from_str(&json(&out.ledger)).unwrap();
                        assert_eq!(again, out.ledger, "{}: ledger round trip", c.name);
                        persisted = Some(json(&out.ledger));
                    } else {
                        assert_eq!(
                            out.ledger, ledger,
                            "{}: duplicate leaves the ledger alone",
                            c.name
                        );
                    }
                    let file = persisted.clone().unwrap_or_default();
                    let executed_length = serde_json::from_str::<Value>(&file)
                        .ok()
                        .and_then(|v| v.get("executed")?.as_array().map(Vec::len))
                        .map_or(-1, |n| n as i64);
                    let mut r = serde_json::Map::new();
                    r.insert("accepted".into(), Value::Bool(out.accepted));
                    r.insert("executedLength".into(), Value::from(executed_length));
                    if !(steps.len() > 10 && i < steps.len() - 1) {
                        r.insert("ledgerJson".into(), Value::String(stringify_file(&file)));
                    }
                    results.push(Value::Object(r));
                }
                json(&results)
            }
            "plan" => {
                let name = DeviceCommandName::parse(c.input.as_str().unwrap()).unwrap();
                match plan(name) {
                    CommandPlan::Privileged(call) => json(&serde_json::json!({
                        "kind": "privileged",
                        "calls": [[call.program(), call.args()]],
                    })),
                    CommandPlan::InProcess(cmd) => {
                        assert_eq!(cmd, name);
                        assert!(name.is_in_process());
                        json(&serde_json::json!({
                            "kind": "in-process",
                            "result": in_process_run_result(cmd),
                        }))
                    }
                }
            }
            "failureReason" => {
                let res: RunResult = serde_json::from_value(c.input.clone()).unwrap();
                json(&failure_reason(&res))
            }
            "containerExitCode" => {
                let name = DeviceCommandName::parse(c.input.as_str().unwrap()).unwrap();
                json(&container_exit_code(name).unwrap())
            }
            "replayExitCodes" => json(&REPLAY_EXIT_CODES),
            "commandAck" => {
                let cmd = parse_command(c.input["payload"].as_str().unwrap().as_bytes()).unwrap();
                let status: AckStatus = serde_json::from_value(c.input["status"].clone()).unwrap();
                let reason = c.input["reason"].as_str().map(str::to_string);
                let ack = match (status, reason.as_deref()) {
                    (AckStatus::Accepted, None) => CommandAck::accepted("dev-1", &cmd),
                    (AckStatus::Rejected, Some(r)) => {
                        let reason: RejectReason =
                            serde_json::from_value(Value::String(r.into())).unwrap();
                        assert_eq!(reason.as_str(), r);
                        CommandAck::rejected("dev-1", &cmd, reason)
                    }
                    (AckStatus::Failed, Some(r)) => {
                        CommandAck::failed("dev-1", &cmd, r.to_string())
                    }
                    _ => CommandAck::new("dev-1", &cmd, status, reason.clone()),
                };
                // The spread keeps the COMMAND's ts, not the ack time.
                assert_eq!(ack.ts, cmd.ts);
                assert_ne!(ack.ts, c.input["nowTs"].as_f64().unwrap());
                let again: CommandAck = serde_json::from_str(&json(&ack)).unwrap();
                assert_eq!(again, ack, "{}: round trip", c.name);
                json(&ack)
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected_json, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 83);
    assert_eq!(ops["parseCommand"], 44);
    assert_eq!(ops["isExpired"], 5);
    assert_eq!(ops["claim"], 13);
    assert_eq!(ops["plan"], 5);
    assert_eq!(ops["failureReason"], 5);
    assert_eq!(ops["containerExitCode"], 2);
    assert_eq!(ops["replayExitCodes"], 1);
    assert_eq!(ops["commandAck"], 8);
}
