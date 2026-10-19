//! The pacing between MQTT connect attempts — ours, because rumqttc has none
//! (issues #703/#918): the event loop simply errors and reconnects on the
//! next `poll()`, so the pause between attempts is this module's job.
//!
//! DEFAULT = mqtt.js parity (orchestrator decision 2026-10-19, superseding
//! Phase 0 A6's 1 s → 60 s jittered backoff): the TS client sets
//! `reconnectPeriod: 2000` (`src/agent/mqtt.ts:63`), and mqtt.js 5.15.1 arms
//! `setInterval(_reconnect, reconnectPeriod)` in `_setupReconnect()` from the
//! client's `close` handler (or from `_cleanUp` when it kills a live link),
//! clearing it in the `connect()` it triggers (`build/lib/client.js:214-224,
//! 249,849-889`). So every close is followed by the next attempt exactly
//! 2000 ms later — no growth, no jitter, no cap — and a hung attempt is
//! bounded by `connectTimeout: 30_000` (the connack timer) before ITS close
//! starts the next 2 s. The mechanism stays configurable (README §4) — an
//! exponential/jittered ladder is one `BackoffConfig` away — but the
//! default is the fixed [`MQTT_JS_RECONNECT_PERIOD`].

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BackoffConfig {
    pub initial: Duration,
    pub max: Duration,
    pub factor: f64,
    pub jitter: bool,
}

/// mqtt.js `reconnectPeriod` as the TS sets it (`mqtt.ts:63`).
pub const MQTT_JS_RECONNECT_PERIOD: Duration = Duration::from_millis(2000);

impl Default for BackoffConfig {
    /// mqtt.js parity: a fixed 2000 ms after every close, no jitter.
    fn default() -> Self {
        Self::fixed(MQTT_JS_RECONNECT_PERIOD)
    }
}

impl BackoffConfig {
    /// A constant pause, no growth, no jitter.
    pub fn fixed(period: Duration) -> Self {
        Self {
            initial: period,
            max: period,
            factor: 1.0,
            jitter: false,
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

    /// The Phase 0 ladder, still expressible (README §4).
    fn ladder(jitter: bool) -> BackoffConfig {
        BackoffConfig {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            factor: 2.0,
            jitter,
        }
    }

    #[test]
    fn default_is_mqtt_js_fixed_2000_ms_without_jitter() {
        let mut b = Backoff::new(BackoffConfig::default());
        for n in 1..=50u32 {
            assert_eq!(b.next_delay(), Duration::from_millis(2000), "attempt {n}");
            assert_eq!(b.attempt(), n);
        }
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(), Duration::from_millis(2000));
    }

    #[test]
    fn grows_exponentially_and_caps_without_jitter() {
        let mut b = Backoff::new(ladder(false));
        let d: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(d, [1, 2, 4, 8, 16, 32, 60, 60]);
        b.reset();
        assert_eq!(b.next_delay().as_secs(), 1);
    }

    #[test]
    fn jitter_stays_within_half_to_full() {
        let mut b = Backoff::new(ladder(true));
        for expected in [1.0, 2.0, 4.0, 8.0] {
            let d = b.next_delay().as_secs_f64();
            assert!(
                d >= expected * 0.5 - 1e-9 && d <= expected + 1e-9,
                "{d} vs {expected}"
            );
        }
    }
}
