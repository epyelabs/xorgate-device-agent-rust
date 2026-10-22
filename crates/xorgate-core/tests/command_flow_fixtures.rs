//! `handleCommandMessage` ORDER fixture (Phase 2 WP4, CF7/CF8):
//! `fixtures/command_flow/handle-command.json` is the trace the REAL
//! `worker.ts:902-1126` text produced under tsx (recording stubs for the
//! worker's collaborators; `phase-2/evidence/wp4-tools/dump-command-flow.mts`).
//! This test interprets [`decide_command`] + [`ledger_write_failed`] +
//! [`execution_outcome`] + the read_sms helpers with the step's injected I/O
//! results and asserts the SAME trace line for line: logs, state.json events,
//! ack payloads, ledger bytes, the refresh flag, report requests and timers,
//! the sudo argv, the sms payload, the replay exit.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::command_flow::*;
use xorgate_core::commands::{load_ledger, parse_command, CommandAck, DeviceCommand, RunResult};
use xorgate_core::sms::{
    build_sms_payload, fixture_sms_clear, fixture_sms_read, sms_result_line, SmsAction,
    SmsPayloadRequest,
};
use xorgate_core::telemetry::{ActivityLevel, RunMode};

#[derive(Deserialize)]
struct Step {
    at: i64,
    payload: String,
    #[serde(default)]
    exec: Option<RunResult>,
    #[serde(default, rename = "ledgerError")]
    ledger_error: Option<String>,
    #[serde(default)]
    puback: Option<bool>,
    trace: Vec<String>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    mode: String,
    #[serde(default, rename = "ledgerText")]
    ledger_text: Option<String>,
    steps: Vec<Step>,
    after: Vec<String>,
    #[serde(rename = "ledgerFinal")]
    ledger_final: Option<String>,
}

fn level(l: ActivityLevel) -> &'static str {
    match l {
        ActivityLevel::Info => "info",
        ActivityLevel::Warn => "warn",
        ActivityLevel::Error => "error",
    }
}

fn console(l: ConsoleLevel) -> &'static str {
    match l {
        ConsoleLevel::Info => "info",
        ConsoleLevel::Warn => "warn",
        ConsoleLevel::Error => "error",
    }
}

struct Sim<'a> {
    device: &'a str,
    mode: RunMode,
    ledger_text: Option<String>,
    last_refresh: Option<i64>,
    timers: Vec<i64>,
    trace: Vec<String>,
    /// The TS `fixtureInboxCleared` module flag (per scenario in the dump).
    fixture_cleared: bool,
}

impl Sim<'_> {
    /// `runSms` (`worker.ts:1027-1067` @ 09da38d) as the command plane runs it,
    /// with the O4 real-mode sweep (the dump's `SmsReader` stub).
    fn run_sms(&mut self, action: SmsAction, source: SmsSource, id: &str, limit: u32, at: i64) {
        let clock = xorgate_core::clock::FixedClock::new(at, 0);
        let sweep = match (action, source) {
            (SmsAction::Read, SmsSource::Fixture) => fixture_sms_read(at, self.fixture_cleared),
            (SmsAction::Clear, SmsSource::Fixture) => {
                fixture_sms_clear(at, &mut self.fixture_cleared)
            }
            (action, SmsSource::Reader) => {
                let mut s = sms_sweep(SmsSource::Reader, &clock);
                if action == SmsAction::Clear {
                    s.cleared = Some(Default::default());
                }
                s
            }
        };
        let p = build_sms_payload(
            &sweep,
            &SmsPayloadRequest {
                device_id: self.device.to_string(),
                request_id: id.to_string(),
                limit,
                action,
            },
        );
        let command = match action {
            SmsAction::Read => "read_sms",
            SmsAction::Clear => "clear_sms",
        };
        let line = sms_result_line(command, id, &p);
        self.trace.push(format!("log.info {AGENT_PREFIX}{line}"));
        self.trace
            .push(format!("event {} {line}", level(read_sms_level(&p))));
        self.trace
            .push(format!("sms {}", serde_json::to_string(&p).unwrap()));
    }

    fn fire_due(&mut self, until: i64) {
        self.timers.sort();
        while self.timers.first().is_some_and(|&d| d <= until) {
            self.timers.remove(0);
            self.trace.push("timer.fire 45000".into());
            self.trace.push("report boot".into());
        }
    }

    /// Interpret `effects` for `cmd` the way the agent's plane does.
    fn run(&mut self, cmd: Option<&DeviceCommand>, effects: Vec<Effect>, step: &Step) -> bool {
        for e in effects {
            match e {
                Effect::PersistLedger(l) => {
                    if let Some(err) = &step.ledger_error {
                        let fail = ledger_write_failed(cmd.unwrap(), err);
                        self.run(cmd, fail, step);
                        return false;
                    }
                    let text = serde_json::to_string(&l).unwrap();
                    self.trace.push(format!("ledger {text}"));
                    self.ledger_text = Some(text);
                }
                Effect::Log(c, line) => self.trace.push(format!("log.{} {line}", console(c))),
                Effect::Event(l, msg) => self.trace.push(format!("event {} {msg}", level(l))),
                Effect::Ack { status, reason } => {
                    let ack = CommandAck::new(self.device, cmd.unwrap(), status, reason);
                    self.trace
                        .push(format!("ack {}", serde_json::to_string(&ack).unwrap()));
                    if step.puback == Some(false) {
                        self.trace.push("ack.timeout 3000".into());
                    }
                }
                Effect::WriteRefreshFlag(c) => {
                    self.trace.push(format!("flag refresh-modem.flag {c}"))
                }
                Effect::RequestReport => self.trace.push("report boot".into()),
                Effect::ScheduleReport { after_ms } => {
                    self.trace.push(format!("timer {after_ms}"));
                    self.timers.push(step.at + after_ms);
                }
                Effect::ReadSms {
                    source,
                    request_id,
                    limit,
                } => self.run_sms(SmsAction::Read, source, &request_id, limit, step.at),
                Effect::ClearSms {
                    source,
                    request_id,
                    limit,
                } => self.run_sms(SmsAction::Clear, source, &request_id, limit, step.at),
                Effect::Execute(call) => {
                    self.trace.push(format!("exec {}", call.argv().join(" ")));
                    let res = step.exec.as_ref().expect("exec result");
                    let out = execution_outcome(cmd.unwrap().command, res);
                    self.run(cmd, out, step);
                }
                Effect::ContainerExit { reason, code } => {
                    self.trace.push(format!("shutdown {reason} {code}"))
                }
                // obd_* WITH the collector: not in this fixture (no peripheral).
                Effect::ObdJob(_) => unreachable!("no obd job without a peripheral"),
            }
        }
        true
    }
}

#[test]
fn handle_command_message_trace_matches_worker_ts() {
    let f = common::fixture::<Case>("command_flow/handle-command.json");
    let device = f.extra["deviceId"].as_str().unwrap().to_string();
    // Re-dumped at 09da38d (Phase 5b WP3): the same section, moved.
    assert_eq!(f.extra["jsLines"], Value::from("1005-1292"));
    assert_eq!(f.js_commit, common::JS_COMMIT_P5B);
    let mut steps = 0;
    for c in &f.cases {
        let mode = RunMode::parse(&c.mode).unwrap();
        let mut sim = Sim {
            device: &device,
            mode,
            ledger_text: c.ledger_text.clone(),
            last_refresh: None,
            timers: vec![],
            trace: vec![],
            fixture_cleared: false,
        };
        for (i, s) in c.steps.iter().enumerate() {
            sim.trace.clear();
            sim.fire_due(s.at);
            let parsed = parse_command(s.payload.as_bytes());
            let ledger = load_ledger(sim.ledger_text.as_deref());
            let d = decide_command(
                parsed.as_ref(),
                s.at,
                &ledger,
                RefreshWindow {
                    now: s.at,
                    last: sim.last_refresh,
                },
                sim.mode,
                false,
            );
            if sim.run(parsed.as_ref(), d.effects, s) {
                if let Some(m) = d.refresh_mark {
                    sim.last_refresh = Some(m);
                }
            }
            assert_eq!(sim.trace, s.trace, "{} — step {i}", c.name);
            steps += 1;
        }
        sim.trace.clear();
        sim.fire_due(i64::MAX);
        assert_eq!(sim.trace, c.after, "{} — after", c.name);
        assert_eq!(sim.ledger_text, c.ledger_final, "{} — ledger", c.name);
    }
    assert_eq!((f.cases.len(), steps), (24, 52));
}

#[test]
fn decision_serialisation_cases() {
    // One case per new public shape the agent consumes: the O4 sweep and its
    // payload/line (the `…/sms` bytes the cloud stores).
    let sweep = sms_reader_unavailable_sweep(1_750_000_001_000, 0);
    assert_eq!(
        serde_json::to_string(&sweep).unwrap(),
        r#"{"status":"error","error":"sms reader not available in this build","finishedAt":1750000001000,"durationMs":0,"storage":{},"skipped":0,"partial":false,"messages":[]}"#
    );
    let p = build_sms_payload(
        &sweep,
        &SmsPayloadRequest {
            device_id: "d".into(),
            request_id: "r".into(),
            limit: 10,
            action: xorgate_core::sms::SmsAction::Read,
        },
    );
    assert_eq!(
        read_sms_line("r", &p),
        "read_sms r: status=error total=0 sent=0 unread=0 skipped=0 partial=false durationMs=0 error=sms reader not available in this build"
    );
    assert_eq!(read_sms_level(&p), ActivityLevel::Warn);
    assert_eq!(
        read_sms_publish_failed_log("r", "x"),
        "[agent] read_sms r: publish failed: x"
    );
    assert_eq!(
        refresh_flag_failed_log("EACCES: permission denied, open '/h/refresh-modem.flag'"),
        "[agent] refresh flag write failed: EACCES: permission denied, open '/h/refresh-modem.flag'"
    );
}
