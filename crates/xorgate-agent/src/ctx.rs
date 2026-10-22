//! The spine's shared context and the FROZEN interfaces wave B builds on
//! (Phase 2 WP1; listed in `phase-2/evidence/wp1.md` "Interfaces for wave B").
//!
//! Runtime model (README §3.2): one tokio runtime, a [`CancellationToken`] +
//! [`TaskTracker`] in [`AgentCtx`], each subsystem a task owning its own
//! state; cross-subsystem reads go through cheap handles ([`StateHandle`],
//! [`MqttHandle`](crate::mqtt_plane::MqttHandle),
//! [`ScopeHandle`](crate::scope::ScopeHandle)) or `watch` snapshots
//! (connection state). The traits here are what the worker wires:
//!
//! | trait / type | implemented by | TS counterpart |
//! |---|---|---|
//! | [`TelemetrySource`] | WP2 `collectors::Collectors` | `createCollector(…)` (`collectors/index.ts:84-191`) |
//! | [`ConfigPlane`] | WP3 `config_plane` | `ConfigSync` + `handleConfigMessage` |
//! | [`CommandPlane`] | WP4 `command_plane` | `handleCommandMessage` |
//! | [`ReportRequester`] | (concrete) | `configSync.request(origin)` / `requestConfigReport` |
//! | [`PhaseHooks`] | Phase 3/4 | `recording.*`, `led.*`, `reconcile*Now`, clock stepper (O5) |
//! | [`ObdControl`] | Phase 5b WP2 `collectors::obd` | `source.obd*()` (`worker.ts:293-297` @ 09da38d) |

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use xorgate_core::build_info::BuildInfo;
use xorgate_core::clock::Clock;
use xorgate_core::config_report::{ReportHints, ReportOrigin};
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::obd::ObdScanResult;
use xorgate_core::telemetry::{
    MetricGroup, ObdStatus, RecordingStatus, TelemetryMedia, TelemetryReading,
};
use xorgate_core::types::{CameraMountState, GnssView, GpsClockReading, PowerSampleReading};
use xorgate_hal::io::TapeSink;

use crate::env::AgentEnv;
use crate::mqtt_plane::MqttHandle;
use crate::scope::ScopeHandle;
use crate::state::StateHandle;

/// A boxed `Send` future (object-safe async trait methods without a crate).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Every file the agent reads or writes, resolved once at boot.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `XORGATE_HOME` (`runtime.ts:20`).
    pub home: PathBuf,
    /// `<home>/state.json` (`runtime.ts:32`).
    pub state: PathBuf,
    /// `<home>/agent.pid` (`runtime.ts:24`).
    pub pid: PathBuf,
    /// `<home>/agent.log` (`runtime.ts:28`) — only the TS self-managed daemon writes it (O9).
    pub log: PathBuf,
    /// `<home>/settings.json` (`settings.ts:554`).
    pub settings: PathBuf,
    /// `<home>/config-sync.json` (`config-sync.ts:192`).
    pub config_sync: PathBuf,
    /// `<home>/commands.json` (`commands.ts:167`).
    pub commands: PathBuf,
    /// `<home>/gnss-sky.json` (`collectors/gps.ts:82,114`).
    pub gnss_sky: PathBuf,
    /// `<home>/refresh-modem.flag` (`worker.ts:1058`).
    pub refresh_flag: PathBuf,
    /// `<home>/sms-reader.lock` (`worker.ts:917`).
    pub sms_lock: PathBuf,
    /// `KVS_MANIFEST` (default `./creds/kvs.json`, resolved; `config.ts:259`).
    pub kvs_manifest: PathBuf,
    /// `HARDWARE_MANIFEST` (default `/etc/xorgate/hardware.json`; `config.ts:283`).
    pub hardware: PathBuf,
    /// `KVS_PIPELINE_DIR` (default `/etc/xorgate/pipelines`; `config.ts:262`).
    pub pipelines: PathBuf,
    /// `LTE_TELEMETRY_FILE ?? /run/sim7600-lte/telemetry.json` (`modem.ts:31`).
    pub lte_telemetry: PathBuf,
    /// `GPS_NMEA_PORT ?? /dev/ttyUSB1` (`hardware/gps.ts:15`).
    pub gps_nmea_port: PathBuf,
    /// `XORGATE_PYTHON_DIR ?? <install root>/python` (`lib/paths.ts:15`).
    pub python_dir: PathBuf,
    /// `XORGATE_RS_TAPE` (record/replay), when set.
    pub tape: Option<PathBuf>,
}

/// Everything a subsystem needs from the spine. Cheap to clone.
#[derive(Clone)]
pub struct AgentCtx {
    /// Resolved configuration (core `AgentConfig` + the env knobs).
    pub env: Arc<AgentEnv>,
    pub paths: Arc<Paths>,
    /// This binary's identity (`effective.agent.build`).
    pub build: Arc<BuildInfo>,
    /// The record/replay sink ([`xorgate_hal::io::NullTape`] unless `XORGATE_RS_TAPE`).
    pub tape: Arc<dyn TapeSink>,
    /// state.json.
    pub state: StateHandle,
    /// Phase 3/4 call sites (no-ops in Phase 2, O5).
    pub hooks: Arc<dyn PhaseHooks>,
    /// The device hardware block in force (`hardwareBlock`, `worker.ts:114`):
    /// loaded at boot, replaced by the config plane when the cloud sends a
    /// changed one (`send_replace`), read by everyone else (`borrow()`).
    pub hardware: Arc<watch::Sender<Option<DeviceHardwareBlock>>>,
    /// `Date.now()` / `monotonicMs()` for every subsystem. The system clock in
    /// production; tests use [`AgentClock::following_tokio`] so
    /// `tokio::time::pause`/`advance` move the wall clock too.
    pub clock: Arc<AgentClock>,
    /// Cancelled when shutdown begins (= the TS `shuttingDown` flag).
    pub cancel: CancellationToken,
    /// Every long-lived task is spawned on this tracker.
    pub tracker: TaskTracker,
}

impl AgentCtx {
    /// `Date.now()` (stamps and names only — invariant 2).
    pub fn now_ms(&self) -> i64 {
        self.clock.wall_ms()
    }

    /// The TS `shuttingDown` flag.
    pub fn shutting_down(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Spawn a supervised subsystem task. A panic inside it is caught and
    /// reported like the TS last-resort net (`worker.ts:656-662`):
    /// `[agent] unhandledRejection <msg>` + `setError("unhandledRejection: <msg>")`.
    pub fn spawn<F>(&self, name: &'static str, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let inner = tokio::spawn(fut);
        let state = self.state.clone();
        let clock = self.clock.clone();
        self.tracker.spawn(async move {
            if let Err(e) = inner.await {
                if e.is_panic() {
                    let p = e.into_panic();
                    let msg = p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "panic".to_string());
                    let msg = format!("{name}: {msg}");
                    tracing::error!("[agent] unhandledRejection {msg}");
                    let t = clock.wall_ms();
                    state.set_error(&format!("unhandledRejection: {msg}"), t);
                    state.flush(t, true);
                }
            }
        });
    }
}

/// The agent's clock (implements `xorgate_core::clock::Clock`).
///
/// `mono_ms` is `tokio::time::Instant` elapsed since the clock was built, so
/// it follows `tokio::time::pause`/`advance` in tests; `wall_ms` is
/// `SystemTime` ([`AgentClock::system`]) or `base + mono`
/// ([`AgentClock::following_tokio`]).
#[derive(Debug)]
pub struct AgentClock {
    origin: tokio::time::Instant,
    /// `None` = the system wall clock; `Some(base)` = `base + mono`.
    wall_base: Option<i64>,
}

impl AgentClock {
    /// Production: wall = `SystemTime::now()`.
    pub fn system() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
            wall_base: None,
        }
    }

    /// Tests: wall starts at `base` and advances with tokio's (pausable) clock.
    pub fn following_tokio(base: i64) -> Self {
        Self {
            origin: tokio::time::Instant::now(),
            wall_base: Some(base),
        }
    }
}

impl xorgate_core::clock::Clock for AgentClock {
    fn wall_ms(&self) -> i64 {
        match self.wall_base {
            None => crate::now_ms(),
            Some(base) => base + self.mono_ms() as i64,
        }
    }

    fn mono_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

/// One group's cached reading + when its data last changed (`GroupSample`,
/// `collectors/index.ts:12-17`). `updated_mono` is the recorder's freshness
/// key (TS `process.hrtime.bigint()`).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupSample {
    pub fields: MetricGroup,
    pub updated_mono: tokio::time::Instant,
}

/// The telemetry source the publish tick reads (`source` in `worker.ts:270`):
/// the real collectors (WP2) in `real` mode, an empty source in `sim` (O7).
pub trait TelemetrySource: Send + Sync + 'static {
    /// One reading per tick (`source.read()`): non-empty groups only, imu →
    /// gps → lte → system; `media` is the idle block when no supervisor runs.
    /// An `Err` becomes `read error: <msg>` in state.json (`worker.ts:1245-1250`).
    fn read(&self) -> BoxFut<'_, Result<TelemetryReading, String>>;
    /// Recorder view (`sampleGroups()`, Phase 3 consumer).
    fn sample_groups(&self) -> BoxFut<'_, IndexMap<String, GroupSample>>;
    /// Power reading for the shed policy (`samplePower()`, Phase 3 consumer).
    fn sample_power(&self) -> BoxFut<'_, Option<PowerSampleReading>>;
    /// Latest fresh GPS time (`gpsClock()`, Phase 4 clock stepper).
    fn gps_clock(&self) -> Option<GpsClockReading>;
    /// GNSS view (`gnssView()`, Phase 3 video gate / Phase 4 LED).
    fn gnss_view(&self) -> Option<GnssView>;
    /// Stop supervised children (`source.stop?.()`, shutdown step).
    fn stop(&self);
    /// The OBD collector (`source.obd*?.()`, Phase 5b I4); `None` without
    /// `peripherals.obd` and in sim mode.
    fn obd(&self) -> Option<Arc<dyn ObdControl>> {
        None
    }
}

/// The KVS media supervisor as the collectors and the recording spine use it
/// (`MediaSupervisor`, `collectors/media.ts`; WP5's `media::MediaSupervisor`).
/// Phase 3 addition. `set_shed` is called ONLY from the recording manager's
/// live choke point (invariant 6).
pub trait MediaControl: Send + Sync + 'static {
    /// `media.read()`: the `media` block; the FIRST call starts the masters
    /// (lazy start from the publish tick, TS parity).
    fn read(&self) -> TelemetryMedia;
    /// `media.stop()` (shutdown: SIGINT every master).
    fn stop(&self);
    /// `restartChannel(key)` — bounce a master onto a respawned capture's socket.
    fn restart_channel(&self, key: &str);
    /// `setShed(shed)` — park/release every live master.
    fn set_shed(&self, shed: bool);
    /// `setDisabledStreams(keys)` — hold down disabled cameras' masters.
    fn set_disabled_streams(&self, keys: &[String]);
}

/// The OBD collector as the spine uses it (Phase 5b I4; the TS
/// `source.obdStatus/obdReport/obdReapply/obdScan/obdForget`,
/// `worker.ts:293-297` @ 09da38d). WP2's `collectors::obd` implements it.
///
/// The worker holds `Option<Arc<dyn ObdControl>>`: `None` = the BOOT-time
/// hardware block declares no `peripherals.obd` = every `source.obd*?.()`
/// of the TS returns `null`/`undefined` → state.json without `obd`, the
/// report without `effective.obd`, `obd_*` commands acked `rejected` with
/// reason `no OBD peripheral`.
pub trait ObdControl: Send + Sync + 'static {
    /// `collector.status()` — state.json `obd` every publish tick.
    fn status(&self) -> ObdStatus;
    /// `collector.report()` — `effective.obd` (I2: already in the TS key
    /// order of `collectors/obd.ts:712-731`, opaque to the config plane).
    fn report(&self) -> serde_json::Value;
    /// `collector.reapply()` — after EVERY cloud config message (`worker.ts:984-987`).
    fn reapply(&self);
    /// `collector.scan()` — joins an in-flight scan. The TS emits the
    /// `scanning for Bluetooth adapters …` event synchronously when called
    /// (before the command ack); an implementation does the same before
    /// returning the future.
    fn scan(&self) -> BoxFut<'static, ObdScanResult>;
    /// `collector.forget()` — resolves to the forgotten MAC (`None` = no
    /// adapter was configured). Like `scan()`, the TS `async forget()` runs
    /// its synchronous part at CALL time, before the command ack: the
    /// not-configured latch, `goOff` (helper SIGINT) and its event
    /// (`adapter <mac> forgotten; …` / `forget: no adapter was configured`).
    /// An implementation does all of that before returning the future; only
    /// the bond removal is awaited.
    fn forget(&self) -> BoxFut<'static, Option<String>>;
}

/// The config plane (WP3): `ConfigSync` + `handleConfigMessage`. Every method
/// is non-blocking — an implementation pushes onto a LOSSLESS (unbounded)
/// queue drained by its own task, which handles config messages strictly in
/// arrival order (O11). A bounded `try_send` would bring back the CF1 loss
/// one hop later (review-wp1 I-a).
pub trait ConfigPlane: Send + Sync + 'static {
    /// The connect handler's `configSync.request("boot"); configSync.startHeartbeat();`
    /// (`worker.ts:1186-1187`), called after the subscribes.
    fn on_connect(&self);
    /// One message on `…/config` (retained redeliveries included), raw bytes.
    fn on_message(&self, payload: Vec<u8>);
    /// `configSync.stop()` (shutdown step, `worker.ts:1305`): no report may be
    /// queued after this returns.
    fn stop(&self);
}

/// What the worker hands the config plane (WP3) at construction
/// (`new ConfigSync({...})`, `worker.ts:700-737`).
pub struct ConfigPlaneDeps {
    /// Report publishes (QoS 1, fire-and-forget like the TS; queued by the
    /// client while the link is down). The TS `online()` gate
    /// (`store.snapshot().connection === "connected"`, `worker.ts:712`) is
    /// `ctx.state.is_connected()`, not `mqtt.connected()` (review-wp1 I-c).
    pub mqtt: MqttHandle,
    /// `telemetryScope.report()` + `applyScope` for config messages.
    pub scope: ScopeHandle,
    /// A requester for the plane's own use (e.g. handing to timers).
    pub reports: ReportRequester,
    /// Every `request(origin)` made by others ([`ReportRequester::request`]).
    pub report_rx: mpsc::UnboundedReceiver<ReportOrigin>,
    /// The OBD collector (Phase 5b I4): `effective.obd` provider
    /// (`obd: () => source.obdReport?.() ?? null`, `worker.ts:790`) and the
    /// `reapply()` after every cloud config message (`:984-987`). `None` =
    /// no `peripherals.obd`.
    pub obd: Option<Arc<dyn ObdControl>>,
}

/// What the worker hands the command plane (WP4).
pub struct CommandPlaneDeps {
    /// Acks (`publish_acked`, 3 s) and sms results (`publish`, QoS 1).
    pub mqtt: MqttHandle,
    /// `configSync.request("boot")` for `refresh` (now and at +45 s).
    pub reports: ReportRequester,
    /// The OBD collector for `obd_scan` / `obd_forget` (Phase 5b I4); `None`
    /// = rejected with `no OBD peripheral` (`worker.ts:1217-1226` @ 09da38d).
    pub obd: Option<Arc<dyn ObdControl>>,
}

/// The command plane (WP4): `handleCommandMessage`. Non-blocking: push onto a
/// LOSSLESS (unbounded) queue (review-wp1 I-a); decide + claim run
/// sequentially on the plane's own task, execution is spawned (O11).
pub trait CommandPlane: Send + Sync + 'static {
    /// One message on `…/command`, raw bytes.
    fn on_message(&self, payload: Vec<u8>);
}

/// `configSync.request(origin)` for everyone who is not the config plane
/// (scope changes, `refresh`, the camera-mount reconcile, the CLI's
/// fingerprint watcher lives inside WP3). Cloneable, never blocks, never
/// fails; requests made before the config plane starts are kept and drained
/// by it (the TS `requestConfigReport` is null until then and drops them —
/// harmless, the connect handler requests `boot` anyway).
#[derive(Clone, Debug)]
pub struct ReportRequester {
    tx: mpsc::UnboundedSender<ReportOrigin>,
}

impl ReportRequester {
    /// The requester and the receiving end the config plane drains.
    pub fn channel() -> (ReportRequester, mpsc::UnboundedReceiver<ReportOrigin>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ReportRequester { tx }, rx)
    }

    /// Ask for a report (`request(origin)`, debounced/coalesced by the plane).
    pub fn request(&self, origin: ReportOrigin) {
        let _ = self.tx.send(origin);
    }
}

/// The Phase 3 / Phase 4 call sites of `worker.ts`, kept at their exact
/// places as no-ops (O5). Every method has the inert default, so the Phase 2
/// [`NoopHooks`] is empty and the owning phase overrides only its own.
pub trait PhaseHooks: Send + Sync + 'static {
    /// `recording.notifyConnectivity(online)` (connect `:1158`, close `:1282`).
    fn notify_connectivity(&self, _online: bool) {}
    /// `recording.status()` — the payload's `recording` block (`:1222`) and
    /// the report hints. Phase 2: the all-off block.
    fn recording_status(&self) -> RecordingStatus {
        RecordingStatus::all_off()
    }
    /// The tick's media overlay (`:1209-1232`): `recording.videoStatus()`
    /// merged into the collector's media block. Phase 2: no capture, so
    /// `media.recording` is `{value:false}` and streams pass through.
    fn overlay_media(&self, mut media: TelemetryMedia) -> TelemetryMedia {
        media.recording.value = false;
        media
    }
    /// `recording.reapply()` + `replay?.reapply()` (config path, both branches, `:878-881`).
    /// `Err` = the reapply threw inside the config message's try: the caller
    /// skips the camera-mount reconcile and logs `cloud config apply failed`
    /// (Phase 3 widening, review-wp1 M1).
    fn reapply_recording(&self) -> Result<(), String> {
        Ok(())
    }
    /// `recording.setUpload(buildUploadOptions())` after an `uploadBucket`
    /// backfill (`:801`).
    fn rearm_upload(&self) {}
    /// `reconcileHardwareNow()` + master bounce + `recording.reapply()` after a
    /// changed hardware block (`:839-845`).
    fn hardware_changed(&self) {}
    /// `void reconcileCameraMountNow()` — fire-and-forget: the config path
    /// (`:889`, WP3) and the worker's 60 s poll (`:648-651`).
    fn reconcile_camera_mount(&self) {}
    /// `await reconcileHardwareNow()` at boot (`:427`), before anything reads
    /// the pipelines. Awaited by the worker. `Err` = the pass threw (a
    /// top-level-await rejection in the TS, before the `:656` net): the
    /// worker exits non-zero (Phase 3 widening, review-wp5 M1).
    fn reconcile_hardware_boot(&self) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// `await reconcileCameraMountNow()` at boot (`:432`), after the hardware
    /// reconcile and before the report plane. Awaited by the worker.
    fn reconcile_camera_mount_boot(&self) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
    /// `await recording.start()` (`:529`), after the recording manager is
    /// built (`buildUploadOptions`, CF33). Awaited by the worker.
    fn recording_start(&self) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
    /// `noteRecordingStatus(recordingStatus)` from the publish tick (`:1207`;
    /// the LED's recording view, `:581-587`).
    fn note_recording_status(&self, _status: &RecordingStatus) {}
    /// `store.setRecording(recording.status())` at shutdown (`:1307`), after
    /// `recording_stop`. `None` writes nothing — Phase 2 keeps state.json's
    /// `recording` absent (T6).
    fn final_recording_status(&self) -> Option<RecordingStatus> {
        None
    }
    /// `cameraMountState` (Phase 4). `None` = not reconciled yet.
    fn camera_mount_state(&self) -> Option<CameraMountState> {
        None
    }
    /// ConfigSync `hints()` (`:727-734`): bootDefer / powerShed / clockStepperArmed.
    ///
    /// Called while ConfigSync holds its core mutex (as are `camera_mount_state`
    /// and the telemetry-scope closure, `sync.rs publish_report`): an
    /// implementation must not take a lock whose holder can call into
    /// ConfigSync (`request()`, `note_cloud_config`) — deadlock (review-wp3 #5,
    /// Phase 3/4 rule).
    fn report_hints(&self) -> ReportHints {
        ReportHints::default()
    }
    /// The publish tick's clock trust gate (Phase 5b, `worker.ts:1406-1412`
    /// @ 09da38d): `true` = publish; `false` = withheld (the hook has already
    /// counted it with `noteHeld()`). Called AFTER `seq += 1`, `setLatest`
    /// and the OBD status edge, BEFORE `telemetryScope.noteTick` (CF72).
    /// Without a clock trust gate (sim, Phase 2–4 hooks): always `true`.
    fn publish_clock_gate(&self) -> bool {
        true
    }
    /// `led.tick()` from the 1 s heartbeat (`:635`).
    fn led_tick(&self) {}
    /// `led.configApplied()` on a real cloud-config change (`:866`).
    fn led_config_applied(&self) {}
    /// Shutdown steps `led.stop()` (`:1299`) and `recording.stop()` (`:1308`).
    fn led_stop(&self) {}
    /// `Err` = the telemetry finalize threw (D29): the TS crashed at
    /// `worker.ts:1306`, so [`crate::shutdown::Shutdown`] skips `online:false`
    /// and drops the link without a DISCONNECT, firing the Last-Will
    /// (review-wp1 X1). The local teardown still runs.
    fn recording_stop(&self) -> Result<(), String> {
        Ok(())
    }
}

/// The Phase 2 hooks: every call site inert.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHooks;

impl PhaseHooks for NoopHooks {}
