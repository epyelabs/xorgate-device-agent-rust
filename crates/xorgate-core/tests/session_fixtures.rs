//! Fixture-driven tests for `xorgate_core::session`, from
//! `test/recording-session.test.ts` › mintSessionId (3) and › timeSource
//! tracker with offline clock steps (3) + TS-oracle extras.
//!
//! The TS mints from `randomBytes(16)`, which a fixture cannot replay, so
//! each recorded id is checked by re-minting it from ITS OWN bytes with the
//! timestamp bytes zeroed and the version/variant bits inverted: the result
//! must equal the TS id byte for byte, which pins the 48-bit `now` encoding
//! (incl. negative / fractional / ≥ 2^48 inputs) and both bit masks.
//! Not fixture-able (I/O, kept for Phase 3 HAL tests): › boot id + time
//! source "returns a stable per-process boot id" (reads /proc + caches per
//! process; its rule is `resolve_boot_id`) and "probes to one of the three
//! provenance values without throwing" (execs timedatectl; its rule is
//! `probe_verdict`) — both unit-tested in `session.rs`.

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::session::*;
use xorgate_core::types::{SteppedClockSource, TimeSource};

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

fn bytes_of(id: &str) -> [u8; 16] {
    let hex: String = id.chars().filter(|c| *c != '-').collect();
    assert_eq!(hex.len(), 32, "{id}");
    let mut b = [0u8; 16];
    for (i, out) in b.iter_mut().enumerate() {
        *out = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    b
}

/// `Math.max(0, Math.floor(now))` for the fixture's JS number.
fn js_now(v: &Value) -> i64 {
    v.as_f64().unwrap().floor() as i64
}

fn check_id(now: i64, id: &str) {
    assert!(is_uuid_v7_shaped(id), "{id}");
    let mut random = bytes_of(id);
    random[..6].fill(0);
    random[6] ^= 0xf0; // scramble the version nibble …
    random[8] ^= 0xc0; // … and the variant bits: mint must restore both.
    assert_eq!(mint_session_id(now, random), id, "now={now}");
}

/// `/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/`.
fn is_uuid_v7_shaped(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    let hex = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && hex(p))
        && parts[2].starts_with('7')
        && parts[3].starts_with(['8', '9', 'a', 'b'])
}

#[test]
fn recording_session_test_ts() {
    let f = common::fixture::<Case>("session/session.json");
    for c in &f.cases {
        let i = &c.input;
        match c.op.as_str() {
            "mintSessionId" => check_id(js_now(&i["now"]), c.expected.as_str().unwrap()),
            "mintSessionIdSort" => {
                let nows: Vec<i64> = i["nows"].as_array().unwrap().iter().map(js_now).collect();
                let ids: Vec<String> = serde_json::from_value(c.expected["ids"].clone()).unwrap();
                for (now, id) in nows.iter().zip(&ids) {
                    check_id(*now, id);
                }
                // Re-minted with fresh "random" bytes, the order still follows `now`.
                let mut ours: Vec<String> = nows
                    .iter()
                    .rev()
                    .map(|n| mint_session_id(*n, [0xa5; 16]))
                    .collect();
                ours.sort();
                let by_time: Vec<String> = nows
                    .iter()
                    .map(|n| mint_session_id(*n, [0xa5; 16]))
                    .collect();
                assert_eq!(ours, by_time, "{}", c.name);
                let sorted: Vec<String> =
                    serde_json::from_value(c.expected["sorted"].clone()).unwrap();
                assert_eq!(sorted, ids, "{}", c.name);
            }
            "mintSessionIdMany" => {
                let now = js_now(&i["now"]);
                let ids: Vec<String> = serde_json::from_value(c.expected.clone()).unwrap();
                assert_eq!(ids.len() as u64, i["count"].as_u64().unwrap());
                let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
                assert_eq!(unique.len(), ids.len(), "{}", c.name);
                for id in &ids {
                    check_id(now, id);
                }
            }
            "timeSourceTracker" => {
                let now = i["now"].as_i64().unwrap();
                let mut t = TimeSourceTracker::new(i["refreshMs"].as_i64().unwrap());
                for (k, step) in c.expected.as_array().unwrap().iter().enumerate() {
                    match step["op"].as_str().unwrap() {
                        "refresh" => {
                            let probed: ProbedTimeSource =
                                serde_json::from_value(step["probe"].clone()).unwrap();
                            t.begin_refresh();
                            let got = t.apply_probe(probed, now);
                            let want: TimeSource =
                                serde_json::from_value(step["expect"].clone()).unwrap();
                            assert_eq!(got, want, "{} step {k}", c.name);
                        }
                        "noteStep" => {
                            let s: SteppedClockSource =
                                serde_json::from_value(step["source"].clone()).unwrap();
                            t.note_step(s);
                        }
                        "current" => {
                            let want: TimeSource =
                                serde_json::from_value(step["expect"].clone()).unwrap();
                            assert_eq!(t.current(now).0, want, "{} step {k}", c.name);
                            // When due, the TS kicks a background probe it never
                            // awaits; in these scripts it lands on the same value
                            // (a step outranks its rtc/unsynced answer), so it is
                            // modelled as dropped.
                            t.probe_failed();
                        }
                        other => panic!("unknown step {other}"),
                    }
                }
            }
            other => panic!("unknown op {other}"),
        }
    }
    let vitest = f
        .cases
        .iter()
        .filter(|c| !c.name.starts_with("extra:"))
        .count();
    assert_eq!(vitest, 6);
}
