//! Fixture-driven tests for `xorgate_core::upload_step`: the REAL TS
//! `Uploader` (`src/recording/uploader.ts` @ 89b5c64) was driven through 13
//! scripted PUT-outcome scenarios with `setTimeout` patched to fire on demand
//! (`phase-3/evidence/wp3-tools/dump-upload-step.mts`). Here the pure
//! decisions (`may_continue`, `on_failure`, `failure_event`, `plan_retry`,
//! `skip_unrecognized_event`, the telemetry key rule) are folded over the same
//! script and must reproduce every event (level + msg) and every armed timer
//! delay. The full runtime replays the same fixture in
//! `crates/xorgate-agent/tests/recording_upload.rs`.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::upload_keys::{js_string_cmp, telemetry_s3_key};
use xorgate_core::upload_step::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    input: Value,
    expected: Value,
}

fn level(l: ActivityLevel) -> &'static str {
    match l {
        ActivityLevel::Info => "info",
        ActivityLevel::Warn => "warn",
        ActivityLevel::Error => "error",
    }
}

#[test]
fn uploader_drain_fixture() {
    let f = common::fixture::<Case>("upload_step/uploader-drain.json");
    let session = f.extra["sessionId"].as_str().unwrap().to_string();
    assert_eq!(f.cases.len(), 13);
    for c in &f.cases {
        let min = c.input["backoffMinMs"].as_u64().unwrap_or(BACKOFF_MIN_MS);
        let max = c.input["backoffMaxMs"].as_u64().unwrap_or(BACKOFF_MAX_MS);
        let mut segs: Vec<String> = c.input["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        segs.sort_by(|a, b| js_string_cmp(a, b));
        let script: Vec<Value> = c.input["script"].as_array().unwrap().clone();
        let pause_after = c.input["pauseAfterFailures"].as_u64();

        let (mut online, mut auto, mut stopped) = (true, true, false);
        let mut backoff = min;
        let mut events: Vec<(String, String)> = vec![];
        let mut timers: Vec<(u64, &str)> = vec![];
        let mut armed: Option<usize> = None;
        let mut skipped: Vec<String> = vec![];
        let (mut i, mut failures, mut paused) = (0usize, 0u64, false);

        // One drain (kick → settled), as `uploader.ts:136-213`.
        let mut drain = |online: &mut bool,
                         auto: &mut bool,
                         stopped: &mut bool,
                         backoff: &mut u64,
                         armed: &mut Option<usize>,
                         timers: &mut Vec<(u64, &str)>,
                         events: &mut Vec<(String, String)>,
                         segs: &mut Vec<String>,
                         failures: &mut u64| {
            if !may_kick(*stopped, false, *online, *auto) {
                return;
            }
            if let Some(t) = armed.take() {
                timers[t].1 = "cleared";
            }
            while may_continue(*stopped, *online, *auto) {
                let Some(name) = segs.iter().find(|n| !skipped.contains(n)).cloned() else {
                    break;
                };
                let full = format!("{session}/{name}");
                if telemetry_s3_key("d", &full).is_none() {
                    skipped.push(name);
                    events.push(("warn".into(), skip_unrecognized_event(&full)));
                    continue;
                }
                let o = script.get(i).cloned().unwrap_or(Value::from("ok"));
                i += 1;
                if o == "ok" {
                    segs.retain(|n| *n != name);
                    *backoff = min;
                    continue;
                }
                *failures += 1;
                let (kind, msg) = o.as_object().unwrap().iter().next().unwrap();
                match kind.as_str() {
                    "failOffline" => *online = false,
                    "failStop" => {
                        *stopped = true;
                        *backoff = min; // stop() → pause()
                    }
                    "failModeOff" => *auto = false,
                    _ => {}
                }
                let step = on_failure(*stopped, *online);
                let (l, m) = failure_event(&step, *backoff, msg.as_str().unwrap());
                events.push((level(l).into(), m));
                if step == FailureStep::Retry {
                    if let Some(p) = plan_retry(armed.is_some(), *stopped, *backoff, max) {
                        timers.push((p.delay_ms, "armed"));
                        *armed = Some(timers.len() - 1);
                        *backoff = p.next_backoff_ms;
                    }
                }
                break;
            }
        };
        drain(
            &mut online,
            &mut auto,
            &mut stopped,
            &mut backoff,
            &mut armed,
            &mut timers,
            &mut events,
            &mut segs,
            &mut failures,
        );
        for _ in 0..100 {
            if pause_after == Some(failures) && !paused {
                paused = true;
                backoff = min; // pause()
                if let Some(t) = armed.take() {
                    timers[t].1 = "cleared";
                }
                drain(
                    &mut online,
                    &mut auto,
                    &mut stopped,
                    &mut backoff,
                    &mut armed,
                    &mut timers,
                    &mut events,
                    &mut segs,
                    &mut failures,
                );
                continue;
            }
            let Some(t) = armed.take() else { break };
            timers[t].1 = "fired";
            drain(
                &mut online,
                &mut auto,
                &mut stopped,
                &mut backoff,
                &mut armed,
                &mut timers,
                &mut events,
                &mut segs,
                &mut failures,
            );
        }

        let want_events: Vec<(String, String)> = c.expected["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["level"].as_str().unwrap().to_string(),
                    e["msg"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(events, want_events, "{}", c.name);
        let want_timers: Vec<(u64, String)> = c.expected["timers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["ms"].as_u64().unwrap(),
                    t["state"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let got_timers: Vec<(u64, String)> =
            timers.iter().map(|(ms, s)| (*ms, s.to_string())).collect();
        assert_eq!(got_timers, want_timers, "{}", c.name);
    }
}
