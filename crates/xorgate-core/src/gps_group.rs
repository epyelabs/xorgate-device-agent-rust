//! The pure half of the gps collector — `src/agent/collectors/gps.ts`
//! (343 lines) minus the tty stream and the file write, which are the agent's
//! (`xorgate-agent::collectors::gps`) and the HAL's (`xorgate-hal::nmea_tty`).
//!
//! Here: the timer constants, the TTFF clock (`noteFixQuality`, `:139-151`),
//! the freshness / silent-fd watchdog / sky-file cadence predicates
//! (`:212-224`, `:232-237`, `:302-305`), `mapLatest` (`:261-300`), `hasFix`
//! (`:330-336`), the `clock()` reading (`:319-325`) and `describeError`
//! (`hardware/gps.ts:36-40`).
//!
//! TIME (O2, accepted divergence): the TS measures the reopen watchdog, NMEA
//! freshness, TTFF and the 15 s `gnss-sky.json` cadence on `Date.now()`; here
//! every one of them takes an injected MONOTONIC `now` (invariant 2). The
//! values are identical unless the wall clock steps.

use crate::clock::MonoMs;
use crate::nmea::{has_position, GpsFix};
use crate::round::round;
use crate::sky_state::{sky_state_code, SkySnapshot, SKY_PUBLISH_MS};
use crate::telemetry::{GpsGroup, MetricLeaf};
use crate::types::{GpsClockReading, SkyState};

/// `NMEA_STALE_MS` (`gps.ts:15`): no parsed sentence within this window →
/// GNSS is treated as not streaming and the group disappears.
pub const NMEA_STALE_MS: u64 = 10_000;

/// `REOPEN_SILENT_MS` (`gps.ts:24`): an open stream silent this long is
/// assumed dead (modem USB re-enumeration) and reopened.
pub const REOPEN_SILENT_MS: u64 = 30_000;

/// `CLOCK_FRESH_MS` (`gps.ts:72`): how old an RMC time reading may be before
/// `clock()` reports none.
pub const CLOCK_FRESH_MS: u64 = 10_000;

/// The silent-fd watchdog warning (`gps.ts:219`), verbatim.
pub const NMEA_SILENT_REOPEN: &str = "[gps] NMEA silent too long — reopening the port";

/// The TTFF clock (`acquiringSince` / `ttffS`, `gps.ts:136-151`).
///
/// Arms when quality-0 sentences appear (acquisition observed), stops at the
/// fix-quality 0→nonzero transition, and is re-armed (never billed across a
/// silent gap) by [`TtffClock::stream_opened`] on every (re)open.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TtffClock {
    /// Mono ms when the current acquiring (quality 0) streak started.
    acquiring_since: Option<MonoMs>,
    /// Measured seconds for the current lock (`null` until measured).
    ttff_s: Option<f64>,
}

impl TtffClock {
    /// `noteFixQuality(fix, now)` (`gps.ts:139-151`).
    pub fn note_fix_quality(&mut self, fix: &GpsFix, now: MonoMs) {
        // `typeof q !== "number"`: no GGA yet — no quality knowledge.
        let Some(q) = fix.fix_quality else { return };
        if q == 0.0 {
            if self.acquiring_since.is_none() {
                self.acquiring_since = Some(now);
                // a new acquisition cycle invalidates the old measurement
                self.ttff_s = None;
            }
        } else if let Some(since) = self.acquiring_since {
            self.ttff_s = Some(now.saturating_sub(since) as f64 / 1000.0);
            self.acquiring_since = None;
        }
    }

    /// `start()`'s `acquiringSince = null` (`gps.ts:157`): a fresh stream is a
    /// fresh acquisition observation. `ttffS` is deliberately kept.
    pub fn stream_opened(&mut self) {
        self.acquiring_since = None;
    }

    /// The measured TTFF of the current lock, seconds.
    pub fn ttff_s(&self) -> Option<f64> {
        self.ttff_s
    }
}

/// `freshFix()`'s staleness test (`gps.ts:302-305`): `Date.now() - lastFixAt >
/// NMEA_STALE_MS` → stale. `last_fix_at` is `None` before the first fix.
pub fn is_fresh(last_fix_at: Option<MonoMs>, now: MonoMs) -> bool {
    match last_fix_at {
        Some(at) => now.saturating_sub(at) <= NMEA_STALE_MS,
        None => false,
    }
}

/// The silent-fd watchdog (`gps.ts:215`): `Date.now() - Math.max(lastFixAt,
/// openedAt) > reopenSilentMs` (`lastFixAt` starts at 0 in the TS).
pub fn reopen_due(
    last_fix_at: Option<MonoMs>,
    opened_at: MonoMs,
    now: MonoMs,
    reopen_silent_ms: u64,
) -> bool {
    let since = last_fix_at.unwrap_or(0).max(opened_at);
    now.saturating_sub(since) > reopen_silent_ms
}

/// `skyNow()`'s write gate (`gps.ts:234-236`): write when the state changed
/// (the TS starts from `skyPublishedState = ""`, so the first call always
/// writes) or `SKY_PUBLISH_MS` elapsed since the last write.
pub fn sky_publish_due(
    published: Option<(SkyState, MonoMs)>,
    state: SkyState,
    now: MonoMs,
) -> bool {
    match published {
        None => true,
        Some((prev, at)) => prev != state || now.saturating_sub(at) >= SKY_PUBLISH_MS,
    }
}

/// `mapLatest(fix)` (`gps.ts:261-300`) with the sky snapshot `skyNow()`
/// returned for this call and the TTFF of the current lock. Key order = the
/// TS insertion order (= [`GpsGroup`]'s field order).
pub fn map_latest(fix: &GpsFix, sky: &SkySnapshot, ttff_s: Option<f64>) -> GpsGroup {
    let positioned = has_position(fix);
    let opt = |b: bool, leaf: MetricLeaf| b.then_some(leaf);
    GpsGroup {
        fix_quality: MetricLeaf::new(fix.fix_quality.unwrap_or(0.0)),
        satellites: fix.satellites.map(MetricLeaf::new),
        satellites_in_view: fix.satellites_in_view.map(MetricLeaf::new),
        sats_heard: Some(MetricLeaf::new(f64::from(sky.heard))),
        snr_max: sky.max_snr.map(|v| MetricLeaf::with_unit(v, "dBHz")),
        sky_state: sky_state_code(sky.state).map(|c| MetricLeaf::new(f64::from(c))),
        hdop: fix.hdop.map(|v| MetricLeaf::new(round(v, 1))),
        lat: fix
            .latitude
            .filter(|_| positioned)
            .map(|v| MetricLeaf::new(round(v, 6))),
        lon: fix
            .longitude
            .filter(|_| positioned)
            .map(|v| MetricLeaf::new(round(v, 6))),
        alt: fix
            .altitude
            .filter(|_| positioned)
            .map(|v| MetricLeaf::with_unit(round(v, 1), "m")),
        speed: fix
            .speed_kmh
            .filter(|_| positioned)
            .map(|v| MetricLeaf::with_unit(round(v, 1), "km/h")),
        course: fix
            .course
            .filter(|_| positioned)
            .map(|v| MetricLeaf::with_unit(round(v, 1), "°")),
        ttff_s: ttff_s.and_then(|t| opt(positioned, MetricLeaf::with_unit(round(t, 1), "s"))),
    }
}

/// `hasFix()` (`gps.ts:330-336`): a FRESH fix with GGA quality > 0 AND a
/// position (the aggregator clears position on lock loss).
pub fn has_fix(fresh: Option<&GpsFix>) -> bool {
    fresh.is_some_and(|f| f.fix_quality.unwrap_or(0.0) > 0.0 && has_position(f))
}

/// `clock()`'s reading (`gps.ts:319-325`): the last distinct RMC `utcMs`, its
/// mono arrival stamp and validity → `None` when absent or older than
/// [`CLOCK_FRESH_MS`].
///
/// Accepted ≤1 ms skew (WP2 review NIT 8): the TS floors the ns difference,
/// this subtracts two ms-truncated stamps, so `age_ms` can read 1 ms high or
/// low and flip freshness exactly at the 10 s edge. Consumer is Phase 4.
pub fn clock_reading(
    utc_ms: Option<f64>,
    arrived_mono: MonoMs,
    valid: bool,
    now: MonoMs,
) -> Option<GpsClockReading> {
    let utc_ms = utc_ms?;
    let age_ms = now.saturating_sub(arrived_mono);
    if age_ms > CLOCK_FRESH_MS {
        return None;
    }
    Some(GpsClockReading {
        utc_ms: utc_ms as i64,
        age_ms,
        valid,
    })
}

/// `describeError(err, port)` (`hardware/gps.ts:36-40`): ENOENT and EACCES
/// get operator wording; anything else is Node's `err.message`.
pub fn describe_error(code: Option<&str>, node_message: &str, port: &str) -> String {
    match code {
        Some("ENOENT") => {
            format!("GPS port {port} not present (is the modem enumerated and GNSS enabled?)")
        }
        Some("EACCES") => {
            format!("permission denied on {port} (add the agent user to the 'dialout' group)")
        }
        _ => node_message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(v: f64) -> GpsFix {
        GpsFix {
            fix_quality: Some(v),
            ..Default::default()
        }
    }

    #[test]
    fn ttff_arms_on_quality_zero_and_stops_on_lock() {
        let mut t = TtffClock::default();
        t.note_fix_quality(&GpsFix::default(), 100); // no GGA yet
        assert_eq!(t, TtffClock::default());
        t.note_fix_quality(&q(0.0), 1_000);
        t.note_fix_quality(&q(0.0), 5_000); // streak keeps its start
        t.note_fix_quality(&q(1.0), 13_450);
        assert_eq!(t.ttff_s(), Some(12.45));
        t.note_fix_quality(&q(1.0), 20_000); // no re-measure under lock
        assert_eq!(t.ttff_s(), Some(12.45));
        t.stream_opened(); // keeps the measurement
        assert_eq!(t.ttff_s(), Some(12.45));
        t.note_fix_quality(&q(0.0), 30_000); // new acquisition invalidates it
        assert_eq!(t.ttff_s(), None);
    }

    #[test]
    fn freshness_and_watchdog_boundaries() {
        assert!(!is_fresh(None, 0));
        assert!(is_fresh(Some(0), NMEA_STALE_MS));
        assert!(!is_fresh(Some(0), NMEA_STALE_MS + 1));
        assert!(!reopen_due(None, 500, 30_500, REOPEN_SILENT_MS));
        assert!(reopen_due(None, 500, 30_501, REOPEN_SILENT_MS));
        assert!(!reopen_due(Some(10_000), 500, 40_000, REOPEN_SILENT_MS));
        assert!(reopen_due(Some(10_000), 500, 40_001, REOPEN_SILENT_MS));
    }

    #[test]
    fn sky_cadence() {
        assert!(sky_publish_due(None, SkyState::Unknown, 0));
        assert!(!sky_publish_due(
            Some((SkyState::Unknown, 0)),
            SkyState::Unknown,
            14_999
        ));
        assert!(sky_publish_due(
            Some((SkyState::Unknown, 0)),
            SkyState::Unknown,
            15_000
        ));
        assert!(sky_publish_due(
            Some((SkyState::Unknown, 0)),
            SkyState::Clear,
            1
        ));
    }

    #[test]
    fn clock_freshness() {
        assert_eq!(clock_reading(None, 0, true, 0), None);
        let r = clock_reading(Some(1.5e12), 1_000, true, 11_000).unwrap();
        assert_eq!(
            (r.utc_ms, r.age_ms, r.valid),
            (1_500_000_000_000, 10_000, true)
        );
        assert_eq!(clock_reading(Some(1.5e12), 1_000, true, 11_001), None);
    }

    #[test]
    fn describe_error_wording() {
        assert_eq!(
            describe_error(Some("ENOENT"), "x", "/dev/ttyUSB1"),
            "GPS port /dev/ttyUSB1 not present (is the modem enumerated and GNSS enabled?)"
        );
        assert_eq!(
            describe_error(Some("EACCES"), "x", "/dev/ttyUSB1"),
            "permission denied on /dev/ttyUSB1 (add the agent user to the 'dialout' group)"
        );
        assert_eq!(
            describe_error(Some("EIO"), "EIO: i/o error, read", "/p"),
            "EIO: i/o error, read"
        );
    }
}
