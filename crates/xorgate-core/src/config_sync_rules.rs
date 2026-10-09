//! The pure decisions inside `class ConfigSync` (`src/agent/config-sync.ts:1211-1651`)
//! — the reported config plane's runtime — lifted out so the Phase 2 agent
//! (`xorgate-agent::config_plane::sync`) keeps only timers and I/O (Phase 2
//! WP3, CF11). Every function names the TS lines it mirrors; the expected
//! values of `fixtures/config_sync_rules/rules.json` were OBSERVED on the real
//! class (tsx driver `phase-2/evidence/wp3-tools/dump-config-sync.mts`).
//!
//! | TS | here |
//! |---|---|
//! | `DEFAULT_*` / `MODEM_ARRIVAL_TRIES` (`:1151-1198`) | the `*_MS` / count constants |
//! | `request` rank + `boot`→`local` upgrade (`:1435-1440`) | [`next_origin`] |
//! | `scheduleSettleReport` predicate + re-request origin (`:1638-1650`) | [`needs_settle_report`], [`settle_origin`] |
//! | `armModemArrivalWatch` trigger (`:1610`), `watchForModemArrival` (`:1622-1628`) | [`modem_arrival_trigger`], [`modem_seen`] |
//! | `JSON.stringify(sample.apn)` / `(report.effective.apn)` (`:1497`, `:1565`) | [`apn_fingerprint`], [`effective_apn_fingerprint`] |
//! | the local diff + empty-diff downgrade (`:1522-1534`) | [`resolve_report_origin`] |
//! | the persist-after-publish patch (`:1553-1560`) | [`persist_after_publish`] |
//! | `checkLocalChange` (`:1383-1426`) | [`check_local_change`] |
//! | `noteSettingsWritten` refusal + re-baseline (`:1335-1353`) | [`note_settings_written`] |
//! | the `onEvent` strings | the `*_event` helpers (verbatim; state.json + journal) |

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::config_namespaces::{apply_class_of, ApplyClass};
use crate::config_report::{
    diff_namespaces, namespaces_of, rebaseline_after_write, ApnBlock, ConfigSyncState,
    ModemBiasSample, NamespaceState, NamespaceStatus, ReportOrigin, SyncPatch,
};
use crate::settings::{AgentSettings, SettingsReadFailure, SettingsReadResult};

/// `DEFAULT_DEBOUNCE_MS` (`config-sync.ts:1151`): a connect and the retained
/// config message ~0.5 s later are one report.
pub const DEFAULT_DEBOUNCE_MS: u64 = 2000;
/// `DEFAULT_HEARTBEAT_MS` (`:1152`): the hourly `boot` repair backstop.
pub const DEFAULT_HEARTBEAT_MS: u64 = 3_600_000;
/// `DEFAULT_WATCH_MS` (`:1160`): the settings.json fingerprint poll.
pub const DEFAULT_WATCH_MS: u64 = 5000;
/// `DEFAULT_SETTLE_MS` (`:1179`): re-report a saved-but-not-in-force
/// `modem-poll` namespace past the daemon's ~30 s poll.
pub const DEFAULT_SETTLE_MS: u64 = 40_000;
/// `DEFAULT_SETTLE_REPORTS` (`:1180`): settle re-reports per desired rev.
pub const DEFAULT_SETTLE_REPORTS: u32 = 3;
/// `DEFAULT_MODEM_ARRIVAL_MS` (`:1188`): boot-race watch cadence.
pub const DEFAULT_MODEM_ARRIVAL_MS: u64 = 30_000;
/// `MODEM_ARRIVAL_TRIES` (`:1189`): 30 s × 20 = a 10-minute window, then give up.
pub const MODEM_ARRIVAL_TRIES: u32 = 20;
/// `DEFAULT_APN_WATCH_MS` (`:1198`): the standing APN-machinery watch.
pub const DEFAULT_APN_WATCH_MS: u64 = 30_000;

/// `request(origin)`'s coalescing (`config-sync.ts:1437-1440`): a pending
/// local change upgrades `boot` to `local` (the connect assertion and the
/// heartbeat double as the local-report retry path); the queued origin is the
/// highest-ranked of the window (`boot < cloud < local`).
pub fn next_origin(
    pending: Option<ReportOrigin>,
    incoming: ReportOrigin,
    local_pending: bool,
) -> ReportOrigin {
    let incoming = if incoming == ReportOrigin::Boot && local_pending {
        ReportOrigin::Local
    } else {
        incoming
    };
    match pending {
        Some(p) if incoming.rank() <= p.rank() => p,
        _ => incoming,
    }
}

/// The `unsettled` test of `scheduleSettleReport` (`config-sync.ts:1640-1642`):
/// some namespace is `adjusted` AND applied by the modem daemon's own poll.
pub fn needs_settle_report(status: &IndexMap<String, NamespaceStatus>) -> bool {
    status.iter().any(|(ns, s)| {
        s.state == NamespaceState::Adjusted && apply_class_of(ns) == Some(ApplyClass::ModemPoll)
    })
}

/// The settle timer's request (`config-sync.ts:1646`):
/// `request(report.origin === "local" ? "local" : "boot")`.
pub fn settle_origin(report_origin: ReportOrigin) -> ReportOrigin {
    if report_origin == ReportOrigin::Local {
        ReportOrigin::Local
    } else {
        ReportOrigin::Boot
    }
}

/// The boot-race probe's trigger (`config-sync.ts:1610`): the modem daemon
/// has spoken once the sample carries `sim` or `uplink` (NOT `apn`, NOT the bias).
pub fn modem_arrival_trigger(sample: &ModemBiasSample) -> bool {
    sample.sim.is_some() || sample.uplink.is_some()
}

/// `watchForModemArrival` (`config-sync.ts:1622-1628`): the published
/// report's `effective` already carries `sim` / `uplink` → latch, no watch.
pub fn modem_seen(effective: &Map<String, Value>) -> bool {
    effective.contains_key("sim") || effective.contains_key("uplink")
}

/// `sample.apn === undefined ? undefined : JSON.stringify(sample.apn)`
/// (`config-sync.ts:1497`): the APN watch's fingerprint of a fresh probe.
pub fn apn_fingerprint(apn: Option<&ApnBlock>) -> Option<String> {
    apn.map(|a| crate::js::to_json_string(&serde_json::to_value(a).expect("ApnBlock serialises")))
}

/// `apnBlock === undefined ? undefined : JSON.stringify(apnBlock)` over the
/// published report's `effective.apn` (`config-sync.ts:1564-1565`).
pub fn effective_apn_fingerprint(effective: &Map<String, Value>) -> Option<String> {
    effective.get("apn").map(crate::js::to_json_string)
}

/// What `publishReport` decided about the origin before building the report
/// (`config-sync.ts:1518-1535`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportOriginResolution {
    /// The origin the report goes out with.
    pub origin: ReportOrigin,
    /// `changed` for a `local` report (non-empty by construction).
    pub changed: Option<Map<String, Value>>,
    /// `namespacesOf(settings)` — what the device holds now.
    pub current: Map<String, Value>,
    /// The `persist({ localPending: false })` of the downgrade branch.
    pub clear_local_pending: bool,
}

/// The local diff of `publishReport` (`config-sync.ts:1520-1535`), computed
/// at PUBLISH time so several edits collapse into one statement; an empty
/// diff (the edit was reverted, or a cloud apply overwrote it) downgrades the
/// report to a plain `boot` assertion and clears `localPending`.
pub fn resolve_report_origin(
    origin: ReportOrigin,
    baseline: Option<&Map<String, Value>>,
    settings: &AgentSettings,
) -> ReportOriginResolution {
    let current = namespaces_of(settings);
    if origin != ReportOrigin::Local {
        return ReportOriginResolution {
            origin,
            changed: None,
            current,
            clear_local_pending: false,
        };
    }
    let changed = diff_namespaces(baseline.unwrap_or(&current), &current);
    if changed.is_empty() {
        return ReportOriginResolution {
            origin: ReportOrigin::Boot,
            changed: None,
            current,
            clear_local_pending: true,
        };
    }
    ReportOriginResolution {
        origin,
        changed: Some(changed),
        current,
        clear_local_pending: false,
    }
}

/// The persist after a publish (`config-sync.ts:1553-1560`), in the patch
/// literal's key order: `{ lastReportTs: now }`, plus — only for a `local`
/// report that actually left the device (`online`) — `namespaces: current,
/// localPending: false`. Published into a dead link, the change stays pending.
pub fn persist_after_publish(
    origin: ReportOrigin,
    online: bool,
    current: &Map<String, Value>,
    now: i64,
) -> Vec<SyncPatch> {
    let mut patch = vec![SyncPatch::LastReportTs(now)];
    if origin == ReportOrigin::Local && online {
        patch.push(SyncPatch::Namespaces(current.clone()));
        patch.push(SyncPatch::LocalPending(false));
    }
    patch
}

/// The `onEvent` warning of an unreadable / invalid settings.json
/// (`config-sync.ts:1398-1401`), verbatim.
pub fn settings_paused_event(reason: SettingsReadFailure, settings_file: &str) -> String {
    format!(
        "settings.json is {} — local-change sync paused (check owner/permissions on {settings_file})",
        reason.as_str()
    )
}

/// `local settings change detected: ${keys.join(", ")}` (`config-sync.ts:1423`).
pub fn local_change_event(keys: &[&str]) -> String {
    format!("local settings change detected: {}", keys.join(", "))
}

/// `config report failed: ${err.message}` (`config-sync.ts:1568`).
pub fn report_failed_event(message: &str) -> String {
    format!("config report failed: {message}")
}

/// `checkApnChange`'s event (`config-sync.ts:1500`).
pub const APN_CHANGED_EVENT: &str = "modem APN state changed — re-reporting";
/// The boot-race watch's event (`config-sync.ts:1613-1614`).
pub const MODEM_ARRIVED_EVENT: &str =
    "modem telemetry arrived after the boot report — re-reporting";
/// `noteCloudConfig`'s event when the rev could not be persisted (`:1314`).
pub const SYNC_STATE_NOT_PERSISTED_EVENT: &str = "config sync state could not be persisted";

/// One `checkLocalChange` tick's verdict (`config-sync.ts:1383-1426`), after
/// the caller hashed the file and (when the hash moved) read it.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalCheck {
    /// Same hash as last time: nobody wrote the file. Nothing to do.
    Unchanged,
    /// The file exists but cannot be read / parsed: skip the tick entirely —
    /// no baseline advance, no report. `warn` is the one-shot event (None
    /// while the latch is already set).
    Paused { warn: Option<String> },
    /// Persist `patch` (never baselined → adopt silently; or a rewrite that
    /// moved no synced namespace → hash only). No report.
    Persist { patch: Vec<SyncPatch> },
    /// A real local change: persist `patch` (`settingsHash`, `localPending:
    /// true`), emit `event`, then `request("local")`.
    Changed {
        patch: Vec<SyncPatch>,
        event: String,
    },
}

/// `checkLocalChange` (`config-sync.ts:1383-1426`) over a performed hash and
/// read. `read` is consulted only when the hash moved (the caller may pass
/// a lazily-read result; it is ignored on [`LocalCheck::Unchanged`]).
/// `unreadable_latched` is the class's `unreadableSettings` flag; the caller
/// sets it when this returns `Paused` and clears it on any other non-`Unchanged`
/// verdict.
pub fn check_local_change(
    state: &ConfigSyncState,
    hash: &str,
    read: &SettingsReadResult,
    unreadable_latched: bool,
    settings_file: &str,
) -> LocalCheck {
    if state.settings_hash.as_deref() == Some(hash) {
        return LocalCheck::Unchanged;
    }
    let settings = match read {
        Ok(s) => s.clone(),
        Err(SettingsReadFailure::Absent) => AgentSettings::default(),
        Err(reason) => {
            return LocalCheck::Paused {
                warn: (!unreadable_latched).then(|| settings_paused_event(*reason, settings_file)),
            }
        }
    };
    let current = namespaces_of(&settings);
    let Some(baseline) = state.namespaces.as_ref() else {
        // Never baselined: adopt the file silently (`ensureConfigSyncBaseline`).
        return LocalCheck::Persist {
            patch: vec![
                SyncPatch::Namespaces(current),
                SyncPatch::SettingsHash(hash.to_string()),
            ],
        };
    };
    let changed = diff_namespaces(baseline, &current);
    if changed.is_empty() {
        return LocalCheck::Persist {
            patch: vec![SyncPatch::SettingsHash(hash.to_string())],
        };
    }
    let keys: Vec<&str> = changed.keys().map(String::as_str).collect();
    LocalCheck::Changed {
        event: local_change_event(&keys),
        patch: vec![
            SyncPatch::SettingsHash(hash.to_string()),
            SyncPatch::LocalPending(true),
        ],
    }
}

/// `noteSettingsWritten(touched)` (`config-sync.ts:1335-1353`): `None` when
/// the file is unreadable / invalid (never re-baseline off a failed read);
/// otherwise the `persist({ namespaces, settingsHash })` patch.
pub fn note_settings_written(
    state: &ConfigSyncState,
    read: &SettingsReadResult,
    touched: &[&str],
    hash: &str,
) -> Option<Vec<SyncPatch>> {
    let settings = match read {
        Ok(s) => s.clone(),
        Err(SettingsReadFailure::Absent) => AgentSettings::default(),
        Err(_) => return None,
    };
    let current = namespaces_of(&settings);
    let base = rebaseline_after_write(state.namespaces.as_ref(), &current, touched);
    Some(vec![
        SyncPatch::Namespaces(base),
        SyncPatch::SettingsHash(hash.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescing_is_rank_max_with_the_local_upgrade() {
        use ReportOrigin::*;
        assert_eq!(next_origin(None, Boot, false), Boot);
        assert_eq!(next_origin(None, Boot, true), Local);
        assert_eq!(next_origin(Some(Cloud), Boot, false), Cloud);
        assert_eq!(next_origin(Some(Cloud), Boot, true), Local);
        assert_eq!(next_origin(Some(Local), Cloud, false), Local);
    }

    #[test]
    fn settle_origin_keeps_local_only() {
        assert_eq!(settle_origin(ReportOrigin::Local), ReportOrigin::Local);
        assert_eq!(settle_origin(ReportOrigin::Cloud), ReportOrigin::Boot);
        assert_eq!(settle_origin(ReportOrigin::Boot), ReportOrigin::Boot);
    }

    #[test]
    fn paused_event_is_verbatim() {
        assert_eq!(
            settings_paused_event(SettingsReadFailure::Unreadable, "/h/settings.json"),
            "settings.json is unreadable — local-change sync paused (check owner/permissions on /h/settings.json)"
        );
    }
}
