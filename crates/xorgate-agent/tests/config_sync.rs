//! `class ConfigSync` (`src/agent/config-sync.ts:1211-1651`) through the
//! Rust port, on tokio's paused clock: the 26 class/timer `it()`s of
//! `test/config-sync.test.ts` and the 2 of `test/config-sync-cellular.test.ts`
//! (CF25), each run with the SAME steps and option values as the it(). The
//! expectations — every published payload byte for byte, the topics, the
//! `onEvent` lines, the modem read count, the final `config-sync.json` bytes
//! and each scenario's extras — were dumped by driving the REAL class from a
//! `git archive 89b5c64` scratch copy (`fixtures/config_sync_rules/class-scenarios.json`,
//! driver `phase-2/evidence/wp3-tools/dump-config-sync.mts`, `now()` pinned).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_agent::config_plane::message::apply_cloud_config;
use xorgate_agent::config_plane::sync::{ConfigSync, ConfigSyncOptions};
use xorgate_agent::mqtt_plane::PublishFailure;
use xorgate_core::build_info::BuildInfo;
use xorgate_core::cloud_config::parse_cloud_message;
use xorgate_core::config_report::{ConfigSyncState, ModemBiasSample, ReportOrigin};
use xorgate_core::settings::{parse_settings_object, SettingsReadFailure};
use xorgate_core::telemetry::ActivityLevel;
use xorgate_hal::config_sync_file::{ensure_config_sync_baseline, load_config_sync_state};
use xorgate_hal::settings_io::sha256_file;

const NOW: i64 = 1_786_017_600_000;
const JS_COMMIT: &str = "89b5c6407ef88e20f7168bd767d22a7be04ebcb6";

#[derive(Deserialize)]
struct Case {
    name: String,
    it: String,
    expected: Value,
}

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "jsCommit")]
    js_commit: String,
    cases: Vec<Case>,
}

fn fixture() -> Fixture {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/config_sync_rules/class-scenarios.json"
    );
    let f: Fixture = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    assert_eq!(f.js_commit, JS_COMMIT);
    f
}

fn expected(it: &str) -> Value {
    let f = fixture();
    let c = f
        .cases
        .into_iter()
        .find(|c| c.it == it)
        .unwrap_or_else(|| panic!("no fixture case {it}"));
    assert!(!c.name.is_empty());
    c.expected
}

fn sample(v: Value) -> ModemBiasSample {
    serde_json::from_value(v).unwrap()
}

fn fresh() -> ModemBiasSample {
    sample(json!({"value": true, "fresh": true}))
}

fn no_modem() -> ModemBiasSample {
    sample(json!({"value": null, "fresh": false, "detail": "modem telemetry unavailable"}))
}

/// What a scenario captured (the driver's `Capture`).
#[derive(Default)]
struct Cap {
    sent: Mutex<Vec<String>>,
    topics: Mutex<Vec<String>>,
    events: Mutex<Vec<String>>,
    reads: AtomicU32,
}

impl Cap {
    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
    fn len(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

struct Home {
    dir: PathBuf,
}

impl Home {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("xg-wp3-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }
    fn settings(&self) -> PathBuf {
        self.dir.join("settings.json")
    }
    fn state(&self) -> PathBuf {
        self.dir.join("config-sync.json")
    }
    /// `saveSettings(<literal>)` = `atomicWriteJson` = `JSON.stringify(literal)`.
    fn save_settings(&self, json_text: &str) {
        std::fs::write(self.settings(), json_text).unwrap();
    }
    fn baseline(&self) {
        ensure_config_sync_baseline(&self.state(), &self.settings());
    }
    fn state_file(&self) -> Value {
        match std::fs::read_to_string(self.state()) {
            Ok(t) => Value::String(t),
            Err(_) => Value::Null,
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `base(c, over)` of the driver: deviceId dev-1, topic "t", publish/onEvent
/// captured, `now: () => NOW`. `modem(n)` gets the post-increment read count.
fn base(
    h: &Home,
    cap: &Arc<Cap>,
    modem: impl Fn(u32) -> Result<ModemBiasSample, String> + Send + Sync + 'static,
) -> ConfigSyncOptions {
    let c = cap.clone();
    let publish = Arc::new(move |topic: &str, payload: Vec<u8>| {
        c.topics.lock().unwrap().push(topic.to_string());
        c.sent
            .lock()
            .unwrap()
            .push(String::from_utf8(payload).unwrap());
        Ok::<(), PublishFailure>(())
    });
    let c = cap.clone();
    let modem = Arc::new(modem);
    let modem_fn = Arc::new(move |_now: i64| {
        let n = c.reads.fetch_add(1, Ordering::SeqCst) + 1;
        let out = modem(n);
        Box::pin(async move { out })
            as xorgate_agent::ctx::BoxFut<'static, Result<ModemBiasSample, String>>
    });
    let mut o = ConfigSyncOptions::new(
        "dev-1",
        "t",
        publish,
        modem_fn,
        h.state(),
        h.settings(),
        BuildInfo {
            version: "test".into(),
            channel: "dev-local".into(),
            built_at: None,
            package_version: "0.0.1".into(),
        },
    );
    let c = cap.clone();
    let home = h.dir.display().to_string();
    o.on_event = Some(Arc::new(move |level: ActivityLevel, msg: &str| {
        let level = match level {
            ActivityLevel::Warn => "warn",
            ActivityLevel::Error => "error",
            ActivityLevel::Info => "info",
        };
        c.events
            .lock()
            .unwrap()
            .push(format!("{level}: {}", msg.replace(&home, "<home>")));
    }));
    o.now = Arc::new(|| NOW);
    o
}

/// `settings: () => (<literal>)`.
fn settings_literal(o: &mut ConfigSyncOptions, v: Value) {
    let s = parse_settings_object(v.as_object().unwrap());
    o.read_settings = Some(Arc::new(move || Ok(s.clone())));
}

fn settings_failing(o: &mut ConfigSyncOptions, reason: SettingsReadFailure) {
    o.read_settings = Some(Arc::new(move || Err(reason)));
}

async fn delay(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn desired(sync: &ConfigSync, msg: Value, applied: bool) {
    let mut m = json!({"v": 1});
    m.as_object_mut()
        .unwrap()
        .extend(msg.as_object().unwrap().clone());
    let parsed = parse_cloud_message(&m.to_string()).unwrap();
    sync.note_cloud_config(parsed.config.rev, parsed.raw, parsed.config, applied);
}

/// `JSON.stringify(sync.syncState)` — the in-memory state, key order included.
fn sync_state(sync: &ConfigSync) -> String {
    sync.sync_state().to_file_string()
}

/// Compare the common capture + the extras the scenario returns.
fn check(it: &str, h: &Home, cap: &Cap, extras: Value) {
    let e = expected(it);
    assert_eq!(json!(cap.sent()), e["sent"], "{it}: sent");
    assert_eq!(
        json!(cap.topics.lock().unwrap().clone()),
        e["topics"],
        "{it}: topics"
    );
    assert_eq!(
        json!(cap.events.lock().unwrap().clone()),
        e["events"],
        "{it}: events"
    );
    assert_eq!(
        json!(cap.reads.load(Ordering::SeqCst)),
        e["reads"],
        "{it}: reads"
    );
    assert_eq!(h.state_file(), e["configSyncFile"], "{it}: config-sync.json");
    for (k, v) in extras.as_object().unwrap() {
        assert_eq!(v, &e[k], "{it}: {k}");
    }
}

// --- config-sync.json state file ---------------------------------------------------

/// config-sync.test.ts:60 "lives beside settings.json, never inside it".
#[test]
fn state_file_lives_beside_settings() {
    let h = Home::new("t60");
    let rel = |p: &Path| {
        p.display()
            .to_string()
            .trim_start_matches(&h.dir.display().to_string())
            .to_string()
    };
    let e = expected("config-sync.test.ts:60");
    assert_eq!(
        json!(rel(&xorgate_hal::config_sync_file::config_sync_path(&h.dir))),
        e["configSyncPath"]
    );
    assert_eq!(
        json!(rel(&xorgate_hal::settings_io::settings_path(&h.dir))),
        e["settingsPath"]
    );
}

/// config-sync.test.ts:67 "reports a failed persist instead of throwing".
#[test]
fn state_file_failed_persist_is_false() {
    let mut s = ConfigSyncState::default();
    s.merge([xorgate_core::config_report::SyncPatch::LastAppliedRev(1)]);
    let saved = xorgate_hal::config_sync_file::save_config_sync_state(
        &s,
        Path::new("/nope/does/not/exist.json"),
    );
    assert_eq!(json!(saved), expected("config-sync.test.ts:67")["saved"]);
}

// --- the boot race (modemArrivalMs 10, debounceMs 1) ----------------------------------

fn arrival(
    h: &Home,
    cap: &Arc<Cap>,
    modem: impl Fn(u32) -> Result<ModemBiasSample, String> + Send + Sync + 'static,
) -> ConfigSync {
    let mut o = base(h, cap, modem);
    settings_literal(&mut o, json!({}));
    o.debounce_ms = 1;
    o.modem_arrival_ms = 10;
    ConfigSync::new(o)
}

/// config-sync.test.ts:199 "re-reports on its own once the daemon finally speaks".
#[tokio::test(start_paused = true)]
async fn boot_race_rereports_when_the_daemon_speaks() {
    let (h, cap) = (Home::new("t199"), Arc::new(Cap::default()));
    let sync = arrival(&h, &cap, |n| {
        Ok(if n <= 2 {
            no_modem()
        } else {
            sample(json!({"value": true, "fresh": true, "sim": {"state": "ready", "present": 1}}))
        })
    });
    sync.publish_report(ReportOrigin::Boot).await;
    assert_eq!(cap.len(), 1);
    delay(120).await;
    sync.stop();
    check("config-sync.test.ts:199", &h, &cap, json!({}));
}

/// config-sync.test.ts:221 "does not arm the watch at all when the modem already spoke".
#[tokio::test(start_paused = true)]
async fn boot_race_no_watch_when_already_spoken() {
    let (h, cap) = (Home::new("t221"), Arc::new(Cap::default()));
    let sync = arrival(&h, &cap, |_| {
        Ok(sample(
            json!({"value": true, "fresh": true, "sim": {"state": "ready", "present": 1}}),
        ))
    });
    sync.publish_report(ReportOrigin::Boot).await;
    delay(120).await;
    sync.stop();
    check("config-sync.test.ts:221", &h, &cap, json!({}));
}

/// config-sync.test.ts:235 "gives up instead of polling forever on a device with no modem".
#[tokio::test(start_paused = true)]
async fn boot_race_gives_up() {
    let (h, cap) = (Home::new("t235"), Arc::new(Cap::default()));
    let sync = arrival(&h, &cap, |_| Ok(no_modem()));
    sync.publish_report(ReportOrigin::Boot).await;
    delay(300).await;
    sync.stop();
    check("config-sync.test.ts:235", &h, &cap, json!({}));
}

/// config-sync.test.ts:249 "survives a probe that throws".
#[tokio::test(start_paused = true)]
async fn boot_race_survives_a_throwing_probe() {
    let (h, cap) = (Home::new("t249"), Arc::new(Cap::default()));
    let sync = arrival(&h, &cap, |_| Err("telemetry exploded".to_string()));
    sync.publish_report(ReportOrigin::Boot).await;
    delay(60).await;
    sync.stop();
    check("config-sync.test.ts:249", &h, &cap, json!({}));
}

// --- ConfigSync (settings {gnssAntBias:true}, modem FRESH, debounceMs 1) ----------------

fn harness(h: &Home, cap: &Arc<Cap>) -> ConfigSyncOptions {
    let mut o = base(h, cap, |_| Ok(fresh()));
    o.topic = "xorgate/devices/dev-1/config/reported".into();
    settings_literal(&mut o, json!({"gnssAntBias": true}));
    o.debounce_ms = 1;
    o
}

/// config-sync.test.ts:620 "coalesces a connect and the retained message that follows into one report".
#[tokio::test(start_paused = true)]
async fn coalesces_into_one_cloud_report() {
    let (h, cap) = (Home::new("t620"), Arc::new(Cap::default()));
    let sync = ConfigSync::new(harness(&h, &cap));
    sync.request(ReportOrigin::Boot);
    sync.request(ReportOrigin::Cloud);
    sync.request(ReportOrigin::Boot);
    delay(30).await;
    sync.stop();
    check("config-sync.test.ts:620", &h, &cap, json!({}));
}

/// config-sync.test.ts:632 "publishes to the reported topic, unretained".
#[tokio::test(start_paused = true)]
async fn publishes_to_the_reported_topic() {
    let (h, cap) = (Home::new("t632"), Arc::new(Cap::default()));
    let mut o = base(&h, &cap, |_| Ok(no_modem()));
    o.topic = "xorgate/devices/dev-1/config/reported".into();
    settings_literal(&mut o, json!({}));
    o.debounce_ms = 1;
    let sync = ConfigSync::new(o);
    sync.request(ReportOrigin::Boot);
    delay(30).await;
    sync.stop();
    check("config-sync.test.ts:632", &h, &cap, json!({}));
}

/// config-sync.test.ts:648 "advances and persists lastAppliedRev only for an applied document".
#[tokio::test(start_paused = true)]
async fn advances_rev_only_when_applied() {
    let (h, cap) = (Home::new("t648"), Arc::new(Cap::default()));
    let sync = ConfigSync::new(harness(&h, &cap));
    let mut steps = vec![json!(sync.should_apply(Some(5)))];
    desired(&sync, json!({"rev": 5, "gnssAntBias": true}), false);
    steps.push(json!(sync.last_applied_rev()));
    let after_skip = h.state_file();
    desired(&sync, json!({"rev": 5, "gnssAntBias": true}), true);
    steps.extend([
        json!(sync.last_applied_rev()),
        json!(sync.should_apply(Some(5))),
        json!(sync.should_apply(Some(6))),
        json!(load_config_sync_state(&h.state()).last_applied_rev),
    ]);
    sync.stop();
    check(
        "config-sync.test.ts:648",
        &h,
        &cap,
        json!({"steps": steps, "afterSkip": after_skip}),
    );
}

/// config-sync.test.ts:666 "carries the last desired doc into the report even when the apply was skipped".
#[tokio::test(start_paused = true)]
async fn carries_the_desired_doc_on_a_skip() {
    let (h, cap) = (Home::new("t666"), Arc::new(Cap::default()));
    let sync = ConfigSync::new(harness(&h, &cap));
    desired(&sync, json!({"rev": 3, "gnssAntBias": true}), true);
    desired(&sync, json!({"rev": 3, "gnssAntBias": true}), false);
    sync.request(ReportOrigin::Cloud);
    delay(30).await;
    sync.stop();
    check("config-sync.test.ts:666", &h, &cap, json!({}));
}

/// config-sync.test.ts:680 "re-reports a modem-poll namespace that has not settled yet".
#[tokio::test(start_paused = true)]
async fn settle_rereport() {
    let (h, cap) = (Home::new("t680"), Arc::new(Cap::default()));
    let mut o = harness(&h, &cap);
    o.topic = "t".into();
    // `modemReads++ > 0`: false on the first read, true after.
    o.modem = base(&h, &cap, |n| Ok(sample(json!({"value": n > 1, "fresh": true})))).modem;
    o.settle_ms = 5;
    let sync = ConfigSync::new(o);
    desired(&sync, json!({"rev": 4, "gnssAntBias": true}), true);
    sync.request(ReportOrigin::Cloud);
    for _ in 0..200 {
        if cap.len() >= 2 {
            break;
        }
        delay(10).await;
    }
    sync.stop();
    check("config-sync.test.ts:680", &h, &cap, json!({}));
}

/// config-sync.test.ts:718 "bounds the settle re-reports so a refused value cannot loop forever".
#[tokio::test(start_paused = true)]
async fn settle_rereports_are_bounded() {
    let (h, cap) = (Home::new("t718"), Arc::new(Cap::default()));
    let mut o = harness(&h, &cap);
    o.topic = "t".into();
    o.modem = base(&h, &cap, |_| Ok(sample(json!({"value": false, "fresh": true})))).modem;
    o.settle_ms = 5;
    o.settle_reports = 2;
    let sync = ConfigSync::new(o);
    desired(&sync, json!({"rev": 4, "gnssAntBias": true}), true);
    sync.request(ReportOrigin::Cloud);
    delay(120).await;
    sync.stop();
    check("config-sync.test.ts:718", &h, &cap, json!({}));
}

/// config-sync.test.ts:741 "does not re-report an agent-poll namespace that is merely divergent".
#[tokio::test(start_paused = true)]
async fn no_settle_for_agent_poll() {
    let (h, cap) = (Home::new("t741"), Arc::new(Cap::default()));
    let mut o = base(&h, &cap, |_| Ok(no_modem()));
    settings_literal(&mut o, json!({"timeSync": {"offlineSources": false}}));
    o.debounce_ms = 1;
    o.settle_ms = 5;
    let sync = ConfigSync::new(o);
    desired(
        &sync,
        json!({"rev": 4, "timeSync": {"offlineSources": true}}),
        true,
    );
    sync.request(ReportOrigin::Cloud);
    delay(120).await;
    sync.stop();
    check("config-sync.test.ts:741", &h, &cap, json!({}));
}

/// config-sync.test.ts:763 "swallows a publish failure rather than throwing into the apply path".
#[tokio::test(start_paused = true)]
async fn swallows_a_publish_failure() {
    let (h, cap) = (Home::new("t763"), Arc::new(Cap::default()));
    let mut o = base(&h, &cap, |_| Ok(no_modem()));
    o.publish = Arc::new(|_, _| Err(PublishFailure::Failed("broker gone".into())));
    settings_literal(&mut o, json!({}));
    o.debounce_ms = 1;
    let sync = ConfigSync::new(o);
    sync.publish_report(ReportOrigin::Boot).await;
    sync.stop();
    check("config-sync.test.ts:763", &h, &cap, json!({}));
}

/// config-sync.test.ts:778 "stops queueing once stopped".
#[tokio::test(start_paused = true)]
async fn stops_queueing_once_stopped() {
    let (h, cap) = (Home::new("t778"), Arc::new(Cap::default()));
    let sync = ConfigSync::new(harness(&h, &cap));
    sync.stop();
    sync.request(ReportOrigin::Cloud);
    delay(30).await;
    check("config-sync.test.ts:778", &h, &cap, json!({}));
}

// --- the settings.json fingerprint watcher (watchMs 5, debounceMs 1, NO_MODEM) ------------

fn watcher(h: &Home, cap: &Arc<Cap>) -> ConfigSyncOptions {
    let mut o = base(h, cap, |_| Ok(no_modem()));
    o.topic = "xorgate/devices/dev-1/config/reported".into();
    o.debounce_ms = 1;
    o.watch_ms = 5;
    o
}

/// The worker's apply path: `applyCloudConfig(message.config)` + `noteCloudConfig(…, true)`.
fn cloud_apply(h: &Home, sync: &ConfigSync, payload: &str) {
    let m = parse_cloud_message(payload).unwrap();
    apply_cloud_config(&m.config, &h.settings()).unwrap();
    sync.note_cloud_config(m.config.rev, m.raw, m.config, true);
}

/// config-sync.test.ts:857 "reports an out-of-band edit as origin `local` with the changed namespace".
#[tokio::test(start_paused = true)]
async fn watcher_reports_an_out_of_band_edit() {
    let (h, cap) = (Home::new("t857"), Arc::new(Cap::default()));
    h.save_settings(r#"{"mode":"real","gnssAntBias":true}"#);
    h.baseline();
    let sync = ConfigSync::new(watcher(&h, &cap));
    sync.start_watcher();
    h.save_settings(r#"{"mode":"real","gnssAntBias":false}"#);
    delay(40).await;
    sync.stop();
    check(
        "config-sync.test.ts:857",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync)}),
    );
}

/// config-sync.test.ts:876 "catches an edit made while the agent was stopped".
#[tokio::test(start_paused = true)]
async fn watcher_catches_an_edit_made_while_stopped() {
    let (h, cap) = (Home::new("t876"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true}"#);
    h.baseline();
    h.save_settings(r#"{"gnssAntBias":false}"#);
    let sync = ConfigSync::new(watcher(&h, &cap));
    sync.start_watcher();
    delay(30).await;
    sync.stop();
    check(
        "config-sync.test.ts:876",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync)}),
    );
}

/// config-sync.test.ts:888 "says nothing about a write that moved no synced namespace".
#[tokio::test(start_paused = true)]
async fn watcher_ignores_a_write_that_moved_nothing() {
    let (h, cap) = (Home::new("t888"), Arc::new(Cap::default()));
    h.save_settings(r#"{"mode":"real","gnssAntBias":true}"#);
    h.baseline();
    let sync = ConfigSync::new(watcher(&h, &cap));
    sync.start_watcher();
    h.save_settings(r#"{"mode":"sim","gnssAntBias":true}"#);
    delay(40).await;
    sync.stop();
    let hash_matches = sync.sync_state().settings_hash.as_deref() == Some(&sha256_file(&h.settings()));
    check(
        "config-sync.test.ts:888",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync), "hashMatches": hash_matches}),
    );
}

/// config-sync.test.ts:901 "does not mistake the worker's own cloud apply for a local change".
#[tokio::test(start_paused = true)]
async fn watcher_ignores_the_workers_own_apply() {
    let (h, cap) = (Home::new("t901"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true}"#);
    h.baseline();
    let sync = ConfigSync::new(watcher(&h, &cap));
    sync.start_watcher();
    cloud_apply(&h, &sync, r#"{"v":1,"rev":4,"gnssAntBias":false}"#);
    sync.request(ReportOrigin::Cloud);
    delay(40).await;
    sync.stop();
    let settings_file = std::fs::read_to_string(h.settings()).unwrap();
    check(
        "config-sync.test.ts:901",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync), "settingsFile": settings_file}),
    );
}

/// config-sync.test.ts:921 "keeps a pending local change the cloud document did not overwrite".
#[tokio::test(start_paused = true)]
async fn watcher_keeps_a_pending_local_change() {
    let (h, cap) = (Home::new("t921"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true,"timeSync":{"offlineSources":true}}"#);
    h.baseline();
    let mut o = watcher(&h, &cap);
    o.online = Some(Arc::new(|| false));
    let sync = ConfigSync::new(o);
    sync.start_watcher();
    h.save_settings(r#"{"gnssAntBias":true,"timeSync":{"offlineSources":false}}"#);
    delay(30).await;
    let pending_mid = sync.sync_state().local_pending;
    cloud_apply(&h, &sync, r#"{"v":1,"rev":7,"gnssAntBias":false}"#);
    delay(30).await;
    sync.stop();
    check(
        "config-sync.test.ts:921",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync), "pendingMid": pending_mid}),
    );
}

/// config-sync.test.ts:944 "re-sends a local report on the next connect when the link was down".
#[tokio::test(start_paused = true)]
async fn watcher_resends_on_the_next_connect() {
    let (h, cap) = (Home::new("t944"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true}"#);
    h.baseline();
    let online = Arc::new(AtomicBool::new(false));
    let mut o = watcher(&h, &cap);
    let on = online.clone();
    o.online = Some(Arc::new(move || on.load(Ordering::SeqCst)));
    let sync = ConfigSync::new(o);
    sync.start_watcher();
    h.save_settings(r#"{"gnssAntBias":false}"#);
    delay(30).await;
    let pending_mid = sync.sync_state().local_pending;
    let sent_mid = cap.len();
    online.store(true, Ordering::SeqCst);
    sync.request(ReportOrigin::Boot);
    delay(30).await;
    sync.stop();
    check(
        "config-sync.test.ts:944",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync), "pendingMid": pending_mid, "sentMid": sent_mid}),
    );
}

/// config-sync.test.ts:965 "downgrades to `boot` when the edit was reverted before the report went out".
#[tokio::test(start_paused = true)]
async fn watcher_downgrades_a_reverted_edit() {
    let (h, cap) = (Home::new("t965"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true}"#);
    h.baseline();
    let mut o = watcher(&h, &cap);
    o.debounce_ms = 40;
    let sync = ConfigSync::new(o);
    sync.start_watcher();
    h.save_settings(r#"{"gnssAntBias":false}"#);
    delay(10).await;
    h.save_settings(r#"{"gnssAntBias":true}"#);
    delay(80).await;
    sync.stop();
    check(
        "config-sync.test.ts:965",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync)}),
    );
}

/// config-sync.test.ts:985 "never reports a local change when settings.json cannot be read".
#[tokio::test(start_paused = true)]
async fn watcher_never_reports_off_an_unreadable_file() {
    let (h, cap) = (Home::new("t985"), Arc::new(Cap::default()));
    h.save_settings(
        r#"{"mode":"real","gnssAntBias":true,"timeSync":{"offlineSources":true},"imuMount":{"enabled":true,"rollDeg":90,"pitchDeg":0,"yawDeg":90}}"#,
    );
    h.baseline();
    let baseline_file = h.state_file();
    let mut o = watcher(&h, &cap);
    settings_failing(&mut o, SettingsReadFailure::Unreadable);
    let sync = ConfigSync::new(o);
    sync.start_watcher();
    h.save_settings(r#"{"gnssAntBias": true, "mode": "real"}"#);
    delay(40).await;
    sync.stop();
    check(
        "config-sync.test.ts:985",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync), "baselineFile": baseline_file}),
    );
}

/// config-sync.test.ts:1010 "says so once, not every tick, while settings.json stays unreadable".
#[tokio::test(start_paused = true)]
async fn watcher_warns_once_while_unreadable() {
    let (h, cap) = (Home::new("t1010"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true}"#);
    h.baseline();
    let mut o = watcher(&h, &cap);
    settings_failing(&mut o, SettingsReadFailure::Invalid);
    let sync = ConfigSync::new(o);
    sync.start_watcher();
    h.save_settings("{ this is not json");
    delay(40).await;
    sync.stop();
    check("config-sync.test.ts:1010", &h, &cap, json!({}));
}

/// config-sync.test.ts:1028 "still treats a genuinely ABSENT settings.json as an erase".
#[tokio::test(start_paused = true)]
async fn watcher_treats_an_absent_file_as_an_erase() {
    let (h, cap) = (Home::new("t1028"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true,"timeSync":{"offlineSources":true}}"#);
    h.baseline();
    let sync = ConfigSync::new(watcher(&h, &cap));
    sync.start_watcher();
    std::fs::remove_file(h.settings()).unwrap();
    delay(40).await;
    sync.stop();
    check(
        "config-sync.test.ts:1028",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync)}),
    );
}

/// config-sync.test.ts:1040 "does not re-baseline off an unreadable file after the worker writes".
#[tokio::test(start_paused = true)]
async fn note_settings_written_refuses_an_unreadable_file() {
    let (h, cap) = (Home::new("t1040"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true,"timeSync":{"offlineSources":true}}"#);
    h.baseline();
    let before = h.state_file();
    let mut o = watcher(&h, &cap);
    settings_failing(&mut o, SettingsReadFailure::Unreadable);
    let sync = ConfigSync::new(o);
    sync.note_settings_written(&["gnssAntBias", "timeSync"]);
    sync.stop();
    check(
        "config-sync.test.ts:1040",
        &h,
        &cap,
        json!({"syncState": sync_state(&sync), "before": before}),
    );
}

// --- checkApnChange (config-sync-cellular.test.ts) -----------------------------------

/// config-sync-cellular.test.ts:263 "requests a report when the machinery moves, and stays quiet when it does not".
#[tokio::test(start_paused = true)]
async fn apn_watch_reports_when_the_machinery_moves() {
    let (h, cap) = (Home::new("c263"), Arc::new(Cap::default()));
    let block = Arc::new(Mutex::new(
        json!({"state": "applying", "apn_requested": "new.apn"}),
    ));
    let b = block.clone();
    let mut o = base(&h, &cap, move |_| {
        Ok(sample(
            json!({"value": true, "fresh": true, "apn": b.lock().unwrap().clone()}),
        ))
    });
    settings_literal(&mut o, json!({}));
    o.debounce_ms = 1;
    let sync = ConfigSync::new(o);
    sync.publish_report(ReportOrigin::Boot).await;
    sync.check_apn_change().await;
    delay(30).await;
    let sent_mid = cap.len();
    *block.lock().unwrap() = json!({"state": "in-force", "apn_applied": "new.apn", "fails": 0});
    sync.check_apn_change().await;
    delay(30).await;
    sync.stop();
    check(
        "config-sync-cellular.test.ts:263",
        &h,
        &cap,
        json!({"sentMid": sent_mid}),
    );
}

/// config-sync-cellular.test.ts:284 "says nothing on a device with no APN machinery at all".
#[tokio::test(start_paused = true)]
async fn apn_watch_is_silent_without_machinery() {
    let (h, cap) = (Home::new("c284"), Arc::new(Cap::default()));
    let mut o = base(&h, &cap, |_| Ok(fresh()));
    settings_literal(&mut o, json!({}));
    o.debounce_ms = 1;
    let sync = ConfigSync::new(o);
    sync.publish_report(ReportOrigin::Boot).await;
    sync.check_apn_change().await;
    delay(30).await;
    sync.stop();
    check("config-sync-cellular.test.ts:284", &h, &cap, json!({}));
}

/// Every scenario of the fixture is driven above (28 = 26 + 2, CF25).
#[test]
fn every_fixture_scenario_has_a_test() {
    let f = fixture();
    assert_eq!(f.cases.len(), 28);
    let src = include_str!("config_sync.rs");
    for c in &f.cases {
        assert!(
            src.contains(&format!("\"{}\"", c.it)),
            "scenario {} has no test",
            c.it
        );
    }
}

// --- WP1 ambiguity #8: a QoS 1 report into a dead link stays pending ----------------

/// The Rust client does not queue offline (`PublishFailure::NotConnected`):
/// the local change must stay pending — even when `online()` already reads
/// true — and the next connect's `boot` request (upgraded to `local`) re-sends it.
#[tokio::test(start_paused = true)]
async fn not_connected_keeps_the_local_change_pending() {
    let (h, cap) = (Home::new("nc"), Arc::new(Cap::default()));
    h.save_settings(r#"{"gnssAntBias":true}"#);
    h.baseline();
    let up = Arc::new(AtomicBool::new(false));
    let mut o = watcher(&h, &cap);
    let (c, u) = (cap.clone(), up.clone());
    o.publish = Arc::new(move |_, payload| {
        if !u.load(Ordering::SeqCst) {
            return Err(PublishFailure::NotConnected);
        }
        c.sent
            .lock()
            .unwrap()
            .push(String::from_utf8(payload).unwrap());
        Ok(())
    });
    o.online = Some(Arc::new(|| true)); // races to true: still not retired
    let sync = ConfigSync::new(o);
    sync.start_watcher();
    h.save_settings(r#"{"gnssAntBias":false}"#);
    delay(30).await;
    assert_eq!(cap.len(), 0);
    assert_eq!(sync.sync_state().local_pending, Some(true));
    up.store(true, Ordering::SeqCst);
    sync.request(ReportOrigin::Boot); // the connect handler's assertion
    delay(30).await;
    sync.stop();
    let sent = cap.sent();
    assert_eq!(sent.len(), 1);
    let r: Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(r["origin"], "local");
    assert_eq!(r["changed"], json!({"gnssAntBias": false}));
    assert_eq!(sync.sync_state().local_pending, Some(false));
}

/// The hourly heartbeat requests `boot` (`startHeartbeat`, `:1452-1459`), idempotently.
#[tokio::test(start_paused = true)]
async fn heartbeat_requests_boot_every_period() {
    let (h, cap) = (Home::new("hb"), Arc::new(Cap::default()));
    let mut o = harness(&h, &cap);
    o.heartbeat_ms = 1000;
    let sync = ConfigSync::new(o);
    sync.start_heartbeat();
    sync.start_heartbeat();
    delay(3500).await;
    sync.stop();
    let origins: Vec<String> = cap
        .sent()
        .iter()
        .map(|s| serde_json::from_str::<Value>(s).unwrap()["origin"].to_string())
        .collect();
    assert_eq!(origins, vec!["\"boot\""; 3]);
}
