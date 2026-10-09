//! Agent configuration from the environment — the same keys as
//! `/etc/xorgate/agent.env` read by `src/agent/config.ts:loadConfig()`.
//!
//! Phase 0 reads: `DEVICE_ID`, `IOT_DATA_ENDPOINT`, `CERT_PATH`, `KEY_PATH`,
//! `CA_PATH`, `MODE`, `RATE_HZ`, `MQTT_KEEPALIVE_SEC`, `OFFLINE`,
//! `XORGATE_HOME` (default `~/.xorgate`), `HARDWARE_MANIFEST` (default
//! `/etc/xorgate/hardware.json`). The KVS_* / DEVICE_DIST_* keys are present
//! in agent.env but belong to later phases and are ignored here.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone)]
pub struct AgentEnv {
    pub device_id: String,
    pub endpoint: String,
    pub mode: String,
    pub rate_hz: f64,
    pub interval_ms: u64,
    pub mqtt_keepalive_sec: u64,
    pub offline: bool,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub ca_path: PathBuf,
    pub hardware_path: PathBuf,
    pub home: PathBuf,
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn truthy(v: Option<String>) -> bool {
    matches!(v.as_deref(), Some("1") | Some("true"))
}

/// `XORGATE_HOME` or `~/.xorgate` (`src/daemon/runtime.ts:runtimeHome`).
pub fn runtime_home() -> PathBuf {
    if let Some(h) = var("XORGATE_HOME") {
        return PathBuf::from(h);
    }
    let home = var("HOME").unwrap_or_else(|| "/".to_string());
    PathBuf::from(home).join(".xorgate")
}

pub fn load(offline_flag: bool) -> Result<AgentEnv> {
    let offline = offline_flag || truthy(var("OFFLINE"));
    // Offline mode needs no real identity / endpoint: same placeholders as the TS.
    let device_id = var("DEVICE_ID").or_else(|| offline.then(|| "sim-local".to_string()));
    let endpoint = var("IOT_DATA_ENDPOINT").or_else(|| offline.then(|| "offline".to_string()));
    let device_id =
        device_id.ok_or_else(|| anyhow!("DEVICE_ID is required (or pass --offline)"))?;
    let endpoint =
        endpoint.ok_or_else(|| anyhow!("IOT_DATA_ENDPOINT is required (or pass --offline)"))?;

    let rate_hz: f64 = var("RATE_HZ")
        .unwrap_or_else(|| "1".into())
        .parse()
        .context("RATE_HZ must be a number")?;
    if !rate_hz.is_finite() || rate_hz <= 0.0 {
        return Err(anyhow!("RATE_HZ must be a positive number, got {rate_hz}"));
    }
    let mqtt_keepalive_sec: u64 = var("MQTT_KEEPALIVE_SEC")
        .unwrap_or_else(|| "60".into())
        .parse()
        .context("MQTT_KEEPALIVE_SEC must be a positive integer")?;
    if mqtt_keepalive_sec == 0 {
        return Err(anyhow!(
            "MQTT_KEEPALIVE_SEC must be a positive number, got 0"
        ));
    }
    // The TS resolves `./creds/*` relative to the CWD; same here via PathBuf.
    let cert_path = PathBuf::from(var("CERT_PATH").unwrap_or_else(|| "./creds/cert.crt".into()));
    let key_path = PathBuf::from(var("KEY_PATH").unwrap_or_else(|| "./creds/private.key".into()));
    let ca_path = PathBuf::from(var("CA_PATH").unwrap_or_else(|| "./creds/ca.pem".into()));
    let hardware_path = PathBuf::from(
        var("HARDWARE_MANIFEST").unwrap_or_else(|| "/etc/xorgate/hardware.json".into()),
    );

    Ok(AgentEnv {
        device_id,
        endpoint,
        // Phase 0 always runs the real system collector; `mode` is recorded
        // for state.json/pidfile parity with the TS (`MODE` env, default "real"
        // here rather than the TS's "sim": there is no simulator to fall back to).
        mode: var("MODE").unwrap_or_else(|| "real".into()),
        rate_hz,
        interval_ms: (1000.0 / rate_hz).round().max(1.0) as u64,
        mqtt_keepalive_sec,
        offline,
        cert_path,
        key_path,
        ca_path,
        hardware_path,
        home: runtime_home(),
    })
}

/// `rails` from the hardware block (`/etc/xorgate/hardware.json`, cm4-support):
/// `false` pre-latches the PMIC reads off. Missing/unreadable/absent key ⇒ `true`
/// (legacy CM5 behaviour: probe and latch).
pub fn hardware_rails(path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return true;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return true;
    };
    v.get("rails").and_then(|r| r.as_bool()).unwrap_or(true)
}
