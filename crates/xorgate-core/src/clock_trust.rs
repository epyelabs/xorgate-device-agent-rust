//! Clock trust gate — the PURE half of `src/agent/clock-trust.ts` (JS @
//! 09da38d): a sans-I/O state machine over injected monotonic ms.
//!
//! Step 0 (Phase 5b WP1) declares the types; the machine follows.

use serde::{Deserialize, Serialize};

/// How trust was established (`ClockTrustSource`, `clock-trust.ts:39`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockTrustSource {
    Ntp,
    Gps,
    Nitz,
}

impl ClockTrustSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ClockTrustSource::Ntp => "ntp",
            ClockTrustSource::Gps => "gps",
            ClockTrustSource::Nitz => "nitz",
        }
    }
}

/// One word for the config report hint and the status line
/// (`ClockTrustState`, `clock-trust.ts:42`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockTrustState {
    Trusted,
    Held,
    Fallback,
}

impl ClockTrustState {
    pub fn as_str(self) -> &'static str {
        match self {
            ClockTrustState::Trusted => "trusted",
            ClockTrustState::Held => "held",
            ClockTrustState::Fallback => "fallback",
        }
    }
}

/// `ClockTrustStatus` (`clock-trust.ts:44-55`). Key order = the TS literal
/// of `status()` (`:203-209`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockTrustStatus {
    /// Producers may stamp with the wall clock (synced, or past the grace window).
    pub trusted: bool,
    pub state: ClockTrustState,
    /// How trust was established; `null` while held or in fallback.
    pub source: Option<ClockTrustSource>,
    /// Telemetry publishes withheld so far this process.
    pub held: u64,
    /// Monotonic ms the hold lasted (frozen once trusted).
    pub held_ms: u64,
}
