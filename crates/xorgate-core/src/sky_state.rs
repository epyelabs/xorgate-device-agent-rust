//! Sky-visibility tracker — 1:1 port of `src/lib/sky-state.ts` (254 lines).
//!
//! GSV lists satellites the receiver merely EXPECTS with a blank C/N0 and
//! fills the field in only for satellites it is actually HEARING, so `heard`
//! (not `inView`) is the RF signal. Four states (`sky-state.ts:12-25`):
//! `clear` / `obstructed` / `no-rf` / `unknown` — `unknown` is NOT `no-rf`:
//! a silenced NMEA stream is the brownout/wedge signature, the exact case
//! where the daemon's wedge restart should still fire.
//!
//! TIME: every window is measured on a MONOTONIC clock (invariant 2). The TS
//! default clock (`monotonicMs` from `recording/boot-policy.ts:37,138`) is
//! replaced by an explicit `now: MonoMs` argument on [`SkyTracker::note`] and
//! [`SkyTracker::snapshot`] — core never reads a clock.
//!
//! `SkyState` and `GnssView` live in [`crate::types`] (WP1) and are re-exported
//! here; their fields match `sky-state.ts:39` / `:109-112` exactly.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::clock::MonoMs;
use crate::nmea::GsvMessage;
use crate::round::js_math_round;
use crate::telemetry::{js_num, js_num_opt};
pub use crate::types::{GnssView, SkyState};

/// `SkyThresholds` (`sky-state.ts:47-66`): GNSS physics, deliberately code
/// constants rather than device settings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkyThresholds {
    /// C/N0 at or above which a satellite counts as usable, dBHz.
    #[serde(with = "js_num")]
    pub usable_snr_db: f64,
    /// Usable satellites required for `clear` (4 = the minimum for a 3D fix).
    pub clear_min_sats: u32,
    /// `clear` must hold this long before it is published.
    pub clear_hold_ms: u64,
    /// Leaving `clear` needs this long of not-clear.
    pub leave_clear_ms: u64,
    /// Nothing heard must persist this long before `no-rf` is claimed.
    pub no_rf_hold_ms: u64,
    /// Hearing something again leaves `no-rf` after this long.
    pub leave_no_rf_ms: u64,
    /// A talker's satellite set older than this is dropped; none fresh ⇒ `unknown`.
    pub data_stale_ms: u64,
}

/// `SKY_DEFAULTS` (`sky-state.ts:68-79`).
pub const SKY_DEFAULTS: SkyThresholds = SkyThresholds {
    usable_snr_db: 20.0,
    clear_min_sats: 4,
    clear_hold_ms: 5_000,
    leave_clear_ms: 15_000,
    // 60 s of complete silence: longer than any underpass / tree tunnel,
    // a small fraction of the video gate's 600 s give-up cap.
    no_rf_hold_ms: 60_000,
    leave_no_rf_ms: 5_000,
    data_stale_ms: 15_000,
};

impl Default for SkyThresholds {
    fn default() -> Self {
        SKY_DEFAULTS
    }
}

/// `SKY_STATE_CODES` (`sky-state.ts:82-86`): coded form for telemetry, 0 is
/// best (matching `lte.gnss_health`); `unknown` has no code (omitted).
pub const SKY_STATE_CODES: [(SkyState, u8); 3] = [
    (SkyState::Clear, 0),
    (SkyState::Obstructed, 1),
    (SkyState::NoRf, 2),
];

/// `SKY_STATE_CODES[state]`, `None` for `unknown`.
pub fn sky_state_code(state: SkyState) -> Option<u8> {
    SKY_STATE_CODES
        .iter()
        .find(|(s, _)| *s == state)
        .map(|(_, c)| *c)
}

/// `SkySnapshot` (`sky-state.ts:88-100`). Key order = the TS interface.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkySnapshot {
    pub state: SkyState,
    /// Satellites reporting a C/N0 at all — the actual RF evidence.
    pub heard: u32,
    /// Satellites at or above `usableSnrDb`.
    pub usable: u32,
    /// Satellites LISTED in view; almanac-driven, so not by itself an RF signal.
    pub in_view: i64,
    /// Best C/N0 this cycle, dBHz; null when nothing is heard.
    #[serde(with = "js_num_opt")]
    pub max_snr: Option<f64>,
    /// How long (monotonic ms) the published state has held.
    pub hold_ms: u64,
}

/// A talker's most recently COMPLETED message cycle (`TalkerView`, `sky-state.ts:128`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct TalkerView {
    in_view: i64,
    heard: u32,
    usable: u32,
    max_snr: Option<f64>,
    at_ms: MonoMs,
}

/// An in-flight cycle (`pending` map value, `sky-state.ts:141`).
#[derive(Debug, Clone, PartialEq)]
struct PendingCycle {
    /// prn → snr, in arrival order (a JS Map keyed by prn).
    sats: IndexMap<i64, Option<f64>>,
    in_view: i64,
    next: i64,
}

/// `SkyTracker` / `createSkyTracker` (`sky-state.ts:114-254`): the hysteresis
/// classifier. Construct with the monotonic time of creation (the TS reads
/// `monoMs()` once for `stateSinceMs`, `:146`).
#[derive(Debug, Clone)]
pub struct SkyTracker {
    cfg: SkyThresholds,
    pending: IndexMap<String, PendingCycle>,
    views: IndexMap<String, TalkerView>,
    state: SkyState,
    state_since_ms: MonoMs,
    /// Raw (un-hysteresised) classification and when it last changed.
    raw: Option<SkyState>,
    raw_since_ms: MonoMs,
}

impl SkyTracker {
    pub fn new(thresholds: SkyThresholds, now: MonoMs) -> Self {
        Self {
            cfg: thresholds,
            pending: IndexMap::new(),
            views: IndexMap::new(),
            state: SkyState::Unknown,
            state_since_ms: now,
            raw: None,
            raw_since_ms: now,
        }
    }

    /// `createSkyTracker({ monoMs })` with `SKY_DEFAULTS`.
    pub fn with_defaults(now: MonoMs) -> Self {
        Self::new(SKY_DEFAULTS, now)
    }

    pub fn thresholds(&self) -> &SkyThresholds {
        &self.cfg
    }

    /// `commit` (`sky-state.ts:151-162`): reduce a completed cycle to a talker view.
    fn commit(
        &mut self,
        talker: &str,
        sats: &IndexMap<i64, Option<f64>>,
        in_view: i64,
        now: MonoMs,
    ) {
        let mut heard = 0;
        let mut usable = 0;
        let mut max_snr: Option<f64> = None;
        for snr in sats.values() {
            let Some(snr) = *snr else { continue };
            if snr <= 0.0 {
                continue;
            }
            heard += 1;
            if snr >= self.cfg.usable_snr_db {
                usable += 1;
            }
            if max_snr.is_none_or(|m| snr > m) {
                max_snr = Some(snr);
            }
        }
        self.views.insert(
            talker.to_string(),
            TalkerView {
                in_view,
                heard,
                usable,
                max_snr,
                at_ms: now,
            },
        );
    }

    /// `classify` (`sky-state.ts:164-168`).
    fn classify(&self, heard: u32, usable: u32) -> SkyState {
        if usable >= self.cfg.clear_min_sats {
            SkyState::Clear
        } else if heard == 0 {
            SkyState::NoRf
        } else {
            SkyState::Obstructed
        }
    }

    /// `evaluate` (`sky-state.ts:176-224`): fold the fresh talker views into
    /// one reading and advance the state machine. Runs on data ARRIVAL and on
    /// every snapshot, so hold times accrue on the NMEA cadence and the drop to
    /// `unknown` happens even when nothing arrives at all.
    fn evaluate(&mut self, now: MonoMs) -> SkySnapshot {
        let stale = self.cfg.data_stale_ms;
        self.views
            .retain(|_, v| now.saturating_sub(v.at_ms) <= stale);
        let mut heard = 0;
        let mut usable = 0;
        let mut in_view = 0;
        let mut max_snr: Option<f64> = None;
        let mut fresh = false;
        for v in self.views.values() {
            fresh = true;
            heard += v.heard;
            usable += v.usable;
            in_view += v.in_view;
            if let Some(m) = v.max_snr {
                if max_snr.is_none_or(|cur| m > cur) {
                    max_snr = Some(m);
                }
            }
        }

        let next = if fresh {
            Some(self.classify(heard, usable))
        } else {
            None
        };
        if next != self.raw {
            self.raw = next;
            self.raw_since_ms = now;
        }
        let held = now.saturating_sub(self.raw_since_ms);

        let mut want = self.state;
        match next {
            None => {
                // No fresh GSV: no opinion. Never claim `no-rf` here — a silent
                // NMEA stream is the wedge signature, not a missing sky.
                want = SkyState::Unknown;
            }
            Some(next) => {
                if self.state == SkyState::Clear {
                    if next != SkyState::Clear && held >= self.cfg.leave_clear_ms {
                        want = SkyState::Obstructed;
                    }
                } else if self.state == SkyState::NoRf {
                    if next != SkyState::NoRf && held >= self.cfg.leave_no_rf_ms {
                        want = if next == SkyState::Clear {
                            SkyState::Clear
                        } else {
                            SkyState::Obstructed
                        };
                    }
                } else if next == SkyState::Clear && held >= self.cfg.clear_hold_ms {
                    want = SkyState::Clear;
                } else if next == SkyState::NoRf && held >= self.cfg.no_rf_hold_ms {
                    want = SkyState::NoRf;
                } else {
                    // Includes "heard nothing, but not for long enough to call
                    // it": honest, and exactly what `obstructed` means.
                    want = SkyState::Obstructed;
                }
            }
        }
        if want != self.state {
            self.state = want;
            self.state_since_ms = now;
        }
        SkySnapshot {
            state: self.state,
            heard,
            usable,
            in_view,
            max_snr,
            hold_ms: now.saturating_sub(self.state_since_ms),
        }
    }

    /// `note(msg)` (`sky-state.ts:227-248`): feed one parsed GSV sentence (any
    /// talker, any position in its cycle). A cycle starts on message 1; a
    /// stream joined mid-cycle is skipped until its next message 1.
    pub fn note(&mut self, msg: &GsvMessage, now: MonoMs) {
        let cur = if msg.msg_num == 1 {
            self.pending.insert(
                msg.talker.clone(),
                PendingCycle {
                    sats: IndexMap::new(),
                    in_view: msg.in_view,
                    next: 1,
                },
            );
            self.pending.get_mut(&msg.talker).expect("just inserted")
        } else {
            match self.pending.get_mut(&msg.talker) {
                Some(cur) if cur.next == msg.msg_num => cur,
                _ => {
                    self.pending.shift_remove(&msg.talker);
                    return;
                }
            }
        };
        for sat in &msg.satellites {
            cur.sats.insert(sat.prn, sat.snr);
        }
        cur.in_view = msg.in_view;
        cur.next = msg.msg_num + 1;
        if msg.msg_num >= msg.num_msgs {
            let done = self
                .pending
                .shift_remove(&msg.talker)
                .expect("cycle in flight");
            self.commit(&msg.talker, &done.sats, done.in_view, now);
            self.evaluate(now);
        }
    }

    /// `snapshot()` (`sky-state.ts:250-252`): evaluate + return the current state.
    pub fn snapshot(&mut self, now: MonoMs) -> SkySnapshot {
        self.evaluate(now)
    }
}

/// The daemon-facing `gnss-sky.json` document (`src/agent/collectors/gps.ts:243-252`
/// `skyNow`). The sim7600-lte daemon reads `timestamp` (freshness) and
/// `state` (`modem/sim7600-lte.sh:1452-1470`), so the key names are a
/// contract (plan Appendix A, Files row). `timestamp` is
/// `new Date(now).toISOString()` of the WALL clock at write time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GnssSkyFile {
    pub v: u32,
    pub timestamp: String,
    pub state: SkyState,
    pub heard: u32,
    pub usable: u32,
    pub in_view: i64,
    #[serde(with = "js_num_opt")]
    pub max_snr: Option<f64>,
    /// `Math.round(holdMs / 1000)`.
    pub hold_s: i64,
}

impl GnssSkyFile {
    /// Build the document from a snapshot and the wall-clock write time.
    pub fn from_snapshot(snap: &SkySnapshot, wall_ms: i64) -> Self {
        Self {
            v: 1,
            timestamp: crate::js::to_iso_string(wall_ms as f64),
            state: snap.state,
            heard: snap.heard,
            usable: snap.usable,
            in_view: snap.in_view,
            max_snr: snap.max_snr,
            hold_s: js_math_round(snap.hold_ms as f64 / 1000.0) as i64,
        }
    }
}

/// `SKY_STATE_FILENAME` (`collectors/gps.ts:82`).
pub const SKY_STATE_FILENAME: &str = "gnss-sky.json";
/// `SKY_PUBLISH_MS` (`collectors/gps.ts:84`): refresh cadence for that file
/// when nothing changes (state changes write immediately).
pub const SKY_PUBLISH_MS: u64 = 15_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_defaults() {
        assert_eq!(sky_state_code(SkyState::Clear), Some(0));
        assert_eq!(sky_state_code(SkyState::Obstructed), Some(1));
        assert_eq!(sky_state_code(SkyState::NoRf), Some(2));
        assert_eq!(sky_state_code(SkyState::Unknown), None);
        assert_eq!(
            serde_json::to_string(&SKY_DEFAULTS).unwrap(),
            r#"{"usableSnrDb":20,"clearMinSats":4,"clearHoldMs":5000,"leaveClearMs":15000,"noRfHoldMs":60000,"leaveNoRfMs":5000,"dataStaleMs":15000}"#
        );
    }

    #[test]
    fn fresh_tracker_is_unknown() {
        let mut t = SkyTracker::with_defaults(1_000);
        let s = t.snapshot(1_000);
        assert_eq!(
            s,
            SkySnapshot {
                state: SkyState::Unknown,
                heard: 0,
                usable: 0,
                in_view: 0,
                max_snr: None,
                hold_ms: 0
            }
        );
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"state":"unknown","heard":0,"usable":0,"inView":0,"maxSnr":null,"holdMs":0}"#
        );
    }
}
