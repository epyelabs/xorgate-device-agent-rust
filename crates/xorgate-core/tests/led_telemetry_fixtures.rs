//! Phase 5b WP3 — LED status in telemetry against
//! `fixtures/led_telemetry/led-telemetry.json`, dumped from the REAL
//! `src/lib/led-state.ts` (`encodeLedTelemetry`, the frozen tables) and
//! `src/agent/led-telemetry.ts` (`withLedLeaves`, `withLedSampleGroups`) @
//! 09da38d by `tools/replay/js/dump-led-telemetry.mts`: every input of
//! `test/led-telemetry.test.ts` (27 its) + extras, and the recorder it (the
//! real `TelemetryRecorder`, 30 ticks at 10 Hz) as its recorded `system`
//! lines, replayed here through the core recorder gate.

mod common;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_core::led::{LedColour, LedPattern};
use xorgate_core::led_state::{
    encode_led_telemetry, led_state_code, resolve_led_state, rung, LedSignals, LedState,
    LedTelemetry, LteStatusView, HEALTH_FLAG_BITS, LED_LADDER, LED_STATE_CODES,
};
use xorgate_core::led_telemetry::{
    with_led_leaves, with_led_sample_groups, LedSampleGroup, LED_TELEMETRY_SIM,
    LED_TELEMETRY_STARTING,
};
use xorgate_core::recorder_gate::RecorderGate;
use xorgate_core::telemetry::{ConnectionState, MetricGroup, Metrics, RecordingStatus};
use xorgate_core::types::{GnssView, SkyState};

#[derive(Deserialize)]
struct Case {
    op: String,
    name: String,
    input: Value,
    expected: Value,
}

fn tel(v: &Value) -> Option<LedTelemetry> {
    (!v.is_null()).then(|| LedTelemetry {
        led_state: v["led_state"].as_u64().unwrap() as u32,
        health_flags: v["health_flags"].as_u64().unwrap() as u32,
    })
}

/// A ladder state by reason; an unknown reason keeps its TEXT (the TS's
/// `{colour, pattern, reason: "nope"}`).
fn state_of(v: &Value) -> LedState {
    let reason = v["reason"].as_str().unwrap();
    match rung(reason) {
        Some(r) => LedState {
            colour: r.colour,
            pattern: r.pattern,
            reason: r.reason,
        },
        None => LedState {
            colour: LedColour::Green,
            pattern: LedPattern::Heartbeat,
            reason: Box::leak(reason.to_string().into_boxed_str()),
        },
    }
}

#[derive(Clone)]
struct G {
    fields: MetricGroup,
    mono: u64,
}

impl LedSampleGroup for G {
    fn fields_mut(&mut self) -> &mut MetricGroup {
        &mut self.fields
    }
}

fn groups_of(v: &Value) -> IndexMap<String, G> {
    v.as_object()
        .unwrap()
        .iter()
        .map(|(k, g)| {
            (
                k.clone(),
                G {
                    fields: serde_json::from_value(g["fields"].clone()).unwrap(),
                    mono: g["updatedMono"].as_u64().unwrap(),
                },
            )
        })
        .collect()
}

fn groups_json(g: &IndexMap<String, G>) -> String {
    let mut m = serde_json::Map::new();
    for (k, v) in g {
        m.insert(
            k.clone(),
            json!({"fields": v.fields, "updatedMono": v.mono}),
        );
    }
    Value::Object(m).to_string()
}

#[test]
fn led_telemetry_matches_the_ts() {
    let f = common::fixture::<Case>("led_telemetry/led-telemetry.json");
    assert_eq!(f.js_commit, common::JS_COMMIT_P5B);

    // The frozen contract tables, in TS key order.
    let codes: Vec<(String, u64)> = f.extra["LED_STATE_CODES"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_u64().unwrap()))
        .collect();
    let ours: Vec<(String, u64)> = LED_STATE_CODES
        .iter()
        .map(|(k, v)| (k.to_string(), u64::from(*v)))
        .collect();
    assert_eq!(ours, codes);
    let bits: Vec<(String, u64)> = f.extra["HEALTH_FLAG_BITS"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_u64().unwrap()))
        .collect();
    let ours: Vec<(String, u64)> = HEALTH_FLAG_BITS
        .iter()
        .map(|(k, v)| (k.to_string(), u64::from(*v)))
        .collect();
    assert_eq!(ours, bits);
    // Every ladder state has a code; the overlay has none.
    let reasons: Vec<&str> = LED_LADDER.iter().map(|r| r.reason).collect();
    assert_eq!(Value::from(reasons.clone()), f.extra["ladderReasons"]);
    for r in reasons {
        assert_eq!(led_state_code(r).is_none(), r == "config-applied", "{r}");
    }
    assert_eq!(
        Some(LED_TELEMETRY_STARTING),
        tel(&f.extra["LED_TELEMETRY_STARTING"])
    );
    assert_eq!(Some(LED_TELEMETRY_SIM), tel(&f.extra["LED_TELEMETRY_SIM"]));

    let mut ops = std::collections::BTreeMap::<String, usize>::new();
    for c in &f.cases {
        *ops.entry(c.op.clone()).or_default() += 1;
        let name = &c.name;
        match c.op.as_str() {
            "encode" => {
                let s = &c.input["signals"];
                let recording: RecordingStatus =
                    serde_json::from_value(s["recording"].clone()).unwrap();
                let connection: ConnectionState =
                    serde_json::from_value(s["connection"].clone()).unwrap();
                let signals = LedSignals {
                    connection,
                    recording: &recording,
                    lte: LteStatusView::from_value(&s["lte"]),
                    // `sky` is never read by the ladder (Phase 4 mapping).
                    gnss: s["gnss"].as_object().map(|g| GnssView {
                        fix: g["fix"].as_bool().unwrap(),
                        sky: SkyState::Clear,
                    }),
                    starting: s["starting"].as_bool().unwrap(),
                };
                let state = state_of(&c.input["state"]);
                if rung(state.reason).is_some() && state.reason != "agent-down" {
                    assert_eq!(resolve_led_state(&signals), state, "{name}: resolved");
                }
                assert_eq!(
                    Some(encode_led_telemetry(&state, &signals)),
                    tel(&c.expected),
                    "{name}"
                );
            }
            "withLedLeaves" => {
                let m: Metrics = serde_json::from_value(c.input["metrics"].clone()).unwrap();
                let out = with_led_leaves(m, tel(&c.input["t"]));
                // Wire bytes (key order, no unit on the leaves).
                assert_eq!(
                    serde_json::to_string(&out).unwrap(),
                    c.expected["out"].to_string(),
                    "{name}"
                );
            }
            "withLedSampleGroups" => {
                let now = c.input["nowMono"].as_u64().unwrap();
                let out = with_led_sample_groups(
                    groups_of(&c.input["groups"]),
                    tel(&c.input["t"]),
                    |fields| G { fields, mono: now },
                );
                assert_eq!(groups_json(&out), c.expected["out"].to_string(), "{name}");
            }
            other => panic!("unknown op {other}"),
        }
    }
    assert_eq!(ops["encode"], 28);
    assert_eq!(ops["withLedLeaves"], 7);
    assert_eq!(ops["withLedSampleGroups"], 4);
}

/// The recorder it: imu fresh every tick, `system` fresh once a second,
/// the LED state moving at tick 5 — the recorded `system` lines carry both
/// leaves at the 1 Hz system cadence, reusing the system sample's stamp.
#[test]
fn recorded_system_lines_carry_the_leaves_at_the_system_cadence() {
    let f = common::fixture::<Case>("led_telemetry/led-telemetry.json");
    let want = &f.extra["recorder"];
    let mut gate: RecorderGate<u64> = RecorderGate::new(None);
    let mut state = LedTelemetry {
        led_state: 0,
        health_flags: 0,
    };
    let mut lines = Vec::new();
    for i in 0..want["ticks"].as_u64().unwrap() {
        if i == 5 {
            state = LedTelemetry {
                led_state: 3,
                health_flags: 8,
            };
        }
        let now = 10_000 + i * 100;
        let mut groups = IndexMap::new();
        let mut imu = MetricGroup::new();
        imu.insert(
            "a".into(),
            xorgate_core::telemetry::MetricLeaf::new((i + 1) as f64),
        );
        groups.insert(
            "imu".to_string(),
            G {
                fields: imu,
                mono: i + 1,
            },
        );
        let mut sys = MetricGroup::new();
        sys.insert(
            "cpu_usage".into(),
            xorgate_core::telemetry::MetricLeaf::with_unit(5.0, "%"),
        );
        groups.insert(
            "system".to_string(),
            G {
                fields: sys,
                mono: now / 1000,
            },
        );
        let groups = with_led_sample_groups(groups, Some(state), |_| unreachable!());
        let g = &groups["system"];
        if gate.admit("system", g.mono, now, 10.0) {
            lines.push(json!({"ts": now, "system": g.fields}));
        }
    }
    assert_eq!(Value::from(lines), want["systemLines"]);
}
