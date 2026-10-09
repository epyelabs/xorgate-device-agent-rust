//! The worker's configuration, read from the process: `src/agent/config.ts`
//! `loadConfig` (the pure precedence rules are `xorgate_core::agent_config`,
//! fixture-pinned) + the env knobs other TS modules read at load time
//! (common-brief rule 6):
//!
//! | knob | TS | default |
//! |---|---|---|
//! | `XORGATE_HOME` | `runtime.ts:21` (`||`) | `os.homedir()/.xorgate` |
//! | `GPS_NMEA_PORT` | `hardware/gps.ts:15` (`??`) | `/dev/ttyUSB1` |
//! | `LTE_TELEMETRY_FILE` | `hardware/modem.ts:31` (`??`) | `/run/sim7600-lte/telemetry.json` |
//! | `XORGATE_PYTHON_DIR` | `lib/paths.ts:15` (`??`) | `<install root>/python` |
//! | `XORGATE_UPLOAD_BUCKET` | `worker.ts:392` (`||`) | — |
//! | `XORGATE_MANAGED_BY` | `worker.ts:340` (`!`) | — (self-managed) |
//! | `XORGATE_RS_TAPE` | Rust only (record/replay) | — |
//!
//! plus everything `loadConfig` reads (`DEVICE_ID`, `IOT_DATA_ENDPOINT`,
//! `CERT_PATH`, `KEY_PATH`, `CA_PATH`, `MODE`, `RATE_HZ`,
//! `MQTT_KEEPALIVE_SEC`, `OFFLINE`, `KVS_*`, `HARDWARE_MANIFEST`, …) and the
//! same `--flag value` overrides.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use xorgate_core::agent_config::{self, AgentConfig, ConfigInputs};
use xorgate_core::telemetry::RunMode;

use crate::ctx::Paths;

/// The resolved configuration of one worker run.
#[derive(Debug, Clone)]
pub struct AgentEnv {
    /// `loadConfig()` minus `creds`.
    pub config: AgentConfig,
    /// `runtimeHome()`.
    pub home: PathBuf,
    /// `!process.env.XORGATE_MANAGED_BY` (`worker.ts:340`): write/remove the pidfile.
    pub self_managed: bool,
    /// `process.env.XORGATE_UPLOAD_BUCKET || …` (`worker.ts:392`): the override, when non-empty.
    pub upload_bucket_override: Option<String>,
    /// The worker's own argv (after the program name), recorded in the pidfile.
    pub argv: Vec<String>,
}

/// The cert material read at startup (`CredBundle`, `config.ts:21-25`).
pub struct Creds {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
    pub ca: Vec<u8>,
}

fn env_var(k: &str) -> Option<String> {
    // `process.env[k]`: unset is undefined, set-but-empty is "".
    std::env::var_os(k).map(|v| v.to_string_lossy().into_owned())
}

/// `os.homedir()` (CF3): `$HOME` when set and non-empty (libuv
/// `uv_os_homedir`), else the passwd entry of the effective uid, else `/`.
pub fn os_homedir() -> String {
    if let Some(h) = env_var("HOME").filter(|h| !h.is_empty()) {
        return h;
    }
    nix::unistd::User::from_uid(nix::unistd::geteuid())
        .ok()
        .flatten()
        .map(|u| u.dir.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string())
}

/// `runtimeHome()` (`runtime.ts:20-22`).
pub fn runtime_home() -> PathBuf {
    PathBuf::from(agent_config::runtime_home(
        env_var("XORGATE_HOME").as_deref(),
        &os_homedir(),
    ))
}

/// The install root (`/opt/xorgate-rs` for `/opt/xorgate-rs/bin/xorgate-rs`),
/// the Rust stand-in for the TS `PACKAGE_ROOT`.
pub fn install_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            let bin = exe.parent()?.to_path_buf();
            Some(bin.parent().map(Path::to_path_buf).unwrap_or(bin))
        })
        .unwrap_or_else(|| PathBuf::from("/opt/xorgate-rs"))
}

/// `loadConfig(argv)` (`config.ts:197-308`) without the creds read.
/// `settings_mode` is `loadSettings().mode` from the home's settings.json.
pub fn load(argv: &[String]) -> Result<AgentEnv> {
    let home = runtime_home();
    let settings_mode: Option<RunMode> =
        xorgate_hal::settings_io::load_settings(&xorgate_hal::settings_io::settings_path(&home))
            .mode;
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string());
    let env = |k: &str| env_var(k);
    let config = agent_config::load_config(&ConfigInputs {
        argv,
        env: &env,
        settings_mode,
        cwd: &cwd,
    })
    .map_err(|e| anyhow!(e))?;
    Ok(AgentEnv {
        config,
        home,
        self_managed: env_var("XORGATE_MANAGED_BY").is_none_or(|v| v.is_empty()),
        upload_bucket_override: env_var("XORGATE_UPLOAD_BUCKET").filter(|b| !b.is_empty()),
        argv: argv.to_vec(),
    })
}

/// The creds read of `loadConfig` (`config.ts:233-247`): online only; the
/// first unreadable file fails the start with the TS message.
pub fn read_creds(cfg: &AgentConfig) -> Result<Creds> {
    let read = |p: &str| {
        std::fs::read(p).map_err(|e| {
            anyhow!(agent_config::creds_error(
                &xorgate_hal::io::node_fs_message(&e, "open", Path::new(p))
            ))
        })
    };
    Ok(Creds {
        cert: read(&cfg.cert_path)?,
        key: read(&cfg.key_path)?,
        ca: read(&cfg.ca_path)?,
    })
}

/// Every path the agent touches, resolved once.
pub fn paths(env: &AgentEnv) -> Paths {
    let h = &env.home;
    let cfg = &env.config;
    Paths {
        home: h.clone(),
        state: h.join("state.json"),
        pid: crate::pidfile::pid_path(h),
        log: h.join("agent.log"),
        settings: xorgate_hal::settings_io::settings_path(h),
        config_sync: h.join("config-sync.json"),
        commands: h.join("commands.json"),
        gnss_sky: h.join("gnss-sky.json"),
        refresh_flag: h.join("refresh-modem.flag"),
        sms_lock: h.join("sms-reader.lock"),
        kvs_manifest: PathBuf::from(&cfg.media.manifest_path),
        hardware: PathBuf::from(&cfg.hardware_path),
        pipelines: PathBuf::from(&cfg.media.pipeline_dir),
        lte_telemetry: xorgate_hal::modem_file::lte_telemetry_file(env_var("LTE_TELEMETRY_FILE")),
        gps_nmea_port: PathBuf::from(
            env_var("GPS_NMEA_PORT").unwrap_or_else(|| "/dev/ttyUSB1".to_string()),
        ),
        python_dir: env_var("XORGATE_PYTHON_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| install_root().join("python")),
        tape: env_var("XORGATE_RS_TAPE")
            .filter(|t| !t.is_empty())
            .map(PathBuf::from),
    }
}
