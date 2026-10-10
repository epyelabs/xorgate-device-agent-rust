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
    AgentClock, AgentCtx, BoxFut, CommandPlaneDeps, ConfigPlaneDeps, GroupSample, NoopHooks,
    ReportRequester, TelemetrySource,
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
/// The MQTT CONNECT keepalive is a u16 (rumqttc casts `as u16`, so a larger
/// value would wrap); review-wp1 #6.
const MAX_KEEPALIVE_SECS: f64 = 65535.0;

/// `MQTT_KEEPALIVE_SEC` → what the client can use: clamped to [5, 65535] s
/// with the one warning line to log (amb. #2, review-wp1 #6). The TS accepts
/// any finite > 0 (`config.ts:217-228`); no provisioned device sets the key
/// (default 60), and a TS client given a value outside the range would never
/// have connected cleanly either. A fractional value (e.g. `7.5`) connects
/// here (rumqttc sends whole seconds), while mqtt-packet refuses it (`Invalid keepalive`,
/// `writeToStream.js:130-134`) and the TS reconnect-loops: Rust is more
/// lenient (review-wp1-followup #8, accepted).
fn clamp_keepalive(sec: f64) -> (f64, Option<String>) {
    let shown = xorgate_core::js::number_to_string(sec);
    if sec < MIN_KEEPALIVE_SECS {
        (
            MIN_KEEPALIVE_SECS,
            Some(format!(
                "[agent] MQTT_KEEPALIVE_SEC {shown} is below the client's 5 s floor — using 5"
            )),
        )
    } else if sec > MAX_KEEPALIVE_SECS {
        (
            MAX_KEEPALIVE_SECS,
            Some(format!(
                "[agent] MQTT_KEEPALIVE_SEC {shown} is above the MQTT maximum of 65535 s — using 65535"
            )),
        )
    } else {
        (sec, None)
    }
}

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

/// The home files a replay instance starts from (settings.json,
/// config-sync.json, commands.json) — captured BEFORE the boot baseline /
/// migration can rewrite them (review-wp1 #5), so the JS replay starts from
/// what the Rust agent found, not from what it wrote.
fn tape_home_files(paths: &crate::ctx::Paths) -> Map<String, Value> {
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
    home
}

/// The tape's meta inputs (hardware.json, kvs.json channels, the pre-boot home files).
fn tape_start(
    env: &AgentEnv,
    build: &str,
    now0: i64,
    paths: &crate::ctx::Paths,
    home: Map<String, Value>,
) -> TapeStart {
    let json = |p: &Path| -> Value {
        read_text(p)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null)
    };
    let kvs = json(&paths.kvs_manifest);
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
    let (keepalive, warning) = clamp_keepalive(cfg.mqtt_keepalive_sec);
    if let Some(w) = warning {
        warn!("{w}");
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
        clock: Arc::new(AgentClock::system()),
        cancel: CancellationToken::new(),
        tracker: TaskTracker::new(),
    };

    // The tape's home-file snapshot, before boot can write any of them (#5).
    let tape_home = paths.tape.as_ref().map(|_| tape_home_files(&paths));

    // settings.json upgrades before any other reader/writer (`worker.ts:92-105`).
    config_plane::boot(&ctx);

    // Hardware block (`worker.ts:114-121`).
    let block = load_hardware_block(&paths.hardware);
    if let Some(b) = &block {
        info!("{}", hardware_block_line(b));
    }
    ctx.hardware.send_replace(block);

    // Record/replay tape (Rust only): opened once the inputs above are known.
    if let (Some(tape_path), Some(home)) = (&paths.tape, tape_home) {
        let start = tape_start(&env, &build.format(), now0, &paths, home);
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

    // Phase 3/4 boot steps (O5), each a PhaseHooks no-op at its TS place:
    // `await reconcileHardwareNow()` (`:427`), `await reconcileCameraMountNow()`
    // (`:432`), the RecordingManager — whose construction calls
    // buildUploadOptions() (`:439`, CF33 → WP3) — and `await recording.start()`
    // (`:529`). The clock stepper (`:536-570`) and the LED manager (`:609`) are
    // Phase 4 constructors with no Phase 2 call site.
    ctx.hooks.reconcile_hardware_boot().await;
    ctx.hooks.reconcile_camera_mount_boot().await;
    config_plane::boot_upload_options(&ctx);
    ctx.hooks.recording_start().await;

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
    // `cameraMountPoll` (`worker.ts:648-651`): every 60 s, first at +60 s.
    let mut camera_mount_poll = every(Duration::from_secs(60));
    let tape_on = ctx.tape.enabled();
    let mono0 = Instant::now();
    sd_notify(&[sd_notify::NotifyState::Ready]);

    let (sig, exit_code) = loop {
        tokio::select! {
            _ = sigterm.recv() => break ("SIGTERM", 0),
            _ = sigint.recv() => break ("SIGINT", 0),
            _ = sighup.recv() => crate::shutdown::on_sighup(&ctx),
            ev = events.recv() => match ev {
                Some(ev) => plane.on_event(ev),
                // The loop task died (review-wp1 #10a): the normal shutdown, exit 1.
                None => break (
                    crate::shutdown::LOOP_ENDED,
                    crate::shutdown::on_mqtt_loop_ended(&ctx),
                ),
            },
            // `heartbeat` (`worker.ts:633-638`) + the systemd watchdog: the
            // loop is alive whatever the MQTT state (Phase 0 A7).
            _ = heartbeat.tick() => {
                sd_notify(&[sd_notify::NotifyState::Watchdog]);
                ctx.hooks.led_tick();
                if state.connection() != ConnectionState::Connected {
                    state.flush(ctx.now_ms(), true);
                }
            }
            _ = camera_mount_poll.tick() => {
                if !ctx.shutting_down() {
                    ctx.hooks.reconcile_camera_mount();
                }
            }
            // `published N messages` (`worker.ts:1286-1289`).
            _ = published_log.tick() => info!("[agent] published {} messages", state.published()),
            _ = tape_clock.tick(), if tape_on => {
                ctx.tape.clock(ctx.now_ms(), mono0.elapsed().as_millis() as u64);
            }
        }
    };

    let code = Shutdown {
        ctx: &ctx,
        mqtt: &mqtt,
        plane: &mut plane,
        config: config.as_ref(),
        source: source.as_ref(),
        // A dead loop has no stream left to close (no `disconnected cleanly`).
        link_closes: mqtt_end.is_some() && exit_code == 0,
    }
    .run(sig, exit_code, |budget| async move {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// review-wp1 #6 / amb. #2: [5, 65535] s, one warning outside it.
    #[test]
    fn keepalive_is_clamped_to_the_client_range() {
        assert_eq!(clamp_keepalive(60.0), (60.0, None));
        assert_eq!(clamp_keepalive(5.0), (5.0, None));
        assert_eq!(clamp_keepalive(65535.0), (65535.0, None));
        let (v, w) = clamp_keepalive(1.5);
        assert_eq!(v, 5.0);
        assert_eq!(
            w.unwrap(),
            "[agent] MQTT_KEEPALIVE_SEC 1.5 is below the client's 5 s floor — using 5"
        );
        let (v, w) = clamp_keepalive(1e300);
        assert_eq!(v, 65535.0);
        assert_eq!(
            w.unwrap(),
            "[agent] MQTT_KEEPALIVE_SEC 1e+300 is above the MQTT maximum of 65535 s — using 65535"
        );
        // No panic in the Duration conversion at either bound.
        let _ = Duration::from_secs_f64(clamp_keepalive(f64::MAX).0);
    }

    /// `loadHardwareBlock` (`hardware-config.ts:129-151`) + the boot line
    /// (`worker.ts:115-121`) over the CM4 block of `fixtures/hardware_block/parse.json`.
    #[test]
    fn hardware_block_load_and_line() {
        let d = std::env::temp_dir().join(format!("xg-hw-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("hardware.json");
        assert!(load_hardware_block(&p).is_none()); // absent: legacy, silent
        std::fs::write(&p, "{not json").unwrap();
        assert!(load_hardware_block(&p).is_none());
        std::fs::write(&p, r#"{"v":1}"#).unwrap();
        assert!(load_hardware_block(&p).is_none());
        std::fs::write(
            &p,
            r#"{"v":2,"platform":"rpi-cm4","encoder":"hw-h264","rails":false,"statusLed":null,"imu":"bno085","video":[{"key":"cam0","connector":"csi0","sensor":"imx290","codec":"h264","label":"Front"}]}"#,
        )
        .unwrap();
        let b = load_hardware_block(&p).expect("valid block");
        assert_eq!(
            hardware_block_line(&b),
            "[agent] hardware block: rpi-cm4 encoder=hw-h264 cameras=cam0@csi0 rails=false led=no imu=bno085"
        );
        let none = DeviceHardwareBlock {
            video: vec![],
            imu: None,
            ..b
        };
        assert_eq!(
            hardware_block_line(&none),
            "[agent] hardware block: rpi-cm4 encoder=hw-h264 cameras=none rails=false led=no imu=none"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
