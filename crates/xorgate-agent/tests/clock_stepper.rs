//! The REAL `ClockStepper` runtime (Phase 4 WP3, I6) — wiring, not decisions
//! (those are core `decide`/`settle`, fixtured in Phase 1):
//!
//! - the 23 Phase 1 stepper sequences (`fixtures/clock_step/clock-step.json`,
//!   34 ticks, dumped from the REAL TS `createClockStepper` through
//!   `test/clock-step.test.ts` `makeHarness`) replayed tick by tick through
//!   `ClockStepper::tick()` with a recording helper runner: same applied
//!   targets (as the exact `sudo -n xorgate-setclock <epoch>` argv), stepped
//!   callbacks, events and `status()`;
//! - `clock-step.test.ts:327` "survives a throwing dependency" (CF60);
//! - CF20 (no probe when disarmed, one `config()` per evaluation,
//!   single-flight), CF55 (`SetInterval`, first tick immediate), NITZ read
//!   only without a valid GPS time, the failure detail rules.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use xorgate_agent::ctx::BoxFut;
use xorgate_agent::ops::clock_stepper::{ClockStepper, ClockStepperOptions};
use xorgate_core::clock::Clock;
use xorgate_core::clock_step::{
    read_modem_clock, setclock_argv, tick_failed_message, EventLevel, ModemClockReading,
    TimeSyncConfig, TIME_SYNC_DEFAULTS,
};
use xorgate_core::commands::RunResult;
use xorgate_core::telemetry::JsNumber;
use xorgate_core::types::{GpsClockReading, ModemTelemetry, SteppedClockSource};
use xorgate_hal::privileged::{HelperArgv, PrivilegedArgv, PrivilegedRunner, RunFut};

/// A settable wall/mono clock (the TS harness's `now`/`monoMs` seams).
#[derive(Default)]
struct FakeClock {
    wall: Mutex<i64>,
    mono: Mutex<u64>,
}
impl FakeClock {
    fn set(&self, wall: i64, mono: u64) {
        *self.wall.lock().unwrap() = wall;
        *self.mono.lock().unwrap() = mono;
    }
}
impl Clock for FakeClock {
    fn wall_ms(&self) -> i64 {
        *self.wall.lock().unwrap()
    }
    fn mono_ms(&self) -> u64 {
        *self.mono.lock().unwrap()
    }
}

/// Records helper argv; answers from a queue; moves the wall clock to
/// `wall_after` when the step "lands" (the TS harness's `applyStep`).
#[derive(Default)]
struct StepRunner {
    clock: Arc<FakeClock>,
    answers: Mutex<VecDeque<(RunResult, Option<i64>)>>,
    calls: Mutex<Vec<Vec<String>>>,
}
impl PrivilegedRunner for StepRunner {
    fn run<'a>(&'a self, _call: &'a PrivilegedArgv) -> RunFut<'a> {
        panic!("the clock stepper never runs a command argv")
    }
    fn run_helper<'a>(&'a self, call: &'a HelperArgv) -> RunFut<'a> {
        self.calls.lock().unwrap().push(call.argv().to_vec());
        let (r, wall_after) = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((ok(), None));
        if let Some(w) = wall_after {
            *self.clock.wall.lock().unwrap() = w;
        }
        Box::pin(async move { r })
    }
}

fn ok() -> RunResult {
    RunResult {
        failed: false,
        not_found: false,
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
    }
}

fn failed(stderr: &str, stdout: &str, code: Option<i64>) -> RunResult {
    RunResult {
        failed: true,
        not_found: false,
        exit_code: code,
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

/// What a test can drive per tick.
struct Inputs {
    ntp: bool,
    cfg: TimeSyncConfig,
    gps: Option<GpsClockReading>,
    modem: Option<ModemClockReading>,
    modem_telemetry: Option<ModemTelemetry>,
}
impl Default for Inputs {
    fn default() -> Self {
        Self {
            ntp: false,
            cfg: TIME_SYNC_DEFAULTS,
            gps: None,
            modem: None,
            modem_telemetry: None,
        }
    }
}

#[derive(Default)]
struct Counters {
    probes: AtomicUsize,
    configs: AtomicUsize,
    gps: AtomicUsize,
    modem: AtomicUsize,
}

struct Harness {
    stepper: ClockStepper,
    clock: Arc<FakeClock>,
    runner: Arc<StepRunner>,
    inputs: Arc<Mutex<Inputs>>,
    counters: Arc<Counters>,
    events: Arc<Mutex<Vec<(EventLevel, String)>>>,
    stepped: Arc<Mutex<Vec<(SteppedClockSource, f64)>>>,
}

fn harness_with(gps_panics: bool) -> Harness {
    let clock = Arc::new(FakeClock::default());
    let runner = Arc::new(StepRunner {
        clock: clock.clone(),
        ..Default::default()
    });
    let inputs = Arc::new(Mutex::new(Inputs::default()));
    let counters = Arc::new(Counters::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let stepped = Arc::new(Mutex::new(Vec::new()));
    let (i1, i2, i3, i4) = (
        inputs.clone(),
        inputs.clone(),
        inputs.clone(),
        inputs.clone(),
    );
    let (c1, c2, c3, c4) = (
        counters.clone(),
        counters.clone(),
        counters.clone(),
        counters.clone(),
    );
    let (ev, st) = (events.clone(), stepped.clone());
    let stepper = ClockStepper::new(ClockStepperOptions {
        config: Box::new(move || {
            c1.configs.fetch_add(1, Ordering::SeqCst);
            i1.lock().unwrap().cfg.clone()
        }),
        gps_clock: Box::new(move || {
            c2.gps.fetch_add(1, Ordering::SeqCst);
            if gps_panics {
                panic!("boom");
            }
            i2.lock().unwrap().gps
        }),
        modem_clock: Box::new(move |now| {
            c3.modem.fetch_add(1, Ordering::SeqCst);
            let i = i3.lock().unwrap();
            let r = match &i.modem_telemetry {
                Some(snap) => read_modem_clock(now, Some(snap)),
                None => i.modem,
            };
            Box::pin(async move { r }) as BoxFut<'static, _>
        }),
        probe_ntp: Box::new(move || {
            c4.probes.fetch_add(1, Ordering::SeqCst);
            let synced = i4.lock().unwrap().ntp;
            Box::pin(async move { synced })
        }),
        runner: runner.clone(),
        on_stepped: Box::new(move |s, o| st.lock().unwrap().push((s, o))),
        on_event: Box::new(move |l, m| ev.lock().unwrap().push((l, m.to_string()))),
        clock: clock.clone(),
    });
    Harness {
        stepper,
        clock,
        runner,
        inputs,
        counters,
        events,
        stepped,
    }
}

fn harness() -> Harness {
    harness_with(false)
}

fn fixture() -> Value {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/clock_step/clock-step.json"
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[derive(Deserialize)]
struct Apply {
    ok: bool,
    detail: String,
    #[serde(rename = "wallAfter")]
    wall_after: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TickInput {
    ntp_synced: bool,
    config: TimeSyncConfig,
    gps: Option<GpsClockReading>,
    #[serde(default)]
    modem: Option<ModemClockReading>,
    #[serde(default)]
    modem_telemetry: Option<ModemTelemetry>,
    now: i64,
    mono: u64,
    apply: Apply,
}

#[derive(Deserialize)]
struct Tick {
    input: TickInput,
    expected: Value,
}

#[derive(Deserialize)]
struct StepperCase {
    name: String,
    ticks: Vec<Tick>,
}

#[tokio::test]
async fn phase1_stepper_sequences_through_the_real_runtime() {
    let cases: Vec<StepperCase> =
        serde_json::from_value(fixture()["stepperCases"].clone()).unwrap();
    assert_eq!(cases.len(), 23);
    let mut ticks_total = 0;
    for c in &cases {
        let h = harness();
        let mut applied: Vec<f64> = Vec::new();
        for (i, t) in c.ticks.iter().enumerate() {
            ticks_total += 1;
            let inp = &t.input;
            {
                let mut x = h.inputs.lock().unwrap();
                x.ntp = inp.ntp_synced;
                x.cfg = inp.config.clone();
                x.gps = inp.gps;
                x.modem = inp.modem;
                x.modem_telemetry = inp.modem_telemetry.clone();
            }
            h.clock.set(inp.now, inp.mono);
            let answer = if inp.apply.ok {
                ok()
            } else {
                failed(&inp.apply.detail, "", Some(1))
            };
            h.runner
                .answers
                .lock()
                .unwrap()
                .push_back((answer, Some(inp.apply.wall_after)));
            let before = h.runner.calls.lock().unwrap().len();
            h.stepper.tick().await;
            let calls = h.runner.calls.lock().unwrap().clone();
            // Every apply is the exact sudo argv; map it back to the target the
            // TS recorded (argv = round(target/1000), so compare argv).
            let expected_this: Vec<f64> =
                serde_json::from_value(t.expected["appliedThisTick"].clone()).unwrap();
            let this_argv: Vec<Vec<String>> = calls[before..].to_vec();
            let want_argv: Vec<Vec<String>> = expected_this
                .iter()
                .map(|t| {
                    let mut v = vec!["sudo".to_string()];
                    v.extend(setclock_argv(*t));
                    v
                })
                .collect();
            assert_eq!(this_argv, want_argv, "{} — tick {i} argv", c.name);
            if this_argv.is_empty() {
                // nothing applied: drop the unused answer
                h.runner.answers.lock().unwrap().clear();
            }
            applied.extend(expected_this.iter().copied());
            let events: Vec<Value> = h
                .events
                .lock()
                .unwrap()
                .iter()
                .map(|(l, m)| serde_json::json!({ "level": l, "msg": m }))
                .collect();
            let stepped: Vec<Value> = h
                .stepped
                .lock()
                .unwrap()
                .iter()
                .map(|(s, o)| serde_json::json!({ "source": s, "offsetMs": JsNumber(*o) }))
                .collect();
            let got = serde_json::json!({
                "applied": applied.iter().map(|v| JsNumber(*v)).collect::<Vec<_>>(),
                "stepped": stepped,
                "events": events,
                "status": h.stepper.status(),
                "appliedThisTick": expected_this.iter().map(|v| JsNumber(*v)).collect::<Vec<_>>(),
            });
            assert_eq!(got, t.expected, "{} — tick {i}", c.name);
        }
    }
    assert_eq!(ticks_total, 34);
}

/// `clock-step.test.ts:327` "survives a throwing dependency (fail open, logged)".
#[tokio::test]
async fn survives_a_throwing_dependency() {
    let h = harness_with(true);
    h.clock.set(1_785_326_400_000, 0);
    h.stepper.tick().await; // resolves (TS: `.resolves.toBeUndefined()`)
    assert_eq!(h.stepper.status().steps, 0);
    assert_eq!(
        *h.events.lock().unwrap(),
        [(EventLevel::Warn, tick_failed_message("boom"))]
    );
    // Still usable: the flag was released, the next tick evaluates again.
    h.stepper.tick().await;
    assert_eq!(h.counters.gps.load(Ordering::SeqCst), 2);
    assert!(h.stepper.status().armed);
}

/// CF20: a disarmed stepper never probes again; `config()` once per
/// evaluation and never on the NTP branch.
#[tokio::test]
async fn cf20_probe_skipped_when_disarmed_and_one_config_read() {
    let h = harness();
    h.clock.set(1_785_326_400_000, 100_000);
    h.stepper.tick().await;
    assert_eq!(
        (
            h.counters.probes.load(Ordering::SeqCst),
            h.counters.configs.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
    h.inputs.lock().unwrap().ntp = true;
    h.stepper.tick().await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 2);
    assert_eq!(h.counters.configs.load(Ordering::SeqCst), 1); // not on the NTP branch
    assert!(!h.stepper.status().armed);
    for _ in 0..3 {
        h.stepper.tick().await;
    }
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 2);
    assert_eq!(h.events.lock().unwrap().len(), 1);
}

/// NITZ is read only when there is no valid GPS time, and only past the
/// kill-switch / hold-off / cap gates.
#[tokio::test]
async fn nitz_read_only_without_valid_gps_and_past_the_gates() {
    let h = harness();
    h.clock.set(1_785_326_400_000, 100_000);
    h.inputs.lock().unwrap().gps = Some(GpsClockReading {
        utc_ms: 1_785_326_400_000,
        age_ms: 0,
        valid: true,
    });
    h.stepper.tick().await;
    assert_eq!(h.counters.modem.load(Ordering::SeqCst), 0);
    h.inputs.lock().unwrap().gps = Some(GpsClockReading {
        utc_ms: 1_785_326_400_000,
        age_ms: 0,
        valid: false,
    });
    h.stepper.tick().await;
    assert_eq!(h.counters.modem.load(Ordering::SeqCst), 1);
    // Kill switch off: no source read at all.
    h.inputs.lock().unwrap().cfg.offline_sources = false;
    h.stepper.tick().await;
    assert_eq!(h.counters.modem.load(Ordering::SeqCst), 1);
    assert_eq!(h.counters.gps.load(Ordering::SeqCst), 2);
}

/// `defaultApplyStep` detail rules: stderr ‖ stdout ‖ `exit N` / `exit null`.
#[tokio::test]
async fn failure_detail_rules_and_hold_off_after_any_attempt() {
    let cases = [
        (
            failed("  sudo: a password is required\n", "x", Some(1)),
            "sudo: a password is required",
        ),
        (failed("", " out \n", Some(2)), "out"),
        (failed("", "", Some(2)), "exit 2"),
        (failed("", "", None), "exit null"),
    ];
    for (r, want) in cases {
        let h = harness();
        h.clock.set(1_785_326_400_000, 100_000);
        h.inputs.lock().unwrap().gps = Some(GpsClockReading {
            utc_ms: 1_785_326_700_000,
            age_ms: 0,
            valid: true,
        });
        h.runner.answers.lock().unwrap().push_back((r, None));
        h.stepper.tick().await;
        let ev = h.events.lock().unwrap().clone();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, EventLevel::Warn);
        assert!(ev[0].1.ends_with(&format!(": {want}")), "{:?}", ev[0].1);
        assert_eq!(h.stepper.status().steps, 0);
        // Held off: the next tick does not hammer sudo.
        h.stepper.tick().await;
        assert_eq!(h.runner.calls.lock().unwrap().len(), 1);
        assert_eq!(
            h.runner.calls.lock().unwrap()[0],
            [
                "sudo",
                "-n",
                "/usr/local/bin/xorgate-setclock",
                "1785326700"
            ]
        );
    }
}

/// CF55 + `start()`: the first evaluation is immediate, then one every 30 s
/// on `SetInterval`; NTP sync disarms and clears the interval for good.
#[tokio::test(start_paused = true)]
async fn start_ticks_immediately_then_every_30s_until_ntp_disarms() {
    let h = harness();
    h.clock.set(1_785_326_400_000, 100_000);
    h.stepper.start();
    h.stepper.start(); // idempotent while running
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(29_990)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 2);
    tokio::time::sleep(Duration::from_millis(30_000)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 3);
    h.inputs.lock().unwrap().ntp = true;
    tokio::time::sleep(Duration::from_millis(30_000)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 4);
    assert!(!h.stepper.status().armed);
    tokio::time::sleep(Duration::from_millis(120_000)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 4);
    h.stepper.start(); // disarmed: start is a no-op
    tokio::time::sleep(Duration::from_millis(60_000)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 4);
}

/// `stop()` clears the interval; `start()` after `stop()` re-arms.
#[tokio::test(start_paused = true)]
async fn stop_clears_the_interval() {
    let h = harness();
    h.clock.set(1_785_326_400_000, 100_000);
    h.stepper.start();
    tokio::time::sleep(Duration::from_millis(1)).await;
    h.stepper.stop();
    tokio::time::sleep(Duration::from_millis(90_000)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 1);
    h.stepper.start();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(h.counters.probes.load(Ordering::SeqCst), 2);
}

/// Single flight: a tick while one is in progress returns at once.
#[tokio::test(start_paused = true)]
async fn single_flight_tick() {
    let clock = Arc::new(FakeClock::default());
    let probes = Arc::new(AtomicU32::new(0));
    let p = probes.clone();
    let stepper = ClockStepper::new(ClockStepperOptions {
        config: Box::new(|| TIME_SYNC_DEFAULTS),
        gps_clock: Box::new(|| None),
        modem_clock: Box::new(|_| Box::pin(async { None })),
        probe_ntp: Box::new(move || {
            p.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                false
            })
        }),
        runner: Arc::new(StepRunner::default()),
        on_stepped: Box::new(|_, _| {}),
        on_event: Box::new(|_, _| {}),
        clock,
    });
    let slow = tokio::spawn({
        let s = stepper.clone();
        async move { s.tick().await }
    });
    tokio::time::sleep(Duration::from_millis(1)).await;
    stepper.tick().await; // returns immediately — no second probe
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    slow.await.unwrap();
    stepper.tick().await;
    assert_eq!(probes.load(Ordering::SeqCst), 2);
}

/// CF58 / D26: `onStepped` → `noteStep(source)` reflected at once, then ONE
/// background `refresh()` (probe) — the TS order (`worker.ts:541-545`).
#[tokio::test]
async fn on_stepped_notes_the_step_then_refreshes_in_the_background() {
    use xorgate_agent::ops::clock_stepper::time_source_on_stepped;
    use xorgate_agent::recording::time_source::{ProbeFn, TimeSourceHandle};
    use xorgate_core::session::ProbedTimeSource;
    use xorgate_core::types::TimeSource;
    let probes = Arc::new(AtomicUsize::new(0));
    let p = probes.clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    let g = gate.clone();
    let probe: ProbeFn = Arc::new(move || {
        p.fetch_add(1, Ordering::SeqCst);
        let g = g.clone();
        Box::pin(async move {
            g.notified().await;
            ProbedTimeSource::Unsynced
        })
    });
    let ts = TimeSourceHandle::create(
        60_000,
        probe,
        Arc::new(xorgate_core::clock::FixedClock::new(0, 0)),
    );
    let on_stepped = time_source_on_stepped(ts.clone());
    on_stepped(SteppedClockSource::Gps, 600_000.0);
    tokio::task::yield_now().await; // the background refresh starts (probe pending)
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    // noteStep landed synchronously: reflected while the probe is in flight.
    assert_eq!(ts.current(), TimeSource::Gps);
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    gate.notify_one();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    // An unsynced probe never displaces the noted step.
    assert_eq!(ts.current(), TimeSource::Gps);
}
