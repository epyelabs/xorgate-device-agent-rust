//! Fixture-driven test for `xorgate_core::recorder_gate`, from the REAL
//! `TelemetryRecorder` (`telemetry-recorder.ts` @ 89b5c64, driver
//! `phase-3/evidence/wp2-tools/dump-recorder.mts`): per tick, which groups the
//! sample line carried (`null` = no line). Covers the five
//! `test/recording-recorder.test.ts` it()s plus rate sweeps 1–20 Hz, an
//! unknown group, the 60 s / 1 MB rotation streams and a cadence override.

mod common;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;
use xorgate_core::recorder_gate::RecorderGate;

#[derive(Deserialize)]
struct Case {
    name: String,
    input: Value,
    expected: Value,
}

/// The step list (`gen.wide` rebuilds the 900-step 1 MB case).
fn steps(input: &Value) -> Vec<(u64, Vec<(String, u64)>)> {
    if let Some(w) = input.pointer("/gen/wide") {
        let n = w["n"].as_u64().unwrap();
        return (0..n)
            .map(|i| (i * 50, vec![("imu".to_string(), i + 1)]))
            .collect();
    }
    input["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let groups = s["groups"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(g, v)| (g.clone(), v[0].as_u64().unwrap()))
                .collect();
            (s["mono"].as_u64().unwrap(), groups)
        })
        .collect()
}

#[test]
fn recorder_gate_matches_the_ts_recorder() {
    let f = common::fixture::<Case>("recorder_gate/telemetry-recorder.json");
    assert_eq!(f.cases.len(), 14);
    for c in &f.cases {
        let rate = c.input["rateHz"].as_f64().unwrap();
        let cadence: Option<IndexMap<String, f64>> = c
            .input
            .get("nativeCadenceHz")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value(v.clone()).unwrap());
        let mut gate = RecorderGate::<u64>::new(cadence);
        let want = c.expected["written"].as_array().unwrap();
        let st = steps(&c.input);
        assert_eq!(st.len(), want.len(), "{}", c.name);
        for (i, (mono, groups)) in st.into_iter().enumerate() {
            let got: Vec<String> = groups
                .into_iter()
                .filter(|(g, um)| gate.admit(g, *um, mono, rate))
                .map(|(g, _)| g)
                .collect();
            let got = if got.is_empty() {
                Value::Null
            } else {
                serde_json::to_value(&got).unwrap()
            };
            assert_eq!(got, want[i], "{} tick {i}", c.name);
        }
    }
}
