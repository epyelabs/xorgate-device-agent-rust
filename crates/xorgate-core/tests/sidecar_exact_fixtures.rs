//! Phase 7a WP2: exact sidecar timing (`plan_sweep_timed`, `TimingTable`,
//! `exact_times`) against `fixtures/sidecar/exact-timing.json`.
//!
//! Rust-only behaviour (JS 09da38d emits birthtime anchors only), so the
//! fixture is not a TS dump: its expected values come from a simulated
//! capture timeline (frame wall, host callback wall = frame + lag, birth =
//! callback + 1 ms, mtime = next callback) and the §8.4 rules, computed
//! outside the Rust code. Fragments that must fall back are listed by name
//! and compared BYTE FOR BYTE with the legacy plan of the same listing.
//!
//! Each case is a sequence of sweeps over one session; the harness plays
//! the agent: it feeds the timing lines into one `TimingTable`, keeps the
//! `deferred_since` map from each plan's `deferred`, and adds every written
//! sidecar to the next listing.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::sidecar::*;

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "deferMaxMs")]
    defer_max_ms: f64,
    manifest: Value,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    #[serde(rename = "nominalMs")]
    nominal_ms: Option<f64>,
    #[serde(rename = "stepMs")]
    step_ms: Option<f64>,
    #[serde(rename = "steppedSeq")]
    stepped_seq: Option<f64>,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    lines: Vec<Value>,
    entries: Vec<DirEntry>,
    opts: Value,
    #[serde(rename = "nowMs")]
    now_ms: f64,
    expect: Value,
}

const DIR: &str = "/v/cam0/sess-x";

fn load() -> Fixture {
    let p = format!(
        "{}/../../fixtures/sidecar/exact-timing.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

fn sweep_opts(o: &Value) -> SweepOpts {
    SweepOpts {
        include_newest: o["includeNewest"].as_bool().unwrap(),
        finalized: o.get("finalized").and_then(Value::as_bool),
        time_source: None,
        clock_pair: serde_json::from_value(o["clockPair"].clone()).unwrap(),
    }
}

fn base(p: &str) -> &str {
    p.rsplit('/').next().unwrap()
}

fn is_int(v: f64) -> bool {
    v.is_finite() && v.fract() == 0.0
}

#[test]
fn exact_timing_fixtures() {
    let f = load();
    let m: SessionManifest = parse_session_manifest(&f.manifest).unwrap().into();
    assert!(f.cases.len() >= 17, "fixture lost cases");
    for c in &f.cases {
        let mut table = TimingTable::default();
        let mut deferred_since: BTreeMap<u64, f64> = BTreeMap::new();
        let mut written: Vec<String> = Vec::new();
        // seq → (written sidecar) for the clock-step check
        let mut by_seq: BTreeMap<u64, VideoSidecar> = BTreeMap::new();
        for (k, step) in c.steps.iter().enumerate() {
            let ctx = format!("{} step {k}", c.name);
            let mut rejected = Vec::new();
            for line in &step.lines {
                if let Some(s) = line.get("start") {
                    let id = s["id"].as_u64().unwrap();
                    let r = table.record_start(
                        id,
                        s["startTs"].as_f64().unwrap(),
                        s["lagMs"].as_f64().unwrap(),
                    );
                    if r.is_err() {
                        rejected.push(id);
                    }
                } else {
                    let e = &line["end"];
                    table
                        .record_end(e["id"].as_u64().unwrap(), e["endTs"].as_f64().unwrap())
                        .unwrap();
                }
            }
            let want_rejected: Vec<u64> =
                serde_json::from_value(step.expect["rejected"].clone()).unwrap();
            assert_eq!(rejected, want_rejected, "{ctx}: rejected");

            let mut entries = step.entries.clone();
            entries.extend(written.iter().map(|n| DirEntry {
                name: n.clone(),
                stat: None,
            }));
            let opts = sweep_opts(&step.opts);
            let x = ExactSweep {
                table: &table,
                deferred_since: &deferred_since,
                now_ms: step.now_ms,
                defer_max_ms: f.defer_max_ms,
            };
            let plan = plan_sweep_timed(DIR, &m, &entries, &opts, Some(&x));
            let legacy = plan_sweep(DIR, &m, &entries, &opts);

            let want_deferred: Vec<u64> =
                serde_json::from_value(step.expect["deferred"].clone()).unwrap();
            assert_eq!(plan.deferred, want_deferred, "{ctx}: deferred");
            if opts.include_newest {
                assert!(plan.deferred.is_empty(), "{ctx}: a final sweep deferred");
            }
            // Same reaps and active path as legacy: timing changes values only.
            assert_eq!(plan.reap, legacy.reap, "{ctx}: reap");
            assert_eq!(plan.active_path, legacy.active_path, "{ctx}: activePath");
            if let Some(want) = step.expect.get("reap") {
                let got: Vec<&str> = plan.reap.iter().map(|p| base(p)).collect();
                assert_eq!(serde_json::to_value(got).unwrap(), *want, "{ctx}: reap");
            }
            if let Some(want) = step.expect.get("activePath") {
                assert_eq!(
                    serde_json::to_value(plan.active_path.as_deref().map(base)).unwrap(),
                    *want,
                    "{ctx}: activePath"
                );
            }

            let exact_want = step.expect["exact"].as_object().unwrap();
            let legacy_want: BTreeSet<String> =
                serde_json::from_value(step.expect["legacy"].clone()).unwrap();
            let mut seen = BTreeSet::new();
            for w in &plan.writes {
                let name = base(&w.sidecar_path).to_string();
                seen.insert(name.clone());
                let sc = w.sidecar.typed().expect("typed manifest").clone();
                // Backend schema (media-segments.ts:51-53) for EVERY sidecar.
                assert!(sc.start_ts > 0.0 && is_int(sc.start_ts), "{ctx} {name}: startTs");
                assert!(
                    sc.duration_ms >= 0.0 && is_int(sc.duration_ms),
                    "{ctx} {name}: durationMs"
                );
                assert!(is_int(sc.start_mono), "{ctx} {name}: startMono");
                // startMono's derivation is unchanged (sidecar.rs build_sidecar).
                let pair = opts.clock_pair;
                assert_eq!(
                    sc.start_mono,
                    (pair.mono_ms - (pair.wall_ms - sc.start_ts)).round(),
                    "{ctx} {name}: startMono derivation"
                );
                if let Some(e) = exact_want.get(&name) {
                    assert_eq!(sc.anchor, EXACT_ANCHOR, "{ctx} {name}: anchor");
                    assert_eq!(sc.start_ts, e["startTs"].as_f64().unwrap(), "{ctx} {name}: startTs");
                    assert_eq!(
                        sc.duration_ms,
                        e["durationMs"].as_f64().unwrap(),
                        "{ctx} {name}: durationMs"
                    );
                    assert_eq!(
                        sc.start_mono,
                        e["startMono"].as_f64().unwrap(),
                        "{ctx} {name}: startMono"
                    );
                    assert_eq!(sc.finalized, e["finalized"].as_bool().unwrap(), "{ctx} {name}");
                    // Only startTs/durationMs/anchor differ from the legacy body.
                    if let Some(l) = legacy.writes.iter().find(|l| l.sidecar_path == w.sidecar_path)
                    {
                        let mut lv = l.sidecar.typed().unwrap().clone();
                        lv.start_ts = sc.start_ts;
                        lv.start_mono = sc.start_mono;
                        lv.duration_ms = sc.duration_ms;
                        lv.anchor = sc.anchor;
                        assert_eq!(lv, sc, "{ctx} {name}: other fields");
                    }
                } else if legacy_want.contains(&name) {
                    let l = legacy
                        .writes
                        .iter()
                        .find(|l| l.sidecar_path == w.sidecar_path)
                        .unwrap_or_else(|| panic!("{ctx} {name}: not in the legacy plan"));
                    assert_eq!(w.sidecar.to_json(), l.sidecar.to_json(), "{ctx} {name}: legacy bytes");
                    assert_ne!(sc.anchor, EXACT_ANCHOR, "{ctx} {name}");
                    if let Some(a) = step.expect.get("legacyAnchor").and_then(|a| a.get(&name)) {
                        assert_eq!(serde_json::to_value(sc.anchor).unwrap(), *a, "{ctx} {name}");
                    }
                } else {
                    panic!("{ctx}: unexpected sidecar {name}");
                }
                // A deferred fragment is written no later than defer_max after
                // its first deferral (or as soon as its line arrives).
                if let Some(since) = deferred_since.get(&(sc.seq as u64)) {
                    assert!(
                        step.now_ms - since <= f.defer_max_ms,
                        "{ctx} {name}: delayed past one poll period"
                    );
                }
                by_seq.insert(sc.seq as u64, sc);
            }
            let want_all: BTreeSet<String> = exact_want
                .keys()
                .cloned()
                .chain(legacy_want.iter().cloned())
                .collect();
            assert_eq!(seen, want_all, "{ctx}: written set");
            // The agent's bookkeeping: keep the first deferral instant.
            deferred_since.retain(|id, _| plan.deferred.contains(id));
            for id in &plan.deferred {
                deferred_since.entry(*id).or_insert(step.now_ms);
            }
            written.extend(seen);
        }
        // Clock step between N and N+1: exactly fragment N moves, by S
        // (what clock-step.ts:50-62 needs); floored at 0 (int ≥ 0).
        if let (Some(nom), Some(s), Some(n)) = (c.nominal_ms, c.step_ms, c.stepped_seq) {
            let moved: Vec<u64> = by_seq
                .iter()
                .filter(|(_, sc)| sc.duration_ms != nom)
                .map(|(k, _)| *k)
                .collect();
            assert_eq!(moved, vec![n as u64], "{}: exactly one fragment moved", c.name);
            assert_eq!(by_seq[&(n as u64)].duration_ms, (nom + s).max(0.0), "{}", c.name);
        }
    }
}

/// An exact-mode sweep with an EMPTY table (an untimed run: `gst-launch`, or
/// a host whose clock precondition failed) plans exactly what the TS plans:
/// every `sweepSessionDir` step of the JS-dumped `video-segments.json`.
#[test]
fn untimed_exact_sweep_equals_legacy_on_every_ts_fixture_step() {
    let p = format!(
        "{}/../../fixtures/sidecar/video-segments.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let table = TimingTable::default();
    let since = BTreeMap::new();
    let mut n = 0;
    for c in v["cases"].as_array().unwrap() {
        if c["op"] != "sweepSessionDir" {
            continue;
        }
        let dir = c["input"]["dir"].as_str().unwrap();
        let m: SessionManifest = parse_session_manifest(&c["input"]["manifest"])
            .unwrap()
            .into();
        for step in c["expected"].as_array().unwrap() {
            let entries: Vec<DirEntry> = serde_json::from_value(step["entries"].clone()).unwrap();
            let o = &step["opts"];
            let opts = SweepOpts {
                include_newest: o["includeNewest"].as_bool().unwrap(),
                finalized: o.get("finalized").and_then(Value::as_bool),
                time_source: o.get("timeSource").map(|t| serde_json::from_value(t.clone()).unwrap()),
                clock_pair: serde_json::from_value(o["clockPair"].clone()).unwrap(),
            };
            let x = ExactSweep {
                table: &table,
                deferred_since: &since,
                now_ms: 0.0,
                defer_max_ms: 10_000.0,
            };
            let timed = plan_sweep_timed(dir, &m, &entries, &opts, Some(&x));
            assert_eq!(timed, plan_sweep(dir, &m, &entries, &opts), "{}", c["name"]);
            assert_eq!(timed, plan_sweep_timed(dir, &m, &entries, &opts, None));
            n += 1;
        }
    }
    assert!(n >= 5, "only {n} sweep steps");
}
