//! Phase 7a R-WP2R3-2 (R7 amended for one field): crash recovery shifts the
//! recovered `fstat` rows of a session an `exact` run left behind, by the
//! clamped median of `birth − startTs` over its `anchor:"pts"` sidecars
//! (`plan_sweep_recovering`). Without a usable `pts` sidecar it IS today's
//! `plan_sweep`, byte for byte (differential test over every TS sweep step
//! and every TS recovery session).
//!
//! Timeline (independent of the code): frame wall `F_k = T0 + 300 + 60 000k`,
//! callback wall `cb_k = F_k + lag_k`, birth `cb_k + 1`, mtime `cb_{k+1}` (or
//! a cut). An in-run exact sidecar k has `startTs = F_k`, so its sample is
//! `birth − startTs = lag_k + 1`.

use std::collections::BTreeMap;

use serde_json::Value;
use xorgate_core::sidecar::*;

const T0: f64 = 1_760_000_000_000.0;
const SEG: f64 = 60_000.0;
const DIR: &str = "/v/cam0/sess-x";

fn manifest() -> SessionManifest {
    read_session_manifest_text(
        r#"{"v":1,"deviceId":"dev-1","streamKey":"cam0","sessionId":"sess-x","bootId":"b","timeSource":"ntp","preset":"720p15-1mbps","width":1280,"height":720,"fps":15,"bitrateKbps":1000,"segmentSeconds":60,"anchorTs":1760000000000,"anchorMono":"5"}"#,
    )
    .unwrap()
}

struct Tl {
    lag: Vec<f64>,
}

impl Tl {
    fn new(lags: &[f64]) -> Self {
        Self { lag: lags.to_vec() }
    }
    fn f(&self, k: usize) -> f64 {
        T0 + 300.0 + k as f64 * SEG
    }
    fn cb(&self, k: usize) -> f64 {
        self.f(k) + self.lag[k]
    }
    /// Files 0..n (mtime = the next callback unless overridden), plus a
    /// sidecar NAME for every seq in `sidecars`.
    fn entries(&self, n: usize, sidecars: &[usize], mtime: &BTreeMap<usize, f64>) -> Vec<DirEntry> {
        let mut v: Vec<DirEntry> = (0..n)
            .map(|k| DirEntry {
                name: format!("{k:05}.mp4"),
                stat: Some(FragmentStat {
                    size_bytes: 1000.0 + k as f64,
                    birthtime_ms: self.cb(k) + 1.0,
                    mtime_ms: mtime.get(&k).copied().unwrap_or_else(|| self.cb(k + 1)),
                }),
            })
            .collect();
        v.extend(sidecars.iter().map(|k| DirEntry {
            name: format!("{k:05}.json"),
            stat: None,
        }));
        v
    }
    /// The pts samples of in-run exact sidecars `ks`.
    fn samples(&self, ks: &[usize]) -> Vec<PtsSample> {
        ks.iter()
            .map(|&k| PtsSample {
                seq: k as f64,
                start_ts: self.f(k),
                birthtime_ms: self.cb(k) + 1.0,
            })
            .collect()
    }
}

fn opts() -> SweepOpts {
    SweepOpts {
        include_newest: true,
        finalized: None,
        time_source: None,
        clock_pair: ClockPair {
            wall_ms: T0 + 900_000.0,
            mono_ms: 1_000_000.0,
        },
    }
}

fn typed(p: &SweepPlan) -> BTreeMap<u64, VideoSidecar> {
    p.writes
        .iter()
        .map(|w| {
            let sc = w.sidecar.typed().unwrap().clone();
            (sc.seq as u64, sc)
        })
        .collect()
}

/// A recovered row = the legacy row with only `startTs` (and `startMono`)
/// moved earlier by `lag`.
fn assert_shifted(got: &VideoSidecar, legacy: &VideoSidecar, lag: f64) {
    let mut want = legacy.clone();
    assert_eq!(want.anchor, SidecarAnchor::Fstat);
    want.start_ts -= lag;
    want.start_mono -= lag;
    assert_eq!(got, &want);
}

/// Agent exit with fragment 3 still deferred and 4 newest: both are left to
/// recovery. Samples 0..2 (lags 12, 6000, 2100: the median of the seq ≥ 1
/// ones is NOT their mean). Recovered rows start at their first frames and
/// the seams stay within the backend's 2000 ms slack.
#[test]
fn exit_with_fragments_unswept_recovers_them_shifted_and_contiguous() {
    let t = Tl::new(&[12.0, 6_000.0, 2_100.0, 2_100.0, 2_100.0, 2_100.0]);
    let entries = t.entries(5, &[0, 1, 2], &BTreeMap::new());
    let samples = t.samples(&[0, 1, 2]);
    // median(6001, 2101) = round(4051) — two seq ≥ 1 samples; add a third so
    // median and mean differ.
    let mut samples3 = samples.clone();
    samples3.push(PtsSample {
        seq: 2.0,
        start_ts: t.f(2),
        birthtime_ms: t.cb(2) + 1.0,
    });
    assert_eq!(
        recovery_shift_lag(&samples3),
        Some(2_101.0),
        "median, not mean 3401"
    );
    let p = plan_sweep_recovering(DIR, &manifest(), &entries, &opts(), &samples3);
    let l = plan_sweep(DIR, &manifest(), &entries, &opts());
    let (got, leg) = (typed(&p), typed(&l));
    assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![3, 4]);
    for k in [3, 4] {
        assert_shifted(&got[&k], &leg[&k], 2_101.0);
        assert_eq!(
            got[&k].start_ts,
            t.f(k as usize),
            "starts at its first frame"
        );
    }
    assert!(got[&3].finalized && !got[&4].finalized);
    // pts 2 (in-run, successor 3 stamped) ends at F3; recovered 3 starts there.
    let pts2_end = t.f(2) + (t.f(3) - t.f(2));
    assert!(got[&3].start_ts - pts2_end <= 2_000.0);
    let hole = got[&4].start_ts - (got[&3].start_ts + got[&3].duration_ms);
    assert!(hole <= 2_000.0, "hole {hole}");
    assert_eq!(p.reap, l.reap);
}

/// Power loss mid-fragment: the newest file was cut 30 s in; recovered as
/// `finalized:false`, shifted, its legacy duration kept.
#[test]
fn power_loss_mid_fragment_shifts_the_cut_newest() {
    let t = Tl::new(&[12.0, 2_100.0, 2_100.0, 2_100.0, 2_100.0]);
    let cut = BTreeMap::from([(3, t.cb(3) + 30_000.0)]);
    let entries = t.entries(4, &[0, 1, 2], &cut);
    let p = plan_sweep_recovering(DIR, &manifest(), &entries, &opts(), &t.samples(&[0, 1, 2]));
    let l = plan_sweep(DIR, &manifest(), &entries, &opts());
    let (got, leg) = (typed(&p), typed(&l));
    assert_shifted(&got[&3], &leg[&3], 2_101.0);
    assert!(!got[&3].finalized);
    assert_eq!(got[&3].duration_ms, 29_999.0);
    // pts 2 ends at F3 (its successor's stamp was known in-run).
    assert!(got[&3].start_ts - t.f(3) <= 2_000.0);
}

/// No pts predecessor = no shift: (a) no pts sidecar at all is the legacy
/// plan byte for byte; (b) a pts sidecar only ABOVE the recovered fragment
/// does not shift it (but does shift the newest after it); (c) a
/// `computed` row (no btime) is never shifted.
#[test]
fn no_pts_predecessor_means_no_shift() {
    let t = Tl::new(&[12.0, 2_100.0, 2_100.0, 2_100.0, 2_100.0, 2_100.0]);
    let entries = t.entries(5, &[0, 3], &BTreeMap::new());
    let legacy = plan_sweep(DIR, &manifest(), &entries, &opts());
    assert_eq!(
        plan_sweep_recovering(DIR, &manifest(), &entries, &opts(), &[]),
        legacy
    );
    // 0 and 3 sidecar'd, 3 pts; 1 and 2 (a failed write, say) and 4 recovered.
    let p = plan_sweep_recovering(DIR, &manifest(), &entries, &opts(), &t.samples(&[3]));
    let (got, leg) = (typed(&p), typed(&legacy));
    assert_eq!(got[&1], leg[&1], "1 has no pts sidecar below it");
    assert_eq!(got[&2], leg[&2]);
    assert_shifted(&got[&4], &leg[&4], 2_101.0);
    // (c) no btime on the recovered fragment: computed, untouched.
    let mut nb = entries.clone();
    for e in &mut nb {
        if e.name == "00004.mp4" {
            e.stat.as_mut().unwrap().birthtime_ms = 0.0;
        }
    }
    let p = plan_sweep_recovering(DIR, &manifest(), &nb, &opts(), &t.samples(&[3]));
    let l = plan_sweep(DIR, &manifest(), &nb, &opts());
    assert_eq!(typed(&p)[&4].anchor, SidecarAnchor::Computed);
    assert_eq!(p, l);
}

/// A pts sample with a missing or odd birthtime (or startTs) is skipped;
/// when every sample is unusable, recovery is the legacy plan.
#[test]
fn unusable_pts_samples_are_skipped() {
    let t = Tl::new(&[12.0, 2_100.0, 2_100.0, 2_100.0, 2_100.0]);
    let entries = t.entries(4, &[0, 1, 2], &BTreeMap::new());
    let odd = vec![
        PtsSample {
            seq: 1.0,
            start_ts: t.f(1),
            birthtime_ms: 0.0,
        },
        PtsSample {
            seq: 2.0,
            start_ts: t.f(2),
            birthtime_ms: f64::NAN,
        },
        PtsSample {
            seq: 2.0,
            start_ts: f64::NAN,
            birthtime_ms: t.cb(2) + 1.0,
        },
    ];
    let legacy = plan_sweep(DIR, &manifest(), &entries, &opts());
    assert_eq!(
        plan_sweep_recovering(DIR, &manifest(), &entries, &opts(), &odd),
        legacy
    );
    let mut some = odd.clone();
    some.push(PtsSample {
        seq: 1.0,
        start_ts: t.f(1),
        birthtime_ms: t.cb(1) + 1.0 + 400.0, // a usable sample: 2501
    });
    let p = plan_sweep_recovering(DIR, &manifest(), &entries, &opts(), &some);
    assert_shifted(&typed(&p)[&3], &typed(&legacy)[&3], 2_501.0);
}

/// Differential: with no pts sample, `plan_sweep_recovering` equals
/// `plan_sweep` on every `sweepSessionDir` step of the JS-dumped fixture and
/// on every session of its `recoverOrphanVideoSessions` cases.
#[test]
fn without_pts_recovery_is_the_legacy_plan_on_every_ts_fixture() {
    let p = format!(
        "{}/../../fixtures/sidecar/video-segments.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let mut n = 0;
    for c in v["cases"].as_array().unwrap() {
        let pair: ClockPair = serde_json::from_value(
            c["input"]
                .get("clockPair")
                .cloned()
                .unwrap_or_else(|| v["frozenClocks"].clone()),
        )
        .unwrap();
        let o = |include_newest: bool, finalized: Option<bool>| SweepOpts {
            include_newest,
            finalized,
            time_source: None,
            clock_pair: pair,
        };
        match c["op"].as_str().unwrap() {
            "sweepSessionDir" => {
                let dir = c["input"]["dir"].as_str().unwrap();
                let m: SessionManifest = parse_session_manifest(&c["input"]["manifest"])
                    .unwrap()
                    .into();
                for step in c["expected"].as_array().unwrap() {
                    let entries: Vec<DirEntry> =
                        serde_json::from_value(step["entries"].clone()).unwrap();
                    let so = &step["opts"];
                    let opts = SweepOpts {
                        include_newest: so["includeNewest"].as_bool().unwrap(),
                        finalized: so.get("finalized").and_then(Value::as_bool),
                        time_source: so
                            .get("timeSource")
                            .map(|t| serde_json::from_value(t.clone()).unwrap()),
                        clock_pair: serde_json::from_value(so["clockPair"].clone()).unwrap(),
                    };
                    assert_eq!(
                        plan_sweep_recovering(dir, &m, &entries, &opts, &[]),
                        plan_sweep(dir, &m, &entries, &opts)
                    );
                    n += 1;
                }
            }
            "recoverOrphanVideoSessions" => {
                for s in c["input"]["sessions"].as_array().unwrap() {
                    let Some(m) = s["manifestText"]
                        .as_str()
                        .and_then(read_session_manifest_text)
                    else {
                        continue;
                    };
                    let entries: Vec<DirEntry> =
                        serde_json::from_value(s["entries"].clone()).unwrap();
                    let opts = o(true, None);
                    assert_eq!(
                        plan_sweep_recovering("/r", &m, &entries, &opts, &[]),
                        plan_sweep("/r", &m, &entries, &opts)
                    );
                    n += 1;
                }
            }
            _ => {}
        }
    }
    assert!(n >= 8, "only {n} plans compared");
}
