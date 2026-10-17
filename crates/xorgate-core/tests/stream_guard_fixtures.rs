//! Fixture-driven test for `xorgate_core::stream_guard`, from
//! `fixtures/stream_guard/verdict.json`: the REAL `config.media.streamGuard`
//! closure (`src/worker.ts:209-220`, sliced and evaluated by
//! `phase-3/evidence/wp5-tools/dump-stream-guard.mts`) over block × armed ×
//! reconcile status × per-stream `.gst` present × global fallback (80 cases).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::stream_guard::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    input: Value,
    expected: String,
}

#[test]
fn worker_ts_stream_guard_verdicts() {
    let f = common::fixture::<Case>("stream_guard/verdict.json");
    assert_eq!(f.cases.len(), 80);
    for c in &f.cases {
        let i = &c.input;
        let status = i["status"]
            .as_str()
            .map(|s| StreamHardwareStatus::parse(s).unwrap());
        // resolveChannelPipeline: `<dir>/<key>.gst` exists → it, else the global fallback.
        let resolves = i["pipelinePresent"].as_bool().unwrap() || i["globalPipeline"].is_string();
        let got = stream_guard_verdict(
            i["hasBlock"].as_bool().unwrap(),
            i["armed"].as_bool().unwrap(),
            status,
            || resolves,
        );
        assert_eq!(got.as_str(), c.expected, "{}", c.name);
    }
}
