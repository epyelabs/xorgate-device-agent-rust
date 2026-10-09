//! Fixture-driven tests for `xorgate_core::sidecar`, from
//! `test/recording-video-segments.test.ts` (all 23 it() sites = 25 cases:
//! the 3-row `for … of` loop is three). F cases (sweep / recover / manifest)
//! are the tmp tree the TS touched, as a directory LISTING with stats →
//! what the TS wrote/deleted — replayed through `plan_sweep` /
//! `plan_recover_orphan_sessions`. Where the vitest passed no clock pair,
//! the dumper froze `Date.now`/`monotonicMs` and the fixture records the
//! pair (`clockPairInjected: true`).
//!
//! WP6 review S1: "extra: untyped manifest …" cases run `buildSidecar` over
//! manifests that pass the TS rule but not the typed parser (copy-through +
//! JS `ToNumber` on `segmentSeconds`), and "extra: a TS-valid manifest with
//! odd or missing fields is still recovered" pins the recovery gate.

mod common;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::clock::FixedClock;
use xorgate_core::sidecar::*;
use xorgate_core::types::TimeSource;
use xorgate_core::video_presets::*;

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

fn manifest(v: &Value) -> VideoSessionManifest {
    parse_session_manifest(v).expect("fixture manifest")
}

fn de<T: for<'de> Deserialize<'de>>(v: &Value) -> T {
    serde_json::from_value(v.clone()).unwrap()
}

fn time_source(v: &Value) -> Option<TimeSource> {
    v.get("timeSource").map(de)
}

fn sidecar_opts(o: &Value) -> SidecarOpts {
    SidecarOpts {
        finalized: o["finalized"].as_bool().unwrap(),
        boundary_end_ms: o.get("boundaryEndMs").and_then(Value::as_f64),
        time_source: time_source(o),
        clock_pair: de(&o["clockPair"]),
    }
}

fn sweep_opts(o: &Value) -> SweepOpts {
    SweepOpts {
        include_newest: o["includeNewest"].as_bool().unwrap(),
        finalized: o.get("finalized").and_then(Value::as_bool),
        time_source: time_source(o),
        clock_pair: de(&o["clockPair"]),
    }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap()
}

#[test]
fn recording_video_segments_test_ts() {
    let f = common::fixture::<Case>("sidecar/video-segments.json");
    let frozen: ClockPair = de(&f.extra["frozenClocks"]);
    let mut its = std::collections::BTreeSet::new();
    for c in &f.cases {
        its.insert(c.name.split(" [").next().unwrap().to_string());
        let i = &c.input;
        let e = &c.expected;
        match c.op.as_str() {
            "presets" => {
                if let Some(found) = e.get("find1080p30") {
                    let p = find_video_preset("1080p30-4mbps").unwrap();
                    assert_eq!(&serde_json::to_value(p).unwrap(), found);
                    assert_eq!(resolve_video_preset("nope").name, e["resolveNope"]);
                    assert_eq!(
                        serde_json::to_value(video_preset_names()).unwrap(),
                        e["names"]
                    );
                    assert_eq!(DEFAULT_VIDEO_PRESET, e["DEFAULT_VIDEO_PRESET"]);
                } else {
                    let d = xorgate_core::recording_config::recording_defaults();
                    assert_eq!(d.video.preset, e["defaultPreset"]);
                    let p = find_video_preset(&d.video.preset).unwrap();
                    assert_eq!(serde_json::to_value(p).unwrap(), e["found"]);
                }
            }
            "segmentSeq" => {
                let got = segment_seq(i["name"].as_str().unwrap());
                assert_eq!(got, e.as_f64(), "{}", c.name);
            }
            "sidecarPathFor" => {
                assert_eq!(
                    sidecar_path_for(i["mp4Path"].as_str().unwrap()),
                    *e,
                    "{}",
                    c.name
                );
            }
            "buildSidecar" => {
                let m = read_session_manifest(&i["manifest"]).expect("TS-valid manifest");
                let body = build_sidecar_body(
                    &m,
                    i["seq"].as_f64().unwrap(),
                    de(&i["stat"]),
                    &sidecar_opts(&i["opts"]),
                );
                let json = body.to_json();
                assert_eq!(json, e["json"].as_str().unwrap(), "{}", c.name);
                if i["clockPairInjected"] == true {
                    assert_eq!(sidecar_opts(&i["opts"]).clock_pair, frozen);
                }
                match &body {
                    SidecarBody::Typed(sc) => {
                        let back: VideoSidecar = serde_json::from_str(&json).unwrap();
                        assert_eq!(&back, sc, "{}: round trip", c.name);
                    }
                    SidecarBody::Raw(_) => {
                        assert!(c.name.starts_with("extra: untyped manifest"), "{}", c.name);
                        assert_eq!(parse_session_manifest(&i["manifest"]), None);
                    }
                }
            }
            "buildSidecarSeries" => {
                let m = manifest(&i["manifest"]);
                let calls = e.as_array().unwrap();
                assert_eq!(calls.len(), 4);
                let mut prev: Option<VideoSidecar> = None;
                for (k, call) in calls.iter().enumerate() {
                    let sc = build_sidecar(
                        &m,
                        call["seq"].as_f64().unwrap(),
                        de(&call["stat"]),
                        &sidecar_opts(&call["opts"]),
                    );
                    assert_eq!(
                        serde_json::to_string(&sc).unwrap(),
                        call["json"].as_str().unwrap(),
                        "{} seq {k}",
                        c.name
                    );
                    // The it()'s own invariants: sane, contiguous, mono in the boot frame.
                    assert_eq!(sc.start_ts, 1_750_000_000_000.0 + k as f64 * 60_000.0);
                    assert_eq!(sc.start_mono, 500_000.0 + k as f64 * 60_000.0);
                    if let Some(p) = &prev {
                        assert_eq!(sc.start_ts - (p.start_ts + p.duration_ms), 0.0);
                    }
                    prev = Some(sc);
                }
            }
            "oldStyleMono" => {
                // M0 + (startTs(seq 0) − (T0 + 53 y)) < 0, while the new sidecar's
                // startMono (pinned by the backward-step series) stays > 0.
                let (m0, t0, d) = (
                    i["M0"].as_f64().unwrap(),
                    i["T0"].as_f64().unwrap(),
                    i["delta"].as_f64().unwrap(),
                );
                let old = m0 + (t0 - (t0 + d));
                assert_eq!(old, e["oldStyle"].as_f64().unwrap());
                assert!(old < 0.0);
                let m = VideoSessionManifest {
                    anchor_ts: t0 + d,
                    ..manifest(&series_manifest(&f.cases))
                };
                let sc = build_sidecar(
                    &m,
                    0.0,
                    FragmentStat {
                        size_bytes: 1000.0,
                        birthtime_ms: t0,
                        mtime_ms: t0 + 60_000.0,
                    },
                    &SidecarOpts {
                        finalized: true,
                        boundary_end_ms: Some(t0 + 60_000.0),
                        time_source: None,
                        clock_pair: ClockPair {
                            wall_ms: t0 + 240_000.0,
                            mono_ms: m0 + 240_000.0,
                        },
                    },
                );
                assert!(sc.start_mono > 0.0);
            }
            "sweepSessionDir" => {
                let dir = i["dir"].as_str().unwrap();
                let m = manifest(&i["manifest"]);
                for (k, step) in c.expected.as_array().unwrap().iter().enumerate() {
                    let entries: Vec<DirEntry> = de(&step["entries"]);
                    let opts = sweep_opts(&step["opts"]);
                    if step["clockPairInjected"] == true {
                        assert_eq!(opts.clock_pair, frozen);
                    }
                    let plan = plan_sweep(dir, &m.clone().into(), &entries, &opts);
                    let want = &step["expected"];
                    let res = plan.result();
                    assert_eq!(
                        serde_json::to_value(&res.closed).unwrap(),
                        want["closed"],
                        "{} step {k}",
                        c.name
                    );
                    assert_eq!(
                        serde_json::to_value(&res.active_path).unwrap(),
                        want["activePath"],
                        "{} step {k}",
                        c.name
                    );
                    let written: BTreeMap<String, String> = plan
                        .writes
                        .iter()
                        .map(|w| {
                            assert_eq!(w.sidecar_path, sidecar_path_for(&w.mp4_path));
                            (basename(&w.sidecar_path).to_string(), w.sidecar.to_json())
                        })
                        .collect();
                    let want_written: BTreeMap<String, String> = de(&want["written"]);
                    assert_eq!(written, want_written, "{} step {k}", c.name);
                    let deleted: Vec<&str> = plan.reap.iter().map(|p| basename(p)).collect();
                    assert_eq!(
                        serde_json::to_value(deleted).unwrap(),
                        want["deleted"],
                        "{} step {k}",
                        c.name
                    );
                }
            }
            "manifestRoundTrip" => {
                let m = manifest(&i["manifest"]);
                // writeSessionManifest = atomicWriteJson = compact JSON.stringify.
                let text = serde_json::to_string(&m).unwrap();
                assert_eq!(text, e["text"].as_str().unwrap(), "{}", c.name);
                assert_eq!(parse_session_manifest_text(&text), Some(m));
                assert_eq!(
                    serde_json::to_value(parse_session_manifest_text(&text)).unwrap(),
                    e["read"]
                );
            }
            "readSessionManifestText" => {
                let text = i["text"].as_str();
                // The exact TS rule: non-null iff valid.
                let valid = text
                    .and_then(|t| serde_json::from_str::<Value>(t).ok())
                    .is_some_and(|v| is_valid_session_manifest(&v));
                assert_eq!(valid, !e.is_null(), "{}", c.name);
                // `read_session_manifest_text` IS the TS rule (WP6 review S1).
                assert_eq!(
                    text.and_then(read_session_manifest_text).is_some(),
                    !e.is_null(),
                    "{}",
                    c.name
                );
                // The typed parser is stricter only for incomplete manifests.
                let typed = text.and_then(parse_session_manifest_text);
                if e.is_null() {
                    assert_eq!(typed, None, "{}", c.name);
                } else if typed.is_none() {
                    assert!(
                        e.get("deviceId").is_none(),
                        "{}: strict only on partial",
                        c.name
                    );
                }
            }
            "recoverOrphanVideoSessions" => {
                let root = i["videoRoot"].as_str().unwrap();
                let sessions: Vec<SessionListing> = i["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| SessionListing {
                        stream_key: s["streamKey"].as_str().unwrap().into(),
                        session_id: s["sessionId"].as_str().unwrap().into(),
                        manifest: s["manifestText"]
                            .as_str()
                            .and_then(read_session_manifest_text),
                        entries: de(&s["entries"]),
                    })
                    .collect();
                let active: Vec<String> = de(&i["activeSessionIds"]);
                let pair: ClockPair = de(&i["clockPair"]);
                let clock = FixedClock::new(pair.wall_ms as i64, pair.mono_ms as u64);
                let plan = plan_recover_orphan_sessions(root, &sessions, &active, &clock);
                assert_eq!(
                    plan.recovered as u64,
                    e["recovered"].as_u64().unwrap(),
                    "{}",
                    c.name
                );
                let written: BTreeMap<String, String> = plan
                    .sweeps
                    .iter()
                    .flat_map(|(_, p)| p.writes.iter())
                    .map(|w| (w.sidecar_path.clone(), w.sidecar.to_json()))
                    .collect();
                let want: BTreeMap<String, String> = de(&e["written"]);
                assert_eq!(written, want, "{}", c.name);
            }
            other => panic!("unknown op {other}"),
        }
    }
    // Every it() of the suite: 23 static sites (inventory count), one of
    // which is the 3-row clock-step loop → 25 cases; plus the dumper's extras.
    let vitest = its.iter().filter(|n| !n.starts_with("extra:")).count();
    assert_eq!(vitest, 25, "{its:#?}");
}

/// The manifest the clock-step series used (for the `oldStyleMono` case).
fn series_manifest(cases: &[Case]) -> Value {
    cases
        .iter()
        .find(|c| c.op == "buildSidecarSeries")
        .map(|c| c.input["manifest"].clone())
        .unwrap()
}

#[test]
fn log_lines_are_verbatim() {
    assert_eq!(
        dropped_empty_fragment_warning("/v/cam0/s/00002.mp4"),
        "[recording] dropped empty fragment /v/cam0/s/00002.mp4"
    );
    assert_eq!(
        sidecar_write_failed_error("/v/x.mp4", "EROFS"),
        "[recording] sidecar write failed for /v/x.mp4: EROFS"
    );
}
