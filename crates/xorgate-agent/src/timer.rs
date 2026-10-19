//! Node `setInterval` semantics on tokio (P-WP6-1).
//!
//! Every periodic timer of the TS agent @89b5c64 is a `setInterval` (none is
//! a chained `setTimeout`). Node re-arms a repeating timer from the instant it
//! FIRES (`lib/internal/timers.js` `listOnTimeout`: `start = getLibuvNow()`
//! before the callback, then `insert(timer, timer._repeat, start)`), so the
//! gap between two fires is always ≥ the period and a late fire shifts every
//! later one (the known few-ms-per-tick drift).
//!
//! `tokio::time::Interval` is FIXED-RATE instead: `MissedTickBehavior::Delay`
//! only re-bases on `now` when a tick is more than 5 ms late, so a tick that
//! fired 1–5 ms late is followed by one 995–999 ms later. Against a shared
//! "recompute when ≥ 1000 ms since the last recompute" gate (`system.ts:104-113`
//! ↔ `collectors/system.rs` + `xorgate_core::collector_cache::poll_due`) that
//! short gap flips which consumer owns the recompute — P-WP6-1 (stale power
//! data in the shed decision, recorder samples missing `system`).
//!
//! [`SetInterval`] is the port: first fire one period after arming, every next
//! deadline = the previous fire instant + period. The callback's duration
//! never shifts the deadline (Node captures `start` before running it).
//! `tick()` is cancel-safe (state changes only inside the poll that returns
//! `Ready`), so it can sit in a `select!` arm like `Interval::tick`.

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::time::{sleep_until, Instant, Sleep};

/// A `setInterval(fn, period)` timer: see the module docs.
#[derive(Debug)]
pub struct SetInterval {
    period: Duration,
    sleep: Pin<Box<Sleep>>,
}

impl SetInterval {
    /// Armed now: the first fire is one period from now. A zero period is
    /// clamped to 1 ms (Node clamps a `setInterval` delay `< 1` to 1).
    pub fn new(period: Duration) -> Self {
        let period = period.max(Duration::from_millis(1));
        Self::starting_at(Instant::now() + period, period)
    }

    /// First fire at `first`, then every `period` after each fire.
    pub fn starting_at(first: Instant, period: Duration) -> Self {
        Self {
            period: period.max(Duration::from_millis(1)),
            sleep: Box::pin(sleep_until(first)),
        }
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    /// When the next fire is due.
    pub fn deadline(&self) -> Instant {
        self.sleep.deadline()
    }

    /// Ready (with the fire instant) once the deadline has passed; re-arms
    /// at that instant + period.
    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        match self.sleep.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                let fired = Instant::now();
                self.sleep.as_mut().reset(fired + self.period);
                Poll::Ready(fired)
            }
        }
    }

    /// The next fire; returns its instant. Cancel-safe.
    pub async fn tick(&mut self) -> Instant {
        poll_fn(|cx| self.poll_tick(cx)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_core::collector_cache::poll_due;

    const SEC: Duration = Duration::from_millis(1000);

    /// Move the paused clock to `deadline + late`, then take the tick there —
    /// a fire that the runtime delivers `late` after its deadline.
    async fn fire_late(iv: &mut SetInterval, late: Duration) -> Instant {
        let at = iv.deadline() + late;
        let now = Instant::now();
        if at > now {
            tokio::time::advance(at - now).await;
        }
        iv.tick().await
    }

    #[tokio::test(start_paused = true)]
    async fn first_fire_one_period_after_arming() {
        let t0 = Instant::now();
        let mut iv = SetInterval::new(SEC);
        assert_eq!(iv.tick().await - t0, SEC);
        assert_eq!(iv.tick().await - t0, 2 * SEC);
    }

    #[tokio::test(start_paused = true)]
    async fn re_arms_from_the_fire_instant_so_a_late_fire_never_shortens_the_next_gap() {
        let mut iv = SetInterval::new(SEC);
        let lateness_ms = [0u64, 3, 0, 1, 5, 2, 0, 4];
        let mut fires = Vec::new();
        for l in lateness_ms {
            fires.push(fire_late(&mut iv, Duration::from_millis(l)).await);
        }
        let gaps: Vec<u64> = fires
            .windows(2)
            .map(|w| (w[1] - w[0]).as_millis() as u64)
            .collect();
        // gap = period + THIS fire's lateness (Node), never period − the
        // previous one's (tokio fixed-rate would give 997, 999, … here).
        assert_eq!(gaps, [1003, 1000, 1001, 1005, 1002, 1000, 1004]);
        assert!(gaps.iter().all(|g| *g >= 1000));
    }

    #[tokio::test(start_paused = true)]
    async fn the_callback_duration_does_not_move_the_deadline() {
        let t0 = Instant::now();
        let mut iv = SetInterval::new(SEC);
        let fired = iv.tick().await;
        tokio::time::sleep(Duration::from_millis(300)).await; // the work
        assert_eq!(iv.deadline(), fired + SEC);
        assert_eq!(iv.tick().await - t0, 2 * SEC);
    }

    #[tokio::test(start_paused = true)]
    async fn tick_is_cancel_safe() {
        let t0 = Instant::now();
        let mut iv = SetInterval::new(SEC);
        // A select! arm that loses: the dropped tick future must not re-arm.
        tokio::select! {
            _ = iv.tick() => panic!("not due yet"),
            _ = tokio::time::sleep(Duration::from_millis(400)) => {}
        }
        assert_eq!(iv.deadline(), t0 + SEC);
        assert_eq!(iv.tick().await - t0, SEC);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_period_is_clamped_to_one_ms() {
        let t0 = Instant::now();
        let mut iv = SetInterval::new(Duration::ZERO);
        assert_eq!(iv.tick().await - t0, Duration::from_millis(1));
    }

    /// P-WP6-1 pin: the shed timer (armed first) and the telemetry recorder
    /// (armed ~200 ms later), both 1 Hz, share the `system.sample()` cache
    /// gated on `now - lastAt >= 1000` ms. With `setInterval` semantics the
    /// shed tick's gap is always ≥ 1000 ms, so once it recomputes it keeps
    /// recomputing — every shed decision reads fresh power data and the
    /// recorder never steals the cache — whatever the per-fire lateness.
    #[tokio::test(start_paused = true)]
    async fn shed_tick_keeps_the_shared_one_second_gate_under_jitter() {
        let origin = Instant::now();
        let ms = |t: Instant| (t - origin).as_millis() as u64; // floor, like mono_ms
        let mut shed = SetInterval::new(SEC);
        tokio::time::advance(Duration::from_millis(200)).await;
        let mut rec = SetInterval::new(SEC);

        // Lateness per fire (ms): the shed timer late by up to 5 ms (inside
        // tokio's Delay grace, the case that broke), the recorder on time.
        let shed_late = [0u64, 1, 0, 5, 2, 0, 1, 3, 0, 4, 1, 0, 2, 5, 0, 1];
        let mut last_at: Option<u64> = None;
        let (mut shed_recomputes, mut rec_recomputes) = (0, 0);
        let (mut si, mut ri) = (0usize, 0usize);
        while si < shed_late.len() {
            let shed_at = shed.deadline() + Duration::from_millis(shed_late[si]);
            let rec_at = rec.deadline();
            let (now, is_shed) = if shed_at <= rec_at {
                si += 1;
                (
                    fire_late(&mut shed, Duration::from_millis(shed_late[si - 1])).await,
                    true,
                )
            } else {
                ri += 1;
                (fire_late(&mut rec, Duration::ZERO).await, false)
            };
            if poll_due(last_at, ms(now)) {
                last_at = Some(ms(now));
                if is_shed {
                    shed_recomputes += 1;
                } else {
                    rec_recomputes += 1;
                }
            }
        }
        assert_eq!(
            shed_recomputes,
            shed_late.len(),
            "every shed tick recomputes"
        );
        assert_eq!(
            rec_recomputes, 0,
            "the recorder never takes the cache ({ri} ticks)"
        );
    }

    /// Why the port exists: tokio's fixed-rate interval (Delay inside its
    /// 5 ms grace) yields a 997 ms gap after a fire 3 ms late.
    #[tokio::test(start_paused = true)]
    async fn tokio_interval_is_fixed_rate_inside_its_grace() {
        let t0 = Instant::now();
        let mut iv = tokio::time::interval_at(t0 + SEC, SEC);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::time::advance(Duration::from_millis(1003)).await;
        iv.tick().await; // returns the SCHEDULED instant; measure `now`
        let a = Instant::now();
        iv.tick().await;
        let b = Instant::now();
        assert_eq!(a - t0, Duration::from_millis(1003));
        assert_eq!(b - a, Duration::from_millis(997));
    }
}
