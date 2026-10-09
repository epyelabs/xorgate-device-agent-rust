//! Serde-exactness tests for `xorgate_core::telemetry`: every shape in
//! `fixtures/telemetry/payload-shapes.json` (JSON.stringify output built from
//! the TS types in worker/manager key order) must parse into the Rust type
//! and re-serialise byte-identically.

mod common;

use serde::Deserialize;
use xorgate_core::telemetry::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    json: String,
}

fn round_trip<T: serde::Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
    json: &str,
    name: &str,
) -> T {
    let v: T = serde_json::from_str(json).unwrap_or_else(|e| panic!("{name}: {e}"));
    let out = serde_json::to_string(&v).unwrap();
    assert_eq!(out, json, "{name}: byte-identical re-serialisation");
    let back: T = serde_json::from_str(&out).unwrap();
    assert_eq!(back, v, "{name}: round trip");
    v
}

#[test]
fn payload_shapes() {
    let f = common::fixture::<Case>("telemetry/payload-shapes.json");
    for c in &f.cases {
        match c.ty.as_str() {
            "TelemetryPayload" => {
                let p: TelemetryPayload = round_trip(&c.json, &c.name);
                // The dynamic map lifts to the typed groups and back.
                let typed = TelemetryMetrics::from_metrics(&p.metrics)
                    .unwrap_or_else(|e| panic!("{}: typed metrics: {e}", c.name));
                assert_eq!(typed.clone().into_metrics(), p.metrics, "{}: typed ↔ map", c.name);
                if c.name.starts_with("full payload") {
                    let gps = typed.gps.as_ref().unwrap();
                    assert_eq!(gps.sats_heard.as_ref().unwrap().value, JsNumber(14.0));
                    assert_eq!(gps.course.as_ref().unwrap().unit.as_deref(), Some("°"));
                    assert!(typed.imu.is_some() && typed.lte.is_some() && typed.system.is_some());
                    let r = p.recording.as_ref().unwrap();
                    assert_eq!(r.boot_defer.as_deref(), Some("released:budget 5000mA >= 5000mA"));
                    assert_eq!(p.media.streams[1].recording, None);
                    assert_eq!(p.media.streams[0].recording, Some(true));
                }
                if c.name.starts_with("recording with") {
                    let r = p.recording.as_ref().unwrap();
                    assert_eq!(r.power_shed.as_deref(), Some("parked:no-live"));
                    assert_eq!(r.live_block.as_ref().unwrap().reason, "undervoltage");
                }
                if c.name.starts_with("sim/offline") {
                    assert!(p.recording.is_none());
                }
            }
            "StateSnapshot" => {
                let s: StateSnapshot = round_trip(&c.json, &c.name);
                if c.name.contains("minimal") {
                    assert_eq!(s.media_enabled, None);
                    assert_eq!(s.rate_hz, JsNumber(0.5));
                    assert_eq!(s.connection, ConnectionState::Connecting);
                    assert!(s.recording.is_none() && s.latest.is_none());
                } else {
                    assert_eq!(s.mode, RunMode::Real);
                    assert_eq!(s.recent_events[1].level, ActivityLevel::Warn);
                    assert_eq!(s.media_enabled, Some(true));
                }
            }
            "TelemetryReading" => {
                let _: TelemetryReading = round_trip(&c.json, &c.name);
            }
            other => panic!("unknown type {other}"),
        }
    }
    assert_eq!(f.cases.len(), 7);
}

#[test]
fn enums_print_like_the_ts_unions() {
    assert_eq!(serde_json::to_string(&RunMode::Replay).unwrap(), r#""replay""#);
    assert_eq!(RunMode::parse("bogus"), None);
    assert_eq!(serde_json::to_string(&ConnectionState::Reconnecting).unwrap(), r#""reconnecting""#);
    assert_eq!(ConnectionState::Error.as_str(), "error");
    let e = ActivityEvent {
        ts: 1,
        level: ActivityLevel::Info,
        msg: "agent starting (mode=real)".into(),
    };
    assert_eq!(
        serde_json::to_string(&e).unwrap(),
        r#"{"ts":1,"level":"info","msg":"agent starting (mode=real)"}"#
    );
}
