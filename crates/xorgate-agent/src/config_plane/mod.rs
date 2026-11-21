//! The config plane (Phase 2 WP3): the boot-time settings.json upgrades,
//! the boot `buildUploadOptions()` call, and the running plane — `ConfigSync`
//! ([`sync`]) + `handleConfigMessage` ([`message`]) — behind the frozen
//! [`ConfigPlane`] interface (`phase-2/evidence/wp1.md` "Interfaces for wave B").
//!
//! Frozen signatures kept: [`boot`], [`boot_upload_options`], [`start`].

pub mod message;
pub mod sync;

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{error, info, warn};
use xorgate_cloud::s3::{build_upload_options, UploadEnv, UploadOptions};
use xorgate_core::config_report::ReportOrigin;
use xorgate_core::settings::AgentSettings;
use xorgate_core::settings_migrate::{migrate_media_enabled, MediaEnabledMigration};
use xorgate_core::telemetry::ActivityLevel;
use xorgate_hal::atomic_write::{atomic_write_bytes_traced, AtomicWriteError};
use xorgate_hal::config_sync_file::ensure_config_sync_baseline;
use xorgate_hal::kvs_manifest_io::load_manifest;
use xorgate_hal::settings_io::read_settings_text;

use crate::ctx::{AgentCtx, ConfigPlane, ConfigPlaneDeps};
use crate::mqtt_plane::QoS;
use message::MessageHandler;
use sync::{ConfigSync, ConfigSyncOptions};

/// `JSON.stringify(settings)` — the bytes `saveSettings` writes
/// (`settings.ts:640-642`), numbers printed like JS (`5`, not `5.0`).
pub fn settings_json(settings: &AgentSettings) -> String {
    xorgate_core::js::to_json_string(&serde_json::to_value(settings).expect("settings serialise"))
}

/// `saveSettings(settings, path)` with JS-exact bytes; the error renders as
/// Node's `err.message` ([`AtomicWriteError::node_message`]).
pub fn save_settings_js(path: &Path, settings: &AgentSettings) -> Result<(), AtomicWriteError> {
    atomic_write_bytes_traced(path, settings_json(settings).as_bytes())
}

/// The boot-time settings.json upgrades (`worker.ts:92-105`), BEFORE any
/// other reader or writer touches the file: `ensureConfigSyncBaseline()`
/// FIRST (a first boot after upgrade adopts the file silently), then
/// `migrateMediaEnabled(streamKeys)` as a real local change, with its two log
/// lines verbatim.
///
/// A failed migration WRITE is fatal, as in the TS (`saveSettings` throws at
/// module top level → node exits 1; systemd restarts): logged, exit(1).
pub fn boot(ctx: &AgentCtx) {
    let paths = &ctx.paths;
    ensure_config_sync_baseline(&paths.config_sync, &paths.settings);
    let stream_keys: Vec<String> = load_manifest(&paths.kvs_manifest)
        .map(|m| m.channels.into_iter().map(|c| c.stream_key).collect())
        .unwrap_or_default();
    let migration =
        migrate_media_enabled(read_settings_text(&paths.settings).as_deref(), &stream_keys);
    if let Some(next) = &migration.save {
        if let Err(e) = save_settings_js(&paths.settings, next) {
            let msg = e.node_message();
            error!("[agent] fatal: settings.json migration write failed: {msg}");
            eprintln!("[agent] fatal: settings.json migration write failed: {msg}");
            std::process::exit(1);
        }
    }
    match migration.result {
        MediaEnabledMigration::CamerasDisabled { cameras } => {
            let list = if cameras.is_empty() {
                "(none in manifest)".to_string()
            } else {
                cameras.join(", ")
            };
            info!("[agent] mediaEnabled:false migrated to disabled cameras: {list}");
        }
        MediaEnabledMigration::Dropped => {
            info!("[agent] dropped the retired mediaEnabled setting from settings.json")
        }
        MediaEnabledMigration::None => {}
    }
}

/// `buildUploadOptions()` (`worker.ts:389-413`; CF33 / CF39): offline → no
/// log; otherwise the manifest is (re-)read and, when the manifest or the
/// bucket is missing, `[agent] segment upload disabled (…)` is logged
/// verbatim (inside `xorgate_cloud::s3::build_upload_options`). Called once
/// at boot (`worker.ts:439`, the RecordingManager's `upload`) and again on
/// every uploadBucket re-arm (`worker.ts:801`, `recording.setUpload(…)`) —
/// never cached, so a re-arm sees the persisted bucket.
pub fn boot_upload_options(ctx: &AgentCtx) -> Option<UploadOptions> {
    upload_options_builder(ctx)()
}

/// The re-armable `buildUploadOptions` closure (holds only the env, the
/// manifest path and the state handle — no `AgentCtx`, so no cycle through
/// `ctx.hooks`).
pub fn upload_options_builder(
    ctx: &AgentCtx,
) -> Arc<dyn Fn() -> Option<UploadOptions> + Send + Sync> {
    let env = ctx.env.clone();
    let state = ctx.state.clone();
    let kvs = ctx.paths.kvs_manifest.clone();
    Arc::new(move || {
        let cfg = &env.config;
        let state = state.clone();
        let kvs = kvs.clone();
        build_upload_options(
            &UploadEnv {
                offline: cfg.offline,
                env_upload_bucket: env.upload_bucket_override.as_deref(),
                thing_name: &cfg.device_id,
                cert_path: Path::new(&cfg.cert_path),
                key_path: Path::new(&cfg.key_path),
            },
            move || load_manifest(&kvs),
            // `isOnline: () => store.snapshot().connection === "connected"`.
            Arc::new(move || state.is_connected()),
        )
    })
}

/// The `ConfigSync` options the worker builds (`worker.ts:700-737`).
pub fn options_for(ctx: &AgentCtx, deps: &ConfigPlaneDeps) -> ConfigSyncOptions {
    let cfg = &ctx.env.config;
    let mqtt = deps.mqtt.clone();
    let tape = ctx.tape.clone();
    let lte = ctx.paths.lte_telemetry.clone();
    let mut o = ConfigSyncOptions::new(
        cfg.device_id.clone(),
        cfg.topics.config_reported.clone(),
        // QoS 1, retain false; fire-and-forget like the TS (no PUBACK wait).
        Arc::new(move |topic, payload| mqtt.publish(topic, payload, QoS::AtLeastOnce, false)),
        Arc::new(move |now| {
            let (tape, lte) = (tape.clone(), lte.clone());
            Box::pin(async move { Ok(sync::read_modem_bias(&lte, tape.as_ref(), now).await) })
        }),
        ctx.paths.config_sync.clone(),
        ctx.paths.settings.clone(),
        (*ctx.build).clone(),
    );
    o.mode = Some(cfg.mode.as_str().to_string());
    // `online: () => store.snapshot().connection === "connected"` (`worker.ts:712`).
    let state = ctx.state.clone();
    o.online = Some(Arc::new(move || state.is_connected()));
    // `cameraMount: () => cameraMountState` (Phase 4: None = not reconciled yet).
    let hooks = ctx.hooks.clone();
    o.camera_mount = Some(Arc::new(move || hooks.camera_mount_state()));
    let scope = deps.scope.clone();
    o.telemetry_scope = Some(Arc::new(move || Some(scope.report())));
    // `obd: () => source.obdReport?.() ?? null` (`worker.ts:790` @ 09da38d).
    if let Some(obd) = deps.obd.clone() {
        o.obd = Some(Arc::new(move || Some(obd.report())));
    }
    // `effective.cameras` (Rust-only): the per-port camera report.
    let hooks = ctx.hooks.clone();
    o.cameras = Some(Arc::new(move || hooks.cameras_report()));
    let hooks = ctx.hooks.clone();
    o.cameras_sent = Some(Arc::new(move || hooks.cameras_report_sent()));
    // `hints` (Phase 3/4 latches; T6 / CF30: all None in Phase 2).
    let hooks = ctx.hooks.clone();
    o.hints = Some(Arc::new(move || hooks.report_hints()));
    // `onEvent`: `[agent] <msg>` (warn → console.warn) + event + forced flush.
    let (state, clock) = (ctx.state.clone(), ctx.clock.clone());
    o.on_event = Some(Arc::new(move |level, msg| {
        match level {
            ActivityLevel::Warn => warn!("[agent] {msg}"),
            _ => info!("[agent] {msg}"),
        }
        state.note(
            level,
            msg,
            xorgate_core::clock::Clock::wall_ms(clock.as_ref()),
        );
    }));
    let clock = ctx.clock.clone();
    o.now = Arc::new(move || xorgate_core::clock::Clock::wall_ms(clock.as_ref()));
    o
}

/// `new ConfigSync({...})` + `requestConfigReport = …` + `startWatcher()` +
/// `startApnWatch()` (`worker.ts:700-746`), and the plane's task: config
/// messages strictly in arrival order (O11) from an UNBOUNDED queue (CF1),
/// interleaved with the report requests other subsystems make.
pub fn start(ctx: &AgentCtx, deps: ConfigPlaneDeps) -> Arc<dyn ConfigPlane> {
    let sync = ConfigSync::new(options_for(ctx, &deps));
    start_with(ctx, deps, sync)
}

/// [`start`] over a prepared [`ConfigSync`] (tests inject fakes through its options).
pub fn start_with(ctx: &AgentCtx, deps: ConfigPlaneDeps, sync: ConfigSync) -> Arc<dyn ConfigPlane> {
    sync.start_watcher();
    sync.start_apn_watch();
    let handler = MessageHandler {
        ctx: ctx.clone(),
        sync: sync.clone(),
        scope: deps.scope.clone(),
        obd: deps.obd.clone(),
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut reports = deps.report_rx;
    let cancel = ctx.cancel.clone();
    let req = sync.clone();
    ctx.spawn("config_plane", async move {
        let mut reports_open = true;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                m = rx.recv() => match m {
                    Some(payload) => handler.handle(&payload),
                    None => break,
                },
                r = reports.recv(), if reports_open => match r {
                    Some(origin) => req.request(origin),
                    None => reports_open = false,
                },
            }
        }
    });
    Arc::new(Plane { sync, tx })
}

struct Plane {
    sync: ConfigSync,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ConfigPlane for Plane {
    /// `configSync.request("boot"); configSync.startHeartbeat();` (`worker.ts:1186-1187`).
    fn on_connect(&self) {
        self.sync.request(ReportOrigin::Boot);
        self.sync.start_heartbeat();
    }

    fn on_message(&self, payload: Vec<u8>) {
        let _ = self.tx.send(payload);
    }

    /// `configSync.stop()` (`worker.ts:1305`).
    fn stop(&self) {
        self.sync.stop();
    }
}
