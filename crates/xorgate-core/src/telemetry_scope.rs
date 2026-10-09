//! Which telemetry topic this device publishes on, and how it gets out of the
//! one failure mode that has ever taken the fleet down. Port of
//! `src/agent/telemetry-scope.ts` (318 lines, fully pure; every method takes
//! an explicit `now` in epoch ms — the worker passes `Date.now()`).
//!
//! The topic: a device boots on the UNSCOPED topic
//! `xorgate/devices/{deviceId}/telemetry` (always permitted by the IoT policy)
//! and moves to the tenant-scoped topic
//! `xorgate/orgs/{org}/ws/{ws}/devices/{deviceId}/telemetry` once the retained
//! config names a tenancy. Both planes are ingested; the unscoped one is a
//! DEGRADED mode (third-party vended credentials only see the scoped topic).
//!
//! The failure mode (2026-09-18, ~50 min of production): the scoped topic is
//! authorised by THING ATTRIBUTES the cloud writes separately from the config
//! `scope`. A running agent pinned to the old topic gets its connection closed
//! on every publish (~3.3 s loop, forever) — the retained config replay races
//! the 1 Hz publish timer and usually loses.
//!
//! The detector: a connection that died in under `short_connection_ms` while
//! the scoped topic was armed AND this agent had published on it at least once
//! since arming (CUMULATIVE — a per-connection publish requirement never
//! reached 30 s unbroken on the bench, 2026-09-19). `failure_threshold` such
//! cycles whose run has lasted `grace_ms` (30 s — Tacloban 03 converged after a
//! 12 s flap) → fall back to the unscoped topic. Ways back up, and no others:
//! a config message carrying a DIFFERENT scope, or the timed re-probe
//! (30 min, doubling to 6 h; after the first fallback ONE bad cycle re-settles).

use serde::{Deserialize, Serialize};

use crate::topics;

/// `TenantScope` (`telemetry-scope.ts:81`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantScope {
    pub organization_id: String,
    pub workspace_id: String,
}

/// `ScopeChange["reason"]` (`telemetry-scope.ts:93`): `cloud` = a config
/// message moved us; `reprobe` = the backoff timer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeReason {
    Cloud,
    Reprobe,
}

/// `ScopeChange` (`telemetry-scope.ts:87`): a topic change the worker should
/// log, publish on, and report. Serialises as the TS union (`kind` tag).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ScopeChange {
    #[serde(rename_all = "camelCase")]
    Scoped {
        topic: String,
        scope: TenantScope,
        reason: ScopeReason,
    },
    #[serde(rename_all = "camelCase")]
    Fallback {
        topic: String,
        /// The scope we could not publish under.
        scope: TenantScope,
        /// Consecutive bad cycles that triggered it.
        cycles: u32,
        /// How long until the next re-probe, ms.
        reprobe_in_ms: i64,
    },
}

/// `TelemetryScopeOptions` (`telemetry-scope.ts:106`). `None` = the TS
/// default (`DEFAULTS`, `:122-128`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryScopeOptions {
    /// `xorgate/devices/{deviceId}/telemetry` — always permitted by the policy.
    pub unscoped_topic: String,
    pub device_id: String,
    /// A connection shorter than this, on an armed scoped topic, is "suspicious".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_connection_ms: Option<i64>,
    /// Consecutive suspicious cycles before the FIRST fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_threshold: Option<u32>,
    /// …and the run must have lasted at least this long.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_ms: Option<i64>,
    /// First re-probe delay after a fallback; doubles per failed probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reprobe_after_ms: Option<i64>,
    /// Ceiling for that doubling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reprobe_ms: Option<i64>,
}

/// `DEFAULTS` (`telemetry-scope.ts:122`).
pub const DEFAULT_SHORT_CONNECTION_MS: i64 = 5_000;
pub const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
pub const DEFAULT_GRACE_MS: i64 = 30_000;
pub const DEFAULT_REPROBE_AFTER_MS: i64 = 30 * 60_000;
pub const DEFAULT_MAX_REPROBE_MS: i64 = 6 * 60 * 60_000;

/// The `fallback` block of [`TelemetryScopeReport`] (`telemetry-scope.ts:139`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackReport {
    /// Epoch ms of the fallback.
    pub since: i64,
    /// Fallbacks for the current desired scope (a re-probe failure adds one).
    pub count: u32,
}

/// `TelemetryScopeReport` (`telemetry-scope.ts:131`): reported-only view of
/// where telemetry is going and why. Key order = the TS `report()` insertion
/// order (`:298-308`); optional keys omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryScopeReport {
    pub topic: String,
    /// True while publishing on the tenant-scoped topic.
    pub scoped: bool,
    /// The tenancy the cloud last named, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Present ONLY while fallen back — its absence is the healthy state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackReport>,
}

/// `scopedTelemetryTopic` (`telemetry-scope.ts:147`). The string itself is
/// defined once, in [`crate::topics::scoped_telemetry_topic`].
pub fn scoped_telemetry_topic(scope: &TenantScope, device_id: &str) -> String {
    topics::scoped_telemetry_topic(&scope.organization_id, &scope.workspace_id, device_id)
}

/// `sameScope` (`telemetry-scope.ts:151`).
pub fn same_scope(a: Option<&TenantScope>, b: &TenantScope) -> bool {
    a.is_some_and(|a| a.organization_id == b.organization_id && a.workspace_id == b.workspace_id)
}

/// The resolved options (`Required<TelemetryScopeOptions>`, `telemetry-scope.ts:158`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    unscoped_topic: String,
    device_id: String,
    short_connection_ms: i64,
    failure_threshold: u32,
    grace_ms: i64,
    reprobe_after_ms: i64,
    max_reprobe_ms: i64,
}

/// `class TelemetryScope` (`telemetry-scope.ts:157`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryScope {
    opts: Resolved,
    /// The tenancy the cloud last told us about; None until the first config message.
    desired: Option<TenantScope>,
    /// True while we are publishing on the scoped topic.
    scoped: bool,
    /// Start of the current measurement cycle: the later of "we connected" and
    /// "we armed the scoped topic". None = not connected.
    cycle_started_at: Option<i64>,
    /// Publishes on the scoped topic since it was armed. Cumulative on purpose.
    publishes_since_arm: u64,
    /// Consecutive suspicious cycles, and when the run began.
    run_cycles: u32,
    run_started_at: i64,
    /// Epoch ms of the current fallback; None = not fallen back.
    fallback_at: Option<i64>,
    /// Fallbacks for the CURRENT desired scope. 0 = never, so use the full threshold.
    fallback_count: u32,
    /// Next re-probe delay, ms. Doubles per failed probe, capped.
    probe_delay_ms: i64,
}

impl TelemetryScope {
    /// `constructor` (`telemetry-scope.ts:186`): `{ ...DEFAULTS, ...opts }`.
    pub fn new(opts: TelemetryScopeOptions) -> Self {
        Self {
            opts: Resolved {
                unscoped_topic: opts.unscoped_topic,
                device_id: opts.device_id,
                short_connection_ms: opts
                    .short_connection_ms
                    .unwrap_or(DEFAULT_SHORT_CONNECTION_MS),
                failure_threshold: opts.failure_threshold.unwrap_or(DEFAULT_FAILURE_THRESHOLD),
                grace_ms: opts.grace_ms.unwrap_or(DEFAULT_GRACE_MS),
                reprobe_after_ms: opts.reprobe_after_ms.unwrap_or(DEFAULT_REPROBE_AFTER_MS),
                max_reprobe_ms: opts.max_reprobe_ms.unwrap_or(DEFAULT_MAX_REPROBE_MS),
            },
            desired: None,
            scoped: false,
            cycle_started_at: None,
            publishes_since_arm: 0,
            run_cycles: 0,
            run_started_at: 0,
            fallback_at: None,
            fallback_count: 0,
            probe_delay_ms: 0,
        }
    }

    /// `get topic` (`telemetry-scope.ts:191`): the topic to publish telemetry on right now.
    pub fn topic(&self) -> String {
        match (&self.desired, self.scoped) {
            (Some(d), true) => scoped_telemetry_topic(d, &self.opts.device_id),
            _ => self.opts.unscoped_topic.clone(),
        }
    }

    /// `get fallenBack` (`telemetry-scope.ts:198`): true while degraded to the
    /// legacy plane despite knowing a tenancy.
    pub fn fallen_back(&self) -> bool {
        self.fallback_at.is_some()
    }

    /// `applyScope` (`telemetry-scope.ts:207`): a config message carried a
    /// `scope`. None when nothing moved — the common case, because the message
    /// is RETAINED and replayed on every connect; and if we are fallen back on
    /// this very scope, STAY (re-arming here is the flap this class avoids).
    pub fn apply_scope(&mut self, scope: TenantScope, now: i64) -> Option<ScopeChange> {
        if same_scope(self.desired.as_ref(), &scope) {
            return None;
        }
        self.desired = Some(scope.clone());
        self.fallback_at = None;
        self.fallback_count = 0;
        self.probe_delay_ms = 0;
        self.arm_scoped(now);
        Some(ScopeChange::Scoped {
            topic: self.topic(),
            scope,
            reason: ScopeReason::Cloud,
        })
    }

    /// `noteConnect` (`telemetry-scope.ts:222`): MQTT connected.
    pub fn note_connect(&mut self, now: i64) {
        self.cycle_started_at = Some(now);
    }

    /// `notePublish` (`telemetry-scope.ts:227`): a telemetry publish left the
    /// client on the current topic.
    pub fn note_publish(&mut self) {
        if self.scoped {
            self.publishes_since_arm += 1;
        }
    }

    /// `noteClose` (`telemetry-scope.ts:236`): MQTT connection closed. Returns
    /// a fallback change when this closed the run that proves the scoped topic
    /// is refused. Must NOT be called for a shutdown close.
    pub fn note_close(&mut self, now: i64) -> Option<ScopeChange> {
        let started_at = self.cycle_started_at.take();

        // A cycle says nothing about the scoped topic unless we were using it
        // and have offered it to the broker at least once.
        if !self.scoped || self.desired.is_none() || self.publishes_since_arm == 0 {
            self.run_cycles = 0;
            return None;
        }
        // A close with no connection behind it — a handshake that never
        // completed — is not evidence either way: neither counts nor clears.
        let started_at = started_at?;
        if now - started_at >= self.opts.short_connection_ms {
            // Long enough to be a working connection. Whatever we were
            // watching, it is over.
            self.run_cycles = 0;
            return None;
        }
        if self.run_cycles == 0 {
            self.run_started_at = now;
        }
        self.run_cycles += 1;

        // After the first fallback for this scope one bad cycle is enough.
        let need_cycles = if self.fallback_count > 0 {
            1
        } else {
            self.opts.failure_threshold
        };
        let need_grace_ms = if self.fallback_count > 0 {
            0
        } else {
            self.opts.grace_ms
        };
        if self.run_cycles < need_cycles {
            return None;
        }
        if now - self.run_started_at < need_grace_ms {
            return None;
        }

        let cycles = self.run_cycles;
        let scope = self.desired.clone()?;
        self.scoped = false;
        self.run_cycles = 0;
        self.fallback_at = Some(now);
        self.fallback_count += 1;
        self.probe_delay_ms = if self.probe_delay_ms == 0 {
            self.opts.reprobe_after_ms
        } else {
            (self.probe_delay_ms * 2).min(self.opts.max_reprobe_ms)
        };
        Some(ScopeChange::Fallback {
            topic: self.topic(),
            scope,
            cycles,
            reprobe_in_ms: self.probe_delay_ms,
        })
    }

    /// `noteTick` (`telemetry-scope.ts:289`): called on the publish tick.
    /// Returns a change when the backoff has elapsed and it is time to try the
    /// scoped topic again.
    pub fn note_tick(&mut self, now: i64) -> Option<ScopeChange> {
        let fallback_at = self.fallback_at?;
        let desired = self.desired.clone()?;
        if now - fallback_at < self.probe_delay_ms {
            return None;
        }
        self.fallback_at = None;
        self.arm_scoped(now);
        Some(ScopeChange::Scoped {
            topic: self.topic(),
            scope: desired,
            reason: ScopeReason::Reprobe,
        })
    }

    /// `report` (`telemetry-scope.ts:298`): reported-only block for the config
    /// report and the local state file.
    pub fn report(&self) -> TelemetryScopeReport {
        TelemetryScopeReport {
            topic: self.topic(),
            scoped: self.scoped,
            organization_id: self.desired.as_ref().map(|d| d.organization_id.clone()),
            workspace_id: self.desired.as_ref().map(|d| d.workspace_id.clone()),
            fallback: self.fallback_at.map(|since| FallbackReport {
                since,
                count: self.fallback_count,
            }),
        }
    }

    /// `armScoped` (`telemetry-scope.ts:311`): arm the scoped topic and restart
    /// the measurement cycle from here.
    fn arm_scoped(&mut self, now: i64) {
        self.scoped = true;
        self.run_cycles = 0;
        self.publishes_since_arm = 0;
        // Only meaningful while connected; a connect will set it otherwise.
        if self.cycle_started_at.is_some() {
            self.cycle_started_at = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make() -> TelemetryScope {
        TelemetryScope::new(TelemetryScopeOptions {
            unscoped_topic: "xorgate/devices/d/telemetry".into(),
            device_id: "d".into(),
            ..Default::default()
        })
    }

    fn ws(w: &str) -> TenantScope {
        TenantScope {
            organization_id: "org-1".into(),
            workspace_id: w.into(),
        }
    }

    #[test]
    fn serde_shapes_match_the_ts_union() {
        let c = ScopeChange::Scoped {
            topic: "t".into(),
            scope: ws("ws-a"),
            reason: ScopeReason::Cloud,
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"kind":"scoped","topic":"t","scope":{"organizationId":"org-1","workspaceId":"ws-a"},"reason":"cloud"}"#
        );
        let f = ScopeChange::Fallback {
            topic: "u".into(),
            scope: ws("ws-a"),
            cycles: 3,
            reprobe_in_ms: 1_800_000,
        };
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#"{"kind":"fallback","topic":"u","scope":{"organizationId":"org-1","workspaceId":"ws-a"},"cycles":3,"reprobeInMs":1800000}"#
        );
        let r = make().report();
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"topic":"xorgate/devices/d/telemetry","scoped":false}"#
        );
    }

    #[test]
    fn defaults_and_same_scope() {
        let s = make();
        assert_eq!(s.opts.short_connection_ms, 5_000);
        assert_eq!(s.opts.failure_threshold, 3);
        assert_eq!(s.opts.grace_ms, 30_000);
        assert_eq!(s.opts.reprobe_after_ms, 1_800_000);
        assert_eq!(s.opts.max_reprobe_ms, 21_600_000);
        assert!(same_scope(Some(&ws("a")), &ws("a")));
        assert!(!same_scope(Some(&ws("a")), &ws("b")));
        assert!(!same_scope(None, &ws("a")));
        assert_eq!(
            scoped_telemetry_topic(&ws("w"), "d"),
            "xorgate/orgs/org-1/ws/w/devices/d/telemetry"
        );
    }
}
