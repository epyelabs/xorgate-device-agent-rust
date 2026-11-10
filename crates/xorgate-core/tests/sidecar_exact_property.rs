//! Phase 7a WP2: random physical sessions through the exact plan AND the
//! legacy plan, judged by the backend's own rules. Adopted from the
//! reviewers' property tests (`review-wp2-r3-artifacts/scripts/zz_rev3.rs`,
//! widened per `review-wp2-r4-artifacts/scripts/zz_rev4_stress.rs`), with a
//! fixed seed and assertions instead of counters.
//!
//! Model: 2–15 fragments; per-fragment lags (fragment 0 opens at its first
//! frame, 3–20 ms; N ≥ 1 ≈ 1950–2150 ms plus a per-session offset of −400 …
//! +600 ms); a rotation stall or accepted spike (+3 … +14 s; the host drops
//! a line whose lag is over the 7000 ms guard) and a second spike (+1.5 …
//! +9 s); lines the agent loses at random; real wall steps of ±8.5 s, 12 s,
//! ±30 s; clean and crash final sweeps; and the agent-exit / power-loss path
//! (in-run live sweeps, then the next boot's recovery with the pts samples
//! on disk), under stalls too.
//!
//! Backend rules: `detectMediaClockStep` (`clock-step.ts:50-62`): a step iff
//! `finalized && durationMs > nominal + 10 000`; `buildSessionTimeline`
//! (`replay-manifest.ts`): a gap iff the hole after a contiguity-bridged
//! finalized segment exceeds `CONTIGUITY_SLACK_MS` = 2000.
//!
//! Asserted:
//! 1. no fake step relative to legacy at any fragment (1 ms tolerance: the
//!    file is born ~1 ms after the callback, R-WP2R4-3);
//! 2. every non-exact row's `durationMs` equals legacy's (± 1 ms);
//! 3. every replay gap the exact plan has and legacy has not is in a NAMED
//!    live residual class, and each class's count stays within its bound
//!    for this seed: (a) a truthful gap after a capped fragment 0 whose 0→1
//!    rotation stalled > 7 s; (b) a shifted row next to an exact row whose
//!    lag differs from the shift L by > 2 s (needs an accepted lag spike of
//!    ≳ 4 s), split by seam direction and by where L came from, plus two
//!    consecutive shifted rows whose L differ (a lost run after a spike:
//!    the first gets the midpoint, the next the plain median);
//! 4. recovery after exit / power loss: after a `pts` predecessor the end
//!    anchor makes the recovered tail contiguous, except when its 7000 ms
//!    clamp is hit (the first recovered rotation stalled > 9 s, class (r1));
//!    after a SHIFTED live predecessor (its line was lost) the ruling's end
//!    anchor does not apply and the median fallback can leave a hole (class
//!    (r2)); any other recovery seam gap fails. Recovered rows differ from
//!    legacy recovery in `startTs`/`startMono` only, and with no pts sample
//!    they ARE legacy recovery.

use std::collections::{BTreeMap, BTreeSet};

use xorgate_core::sidecar::*;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const NOM: f64 = 60_000.0;
const MIN: f64 = 10_000.0;
const SLACK: f64 = 2_000.0;
const SESSIONS: u64 = 100_000;

fn manifest() -> SessionManifest {
    read_session_manifest_text(
        r#"{"v":1,"deviceId":"dev-1","streamKey":"cam0","sessionId":"sess-1","bootId":"boot-1","timeSource":"ntp","preset":"720p15-1mbps","width":1280,"height":720,"fps":15,"bitrateKbps":1000,"segmentSeconds":60,"anchorTs":1760000000000,"anchorMono":"500000000000"}"#,
    )
    .unwrap()
}

fn step_of(sc: &VideoSidecar) -> Option<f64> {
    (sc.finalized && sc.duration_ms > NOM + MIN).then_some(sc.duration_ms - NOM)
}

/// `(fromSeq, toSeq, hole)` for every seam the backend would turn into a gap.
fn gaps(rows: &[VideoSidecar]) -> Vec<(u64, u64, f64)> {
    let mut v: Vec<&VideoSidecar> = rows.iter().collect();
    v.sort_by(|a, b| {
        a.start_ts
            .partial_cmp(&b.start_ts)
            .unwrap()
            .then(a.seq.partial_cmp(&b.seq).unwrap())
    });
    let mut out = vec![];
    for w in v.windows(2) {
        let (c, n) = (w[0], w[1]);
        let delta = n.start_ts - c.start_ts;
        let contiguous = c.finalized && delta > 0.0 && delta <= c.duration_ms + SLACK;
        let eff = if contiguous { delta } else { c.duration_ms };
        let hole = n.start_ts - (c.start_ts + eff);
        if hole > SLACK {
            out.push((c.seq as u64, n.seq as u64, hole));
        }
    }
    out
}

fn rows(p: &SweepPlan) -> Vec<VideoSidecar> {
    p.writes
        .iter()
        .map(|w| w.sidecar.typed().unwrap().clone())
        .collect()
}

/// Where a live shift L came from (`TimingTable::shift_lag_below`'s arms).
fn l_source(t: &TimingTable, id: u64) -> &'static str {
    let pred = id >= 2 && t.exact(id - 1).is_some();
    let end = if pred { id - 1 } else { id };
    let m = (1..end).any(|k| t.exact(k).is_some());
    match (pred, m) {
        (true, true) => "midpoint",
        (true, false) => "exact predecessor alone (no median)",
        (false, true) => "median",
        (false, false) => "lag(0) fallback",
    }
}

#[test]
#[allow(clippy::needless_range_loop)]
fn random_physical_sessions_vs_legacy_under_backend_rules() {
    let m = manifest();
    let mut r = Lcg(20_261_106);
    let t0 = 1_760_000_000_000.0;
    let pair = ClockPair {
        wall_ms: t0 + 9e6,
        mono_ms: 1e7,
    };
    let since = BTreeMap::new();
    let mut classes: BTreeMap<String, u64> = BTreeMap::new();
    let mut info: BTreeMap<&str, u64> = BTreeMap::new();
    for case in 0..SESSIONS {
        let n = 2 + r.below(14) as usize;
        let off = [-400.0, -200.0, 0.0, 300.0, 600.0][r.below(5) as usize];
        let mut lag: Vec<f64> = (0..=n)
            .map(|k| {
                if k == 0 {
                    3.0 + r.below(18) as f64
                } else {
                    1_950.0 + off + r.below(200) as f64
                }
            })
            .collect();
        let stall_at = (r.below(4) == 0).then(|| r.below(n as u64 + 1) as usize);
        if let Some(k) = stall_at {
            lag[k] +=
                [3_000.0, 5_500.0, 8_500.0, 10_500.0, 11_900.0, 14_000.0][r.below(6) as usize];
        }
        if r.below(3) == 0 {
            let k2 = 1 + r.below(n as u64) as usize;
            lag[k2] += [1_500.0, 3_000.0, 4_500.0, 6_000.0, 9_000.0][r.below(5) as usize];
        }
        let step = if r.below(4) == 0 {
            [8_500.0, -8_500.0, 30_000.0, -30_000.0, 12_000.0][r.below(5) as usize]
        } else {
            0.0
        };
        let j = r.below(n as u64) as usize;
        let w = |k: usize| if step != 0.0 && k > j { step } else { 0.0 };
        let fr = |k: usize| t0 + 300.0 + k as f64 * NOM + w(k);
        let cb = |k: usize| fr(k) + lag[k];
        let mut t = TimingTable::default();
        let mut lost = BTreeSet::new();
        for k in 0..=n {
            let emitted = lag[k] <= LAG_GUARD_MAX_MS;
            let agent_lost = r.below(5) == 0;
            if emitted && !agent_lost {
                t.record_start(k as u64, fr(k), lag[k], cb(k)).unwrap();
            } else {
                lost.insert(k);
            }
        }
        let entries: Vec<DirEntry> = (0..n)
            .map(|k| DirEntry {
                name: format!("{k:05}.mp4"),
                stat: Some(FragmentStat {
                    size_bytes: 10.0,
                    birthtime_ms: cb(k) + 1.0,
                    mtime_ms: cb(k + 1),
                }),
            })
            .collect();
        let clean = r.below(2) == 0;
        let opts = SweepOpts {
            include_newest: true,
            finalized: clean.then_some(true),
            time_source: None,
            clock_pair: pair,
        };
        let x = ExactSweep {
            table: &t,
            deferred_since: &since,
            now_ms: 0.0,
            defer_max_ms: 10_000.0,
        };
        let ex = rows(&plan_sweep_timed("/v/c/s", &m, &entries, &opts, Some(&x)));
        let lg = rows(&plan_sweep("/v/c/s", &m, &entries, &opts));
        assert_eq!(ex.len(), lg.len(), "row counts");
        let desc = format!(
            "case {case}: lags {:?} stall_at {stall_at:?} step {step} at {j} lost {lost:?} clean {clean}",
            lag.iter().map(|v| *v as i64).collect::<Vec<_>>()
        );
        for (e, g) in ex.iter().zip(&lg) {
            assert_eq!(e.seq, g.seq);
            let k = e.seq as usize;
            let truth = step != 0.0 && k == j;
            // 1. no fake step relative to legacy (1 ms tolerance).
            let fake = step_of(e).is_some()
                && !truth
                && step_of(g).is_none()
                && e.duration_ms > NOM + MIN + 1.0;
            assert!(
                !fake,
                "{desc}: FAKE step at fragment {k}: exact {} legacy {}",
                e.duration_ms, g.duration_ms
            );
            if step_of(e).is_some() && truth && step_of(g).is_none() {
                *info
                    .entry("real steps exact detects and legacy hides")
                    .or_default() += 1;
            }
            // 2. non-exact rows keep legacy's duration.
            if e.anchor != EXACT_ANCHOR {
                assert!(
                    (e.duration_ms - g.duration_ms).abs() <= 1.0,
                    "{desc}: non-exact fragment {k} duration {} vs legacy {}",
                    e.duration_ms,
                    g.duration_ms
                );
            }
        }
        // 3. new live gaps only in the named classes.
        let kind = |k: u64| -> &'static str {
            let (e, g) = (
                ex.iter().find(|e| e.seq as u64 == k).unwrap(),
                lg.iter().find(|g| g.seq as u64 == k).unwrap(),
            );
            if e.anchor == EXACT_ANCHOR {
                "exact"
            } else if e.start_ts != g.start_ts {
                "shifted"
            } else {
                "legacy"
            }
        };
        let legacy_gaps: BTreeSet<u64> = gaps(&lg).iter().map(|g| g.0).collect();
        for (a, b, h) in gaps(&ex) {
            if legacy_gaps.contains(&a) {
                continue;
            }
            let capped0 = a == 0
                && ex
                    .iter()
                    .any(|e| e.seq == 0.0 && e.duration_ms == NOM + LAG_GUARD_MAX_MS);
            let class = match (capped0, kind(a), kind(b)) {
                (true, _, _) => "(a) truthful 0->1 stall gap after a capped fragment 0".to_string(),
                (_, "exact", "shifted") => format!(
                    "(b) exact -> shifted, lag(N) - L > 2 s, L from {}",
                    l_source(&t, b)
                ),
                (_, "shifted", "exact") => format!(
                    "(b) shifted -> exact, L - lag(N+1) > 2 s, L from {}",
                    l_source(&t, a)
                ),
                (_, "shifted", "shifted") => format!(
                    "(b) shifted -> shifted inside a lost run, L from {} then {}",
                    l_source(&t, a),
                    l_source(&t, b)
                ),
                (_, ka, kb) => {
                    panic!("{desc}: UNCLASSIFIED live gap {ka} {a} -> {kb} {b}, hole {h}")
                }
            };
            *classes.entry(class).or_default() += 1;
        }
        // 4. agent exit / power loss: live sweeps in-run, then recovery with
        // the pts samples on disk (stalls included).
        if step == 0.0 {
            let live = SweepOpts {
                include_newest: false,
                finalized: None,
                time_source: None,
                clock_pair: pair,
            };
            let pl = plan_sweep_timed("/v/c/s", &m, &entries, &live, Some(&x));
            let live_rows = rows(&pl);
            let samples: Vec<PtsSample> = live_rows
                .iter()
                .filter(|sc| sc.anchor == EXACT_ANCHOR)
                .map(|sc| PtsSample {
                    seq: sc.seq,
                    start_ts: sc.start_ts,
                    duration_ms: sc.duration_ms,
                    birthtime_ms: cb(sc.seq as usize) + 1.0,
                })
                .collect();
            let mut ent2 = entries.clone();
            for w in &pl.writes {
                ent2.push(DirEntry {
                    name: w.sidecar_path.rsplit('/').next().unwrap().to_string(),
                    stat: None,
                });
            }
            let rec_opts = SweepOpts {
                include_newest: true,
                finalized: None,
                time_source: None,
                clock_pair: pair,
            };
            let rec = rows(&plan_sweep_recovering(
                "/v/c/s", &m, &ent2, &rec_opts, &samples,
            ));
            let rec_legacy_plan = plan_sweep("/v/c/s", &m, &ent2, &rec_opts);
            if samples.is_empty() {
                assert_eq!(
                    plan_sweep_recovering("/v/c/s", &m, &ent2, &rec_opts, &samples),
                    rec_legacy_plan,
                    "{desc}: recovery without pts"
                );
            }
            let rec_legacy = rows(&rec_legacy_plan);
            assert_eq!(rec.len(), rec_legacy.len(), "{desc}: recovered row counts");
            for (a, b) in rec.iter().zip(rec_legacy.iter()) {
                assert_eq!(
                    (a.seq, a.duration_ms, a.finalized, a.anchor, step_of(a)),
                    (b.seq, b.duration_ms, b.finalized, b.anchor, step_of(b)),
                    "{desc}: recovered row differs beyond startTs"
                );
            }
            let recovered: BTreeSet<u64> = rec.iter().map(|r| r.seq as u64).collect();
            let live_gaps: BTreeSet<(u64, u64)> =
                gaps(&live_rows).iter().map(|g| (g.0, g.1)).collect();
            let mut all = live_rows.clone();
            all.extend(rec.iter().cloned());
            for (a, b, h) in gaps(&all) {
                if live_gaps.contains(&(a, b)) {
                    continue; // an in-run seam: item 3's business
                }
                if !(recovered.contains(&a) || recovered.contains(&b)) {
                    continue;
                }
                // Classify by the in-run predecessor P of the recovered row b.
                let p_row = all.iter().find(|r| r.seq as u64 == a).unwrap();
                let live_legacy = rows(&plan_sweep("/v/c/s", &m, &entries, &live));
                let class = if recovered.contains(&a) {
                    panic!("{desc}: gap between two recovered rows {a} -> {b}, hole {h}")
                } else if p_row.anchor == EXACT_ANCHOR {
                    // End anchor applied; only its 7000 ms clamp can leave a
                    // hole: K's file was born > 9 s after P's end (a stall).
                    let k_birth = cb(b as usize) + 1.0;
                    let behind = k_birth - (p_row.start_ts + p_row.duration_ms);
                    assert!(
                        behind > LAG_GUARD_MAX_MS + SLACK,
                        "{desc}: end-anchored recovery seam {a} -> {b}, hole {h}, behind {behind}"
                    );
                    "(r1) recovery: first recovered rotation stalled > 9 s (end anchor clamped at 7000)"
                } else if live_legacy
                    .iter()
                    .find(|g| g.seq as u64 == a)
                    .is_some_and(|g| g.start_ts != p_row.start_ts)
                {
                    "(r2) recovery after a SHIFTED live row (its line lost): no pts end anchor, median fallback"
                } else {
                    panic!(
                        "{desc}: recovery seam after an unshifted legacy row {a} -> {b}, hole {h}"
                    )
                };
                *classes.entry(class.to_string()).or_default() += 1;
            }
            if !recovered.is_empty() {
                *info.entry("sessions with recovered rows").or_default() += 1;
            }
        }
    }
    for (k, v) in &classes {
        eprintln!("{v:>7}  {k}");
    }
    for (k, v) in &info {
        eprintln!("{v:>7}  (info) {k}");
    }
    // Bounded per class for this seed (± 10 %, at least ± 5): a change that
    // makes a class grow, or a new class appear, fails here.
    let bound: &[(&str, u64)] = BOUNDS;
    for (k, v) in &classes {
        let Some((_, want)) = bound.iter().find(|(n, _)| n == k) else {
            panic!("an unbounded new class: {k} ({v})");
        };
        let tol = (want / 10).max(5);
        assert!(v.abs_diff(*want) <= tol, "{k}: {v} vs bound {want} ± {tol}");
    }
}

/// The residual classes and their counts at the fixed seed (measured; see
/// `phase-7a/evidence/wp2.md` §14).
const BOUNDS: &[(&str, u64)] = &[
    ("(a) truthful 0->1 stall gap after a capped fragment 0", 2969),
    ("(b) exact -> shifted, lag(N) - L > 2 s, L from midpoint", 468),
    ("(b) shifted -> exact, L - lag(N+1) > 2 s, L from exact predecessor alone (no median)", 274),
    ("(b) shifted -> exact, L - lag(N+1) > 2 s, L from lag(0) fallback", 82),
    ("(b) shifted -> exact, L - lag(N+1) > 2 s, L from median", 98),
    ("(b) shifted -> exact, L - lag(N+1) > 2 s, L from midpoint", 367),
    ("(b) shifted -> shifted inside a lost run, L from midpoint then median", 62),
    ("(r1) recovery: first recovered rotation stalled > 9 s (end anchor clamped at 7000)", 53),
    ("(r2) recovery after a SHIFTED live row (its line lost): no pts end anchor, median fallback", 54),
];
