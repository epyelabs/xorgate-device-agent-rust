//! The worker — the composition root that `src/worker.ts` is (README §3.2):
//! boot (config, home, settings upgrades, hardware block, pidfile, initial
//! state.json), the subsystems (collectors, MQTT plane + publish loop, config
//! plane, command plane), the 1 s heartbeat (systemd watchdog), the 10 s
//! `published N messages` line, signals, and the ordered shutdown.
//!
//! Boot order mirrors `worker.ts` top to bottom; every Phase 3/4 call site is
//! either a [`PhaseHooks`](crate::ctx::PhaseHooks) no-op or a comment naming
//! the TS line it stands in for (O5).

use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::{interval_at, Instant, Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{error, info, warn};
use xorgate_cloud::mqtt::{MqttClient, MqttConfig, MqttEvent, QoS, Will};
use xorgate_core::hardware_block::{parse_hardware_block, DeviceHardwareBlock};
use xorgate_core::telemetry::{
    ActivityEvent, ActivityLevel, ConnectionState, JsNumber, RunMode, StateSnapshot, StatusPayload,
    TelemetryMedia, TelemetryReading,
};
use xorgate_core::types::{GnssView, GpsClockReading, PowerSampleReading};
use xorgate_hal::io::{null_tape, open_file_tape, TapeStart};
use xorgate_hal::read_lossy::{read_text, FileText};

use crate::ctx::{
    AgentCtx, BoxFut, CommandPlaneDeps, ConfigPlaneDeps, GroupSample, NoopHooks, ReportRequester,
    TelemetrySource,
};
use crate::env::{AgentEnv, Creds};
use crate::mqtt_plane::{MqttHandle, MqttPlane, OfflineTransport, RealTransport};
use crate::scope::ScopeHandle;
use crate::shutdown::Shutdown;
use crate::state::StateHandle;
use crate::{build_info, collectors, command_plane, config_plane, env, logging, pidfile};

/// `WRITTEN_BY` (`worker.ts:77`), Rust-flavoured (tolerated, T8).
const WRITTEN_BY_PREFIX: &str = "xorgate-device-agent-rs@";
/// rumqttc refuses a keepalive under 5 s (it asserts); the TS accepts any > 0.
const MIN_KEEPALIVE_SECS: f64 = 5.0;

/// `xorgate-rs run [flags]`: run the worker to completion; the exit code.
pub fn main(args: &[String]) -> i32 {
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

fn sd_notify(states: &[sd_notify::NotifyState]) {
    // Harmless when not under systemd (NOTIFY_SOCKET unset).
    let _ = sd_notify::notify(states);
}

/// A `setInterval(fn, period)`: first fire one period from now.
fn every(period: Duration) -> Interval {
    let mut i = interval_at(Instant::now() + period, period);
    i.set_missed_tick_behavior(MissedTickBehavior::Delay);
    i
}

/// `ensureHome()` (`runtime.ts:37-41`): mkdir -p, 0700.
fn ensure_home(home: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .with_context(|| format!("create XORGATE_HOME {}", home.display()))
}

/// `loadHardwareBlock(path)` (`hardware-config.ts:129-151`): absent →
/// `None` silently; not JSON / failing validation → `None` + one
/// `[hardware] …` warning. (WP3's `hardware_file.rs` owns the save side.)
pub fn load_hardware_block(path: &Path) -> Option<DeviceHardwareBlock> {
    let text = read_text(path).ok()?;
    let p = path.display();
    let Ok(obj) = serde_json::from_str::<Value>(&text) else {
        warn!("[hardware] {p} is not valid JSON — ignoring; using legacy hardware behavior");
        return None;
    };
    let block = parse_hardware_block(&obj);
    if block.is_none() {
        warn!("[hardware] {p} failed validation — ignoring; using legacy hardware behavior");
    }
    block
}

/// The boot log line of a loaded hardware block (`worker.ts:115-121`).
pub fn hardware_block_line(b: &DeviceHardwareBlock) -> String {
    let cameras = b
        .video
        .iter()
        .map(|v| format!("{}@{}", v.key, v.connector))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "[agent] hardware block: {} encoder={} cameras={} rails={} led={} imu={}",
        b.platform.as_str(),
        b.encoder.as_str(),
        if cameras.is_empty() { "none" } else { &cameras },
        b.rails,
        if b.status_led.is_some() { "yes" } else { "no" },
        b.imu.as_deref().unwrap_or("none"),
    )
}

/// `sim` mode's source (O7): collectors off, no generator — empty metrics
/// and the idle media block every tick.
struct SimSource;

impl TelemetrySource for SimSource {
    fn read(&self) -> BoxFut<'_, Result<TelemetryReading, String>> {
        Box::pin(async {
            Ok(TelemetryReading {
                metrics: Default::default(),
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
}

/// The tape's meta inputs (hardware.json, kvs.json channels, home files).
fn tape_start(env: &AgentEnv, build: &str, now0: i64, paths: &crate::ctx::Paths) -> TapeStart {
    let json = |p: &Path| -> Value {
        read_text(p)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null)
    };
    let kvs = json(&paths.kvs_manifest);
    let mut home = Map::new();
    for (name, p) in [
        ("settings.json", &paths.settings),
        ("config-sync.json", &paths.config_sync),
        ("commands.json", &paths.commands),
    ] {
        let v = match read_text(p) {
            FileText::Text(t) => Value::String(t),
            _ => Value::Null,
        };
        home.insert(name.to_string(), v);
    }
    TapeStart {
        device_id: env.config.device_id.clone(),
        build: build.to_string(),
        start_wall: now0,
        hardware: json(&paths.hardware),
        kvs_channels: kvs
            .get("channels")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        home_files: Some(home),
    }
}

/// The MQTT side of the run: the handle, the event stream, and how to end it.
struct Link {
    mqtt: MqttHandle,
    events: tokio::sync::mpsc::UnboundedReceiver<MqttEvent>,
    shutdown: Option<xorgate_cloud::mqtt::MqttShutdown>,
}

/// `connect(config)` (`mqtt.ts:38-67`; offline stub `:74-105`).
fn connect(env: &AgentEnv, creds: Option<Creds>) -> Result<Link> {
    let cfg = &env.config;
    let Some(creds) = creds else {
        let (t, events) = OfflineTransport::new();
        return Ok(Link {
            mqtt: MqttHandle::new(t),
            events,
            shutdown: None,
        });
    };
    let tls = xorgate_cloud::tls::client_config(Some(&creds.ca), &creds.cert, &creds.key)
        .context("TLS client config")?;
    let mut mqtt_cfg = MqttConfig::new(&cfg.device_id, &cfg.endpoint, tls);
    let mut keepalive = cfg.mqtt_keepalive_sec;
    if keepalive < MIN_KEEPALIVE_SECS {
        warn!(
            "[agent] MQTT_KEEPALIVE_SEC {} is below the client's 5 s floor — using 5",
            xorgate_core::js::number_to_string(keepalive)
        );
        keepalive = MIN_KEEPALIVE_SECS;
    }
    mqtt_cfg.keep_alive = Duration::from_secs_f64(keepalive);
    {
        let topic = cfg.topics.status.clone();
        let id = cfg.device_id.clone();
        mqtt_cfg.will = Some(Arc::new(move || Will {
            topic: topic.clone(),
            payload: serde_json::to_vec(&StatusPayload::new(&id, crate::now_ms(), false))
                .unwrap_or_default(),
            qos: QoS::AtMostOnce,
            retain: false,
        }));
    }
    info!(
        "[agent] connecting to {}:8883 as {} (keepalive {} s)",
        cfg.endpoint,
        cfg.device_id,
        xorgate_core::js::number_to_string(keepalive)
    );
    let (publisher, events, shutdown) = MqttClient::connect(mqtt_cfg).split();
    Ok(Link {
        mqtt: MqttHandle::new(Arc::new(RealTransport(publisher))),
        events,
        shutdown: Some(shutdown),
    })
}

async fn run(args: &[String]) -> Result<i32> {
    let log_sink = logging::init();
    let build = build_info::load();
    // `const config = loadConfig();` (`worker.ts:79`) — throws → exit 1.
    let env = env::load(args)?;
    if env.config.mode == RunMode::Replay {
        bail!("MODE=replay is not supported by the Rust agent (plan Q6) — run the JS agent for replay containers");
    }
    let creds = if env.config.offline {
        None
    } else {
        Some(env::read_creds(&env.config)?)
    };
    info!(
        "[agent] xorgate-rs {} starting (mode={}, home={}, log={log_sink})",
        build.format(),
        env.config.mode.as_str(),
        env.home.display()
    );
    // A10: gstreamer-rs (`v1_22`) links libgstreamer-1.0 dynamically; prove it at boot.
    match gstreamer::init() {
        Ok(()) => info!(
            "[agent] gstreamer initialised: {}",
            gstreamer::version_string()
        ),
        Err(e) => error!("[agent] gstreamer init failed: {e}"),
    }
    ensure_home(&env.home)?;

    let paths = Arc::new(env::paths(&env));
    let env = Arc::new(env);
    let cfg = &env.config;
    let now0 = crate::now_ms();
    let pid = std::process::id();

    let state = StateHandle::new(
        StateSnapshot {
            v: 1,
            written_by: format!("{WRITTEN_BY_PREFIX}{}", build.package_version),
            device_id: cfg.device_id.clone(),
            mode: cfg.mode,
            media_enabled: Some(cfg.media.enabled),
            rate_hz: JsNumber(cfg.rate_hz),
            endpoint: cfg.endpoint.clone(),
            offline: cfg.offline,
            pid,
            started_at: now0,
            last_write_at: now0,
            connection: ConnectionState::Connecting,
            connection_since: now0,
            seq: 0,
            published: 0,
            publish_errors: 0,
            last_publish_at: None,
            last_error: None,
            latest: None,
            recent_events: vec![ActivityEvent {
                ts: now0,
                level: ActivityLevel::Info,
                msg: format!("agent starting (mode={})", cfg.mode.as_str()),
            }],
            recording: None,
        },
        paths.state.clone(),
    );
    let (hardware_tx, _) = tokio::sync::watch::channel(None);
    let mut ctx = AgentCtx {
        env: env.clone(),
        paths: paths.clone(),
        build: Arc::new(build.clone()),
        tape: null_tape(),
        state: state.clone(),
        hooks: Arc::new(NoopHooks),
        hardware: Arc::new(hardware_tx),
        cancel: CancellationToken::new(),
        tracker: TaskTracker::new(),
    };

    // settings.json upgrades before any other reader/writer (`worker.ts:92-105`).
    config_plane::boot(&ctx);

    // Hardware block (`worker.ts:114-121`).
    let block = load_hardware_block(&paths.hardware);
    if let Some(b) = &block {
        info!("{}", hardware_block_line(b));
    }
    ctx.hardware.send_replace(block);

    // Record/replay tape (Rust only): opened once the inputs above are known.
    if let Some(tape_path) = &paths.tape {
        let start = tape_start(&env, &build.format(), now0, &paths);
        match open_file_tape(tape_path, &start) {
            Ok(t) => {
                info!("[agent] recording tape → {}", tape_path.display());
                ctx.tape = t;
            }
            Err(e) => warn!("[agent] tape not opened: {e}"),
        }
    }

    // `source` (`worker.ts:270-326`): real collectors, or the empty sim source (O7).
    let source: Arc<dyn TelemetrySource> = match cfg.mode {
        RunMode::Real => collectors::start(&ctx).await,
        _ => Arc::new(SimSource),
    };

    // Pidfile (`worker.ts:340-353`), then the initial state (`:356-379`).
    if env.self_managed {
        let worker = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let rec = pidfile::PidRecord {
            pid,
            started_at: now0,
            device_id: cfg.device_id.clone(),
            mode: cfg.mode.as_str().to_string(),
            rate_hz: JsNumber(cfg.rate_hz),
            offline: cfg.offline,
            worker,
            argv: std::iter::once("run".to_string())
                .chain(env.argv.iter().cloned())
                .collect(),
        };
        if let Err(e) = pidfile::write_pidfile(&paths.pid, &rec) {
            warn!("[agent] pidfile write failed: {e}");
        }
    }
    state.flush(now0, true);

    // Phase 3/4 boot steps (O5): reconcileHardwareNow (`:438`), reconcileCameraMountNow
    // (`:443`), the RecordingManager — whose construction calls buildUploadOptions()
    // (`:439`, CF33 → WP3) — recording.start() (`:540`), the clock stepper (`:548`),
    // the LED manager (`:609`).
    config_plane::boot_upload_options(&ctx);

    // MQTT (`worker.ts:664`), scope (`:684`), config plane (`:700-746`), commands.
    let link = connect(&env, creds)?;
    let Link {
        mqtt,
        mut events,
        shutdown: mqtt_end,
    } = link;
    let (reports, report_rx) = ReportRequester::channel();
    let scope = ScopeHandle::new(
        &cfg.device_id,
        &cfg.topics.telemetry,
        state.clone(),
        reports.clone(),
    );
    let config = config_plane::start(
        &ctx,
        ConfigPlaneDeps {
            mqtt: mqtt.clone(),
            scope: scope.clone(),
            reports: reports.clone(),
            report_rx,
        },
    );
    let commands = command_plane::start(
        &ctx,
        CommandPlaneDeps {
            mqtt: mqtt.clone(),
            reports: reports.clone(),
        },
    );
    let mut plane = MqttPlane::new(
        ctx.clone(),
        mqtt.clone(),
        scope.clone(),
        config.clone(),
        commands,
        source.clone(),
    );

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut heartbeat = every(Duration::from_secs(1));
    let mut published_log = every(Duration::from_secs(10));
    let mut tape_clock = every(Duration::from_secs(10));
    let tape_on = ctx.tape.enabled();
    let mono0 = Instant::now();
    sd_notify(&[sd_notify::NotifyState::Ready]);

    let sig = loop {
        tokio::select! {
            _ = sigterm.recv() => break "SIGTERM",
            _ = sigint.recv() => break "SIGINT",
            _ = sighup.recv() => crate::shutdown::on_sighup(&ctx),
            ev = events.recv() => match ev {
                Some(ev) => plane.on_event(ev),
                None => {
                    let t = crate::now_ms();
                    error!("[agent] mqtt event loop ended unexpectedly");
                    state.set_error("mqtt event loop ended", t);
                    state.flush(t, true);
                    return Ok(1);
                }
            },
            // `heartbeat` (`worker.ts:633-638`) + the systemd watchdog: the
            // loop is alive whatever the MQTT state (Phase 0 A7).
            _ = heartbeat.tick() => {
                sd_notify(&[sd_notify::NotifyState::Watchdog]);
                ctx.hooks.led_tick();
                if state.connection() != ConnectionState::Connected {
                    state.flush(crate::now_ms(), true);
                }
            }
            // `published N messages` (`worker.ts:1286-1289`).
            _ = published_log.tick() => info!("[agent] published {} messages", state.published()),
            _ = tape_clock.tick(), if tape_on => {
                ctx.tape.clock(crate::now_ms(), mono0.elapsed().as_millis() as u64);
            }
        }
    };

    let code = Shutdown {
        ctx: &ctx,
        mqtt: &mqtt,
        plane: &mut plane,
        config: config.as_ref(),
        source: source.as_ref(),
    }
    .run(sig, 0, |budget| async move {
        match mqtt_end {
            Some(end) => end.shutdown(budget).await,
            None => true,
        }
    })
    .await;
    ctx.tracker.close();
    let _ = tokio::time::timeout(Duration::from_millis(500), ctx.tracker.wait()).await;
    ctx.tape.close();
    sd_notify(&[sd_notify::NotifyState::Stopping]);
    Ok(code)
}
