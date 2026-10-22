//! Phase 5b WP2: the OBD collector runtime over the traced supervisor.
//!
//! - registry gating (`obd.test.ts:558` "without peripherals.obd the
//!   collector list and telemetry output are byte-identical"; `:571` "with
//!   peripherals.obd the obd group appears with link (mock helper stream)");
//! - the I4 contract: `scan()` and `forget()` do their synchronous part (the
//!   start event / `goOff` + its event + SIGINT) at CALL time, before the
//!   future is polled; a second scan joins the first;
//! - the publish tick's state.json `obd` + the report edge (`obdReportDue`),
//!   before the clock gate (`worker.ts:1395-1405` @ 09da38d).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use indexmap::IndexMap;
use tokio::sync::{mpsc, oneshot};
use xorgate_agent::collectors::obd::ObdCollector;
use xorgate_agent::collectors::{Backends, Collector, Collectors, IoCapabilities};
use xorgate_agent::ctx::{
    AgentClock, BoxFut, GroupSample, NoopHooks, ObdControl, ReportRequester, TelemetrySource,
};
use xorgate_agent::mqtt_plane::{MqttHandle, PublishTick};
use xorgate_agent::scope::ScopeHandle;
use xorgate_core::config_report::ReportOrigin;
use xorgate_core::obd::{ObdEnv, ObdHelperConfig, ObdScanCandidate, ObdScanResult, ObdSettings};
use xorgate_core::telemetry::{
    MetricGroup, MetricLeaf, Metrics, ObdStatus, TelemetryMedia, TelemetryReading,
};
use xorgate_core::types::{GnssView, GpsClockReading, PowerSampleReading};
use xorgate_hal::io::null_tape;
use xorgate_hal::obd_io::{ObdFut, ObdHelperEvent, ObdHelperHandle, ObdIo, RealObdIo};

fn ctx(tag: &str) -> xorgate_agent::ctx::AgentCtx {
    let home = scratch(tag);
    let mut c = test_ctx(&home, "dev-obd", Arc::new(NoopHooks), null_tape());
    c.clock = Arc::new(AgentClock::following_tokio(1_760_000_000_000));
    c
}

fn env_adapter() -> ObdEnv {
    ObdEnv {
        adapter: Some("AA:BB:CC:DD:EE:FF".into()),
        ..Default::default()
    }
}

fn no_settings() -> Arc<dyn Fn() -> Option<ObdSettings> + Send + Sync> {
    Arc::new(|| None)
}

fn events(c: &xorgate_agent::ctx::AgentCtx) -> Vec<String> {
    state_events(c)
}

// ---------------------------------------------------------------------------
// gating
// ---------------------------------------------------------------------------

struct NoImu;
impl xorgate_agent::collectors::imu::ImuIo for NoImu {
    fn detect<'a>(
        &'a self,
        _tape: &'a dyn xorgate_hal::io::TapeSink,
    ) -> BoxFut<'a, xorgate_core::imu::ImuDetect> {
        Box::pin(async { xorgate_core::imu::unavailable_detect("no i2c") })
    }
    fn spawn(
        &self,
        _spec: &xorgate_hal::child::ChildSpec,
    ) -> (
        xorgate_hal::child::ChildHandle,
        mpsc::UnboundedReceiver<xorgate_hal::child::ChildEvent>,
    ) {
        unreachable!("no IMU")
    }
}

/// `obd.test.ts:558`: no block / a block without `peripherals.obd` → no obd
/// collector, no `obd` group, `source.obd()` is `None`.
#[tokio::test(start_paused = true)]
async fn without_peripherals_obd_there_is_no_obd_collector() {
    for raw in [
        None,
        Some(
            serde_json::json!({"v": 2, "platform": "rpi-cm5", "encoder": "hw-h264", "rails": true,
            "statusLed": null, "imu": null, "video": []}),
        ),
        Some(
            serde_json::json!({"v": 2, "platform": "rpi-cm5", "encoder": "hw-h264", "rails": true,
            "statusLed": null, "imu": null, "video": [], "peripherals": {}}),
        ),
    ] {
        let block = raw.map(|r| xorgate_core::hardware_block::parse_hardware_block(&r).unwrap());
        let caps = IoCapabilities::from_hardware(block.as_ref());
        assert!(!caps.obd, "{block:?}");
        let c = ctx("obd-gate-none");
        let src = Collectors::new(
            &c,
            IoCapabilities {
                imu: false,
                gps: false,
                signal: false,
                system_rails: None,
                obd: caps.obd,
            },
            Backends {
                nmea: Arc::new(xorgate_hal::nmea_tty::TtyOpener),
                imu: Arc::new(NoImu),
            },
        )
        .await;
        let r = src.read().await.unwrap();
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"metrics":{},"media":{"kvs":{"value":false},"recording":{"value":false},"streams":[]}}"#
        );
        assert!(src.sample_groups().await.is_empty());
        assert!(src.obd().is_none());
        src.stop();
    }
    let with = xorgate_core::hardware_block::parse_hardware_block(&serde_json::json!({
        "v": 2, "platform": "rpi-cm5", "encoder": "hw-h264", "rails": true, "statusLed": null,
        "imu": null, "video": [], "peripherals": {"obd": {"adapter": "elm327", "transport": "bt-spp"}}
    }))
    .unwrap();
    assert!(IoCapabilities::from_hardware(Some(&with)).obd);
}

/// `obd.test.ts:571`: with the peripheral (mock helper stream, `XORGATE_MOCK`)
/// the group appears with `link`, goes live, stop → `stopped`.
#[tokio::test(start_paused = true)]
async fn with_peripherals_obd_the_mock_stream_goes_live() {
    let c = ctx("obd-mock");
    let io = Arc::new(RealObdIo {
        python_dir: "/nonexistent".into(),
        mock: true,
    });
    let obd = Arc::new(ObdCollector::with(&c, io, no_settings(), env_adapter()));
    let first = obd.read().await;
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        r#"{"link":{"value":0}}"#
    );
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let later = obd.read().await;
    assert_eq!(later["link"], MetricLeaf::new(2.0));
    assert!(later["rpm"].value.0 > 800.0);
    assert_eq!(later["rpm"].unit.as_deref(), Some("rpm"));
    assert_eq!(obd.status().state, "live");
    assert_eq!(obd.report()["profile"]["protocol"]["dpn"], "A6");
    assert!(
        events(&c).iter().any(|e| e.starts_with("obd: live")),
        "{:?}",
        events(&c)
    );
    // The recorder view: the same mapping + a stamp.
    let s = obd.sample().await.unwrap();
    assert_eq!(s.fields["link"], MetricLeaf::new(2.0));
    obd.stop();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(obd.status().state, "stopped");
    assert_eq!(obd.status().link, 0);
}

// ---------------------------------------------------------------------------
// I4: scan / forget synchronous halves
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeIo {
    spawned: Mutex<Vec<ObdHelperConfig>>,
    signals: Arc<Mutex<Vec<&'static str>>>,
    exits: Mutex<Vec<mpsc::UnboundedSender<ObdHelperEvent>>>,
    scan_gate: Mutex<Option<oneshot::Receiver<()>>>,
    scans: AtomicUsize,
    unbonded: Mutex<Vec<String>>,
}

impl ObdIo for FakeIo {
    fn spawn(
        &self,
        cfg: &ObdHelperConfig,
    ) -> (ObdHelperHandle, mpsc::UnboundedReceiver<ObdHelperEvent>) {
        self.spawned.lock().unwrap().push(cfg.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        self.exits.lock().unwrap().push(tx);
        (ObdHelperHandle::fake(self.signals.clone()), rx)
    }
    fn scan(&self, seconds: f64) -> ObdFut<Result<Vec<ObdScanCandidate>, String>> {
        assert_eq!(seconds, 25.0);
        self.scans.fetch_add(1, Ordering::SeqCst);
        let gate = self.scan_gate.lock().unwrap().take();
        Box::pin(async move {
            if let Some(g) = gate {
                let _ = g.await;
            }
            Ok(vec![ObdScanCandidate {
                mac: "00:1D:A5:68:98:8B".into(),
                name: "V-LINK".into(),
                rssi: Some(-61.0),
            }])
        })
    }
    fn unbond(&self, mac: String) -> ObdFut<Result<(), String>> {
        self.unbonded.lock().unwrap().push(mac);
        Box::pin(async { Err("bluetoothctl not found".into()) })
    }
}

#[tokio::test(start_paused = true)]
async fn scan_emits_its_start_event_at_call_time_and_joins() {
    let c = ctx("obd-scan");
    let io = Arc::new(FakeIo::default());
    let (open, gate) = oneshot::channel();
    *io.scan_gate.lock().unwrap() = Some(gate);
    let obd = ObdCollector::with(&c, io.clone(), no_settings(), env_adapter());
    obd.read().await;
    let f1 = obd.scan();
    // Before any poll of the future: the event is in state.json already.
    assert_eq!(
        events(&c).last().unwrap(),
        "obd: scanning for Bluetooth adapters (25 s, never pairs)"
    );
    let f2 = obd.scan();
    let n = events(&c).len();
    open.send(()).unwrap();
    let (a, b) = tokio::join!(f1, f2);
    assert_eq!(a, b);
    assert_eq!(io.scans.load(Ordering::SeqCst), 1, "the second scan joined");
    assert_eq!(a.candidates.len(), 1);
    assert_eq!(
        events(&c)[n..],
        ["obd: scan done: 1 named device(s)".to_string()]
    );
    assert_eq!(obd.report()["scan"], serde_json::to_value(&a).unwrap());
    assert!(
        io.signals.lock().unwrap().is_empty(),
        "the helper kept running"
    );
}

#[tokio::test(start_paused = true)]
async fn forget_goes_off_at_call_time_then_unbonds() {
    let c = ctx("obd-forget");
    let io = Arc::new(FakeIo::default());
    let settings = ObdSettings {
        enabled: Some(true),
        adapter_mac: Some(Some("10:21:3E:4F:33:65".into())),
        ..Default::default()
    };
    let obd = ObdCollector::with(
        &c,
        io.clone(),
        Arc::new(move || Some(settings.clone())),
        ObdEnv::default(),
    );
    obd.read().await;
    assert_eq!(io.spawned.lock().unwrap().len(), 1);
    let fut = obd.forget();
    // Synchronous half: goOff (SIGINT + not_configured + the event).
    assert_eq!(io.signals.lock().unwrap().as_slice(), ["SIGINT"]);
    assert_eq!(obd.status().state, "not_configured");
    assert_eq!(
        events(&c).last().unwrap(),
        "obd: adapter 10:21:3E:4F:33:65 forgotten; helper stopped, bond removed, waiting for new obd settings"
    );
    assert_eq!(fut.await.as_deref(), Some("10:21:3E:4F:33:65"));
    assert_eq!(
        io.unbonded.lock().unwrap().as_slice(),
        ["10:21:3E:4F:33:65"]
    );
    assert_eq!(
        events(&c).last().unwrap(),
        "obd: forget: bond removal failed for 10:21:3E:4F:33:65: bluetoothctl not found"
    );
    // The helper's exit after the SIGINT keeps it off; the 1 s poll does not respawn.
    let tx = io.exits.lock().unwrap()[0].clone();
    tx.send(ObdHelperEvent::Exit(Some(0))).unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    obd.read().await;
    assert_eq!(io.spawned.lock().unwrap().len(), 1);
    assert_eq!(obd.status().state, "not_configured");
}

/// A quick helper exit backs off 2 s on the (paused) tokio clock, then respawns.
#[tokio::test(start_paused = true)]
async fn the_runtime_drives_the_restart_timer() {
    let c = ctx("obd-timer");
    let io = Arc::new(FakeIo::default());
    let obd = ObdCollector::with(&c, io.clone(), no_settings(), env_adapter());
    obd.read().await;
    let tx = io.exits.lock().unwrap()[0].clone();
    tx.send(ObdHelperEvent::Stderr("ImportError: x".into()))
        .unwrap();
    tx.send(ObdHelperEvent::Exit(Some(1))).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(obd.status().state, "retrying");
    tokio::time::sleep(Duration::from_millis(1_980)).await;
    assert_eq!(io.spawned.lock().unwrap().len(), 1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(io.spawned.lock().unwrap().len(), 2);
    assert_eq!(obd.status().state, "connecting");
}

// ---------------------------------------------------------------------------
// publish tick: state.json obd + the report edge
// ---------------------------------------------------------------------------

struct StatusSeq(Mutex<Vec<ObdStatus>>);
impl ObdControl for StatusSeq {
    fn status(&self) -> ObdStatus {
        let mut v = self.0.lock().unwrap();
        if v.len() > 1 {
            v.remove(0)
        } else {
            v[0].clone()
        }
    }
    fn report(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn reapply(&self) {}
    fn scan(&self) -> BoxFut<'static, ObdScanResult> {
        unreachable!()
    }
    fn forget(&self) -> BoxFut<'static, Option<String>> {
        unreachable!()
    }
}

struct ObdSource(Option<Arc<dyn ObdControl>>);
impl TelemetrySource for ObdSource {
    fn read(&self) -> BoxFut<'_, Result<TelemetryReading, String>> {
        Box::pin(async {
            let mut g = MetricGroup::new();
            g.insert("link".into(), MetricLeaf::new(0.0));
            let mut metrics = Metrics::new();
            metrics.insert("obd".into(), g);
            Ok(TelemetryReading {
                metrics,
                media: TelemetryMedia::idle(),
            })
        })
    }
    fn sample_groups(&self) -> BoxFut<'_, IndexMap<String, GroupSample>> {
        Box::pin(async { IndexMap::new() })
    }
    fn sample_power(&self) -> BoxFut<'_, Option<PowerSampleReading>> {
        Box::pin(async { None })
    }
    fn gps_clock(&self) -> Option<GpsClockReading> {
        None
    }
    fn gnss_view(&self) -> Option<GnssView> {
        None
    }
    fn stop(&self) {}
    fn obd(&self) -> Option<Arc<dyn ObdControl>> {
        self.0.clone()
    }
}

fn st(state: &str, link: u8) -> ObdStatus {
    ObdStatus {
        state: state.into(),
        link,
        req_per_sec: None,
        errors: 0,
        forbidden: 0,
        voltage: None,
        detail: None,
        since: 0.0,
        helper_version: None,
        adapter: Some(None),
        profile: Some(None),
        voltage_low_events: Some(0),
        audit_seq: Some(0),
        last_stop: Some(None),
    }
}

#[tokio::test(start_paused = true)]
async fn the_publish_tick_writes_state_obd_and_requests_a_report_on_the_edge() {
    let c = ctx("obd-tick");
    c.state
        .set_recording(xorgate_core::telemetry::RecordingStatus::all_off());
    let timeline: Timeline = Arc::new(Mutex::new(vec![]));
    let link = FakeLink::new(timeline);
    let mqtt = MqttHandle::new(link);
    let (requester, mut reports) = ReportRequester::channel();
    let scope = ScopeHandle::new(
        "dev-obd",
        &c.env.config.topics.telemetry,
        c.state.clone(),
        requester,
    );
    let seq = vec![
        st("connecting", 0),
        st("adapter_not_found", 0),
        st("probing", 1),
        st("live", 1),
    ];
    let tick = PublishTick {
        ctx: c.clone(),
        mqtt,
        scope,
        source: Arc::new(ObdSource(Some(Arc::new(StatusSeq(Mutex::new(seq)))))),
        seq: Default::default(),
        obd_last: Default::default(),
    };
    let mut due = vec![];
    for _ in 0..4 {
        tick.run().await;
        due.push(reports.try_recv().ok());
        assert!(reports.try_recv().is_err());
    }
    // null → status (appears), no move, link 0 → 1, no move.
    assert_eq!(
        due,
        [
            Some(ReportOrigin::Boot),
            None,
            Some(ReportOrigin::Boot),
            None
        ]
    );
    let snap = c.state.snapshot();
    assert_eq!(snap.obd.as_ref().unwrap().state, "live");
    // state.json key order: `obd` after `recording` (the TS insertion order).
    let v = serde_json::to_value(&snap).unwrap();
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    let (r, o) = (
        keys.iter().position(|k| *k == "recording").unwrap(),
        keys.iter().position(|k| *k == "obd").unwrap(),
    );
    assert_eq!(o, r + 1, "{keys:?}");

    // Without the peripheral: no `obd` key, never a request.
    let c2 = ctx("obd-tick-none");
    let (requester, mut reports) = ReportRequester::channel();
    let tick = PublishTick {
        ctx: c2.clone(),
        mqtt: MqttHandle::new(FakeLink::new(Arc::new(Mutex::new(vec![])))),
        scope: ScopeHandle::new(
            "dev-obd",
            &c2.env.config.topics.telemetry,
            c2.state.clone(),
            requester,
        ),
        source: Arc::new(ObdSource(None)),
        seq: Default::default(),
        obd_last: Default::default(),
    };
    tick.run().await;
    assert!(reports.try_recv().is_err());
    let v = serde_json::to_value(c2.state.snapshot()).unwrap();
    assert!(
        v.get("obd").is_none(),
        "no top-level `obd` without the peripheral"
    );
}
