//! Phase 4 WP2 — `ops::sms::SmsReader` (`sms-modem.ts:472-541`), the
//! `command_plane::SmsSource::Real` arm (`worker.ts:924-945`) and the `sms`
//! CLI rendering (`commands/sms.ts`), against fixtures dumped by tsx from the
//! REAL TS @ 89b5c64:
//! - `fixtures/sms_reader/sms-modem-io.json` (`dump-sms-reader.mts`): the
//!   three `SmsReader` its + one extra, and the `reader` half of every sweep
//!   case (the whole SmsReader path over the same FakeModem, fixed clock);
//! - `fixtures/sms_cli/sms-cli.json` (`dump-sms-cli.mts`): stdout + exit code
//!   of `smsCommand` for 25 (sweep, argv) pairs.

#[path = "../../xorgate-hal/tests/support/fake_modem.rs"]
mod fake_modem;

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use fake_modem::{FakeModem, FakeSpec, MemTransport, TestClock};
use serde_json::Value;
use xorgate_agent::command_plane::{start_with_sms, SmsSource};
use xorgate_agent::ctx::{AgentClock, CommandPlaneDeps, NoopHooks, ReportRequester};
use xorgate_agent::env::AgentEnv;
use xorgate_agent::mqtt_plane::MqttHandle;
use xorgate_agent::ops::sms::{OpenTransportFn, SmsReader, SmsReaderDeps};
use xorgate_cloud::mqtt::ConnectionState as LinkState;
use xorgate_core::sms::{build_sms_payload, SmsPayloadRequest, SmsReadStatus, SmsSweep};
use xorgate_core::sms_cli::{exit_code, render_json, render_table, SmsCliArgs, SMS_HELP};
use xorgate_core::telemetry::RunMode;
use xorgate_hal::at_port::{AtTransport, SmsReadError};
use xorgate_hal::io::null_tape;
use xorgate_hal::privileged::RecordingRunner;

fn load(rel: &str) -> Value {
    let p = format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn io_cases(kind: &str) -> Vec<Value> {
    load("sms_reader/sms-modem-io.json")["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == kind)
        .cloned()
        .collect()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

/// A Rust sweep vs the TS sweep object: same fields (order-free), and the
/// `storage` key order exactly (wire bytes).
fn assert_sweep(got: &SmsSweep, want: &Value, name: &str) {
    assert_eq!(serde_json::to_value(got).unwrap(), *want, "{name}");
    assert_eq!(
        serde_json::to_string(&got.storage).unwrap(),
        serde_json::to_string(&want["storage"]).unwrap(),
        "{name}: storage order"
    );
}

/// Every opened transport's modem + closed flag, in open order.
type Opened = Arc<Mutex<Vec<(Arc<Mutex<FakeModem>>, Arc<std::sync::atomic::AtomicBool>)>>>;

fn opener(spec: FakeSpec, tick: Option<(Arc<TestClock>, i64)>, opened: Opened) -> OpenTransportFn {
    Box::new(move |_port: PathBuf| {
        let modem = Arc::new(Mutex::new(FakeModem::new(spec.clone())));
        let t = MemTransport::new(modem.clone(), tick.clone());
        opened.lock().unwrap().push((modem, t.closed.clone()));
        Box::pin(async move { Ok(Box::new(t) as Box<dyn AtTransport>) })
    })
}

// ---------------------------------------------------------------------------
// SmsReader (`sms-modem.test.ts:224-271`)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn joins_concurrent_callers_onto_one_sweep_and_reuses_it_inside_the_window() {
    let c = io_cases("reader_join").remove(0);
    let spec: FakeSpec = serde_json::from_value(c["input"]["fake"].clone()).unwrap();
    let clock = TestClock::new(c["input"]["t0"].as_i64().unwrap());
    let opened: Opened = Default::default();
    let reader = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/fake"))),
        opener(spec, None, opened.clone()),
        clock.clone(),
    ));
    let steps = c["expected"]["steps"].as_array().unwrap();
    let (a, b) = tokio::join!(reader.read(), reader.read());
    assert_eq!(opened.lock().unwrap().len(), 1);
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(steps[0]["opens"], 1);
    assert_eq!(steps[0]["sameAsFirst"], true);
    assert_sweep(&a, &steps[0]["sweep"], "first sweep");
    clock.advance(4000);
    let a2 = reader.read().await;
    assert!(Arc::ptr_eq(&a2, &a), "reused inside the window");
    assert_eq!(opened.lock().unwrap().len(), 1);
    assert_eq!(steps[1]["sameAsFirst"], true);
    clock.advance(2000);
    let c2 = reader.read().await;
    assert_eq!(opened.lock().unwrap().len(), 2);
    assert!(!Arc::ptr_eq(&c2, &a));
    assert_eq!(c2.status, SmsReadStatus::Ok);
    assert_eq!(steps[2]["opens"], 2);
    assert_eq!(steps[2]["sameAsFirst"], false);
    assert_sweep(&c2, &steps[2]["sweep"], "second sweep");
    let logs = c["expected"]["logs"].as_array().unwrap();
    for (i, (m, closed)) in opened.lock().unwrap().iter().enumerate() {
        assert_eq!(m.lock().unwrap().log, strings(&logs[i]), "open {i} log");
        assert!(closed.load(Ordering::SeqCst), "open {i} closed");
    }
}

#[tokio::test(start_paused = true)]
async fn reports_no_modem_when_there_is_no_port_and_closes_the_transport_on_failure() {
    let c = io_cases("reader_fail").remove(0);
    let now = c["input"]["now"].as_i64().unwrap();
    let opens = Arc::new(AtomicUsize::new(0));
    let o2 = opens.clone();
    let none = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| None),
        Box::new(move |_| {
            o2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(SmsReadError::new(SmsReadStatus::Error, "unreachable")) })
        }),
        TestClock::new(now),
    ))
    .read()
    .await;
    assert_sweep(&none, &c["expected"]["noPort"], "no port");
    assert_eq!(opens.load(Ordering::SeqCst), 0);

    let spec: FakeSpec = serde_json::from_value(c["input"]["fake"].clone()).unwrap();
    let opened: Opened = Default::default();
    let r = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/x"))),
        opener(spec, None, opened.clone()),
        TestClock::new(now),
    ))
    .read()
    .await;
    assert_sweep(&r, &c["expected"]["failed"], "no_sim");
    let (m, closed) = opened.lock().unwrap()[0].clone();
    assert!(closed.load(Ordering::SeqCst));
    assert_eq!(c["expected"]["closed"], true);
    assert_eq!(m.lock().unwrap().log, strings(&c["expected"]["log"]));
}

#[tokio::test(start_paused = true)]
async fn reports_modem_busy_when_another_process_holds_the_lock() {
    let c = io_cases("reader_locked").remove(0);
    let home = scratch("sms-lock-held");
    let lock = home.join("sms.lock");
    std::fs::write(&lock, std::process::id().to_string()).unwrap(); // a live pid: ours
    let opened: Opened = Default::default();
    let mut deps = SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/x"))),
        opener(FakeSpec::default(), None, opened.clone()),
        TestClock::new(c["input"]["now"].as_i64().unwrap()),
    );
    deps.lock_path = Some(lock.clone());
    let r = SmsReader::new(deps).read().await;
    assert_sweep(&r, &c["expected"]["sweep"], "locked");
    assert!(opened.lock().unwrap().is_empty());
    assert_eq!(c["expected"]["opened"], false);
    assert!(lock.exists(), "someone else's lock is left alone");
}

#[tokio::test(start_paused = true)]
async fn an_open_failure_maps_its_status_and_releases_the_lock() {
    let c = io_cases("reader_open_fails").remove(0);
    let home = scratch("sms-open-fails");
    let lock = home.join("sms.lock");
    let e = &c["input"]["openError"];
    let err = SmsReadError::new(
        serde_json::from_value(e["status"].clone()).unwrap(),
        e["message"].as_str().unwrap(),
    );
    let mut deps = SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/x"))),
        Box::new(move |_| {
            let err = err.clone();
            Box::pin(async move { Err(err) })
        }),
        TestClock::new(c["input"]["now"].as_i64().unwrap()),
    );
    deps.lock_path = Some(lock.clone());
    let r = SmsReader::new(deps).read().await;
    assert_sweep(&r, &c["expected"]["sweep"], "open fails");
    assert_eq!(lock.exists(), c["expected"]["lockLeft"].as_bool().unwrap());
}

/// The `reader` half of every sweep case: the same FakeModem through the
/// whole `SmsReader` path (fixed clock, or the budget test's ticking one).
#[tokio::test(start_paused = true)]
async fn every_sweep_case_through_the_reader() {
    let cs = io_cases("sweep");
    assert_eq!(cs.len(), 22);
    for c in cs {
        let name = c["name"].as_str().unwrap();
        let input = &c["input"];
        let spec: FakeSpec = serde_json::from_value(input["fake"].clone()).unwrap();
        let clock = match input["readerNow"].as_i64() {
            Some(t) => TestClock::new(t),
            None => TestClock::new(spec.t0.unwrap_or(0)),
        };
        let tick = spec.tick_ms.map(|ms| (clock.clone(), ms));
        let opened: Opened = Default::default();
        let mut deps = SmsReaderDeps::with(
            Box::new(|| Some(PathBuf::from("/dev/fake"))),
            opener(spec, tick, opened.clone()),
            clock.clone(),
        );
        if let Some(ms) = input["opts"]["commandTimeoutMs"].as_u64() {
            deps.command_timeout_ms = ms;
        }
        if let Some(ms) = input["opts"]["budgetMs"].as_u64() {
            deps.budget_ms = ms;
        }
        let r = SmsReader::new(deps).read().await;
        let want = &c["expected"]["reader"];
        assert_sweep(&r, &want["sweep"], name);
        let (m, closed) = opened.lock().unwrap()[0].clone();
        assert_eq!(m.lock().unwrap().log, strings(&want["log"]), "{name}: log");
        assert!(closed.load(Ordering::SeqCst), "{name}: closed");
        assert_eq!(want["closed"], true);
    }
}

/// A dropped caller does not cancel the sweep (a TS promise keeps running):
/// the next caller still joins / reuses its result, and the modem is
/// restored and closed.
#[tokio::test(start_paused = true)]
async fn a_dropped_caller_does_not_cancel_the_sweep() {
    let c = io_cases("sweep").remove(0);
    let spec: FakeSpec = serde_json::from_value(c["input"]["fake"].clone()).unwrap();
    let opened: Opened = Default::default();
    let clock = TestClock::new(1_000);
    let reader = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/fake"))),
        opener(spec, None, opened.clone()),
        clock,
    ));
    drop(reader.read());
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let r = reader.read().await;
    assert_eq!(
        opened.lock().unwrap().len(),
        1,
        "joined or reused, not re-run"
    );
    assert_eq!(r.messages.len(), 30);
    let (m, closed) = opened.lock().unwrap()[0].clone();
    assert_eq!(m.lock().unwrap().mem1, "SR");
    assert!(closed.load(Ordering::SeqCst));
}

/// Review WP2 S1: a panic inside the sweep task must not wedge the reader.
/// The TS clears `inflight` in `.finally` (`sms-modem.ts:498-500`) however
/// the sweep ends; Rust clears it from a drop guard. The panicking call's
/// joiners get the catch-all `error "unknown"`, and the NEXT `read()` runs a
/// fresh sweep against the modem (it does not join the dead one).
#[tokio::test(start_paused = true)]
async fn a_panicking_sweep_clears_inflight_and_the_next_read_runs_a_fresh_sweep() {
    let c = io_cases("sweep").remove(0);
    let spec: FakeSpec = serde_json::from_value(c["input"]["fake"].clone()).unwrap();
    let opened: Opened = Default::default();
    let good = opener(spec, None, opened.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let open: OpenTransportFn = Box::new(move |p: PathBuf| {
        if calls2.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("injected open panic (S1 test)");
        }
        good(p)
    });
    let reader = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/fake"))),
        open,
        TestClock::new(1_000),
    ));
    let (a, b) = tokio::join!(reader.read(), reader.read());
    assert_eq!(a.status, SmsReadStatus::Error);
    assert_eq!(a.error.as_deref(), Some("unknown"));
    assert_eq!(
        b.status,
        SmsReadStatus::Error,
        "the joiner sees the same end"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(reader.last().is_none(), "no finish() ran: nothing to reuse");
    // Same mono instant (inside any reuse window): still a fresh sweep.
    let r = reader.read().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a fresh sweep, not the dead one"
    );
    assert_eq!(r.status, SmsReadStatus::Ok);
    assert_eq!(r.messages.len(), 30);
    let (m, closed) = opened.lock().unwrap()[0].clone();
    assert_eq!(m.lock().unwrap().mem1, "SR");
    assert!(closed.load(Ordering::SeqCst));
}

/// A clock whose wall and monotonic halves move independently (a stepper
/// `settimeofday` moves only the wall).
struct SplitClock {
    wall: std::sync::atomic::AtomicI64,
    mono: std::sync::atomic::AtomicU64,
}

impl xorgate_core::clock::Clock for SplitClock {
    fn wall_ms(&self) -> i64 {
        self.wall.load(Ordering::SeqCst)
    }
    fn mono_ms(&self) -> u64 {
        self.mono.load(Ordering::SeqCst)
    }
}

/// Accepted divergence D32: the TS gates the 5 s reuse window on
/// `Date.now()` (`sms-modem.ts:495-497`, `finishedAt` from `:510-518`); Rust
/// gates it on the monotonic clock (invariant 2). A BACKWARD wall step of an
/// hour would make the TS reuse the stale sweep for that hour (`now -
/// finishedAt` negative < 5000); Rust re-sweeps once 5 s of mono pass. A
/// FORWARD wall step would make the TS re-sweep at once; Rust reuses inside
/// 5 s of mono. `finishedAt` stays a wall stamp, `durationMs` mono.
#[tokio::test(start_paused = true)]
async fn d32_reuse_window_runs_on_mono_across_wall_steps() {
    let c = io_cases("sweep").remove(0);
    let spec: FakeSpec = serde_json::from_value(c["input"]["fake"].clone()).unwrap();
    let opened: Opened = Default::default();
    let t0 = 1_760_000_000_000_i64;
    let clock = Arc::new(SplitClock {
        wall: std::sync::atomic::AtomicI64::new(t0),
        mono: std::sync::atomic::AtomicU64::new(50_000),
    });
    let reader = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/fake"))),
        opener(spec, None, opened.clone()),
        clock.clone(),
    ));
    let first = reader.read().await;
    assert_eq!(first.finished_at, t0, "finishedAt is the wall stamp");
    assert_eq!(first.duration_ms, 0, "durationMs on mono (no mono elapsed)");
    assert_eq!(opened.lock().unwrap().len(), 1);

    // Forward wall step of an hour, 1 s of mono: the TS would re-sweep
    // (wall gap 3 601 000 ≥ 5000); Rust reuses (mono gap 1000 < 5000).
    clock.wall.fetch_add(3_600_000, Ordering::SeqCst);
    clock.mono.fetch_add(1_000, Ordering::SeqCst);
    let fwd = reader.read().await;
    assert!(Arc::ptr_eq(&fwd, &first), "forward step: reused on mono");
    assert_eq!(opened.lock().unwrap().len(), 1);

    // Backward wall step of two hours (now an hour BEFORE finishedAt), 5 s
    // more of mono: the TS would reuse (wall gap −3 594 000 < 5000) until
    // wall time catches up; Rust re-sweeps (mono gap 6000 ≥ 5000).
    clock.wall.fetch_sub(7_200_000, Ordering::SeqCst);
    clock.mono.fetch_add(5_000, Ordering::SeqCst);
    let back = reader.read().await;
    assert!(
        !Arc::ptr_eq(&back, &first),
        "backward step: fresh sweep on mono"
    );
    assert_eq!(opened.lock().unwrap().len(), 2);
    assert_eq!(back.status, SmsReadStatus::Ok);
    assert_eq!(back.finished_at, t0 - 3_600_000, "the stepped wall stamp");
}

// ---------------------------------------------------------------------------
// command_plane: real mode → the reader (`worker.ts:924-945`, CF48 variant)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn real_mode_read_sms_publishes_the_readers_sweep() {
    let home = scratch("sms-plane-real");
    let mut ctx = test_ctx(&home, "dev-sms", Arc::new(NoopHooks), null_tape());
    let mut env: AgentEnv = (*ctx.env).clone();
    env.config.mode = RunMode::Real;
    ctx.env = Arc::new(env);
    let t0 = 1_760_000_000_000;
    ctx.clock = Arc::new(AgentClock::following_tokio(t0));
    let link = FakeLink::new(Default::default());
    link.set(LinkState::Connected);
    let (reports, _rx) = ReportRequester::channel();

    let c = io_cases("sweep").remove(0); // the bench inbox
    let spec: FakeSpec = serde_json::from_value(c["input"]["fake"].clone()).unwrap();
    let opened: Opened = Default::default();
    let reader = SmsReader::new(SmsReaderDeps::with(
        Box::new(|| Some(PathBuf::from("/dev/fake"))),
        opener(spec, None, opened.clone()),
        ctx.clock.clone(),
    ));
    let plane = start_with_sms(
        &ctx,
        CommandPlaneDeps {
            mqtt: MqttHandle::new(link.clone()),
            reports,
            obd: None,
        },
        Arc::new(RecordingRunner::default()),
        SmsSource::Real(reader.clone()),
    );
    let payload = format!(
        r#"{{"v":1,"requestId":"req-real","command":"read_sms","ts":{t0},"expiresAt":{},"args":{{"limit":2}}}}"#,
        t0 + 60_000
    );
    plane.on_message(payload.into_bytes());
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
    assert_eq!(opened.lock().unwrap().len(), 1, "the reader ran once");
    let pubs = link.publishes.lock().unwrap().clone();
    let sms: Vec<_> = pubs
        .iter()
        .filter(|(t, _, _)| t.ends_with("/sms"))
        .collect();
    assert_eq!(sms.len(), 1);
    let sweep = reader.last().expect("a finished sweep");
    let want = build_sms_payload(
        &sweep,
        &SmsPayloadRequest {
            device_id: "dev-sms".into(),
            request_id: "req-real".into(),
            limit: 2,
            action: xorgate_core::sms::SmsAction::Read,
        },
    );
    assert_eq!(sms[0].1, serde_json::to_vec(&want).unwrap());
    assert_eq!(want.total, 30);
    assert_eq!(want.messages.len(), 2);
    assert!(state_events(&ctx).iter().any(|e| e.contains(
        "read_sms req-real: status=ok total=30 sent=2 unread=2 skipped=0 partial=false durationMs="
    )));
}

// ---------------------------------------------------------------------------
// `xorgate-rs sms` rendering (`commands/sms.ts:33-93`)
// ---------------------------------------------------------------------------

#[test]
fn sms_cli_goldens() {
    use xorgate_core::sms_cli::{
        clear_gate, clear_rest, confirm_answer, render_clear_line, sms_clear_help, ClearGate,
        SmsClearArgs, SMS_CLEAR_CANCELLED, SMS_CLEAR_PROMPT, SMS_CLEAR_REFUSED,
    };
    let f = load("sms_cli/sms-cli.json");
    assert_eq!(f["jsCommit"], "09da38dcc8b75c248625a3a0bee43b70e7def55d");
    let sweeps = f["sweeps"].as_object().unwrap();
    let cases = f["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 43);
    let mut clear_cases = 0;
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let input = &c["input"];
        let want = &c["expected"];
        let sweep: SmsSweep =
            serde_json::from_value(sweeps[input["sweep"].as_str().unwrap()].clone()).unwrap();
        let argv = strings(&input["argv"]);
        // `xorgate-rs sms clear …` (`sms.ts:50-80` @ 09da38d), as `cli/sms.rs`
        // drives it: the gate, the prompt, the clear, the line.
        if let Some(rest) = clear_rest(&argv) {
            clear_cases += 1;
            let tty = input.get("ttyAnswer").and_then(Value::as_str);
            let (mut out, mut err, mut prompt, mut cleared) = (vec![], vec![], String::new(), 0);
            let mut proceed = false;
            let code = match clear_gate(SmsClearArgs::parse(rest), tty.is_some()) {
                ClearGate::Help => {
                    // D34: the usage names this binary.
                    out.push(
                        sms_clear_help(xorgate_hal::at_clear::delete_all_text())
                            .replace("xorgate-rs sms", "xorgate sms"),
                    );
                    0
                }
                ClearGate::Refuse => {
                    err.push(SMS_CLEAR_REFUSED.to_string());
                    1
                }
                ClearGate::Ask => {
                    prompt = SMS_CLEAR_PROMPT.to_string();
                    if confirm_answer(tty.unwrap()) {
                        proceed = true;
                        0
                    } else {
                        out.push(SMS_CLEAR_CANCELLED.to_string());
                        0
                    }
                }
                ClearGate::Proceed => {
                    proceed = true;
                    0
                }
            };
            let code = if proceed {
                cleared = 1;
                let p = build_sms_payload(
                    &sweep,
                    &SmsPayloadRequest {
                        device_id: "local".into(),
                        request_id: "cli".into(),
                        limit: 1,
                        action: xorgate_core::sms::SmsAction::Clear,
                    },
                );
                out.push(render_clear_line(&p));
                exit_code(&p)
            } else {
                code
            };
            assert_eq!(out, strings(&want["stdout"]), "{name}");
            assert_eq!(Value::from(code), want["exitCode"], "{name}: exit");
            let want_err = want.get("stderr").map(strings).unwrap_or_default();
            assert_eq!(err, want_err, "{name}: stderr");
            assert_eq!(
                prompt,
                want.get("prompt").and_then(Value::as_str).unwrap_or(""),
                "{name}: prompt"
            );
            assert_eq!(Value::from(cleared), want["cleared"], "{name}: clears");
            continue;
        }
        let args = SmsCliArgs::parse(&argv);
        let (lines, code) = if args.help {
            // Q9: the usage names this binary.
            (vec![SMS_HELP.replace("xorgate-rs sms", "xorgate sms")], 0)
        } else {
            let p = build_sms_payload(
                &sweep,
                &SmsPayloadRequest::read("local", "cli", args.limit),
            );
            let lines = if args.json {
                vec![render_json(&p)]
            } else {
                let port = input["port"].as_str().unwrap_or("(none)");
                render_table(&p, port, input["columns"].as_u64().map(|c| c as u16))
            };
            (lines, exit_code(&p))
        };
        // `console.log` of a multi-line string is one entry in the capture.
        let got: Vec<String> = lines;
        assert_eq!(got, strings(&want["stdout"]), "{name}");
        assert_eq!(Value::from(code), want["exitCode"], "{name}: exit");
        if let Some(n) = want.get("cleared") {
            assert_eq!(*n, Value::from(0), "{name}: a read never clears");
        }
    }
    assert_eq!(clear_cases, 17);
}
