//! Phase 7a WP2: the agent half of exact sidecar timing
//! (phase-7a-inprocess-gstreamer.md §8.3–8.5, §9).
//!
//! - `XORGATE_SIDECAR_TIMING` = `legacy` (default, byte parity with JS
//!   09da38d) | `exact`. `exact` needs the capture host
//!   (`XORGATE_CAPTURE_BACKEND=host`): with `gst-launch` there are no timing
//!   lines, so it warns ONCE and stays `legacy`.
//! - `exact` must NOT be enabled on any device before the backend accepts
//!   the `"pts"` anchor (`media-segments.ts` zod enum, OpenAPI, SDK type;
//!   user-owned, phase-7a K15): an unknown anchor DLQs the segment event.
//! - The host's `XG-FRAGMENT` / `XG-FRAGMENT-END` stdout lines feed the run's
//!   [`TimingTable`] (cleared per spawn); every line triggers a sweep.
//!
//! The line format is WP1's `xorgate_core::capture_timing` (the one
//! definition, shared with the host's emitter); this file only feeds it into
//! the run's table.

use std::collections::BTreeMap;
use std::sync::Once;

use xorgate_core::capture_timing::{self, ParseError, TimingLine};
use xorgate_core::sidecar::{StampReject, TimingTable};

/// The switch (phase-7a §Design 9). Unset/empty = `legacy`.
pub const SIDECAR_TIMING_ENV: &str = "XORGATE_SIDECAR_TIMING";

/// How sidecar `startTs`/`durationMs`/`anchor` are derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidecarTiming {
    /// Birthtime anchors (`fstat`/`boundary`/`computed`), byte parity with JS.
    #[default]
    Legacy,
    /// The capture host's per-fragment stamps (anchor `"pts"`).
    Exact,
}

/// `XORGATE_SIDECAR_TIMING` → mode: unset/empty/`legacy` → `Legacy`,
/// `exact` → `Exact` (trimmed, case-sensitive); anything else → `Err(value)`.
pub fn select_sidecar_timing(value: Option<&str>) -> Result<SidecarTiming, String> {
    match value.map(str::trim) {
        None | Some("") | Some("legacy") => Ok(SidecarTiming::Legacy),
        Some("exact") => Ok(SidecarTiming::Exact),
        Some(other) => Err(other.to_string()),
    }
}

/// What the run will actually do, and the one warning to log (if any).
pub fn resolve_sidecar_timing(
    value: Option<&str>,
    backend_emits_timing: bool,
) -> (SidecarTiming, Option<String>) {
    match select_sidecar_timing(value) {
        Err(bad) => (
            SidecarTiming::Legacy,
            Some(format!(
                "[video-capture] {SIDECAR_TIMING_ENV}={bad:?} is not legacy|exact; using legacy"
            )),
        ),
        Ok(SidecarTiming::Exact) if !backend_emits_timing => (
            SidecarTiming::Legacy,
            Some(format!(
                "[video-capture] {SIDECAR_TIMING_ENV}=exact needs XORGATE_CAPTURE_BACKEND=host \
                 (gst-launch emits no fragment timing); using legacy"
            )),
        ),
        Ok(t) => (t, None),
    }
}

/// [`resolve_sidecar_timing`] for this process's environment; the warning
/// is logged ONCE per process (the supervisor is rebuilt on every video
/// transition).
pub fn sidecar_timing_from_env(backend_emits_timing: bool) -> SidecarTiming {
    let (t, warn) = resolve_sidecar_timing(
        std::env::var(SIDECAR_TIMING_ENV).ok().as_deref(),
        backend_emits_timing,
    );
    if let Some(w) = warn {
        static WARN: Once = Once::new();
        WARN.call_once(|| tracing::warn!("{w}"));
    }
    t
}

/// `line` as a timing line (WP1's [`capture_timing::parse_line`], the one
/// definition of the §8.3 format): `None` = not one (every other host /
/// gst-launch line); `Some(Err)` = a timing prefix the agent cannot use (bad
/// JSON, unknown version), warned once per session — the fragment falls
/// back to the legacy anchors.
pub fn parse_timing_line(line: &str) -> Option<Result<TimingLine, ParseError>> {
    capture_timing::parse_line(line)
}

/// The run's exact-timing state: the table plus the deferral bookkeeping.
/// One per spawn ([`RunTiming::default`] at every spawn: a new session dir,
/// so ids never carry over).
#[derive(Debug, Default)]
pub struct RunTiming {
    pub table: TimingTable,
    /// seq → agent mono ms of the sweep that first deferred it.
    pub deferred_since: BTreeMap<u64, f64>,
    /// One timing warning per session.
    pub warned: bool,
}

impl RunTiming {
    /// Feed one line; `Err` = the warning detail (the caller warns once).
    pub fn apply(&mut self, line: &TimingLine) -> Result<(), StampReject> {
        match line {
            TimingLine::Start(s) => self.table.record_start(
                u64::from(s.id),
                s.start_ts as f64,
                s.lag_ms as f64,
                s.cb_wall_ms as f64,
            ),
            TimingLine::End(e) => self.table.record_end(u64::from(e.id), e.end_ts as f64),
        }
    }

    /// Keep the first deferral instant of every fragment still deferred.
    pub fn note_deferred(&mut self, deferred: &[u64], now_ms: f64) {
        self.deferred_since.retain(|id, _| deferred.contains(id));
        for id in deferred {
            self.deferred_since.entry(*id).or_insert(now_ms);
        }
    }

    /// The earliest instant a deferred fragment's budget runs out.
    pub fn next_deadline(&self, defer_max_ms: f64) -> Option<f64> {
        self.deferred_since
            .values()
            .fold(None, |m: Option<f64>, s| Some(m.map_or(*s, |m| m.min(*s))))
            .map(|s| s + defer_max_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_defaults_to_legacy() {
        assert_eq!(select_sidecar_timing(None), Ok(SidecarTiming::Legacy));
        assert_eq!(select_sidecar_timing(Some("")), Ok(SidecarTiming::Legacy));
        assert_eq!(
            select_sidecar_timing(Some(" legacy")),
            Ok(SidecarTiming::Legacy)
        );
        assert_eq!(
            select_sidecar_timing(Some("exact\n")),
            Ok(SidecarTiming::Exact)
        );
        assert_eq!(select_sidecar_timing(Some("Exact")), Err("Exact".into()));
        assert_eq!(SidecarTiming::default(), SidecarTiming::Legacy);
    }

    #[test]
    fn exact_with_gst_launch_warns_and_stays_legacy() {
        let (t, w) = resolve_sidecar_timing(Some("exact"), false);
        assert_eq!(t, SidecarTiming::Legacy);
        assert!(w.unwrap().contains("needs XORGATE_CAPTURE_BACKEND=host"));
        assert_eq!(
            resolve_sidecar_timing(Some("exact"), true),
            (SidecarTiming::Exact, None)
        );
        assert_eq!(
            resolve_sidecar_timing(None, true),
            (SidecarTiming::Legacy, None)
        );
        assert_eq!(
            resolve_sidecar_timing(None, false),
            (SidecarTiming::Legacy, None)
        );
        assert_eq!(
            resolve_sidecar_timing(Some("legacy"), false),
            (SidecarTiming::Legacy, None)
        );
        let (t, w) = resolve_sidecar_timing(Some("pts"), true);
        assert_eq!(t, SidecarTiming::Legacy);
        assert!(w.unwrap().contains("is not legacy|exact"));
    }

    fn start(id: u32, start_ts: u64, lag_ms: u64) -> TimingLine {
        TimingLine::Start(capture_timing::FragmentStart {
            v: capture_timing::VERSION,
            id,
            frame_mono_ns: 100_000_000_000,
            cb_mono_ns: 100_000_000_000 + lag_ms * 1_000_000,
            cb_wall_ms: start_ts + lag_ms,
            lag_ms,
            start_ts,
        })
    }

    /// The agent reads WP1's lines through WP1's parser (no second definition).
    #[test]
    fn reads_wp1_lines() {
        let line = match start(3, 1_760_000_180_300, 2_100) {
            TimingLine::Start(s) => s.to_line(),
            TimingLine::End(_) => unreachable!(),
        };
        assert_eq!(
            parse_timing_line(&line),
            Some(Ok(start(3, 1_760_000_180_300, 2_100)))
        );
        assert_eq!(parse_timing_line("New clock: GstSystemClock"), None);
        assert_eq!(
            parse_timing_line(r#"XG-FRAGMENT {"v":2,"id":0}"#),
            Some(Err(ParseError::Version(2)))
        );
        let mut r = RunTiming::default();
        r.apply(&parse_timing_line(&line).unwrap().unwrap())
            .unwrap();
        assert_eq!(r.table.exact(3).unwrap().start_ts, 1_760_000_180_300.0);
        assert_eq!(r.table.exact(3).unwrap().lag_ms, 2_100.0);
    }

    /// WP1 rig measurements: fragment 0's small lag is normal, and an END
    /// stamp ahead of its own callback wall (endMonoNs > cbMonoNs by up to
    /// one frame on a SIGINT EOS) is taken as is.
    #[test]
    fn wp1_measured_shapes_are_accepted() {
        let mut r = RunTiming::default();
        r.apply(&start(0, 1_760_000_000_300, 3)).unwrap();
        r.apply(&start(1, 1_760_000_060_300, 2_000)).unwrap();
        let end = capture_timing::FragmentEnd {
            v: capture_timing::VERSION,
            id: 1,
            end_mono_ns: 1_000_066_000_000,
            cb_mono_ns: 1_000_000_000_000,
            cb_wall_ms: 1_760_000_105_300,
            end_ts: 1_760_000_105_366,
        };
        let line = end.to_line();
        r.apply(&parse_timing_line(&line).unwrap().unwrap())
            .unwrap();
        assert_eq!(r.table.exact(0).unwrap().lag_ms, 3.0);
        assert_eq!(r.table.exact(1).unwrap().end_ts, Some(1_760_000_105_366.0));
    }

    #[test]
    fn run_timing_bookkeeping() {
        let mut r = RunTiming::default();
        r.apply(&start(0, 1_760_000_000_300, 2_100)).unwrap();
        assert_eq!(
            r.apply(&start(1, 1_760_000_060_300, 9_000)),
            Err(StampReject::Lag(9_000.0))
        );
        assert_eq!(
            r.apply(&TimingLine::End(capture_timing::FragmentEnd {
                v: 1,
                id: 1,
                end_mono_ns: 1,
                cb_mono_ns: 2,
                cb_wall_ms: 1_760_000_100_000,
                end_ts: 1_760_000_100_000,
            })),
            Err(StampReject::EndWithoutStart)
        );
        r.note_deferred(&[2, 3], 100.0);
        r.note_deferred(&[3, 4], 150.0);
        assert_eq!(r.deferred_since, BTreeMap::from([(3, 100.0), (4, 150.0)]));
        assert_eq!(r.next_deadline(10_000.0), Some(10_100.0));
        r.note_deferred(&[], 200.0);
        assert_eq!(r.next_deadline(10_000.0), None);
    }
}
