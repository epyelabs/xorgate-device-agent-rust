//! The telemetry topic in force (`worker.ts:664-687`) and the one place that
//! logs a topic change (`noteScopeChange`, `worker.ts:739-779`). The state
//! machine itself is `xorgate_core::telemetry_scope::TelemetryScope`; this
//! handle shares it between the MQTT plane (connect / close / tick /
//! publish) and the config plane (`applyScope`, WP3).

use std::sync::{Arc, Mutex, MutexGuard};

use tracing::{info, warn};
use xorgate_core::config_report::ReportOrigin;
use xorgate_core::js::number_to_string;
use xorgate_core::round::js_math_round;
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::telemetry_scope::{
    ScopeChange, ScopeReason, TelemetryScope, TelemetryScopeOptions, TelemetryScopeReport,
    TenantScope,
};

use crate::ctx::ReportRequester;
use crate::state::StateHandle;

/// Cloneable handle on the agent's one [`TelemetryScope`] (frozen for wave B).
#[derive(Clone)]
pub struct ScopeHandle {
    inner: Arc<Mutex<TelemetryScope>>,
    state: StateHandle,
    reports: ReportRequester,
}

impl ScopeHandle {
    /// `new TelemetryScope({ deviceId, unscopedTopic: config.telemetryTopic })`
    /// (`worker.ts:684-687`), TS defaults for every threshold.
    pub fn new(
        device_id: &str,
        unscoped_topic: &str,
        state: StateHandle,
        reports: ReportRequester,
    ) -> Self {
        Self::with_options(
            TelemetryScopeOptions {
                unscoped_topic: unscoped_topic.to_string(),
                device_id: device_id.to_string(),
                ..Default::default()
            },
            state,
            reports,
        )
    }

    pub fn with_options(
        opts: TelemetryScopeOptions,
        state: StateHandle,
        reports: ReportRequester,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TelemetryScope::new(opts))),
            state,
            reports,
        }
    }

    fn lock(&self) -> MutexGuard<'_, TelemetryScope> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `telemetryScope.topic` — where the next telemetry publish goes.
    pub fn topic(&self) -> String {
        self.lock().topic()
    }

    /// `telemetryScope.report()` — `effective.telemetryScope` (WP3).
    pub fn report(&self) -> TelemetryScopeReport {
        self.lock().report()
    }

    /// `const change = telemetryScope.applyScope(parsed.scope, now); if
    /// (change) noteScopeChange(change, now);` (`worker.ts:815-818`) — the
    /// config plane calls this for every message carrying `scope`, outside the
    /// rev guard.
    pub fn apply_scope(&self, scope: TenantScope, now: i64) -> Option<ScopeChange> {
        let change = self.lock().apply_scope(scope, now);
        if let Some(c) = &change {
            self.note_scope_change(c, now);
        }
        change
    }

    /// `telemetryScope.noteConnect(now)` (`worker.ts:1157`).
    pub fn note_connect(&self, now: i64) {
        self.lock().note_connect(now);
    }

    /// `telemetryScope.notePublish()` (`worker.ts:1243`).
    pub fn note_publish(&self) {
        self.lock().note_publish();
    }

    /// `noteClose` + `noteScopeChange` (`worker.ts:1278-1281`); the caller
    /// skips it while shutting down.
    pub fn note_close(&self, now: i64) -> Option<ScopeChange> {
        let change = self.lock().note_close(now);
        if let Some(c) = &change {
            self.note_scope_change(c, now);
        }
        change
    }

    /// `noteTick` + `noteScopeChange` (`worker.ts:1233-1234`).
    pub fn note_tick(&self, now: i64) -> Option<ScopeChange> {
        let change = self.lock().note_tick(now);
        if let Some(c) = &change {
            self.note_scope_change(c, now);
        }
        change
    }

    /// `noteScopeChange` (`worker.ts:739-779`), wording verbatim: one log
    /// line, one state.json event, a forced flush, and a `boot` report so the
    /// cloud sees `effective.telemetryScope` move.
    pub fn note_scope_change(&self, change: &ScopeChange, now: i64) {
        let (level, msg) = scope_change_messages(change);
        match level {
            ActivityLevel::Warn => warn!("{}", msg.0),
            _ => info!("{}", msg.0),
        }
        self.state.note(level, msg.1, now);
        self.reports.request(ReportOrigin::Boot);
    }
}

/// The (journal line, state.json event) pair of `noteScopeChange`.
pub fn scope_change_messages(change: &ScopeChange) -> (ActivityLevel, (String, String)) {
    match change {
        ScopeChange::Scoped {
            topic,
            scope,
            reason,
        } => {
            let how = if *reason == ScopeReason::Reprobe {
                " (re-probe after fallback)"
            } else {
                ""
            };
            (
                ActivityLevel::Info,
                (
                    format!("[agent] telemetry topic -> {topic}{how}"),
                    format!("telemetry topic scoped to ws {}{how}", scope.workspace_id),
                ),
            )
        }
        ScopeChange::Fallback {
            topic,
            scope,
            cycles,
            reprobe_in_ms,
        } => {
            let mins = number_to_string(js_math_round(*reprobe_in_ms as f64 / 60_000.0));
            let ws = &scope.workspace_id;
            (
                ActivityLevel::Warn,
                (
                    format!(
                        "[agent] telemetry topic FALLBACK -> {topic}: the broker refused ws {ws} on {cycles} consecutive connections. Publishing on the unscoped topic (still ingested); re-probing in ~{mins} min."
                    ),
                    format!(
                        "telemetry topic fallback to unscoped: ws {ws} refused ({cycles} cycles); re-probe in ~{mins} min"
                    ),
                ),
            )
        }
    }
}
