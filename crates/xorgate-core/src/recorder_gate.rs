//! The freshness + cadence gate of `src/recording/telemetry-recorder.ts`
//! (126 lines): which metric groups one recorder tick writes.
//!
//! Freshness gating (plan): a group is written only when its cache advanced
//! (`updatedMono`) since the last written sample AND its native cadence
//! allows another write — so at `rateHz = 10`, IMU (20 Hz native) lands
//! ~10×/s while GPS/LTE/system (1 Hz native) land ~1×/s instead of ten
//! duplicated rows.
//!
//! The cadence gate is measured on the MONOTONIC clock (optimize-system-
//! process-cycle Phase 8, `telemetry-recorder.ts:13-17`): it used to compare
//! wall-clock stamps, so a backward clock correction made every gap
//! negative and starved the recorder for the rest of the boot
//! (field-observed 2026-07-29 after a 2080→2026 NTP rewind).
//!
//! Out of core: the interval, the `sampleGroups()` await, the segment
//! writer I/O and the `[recording] tick failed` catch (the agent's
//! `recording::telemetry`).

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::clock::MonoMs;

/// `CADENCE_SLACK` (`telemetry-recorder.ts:33`): a tick landing a few ms
/// early still counts as the next period.
pub const CADENCE_SLACK: f64 = 0.9;

/// `NATIVE_CADENCE_HZ` (`telemetry-recorder.ts:25-30`): how fast each
/// group's underlying data actually refreshes (Hz). Unknown groups fall back
/// to being capped at `rateHz`.
pub fn native_cadence_hz() -> IndexMap<String, f64> {
    [("imu", 20.0), ("gps", 1.0), ("lte", 1.0), ("system", 1.0)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// `Math.round(1000 / this.opts.rateHz)` (`telemetry-recorder.ts:71`): the
/// recorder's own `setInterval` period in ms. `rateHz` is the resolved
/// `recording.telemetry.rateHz` (an integer ≥ 1); JS `Math.round` rounds
/// half up (`floor(x + 0.5)`), mirrored here.
pub fn tick_interval_ms(rate_hz: f64) -> u64 {
    (1000.0 / rate_hz + 0.5).floor() as u64
}

/// `(1000 / Math.min(nativeHz, rateHz)) * CADENCE_SLACK`
/// (`telemetry-recorder.ts:103-104`): the minimum monotonic gap between two
/// written samples of a group. `native_hz` is `this.cadence[group]`; `None`
/// (an unknown group) is `?? this.opts.rateHz`.
pub fn min_gap_ms(native_hz: Option<f64>, rate_hz: f64) -> f64 {
    let native = native_hz.unwrap_or(rate_hz);
    (1000.0 / native.min(rate_hz)) * CADENCE_SLACK
}

/// The two per-group maps of `class TelemetryRecorder`
/// (`telemetry-recorder.ts:55-57`): `lastWrittenMono` (the cache stamp of the
/// last written sample) and `lastWriteAtMono` (monotonic ms of that write —
/// never wall clock). `K` is the freshness key (`updatedMono`, a bigint ns in
/// the TS; the agent uses `tokio::time::Instant`) — only its order matters.
#[derive(Debug, Clone)]
pub struct RecorderGate<K> {
    cadence: IndexMap<String, f64>,
    last_written: HashMap<String, K>,
    last_write_at_mono: HashMap<String, MonoMs>,
}

impl<K: PartialOrd + Copy> RecorderGate<K> {
    /// `this.cadence = opts.nativeCadenceHz ?? NATIVE_CADENCE_HZ`
    /// (`telemetry-recorder.ts:64`): an override REPLACES the table.
    pub fn new(cadence_override: Option<IndexMap<String, f64>>) -> Self {
        Self {
            cadence: cadence_override.unwrap_or_else(native_cadence_hz),
            last_written: HashMap::new(),
            last_write_at_mono: HashMap::new(),
        }
    }

    /// One iteration of the `for (const [group, s] of Object.entries(groups))`
    /// loop (`telemetry-recorder.ts:100-110`): `true` = write `group` in this
    /// tick's sample (and the gate has recorded the write); `false` =
    /// `continue` (cache not advanced, or the cadence floor not yet cleared).
    pub fn admit(&mut self, group: &str, updated: K, now_mono: MonoMs, rate_hz: f64) -> bool {
        if let Some(last) = self.last_written.get(group) {
            // `s.updatedMono <= lastMono` → continue.
            if updated.partial_cmp(last) != Some(std::cmp::Ordering::Greater) {
                return false;
            }
        }
        let gap = min_gap_ms(self.cadence.get(group).copied(), rate_hz);
        if let Some(&last_at) = self.last_write_at_mono.get(group) {
            // `nowMono - lastAt < minGapMs` → continue. Monotonic: never
            // negative; computed in f64 like the JS number subtraction.
            if (now_mono as f64) - (last_at as f64) < gap {
                return false;
            }
        }
        self.last_written.insert(group.to_string(), updated);
        self.last_write_at_mono.insert(group.to_string(), now_mono);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_and_gaps() {
        assert_eq!(tick_interval_ms(1.0), 1000);
        assert_eq!(tick_interval_ms(3.0), 333);
        assert_eq!(tick_interval_ms(10.0), 100);
        assert_eq!(tick_interval_ms(20.0), 50);
        assert_eq!(tick_interval_ms(7.0), 143);
        assert!((min_gap_ms(Some(20.0), 10.0) - 90.0).abs() < 1e-9);
        assert!((min_gap_ms(Some(1.0), 10.0) - 900.0).abs() < 1e-9);
        assert!((min_gap_ms(None, 5.0) - 180.0).abs() < 1e-9);
        assert_eq!(native_cadence_hz().get("imu"), Some(&20.0));
    }

    #[test]
    fn freshness_then_cadence() {
        let mut g = RecorderGate::<u64>::new(None);
        assert!(g.admit("gps", 1, 10_000, 10.0));
        assert!(!g.admit("gps", 1, 11_000, 10.0)); // not advanced
        assert!(!g.admit("gps", 2, 10_300, 10.0)); // 300 < 900
        assert!(g.admit("gps", 2, 11_200, 10.0));
        // A rejected write does not move the cadence anchor.
        assert!(g.admit("imu", 1, 0, 10.0));
        assert!(!g.admit("imu", 2, 50, 10.0));
        assert!(g.admit("imu", 3, 100, 10.0));
    }
}
