//! RGB status LED — the policy, as one pure function. 1:1 port of
//! `src/lib/led-state.ts` (199 lines).
//!
//! **The LED is a renderer, not a policy engine.** Every condition below is
//! already computed and already published by the module that owns it; this
//! file only ranks them (`plans/done/power_management_improvements/README.md`
//! §Principles 5: one choke point — live parking flows through the single
//! boolean in the manager). If the ladder ever needs a fact nobody publishes,
//! compute it once in the owning module and publish it — never work it out
//! here.
//!
//! Pure by contract: no I/O, no clocks, no randomness. The 2 s "config
//! applied" overlay is deliberately NOT here — it is time-based, so the LED
//! manager (Phase 4) owns it.

use serde::Serialize;

use crate::led::{LedColour, LedPattern};
use crate::telemetry::{ConnectionState, RecordingStatus};
use crate::types::GnssView;

/// `LedSignals.lte` (`led-state.ts:31`): `{ status?: string } | null`. The
/// TS reads the raw telemetry.json object and keeps `status` only when it is
/// a string; the caller does that `typeof` check when it builds this view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LteStatusView {
    pub status: Option<String>,
}

impl LteStatusView {
    /// The view of a parsed telemetry.json value: `typeof s.lte?.status ===
    /// "string" ? s.lte.status : null` (`led-state.ts:183`). A non-object
    /// value is the caller's `lte: null`; a missing or non-string `status`
    /// reads as `None` (fail open — "a malformed lte block is ignored").
    pub fn from_value(v: &serde_json::Value) -> Option<Self> {
        let obj = v.as_object()?;
        Some(Self {
            status: obj
                .get("status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        })
    }
}

/// `LedSignals` (`led-state.ts:25-36`).
#[derive(Debug, Clone, PartialEq)]
pub struct LedSignals<'a> {
    /// Cloud link, from the state store.
    pub connection: ConnectionState,
    /// `recording.status()` — carries liveBlock and powerShed.
    pub recording: &'a RecordingStatus,
    /// `/run/sim7600-lte/telemetry.json`. `None` = unavailable, NOT down.
    pub lte: Option<LteStatusView>,
    /// `source.gnssView()`. `None` = no receiver, NOT "no fix".
    pub gnss: Option<GnssView>,
    /// Agent still coming up — before real state exists.
    pub starting: bool,
}

/// `LedState` (`led-state.ts:38`). Key order = the `state()` literal
/// (`led-state.ts:132`): colour, pattern, reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LedState {
    pub colour: LedColour,
    pub pattern: LedPattern,
    /// Stable machine reason; also the key into [`LED_LADDER`].
    pub reason: &'static str,
}

/// `LedRung` (`led-state.ts:45`). Key order = the ladder literals
/// (`led-state.ts:63-118`): pri, reason, label, colour, pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LedRung {
    /// Priority as written in the plan's ladder table.
    pub pri: &'static str,
    pub reason: &'static str,
    /// Human label — drives `xorgate led test` and the legend card.
    pub label: &'static str,
    pub colour: LedColour,
    pub pattern: LedPattern,
}

/// `LED_LADDER` (`led-state.ts:62-119`): the agreed ladder (plan §5), in the
/// order `xorgate led test` walks it. The SINGLE definition: [`resolve_led_state`]
/// returns rungs from here and the CLI walks the same array.
///
/// Rung 3 (red blink-slow, "data loss") is deliberately absent —
/// `droppedSegments` is a lifetime counter, so consuming it would latch the
/// LED into a permanent false alarm (plans/rgb_led_events phase 2).
pub const LED_LADDER: [LedRung; 8] = [
    LedRung {
        pri: "0",
        reason: "starting",
        label: "Starting up (no real state yet)",
        colour: LedColour::White,
        pattern: LedPattern::Solid,
    },
    LedRung {
        pri: "1",
        reason: "agent-down",
        label: "Agent not running",
        colour: LedColour::Red,
        pattern: LedPattern::Solid,
    },
    LedRung {
        pri: "2",
        reason: "undervoltage",
        label: "Power fault — undervoltage / shedding",
        colour: LedColour::Red,
        pattern: LedPattern::BlinkFast,
    },
    LedRung {
        pri: "4",
        reason: "thermal",
        label: "Running hot — thermal shed, still recording",
        colour: LedColour::Cyan,
        pattern: LedPattern::BlinkSlow,
    },
    LedRung {
        pri: "5",
        reason: "network",
        label: "Network trouble — LTE/cloud down, recovering",
        colour: LedColour::Purple,
        pattern: LedPattern::BlinkFast,
    },
    LedRung {
        pri: "6",
        reason: "gps-wait",
        label: "Waiting for a GPS fix",
        colour: LedColour::Blue,
        pattern: LedPattern::BlinkSlow,
    },
    LedRung {
        pri: "7",
        reason: "healthy",
        label: "Healthy",
        colour: LedColour::Green,
        pattern: LedPattern::Heartbeat,
    },
    LedRung {
        pri: "—",
        reason: "config-applied",
        label: "Settings applied / command accepted",
        colour: LedColour::White,
        pattern: LedPattern::Flash2,
    },
];

/// `rung` (`led-state.ts:124-128`): look a rung up by machine reason. The TS
/// throws on an unknown reason ("only on a typo in this file"); Rust returns
/// `None` and lets [`state`] carry the panic for the in-file lookups.
pub fn rung(reason: &str) -> Option<&'static LedRung> {
    LED_LADDER.iter().find(|r| r.reason == reason)
}

/// `state` (`led-state.ts:130-133`).
fn state(reason: &'static str) -> LedState {
    let r = rung(reason).unwrap_or_else(|| panic!("no LED ladder rung for reason '{reason}'"));
    LedState {
        colour: r.colour,
        pattern: r.pattern,
        reason: r.reason,
    }
}

/// `resolveLedState` (`led-state.ts:143-199`): strict priority, first match
/// wins.
///
/// Every unknown resolves toward *fine* (fail open). A stale feed, a missing
/// collector or a `liveBlock.reason` this build has never heard of must never
/// light red: an LED crying wolf on missing information is worse than the
/// problem the rule was written about.
pub fn resolve_led_state(s: &LedSignals) -> LedState {
    if s.starting {
        return state("starting");
    }

    // `liveBlock` is ABSENT (never null, never {}) whenever live is allowed,
    // and its `reason` is an OPEN SET by contract — compare it as a plain
    // string and ignore values we do not recognise (`led-state.ts:149-150`).
    let block_reason = s.recording.live_block.as_ref().map(|b| b.reason.as_str());

    // Rungs 2 and 4 — power vs thermal. `liveBlock.reason` is the ADJUDICATED
    // verdict and is consulted first, both of them, before `powerShed` is
    // considered at all: a thermal shed sets `powerShed` ("no-live") exactly
    // like a power shed does, because both walk the same ladder
    // (`led-state.ts:152-168`).
    if block_reason == Some("undervoltage") {
        return state("undervoltage");
    }
    if block_reason == Some("thermal") {
        return state("thermal");
    }
    // A shed with no verdict explaining it: power is both the likelier cause
    // and the more urgent one to get wrong in the safe direction (`:170-173`).
    if s.recording.power_shed.is_some() {
        return state("undervoltage");
    }

    // Rung 5 — network. `dead_zone` is a SETTLED verdict, not a fault
    // (boot-policy releases the video gate on it), and it suppresses the
    // cloud-link clause as well (`led-state.ts:175-190`).
    let lte_status = s.lte.as_ref().and_then(|l| l.status.as_deref());
    if lte_status != Some("dead_zone") {
        let lte_trying = matches!(lte_status, Some("disconnected" | "connecting"));
        // `lte: null` means the telemetry file is unavailable, NOT that the
        // modem is down — fail open and let the cloud-link clause speak.
        let cloud_down = s.connection != ConnectionState::Connected;
        if lte_trying || cloud_down {
            return state("network");
        }
    }

    // Rung 6 — waiting on the sky. `gnss: None` means no receiver at all,
    // which is not "no fix yet" (`led-state.ts:192-196`).
    if block_reason == Some("gps-gate") || s.gnss.is_some_and(|g| !g.fix) {
        return state("gps-wait");
    }

    state("healthy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_is_the_single_definition() {
        for r in &LED_LADDER {
            assert_eq!(rung(r.reason), Some(r));
            assert!(!r.label.is_empty());
        }
        assert_eq!(rung("data-loss"), None);
        let json = serde_json::to_string(&LED_LADDER[0]).unwrap();
        assert_eq!(
            json,
            r#"{"pri":"0","reason":"starting","label":"Starting up (no real state yet)","colour":"white","pattern":"solid"}"#
        );
        assert_eq!(
            serde_json::to_string(&state("healthy")).unwrap(),
            r#"{"colour":"green","pattern":"heartbeat","reason":"healthy"}"#
        );
    }

    #[test]
    #[should_panic(expected = "no LED ladder rung for reason 'typo'")]
    fn state_panics_on_a_typo_like_the_ts_throws() {
        let _ = state("typo");
    }
}
