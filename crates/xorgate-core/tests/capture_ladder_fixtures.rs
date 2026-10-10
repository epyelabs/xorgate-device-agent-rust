//! Fixture-driven tests for `xorgate_core::capture_ladder`, dumped from the
//! REAL `VideoCaptureSupervisor` @ 89b5c64 by
//! `plans/device-agent-js-to-rust/phase-3/evidence/wp4-tools/dump-capture-ladder.mts`
//! (private `onExit` / `checkProgress` / `isRecording` / `reapShmSegments`
//! driven over an input grid with `Date.now`/`setTimeout` stubbed; the spawn
//! and `setCameras` event text from real `start()`/`setCameras()` calls).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::capture_ladder::*;
use xorgate_core::telemetry::ActivityLevel;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

fn state(v: &Value) -> CaptureState {
    serde_json::from_value(v.clone()).expect("state")
}

fn events(v: &Value) -> Vec<(ActivityLevel, String)> {
    v.as_array()
        .expect("events")
        .iter()
        .map(|e| {
            (
                serde_json::from_value(e["level"].clone()).unwrap(),
                e["msg"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| panic!("u64: {v}"))
}

#[test]
fn capture_ladder_fixtures() {
    let f = common::fixture::<Case>("capture_ladder/ladder.json");
    assert_eq!(u(&f.extra["constants"]["STABLE_MS"]), STABLE_MS);
    assert_eq!(u(&f.extra["constants"]["BASE_BACKOFF_MS"]), BASE_BACKOFF_MS);
    assert_eq!(u(&f.extra["constants"]["MAX_BACKOFF_MS"]), MAX_BACKOFF_MS);
    assert_eq!(
        u(&f.extra["constants"]["MAX_QUICK_FAILURES"]),
        u64::from(MAX_QUICK_FAILURES)
    );
    assert_eq!(u(&f.extra["constants"]["PARKED_RETRY_MS"]), PARKED_RETRY_MS);
    assert_eq!(u(&f.extra["constants"]["SWEEP_POLL_MS"]), SWEEP_POLL_MS);
    assert_eq!(u(&f.extra["constants"]["STALL_MS"]), STALL_MS);
    assert_eq!(
        u(&f.extra["constants"]["MAX_STALL_FAILURES"]),
        u64::from(MAX_STALL_FAILURES)
    );
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *counts.entry(c.op.clone()).or_default() += 1;
        match c.op.as_str() {
            "onExit" => on_exit(c),
            "checkProgress" => check_progress(c),
            "isRecording" => recording(c),
            "reapShmSegments" => shm(c),
            "spawnEvent" => spawn(c),
            "disabledEvent" => {
                let k = c.input["streamKey"].as_str().unwrap();
                assert_eq!(
                    events(&c.expected["events"]),
                    vec![(ActivityLevel::Info, camera_disabled_event(k))],
                    "{}",
                    c.name
                );
            }
            other => panic!("{}: unknown op {other}", c.name),
        }
    }
    assert_eq!(counts["onExit"], 438);
    assert_eq!(counts["checkProgress"], 250);
    assert_eq!(counts["isRecording"], 40);
    assert_eq!(counts["reapShmSegments"], 5);
    assert_eq!(counts["spawnEvent"], 6);
    assert_eq!(counts["disabledEvent"], 2);
}

fn on_exit(c: &Case) {
    let i = &c.input;
    let session = i["endedSession"].as_str();
    let input = ExitInput {
        stream_key: i["streamKey"].as_str().unwrap(),
        exit_code: i["exitCode"].as_i64().map(|v| v as i32),
        not_found: i["notFound"].as_bool().unwrap(),
        stalled: i["stalled"].as_bool().unwrap(),
        ran_ms: u(&i["ranMs"]),
        stable_ms: i.get("stableMs").map_or(STABLE_MS, u),
        ended_session: session,
        counters: LadderCounters {
            quick_failures: u(&i["quickFailures"]) as u32,
            stall_failures: u(&i["stallFailures"]) as u32,
            backoff_ms: u(&i["backoffMs"]),
        },
    };
    let v = exit_verdict(&input);
    let e = &c.expected;
    // The dumper's run started in "running".
    assert_eq!(
        v.state.unwrap_or(CaptureState::Running),
        state(&e["state"]),
        "{}",
        c.name
    );
    assert_eq!(
        v.counters,
        LadderCounters {
            quick_failures: u(&e["quickFailures"]) as u32,
            stall_failures: u(&e["stallFailures"]) as u32,
            backoff_ms: u(&e["backoffMs"]),
        },
        "{}",
        c.name
    );
    assert_eq!(
        e["restartMs"],
        serde_json::json!([v.restart_ms]),
        "{}",
        c.name
    );
    assert_eq!(
        events(&e["events"]),
        vec![(v.level, v.event.clone())],
        "{}",
        c.name
    );
}

fn check_progress(c: &Case) {
    let i = &c.input;
    let e = &c.expected;
    let key = i["streamKey"].as_str().unwrap();
    let sid = i["sessionId"].as_str();
    let stall_ms = i.get("stallMs").map_or(STALL_MS, u);
    let pre = i["pre"].as_str().unwrap();
    let path_of = |tag: &str| format!("/s/{tag}");
    // The precondition gate of checkProgress (`:964-965`) is the agent's;
    // here: anything but "live" must be a no-op.
    if pre != "live" {
        assert_eq!(e["signal"], Value::Null, "{}", c.name);
        assert!(events(&e["events"]).is_empty(), "{}", c.name);
        return;
    }
    let active = match i["active"].as_str().unwrap() {
        "none" => None,
        "missing" => Some(path_of("missing")),
        _ => Some(path_of("A")),
    };
    let last = Progress {
        path: i["progressPath"].as_str().map(path_of),
        bytes: u(&i["progressBytes"]),
        at_ms: 1_000_000 - u(&i["elapsed"]),
    };
    let v = progress_verdict(
        active.as_deref(),
        u(&i["activeBytes"]),
        &last,
        1_000_000,
        stall_ms,
    );
    let tag = |p: &Option<String>| {
        p.as_deref()
            .map(|s| s.trim_start_matches("/s/").to_string())
    };
    match v {
        ProgressVerdict::Advanced { path, bytes } => {
            assert_eq!(
                e["progressPath"].as_str(),
                Some(tag(&Some(path)).unwrap().as_str()),
                "{}",
                c.name
            );
            assert_eq!(u(&e["progressBytes"]), bytes, "{}", c.name);
            assert_eq!(e["progressAtDelta"], 0, "{}", c.name);
            assert_eq!(e["stallFailures"], 0, "{}", c.name);
            assert_eq!(e["signal"], Value::Null, "{}", c.name);
        }
        ProgressVerdict::Waiting => {
            assert_eq!(
                e["progressPath"].as_str().map(str::to_string),
                tag(&last.path),
                "{}",
                c.name
            );
            assert_eq!(e["stalled"], false, "{}", c.name);
            assert_eq!(e["signal"], Value::Null, "{}", c.name);
            assert!(events(&e["events"]).is_empty(), "{}", c.name);
        }
        ProgressVerdict::Stalled => {
            assert_eq!(e["stalled"], true, "{}", c.name);
            assert_eq!(state(&e["state"]), CaptureState::Failed, "{}", c.name);
            assert_eq!(e["signal"], "SIGKILL", "{}", c.name);
            assert_eq!(
                events(&e["events"]),
                vec![(ActivityLevel::Warn, stall_event(key, stall_ms, sid))],
                "{}",
                c.name
            );
        }
    }
}

fn recording(c: &Case) {
    let i = &c.input;
    let r = is_recording(
        state(&i["state"]),
        u(&i["stallFailures"]) as u32,
        u(&i["progressBytes"]),
    );
    let record = i["record"].as_bool().unwrap();
    assert_eq!(c.expected["isRecording"], r, "{}", c.name);
    assert_eq!(c.expected["statusRecording"], record && r, "{}", c.name);
    assert_eq!(c.expected["anyRecording"], record && r, "{}", c.name);
}

fn shm(c: &Case) {
    let names: Vec<String> = serde_json::from_value(c.input["names"].clone()).unwrap();
    let pid = c.input["pid"].as_u64().map(|p| p as u32);
    let reap = shm_names_to_reap(&names, pid);
    let mut remaining: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| !reap.contains(n))
        .collect();
    remaining.sort_unstable();
    let expected: Vec<String> = serde_json::from_value(c.expected["remaining"].clone()).unwrap();
    assert_eq!(remaining, expected, "{}", c.name);
}

fn spawn(c: &Case) {
    let i = &c.input;
    let record = i["record"].as_bool().unwrap();
    let msg = spawn_event(
        record,
        i["streamKey"].as_str().unwrap(),
        i["preset"].as_str().unwrap(),
        i["sessionId"].as_str().unwrap_or(""),
    );
    assert_eq!(
        events(&c.expected["events"]),
        vec![(ActivityLevel::Info, msg)],
        "{}",
        c.name
    );
}
