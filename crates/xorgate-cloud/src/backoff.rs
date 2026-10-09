//! Exponential backoff with jitter — ours, because rumqttc has none
//! (issues #703/#918): the event loop simply errors and reconnects on the
//! next `poll()`, so the pacing between attempts is this module's job.
//!
//! The JS agent used mqtt.js's fixed `reconnectPeriod: 2000`. Phase 0 (A6)
//! deliberately upgrades that to 1 s → 2 → 4 → … → 60 s with full-range
//! jitter in `[delay/2, delay]`, reset on every successful CONNACK.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BackoffConfig {
    pub initial: Duration,
    pub max: Duration,
    pub factor: f64,
    pub jitter: bool,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            factor: 2.0,
            jitter: true,
        }
    }
}

#[derive(Debug)]
pub struct Backoff {
    cfg: BackoffConfig,
    attempt: u32,
    rng: u64,
}

impl Backoff {
    pub fn new(cfg: BackoffConfig) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            ^ ((std::process::id() as u64) << 32)
            | 1;
        Self {
            cfg,
            attempt: 0,
            rng: seed,
        }
    }

    /// Number of consecutive failures so far (0 right after a reset).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The delay to sleep before the NEXT attempt; advances the counter.
    pub fn next_delay(&mut self) -> Duration {
        let exp =
            self.cfg.initial.as_secs_f64() * self.cfg.factor.powi(self.attempt.min(30) as i32);
        let capped = exp.min(self.cfg.max.as_secs_f64());
        self.attempt = self.attempt.saturating_add(1);
        let secs = if self.cfg.jitter {
            // xorshift64* — good enough to spread a fleet's reconnects.
            self.rng ^= self.rng >> 12;
            self.rng ^= self.rng << 25;
            self.rng ^= self.rng >> 27;
            let unit =
                (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
            capped * (0.5 + 0.5 * unit)
        } else {
            capped
        };
        Duration::from_secs_f64(secs)
    }

    /// Call on a successful connection: the next failure starts from `initial` again.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_exponentially_and_caps_without_jitter() {
        let mut b = Backoff::new(BackoffConfig {
            jitter: false,
            ..Default::default()
        });
        let d: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(d, [1, 2, 4, 8, 16, 32, 60, 60]);
        b.reset();
        assert_eq!(b.next_delay().as_secs(), 1);
    }

    #[test]
    fn jitter_stays_within_half_to_full() {
        let mut b = Backoff::new(BackoffConfig::default());
        for expected in [1.0, 2.0, 4.0, 8.0] {
            let d = b.next_delay().as_secs_f64();
            assert!(
                d >= expected * 0.5 - 1e-9 && d <= expected + 1e-9,
                "{d} vs {expected}"
            );
        }
    }
}
