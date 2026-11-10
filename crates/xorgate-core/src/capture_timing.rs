//! The capture host's per-fragment timing lines (Phase 7a §Design 8.1-8.3,
//! R4/R5): the ONE definition of the `XG-FRAGMENT` / `XG-FRAGMENT-END` stdout
//! line format, shared by the emitter (`xorgate-agent` `capture_host`, WP1)
//! and the consumer (the capture supervisor's `consume`, WP2). No TS
//! counterpart: gst-launch never prints these, so they exist only on the
//! `XORGATE_CAPTURE_BACKEND=host` path.
//!
//! One line per event, compact JSON after a fixed prefix, flushed per line:
//!
//! ```text
//! XG-FRAGMENT {"v":1,"id":N,"frameMonoNs":…,"cbMonoNs":…,"cbWallMs":…,"lagMs":…,"startTs":…}
//! XG-FRAGMENT-END {"v":1,"id":N,"endMonoNs":…,"cbMonoNs":…,"cbWallMs":…,"endTs":…}
//! ```
//!
//! - `id` is splitmuxsink's `fragment_id` = the file's `%05d` sequence
//!   (`start-index` at its default 0; pinned by WP1).
//! - `*MonoNs` are CLOCK_MONOTONIC nanoseconds (the pipeline clock is checked
//!   to be a monotonic `GstSystemClock` before any line is emitted), the same
//!   frame as the agent's `hrtime_ns`.
//! - `frameMonoNs = base_time + running_time(first sample PTS)`;
//!   `cbMonoNs` = the pipeline clock read in the callback; `cbWallMs` =
//!   CLOCK_REALTIME read right after it; `lagMs = round((cbMonoNs −
//!   frameMonoNs) / 1e6)`; `startTs = round((cbWall_ns − lag_ns) / 1e6)`.
//!   `startTs` is computed from the nanosecond values, so it can differ by
//!   ±1 from `cbWallMs − lagMs`.
//! - `XG-FRAGMENT-END` is emitted once, for the LAST fragment, on a clean EOS:
//!   `endMonoNs` = base_time plus the final `splitmuxsink-fragment-closed`
//!   running time (the START of the fragment's last frame) plus one frame
//!   period, i.e. the END of its last frame; `endTs = round((cbWall_ns −
//!   (cbMonoNs − endMonoNs)) / 1e6)`. On an EOS forced by SIGINT the last
//!   frame can still be "running", so `endMonoNs` may exceed `cbMonoNs` by
//!   up to one frame period (measured on the rig, WP1); the guard applies to
//!   the last frame's START.
//! - A stamp outside the [`lag_in_guard`] bound is never emitted (the
//!   fragment keeps the legacy anchors, §Design 8.2).
//!
//! Versioning: `v` is [`VERSION`]. A parser refuses any other `v` (the
//! fragment then falls back to legacy timing); unknown extra fields are
//! ignored, so a later additive field does not need a version bump.

use serde::{Deserialize, Serialize};

/// The line format version every emitted line carries as `"v"`.
pub const VERSION: u32 = 1;

/// The prefix of a per-fragment start line (note the trailing space).
pub const FRAGMENT_PREFIX: &str = "XG-FRAGMENT ";
/// The prefix of the final fragment's end line (note the trailing space).
pub const FRAGMENT_END_PREFIX: &str = "XG-FRAGMENT-END ";

/// The capture GOP: `key-int-max = fps × 2` (`video_capture.rs`
/// `build_capture_pipeline_args`), i.e. one keyframe every 2 s at any preset.
pub const GOP_NS: u64 = 2_000_000_000;
/// The validity guard's upper bound: `3 × GOP + 1 s` (§Design 8.2).
pub const MAX_LAG_NS: u64 = 3 * GOP_NS + 1_000_000_000;

/// `0 ≤ lag ≤ 3 × GOP + 1 s`. A negative lag (`now < frame`) or one beyond
/// the bound means the clock domain is wrong (for example a source PTS that
/// is not on the pipeline clock).
pub fn lag_in_guard(lag_ns: i128) -> bool {
    (0..=i128::from(MAX_LAG_NS)).contains(&lag_ns)
}

/// `XG-FRAGMENT`: the exact start of one fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FragmentStart {
    pub v: u32,
    pub id: u32,
    pub frame_mono_ns: u64,
    pub cb_mono_ns: u64,
    pub cb_wall_ms: u64,
    pub lag_ms: u64,
    pub start_ts: u64,
}

/// `XG-FRAGMENT-END`: the exact end of the last fragment (clean EOS only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FragmentEnd {
    pub v: u32,
    pub id: u32,
    pub end_mono_ns: u64,
    pub cb_mono_ns: u64,
    pub cb_wall_ms: u64,
    pub end_ts: u64,
}

/// Why a stamp was not produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// `cbMonoNs − frameMonoNs` outside [`lag_in_guard`] (value in ns).
    Guard(i128),
    /// The wall clock reads before the lag-corrected instant's epoch (ns).
    Wall(u64),
}

fn ns_to_ms_round(ns: u64) -> u64 {
    ns.saturating_add(500_000) / 1_000_000
}

/// The wall instant (ms, rounded) of a mono instant `lag_ns` before the
/// callback, from the callback's wall reading.
fn wall_before(cb_wall_ns: u64, lag_ns: u64) -> Result<u64, Reject> {
    let ns = cb_wall_ns
        .checked_sub(lag_ns)
        .ok_or(Reject::Wall(cb_wall_ns))?;
    let ms = ns_to_ms_round(ns);
    if ms == 0 {
        return Err(Reject::Wall(cb_wall_ns));
    }
    Ok(ms)
}

impl FragmentStart {
    /// §Design 8.1: the stamp from the four readings, guarded (§Design 8.2).
    /// `frame_mono_ns` = base_time + running_time(PTS); `cb_mono_ns` = the
    /// pipeline clock in the callback; `cb_wall_ns` = CLOCK_REALTIME right
    /// after it.
    pub fn from_clocks(
        id: u32,
        frame_mono_ns: u64,
        cb_mono_ns: u64,
        cb_wall_ns: u64,
    ) -> Result<Self, Reject> {
        let lag = i128::from(cb_mono_ns) - i128::from(frame_mono_ns);
        if !lag_in_guard(lag) {
            return Err(Reject::Guard(lag));
        }
        let lag_ns = lag as u64;
        Ok(Self {
            v: VERSION,
            id,
            frame_mono_ns,
            cb_mono_ns,
            cb_wall_ms: ns_to_ms_round(cb_wall_ns),
            lag_ms: ns_to_ms_round(lag_ns),
            start_ts: wall_before(cb_wall_ns, lag_ns)?,
        })
    }

    /// The full line, without the newline.
    pub fn to_line(&self) -> String {
        format!("{FRAGMENT_PREFIX}{}", to_json(self))
    }
}

impl FragmentEnd {
    /// The end stamp of the last fragment: its last frame STARTS at
    /// `last_frame_mono_ns` (guarded like a start) and lasts
    /// `frame_period_ns`.
    pub fn from_clocks(
        id: u32,
        last_frame_mono_ns: u64,
        frame_period_ns: u64,
        cb_mono_ns: u64,
        cb_wall_ns: u64,
    ) -> Result<Self, Reject> {
        let lag = i128::from(cb_mono_ns) - i128::from(last_frame_mono_ns);
        if !lag_in_guard(lag) {
            return Err(Reject::Guard(lag));
        }
        let end_mono_ns = last_frame_mono_ns.saturating_add(frame_period_ns);
        // cb − end may be negative (the last frame still "running").
        let end_wall = i128::from(cb_wall_ns) - (i128::from(cb_mono_ns) - i128::from(end_mono_ns));
        let end_wall = u64::try_from(end_wall).map_err(|_| Reject::Wall(cb_wall_ns))?;
        let end_ts = ns_to_ms_round(end_wall);
        if end_ts == 0 {
            return Err(Reject::Wall(cb_wall_ns));
        }
        Ok(Self {
            v: VERSION,
            id,
            end_mono_ns,
            cb_mono_ns,
            cb_wall_ms: ns_to_ms_round(cb_wall_ns),
            end_ts,
        })
    }

    /// The full line, without the newline.
    pub fn to_line(&self) -> String {
        format!("{FRAGMENT_END_PREFIX}{}", to_json(self))
    }
}

fn to_json<T: Serialize>(v: &T) -> String {
    // Plain structs of integers: serialisation cannot fail.
    serde_json::to_string(v).unwrap_or_default()
}

/// One parsed timing line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingLine {
    Start(FragmentStart),
    End(FragmentEnd),
}

impl TimingLine {
    /// The fragment id (`%05d` sequence) the line is about.
    pub fn id(&self) -> u32 {
        match self {
            Self::Start(s) => s.id,
            Self::End(e) => e.id,
        }
    }
}

/// A line that carries a timing prefix but cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The JSON after the prefix did not parse into the struct.
    Json(String),
    /// `v` is not [`VERSION`].
    Version(u32),
}

/// `None` = not a timing line (every gst-launch line); `Some(Err)` = a timing
/// prefix with an unusable body (the fragment falls back to legacy timing).
/// The line is one stdout line without its newline (as the child pipe drain
/// hands it over).
pub fn parse_line(line: &str) -> Option<Result<TimingLine, ParseError>> {
    if let Some(body) = line.strip_prefix(FRAGMENT_END_PREFIX) {
        return Some(parse_body::<FragmentEnd>(body).map(TimingLine::End));
    }
    if let Some(body) = line.strip_prefix(FRAGMENT_PREFIX) {
        return Some(parse_body::<FragmentStart>(body).map(TimingLine::Start));
    }
    None
}

#[derive(Deserialize)]
struct VersionOnly {
    v: u32,
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, ParseError> {
    let v: VersionOnly = serde_json::from_str(body).map_err(|e| ParseError::Json(e.to_string()))?;
    if v.v != VERSION {
        return Err(ParseError::Version(v.v));
    }
    serde_json::from_str(body).map_err(|e| ParseError::Json(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;
    const MS: u64 = 1_000_000;

    #[test]
    fn start_line_has_the_frozen_key_order_and_round_trips() {
        // 720p15: the first frame was captured 2.1 s before the callback.
        let wall = 1_762_400_000_000 * MS + 400_000; // …000.4 ms
        let s = FragmentStart::from_clocks(3, 100 * S, 102 * S + 100 * MS, wall).unwrap();
        assert_eq!(
            s.to_line(),
            "XG-FRAGMENT {\"v\":1,\"id\":3,\"frameMonoNs\":100000000000,\"cbMonoNs\":102100000000,\
             \"cbWallMs\":1762400000000,\"lagMs\":2100,\"startTs\":1762399997900}"
        );
        assert_eq!(parse_line(&s.to_line()), Some(Ok(TimingLine::Start(s))));
        assert_eq!(TimingLine::Start(s).id(), 3);
    }

    #[test]
    fn end_line_has_the_frozen_key_order_and_round_trips() {
        // Last frame starts at 50 s, lasts 0 ns here; the close is 40 ms later.
        let e = FragmentEnd::from_clocks(7, 50 * S, 0, 50 * S + 40 * MS, 1_762_400_000_000 * MS)
            .unwrap();
        assert_eq!(
            e.to_line(),
            "XG-FRAGMENT-END {\"v\":1,\"id\":7,\"endMonoNs\":50000000000,\"cbMonoNs\":50040000000,\
             \"cbWallMs\":1762400000000,\"endTs\":1762399999960}"
        );
        assert_eq!(parse_line(&e.to_line()), Some(Ok(TimingLine::End(e))));
    }

    #[test]
    fn start_ts_is_rounded_from_nanoseconds() {
        // wall …000.6 ms, lag 2 100.2 ms → start …997 900.4 → 1762399997900.
        let wall = 1_762_400_000_000 * MS + 600_000;
        let s = FragmentStart::from_clocks(0, 0, 2_100 * MS + 200_000, wall).unwrap();
        assert_eq!(s.cb_wall_ms, 1_762_400_000_001);
        assert_eq!(s.lag_ms, 2_100);
        assert_eq!(s.start_ts, 1_762_399_997_900);
    }

    #[test]
    fn the_guard_is_zero_to_three_gops_plus_one_second() {
        assert_eq!(MAX_LAG_NS, 7 * S);
        assert!(lag_in_guard(0));
        assert!(lag_in_guard(i128::from(7 * S)));
        assert!(!lag_in_guard(-1));
        assert!(!lag_in_guard(i128::from(7 * S) + 1));
        let wall = 1_762_400_000_000 * MS;
        assert_eq!(
            FragmentStart::from_clocks(0, 10 * S, 10 * S - 1, wall),
            Err(Reject::Guard(-1))
        );
        assert_eq!(
            FragmentStart::from_clocks(0, 10 * S, 17 * S + 1, wall),
            Err(Reject::Guard(i128::from(7 * S) + 1))
        );
        assert!(FragmentStart::from_clocks(0, 10 * S, 17 * S, wall).is_ok());
        assert_eq!(
            FragmentEnd::from_clocks(0, 10 * S, 0, 9 * S, wall),
            Err(Reject::Guard(-i128::from(S)))
        );
    }

    /// Measured on the rig (WP1): at an EOS forced by SIGINT the final close
    /// comes 1-2 ms after the last frame STARTED, so the frame's end (+1
    /// frame period) lies in the future of the callback. The stamp is still
    /// valid: the guard is on the frame's start, the end is ahead of `cb`.
    #[test]
    fn the_end_may_lie_up_to_one_frame_ahead_of_the_close() {
        let wall = 1_762_400_000_000 * MS;
        let period = 66_666_666;
        let e = FragmentEnd::from_clocks(4, 100 * S, period, 100 * S + 2 * MS, wall).unwrap();
        assert_eq!(e.end_mono_ns, 100 * S + period);
        // end = wall + (66.67 − 2) ms.
        assert_eq!(e.end_ts, 1_762_400_000_065);
        assert!(e.end_mono_ns > e.cb_mono_ns);
    }

    #[test]
    fn a_wall_clock_before_the_epoch_offset_is_rejected() {
        assert_eq!(
            FragmentStart::from_clocks(0, 0, 2 * S, S),
            Err(Reject::Wall(S))
        );
    }

    #[test]
    fn gst_launch_lines_are_not_timing_lines() {
        for l in [
            "New clock: GstSystemClock",
            "Got EOS from element \"pipeline0\".",
            "XG-FRAGMENTS {}",
            "XG-FRAGMENT",
            "",
        ] {
            assert_eq!(parse_line(l), None, "{l:?}");
        }
    }

    #[test]
    fn other_versions_and_bad_bodies_are_refused_unknown_fields_ignored() {
        assert_eq!(
            parse_line("XG-FRAGMENT {\"v\":2,\"id\":0}"),
            Some(Err(ParseError::Version(2)))
        );
        assert!(matches!(
            parse_line("XG-FRAGMENT {\"v\":1,\"id\":0}"),
            Some(Err(ParseError::Json(_)))
        ));
        assert!(matches!(
            parse_line("XG-FRAGMENT-END not json"),
            Some(Err(ParseError::Json(_)))
        ));
        let l = "XG-FRAGMENT {\"v\":1,\"id\":1,\"frameMonoNs\":1,\"cbMonoNs\":2,\"cbWallMs\":3,\
                 \"lagMs\":0,\"startTs\":3,\"later\":true}";
        assert!(matches!(parse_line(l), Some(Ok(TimingLine::Start(_)))));
    }
}
