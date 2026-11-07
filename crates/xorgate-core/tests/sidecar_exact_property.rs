//! Phase 7a WP2: random physical sessions through the exact plan AND the
//! legacy plan, judged by the backend's own rules. Adopted from the round-3
//! reviewer's property test (`review-wp2-r3-artifacts/scripts/zz_rev3.rs`),
//! with its fixed seed and 200 000 sessions (≈ 2.5 s in a debug build), and
//! assertions instead of counters.
//!
//! Model: per-fragment lags (fragment 0 opens at its first frame, 3–20 ms;
//! N ≥ 1 ≈ 1950–2150 ms); an occasional lag spike or rotation stall (+3 s …
//! +14 s: the host drops a line whose lag is over the 7000 ms guard); lines
//! the agent loses at random; real wall steps of ±8.5 s, 12 s, ±30 s; clean
//! and crash final sweeps; and the agent-exit / power-loss path (in-run live
//! sweeps, then the next boot's recovery with the pts samples on disk).
//!
//! Backend rules: `detectMediaClockStep` (`clock-step.ts:50-62`): a step iff
//! `finalized && durationMs > nominal + 10 000`; `buildSessionTimeline`
//! (`replay-manifest.ts`): a gap iff the hole after a contiguity-bridged
//! finalized segment exceeds `CONTIGUITY_SLACK_MS` = 2000.
//!
//! Asserted:
//! 1. exact never detects a step where there is none and legacy detects none
//!    (no FAKE step), at any fragment (R-WP2R3-1 closed fragment 0);
//! 2. every non-exact row's `durationMs` equals legacy's (± 1 ms);
//! 3. every replay gap the exact plan has and legacy has not is one of the
//!    two known, truthful-or-bounded residual classes, named and counted:
//!    (a) after a capped fragment 0 whose 0→1 rotation stalled > 7 s (the
//!    stall is real); (b) next to a shifted row whose shift L exceeds that
//!    row's own physical lag by > 2000 (a single accepted spike is the
//!    whole seq ≥ 1 median, R-WP2R3-3 residual);
//! 4. after agent exit / power loss, recovery leaves no gap except class
//!    (c), a session that died in fragment 1 with only fragment 0's sample
//!    on disk (recovered with the one-GOP fallback); recovered rows differ
//!    from legacy recovery in `startTs`/`startMono` only, and with no pts
//!    sample they ARE legacy recovery.

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
const SESSIONS: u64 = 200_000;

fn manifest() -> SessionManifest {
    read_session_manifest_text(
        r#"{"v":1,"deviceId":"dev-1","streamKey":"cam0","sessionId":"sess-1","bootId":"boot-1","timeSource":"ntp","preset":"720p15-1mbps","width":1280,"height":720,"fps":15,"bitrateKbps":1000,"segmentSeconds":60,"anchorTs":1760000000000,"anchorMono":"500000000000"}"#,
    )
    .unwrap()
}

fn step_of(sc: &VideoSidecar) -> Option<f64> {
    (sc.finalized && sc.duration_ms > NOM + MIN).then_some(sc.duration_ms - NOM)
}

/// `(fromSeq, hole)` for every seam the backend would turn into a gap.
fn gaps(rows: &[VideoSidecar]) -> Vec<(u64, f64)> {
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
            out.push((c.seq as u64, hole));
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
    let mut residual: BTreeMap<&str, u64> = BTreeMap::new();
    let mut exact_better = 0u64;
    for case in 0..SESSIONS {
        let n = 2 + r.below(6) as usize;
        let mut lag: Vec<f64> = (0..=n)
            .map(|k| {
                if k == 0 {
                    3.0 + r.below(18) as f64
                } else {
                    1_950.0 + r.below(200) as f64
                }
            })
            .collect();
        let stall_at = (r.below(4) == 0).then(|| r.below(n as u64 + 1) as usize);
        if let Some(k) = stall_at {
            lag[k] +=
                [3_000.0, 5_500.0, 8_500.0, 10_500.0, 11_900.0, 14_000.0][r.below(6) as usize];
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
        assert_eq!(ex.len(), lg.len());
        let desc = format!(
            "case {case}: lags {:?} stall_at {stall_at:?} step {step} at {j} lost {lost:?} clean {clean}",
            lag.iter().map(|v| *v as i64).collect::<Vec<_>>()
        );
        for (e, g) in ex.iter().zip(&lg) {
            let k = e.seq as usize;
            let truth = step != 0.0 && k == j;
            // 1. no fake step relative to legacy, at any fragment.
            assert!(
                !(step_of(e).is_some() && !truth && step_of(g).is_none()),
                "{desc}: FAKE step at fragment {k}: exact {} legacy {}",
                e.duration_ms,
                g.duration_ms
            );
            if step_of(e).is_some() && truth && step_of(g).is_none() {
                exact_better += 1; // e.g. a real step inside fragment 0
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
        // 3. new gaps only in the named residual classes.
        let (ge, gl) = (gaps(&ex), gaps(&lg));
        for (s, h) in &ge {
            if gl.iter().any(|(s2, _)| s2 == s) {
                continue;
            }
            let s = *s as usize;
            let row = |k: usize| ex.iter().find(|e| e.seq as usize == k);
            let capped0 = s == 0 && row(0).is_some_and(|e| e.duration_ms == NOM + LAG_GUARD_MAX_MS);
            let stale = [s, s + 1].iter().any(|&k| {
                row(k)
                    .zip(lg.iter().find(|g| g.seq as usize == k))
                    .is_some_and(|(e, g)| {
                        e.anchor == SidecarAnchor::Fstat
                            && e.start_ts != g.start_ts
                            && (g.start_ts - e.start_ts) - (lag[k] + 1.0) > SLACK - 1.0
                    })
            });
            let class = if capped0 {
                "(a) truthful 0->1 stall gap after a capped fragment 0"
            } else if stale {
                "(b) stale shift: a single accepted spike is the whole median"
            } else {
                panic!("{desc}: NEW replay gap after fragment {s}, hole {h}");
            };
            *residual.entry(class).or_default() += 1;
        }
        // 4. agent exit / power loss: live sweeps in-run, then recovery with
        // the pts samples on disk.
        if step == 0.0 && stall_at.is_none() {
            let live = SweepOpts {
                include_newest: false,
                finalized: None,
                time_source: None,
                clock_pair: pair,
            };
            let pl = plan_sweep_timed("/v/c/s", &m, &entries, &live, Some(&x));
            let mut all = rows(&pl);
            let samples: Vec<PtsSample> = all
                .iter()
                .filter(|sc| sc.anchor == EXACT_ANCHOR)
                .map(|sc| PtsSample {
                    seq: sc.seq,
                    start_ts: sc.start_ts,
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
            let rec = plan_sweep_recovering("/v/c/s", &m, &ent2, &rec_opts, &samples);
            // Recovery changes startTs (and startMono) only: every recovered
            // duration, finalized flag and so every backend step figure is
            // legacy recovery's, and no pts sample = legacy recovery exactly.
            let rec_legacy = plan_sweep("/v/c/s", &m, &ent2, &rec_opts);
            if samples.is_empty() {
                assert_eq!(rec, rec_legacy, "{desc}: recovery without pts");
            }
            for (a, b) in rows(&rec).iter().zip(rows(&rec_legacy).iter()) {
                assert_eq!(
                    (a.seq, a.duration_ms, a.finalized, a.anchor, step_of(a)),
                    (b.seq, b.duration_ms, b.finalized, b.anchor, step_of(b)),
                    "{desc}: recovered row differs beyond startTs"
                );
                if a.start_ts != b.start_ts && samples.iter().all(|p| p.seq == 0.0) {
                    *residual
                        .entry("(info) recovered rows shifted by the seq-0-only fallback")
                        .or_default() += 1;
                }
            }
            all.extend(rows(&rec));
            for (s, h) in gaps(&all) {
                let only_seq0 = samples.iter().all(|p| p.seq == 0.0);
                if s == 0 && only_seq0 {
                    *residual
                        .entry("(c) died in fragment 1 with only fragment 0's sample on disk")
                        .or_default() += 1;
                    continue;
                }
                panic!("{desc}: recovery seam gap after fragment {s}, hole {h}");
            }
        }
    }
    for (k, v) in &residual {
        eprintln!("{v:>7}  {k}");
    }
    eprintln!("{exact_better:>7}  real steps exact detects and legacy hides");
}
