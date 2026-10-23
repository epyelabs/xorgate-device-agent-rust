//! Test harness for the spine: an `AgentCtx` over a scratch home, a fake MQTT
//! transport, and recording fakes for the planes, the source and the hooks.
//! Everything appends to ONE shared timeline so tests can assert ORDER.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use xorgate_agent::ctx::{
    AgentClock, AgentCtx, BoxFut, CommandPlane, ConfigPlane, GroupSample, PhaseHooks,
    TelemetrySource,
};
use xorgate_agent::env::{paths, AgentEnv};
use xorgate_agent::mqtt_plane::{AckOutcome, MqttTransport, PublishDone, QoS};
use xorgate_agent::state::StateHandle;
use xorgate_cloud::mqtt::ConnectionState as LinkState;
use xorgate_core::agent_config::{load_config, ConfigInputs};
use xorgate_core::build_info::BuildInfo;
use xorgate_core::telemetry::{
    ConnectionState, JsNumber, MetricGroup, MetricLeaf, Metrics, StateSnapshot, TelemetryMedia,
    TelemetryReading,
};
use xorgate_core::types::{GnssView, GpsClockReading, PowerSampleReading};
use xorgate_hal::io::TapeSink;

pub type Timeline = Arc<Mutex<Vec<String>>>;

pub fn push(t: &Timeline, s: impl Into<String>) {
    t.lock().unwrap().push(s.into());
}

pub fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-agent-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An `AgentEnv` for device `dev`, endpoint `ep`, real mode, 1 Hz, home = `home`.
pub fn test_env(home: &Path, device: &str) -> AgentEnv {
    let env = |k: &str| match k {
        "DEVICE_ID" => Some(device.to_string()),
        "IOT_DATA_ENDPOINT" => Some("ep".to_string()),
        "MODE" => Some("real".to_string()),
        "HARDWARE_MANIFEST" => Some(home.join("hardware.json").display().to_string()),
        _ => None,
    };
    let config = load_config(&ConfigInputs {
        argv: &[],
        env: &env,
        settings_mode: None,
        cwd: "/",
    })
    .unwrap();
    AgentEnv {
        config,
        home: home.to_path_buf(),
        self_managed: true,
        upload_bucket_override: None,
        argv: vec![],
    }
}

/// The ctx the worker would build, with a wall clock that follows tokio's
/// (pausable) clock from `wall0`.
pub fn test_ctx(
    home: &Path,
    device: &str,
    hooks: Arc<dyn PhaseHooks>,
    tape: Arc<dyn TapeSink>,
) -> AgentCtx {
    let env = test_env(home, device);
    let p = paths(&env);
    let cfg = &env.config;
    let state = StateHandle::new(
        StateSnapshot {
            v: 1,
            written_by: "test".into(),
            device_id: cfg.device_id.clone(),
            mode: cfg.mode,
            media_enabled: Some(true),
            rate_hz: JsNumber(1.0),
            endpoint: cfg.endpoint.clone(),
            offline: false,
            pid: 1,
            started_at: 0,
            last_write_at: 0,
            connection: ConnectionState::Connecting,
            connection_since: 0,
            seq: 0,
            published: 0,
            publish_errors: 0,
            last_publish_at: None,
            last_error: None,
            latest: None,
            recent_events: vec![],
            recording: None,
            obd: None,
        },
        p.state.clone(),
    );
    let (hw, _) = watch::channel(None);
    AgentCtx {
        env: Arc::new(env),
        paths: Arc::new(p),
        build: Arc::new(BuildInfo {
            version: "test".into(),
            channel: "dev-local".into(),
            built_at: None,
            package_version: "0.0.1".into(),
        }),
        tape,
        state,
        hooks,
        hardware: Arc::new(hw),
        clock: Arc::new(AgentClock::following_tokio(0)),
        cancel: CancellationToken::new(),
        tracker: TaskTracker::new(),
    }
}

/// A transport whose link state the test drives; every call lands on the timeline.
pub struct FakeLink {
    pub timeline: Timeline,
    pub state: watch::Sender<LinkState>,
    pub publishes: Mutex<Vec<(String, Vec<u8>, QoS)>>,
    /// What `publish_acked` resolves to; `None` = never (the caller's timeout wins).
    pub ack: Mutex<Option<AckOutcome>>,
    /// `Some`: `publish_notify` callbacks are HELD here (like a real client's
    /// offline queue) until the test fires them; `None`: they fire at once.
    pub held: Mutex<Option<Vec<tokio::sync::oneshot::Sender<PublishDone>>>>,
}

impl FakeLink {
    pub fn new(timeline: Timeline) -> Arc<Self> {
        let (state, _) = watch::channel(LinkState::Connecting);
        Arc::new(Self {
            timeline,
            state,
            publishes: Mutex::new(vec![]),
            ack: Mutex::new(Some(AckOutcome::Acked)),
            held: Mutex::new(None),
        })
    }

    pub fn set(&self, s: LinkState) {
        self.state.send_replace(s);
    }

    pub fn topics(&self) -> Vec<String> {
        self.publishes
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.0.clone())
            .collect()
    }
}

fn short(topic: &str) -> &str {
    topic
        .rsplit_once("/devices/")
        .map(|(_, r)| r)
        .unwrap_or(topic)
}

impl MqttTransport for FakeLink {
    fn publish(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        _retain: bool,
    ) -> Result<(), String> {
        let label = if topic.ends_with("/status") {
            let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            format!("publish status online:{}", v["online"])
        } else {
            format!("publish {}", short(topic))
        };
        push(&self.timeline, label);
        self.publishes
            .lock()
            .unwrap()
            .push((topic.to_string(), payload, qos));
        Ok(())
    }

    fn publish_acked(
        &self,
        topic: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> BoxFut<'static, AckOutcome> {
        push(&self.timeline, format!("publish_acked {}", short(topic)));
        self.publishes
            .lock()
            .unwrap()
            .push((topic.to_string(), payload, QoS::AtLeastOnce));
        let ack = self.ack.lock().unwrap().clone();
        Box::pin(async move {
            match ack {
                Some(a) => a,
                None => {
                    tokio::time::sleep(timeout).await;
                    AckOutcome::TimedOut
                }
            }
        })
    }

    fn subscribe(&self, topic: &str, _qos: QoS) -> Result<(), String> {
        push(&self.timeline, format!("subscribe {}", short(topic)));
        Ok(())
    }

    fn publish_notify(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> BoxFut<'static, PublishDone> {
        let r = self.publish(topic, payload, qos, retain);
        let mut held = self.held.lock().unwrap();
        match held.as_mut() {
            Some(q) => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                q.push(tx);
                Box::pin(async move { rx.await.ok().flatten() })
            }
            None => Box::pin(async move { Some(r) }),
        }
    }

    fn connection_watch(&self) -> watch::Receiver<LinkState> {
        self.state.subscribe()
    }
}

pub struct FakeConfig(pub Timeline);
impl ConfigPlane for FakeConfig {
    fn on_connect(&self) {
        push(&self.0, "config.on_connect");
    }
    fn on_message(&self, payload: Vec<u8>) {
        push(
            &self.0,
            format!("config.on_message {}", String::from_utf8_lossy(&payload)),
        );
    }
    fn stop(&self) {
        push(&self.0, "config.stop");
    }
}

pub struct FakeCommands(pub Timeline);
impl CommandPlane for FakeCommands {
    fn on_message(&self, payload: Vec<u8>) {
        push(
            &self.0,
            format!("commands.on_message {}", String::from_utf8_lossy(&payload)),
        );
    }
}

/// A source with one `system.cpu_usage` leaf; `fail` makes `read()` error.
pub struct FakeSource {
    pub timeline: Timeline,
    pub fail: Mutex<Option<String>>,
}
impl TelemetrySource for FakeSource {
    fn read(&self) -> BoxFut<'_, Result<TelemetryReading, String>> {
        Box::pin(async move {
            if let Some(e) = self.fail.lock().unwrap().clone() {
                return Err(e);
            }
            let mut g = MetricGroup::new();
            g.insert("cpu_usage".into(), MetricLeaf::with_unit(12.5, "%"));
            let mut metrics = Metrics::new();
            metrics.insert("system".into(), g);
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
    fn stop(&self) {
        push(&self.timeline, "source.stop");
    }
}

pub struct FakeHooks(pub Timeline);
impl PhaseHooks for FakeHooks {
    fn notify_connectivity(&self, online: bool) {
        push(&self.0, format!("hooks.notify_connectivity {online}"));
    }
    fn led_stop(&self) {
        push(&self.0, "hooks.led_stop");
    }
    fn recording_stop(&self) -> Result<(), String> {
        push(&self.0, "hooks.recording_stop");
        Ok(())
    }
    fn final_recording_status(&self) -> Option<xorgate_core::telemetry::RecordingStatus> {
        push(&self.0, "hooks.final_recording_status");
        None
    }
}

#[derive(Default)]
pub struct FakeTape(pub Mutex<Vec<String>>);
impl TapeSink for FakeTape {
    fn enabled(&self) -> bool {
        true
    }
    fn mqtt_in(&self, topic: &str, payload: &[u8], retain: bool, qos: u8) {
        self.0.lock().unwrap().push(format!(
            "mqtt_in {topic} {} retain={retain} qos={qos}",
            String::from_utf8_lossy(payload)
        ));
    }
    fn conn(&self, connected: bool) {
        self.0.lock().unwrap().push(format!("conn {connected}"));
    }
}

/// The `msg` of every state.json event, oldest first (the file on disk).
pub fn state_events(ctx: &AgentCtx) -> Vec<String> {
    ctx.state
        .snapshot()
        .recent_events
        .iter()
        .map(|e| e.msg.clone())
        .collect()
}
