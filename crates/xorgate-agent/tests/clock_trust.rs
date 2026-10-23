//! `ops::clock_trust` runtime (Phase 5b WP1, `ts:src/agent/clock-trust.ts`
//! @ 09da38d): the 5 s probe timer (re-armed from the fire), the 30 s slow
//! poll after the fallback flip, the latch stopping the timer, and the
//! publish gate (`PhaseHooks::publish_clock_gate`: held ticks are counted,
//! never published). The decisions and strings are pinned in core
//! (`fixtures/clock_trust/clock-trust.json`); this pins the timing.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use xorgate_agent::ctx::{AgentClock, PhaseHooks};
use xorgate_agent::ops::clock_trust::{ClockTrust, ClockTrustOptions, TrustProbe};
use xorgate_core::clock_step::EventLevel;
use xorgate_core::clock_trust::ClockTrustState;
use xorgate_core::session::ProbedTimeSource;
use xorgate_core::types::SteppedClockSource;

type Events = Arc<Mutex<Vec<(EventLevel, String)>>>;

struct Rig {
    trust: ClockTrust,
    probes: Arc<AtomicUsize>,
    events: Events,
}

/// `answers[i]` for the i-th probe (the last one repeats).
fn rig(answers: Vec<ProbedTimeSource>) -> Rig {
    let probes = Arc::new(AtomicUsize::new(0));
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let p = probes.clone();
    let answers = Arc::new(answers);
    let probe: TrustProbe = Box::new(move || {
        let n = p.fetch_add(1, Ordering::SeqCst);
        let a = answers[n.min(answers.len() - 1)];
        Box::pin(async move { Ok(a) })
    });
    let ev = events.clone();
    let trust = ClockTrust::new(ClockTrustOptions::new(
        probe,
        Arc::new(AgentClock::following_tokio(1_800_000_000_000)),
        Box::new(move |l, m| ev.lock().unwrap().push((l, m.to_string()))),
    ));
    Rig {
        trust,
        probes,
        events,
    }
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// Advance in ≤ 1 s steps so every timer fire in between runs (a single
/// big jump coalesces the missed fires of a re-arm-from-fire interval).
async fn advance(ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(1_000);
        tokio::time::advance(Duration::from_millis(step)).await;
        settle().await;
        left -= step;
    }
}

#[tokio::test(start_paused = true)]
async fn polls_every_5_s_from_start_and_stops_once_ntp_lands() {
    use ProbedTimeSource::{Ntp, Unsynced};
    let r = rig(vec![Unsynced, Unsynced, Unsynced, Ntp]);
    r.trust.start();
    settle().await;
    assert_eq!(
        r.probes.load(Ordering::SeqCst),
        1,
        "the immediate first tick"
    );
    advance(4_999).await;
    assert_eq!(r.probes.load(Ordering::SeqCst), 1);
    advance(1).await;
    assert_eq!(r.probes.load(Ordering::SeqCst), 2);
    r.trust.note_held();
    advance(5_000).await;
    advance(5_000).await;
    assert_eq!(r.probes.load(Ordering::SeqCst), 4);
    assert!(r.trust.trusted());
    assert!(!r.trust.timer_armed(), "latched trust clears the interval");
    let s = r.trust.status();
    assert_eq!(s.state, ClockTrustState::Trusted);
    assert_eq!(s.held, 1);
    assert_eq!(s.held_ms, 15_000);
    assert_eq!(
        r.events.lock().unwrap().clone(),
        vec![(
            EventLevel::Info,
            "[clock] ntp synchronized — clock trusted (telemetry held 15 s, 1 sample(s))"
                .to_string()
        )]
    );
    advance(60_000).await;
    assert_eq!(
        r.probes.load(Ordering::SeqCst),
        4,
        "no probe after the latch"
    );
}

#[tokio::test(start_paused = true)]
async fn the_fallback_flip_switches_to_the_30_s_poll() {
    let r = rig(vec![ProbedTimeSource::Rtc]);
    r.trust.start();
    settle().await;
    // 299 s: 1 + 59 probes, still held.
    advance(299_000).await;
    assert_eq!(r.probes.load(Ordering::SeqCst), 60);
    assert!(!r.trusted_now());
    // The 300 s probe tick flips it (checkGrace inside tick) → slow poll.
    advance(1_000).await;
    assert_eq!(r.probes.load(Ordering::SeqCst), 61);
    assert!(r.trusted_now());
    assert_eq!(r.trust.status().state, ClockTrustState::Fallback);
    assert!(r.trust.timer_armed());
    advance(29_999).await;
    assert_eq!(r.probes.load(Ordering::SeqCst), 61);
    advance(1).await;
    assert_eq!(
        r.probes.load(Ordering::SeqCst),
        62,
        "30 s cadence in fallback"
    );
    let ev = r.events.lock().unwrap().clone();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, EventLevel::Warn);
    assert!(ev[0]
        .1
        .starts_with("[clock] no NTP/GPS/NITZ sync 300 s after start"));
}

#[tokio::test(start_paused = true)]
async fn a_gps_step_latches_and_stops_the_timer() {
    let r = rig(vec![ProbedTimeSource::Unsynced]);
    r.trust.start();
    settle().await;
    advance(7_000).await;
    r.trust.note_step(SteppedClockSource::Gps);
    assert!(r.trust.trusted());
    assert!(!r.trust.timer_armed());
    assert_eq!(r.trust.status().source.map(|s| s.as_str()), Some("gps"));
    r.trust.stop();
    r.trust.start(); // stopped + latched: no-op
    assert!(!r.trust.timer_armed());
}

impl Rig {
    fn trusted_now(&self) -> bool {
        self.trust.trusted()
    }
}

/// The publish gate as the tick calls it: false + `noteHeld()` while held;
/// `true` once trusted; always `true` without a gate (sim / Phase 2–4 hooks).
#[tokio::test(start_paused = true)]
async fn publish_gate_counts_held_ticks() {
    use xorgate_agent::ops::hooks::OpsHooks;
    use xorgate_agent::recording::RecordingHooks;
    let clock = Arc::new(AgentClock::following_tokio(1_800_000_000_000));
    let rec = Arc::new(RecordingHooks::new(None, None, Arc::new(|| None), clock));
    let ops = OpsHooks::new(rec);
    assert!(ops.publish_clock_gate(), "no gate bound → publish");
    assert_eq!(ops.report_hints().clock_trust, None);
    let r = rig(vec![ProbedTimeSource::Unsynced]);
    ops.bind_clock_trust(r.trust.clone());
    assert!(!ops.publish_clock_gate());
    assert!(!ops.publish_clock_gate());
    assert_eq!(r.trust.status().held, 2);
    assert_eq!(ops.report_hints().clock_trust, Some(ClockTrustState::Held));
    r.trust.note_step(SteppedClockSource::Nitz);
    assert!(ops.publish_clock_gate());
    assert_eq!(r.trust.status().held, 2);
    assert_eq!(
        ops.report_hints().clock_trust,
        Some(ClockTrustState::Trusted)
    );
    assert_eq!(
        serde_json::to_value(ops.report_hints()).unwrap()["clockTrust"],
        "trusted"
    );
}
