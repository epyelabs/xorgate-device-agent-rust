//! The worker — port of the Phase 0 slice of `src/worker.ts`: boot, pidfile +
//! state.json, MQTT connect / status / LWT, config + command subscriptions
//! (logged, not acted on), the 1 Hz publish loop, systemd notify/watchdog,
//! graceful SIGTERM/SIGINT (`online:false`, ≤3 s, exit 0), SIGHUP ignored.
//!
//! `--offline`: no network; every would-be payload is printed to stdout once
//! per tick (acceptance check A2).

use std::os::unix::fs::DirBuilderExt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::{interval, Interval, MissedTickBehavior};
use tracing::{error, info, warn};
use xorgate_cloud::mqtt::{ConnectionState, MqttClient, MqttConfig, MqttEvent, QoS, Will};
use xorgate_core::telemetry::{JsNumber, Metrics, StatusPayload, TelemetryPayload};
use xorgate_core::topics::Topics;
use xorgate_hal::system::SystemSampler;

use crate::env::AgentEnv;
use crate::state::{StateSnapshot, StateStore};
use crate::{build_info, env, logging, pidfile};

/// How long the graceful stop waits for the final `online:false` to reach the wire.
const ACK_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);
const WRITTEN_BY_PREFIX: &str = "xorgate-device-agent-rs@";

pub struct RunArgs {
    pub offline: bool,
}

pub fn main(args: RunArgs) -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[agent] tokio runtime: {e}");
            return 1;
        }
    };
    match rt.block_on(run(args)) {
        Ok(code) => code,
        Err(e) => {
            error!("[agent] fatal: {e:#}");
            eprintln!("[agent] fatal: {e:#}");
            1
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sd_notify(states: &[sd_notify::NotifyState]) {
    // Harmless when not under systemd (NOTIFY_SOCKET unset).
    let _ = sd_notify::notify(states);
}

/// Per-process publish state: `seq` starts at 0 and increments per tick.
struct Ticker {
    device_id: String,
    seq: u64,
    sampler: SystemSampler,
}

impl Ticker {
    async fn payload(&mut self) -> TelemetryPayload {
        let system = self.sampler.read().await;
        let mut metrics = Metrics::new();
        if !system.is_empty() {
            metrics.insert("system".into(), system);
        }
        let seq = self.seq;
        self.seq += 1;
        TelemetryPayload::new(&self.device_id, now_ms(), seq, metrics)
    }
}

async fn run(args: RunArgs) -> Result<i32> {
    let log_sink = logging::init();
    let cfg = env::load(args.offline)?;
    let build = build_info::load();
    info!(
        "[agent] xorgate-rs {} starting (mode={}, home={}, log={log_sink}, rust phase 0)",
        build.format(),
        cfg.mode,
        cfg.home.display()
    );

    // A10: gstreamer-rs (`v1_22`) links libgstreamer-1.0 dynamically; prove it at boot.
    match gstreamer::init() {
        Ok(()) => info!(
            "[agent] gstreamer initialised: {}",
            gstreamer::version_string()
        ),
        Err(e) => error!("[agent] gstreamer init failed: {e}"),
    }

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&cfg.home)
        .with_context(|| format!("create XORGATE_HOME {}", cfg.home.display()))?;

    let now0 = now_ms();
    let pid = std::process::id();
    let worker_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if let Err(e) = pidfile::write(
        &cfg.home,
        &pidfile::PidRecord {
            pid,
            started_at: now0,
            device_id: cfg.device_id.clone(),
            mode: cfg.mode.clone(),
            rate_hz: JsNumber(cfg.rate_hz),
            offline: cfg.offline,
            worker: worker_path,
            argv: std::env::args().skip(1).collect(),
        },
    ) {
        warn!("[agent] pidfile write failed: {e}");
    }

    let mut store = StateStore::new(
        StateSnapshot {
            v: 1,
            written_by: format!("{WRITTEN_BY_PREFIX}{}", build.package_version),
            device_id: cfg.device_id.clone(),
            mode: cfg.mode.clone(),
            rate_hz: JsNumber(cfg.rate_hz),
            endpoint: cfg.endpoint.clone(),
            offline: cfg.offline,
            pid,
            started_at: now0,
            last_write_at: now0,
            connection: ConnectionState::Connecting.as_str(),
            connection_since: now0,
            seq: 0,
            published: 0,
            publish_errors: 0,
            last_publish_at: None,
            last_error: None,
            latest: None,
            recent_events: vec![crate::state::ActivityEvent {
                ts: now0,
                level: "info",
                msg: format!("agent starting (mode={})", cfg.mode),
            }],
        },
        cfg.home.join("state.json"),
    );
    store.flush(now0, true);

    let rails = env::hardware_rails(&cfg.hardware_path);
    let ticker = Ticker {
        device_id: cfg.device_id.clone(),
        seq: 0,
        sampler: SystemSampler::new(rails).await,
    };

    let code = if cfg.offline {
        run_offline(&cfg, ticker, &mut store).await
    } else {
        run_online(&cfg, ticker, &mut store).await
    };
    pidfile::remove(&cfg.home);
    sd_notify(&[sd_notify::NotifyState::Stopping]);
    code
}

fn new_interval(period: Duration) -> Interval {
    let mut i = interval(period);
    i.set_missed_tick_behavior(MissedTickBehavior::Delay);
    i
}

async fn run_offline(cfg: &AgentEnv, mut ticker: Ticker, store: &mut StateStore) -> Result<i32> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;

    // The TS offline stub emits `connect` on the next tick.
    let now = now_ms();
    info!(
        "[agent] connected as {} → {} (mode={}, {} Hz, offline)",
        cfg.device_id, cfg.endpoint, cfg.mode, cfg.rate_hz
    );
    store.set_connection(ConnectionState::Connected, now);
    store.event("info", format!("connected to {}", cfg.endpoint), now);
    store.flush(now, true);
    sd_notify(&[sd_notify::NotifyState::Ready]);

    let mut tick = new_interval(Duration::from_millis(cfg.interval_ms));
    let mut log_timer = new_interval(Duration::from_secs(10));
    log_timer.tick().await; // skip the immediate first tick
    let sig = loop {
        tokio::select! {
            _ = sigterm.recv() => break "SIGTERM",
            _ = sigint.recv() => break "SIGINT",
            _ = sighup.recv() => {
                let t = now_ms();
                warn!("[agent] SIGHUP ignored (tty hangup — likely modem USB drop/re-enumeration)");
                store.event("warn", "SIGHUP ignored (likely modem USB re-enumeration)".into(), t);
                store.flush(t, true);
            }
            _ = tick.tick() => {
                let payload = ticker.payload().await;
                let json = serde_json::to_string(&payload)?;
                println!("{json}");
                let t = now_ms();
                store.set_latest(payload);
                store.record_publish(t);
                store.flush(t, false);
                sd_notify(&[sd_notify::NotifyState::Watchdog]);
            }
            _ = log_timer.tick() => info!("[agent] published {} messages", store.snapshot().published),
        }
    };
    let now = now_ms();
    info!("[agent] shutting down ({sig})…");
    store.set_connection(ConnectionState::Closed, now);
    store.event("info", format!("shutting down ({sig})"), now);
    store.flush(now, true);
    Ok(0)
}

fn read_pem(path: &std::path::Path, what: &str) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| {
        format!(
            "failed to read creds ({what} {}). Provision the device, or run with --offline.",
            path.display()
        )
    })
}

async fn run_online(cfg: &AgentEnv, mut ticker: Ticker, store: &mut StateStore) -> Result<i32> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;

    let cert = read_pem(&cfg.cert_path, "cert")?;
    let key = read_pem(&cfg.key_path, "key")?;
    let ca = match std::fs::read(&cfg.ca_path) {
        Ok(ca) => Some(ca),
        Err(e) => {
            warn!(
                "[agent] CA_PATH {} unreadable ({e}) — using the bundled Amazon Root CA 1",
                cfg.ca_path.display()
            );
            None
        }
    };
    let tls = xorgate_cloud::tls::client_config(ca.as_deref(), &cert, &key)
        .context("TLS client config")?;

    let topics = Topics::for_device(&cfg.device_id);
    let mut mqtt_cfg = MqttConfig::new(&cfg.device_id, &cfg.endpoint, tls);
    mqtt_cfg.keep_alive = Duration::from_secs(cfg.mqtt_keepalive_sec);
    mqtt_cfg.subscriptions = vec![
        (topics.config.clone(), QoS::AtLeastOnce),
        (topics.command.clone(), QoS::AtLeastOnce),
    ];
    {
        let topic = topics.status.clone();
        let id = cfg.device_id.clone();
        mqtt_cfg.will = Some(Arc::new(move || Will {
            topic: topic.clone(),
            payload: serde_json::to_vec(&StatusPayload::new(&id, now_ms(), false))
                .unwrap_or_default(),
            qos: QoS::AtMostOnce,
            retain: false,
        }));
    }
    info!(
        "[agent] connecting to {}:8883 as {} (keepalive {} s)",
        cfg.endpoint, cfg.device_id, cfg.mqtt_keepalive_sec
    );
    let mut mqtt = MqttClient::connect(mqtt_cfg);

    let mut tick: Option<Interval> = None;
    let mut heartbeat = new_interval(Duration::from_secs(1));
    let mut log_timer = new_interval(Duration::from_secs(10));
    log_timer.tick().await;
    let mut ready_sent = false;
    let mut send_ready = |store: &mut StateStore| {
        if !ready_sent {
            ready_sent = true;
            sd_notify(&[sd_notify::NotifyState::Ready]);
            store.flush(now_ms(), true);
        }
    };

    let sig = loop {
        tokio::select! {
            _ = sigterm.recv() => break "SIGTERM",
            _ = sigint.recv() => break "SIGINT",
            _ = sighup.recv() => {
                let t = now_ms();
                warn!("[agent] SIGHUP ignored (tty hangup — likely modem USB drop/re-enumeration)");
                store.event("warn", "SIGHUP ignored (likely modem USB re-enumeration)".into(), t);
                store.flush(t, true);
            }
            ev = mqtt.next_event() => {
                let now = now_ms();
                match ev {
                    None => {
                        error!("[agent] mqtt event loop ended unexpectedly");
                        store.set_error("mqtt event loop ended", now);
                        store.flush(now, true);
                        return Ok(1);
                    }
                    Some(MqttEvent::Connected { session_present }) => {
                        info!(
                            "[agent] connected as {} → {} (mode={}, {} Hz{})",
                            cfg.device_id, cfg.endpoint, cfg.mode, cfg.rate_hz,
                            if session_present { ", session present" } else { "" }
                        );
                        store.set_connection(ConnectionState::Connected, now);
                        store.event("info", format!("connected to {}", cfg.endpoint), now);
                        store.flush(now, true);
                        let status = serde_json::to_vec(&StatusPayload::new(&cfg.device_id, now, true))?;
                        if let Err(e) = mqtt.try_publish(&topics.status, QoS::AtMostOnce, false, status) {
                            error!("[agent] status publish failed: {e}");
                        }
                        info!("[agent] subscribed: {} (retained config), {} (commands)", topics.config, topics.command);
                        send_ready(store);
                        if tick.is_none() {
                            tick = Some(new_interval(Duration::from_millis(cfg.interval_ms)));
                        }
                    }
                    Some(MqttEvent::Disconnected { reason }) => {
                        warn!("[agent] connection closed: {reason}");
                        store.set_connection(ConnectionState::Closed, now);
                        store.set_error(&reason, now);
                        store.event("warn", "connection closed".into(), now);
                        store.flush(now, true);
                        send_ready(store);
                    }
                    Some(MqttEvent::Reconnecting { attempt, delay }) => {
                        info!("[agent] reconnecting… (attempt {attempt}, in {:.1} s)", delay.as_secs_f64());
                        store.set_connection(ConnectionState::Reconnecting, now);
                        store.event("warn", "reconnecting…".into(), now);
                        store.flush(now, true);
                    }
                    Some(MqttEvent::ServerDisconnect { reason }) => {
                        warn!("[agent] broker sent DISCONNECT ({reason}) — another client with our clientId?");
                        store.event("warn", format!("broker disconnect: {reason}"), now);
                        store.flush(now, true);
                    }
                    Some(MqttEvent::SubscribeFailed { topic, reason }) => {
                        error!("[agent] subscribe failed for {topic}: {reason}");
                        store.event("warn", format!("subscribe failed: {topic}: {reason}"), now);
                        store.flush(now, true);
                    }
                    Some(MqttEvent::Message(m)) => {
                        if m.topic == topics.config {
                            on_config_message(&m.payload, m.retain, store, now);
                        } else if m.topic == topics.command {
                            on_command_message(&m.payload, store, now);
                        } else {
                            warn!("[agent] message on unexpected topic {}", m.topic);
                        }
                    }
                }
            }
            _ = async { tick.as_mut().expect("tick set").tick().await }, if tick.is_some() => {
                let payload = ticker.payload().await;
                let json = serde_json::to_vec(&payload)?;
                let t = now_ms();
                store.set_latest(payload);
                if mqtt.is_connected() {
                    match mqtt.try_publish(&topics.telemetry, QoS::AtMostOnce, false, json) {
                        Ok(()) => store.record_publish(t),
                        Err(e) => store.record_publish_error(&e.to_string(), t),
                    }
                }
                store.flush(t, false);
                sd_notify(&[sd_notify::NotifyState::Watchdog]);
            }
            _ = heartbeat.tick() => {
                // Keep lastWriteAt fresh while not connected so the file never looks stale.
                if !mqtt.is_connected() {
                    store.flush(now_ms(), true);
                }
            }
            _ = log_timer.tick() => info!("[agent] published {} messages", store.snapshot().published),
        }
    };

    let now = now_ms();
    info!("[agent] shutting down ({sig})…");
    store.set_connection(ConnectionState::Closed, now);
    store.event("info", format!("shutting down ({sig})"), now);
    store.flush(now, true);
    let offline = serde_json::to_vec(&StatusPayload::new(&cfg.device_id, now, false))?;
    if let Err(e) = mqtt.try_publish(&topics.status, QoS::AtMostOnce, false, offline) {
        warn!("[agent] final status publish not queued: {e}");
    }
    let clean = mqtt.shutdown(ACK_FLUSH_TIMEOUT).await;
    if clean {
        info!("[agent] disconnected cleanly (online:false sent)");
    } else {
        warn!(
            "[agent] disconnect not confirmed within {} s — exiting anyway",
            ACK_FLUSH_TIMEOUT.as_secs()
        );
    }
    Ok(0)
}

fn on_config_message(payload: &[u8], retain: bool, store: &mut StateStore, now: i64) {
    match serde_json::from_slice::<Value>(payload) {
        Ok(v) if v.is_object() => {
            let rev = v.get("rev").and_then(Value::as_i64).unwrap_or(0);
            let scope = v
                .get("scope")
                .and_then(|s| s.get("workspaceId"))
                .and_then(Value::as_str)
                .map(|ws| format!(", scope ws={ws}"))
                .unwrap_or_default();
            let namespaces: Vec<&str> = v
                .as_object()
                .map(|o| {
                    o.keys()
                        .map(String::as_str)
                        .filter(|k| {
                            !matches!(
                                *k,
                                "v" | "rev" | "ts" | "scope" | "uploadBucket" | "hardware"
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            info!(
                "[agent] retained config rev={rev} (not applied, phase 0){scope}; namespaces: {}; {} bytes, retain={retain}",
                namespaces.join(","),
                payload.len()
            );
            store.event(
                "info",
                format!("retained config rev={rev} received (not applied, phase 0)"),
                now,
            );
        }
        _ => {
            error!("[agent] ignoring malformed config message");
            store.event(
                "warn",
                "ignoring malformed cloud config message".into(),
                now,
            );
        }
    }
    store.flush(now, true);
}

fn on_command_message(payload: &[u8], store: &mut StateStore, now: i64) {
    match serde_json::from_slice::<Value>(payload) {
        Ok(v) if v.is_object() => {
            let command = v.get("command").and_then(Value::as_str).unwrap_or("?");
            let request_id = v.get("requestId").and_then(Value::as_str).unwrap_or("?");
            warn!("[agent] {command} command received ({request_id}) — not executed, phase 0");
            store.event(
                "warn",
                format!("{command} command received (not executed, phase 0)"),
                now,
            );
        }
        _ => {
            error!("[agent] ignoring malformed command message");
            store.event("warn", "ignoring malformed command message".into(), now);
        }
    }
    store.flush(now, true);
}
