//! Fixture-driven tests for `xorgate_core::clock_step` — `test/clock-step.test.ts`
//! (20 `it`): `parseCclkUtcMs` 5, `readModemClock` 4, `resolveTimeSyncConfig` 1
//! as tables (+ extras), and the 9 fixture-able `createClockStepper` cases (+13
//! extras) replayed tick by tick through `decide` + `settle`. Each recorded
//! tick carries what the TS harness fed the stepper and the cumulative
//! `applied` / `stepped` / `events` / `status()` the real TS stepper produced.
//! "survives a throwing dependency" is the agent's `tick()` catch — not pure.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::clock_step::*;
use xorgate_core::settings::TimeSyncSettings;
use xorgate_core::types::{GpsClockReading, ModemTelemetry, SteppedClockSource};

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

#[test]
fn clock_step_test_ts_tables() {
    let f = common::fixture::<Case>("clock_step/clock-step.json");
    let mut n = (0, 0, 0);
    for c in &f.cases {
        let got: Value = match c.op.as_str() {
            "parseCclkUtcMs" => {
                n.0 += 1;
                serde_json::to_value(parse_cclk_utc_ms(c.input.as_str().unwrap())).unwrap()
            }
            "readModemClock" => {
                n.1 += 1;
                let now = c.input["now"].as_i64().unwrap();
                let t: Option<ModemTelemetry> =
                    serde_json::from_value(c.input["telemetry"].clone()).unwrap();
                serde_json::to_value(read_modem_clock(now, t.as_ref())).unwrap()
            }
            "resolveTimeSyncConfig" => {
                n.2 += 1;
                let s: Option<TimeSyncSettings> = if common::is_undefined_marker(&c.input) {
                    None
                } else {
                    Some(serde_json::from_value(c.input.clone()).unwrap())
                };
                serde_json::to_value(resolve_time_sync_config(s.as_ref())).unwrap()
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 38);
    assert_eq!(n, (17, 19, 2));

    let k = &f.extra["constants"];
    assert_eq!(
        serde_json::to_value(TIME_SYNC_DEFAULTS).unwrap(),
        k["TIME_SYNC_DEFAULTS"]
    );
    assert_eq!(k["SETCLOCK_HELPER"], SETCLOCK_HELPER);
    assert_eq!(k["MIN_PLAUSIBLE_MS"].as_f64().unwrap(), MIN_PLAUSIBLE_MS);
    assert_eq!(k["MAX_PLAUSIBLE_MS"].as_f64().unwrap(), MAX_PLAUSIBLE_MS);
    assert_eq!(k["HOLD_OFF_MS"].as_u64().unwrap(), HOLD_OFF_MS);
    assert_eq!(k["CHECK_INTERVAL_MS"].as_u64().unwrap(), CHECK_INTERVAL_MS);
    assert_eq!(
        k["MODEM_CLOCK_FRESH_MS"].as_f64().unwrap(),
        MODEM_CLOCK_FRESH_MS
    );
}

#[derive(Deserialize)]
struct Apply {
    ok: bool,
    detail: String,
    #[serde(rename = "wallAfter")]
    wall_after: i64,
}

#[derive(Deserialize)]
struct TickInput {
    #[serde(rename = "ntpSynced")]
    ntp_synced: bool,
    config: TimeSyncConfig,
    gps: Option<GpsClockReading>,
    #[serde(default)]
    modem: Option<ModemClockReading>,
    #[serde(default, rename = "modemTelemetry")]
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

#[derive(serde::Serialize)]
struct SteppedRec {
    source: SteppedClockSource,
    #[serde(rename = "offsetMs", with = "xorgate_core::telemetry::js_num")]
    offset_ms: f64,
}

#[test]
fn clock_step_test_ts_stepper_sequences() {
    let f = common::fixture::<Case>("clock_step/clock-step.json");
    let cases: Vec<StepperCase> = serde_json::from_value(f.extra["stepperCases"].clone()).unwrap();
    assert_eq!(cases.len(), 23);
    let mut ticks_total = 0;
    for c in &cases {
        let mut state = ClockStepperState::new();
        let mut applied: Vec<f64> = Vec::new();
        let mut stepped: Vec<SteppedRec> = Vec::new();
        let mut events: Vec<ClockEvent> = Vec::new();
        for (i, t) in c.ticks.iter().enumerate() {
            ticks_total += 1;
            let inp = &t.input;
            // The end-to-end case drives the REAL readModemClock over a snapshot.
            let modem = match &inp.modem_telemetry {
                Some(snap) => read_modem_clock(inp.now, Some(snap)),
                None => inp.modem,
            };
            let d = decide(
                &state,
                &DecideInputs {
                    ntp_synced: inp.ntp_synced,
                    cfg: &inp.config,
                    gps: inp.gps,
                    modem,
                    now: inp.now,
                    mono: inp.mono,
                    hold_off_ms: HOLD_OFF_MS,
                },
            );
            state = d.state;
            events.extend(d.events);
            let mut this_tick: Vec<f64> = Vec::new();
            if let Some(step) = d.step {
                applied.push(step.target_ms);
                this_tick.push(step.target_ms);
                let res = StepResult {
                    ok: inp.apply.ok,
                    detail: inp.apply.detail.clone(),
                };
                let s = settle(&state, &step, &res, inp.apply.wall_after, &inp.config);
                state = s.state;
                events.extend(s.events);
                if let Some((source, offset_ms)) = s.stepped {
                    stepped.push(SteppedRec { source, offset_ms });
                }
            }
            let got = serde_json::json!({
                "applied": applied.iter().map(|v| xorgate_core::telemetry::JsNumber(*v)).collect::<Vec<_>>(),
                "stepped": stepped,
                "events": events,
                "status": state.status(),
                "appliedThisTick": this_tick.iter().map(|v| xorgate_core::telemetry::JsNumber(*v)).collect::<Vec<_>>(),
            });
            assert_eq!(got, t.expected, "{} — tick {i}", c.name);
        }
    }
    assert_eq!(ticks_total, 34);
}
