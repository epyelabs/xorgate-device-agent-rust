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
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{error, info, warn};
use xorgate_cloud::mqtt::{MqttClient, MqttConfig, MqttEvent, QoS, Will};
use xorgate_core::clock::Clock;
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::telemetry::{
    ActivityEvent, ActivityLevel, ConnectionState, JsNumber, RunMode, StateSnapshot, StatusPayload,
    TelemetryMedia, TelemetryReading,
};
use xorgate_core::types::{GnssView, GpsClockReading, PowerSampleReading};
use xorgate_hal::io::{null_tape, open_file_tape, TapeStart};
use xorgate_hal::read_lossy::{read_text, FileText};

use crate::ctx::{
    AgentClock, AgentCtx, BoxFut, CommandPlaneDeps, ConfigPlaneDeps, GroupSample, MediaControl,
    NoopHooks, ReportRequester, TelemetrySource,
};
use crate::env::{AgentEnv, Creds};
use crate::mqtt_plane::{MqttHandle, MqttPlane, OfflineTransport, RealTransport};
use crate::ops::hooks::OpsHooks;
use crate::recording::hooks::{stream_guard_fn, MediaReconciler, StreamHardware};
use crate::recording::{policy_sources, RecordingHooks, RecordingManager, RecordingManagerOpts};
use crate::scope::ScopeHandle;
use crate::shutdown::{ClientEnd, Shutdown};
use crate::state::StateHandle;
use crate::timer::SetInterval;
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

/// The next message on the recording manager's fatal channel, or never.
async fn next_fatal(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
) -> Option<String> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// A `setInterval(fn, period)`: first fire one period from now, re-armed
/// from each fire (P-WP6-1, [`SetInterval`]).
fn every(period: Duration) -> SetInterval {
    SetInterval::new(period)
}

/// `ensureHome()` (`runtime.ts:37-41`): mkdir -p, 0700.
fn ensure_home(home: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .with_context(|| format!("create XORGATE_HOME {}", home.display()))
}

/// `loadHardwareBlock(path)` (`hardware-config.ts:196-217` @ 09da38d): absent
/// → `None` silently; not JSON / failing validation → `None` + one
/// `[hardware] …` warning; the `peripherals` warnings of the parse go
/// through the same `[hardware] ` sink (X9). The read lives in the HAL
/// (`xorgate_hal::hardware_file::load_hardware_block`).
pub fn load_hardware_block(path: &Path) -> Option<DeviceHardwareBlock> {
    xorgate_hal::hardware_file::load_hardware_block(path, &mut |m| {
        warn!("{}", xorgate_core::hardware_block::hardware_warn_line(&m))
    })
}

/// The boot log line of a loaded hardware block (`worker.ts:126-134` @ 09da38d).
pub fn hardware_block_line(b: &DeviceHardwareBlock) -> String {
    let cameras = b
        .video
        .iter()
        .map(|v| format!("{}@{}", v.key, v.connector))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "[agent] hardware block: {} encoder={} cameras={} rails={} led={} imu={}{}",
        b.platform.as_str(),
        b.encoder.as_str(),
        if cameras.is_empty() { "none" } else { &cameras },
        b.rails,
        if b.status_led.is_some() { "yes" } else { "no" },
        b.imu.as_deref().unwrap_or("none"),
        // `(hardwareBlock.peripherals?.obd ? " obd=elm327/bt-spp" : "")`
        // (`worker.ts:132` @ 09da38d).
        if b.peripherals.as_ref().is_some_and(|p| p.obd.is_some()) {
            " obd=elm327/bt-spp"
        } else {
            ""
        },
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

/// `new RecordingManager({...})` (`worker.ts:439-501`).
fn build_recording_manager(
    ctx: &AgentCtx,
    source: Arc<dyn TelemetrySource>,
    media: Option<Arc<dyn MediaControl>>,
    upload: Option<xorgate_cloud::s3::UploadOptions>,
    time_source: crate::recording::time_source::TimeSourceHandle,
    clock_trust: Option<crate::ops::clock_trust::ClockTrust>,
) -> RecordingManager {
    let cfg = &ctx.env.config;
    let real = cfg.mode == RunMode::Real;
    let (m1, m2, m3) = (media.clone(), media.clone(), media);
    let video = crate::recording::VideoOpts {
        media_enabled: cfg.media.enabled,
        cameras: policy_sources::cameras_fn(ctx),
        encoder: policy_sources::encoder_fn(ctx),
        // `source.restartMediaChannel?.(streamKey)`: bounce the master onto the new socket.
        on_capture_spawned: Arc::new(move |key: &str| {
            if let Some(m) = &m1 {
                m.restart_channel(key);
            }
        }),
        // `source.setLiveShed?.(shed)` — reached ONLY through the manager's choke point.
        set_live_shed: Some(Arc::new(move |shed: bool| {
            if let Some(m) = &m2 {
                m.set_shed(shed);
            }
        })),
        // `source.setDisabledStreams?.(streamKeys)`.
        set_disabled_cameras: Some(Arc::new(move |keys: &[String]| {
            if let Some(m) = &m3 {
                m.set_disabled_streams(keys);
            }
        })),
    };
    let (state, clock) = (ctx.state.clone(), ctx.clock.clone());
    let on_status = Arc::new(move |s: &xorgate_core::telemetry::RecordingStatus| {
        // `store.setRecording(status); store.flush(Date.now())`.
        state.set_recording(s.clone());
        state.flush(clock.wall_ms(), false);
    });
    let (state, clock) = (ctx.state.clone(), ctx.clock.clone());
    let on_event = Arc::new(move |level: ActivityLevel, msg: &str| {
        // `console.log(`[agent] ${msg}`)` — log level does not follow `level`.
        info!("[agent] {msg}");
        state.note(level, msg, clock.wall_ms());
    });
    RecordingManager::new(RecordingManagerOpts {
        device_id: cfg.device_id.clone(),
        root: ctx.paths.home.join("recordings"),
        shm_dir: ctx.paths.home.join("shm"),
        settings_path: ctx.paths.settings.clone(),
        // `createTimeSourceTracker()` (60 s cache, the `timedatectl` probe).
        time_source: Arc::new(time_source.clone()),
        upload,
        video: Some(video),
        sample_groups: policy_sources::sample_groups_fn(source.clone()),
        power_sample: policy_sources::power_sample_fn(source.clone(), real),
        boot_sources: policy_sources::boot_sources(ctx, source),
        on_status,
        on_event,
        clock: ctx.clock.clone(),
        // WP2 recorder/disk/time source + WP3 uploader + WP4 capture.
        backend: Arc::new(crate::recording::Backend {
            clock: ctx.clock.clone(),
            tape: ctx.tape.clone(),
            home: Some(ctx.paths.home.clone()),
            time_source,
        }),
        poll_ms: None,
        shed_sample_ms: None,
        time_probe_timeout_ms: None,
        // `clockTrusted: () => clockTrust?.trusted() ?? true` (`worker.ts:542` @ 09da38d).
        clock_trusted: clock_trust
            .map(|t| Arc::new(move || t.trusted()) as crate::recording::manager::ClockTrustedFn),
    })
}

/// `reconcileCameraMountNow`'s inputs (`worker.ts:171-182`): settings.json
/// `cameraMount` and the hardware block in force, read per pass; the
/// `[camera-mount] …` + `camera mount: …` event sink.
fn camera_mount_reconciler(
    ctx: &AgentCtx,
    pipeline_dir: &str,
    runner: Arc<dyn xorgate_hal::privileged::PrivilegedRunner>,
    on_change: Arc<dyn Fn() + Send + Sync>,
) -> crate::ops::camera_mount::CameraMountReconciler {
    use crate::ops::camera_mount as cm;
    let settings = ctx.paths.settings.clone();
    let hw = ctx.hardware.clone();
    cm::CameraMountReconciler::new(
        cm::CameraMountReconcilerOptions {
            settings: Box::new(move || {
                xorgate_hal::settings_io::load_settings(&settings).camera_mount
            }),
            hardware: Box::new(move || hw.borrow().clone()),
            pipeline_dir: pipeline_dir.to_string(),
            boot_config_path: cm::BootConfigPath::Resolve,
            dt_root: cm::default_dt_root(),
            probe: Arc::new(cm::DtoverlayProbe {
                tape: ctx.tape.clone(),
            }),
            apply_rotation: Arc::new(cm::HelperRotationWriter { runner }),
            log: cm::camera_mount_event_sink(ctx.state.clone(), ctx.clock.clone()),
            clock: ctx.clock.clone(),
        },
        Box::new(move || on_change()),
    )
}

/// `createClockStepper({...})` (`worker.ts:537-552`).
fn clock_stepper(
    ctx: &AgentCtx,
    source: Arc<dyn TelemetrySource>,
    runner: Arc<dyn xorgate_hal::privileged::PrivilegedRunner>,
    time_source: crate::recording::time_source::TimeSourceHandle,
    clock_trust: Option<crate::ops::clock_trust::ClockTrust>,
) -> crate::ops::clock_stepper::ClockStepper {
    use crate::ops::clock_stepper as cs;
    let settings = ctx.paths.settings.clone();
    let (lte, tape) = (ctx.paths.lte_telemetry.clone(), ctx.tape.clone());
    cs::ClockStepper::new(cs::ClockStepperOptions {
        // `resolveTimeSyncConfig(loadSettings().timeSync)`.
        config: Box::new(move || {
            xorgate_core::settings::resolve_time_sync_config(
                xorgate_hal::settings_io::load_settings(&settings)
                    .time_sync
                    .as_ref(),
            )
        }),
        // `source.gpsClock?.() ?? null`.
        gps_clock: Box::new(move || source.gps_clock()),
        // `readModemClock(now)` over the daemon's file.
        modem_clock: Box::new(move |now| {
            let (lte, tape) = (lte.clone(), tape.clone());
            Box::pin(async move {
                let snap = xorgate_hal::modem_file::read(&lte, tape.as_ref()).await;
                xorgate_core::clock_step::read_modem_clock(now, snap.typed.as_ref())
            })
        }),
        probe_ntp: cs::system_probe_ntp(ctx.tape.clone()),
        runner,
        on_stepped: cs::time_source_on_stepped_with_trust(time_source, clock_trust),
        on_event: cs::agent_event_sink(ctx.state.clone(), ctx.clock.clone()),
        clock: ctx.clock.clone(),
        tape: ctx.tape.clone(),
    })
}

/// `new LedManager({...})` (`worker.ts:596-622`): the driver from the boot
/// hardware block (`led_wiring::led_driver_choice`, CF61; the Rust writer
/// choice `XORGATE_LED_WRITER`, chip override `XORGATE_LED_GPIOCHIP`, Q6).
fn led_manager(
    ctx: &AgentCtx,
    ops: &Arc<OpsHooks>,
    source: Arc<dyn TelemetrySource>,
) -> crate::ops::led::LedManager {
    use xorgate_core::led_wiring::{led_driver_choice, LedDriverChoice};
    use xorgate_hal::led_driver as ld;
    let block = ctx.hardware.borrow().clone();
    let driver = match led_driver_choice(block.as_ref()) {
        LedDriverChoice::Probe(map) => ld::agent_driver(
            map,
            ld::WriterChoice::from_env(),
            ld::chip_override_from_env(),
            ctx.tape.clone(),
            None,
        ),
        LedDriverChoice::Inert(reason) => ld::LedDriver::new(ld::LedDriverOptions {
            map: None,
            writer: None,
            probe: Some(Box::new(move || {
                Box::pin(async move {
                    xorgate_core::led::PinMapCheck {
                        ok: false,
                        reason: Some(reason.to_string()),
                    }
                })
            })),
            log: None,
        }),
    };
    let (state, rec, starting) = (
        ctx.state.clone(),
        ops.recording().clone(),
        ops.led_starting_flag(),
    );
    crate::ops::led::LedManager::new(
        driver,
        Box::new(move || crate::ops::led::LedSignalsOwned {
            connection: state.connection(),
            recording: rec.recording_status_for_led(),
            gnss: source.gnss_view(),
            starting: starting.load(std::sync::atomic::Ordering::SeqCst),
        }),
        crate::ops::led::file_read_lte(ctx.paths.lte_telemetry.clone(), ctx.tape.clone()),
        None,
    )
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
            obd: None,
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

    // The KVS media supervisor (`createMediaCollector(config.media)` inside
    // `createCollector`, `collectors/index.ts:133-135`): real mode with media
    // enabled only, with the stream guard closure of `worker.ts:205-218` over
    // the reconcile verdicts `reconcileHardwareNow` publishes.
    let stream_hw = Arc::new(StreamHardware::default());
    let media_cfg = crate::media::MediaConfig::from_env(&env, cfg.offline);
    let media: Option<Arc<dyn MediaControl>> = (cfg.mode == RunMode::Real && media_cfg.enabled)
        .then(|| {
            let guard = stream_guard_fn(ctx.hardware.clone(), stream_hw.clone(), &media_cfg);
            Arc::new(crate::media::MediaSupervisor::new(
                media_cfg.clone(),
                guard,
                ctx.tape.clone(),
            )) as Arc<dyn MediaControl>
        });

    // The Phase 3 hooks, installed before any subsystem clones the ctx. The
    // manager itself is built after the boot reconciles (`worker.ts:439`).
    let hooks = Arc::new(RecordingHooks::new(
        media.clone(),
        Some(Arc::new(MediaReconciler {
            ctx: ctx.clone(),
            stream_hw: stream_hw.clone(),
            pipeline_dir: media_cfg.pipeline_dir.clone().into(),
            list_exec: crate::media::real_list_exec(ctx.tape.clone()),
        })),
        config_plane::upload_options_builder(&ctx),
        ctx.clock.clone(),
    ));
    // Phase 4 (WP6): `OpsHooks` composes the Phase 3 hooks with the LED,
    // clock-stepper and camera-mount call sites (`worker.ts:150-199, 427-432,
    // 536-651, 713-726, 866-889, 1296-1302`).
    let ops = Arc::new(OpsHooks::new(hooks.clone()));
    ctx.hooks = ops.clone();

    // `source` (`worker.ts:270-326`): real collectors, or the empty sim source (O7).
    let source: Arc<dyn TelemetrySource> = match cfg.mode {
        RunMode::Real => collectors::start_with_media(&ctx, media.clone()).await,
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

    // Boot steps at their TS places: `await reconcileHardwareNow()` (`:427`),
    // `await reconcileCameraMountNow()` (`:432`, Phase 4 no-op), the
    // RecordingManager — whose construction consumes buildUploadOptions()
    // (`:439`, CF33) — and `await recording.start()` (`:529`), awaited before
    // the first capture spawn. The clock stepper (`:536-570`) and the LED
    // manager (`:609`) are Phase 4.
    //
    // `const timeSource = createTimeSourceTracker()` (`:420`): shared by the
    // recording manager (stamps) and the clock stepper (`noteStep`, CF58).
    let time_source = crate::recording::time_source::TimeSourceHandle::system(
        ctx.clock.clone(),
        ctx.tape.clone(),
    );
    // Clock trust gate (`worker.ts:471-487` @ 09da38d): until NTP (or a
    // GPS/NITZ step) lands, the live publish and the telemetry recorder are
    // HELD, for at most 5 min; real mode only (sim runs on the host clock).
    let clock_trust = (cfg.mode == RunMode::Real).then(|| {
        use crate::ops::clock_trust as ct;
        ct::ClockTrust::new(ct::ClockTrustOptions::new(
            ct::system_probe(ctx.tape.clone()),
            ctx.clock.clone(),
            crate::ops::clock_stepper::agent_event_sink(ctx.state.clone(), ctx.clock.clone()),
        ))
    });
    if let Some(t) = &clock_trust {
        t.start();
        ops.bind_clock_trust(t.clone());
    }
    // The privileged runner (exact-args sudo; `sudo` tape records, I9).
    let runner: Arc<dyn xorgate_hal::privileged::PrivilegedRunner> =
        Arc::new(xorgate_hal::privileged::TapedRunner::new(
            Arc::new(xorgate_hal::privileged::SudoRunner),
            ctx.tape.clone(),
        ));
    ctx.hooks
        .reconcile_hardware_boot()
        .await
        .map_err(|e| anyhow::anyhow!("hardware reconcile failed: {e}"))?;
    // Camera mount (`:150-199`): real mode only (`:170`).
    if cfg.mode == RunMode::Real {
        ops.bind_camera(Arc::new(camera_mount_reconciler(
            &ctx,
            &media_cfg.pipeline_dir,
            runner.clone(),
            ops.camera_mount_on_change(),
        )));
    }
    ctx.hooks.reconcile_camera_mount_boot().await;
    let upload = config_plane::boot_upload_options(&ctx);
    hooks.install(build_recording_manager(
        &ctx,
        source.clone(),
        media.clone(),
        upload,
        time_source.clone(),
        clock_trust.clone(),
    ));
    // Errors the TS leaves uncaught in the manager (the 5 s poll, M1): exit 1.
    let mut recording_fatal = hooks.manager().and_then(|m| m.take_fatal());
    ctx.hooks.recording_start().await;

    // Offline clock stepper (`:536-555`): real mode only.
    if cfg.mode == RunMode::Real {
        let stepper = clock_stepper(
            &ctx,
            source.clone(),
            runner.clone(),
            time_source.clone(),
            clock_trust.clone(),
        );
        stepper.start();
        ops.bind_stepper(Arc::new(stepper));
    }
    // RGB status LED (`:567-628`): built once from the boot hardware block
    // (CF61), signals {connection, recording (2 s cache, CF56), gnss,
    // starting}; `void led.start()` — never awaited.
    let led = led_manager(&ctx, &ops, source.clone());
    ops.bind_led(Arc::new(led), |fut| {
        tokio::spawn(fut);
    });

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
            // `source.obdReport?.()` / `source.obdReapply?.()` (Phase 5b I4).
            obd: source.obd(),
        },
    );
    // `requestConfigReport = (origin) => configSync.request(origin);` (`:746`):
    // camera-mount verdict changes request a report from here on.
    ops.set_report_requester(reports.clone());
    // `read_sms` (`worker.ts:917`, CF48 route): real mode → the process's ONE
    // SmsReader (lock `$XORGATE_HOME/sms-reader.lock`) — `config.mode ===
    // "real"` only, `--offline` included (the TS does not look at offline);
    // sim / replay answer from the fixture sweep (core `SmsSource::Fixture`), so
    // `SMS_READER_UNAVAILABLE` is unreachable from the worker. The command plane executes through the same
    // taped runner (I9 `sudo` records for reboot / power_off / update).
    let sms = if cfg.mode == RunMode::Real {
        command_plane::SmsSource::Real(crate::ops::sms::SmsReader::new(
            crate::ops::sms::SmsReaderDeps::system(
                Some(paths.sms_lock.clone()),
                ctx.clock.clone(),
                ctx.tape.clone(),
            ),
        ))
    } else {
        command_plane::SmsSource::Unavailable
    };
    let commands = command_plane::start_with_sms(
        &ctx,
        CommandPlaneDeps {
            mqtt: mqtt.clone(),
            reports: reports.clone(),
            // `source.obdScan?.()` / `source.obdForget?.()` (Phase 5b I4).
            obd: source.obd(),
        },
        runner.clone(),
        sms,
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
            Some(msg) = next_fatal(&mut recording_fatal) => break (
                crate::shutdown::RECORDING_FATAL,
                crate::shutdown::on_recording_fatal(&msg),
            ),
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
        link_closes: mqtt_end.is_some() && sig != crate::shutdown::LOOP_ENDED,
    }
    .run_with(sig, exit_code, |how| async move {
        match (mqtt_end, how) {
            (Some(end), ClientEnd::Graceful(budget)) => end.shutdown(budget).await,
            // X1: the TS would have crashed — no DISCONNECT, the will fires.
            (Some(end), ClientEnd::Abort) => {
                end.abort().await;
                true
            }
            (None, _) => true,
        }
    })
    .await;
    // The OBD helper's close (review-wp2 M2): the TS process exit leaves it
    // to finish (execa `cleanup` SIGTERMs it, it writes its `close` audit
    // record); here SIGTERM + a bounded wait before the runtime drop.
    source.shutdown_children().await;
    // `clockTrust?.stop()` (`worker.ts:1497` @ 09da38d), after `recording.stop()`.
    if let Some(t) = &clock_trust {
        t.stop();
    }
    ctx.tracker.close();
    let _ = tokio::time::timeout(Duration::from_millis(500), ctx.tracker.wait()).await;
    ctx.tape.close();
    sd_notify(&[sd_notify::NotifyState::Stopping]);
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D38 (Phase 5b): `sim` mode emits no synthetic `obd` group, whatever the
    /// boot hardware block declares (the TS `createSimulator({obd: true})`,
    /// `worker.ts:323-326` @ 09da38d, is not ported — README Q6). The sim
    /// source has no OBD collector, so the worker hands BOTH planes `None`:
    /// no `effective.obd`, no state.json `obd`, and `obd_scan` / `obd_forget`
    /// are acked `rejected` / `no OBD peripheral`.
    #[tokio::test]
    async fn d38_sim_mode_has_no_obd_group_and_rejects_obd_commands() {
        use xorgate_core::command_flow::{decide_command, Effect, RefreshWindow};
        use xorgate_core::commands::{parse_command, AckStatus, Ledger};
        let src: Arc<dyn TelemetrySource> = Arc::new(SimSource);
        assert!(src.obd().is_none(), "sim: no OBD collector");
        let r = src.read().await.unwrap();
        assert!(!r.metrics.contains_key("obd"), "sim: no obd group");
        for wire in ["obd_scan", "obd_forget"] {
            let c = parse_command(
                format!(
                    r#"{{"v":1,"requestId":"r-1","command":"{wire}","ts":1000,"expiresAt":61000}}"#
                )
                .as_bytes(),
            );
            let d = decide_command(
                c.as_ref(),
                1001,
                &Ledger::empty(),
                RefreshWindow { now: 0, last: None },
                RunMode::Sim,
                src.obd().is_some(),
            );
            assert!(
                d.effects.iter().any(|e| *e
                    == Effect::Ack {
                        status: AckStatus::Rejected,
                        reason: Some("no OBD peripheral".into()),
                    }),
                "{wire}: {:?}",
                d.effects
            );
            assert!(!d.effects.iter().any(|e| matches!(e, Effect::ObdJob(_))));
        }
    }

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
