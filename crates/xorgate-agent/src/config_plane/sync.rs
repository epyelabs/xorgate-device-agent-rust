//! `class ConfigSync` (`src/agent/config-sync.ts:1211-1651`): the device's
//! half of the config plane — the rev guard, `config-sync.json`, the
//! debounced report publisher, the hourly heartbeat, the settings.json
//! fingerprint watcher, the settle / modem-arrival / APN watches.
//!
//! A literal port of the class's runtime model: one shared state behind a
//! mutex (never held across an `.await`), and every `setTimeout` /
//! `setInterval` a spawned tokio task whose abort handle sits where the TS
//! keeps its `NodeJS.Timeout` (`clearTimeout` = `abort`). A timer clears its
//! own slot when it fires (`this.timer = null`), so `stop()` never cancels a
//! callback that is already running — exactly the Node semantics. The pure
//! decisions live in `xorgate_core::config_sync_rules`; the state merge is
//! ALWAYS `ConfigSyncState::merge` (CF13).
//!
//! Every public method is total: nothing here returns an error into a caller
//! (design.md §7 — a broken report path must degrade to "no report yet").
//!
//! Deliberate differences (named in `phase-2/evidence/wp3.md`):
//! - `publish` returns [`PublishFailure`]: `Failed(msg)` is the TS "publish
//!   threw" path (`config report failed: …`, nothing persisted);
//!   `NotConnected` is the TS "published into a dead link" path (mqtt.js would
//!   have queued it; the Rust client drops it — WP1 ambiguity #8): the report
//!   is treated as not delivered, so a `local` change is NOT retired even if
//!   `online()` raced to true, and the connect handler's `boot` (upgraded to
//!   `local`) re-sends it.
//! - `publishReport` re-checks `stopped` after the modem read, so no report is
//!   published once `stop()` returned (the frozen `ConfigPlane::stop` contract;
//!   the TS checks only on entry).
//! - The modem reader is fallible only in tests (`modem` option); the real one
//!   (`read_modem_bias`) never fails, as `readModemBias` never rejects.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::task::AbortHandle;
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use xorgate_core::boot_config::CameraMountState;
use xorgate_core::build_info::BuildInfo;
use xorgate_core::cloud_config::CloudConfig;
use xorgate_core::config_report::{
    build_config_report, modem_bias_from_telemetry, ConfigReport, ConfigSyncState, DesiredSnapshot,
    ModemBiasSample, ReportHints, ReportInput, ReportOrigin, SyncPatch,
};
use xorgate_core::config_sync_rules as rules;
use xorgate_core::settings::{AgentSettings, SettingsReadResult};
use xorgate_core::telemetry::ActivityLevel;
use xorgate_core::telemetry_scope::TelemetryScopeReport;
use xorgate_hal::config_sync_file::{load_config_sync_state, save_config_sync_state};
use xorgate_hal::io::TapeSink;
use xorgate_hal::settings_io::{read_settings_file, sha256_file};

use crate::ctx::BoxFut;
use crate::mqtt_plane::PublishFailure;

/// `publish: (topic, payload) => client.publish(topic, payload, { qos: 1, retain: false })`.
pub type PublishFn = Arc<dyn Fn(&str, Vec<u8>) -> Result<(), PublishFailure> + Send + Sync>;
/// `readSettings` (default `readSettingsFile(settingsFile)`).
pub type ReadSettingsFn = Arc<dyn Fn() -> SettingsReadResult + Send + Sync>;
/// `modem: (now) => Promise<ModemBiasSample>` (default `readModemBias`); `Err`
/// = the promise rejected (test fakes only).
pub type ModemFn =
    Arc<dyn Fn(i64) -> BoxFut<'static, Result<ModemBiasSample, String>> + Send + Sync>;
/// `onEvent: (level, msg) => void`.
pub type EventFn = Arc<dyn Fn(ActivityLevel, &str) + Send + Sync>;

/// `ConfigSyncOptions` (`config-sync.ts:1093-1149`). The TS `settings` option
/// (a reader that cannot fail) is `read_settings` returning `Ok`.
#[derive(Clone)]
pub struct ConfigSyncOptions {
    pub device_id: String,
    /// `xorgate/devices/{deviceId}/config/reported`.
    pub topic: String,
    pub publish: PublishFn,
    pub read_settings: Option<ReadSettingsFn>,
    pub modem: ModemFn,
    pub hints: Option<Arc<dyn Fn() -> ReportHints + Send + Sync>>,
    pub camera_mount: Option<Arc<dyn Fn() -> Option<CameraMountState> + Send + Sync>>,
    pub telemetry_scope: Option<Arc<dyn Fn() -> Option<TelemetryScopeReport> + Send + Sync>>,
    pub on_event: Option<EventFn>,
    /// `Date.now()` (report `ts`, `lastReportTs`).
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// `config-sync.json`.
    pub state_path: PathBuf,
    /// settings.json — watched for writes this module did not make.
    pub settings_file: PathBuf,
    /// The run mode in force (`effective.agent.mode`, reported-only).
    pub mode: Option<String>,
    /// Is the MQTT link up? Absent = assume connected.
    pub online: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// `loadBuildInfo()` (`effective.agent.build`).
    pub build: BuildInfo,
    pub watch_ms: u64,
    pub debounce_ms: u64,
    pub heartbeat_ms: u64,
    pub settle_ms: u64,
    pub settle_reports: u32,
    pub modem_arrival_ms: u64,
    pub apn_watch_ms: u64,
}

impl ConfigSyncOptions {
    /// The required options + the TS defaults for every cadence.
    pub fn new(
        device_id: impl Into<String>,
        topic: impl Into<String>,
        publish: PublishFn,
        modem: ModemFn,
        state_path: PathBuf,
        settings_file: PathBuf,
        build: BuildInfo,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            topic: topic.into(),
            publish,
            read_settings: None,
            modem,
            hints: None,
            camera_mount: None,
            telemetry_scope: None,
            on_event: None,
            now: Arc::new(crate::now_ms),
            state_path,
            settings_file,
            mode: None,
            online: None,
            build,
            watch_ms: rules::DEFAULT_WATCH_MS,
            debounce_ms: rules::DEFAULT_DEBOUNCE_MS,
            heartbeat_ms: rules::DEFAULT_HEARTBEAT_MS,
            settle_ms: rules::DEFAULT_SETTLE_MS,
            settle_reports: rules::DEFAULT_SETTLE_REPORTS,
            modem_arrival_ms: rules::DEFAULT_MODEM_ARRIVAL_MS,
            apn_watch_ms: rules::DEFAULT_APN_WATCH_MS,
        }
    }
}

/// `readModemBias(now, file)` (`config-sync.ts:505-538`): one read of the
/// daemon's telemetry file (reported to the tape, CF16) and the core
/// decision over the parsed value. Never fails.
pub async fn read_modem_bias(path: &Path, tape: &dyn TapeSink, now: i64) -> ModemBiasSample {
    let snap = xorgate_hal::modem_file::read(path, tape).await;
    modem_bias_from_telemetry(snap.raw.as_ref(), now)
}

/// The class's private fields (`config-sync.ts:1212-1236`).
#[derive(Default)]
struct Core {
    state: ConfigSyncState,
    desired: Option<DesiredSnapshot>,
    timer: Option<AbortHandle>,
    heartbeat: Option<AbortHandle>,
    settle_timer: Option<AbortHandle>,
    modem_arrival_timer: Option<AbortHandle>,
    modem_arrival_tries: u32,
    modem_seen: bool,
    apn_watch_timer: Option<AbortHandle>,
    last_reported_apn: Option<String>,
    watcher: Option<AbortHandle>,
    settle_budget: u32,
    pending_origin: Option<ReportOrigin>,
    stopped: bool,
    unreadable_settings: bool,
}

struct Inner {
    opts: ConfigSyncOptions,
    core: Mutex<Core>,
}

/// `ConfigSync` — a cheap cloneable handle.
#[derive(Clone)]
pub struct ConfigSync {
    inner: Arc<Inner>,
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

impl ConfigSync {
    /// `new ConfigSync(opts)` (`config-sync.ts:1238-1241`): loads the sync state.
    pub fn new(opts: ConfigSyncOptions) -> Self {
        let state = load_config_sync_state(&opts.state_path);
        Self {
            inner: Arc::new(Inner {
                opts,
                core: Mutex::new(Core {
                    state,
                    ..Default::default()
                }),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Core> {
        self.inner.core.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn opts(&self) -> &ConfigSyncOptions {
        &self.inner.opts
    }

    fn event(&self, level: ActivityLevel, msg: &str) {
        if let Some(f) = &self.opts().on_event {
            f(level, msg);
        }
    }

    /// `get lastAppliedRev()` (`:1243`).
    pub fn last_applied_rev(&self) -> u64 {
        self.lock().state.last_applied_rev
    }

    /// `get syncState()` (`:1248`): a copy of the persisted bookkeeping.
    pub fn sync_state(&self) -> ConfigSyncState {
        self.lock().state.clone()
    }

    /// `readSettings()` (`:1271-1275`).
    fn read_settings(&self) -> SettingsReadResult {
        match &self.opts().read_settings {
            Some(f) => f(),
            None => read_settings_file(&self.opts().settings_file),
        }
    }

    /// `persist(patch)` (`:1277-1280`): merge (CF13) + save; never fails.
    fn persist(&self, core: &mut Core, patch: Vec<SyncPatch>) {
        core.state.merge(patch);
        save_config_sync_state(&core.state, &self.opts().state_path);
    }

    /// `shouldApply(rev)` (`:1283-1285`): the rev guard.
    pub fn should_apply(&self, rev: Option<u64>) -> bool {
        xorgate_core::config_report::should_apply_rev(rev, self.lock().state.last_applied_rev)
    }

    /// `noteCloudConfig(rev, raw, parsed, applied)` (`:1293-1318`): remember
    /// the desired doc (for a SKIPPED one too), refresh the settle budget, and
    /// for an applied one re-baseline the namespaces it carried and advance +
    /// persist the rev.
    pub fn note_cloud_config(
        &self,
        rev: Option<u64>,
        raw: Map<String, Value>,
        parsed: CloudConfig,
        applied: bool,
    ) {
        let touched: Vec<String> = raw.keys().cloned().collect();
        let mut core = self.lock();
        core.desired = Some(DesiredSnapshot {
            rev: rev.unwrap_or(0),
            raw,
            parsed,
        });
        core.settle_budget = self.opts().settle_reports;
        if !applied {
            return;
        }
        let touched: Vec<&str> = touched.iter().map(String::as_str).collect();
        self.note_settings_written_locked(&mut core, &touched);
        if let Some(r) = rev {
            if r > core.state.last_applied_rev {
                core.state.merge([SyncPatch::LastAppliedRev(r)]);
                if !save_config_sync_state(&core.state, &self.opts().state_path) {
                    drop(core);
                    self.event(ActivityLevel::Warn, rules::SYNC_STATE_NOT_PERSISTED_EVENT);
                }
            }
        }
    }

    /// `noteSettingsWritten(touched)` (`:1335-1353`).
    pub fn note_settings_written(&self, touched: &[&str]) {
        let mut core = self.lock();
        self.note_settings_written_locked(&mut core, touched);
    }

    fn note_settings_written_locked(&self, core: &mut Core, touched: &[&str]) {
        let read = self.read_settings();
        let hash = sha256_file(&self.opts().settings_file);
        if let Some(patch) = rules::note_settings_written(&core.state, &read, touched, &hash) {
            self.persist(core, patch);
        }
    }

    /// `startWatcher()` (`:1364-1372`): check once now, then every `watchMs`.
    pub fn start_watcher(&self) {
        {
            let core = self.lock();
            if core.watcher.is_some() || core.stopped {
                return;
            }
        }
        self.check_local_change();
        let me = self.clone();
        let period = ms(self.opts().watch_ms);
        let h = tokio::spawn(async move {
            let mut iv = interval_at(Instant::now() + period, period);
            iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                me.check_local_change();
            }
        });
        let mut core = self.lock();
        if core.stopped {
            h.abort();
        } else {
            core.watcher = Some(h.abort_handle());
        }
    }

    /// `checkLocalChange()` (`:1383-1426`): one fingerprint-watcher tick.
    pub fn check_local_change(&self) {
        let mut core = self.lock();
        if core.stopped {
            return;
        }
        let hash = sha256_file(&self.opts().settings_file);
        if core.state.settings_hash.as_deref() == Some(hash.as_str()) {
            return;
        }
        let read = self.read_settings();
        let file = self.opts().settings_file.display().to_string();
        match rules::check_local_change(&core.state, &hash, &read, core.unreadable_settings, &file)
        {
            rules::LocalCheck::Unchanged => {}
            rules::LocalCheck::Paused { warn } => {
                core.unreadable_settings = true;
                drop(core);
                if let Some(w) = warn {
                    self.event(ActivityLevel::Warn, &w);
                }
            }
            rules::LocalCheck::Persist { patch } => {
                core.unreadable_settings = false;
                self.persist(&mut core, patch);
            }
            rules::LocalCheck::Changed { patch, event } => {
                core.unreadable_settings = false;
                self.persist(&mut core, patch);
                drop(core);
                self.event(ActivityLevel::Info, &event);
                self.request(ReportOrigin::Local);
            }
        }
    }

    /// `request(origin)` (`:1435-1449`): queue a report, coalesced with
    /// anything already queued in the debounce window.
    pub fn request(&self, origin: ReportOrigin) {
        let mut core = self.lock();
        self.request_locked(&mut core, origin);
    }

    fn request_locked(&self, core: &mut Core, origin: ReportOrigin) {
        if core.stopped {
            return;
        }
        core.pending_origin = Some(rules::next_origin(
            core.pending_origin,
            origin,
            core.state.local_pending == Some(true),
        ));
        if core.timer.is_some() {
            return;
        }
        let me = self.clone();
        let delay = ms(self.opts().debounce_ms);
        let h = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let next = {
                let mut core = me.lock();
                core.timer = None;
                core.pending_origin.take().unwrap_or(ReportOrigin::Boot)
            };
            me.publish_report(next).await;
        });
        core.timer = Some(h.abort_handle());
    }

    /// `startHeartbeat()` (`:1452-1459`): the hourly `boot` repair backstop. Idempotent.
    pub fn start_heartbeat(&self) {
        let mut core = self.lock();
        if core.heartbeat.is_some() || core.stopped {
            return;
        }
        let me = self.clone();
        let period = ms(self.opts().heartbeat_ms);
        let h = tokio::spawn(async move {
            let mut iv = interval_at(Instant::now() + period, period);
            iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                me.request(ReportOrigin::Boot);
            }
        });
        core.heartbeat = Some(h.abort_handle());
    }

    /// `stop()` (`:1461-1475`): no timer fires and no request is queued after this.
    pub fn stop(&self) {
        let mut guard = self.lock();
        let core = &mut *guard;
        core.stopped = true;
        for slot in [
            &mut core.timer,
            &mut core.heartbeat,
            &mut core.settle_timer,
            &mut core.modem_arrival_timer,
            &mut core.apn_watch_timer,
            &mut core.watcher,
        ] {
            if let Some(h) = slot.take() {
                h.abort();
            }
        }
    }

    /// `startApnWatch()` (`:1486-1492`): the standing APN-machinery watch.
    pub fn start_apn_watch(&self) {
        let mut core = self.lock();
        if core.apn_watch_timer.is_some() || core.stopped {
            return;
        }
        let me = self.clone();
        let period = ms(self.opts().apn_watch_ms);
        let h = tokio::spawn(async move {
            let mut iv = interval_at(Instant::now() + period, period);
            iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                me.check_apn_change().await;
            }
        });
        core.apn_watch_timer = Some(h.abort_handle());
    }

    /// `checkApnChange()` (`:1495-1507`): request a `boot` report when the
    /// modem's APN block differs from the last one REPORTED, unless a report
    /// is already queued (it will carry it). Never fails.
    pub async fn check_apn_change(&self) {
        if self.lock().stopped {
            return;
        }
        let now = (self.opts().now)();
        let Ok(sample) = (self.opts().modem)(now).await else {
            return; // a failed probe is a missed tick
        };
        let fp = rules::apn_fingerprint(sample.apn.as_ref());
        let mut core = self.lock();
        if fp == core.last_reported_apn || core.timer.is_some() {
            return;
        }
        drop(core);
        self.event(ActivityLevel::Info, rules::APN_CHANGED_EVENT);
        core = self.lock();
        self.request_locked(&mut core, ReportOrigin::Boot);
    }

    /// `publishReport(origin)` (`:1513-1569`): build and publish one report.
    /// Never fails: a failure is a lost report (`config report failed: …`).
    pub async fn publish_report(&self, origin: ReportOrigin) {
        if self.lock().stopped {
            return;
        }
        let opts = self.opts();
        let now = (opts.now)();
        // `loadSettings()`: `{}` when the file could not be read.
        let settings: AgentSettings = self.read_settings().unwrap_or_default();
        let modem = match (opts.modem)(now).await {
            Ok(m) => m,
            Err(msg) => {
                self.event(ActivityLevel::Warn, &rules::report_failed_event(&msg));
                return;
            }
        };
        let mut core = self.lock();
        if core.stopped {
            return;
        }
        let res = rules::resolve_report_origin(origin, core.state.namespaces.as_ref(), &settings);
        if res.clear_local_pending {
            self.persist(&mut core, vec![SyncPatch::LocalPending(false)]);
        }
        let report = build_config_report(
            &ReportInput {
                device_id: opts.device_id.clone(),
                ts: now,
                origin: res.origin,
                last_applied_rev: core.state.last_applied_rev,
                settings,
                modem,
                desired: core.desired.clone(),
                hints: opts.hints.as_ref().map(|f| f()),
                camera_mount: opts.camera_mount.as_ref().and_then(|f| f()),
                telemetry_scope: opts.telemetry_scope.as_ref().and_then(|f| f()),
                mode: opts.mode.clone(),
                changed: res.changed,
            },
            &opts.build,
        );
        // The 32 KB cap is the backend's; advisory only — never drop a report (CF14).
        let delivered = match (opts.publish)(&opts.topic, report.to_wire().into_bytes()) {
            Ok(()) => true,
            Err(PublishFailure::NotConnected) => false,
            Err(PublishFailure::Failed(msg)) => {
                drop(core);
                self.event(ActivityLevel::Warn, &rules::report_failed_event(&msg));
                return;
            }
        };
        let online = delivered && opts.online.as_ref().is_none_or(|f| f());
        let patch = rules::persist_after_publish(res.origin, online, &res.current, now);
        self.persist(&mut core, patch);
        core.last_reported_apn = rules::effective_apn_fingerprint(&report.effective);
        self.schedule_settle_report(&mut core, &report);
        self.watch_for_modem_arrival(&mut core, &report);
    }

    /// `armModemArrivalWatch()` (`:1593-1620`): one probe every
    /// `modemArrivalMs`, at most `MODEM_ARRIVAL_TRIES`, until the daemon speaks.
    fn arm_modem_arrival_watch(&self, core: &mut Core) {
        if core.stopped || core.modem_arrival_timer.is_some() || core.modem_seen {
            return;
        }
        if core.modem_arrival_tries >= rules::MODEM_ARRIVAL_TRIES {
            return;
        }
        let me = self.clone();
        let delay = ms(self.opts().modem_arrival_ms);
        let h = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            {
                let mut core = me.lock();
                core.modem_arrival_timer = None;
                if core.stopped || core.modem_seen {
                    return;
                }
                core.modem_arrival_tries += 1;
            }
            let now = (me.opts().now)();
            if let Ok(sample) = (me.opts().modem)(now).await {
                if rules::modem_arrival_trigger(&sample) {
                    me.lock().modem_seen = true;
                    me.event(ActivityLevel::Info, rules::MODEM_ARRIVED_EVENT);
                    me.request(ReportOrigin::Boot);
                    return;
                }
            }
            let mut core = me.lock();
            me.arm_modem_arrival_watch(&mut core);
        });
        core.modem_arrival_timer = Some(h.abort_handle());
    }

    /// `watchForModemArrival(report)` (`:1622-1629`).
    fn watch_for_modem_arrival(&self, core: &mut Core, report: &ConfigReport) {
        if rules::modem_seen(&report.effective) {
            core.modem_seen = true;
            return;
        }
        self.arm_modem_arrival_watch(core);
    }

    /// `scheduleSettleReport(report)` (`:1636-1650`): one more report when a
    /// `modem-poll` namespace is saved but not yet in force; bounded per rev.
    fn schedule_settle_report(&self, core: &mut Core, report: &ConfigReport) {
        if core.stopped || core.settle_timer.is_some() || core.settle_budget == 0 {
            return;
        }
        if !rules::needs_settle_report(&report.status) {
            return;
        }
        core.settle_budget -= 1;
        let origin = rules::settle_origin(report.origin);
        let me = self.clone();
        let delay = ms(self.opts().settle_ms);
        let h = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let mut core = me.lock();
            core.settle_timer = None;
            me.request_locked(&mut core, origin);
        });
        core.settle_timer = Some(h.abort_handle());
    }
}
