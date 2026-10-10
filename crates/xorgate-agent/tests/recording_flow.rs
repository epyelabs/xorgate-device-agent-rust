//! `fixtures/recording_flow/manager.json` — the real TS `RecordingManager`
//! (89b5c64) driven through 27 scenario scripts (the 25 RecordingManager
//! `it()`s of `test/recording-video-capture.test.ts` with deterministic
//! clocks + two invariant-7 scenarios), replayed against the Rust
//! `RecordingManager` over the fake backend: every manager event (level +
//! msg), every `setLiveShed` / `setDisabledCameras` call, every capture
//! spawn `{cam, record}`, the live set, the manager-owned `status()` fields
//! (key presence included — `liveBlock` ABSENT, never null) and the
//! `videoKey` must match step by step.

#[path = "recording_support/mod.rs"]
mod recording_support;

use recording_support::*;
use serde_json::{json, Value};
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::types::{GnssView, PowerSampleReading};

const JS_COMMIT: &str = "89b5c6407ef88e20f7168bd767d22a7be04ebcb6";

fn fixture() -> Value {
    let p = format!(
        "{}/../../fixtures/recording_flow/manager.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let f: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(f["jsCommit"], JS_COMMIT);
    assert_eq!(f["jsSourceFile"], "src/recording/manager.ts");
    f
}

fn status_view(s: &xorgate_core::telemetry::RecordingStatus) -> Value {
    let full = serde_json::to_value(s).unwrap();
    let mut out = json!({ "enabled": s.enabled, "session": s.session_id.is_some() });
    for k in ["bootDefer", "powerShed", "liveBlock"] {
        if let Some(v) = full.get(k) {
            out[k] = v.clone();
        }
    }
    out
}

fn level(l: ActivityLevel) -> &'static str {
    match l {
        ActivityLevel::Info => "info",
        ActivityLevel::Warn => "warn",
        ActivityLevel::Error => "error",
    }
}

async fn run_case(c: &Value) -> usize {
    let name = c["name"].as_str().unwrap();
    let mut rig = Rig::new("flow");
    if let Some(cams) = c.get("cameras").and_then(Value::as_array) {
        rig.cameras = cams
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
    }
    rig.media_enabled = c.get("mediaEnabled") != Some(&Value::Bool(false));
    rig.video = c.get("video") != Some(&Value::Bool(false));
    rig.power_sample = c.get("powerSample") != Some(&Value::Bool(false));
    rig.gnss_wired = c.get("gnssWired") != Some(&Value::Bool(false));
    rig.budget_ma = c.get("budgetMa").and_then(Value::as_u64).map(|v| v as u32);
    *rig.gnss.lock().unwrap() = c
        .get("gnss")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value::<GnssView>(v.clone()).unwrap());
    *rig.reading.lock().unwrap() = c
        .get("reading")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value::<PowerSampleReading>(v.clone()).unwrap());
    if let Some(s) = c.get("settings") {
        rig.save_recording(s);
    }
    let m = rig.build();
    let (mut ev0, mut ls0, mut dc0, mut sp0) = (0, 0, 0, 0);
    let mut steps = 0;
    for (i, step) in c["steps"].as_array().unwrap().iter().enumerate() {
        let at = format!("{name} [step {i} {}]", step["op"]);
        match step["op"].as_str().unwrap() {
            "start" => m.start().await,
            "settings" => {
                rig.save_recording(&step["recording"]);
                m.reapply();
            }
            "reapply" => m.reapply(),
            "telemetry" => match step.get("wrote") {
                Some(t) => std::fs::write(rig.telemetry_path(), t.as_str().unwrap()).unwrap(),
                None => {
                    let _ = std::fs::remove_file(rig.telemetry_path());
                }
            },
            "gnss" => {
                *rig.gnss.lock().unwrap() =
                    Some(serde_json::from_value(step["value"].clone()).unwrap())
            }
            "reading" => {
                *rig.reading.lock().unwrap() =
                    Some(serde_json::from_value(step["value"].clone()).unwrap())
            }
            "advance" => rig.clock.advance(step["ms"].as_u64().unwrap()),
            "shedTicks" => {
                for _ in 0..step["n"].as_u64().unwrap() {
                    m.shed_tick_now().await;
                    m.settled().await;
                }
            }
            other => panic!("{at}: unknown op {other}"),
        }
        m.settled().await;
        let exp = &step["expected"];
        let events: Vec<Value> = rig.log.events.lock().unwrap()[ev0..]
            .iter()
            .map(|(l, msg)| json!({ "level": level(*l), "msg": msg }))
            .collect();
        assert_eq!(Value::Array(events), exp["events"], "{at}: events");
        let ls: Vec<bool> = rig.log.live_shed.lock().unwrap()[ls0..].to_vec();
        assert_eq!(json!(ls), exp["liveShed"], "{at}: setLiveShed calls");
        let dc: Vec<Vec<String>> = rig.log.disabled.lock().unwrap()[dc0..].to_vec();
        assert_eq!(json!(dc), exp["disabled"], "{at}: setDisabledCameras calls");
        let sp: Vec<Value> = rig.spawns()[sp0..]
            .iter()
            .map(|s| json!({ "cam": s.cam, "record": s.record }))
            .collect();
        assert_eq!(Value::Array(sp), exp["spawns"], "{at}: spawns");
        assert_eq!(json!(rig.live_cams()), exp["live"], "{at}: live children");
        assert_eq!(status_view(&m.status()), exp["status"], "{at}: status");
        assert_eq!(json!(m.video_key()), exp["videoKey"], "{at}: videoKey");
        let (wall, mono) = rig.clock.get();
        assert_eq!(
            json!({"wall": wall, "mono": mono}),
            exp["clock"],
            "{at}: clock"
        );
        assert_eq!(
            json!(rig
                .log
                .sample_calls
                .load(std::sync::atomic::Ordering::SeqCst)),
            exp["sampleCalls"],
            "{at}: powerSample calls"
        );
        ev0 = rig.log.events.lock().unwrap().len();
        ls0 = rig.log.live_shed.lock().unwrap().len();
        dc0 = rig.log.disabled.lock().unwrap().len();
        sp0 = rig.spawns().len();
        steps += 1;
    }
    m.stop();
    steps
}

#[tokio::test]
async fn recording_manager_flow_matches_ts() {
    let f = fixture();
    let cases = f["cases"].as_array().unwrap();
    let mut steps = 0;
    for c in cases {
        steps += run_case(c).await;
    }
    assert_eq!(cases.len(), 27);
    assert!(steps >= 80, "{steps} steps");
}
