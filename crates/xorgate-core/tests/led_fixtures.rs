//! Fixture-driven tests for `xorgate_core::led` — the pure half of
//! `src/hardware/led.ts` (`test/led.test.ts`: 12 of 26 `it()`s; the other 14
//! drive `LedDriver`, listed under `notFixtureAble` in the fixture and in
//! evidence/wp3.md — Phase 4 HAL).

mod common;

use serde::Deserialize;
use serde_json::{Map, Value};
use xorgate_core::led::*;

#[derive(Deserialize)]
struct Call {
    op: String,
    input: Value,
    expected: Value,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    constants: Vec<String>,
    calls: Vec<Call>,
}

fn led_colours_object() -> Value {
    let mut m = Map::new();
    for (c, ch) in LED_COLOURS {
        m.insert(c.as_str().to_string(), serde_json::to_value(ch).unwrap());
    }
    Value::Object(m)
}

#[test]
fn led_test_ts() {
    let f = common::fixture::<Case>("led/led.json");
    // Exported tables, pinned verbatim (preserve_order: key order included).
    assert_eq!(led_colours_object(), f.extra["LED_COLOURS"], "LED_COLOURS");
    assert_eq!(
        serde_json::to_value(LED_COLOUR_NAMES).unwrap(),
        f.extra["LED_COLOUR_NAMES"]
    );
    assert_eq!(
        serde_json::to_value(LED_PATTERN_NAMES).unwrap(),
        f.extra["LED_PATTERN_NAMES"]
    );
    assert_eq!(
        serde_json::to_value(status_led_map()).unwrap(),
        f.extra["STATUS_LED_MAP"]
    );
    assert_eq!(
        f.extra["notFixtureAble"]["its"].as_array().unwrap().len(),
        14
    );

    let mut calls = 0;
    for c in &f.cases {
        assert!(!c.calls.is_empty() || !c.constants.is_empty(), "{}", c.name);
        for (i, call) in c.calls.iter().enumerate() {
            let at = format!("{} [call {i}]", c.name);
            let got = match call.op.as_str() {
                "ledTimeline" => {
                    let p = LedPattern::parse(call.input.as_str().unwrap()).unwrap();
                    serde_json::to_value(led_timeline(p)).unwrap()
                }
                "validatePinMap" => {
                    let map: LedPinMap = serde_json::from_value(call.input["map"].clone()).unwrap();
                    let busy: Vec<u32> =
                        serde_json::from_value(call.input["busyPins"].clone()).unwrap();
                    serde_json::to_value(validate_pin_map(&map, &busy)).unwrap()
                }
                "parseBusyPins" => {
                    serde_json::to_value(parse_busy_pins(call.input.as_str().unwrap())).unwrap()
                }
                other => panic!("{at}: unknown op {other}"),
            };
            assert_eq!(got, call.expected, "{at}");
            calls += 1;
        }
    }
    // "has no yellow/amber": the closed colour set.
    assert!(!is_led_colour("yellow") && !is_led_colour("amber"));
    for name in LED_COLOUR_NAMES {
        assert!(is_led_colour(name.as_str()));
    }
    for p in LED_PATTERN_NAMES {
        assert!(is_led_pattern(p.as_str()));
    }
    assert_eq!(f.cases.len(), 12);
    assert_eq!(calls, 15);
}
