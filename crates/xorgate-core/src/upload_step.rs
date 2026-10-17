//! The uploader's pure decisions and every event string it emits
//! (`src/recording/uploader.ts` @ `89b5c64`); the single-flight runtime that
//! applies them is `xorgate-agent::recording::uploader::Uploader`.
//!
//! All intervals here are plain milliseconds; the runtime measures them on
//! the monotonic tokio clock (P8).

use crate::js::number_to_string;
use crate::round::js_math_round;
use crate::telemetry::ActivityLevel;

/// `BACKOFF_MIN_MS` (`uploader.ts:73`).
pub const BACKOFF_MIN_MS: u64 = 2_000;
/// `BACKOFF_MAX_MS` (`uploader.ts:74`): 5 min.
pub const BACKOFF_MAX_MS: u64 = 5 * 60_000;

/// `onEvent("info", "upload paused (offline)")` (`uploader.ts:202`).
pub const UPLOAD_PAUSED_EVENT: &str = "upload paused (offline)";

/// `upload: skipping unrecognized segment ${seg.name}` (`uploader.ts:148`, warn).
pub fn skip_unrecognized_event(seg_name: &str) -> String {
    format!("upload: skipping unrecognized segment {seg_name}")
}

/// `upload failed, retrying in ${Math.round(this.backoffMs / 1000)}s: ${msg}`
/// (`uploader.ts:204`, warn) — the delay printed is the CURRENT backoff (the
/// one `scheduleRetry` is about to arm), rounded half-up like `Math.round`.
pub fn retry_event(backoff_ms: u64, message: &str) -> String {
    let secs = js_math_round(backoff_ms as f64 / 1000.0);
    format!(
        "upload failed, retrying in {}s: {message}",
        number_to_string(secs)
    )
}

/// `kick()`'s guard (`uploader.ts:104-105`): start a drain only when not
/// stopped, not already running, online and `mode() === "auto"`.
pub fn may_kick(stopped: bool, running: bool, online: bool, mode_auto: bool) -> bool {
    !stopped && !running && online && mode_auto
}

/// The drain loop condition (`uploader.ts:140`), re-checked before every unit.
pub fn may_continue(stopped: bool, online: bool, mode_auto: bool) -> bool {
    !stopped && online && mode_auto
}

/// What a rejected PUT unit turns into (`uploader.ts:198-206`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureStep {
    /// `pause()`/`stop()` aborted it (or the link dropped): one info event,
    /// no retry timer — the reconnect kick resumes.
    Paused,
    /// A real failure while online: one warn event, then `scheduleRetry()`.
    Retry,
}

/// `this.stopped || !this.opts.isOnline()` (`uploader.ts:200`). Upload
/// `mode` is deliberately NOT consulted: a PUT failing right after the mode
/// flipped to off still logs a retry (whose timer's kick then no-ops).
pub fn on_failure(stopped: bool, online: bool) -> FailureStep {
    if stopped || !online {
        FailureStep::Paused
    } else {
        FailureStep::Retry
    }
}

/// The event `on_failure` emits: `(level, msg)`.
pub fn failure_event(
    step: &FailureStep,
    backoff_ms: u64,
    message: &str,
) -> (ActivityLevel, String) {
    match step {
        FailureStep::Paused => (ActivityLevel::Info, UPLOAD_PAUSED_EVENT.to_string()),
        FailureStep::Retry => (ActivityLevel::Warn, retry_event(backoff_ms, message)),
    }
}

/// One armed retry (`scheduleRetry`, `uploader.ts:215-224`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPlan {
    /// The timer delay = the backoff BEFORE doubling.
    pub delay_ms: u64,
    /// `Math.min(this.backoffMs * 2, backoffMaxMs)`.
    pub next_backoff_ms: u64,
}

/// `scheduleRetry()`: `None` when a timer is already armed or the uploader
/// is stopped; otherwise the delay and the doubled (capped) backoff.
pub fn plan_retry(
    timer_armed: bool,
    stopped: bool,
    backoff_ms: u64,
    max_ms: u64,
) -> Option<RetryPlan> {
    if timer_armed || stopped {
        return None;
    }
    Some(RetryPlan {
        delay_ms: backoff_ms,
        next_backoff_ms: backoff_ms.saturating_mul(2).min(max_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_seconds_round_half_up() {
        assert_eq!(retry_event(500, "x"), "upload failed, retrying in 1s: x");
        assert_eq!(retry_event(1_499, "x"), "upload failed, retrying in 1s: x");
        assert_eq!(retry_event(20, ""), "upload failed, retrying in 0s: ");
        assert_eq!(
            retry_event(300_000, "boom"),
            "upload failed, retrying in 300s: boom"
        );
    }

    #[test]
    fn retry_plan_doubles_to_the_cap() {
        let mut b = BACKOFF_MIN_MS;
        let mut delays = vec![];
        for _ in 0..10 {
            let p = plan_retry(false, false, b, BACKOFF_MAX_MS).unwrap();
            delays.push(p.delay_ms / 1000);
            b = p.next_backoff_ms;
        }
        assert_eq!(delays, [2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(plan_retry(true, false, b, BACKOFF_MAX_MS), None);
        assert_eq!(plan_retry(false, true, b, BACKOFF_MAX_MS), None);
    }

    #[test]
    fn mode_is_not_part_of_the_failure_verdict() {
        assert_eq!(on_failure(false, true), FailureStep::Retry);
        assert_eq!(on_failure(true, true), FailureStep::Paused);
        assert_eq!(on_failure(false, false), FailureStep::Paused);
        assert!(!may_kick(false, false, true, false));
        assert!(may_kick(false, false, true, true));
        assert!(!may_kick(false, true, true, true));
    }
}
