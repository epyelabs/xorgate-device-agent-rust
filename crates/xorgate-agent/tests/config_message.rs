//! The config plane around `ConfigSync`:
//! - `handleConfigMessage` ORDER (`worker.ts:781-900`) through
//!   [`MessageHandler`] with a recording [`PhaseHooks`] and the real
//!   state.json events: malformed, applied (uploadBucket + scope + hardware +
//!   settings), skipped redelivery, refused (unreadable settings.json),
//!   invalid hardware block;
//! - the plane's wiring through the frozen [`ConfigPlane`] (`start`): one
//!   report for a connect + the retained message, requests from
//!   [`ReportRequester`], nothing after `stop()`;
//! - the `cloud config applied: …` summaries, the config-sync.json byte
//!   shapes and the boot upgrades (`worker.ts:92-105`) against
//!   `fixtures/config_sync_rules/rules.json` (tsx-dumped);
//! - the hardware.json save/load round trip (CF27) and `readModemBias` over
//!   the telemetry-tolerance fixture (CF17).

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use serde::Deserialize;
use serde_json::{json, Value};
use xorgate_agent::config_plane::message::{applied_summary, MessageHandler};
use xorgate_agent::config_plane::sync::{read_modem_bias, ConfigSync, ConfigSyncOptions};
use xorgate_agent::config_plane::{self, options_for};
use xorgate_agent::ctx::{AgentCtx, ConfigPlaneDeps, PhaseHooks, ReportRequester};
use xorgate_agent::mqtt_plane::MqttHandle;
use xorgate_agent::scope::ScopeHandle;
use xorgate_cloud::mqtt::ConnectionState as LinkState;
use xorgate_core::cloud_config::parse_cloud_message;
use xorgate_core::config_report::{
    modem_bias_from_telemetry, parse_config_sync_state, ReportOrigin, SyncPatch,
};
use xorgate_core::hardware_block::parse_hardware_block;
use xorgate_core::telemetry::{ActivityLevel, ConnectionState};
use xorgate_hal::config_sync_file::{ensure_config_sync_baseline, save_config_sync_state};
use xorgate_hal::hardware_file::{read_hardware_file, save_hardware_block, HardwareFile};
use xorgate_hal::io::NullTape;

const JS_COMMIT: &str = "89b5c6407ef88e20f7168bd767d22a7be04ebcb6";

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    input: Value,
    expected: Value,
}

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "jsCommit")]
    js_commit: String,
    cases: Vec<Case>,
}

fn fixture(rel: &str) -> Fixture {
    let p = format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    let f: Fixture = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(f.js_commit, JS_COMMIT);
    f
}

fn rules(op: &str) -> Vec<Case> {
    fixture("config_sync_rules/rules.json")
        .cases
        .into_iter()
        .filter(|c| c.op == op)
        .collect()
}

/// `JSON.stringify(parsed)` of `cloud config applied: …` (`worker.ts:863`).
#[test]
fn applied_summaries_match_json_stringify() {
    let cases = rules("appliedSummary");
    assert_eq!(cases.len(), 5);
    for c in cases {
        let m = parse_cloud_message(c.input["payload"].as_str().unwrap()).unwrap();
        assert_eq!(
            json!(applied_summary(&m.config)),
            c.expected["summary"],
            "{}",
            c.name
        );
    }
}

/// `saveConfigSyncState` / `ensureConfigSyncBaseline` byte shapes.
#[test]
fn config_sync_file_shapes() {
    let home = scratch("wp3-shapes");
    let (p, s) = (home.join("config-sync.json"), home.join("settings.json"));
    let cases = rules("stateFile");
    assert_eq!(cases.len(), 2);
    let mut st = xorgate_core::config_report::ConfigSyncState::default();
    st.merge([SyncPatch::LastAppliedRev(42), SyncPatch::LastReportTs(1700)]);
    assert!(save_config_sync_state(&st, &p));
    assert_eq!(
        json!(std::fs::read_to_string(&p).unwrap()),
        cases[0].expected["file"]
    );
    std::fs::write(&s, cases[1].input["settings"].as_str().unwrap()).unwrap();
    ensure_config_sync_baseline(&p, &s);
    assert_eq!(
        json!(std::fs::read_to_string(&p).unwrap()),
        cases[1].expected["file"],
        "{}",
        cases[1].name
    );
    let _ = std::fs::remove_dir_all(&home);
}

fn ctx_in(tag: &str, hooks: Arc<dyn PhaseHooks>) -> (AgentCtx, PathBuf) {
    let home = scratch(tag);
    let mut ctx = test_ctx(&home, "dev-wp3", hooks, Arc::new(NullTape));
    let mut p = (*ctx.paths).clone();
    p.kvs_manifest = home.join("kvs.json");
    p.lte_telemetry = home.join("telemetry.json");
    ctx.paths = Arc::new(p);
    (ctx, home)
}

/// The boot upgrades (`worker.ts:92-105`): baseline FIRST, then the
/// `mediaEnabled` migration. ACCEPTED DIVERGENCE (review-wp1 #4,
/// `settings_migrate.rs`): a `recording` key the migration ADDS sits in
/// parser order in Rust, appended last in the TS — same values, so the
/// rewritten settings.json is compared structurally when the bytes differ.
#[test]
fn boot_upgrades() {
    let cases = rules("bootUpgrade");
    assert_eq!(cases.len(), 4);
    for (k, c) in cases.iter().enumerate() {
        let (ctx, home) = ctx_in(
            &format!("wp3-boot{k}"),
            Arc::new(xorgate_agent::ctx::NoopHooks),
        );
        std::fs::write(&ctx.paths.settings, c.input["settings"].as_str().unwrap()).unwrap();
        if let Some(ch) = c.input["channels"].as_array() {
            let channels: Vec<Value> = ch
                .iter()
                .map(|k| json!({"streamKey": k, "channelName": format!("dev-1-{}", k.as_str().unwrap())}))
                .collect();
            std::fs::write(
                &ctx.paths.kvs_manifest,
                json!({"deviceId": "dev-1", "region": "us-east-1", "credentialEndpoint": "c.example", "roleAlias": "ra", "channels": channels}).to_string(),
            )
            .unwrap();
        }
        config_plane::boot(&ctx);
        assert_eq!(
            json!(std::fs::read_to_string(&ctx.paths.config_sync).unwrap()),
            c.expected["configSyncFile"],
            "{}",
            c.name
        );
        let got = std::fs::read_to_string(&ctx.paths.settings).unwrap();
        let want = c.expected["settingsFile"].as_str().unwrap();
        if got != want {
            let (g, w): (Value, Value) = (
                serde_json::from_str(&got).unwrap(),
                serde_json::from_str(want).unwrap(),
            );
            assert_eq!(g, w, "{}", c.name);
            assert_eq!(c.name, "boot: mediaEnabled false, two cameras");
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// CF27 — the hardware.json half of `hardware-config.test.ts` "round-trips
/// through save/load and compares structurally": `saveHardwareBlock` bytes =
/// the tsx `JSON.stringify(block)` (`fixtures/hardware_block/parse.json`
/// `expectedJson`), and the read-back block is equal.
#[test]
fn hardware_block_save_load_round_trip() {
    #[derive(Deserialize)]
    struct HwCase {
        name: String,
        input: Value,
        #[serde(rename = "expectedJson")]
        expected_json: Option<String>,
    }
    #[derive(Deserialize)]
    struct HwFixture {
        cases: Vec<HwCase>,
    }
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hardware_block/parse.json"
    );
    let f: HwFixture = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let home = scratch("wp3-hw");
    let path = home.join("hardware.json");
    assert_eq!(read_hardware_file(&path), HardwareFile::Absent);
    let mut n = 0;
    for c in f.cases.iter().filter(|c| {
        c.expected_json.as_deref().is_some_and(|j| j != "null")
            && (c.name.contains("round-trips") || c.name.contains("full optional"))
    }) {
        let block = parse_hardware_block(&c.input).unwrap();
        save_hardware_block(&path, &block).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            c.expected_json.clone().unwrap(),
            "{}",
            c.name
        );
        assert_eq!(
            read_hardware_file(&path),
            HardwareFile::Block(block),
            "{}",
            c.name
        );
        n += 1;
    }
    assert_eq!(n, 2);
    std::fs::write(&path, "{not json").unwrap();
    assert_eq!(read_hardware_file(&path), HardwareFile::Invalid);
    std::fs::write(&path, r#"{"v":1}"#).unwrap();
    assert_eq!(read_hardware_file(&path), HardwareFile::FailedValidation);
    let _ = std::fs::remove_dir_all(&home);
}

/// CF17 — `readModemBias` (the I/O wrapper the plane uses) over every raw text
/// of `fixtures/modem/telemetry-tolerance.json` = the tsx `readModemBias`.
#[tokio::test]
async fn read_modem_bias_over_the_tolerance_fixture() {
    #[derive(Deserialize)]
    struct MCase {
        name: String,
        input: Value,
        expected: Value,
    }
    #[derive(Deserialize)]
    struct MFixture {
        cases: Vec<MCase>,
    }
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/modem/telemetry-tolerance.json"
    );
    let f: MFixture = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let home = scratch("wp3-modem");
    let file = home.join("telemetry.json");
    for c in &f.cases {
        std::fs::write(&file, c.input["raw"].as_str().unwrap()).unwrap();
        let now = c.input["now"].as_i64().unwrap();
        let got = read_modem_bias(&file, &NullTape, now).await;
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            c.expected["readModemBias"],
            "{}",
            c.name
        );
    }
    assert_eq!(f.cases.len(), 17);
    let missing = read_modem_bias(&home.join("missing.json"), &NullTape, 0).await;
    assert_eq!(missing, modem_bias_from_telemetry(None, 0));
    let _ = std::fs::remove_dir_all(&home);
}

// --- handleConfigMessage ORDER ----------------------------------------------------

/// `Some(msg)` = the next `reapply_recording` fails with `msg` (a throwing
/// telemetry finalize, review-wp1 M1).
type ReapplyErr = Arc<Mutex<Option<String>>>;

struct RecHooks(Timeline, ReapplyErr);
impl PhaseHooks for RecHooks {
    fn reapply_recording(&self) -> Result<(), String> {
        push(&self.0, "hooks.reapply_recording");
        match self.1.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    fn rearm_upload(&self) {
        push(&self.0, "hooks.rearm_upload");
    }
    fn hardware_changed(&self) {
        push(&self.0, "hooks.hardware_changed");
    }
    fn reconcile_camera_mount(&self) {
        push(&self.0, "hooks.reconcile_camera_mount");
    }
    fn led_config_applied(&self) {
        push(&self.0, "hooks.led_config_applied");
    }
}

struct Rig {
    ctx: AgentCtx,
    home: PathBuf,
    timeline: Timeline,
    sync: ConfigSync,
    handler: MessageHandler,
    sent: Arc<Mutex<Vec<Value>>>,
    reapply_err: ReapplyErr,
    _reports: tokio::sync::mpsc::UnboundedReceiver<ReportOrigin>,
}

fn rig(tag: &str) -> Rig {
    let timeline: Timeline = Arc::new(Mutex::new(vec![]));
    let reapply_err: ReapplyErr = Arc::new(Mutex::new(None));
    let (ctx, home) = ctx_in(
        tag,
        Arc::new(RecHooks(timeline.clone(), reapply_err.clone())),
    );
    let (requester, reports) = ReportRequester::channel();
    let scope = ScopeHandle::new(
        "dev-wp3",
        &ctx.env.config.topics.telemetry,
        ctx.state.clone(),
        requester.clone(),
    );
    let link = FakeLink::new(timeline.clone());
    link.set(LinkState::Connected);
    let (_r2, rx2) = ReportRequester::channel();
    let deps = ConfigPlaneDeps {
        mqtt: MqttHandle::new(link),
        scope: scope.clone(),
        reports: requester,
        report_rx: rx2,
        obd: None,
    };
    let mut o: ConfigSyncOptions = options_for(&ctx, &deps);
    let sent = Arc::new(Mutex::new(vec![]));
    let (s, t) = (sent.clone(), timeline.clone());
    o.publish = Arc::new(move |_topic, payload| {
        let v: Value = serde_json::from_slice(&payload).unwrap();
        push(
            &t,
            format!("report {} rev={}", v["origin"].as_str().unwrap(), v["rev"]),
        );
        s.lock().unwrap().push(v);
        Ok(())
    });
    let sync = ConfigSync::new(o);
    let handler = MessageHandler {
        ctx: ctx.clone(),
        sync: sync.clone(),
        scope,
        obd: None,
    };
    Rig {
        ctx,
        home,
        timeline,
        sync,
        handler,
        sent,
        reapply_err,
        _reports: reports,
    }
}

impl Rig {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.timeline.lock().unwrap())
    }
}

const CM4: &str = r#"{"v":2,"platform":"rpi-cm4","encoder":"hw-h264","rails":false,"statusLed":null,"imu":"bno085","video":[{"key":"cam0","connector":"csi0","sensor":"imx290"}]}"#;

/// `handleConfigMessage` (`worker.ts:781-900`): the side effects of every
/// branch, in order, with the verbatim state.json event strings.
#[tokio::test(start_paused = true)]
async fn handle_config_message_order() {
    let r = rig("wp3-order");
    let ctx = &r.ctx;
    std::fs::write(
        &ctx.paths.kvs_manifest,
        r#"{"deviceId":"dev-wp3","region":"us-east-1","credentialEndpoint":"c.example","roleAlias":"ra","channels":[]}"#,
    )
    .unwrap();
    std::fs::write(&ctx.paths.settings, r#"{"mode":"real"}"#).unwrap();

    // 1. malformed: one warning event, nothing else, no report.
    r.handler.handle(b"{not json");
    r.handler.handle(br#"{"v":2}"#);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(r.take(), Vec::<String>::new());
    assert_eq!(
        state_events(ctx),
        vec![
            "ignoring malformed cloud config message",
            "ignoring malformed cloud config message"
        ]
    );

    // 2. applied: uploadBucket → scope → hardware → apply → reapply hooks → one `cloud` report.
    let doc = format!(
        r#"{{"v":1,"rev":11,"uploadBucket":"xorgate-dev-media","scope":{{"organizationId":"org-1","workspaceId":"ws-a"}},"hardware":{CM4},"gnssAntBias":true,"timeSync":{{"stepThresholdSecs":12}}}}"#
    );
    let summary = applied_summary(&parse_cloud_message(&doc).unwrap().config);
    r.handler.handle(doc.as_bytes());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        r.take(),
        vec![
            "hooks.rearm_upload",
            "hooks.hardware_changed",
            "hooks.led_config_applied",
            "hooks.reapply_recording",
            "hooks.reconcile_camera_mount",
            "report cloud rev=11",
        ]
    );
    assert_eq!(
        state_events(ctx)[2..],
        [
            "uploadBucket backfilled: xorgate-dev-media".to_string(),
            "telemetry topic scoped to ws ws-a".to_string(),
            "hardware block updated: rpi-cm4/hw-h264".to_string(),
            format!("cloud config applied: {summary}"),
        ]
    );
    assert!(std::fs::read_to_string(&ctx.paths.kvs_manifest)
        .unwrap()
        .ends_with(r#""channels":[],"uploadBucket":"xorgate-dev-media"}"#));
    assert_eq!(
        std::fs::read_to_string(&ctx.paths.hardware).unwrap(),
        r#"{"v":2,"platform":"rpi-cm4","encoder":"hw-h264","rails":false,"statusLed":null,"imu":"bno085","video":[{"key":"cam0","connector":"csi0","sensor":"imx290","codec":"h264"}]}"#
    );
    assert!(ctx.hardware.borrow().is_some());
    assert_eq!(
        std::fs::read_to_string(&ctx.paths.settings).unwrap(),
        r#"{"mode":"real","gnssAntBias":true,"timeSync":{"stepThresholdSecs":12}}"#
    );
    assert_eq!(r.sync.last_applied_rev(), 11);

    // 3. the retained redelivery: every envelope step a no-op, the apply
    //    SKIPPED, the reapply hooks still run, the cloud still hears.
    let n_events = state_events(ctx).len();
    r.handler.handle(doc.as_bytes());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        r.take(),
        vec![
            "hooks.reapply_recording",
            "hooks.reconcile_camera_mount",
            "report cloud rev=11",
        ]
    );
    assert_eq!(state_events(ctx).len(), n_events);

    // 4. refused: settings.json exists but cannot be read → the apply throws
    //    BEFORE the reapply hooks; the rev does not advance; still reported.
    std::fs::remove_file(&ctx.paths.settings).unwrap();
    std::fs::create_dir(&ctx.paths.settings).unwrap(); // EISDIR = unreadable
    r.handler.handle(br#"{"v":1,"rev":12,"gnssAntBias":false}"#);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(r.take(), vec!["report cloud rev=11"]);
    assert_eq!(
        state_events(ctx).last().unwrap(),
        &format!(
            "cloud config apply failed: settings.json exists but cannot be read — refusing to overwrite it (check owner/permissions on {})",
            ctx.paths.settings.display()
        )
    );
    assert_eq!(r.sync.last_applied_rev(), 11);
    std::fs::remove_dir(&ctx.paths.settings).unwrap();

    // 5. an invalid hardware block: warn + keep current; a rev-less doc applies.
    r.handler.handle(br#"{"v":1,"hardware":{"v":1}}"#);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        r.take(),
        vec![
            "hooks.reapply_recording",
            "hooks.reconcile_camera_mount",
            "report cloud rev=11",
        ]
    );
    assert_eq!(
        state_events(ctx).last().unwrap(),
        "invalid hardware block on config message — keeping current"
    );
    assert!(ctx.hardware.borrow().is_some());
    r.sync.stop();
    let _ = std::fs::remove_dir_all(&r.home);
    assert_eq!(r.sent.lock().unwrap().len(), 4);
}

/// review-wp1 M1, config-message call site: `recording.reapply()` throws inside
/// the try of `worker.ts:864-893` (a failed telemetry finalize). The catch logs
/// `[agent] cloud config apply failed <msg>` and records the warn event
/// `cloud config apply failed: <msg>`. `reconcileCameraMountNow` is skipped.
/// The rev still advances (the apply ran before the throw), and the cloud
/// still hears.
#[tokio::test(start_paused = true)]
async fn a_throwing_recording_reapply_lands_in_cloud_config_apply_failed() {
    let r = rig("wp1-m1-config");
    let ctx = &r.ctx;
    std::fs::write(&ctx.paths.settings, r#"{"mode":"real"}"#).unwrap();
    *r.reapply_err.lock().unwrap() = Some("ENOSPC: no space left on device, write".into());
    r.handler.handle(br#"{"v":1,"rev":21,"gnssAntBias":true}"#);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        r.take(),
        vec![
            "hooks.led_config_applied",
            "hooks.reapply_recording",
            "report cloud rev=21"
        ],
        "the camera-mount reconcile is skipped"
    );
    let events = state_events(ctx);
    assert_eq!(
        events.last().unwrap(),
        "cloud config apply failed: ENOSPC: no space left on device, write"
    );
    let snap = ctx.state.snapshot();
    let last = snap.recent_events.last().unwrap();
    assert_eq!(last.level, ActivityLevel::Warn);
    assert_eq!(r.sync.last_applied_rev(), 21);
    r.sync.stop();
    let _ = std::fs::remove_dir_all(&r.home);
}

/// Offline (`--offline`): `uploadBucket` is never persisted (`worker.ts:796`).
#[tokio::test(start_paused = true)]
async fn upload_bucket_is_skipped_offline() {
    let mut r = rig("wp3-offline");
    let mut env = (*r.ctx.env).clone();
    env.config.offline = true;
    r.ctx.env = Arc::new(env);
    r.handler.ctx = r.ctx.clone();
    let kvs =
        r#"{"deviceId":"d","region":"r","credentialEndpoint":"c","roleAlias":"a","channels":[]}"#;
    std::fs::write(&r.ctx.paths.kvs_manifest, kvs).unwrap();
    r.handler.handle(br#"{"v":1,"uploadBucket":"b"}"#);
    assert_eq!(
        std::fs::read_to_string(&r.ctx.paths.kvs_manifest).unwrap(),
        kvs
    );
    r.sync.stop();
    let _ = std::fs::remove_dir_all(&r.home);
}

// --- the plane through the frozen ConfigPlane interface -------------------------------

/// `start` (`worker.ts:700-746`) + the connect handler's `request("boot") +
/// startHeartbeat()` + `handleConfigMessage` on the plane's own task: a
/// connect and the retained message that follows are ONE `cloud` report;
/// a `ReportRequester` request is a `boot` report; nothing after `stop()`.
#[tokio::test(start_paused = true)]
async fn plane_wiring_through_config_plane() {
    let timeline: Timeline = Arc::new(Mutex::new(vec![]));
    let (ctx, home) = ctx_in("wp3-plane", Arc::new(xorgate_agent::ctx::NoopHooks));
    std::fs::write(&ctx.paths.settings, r#"{"mode":"real"}"#).unwrap();
    let link = FakeLink::new(timeline.clone());
    link.set(LinkState::Connected);
    ctx.state.set_connection(ConnectionState::Connected, 0);
    let (requester, report_rx) = ReportRequester::channel();
    let scope = ScopeHandle::new(
        "dev-wp3",
        &ctx.env.config.topics.telemetry,
        ctx.state.clone(),
        requester.clone(),
    );
    let plane = config_plane::start(
        &ctx,
        ConfigPlaneDeps {
            mqtt: MqttHandle::new(link.clone()),
            scope,
            reports: requester.clone(),
            report_rx,
            obd: None,
        },
    );
    plane.on_connect();
    tokio::time::sleep(Duration::from_millis(500)).await;
    plane.on_message(br#"{"v":1,"rev":3,"gnssAntBias":true}"#.to_vec());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let reports = |link: &FakeLink| -> Vec<Value> {
        link.publishes
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.0.ends_with("/config/reported"))
            .map(|p| serde_json::from_slice(&p.1).unwrap())
            .collect()
    };
    let r = reports(&link);
    assert_eq!(r.len(), 1);
    assert_eq!(
        (r[0]["origin"].as_str(), r[0]["rev"].as_u64()),
        (Some("cloud"), Some(3))
    );
    assert_eq!(r[0]["effective"]["agent"]["mode"], "real");
    assert_eq!(
        r[0]["effective"]["telemetryScope"]["topic"],
        ctx.env.config.topics.telemetry
    );
    assert_eq!(r[0]["status"]["gnssAntBias"]["state"], "adjusted");
    requester.request(ReportOrigin::Boot);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let r = reports(&link);
    assert_eq!(r.len(), 2);
    assert_eq!(r[1]["origin"], "boot");
    plane.stop();
    requester.request(ReportOrigin::Boot);
    plane.on_connect();
    tokio::time::sleep(Duration::from_millis(5000)).await;
    assert_eq!(reports(&link).len(), 2);
    let sync = parse_config_sync_state(Some(
        &std::fs::read_to_string(&ctx.paths.config_sync).unwrap(),
    ));
    assert_eq!(sync.last_applied_rev, 3);
    ctx.cancel.cancel();
    let _ = std::fs::remove_dir_all(&home);
}

/// CF33: the boot `buildUploadOptions()` runs without a manifest (logs
/// `[agent] segment upload disabled (no kvs.json manifest)`) and offline.
#[test]
fn boot_upload_options_runs() {
    let (ctx, home) = ctx_in("wp3-upload", Arc::new(xorgate_agent::ctx::NoopHooks));
    config_plane::boot_upload_options(&ctx);
    let _ = std::fs::remove_dir_all(&home);
}

// --- Phase 5b: the `unheldNamespaces` rev-guard exception (worker.ts:938-971 @ 09da38d)

/// A fake OBD collector: counts `reapply()` (`source.obdReapply?.()`).
struct FakeObd(Arc<std::sync::atomic::AtomicUsize>);

impl xorgate_agent::ctx::ObdControl for FakeObd {
    fn status(&self) -> xorgate_core::telemetry::ObdStatus {
        unreachable!("not read by the config plane")
    }
    fn report(&self) -> Value {
        serde_json::json!({})
    }
    fn reapply(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    fn scan(&self) -> xorgate_agent::ctx::BoxFut<'static, xorgate_core::obd::ObdScanResult> {
        unreachable!()
    }
    fn forget(&self) -> xorgate_agent::ctx::BoxFut<'static, Option<String>> {
        unreachable!()
    }
}

/// An already-applied rev carrying a namespace this build never held (absent
/// from settings.json AND the baseline) adopts EXACTLY that key: the log line,
/// a `cloud config applied:` summary of the SUBSET, the LED flash, the reapply
/// hooks and the report. A redelivery is then plain rev-guarded; `null` (an
/// all-invalid object) is never "carried"; no baseline → no adoption.
#[tokio::test(start_paused = true)]
async fn unheld_namespaces_are_adopted_under_an_applied_rev() {
    let mut r = rig("p5b-unheld");
    let ctx = r.ctx.clone();
    std::fs::write(&ctx.paths.settings, r#"{"mode":"real","gnssAntBias":true}"#).unwrap();
    std::fs::write(
        &ctx.paths.config_sync,
        r#"{"lastAppliedRev":51,"lastReportTs":0,"namespaces":{"gnssAntBias":true}}"#,
    )
    .unwrap();
    // Rebuild the plane over the seeded sync file (the state loads at construction).
    let reapplies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    r.sync = rebuild_sync(&r);
    r.handler = MessageHandler {
        ctx: ctx.clone(),
        sync: r.sync.clone(),
        scope: r.handler.scope.clone(),
        obd: Some(Arc::new(FakeObd(reapplies.clone()))),
    };
    let doc = r#"{"v":1,"rev":51,"gnssAntBias":true,"obd":{"enabled":true,"adapterMac":"10:21:3e:4f:33:65"},"timeSync":{}}"#;
    r.handler.handle(doc.as_bytes());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        r.take(),
        vec![
            "hooks.led_config_applied",
            "hooks.reapply_recording",
            "hooks.reconcile_camera_mount",
            "report cloud rev=51",
        ]
    );
    assert_eq!(reapplies.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(&ctx.paths.settings).unwrap(),
        r#"{"mode":"real","gnssAntBias":true,"obd":{"enabled":true,"adapterMac":"10:21:3E:4F:33:65"}}"#
    );
    assert_eq!(
        state_events(&ctx).last().unwrap(),
        r#"cloud config applied: {"obd":{"enabled":true,"adapterMac":"10:21:3E:4F:33:65"}}"#
    );
    assert_eq!(
        xorgate_core::config_report::unheld_adopt_log(Some(51), &["obd"]),
        "[agent] cloud config rev 51 already applied, but it carries namespace(s) this build never held: obd — adopting"
    );
    // `timeSync: {}` parsed to null: not carried, not adopted, rev unchanged.
    assert_eq!(r.sync.last_applied_rev(), 51);

    // Redelivery: obd is held now → plain "already applied", no write.
    r.handler.handle(doc.as_bytes());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        r.take(),
        vec![
            "hooks.reapply_recording",
            "hooks.reconcile_camera_mount",
            "report cloud rev=51",
        ]
    );
    assert_eq!(reapplies.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn no_adoption_without_a_baseline() {
    let mut r = rig("p5b-unheld-nobase");
    let ctx = r.ctx.clone();
    std::fs::write(&ctx.paths.settings, r#"{"mode":"real"}"#).unwrap();
    std::fs::write(
        &ctx.paths.config_sync,
        r#"{"lastAppliedRev":51,"lastReportTs":0}"#,
    )
    .unwrap();
    r.sync = rebuild_sync(&r);
    r.handler = MessageHandler {
        ctx: ctx.clone(),
        sync: r.sync.clone(),
        scope: r.handler.scope.clone(),
        obd: None,
    };
    r.handler
        .handle(br#"{"v":1,"rev":51,"obd":{"enabled":true}}"#);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        r.take(),
        vec![
            "hooks.reapply_recording",
            "hooks.reconcile_camera_mount",
            "report cloud rev=51",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&ctx.paths.settings).unwrap(),
        r#"{"mode":"real"}"#
    );
}

/// A live hardware block that adds or drops `peripherals.obd` only warns
/// (the collector is built once from the BOOT block).
#[tokio::test(start_paused = true)]
async fn a_live_obd_peripheral_change_asks_for_a_restart() {
    let r = rig("p5b-hw-obd");
    let ctx = &r.ctx;
    std::fs::write(&ctx.paths.settings, r#"{"mode":"real"}"#).unwrap();
    let with_obd = r#"{"v":1,"hardware":{"v":2,"platform":"rpi-cm4","encoder":"hw-h264","rails":false,"statusLed":null,"imu":"bno085","video":[],"peripherals":{"obd":{"adapter":"elm327","transport":"bt-spp"}}}}"#;
    r.handler.handle(with_obd.as_bytes());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let ev = state_events(ctx);
    let i = ev
        .iter()
        .position(|m| m == "hardware block updated: rpi-cm4/hw-h264")
        .expect("updated");
    assert_eq!(
        ev[i + 1],
        "hardware block changed peripherals.obd — restart the agent to apply"
    );
}

/// A fresh `ConfigSync` for an existing rig (re-reads config-sync.json),
/// publishing into the rig's timeline like `rig()` does.
fn rebuild_sync(r: &Rig) -> ConfigSync {
    let (requester, _) = ReportRequester::channel();
    let (_r2, rx2) = ReportRequester::channel();
    let link = FakeLink::new(r.timeline.clone());
    link.set(LinkState::Connected);
    let deps = ConfigPlaneDeps {
        mqtt: MqttHandle::new(link),
        scope: r.handler.scope.clone(),
        reports: requester,
        report_rx: rx2,
        obd: None,
    };
    let mut o: ConfigSyncOptions = options_for(&r.ctx, &deps);
    let (s, t) = (r.sent.clone(), r.timeline.clone());
    o.publish = Arc::new(move |_topic, payload| {
        let v: Value = serde_json::from_slice(&payload).unwrap();
        push(
            &t,
            format!("report {} rev={}", v["origin"].as_str().unwrap(), v["rev"]),
        );
        s.lock().unwrap().push(v);
        Ok(())
    });
    ConfigSync::new(o)
}
