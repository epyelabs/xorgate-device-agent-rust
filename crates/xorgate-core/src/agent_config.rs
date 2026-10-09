//! The worker's configuration — the pure half of `src/agent/config.ts`
//! (`parseArgs` `:166-186`, `loadConfig` `:197-308`) and of
//! `src/daemon/runtime.ts:runtimeHome` (`:20-22`).
//!
//! Everything the TS reads from the process (argv, `process.env`, the cwd for
//! `path.resolve`, `os.homedir()`, the `mode` out of settings.json) is handed
//! in through [`ConfigInputs`], so the precedence rules can be pinned by a
//! fixture dumped from the real `loadConfig` (`fixtures/agent_config/`). The
//! one I/O step of `loadConfig` — reading the cred files (`:233-247`) — stays
//! with the caller (`xorgate-agent::env`), which formats the same error text
//! via [`creds_error`].
//!
//! JS semantics kept on purpose:
//! - `get(cli, env, fallback)` is `flag ?? env ?? fallback`: an env var set to
//!   the EMPTY string wins over the fallback (`DEVICE_ID=` → `""` → the
//!   "is required" error; `RATE_HZ=` → `Number("")` = 0 → the range error).
//! - `XORGATE_HOME` is `||`: empty means unset (`runtime.ts:21`).
//! - Numbers are `Number(string)` ([`crate::system::js_number`]): `"60.0"`,
//!   `" 60 "`, `"0x3c"` are all 60; `"abc"` is NaN and rejected (CF3).
//! - Paths go through `path.resolve` ([`resolve_path`]).

use crate::round::js_math_round;
use crate::system::js_number;
use crate::telemetry::RunMode;
use crate::topics::Topics;

/// `BOOLEAN_FLAGS` (`config.ts:164`): flags that take no value.
pub const BOOLEAN_FLAGS: &[&str] = &["offline"];

/// One parsed `--flag` (`parseArgs`'s `string | boolean`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgValue {
    Str(String),
    Flag,
}

/// `parseArgs` (`config.ts:173-191`): `--flag value` and boolean `--flag`.
/// Every non-boolean flag consumes the next token as its value, even one that
/// starts with `--`; a trailing valueless flag is `true`. Later flags win.
/// Insertion order is kept (a re-set key keeps its first position, like a JS
/// object property).
pub fn parse_args(argv: &[String], boolean_flags: &[&str]) -> Vec<(String, ArgValue)> {
    let mut out: Vec<(String, ArgValue)> = Vec::new();
    let mut set = |k: &str, v: ArgValue| {
        if let Some(slot) = out.iter_mut().find(|(key, _)| key == k) {
            slot.1 = v;
        } else {
            out.push((k.to_string(), v));
        }
    };
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if let Some(key) = a.strip_prefix("--") {
            if boolean_flags.contains(&key) {
                set(key, ArgValue::Flag);
            } else if let Some(next) = argv.get(i + 1) {
                set(key, ArgValue::Str(next.clone()));
                i += 1;
            } else {
                set(key, ArgValue::Flag);
            }
        }
        i += 1;
    }
    out
}

fn arg_str<'a>(args: &'a [(String, ArgValue)], key: &str) -> Option<&'a str> {
    args.iter().find_map(|(k, v)| match v {
        ArgValue::Str(s) if k == key => Some(s.as_str()),
        _ => None,
    })
}

fn arg_truthy(args: &[(String, ArgValue)], key: &str) -> bool {
    args.iter().any(|(k, v)| {
        k == key
            && match v {
                ArgValue::Flag => true,
                ArgValue::Str(s) => s == "1" || s == "true",
            }
    })
}

/// `truthy` (`config.ts:188`) over an env value.
fn env_truthy(v: Option<&str>) -> bool {
    matches!(v, Some("1") | Some("true"))
}

/// Node's `path.resolve(p)` (POSIX) against `cwd`: an absolute `p` is
/// normalised as-is, a relative one is joined onto `cwd` first; `.`/empty
/// segments drop, `..` pops (never above `/`), no trailing slash.
/// `resolve("")` is the cwd.
pub fn resolve_path(cwd: &str, p: &str) -> String {
    let joined = if p.starts_with('/') {
        p.to_string()
    } else {
        format!("{cwd}/{p}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `runtimeHome()` (`runtime.ts:20-22`): `XORGATE_HOME || join(homedir(), ".xorgate")`.
/// `homedir` is what `os.homedir()` returned (the caller resolves `$HOME` /
/// the passwd entry — CF3).
pub fn runtime_home(xorgate_home: Option<&str>, homedir: &str) -> String {
    match xorgate_home {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => join(homedir, ".xorgate"),
    }
}

/// `path.join(a, b)` for the simple two-segment case the agent uses.
fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    let a = a.trim_end_matches('/');
    if a.is_empty() {
        format!("/{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `dirname` (POSIX) of an absolute, already-resolved path.
fn dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

/// `MediaConfig` (`config.ts:26-87`) minus `streamGuard` (a worker callback).
/// Phase 2 only reads `enabled`, `manifest_path` and `pipeline_dir`; the rest
/// is resolved now so Phase 3 finds the same defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaConfig {
    /// `!offline` (`config.ts:254`).
    pub enabled: bool,
    pub binary_path: String,
    pub lib_dir: String,
    pub manifest_path: String,
    pub source: String,
    pub media_type: String,
    pub pipeline_dir: String,
    pub pipeline_path: Option<String>,
    pub log_level: String,
    pub idle_teardown_secs: String,
    pub cert_path: String,
    pub key_path: String,
    pub ca_cert_path: String,
}

/// `ReplayConfig` (`config.ts:90-98`). Replay mode itself refuses to start
/// in the Rust agent (O7); the paths are still resolved like the TS does.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayConfig {
    pub assets_dir: String,
    pub default_scenario_id: Option<String>,
}

/// `AgentConfig` (`config.ts:100-160`) without the loaded `creds` bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub device_id: String,
    pub endpoint: String,
    pub mode: RunMode,
    pub rate_hz: f64,
    /// `Math.round(1000 / rateHz)`.
    pub interval_ms: f64,
    /// `Number(MQTT_KEEPALIVE_SEC)`; any finite positive number (CF3).
    pub mqtt_keepalive_sec: f64,
    pub offline: bool,
    pub topics: Topics,
    pub cert_path: String,
    pub key_path: String,
    pub ca_path: String,
    pub media: MediaConfig,
    pub hardware_path: String,
    pub replay: ReplayConfig,
}

/// What `loadConfig` reads from the process, injected.
pub struct ConfigInputs<'a> {
    /// `process.argv.slice(2)` — the worker's own arguments.
    pub argv: &'a [String],
    /// `process.env[key]` (`None` = unset; `Some("")` = set to empty).
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// `loadSettings().mode` (settings.json, read by the caller).
    pub settings_mode: Option<RunMode>,
    /// `process.cwd()` for `path.resolve`.
    pub cwd: &'a str,
}

/// `loadConfig` (`config.ts:197-308`) minus the creds read. The error text is
/// the TS `Error.message`, verbatim.
pub fn load_config(inp: &ConfigInputs<'_>) -> Result<AgentConfig, String> {
    let args = parse_args(inp.argv, BOOLEAN_FLAGS);
    let env = |k: &str| (inp.env)(k);
    // `get` (`config.ts:200-203`): flag (only when a string) ?? env ?? fallback.
    let get = |cli: &str, envk: &str, fallback: Option<&str>| -> Option<String> {
        arg_str(&args, cli)
            .map(str::to_string)
            .or_else(|| env(envk))
            .or_else(|| fallback.map(str::to_string))
    };
    let resolve = |p: &str| resolve_path(inp.cwd, p);

    let offline = arg_truthy(&args, "offline") || env_truthy(env("OFFLINE").as_deref());

    // Mode precedence (`:206-210`): --mode > settings.json > MODE > "sim".
    let mode_text: String = match arg_str(&args, "mode") {
        Some(m) => m.to_string(),
        None => match inp.settings_mode {
            Some(m) => m.as_str().to_string(),
            None => env("MODE").unwrap_or_else(|| "sim".to_string()),
        },
    };
    let rate_hz = js_number(&get("rate", "RATE_HZ", Some("1")).unwrap_or_default());
    let mqtt_keepalive_sec =
        js_number(&get("mqtt-keepalive", "MQTT_KEEPALIVE_SEC", Some("60")).unwrap_or_default());

    let device_id = get(
        "device-id",
        "DEVICE_ID",
        if offline { Some("sim-local") } else { None },
    );
    let endpoint = get(
        "endpoint",
        "IOT_DATA_ENDPOINT",
        if offline { Some("offline") } else { None },
    );

    let cert_path =
        resolve(&get("cert", "CERT_PATH", Some("./creds/cert.crt")).unwrap_or_default());
    let key_path =
        resolve(&get("key", "KEY_PATH", Some("./creds/private.key")).unwrap_or_default());
    let ca_path = resolve(&get("ca", "CA_PATH", Some("./creds/ca.pem")).unwrap_or_default());

    // `:223-232`, in the TS order. `!deviceId` is also true for "".
    let device_id = match device_id {
        Some(d) if !d.is_empty() => d,
        _ => return Err("DEVICE_ID (or --device-id) is required (or pass --offline)".to_string()),
    };
    let endpoint = match endpoint {
        Some(e) if !e.is_empty() => e,
        _ => {
            return Err(
                "IOT_DATA_ENDPOINT (or --endpoint) is required (or pass --offline)".to_string(),
            )
        }
    };
    if !rate_hz.is_finite() || rate_hz <= 0.0 {
        return Err(format!(
            "RATE_HZ must be a positive number, got {}",
            crate::js::number_to_string(rate_hz)
        ));
    }
    if !mqtt_keepalive_sec.is_finite() || mqtt_keepalive_sec <= 0.0 {
        return Err(format!(
            "MQTT_KEEPALIVE_SEC must be a positive number, got {}",
            crate::js::number_to_string(mqtt_keepalive_sec)
        ));
    }
    let Some(mode) = RunMode::parse(&mode_text) else {
        return Err(format!(
            "MODE must be \"sim\", \"real\" or \"replay\", got \"{mode_text}\""
        ));
    };

    // Media runtime (`:254-279`).
    let media_bin = resolve(
        &get(
            "media-bin",
            "KVS_RUNTIME_BIN",
            Some("/opt/xorgate/media/xorgate-kvs-webrtc"),
        )
        .unwrap_or_default(),
    );
    let default_cacert = join(&dirname(&media_bin), "cert.pem");
    let media = MediaConfig {
        enabled: !offline,
        lib_dir: resolve(
            &get(
                "media-lib",
                "KVS_RUNTIME_LIB_DIR",
                Some("/opt/xorgate/media/lib"),
            )
            .unwrap_or_default(),
        ),
        manifest_path: resolve(
            &get("media-manifest", "KVS_MANIFEST", Some("./creds/kvs.json")).unwrap_or_default(),
        ),
        source: get("media-source", "MEDIA_SOURCE", Some("testsrc")).unwrap_or_default(),
        media_type: get("media-type", "MEDIA_TYPE", Some("video-only")).unwrap_or_default(),
        pipeline_dir: resolve(
            &get(
                "media-pipeline-dir",
                "KVS_PIPELINE_DIR",
                Some("/etc/xorgate/pipelines"),
            )
            .unwrap_or_default(),
        ),
        // `process.env.KVS_GST_PIPELINE ? resolve(…) : undefined` — truthy, so "" is unset.
        pipeline_path: env("KVS_GST_PIPELINE")
            .filter(|p| !p.is_empty())
            .map(|p| resolve(&p)),
        log_level: get("media-log", "AWS_KVS_LOG_LEVEL", Some("3")).unwrap_or_default(),
        idle_teardown_secs: get("media-idle-teardown", "KVS_IDLE_TEARDOWN_SECS", Some("30"))
            .unwrap_or_default(),
        cert_path: cert_path.clone(),
        key_path: key_path.clone(),
        ca_cert_path: resolve(
            &get("media-cacert", "KVS_CACERT_PATH", Some(&default_cacert)).unwrap_or_default(),
        ),
        binary_path: media_bin,
    };

    let hardware_path = resolve(
        &get(
            "hardware",
            "HARDWARE_MANIFEST",
            Some("/etc/xorgate/hardware.json"),
        )
        .unwrap_or_default(),
    );
    let replay = ReplayConfig {
        assets_dir: resolve(
            &get("replay-assets", "XORGATE_REPLAY_ASSETS", Some("/assets/v1")).unwrap_or_default(),
        ),
        default_scenario_id: get("replay-scenario", "XORGATE_REPLAY_SCENARIO", None),
    };

    Ok(AgentConfig {
        topics: Topics::for_device(&device_id),
        device_id,
        endpoint,
        mode,
        rate_hz,
        interval_ms: js_math_round(1000.0 / rate_hz),
        mqtt_keepalive_sec,
        offline,
        cert_path,
        key_path,
        ca_path,
        media,
        hardware_path,
        replay,
    })
}

/// The creds-read failure message (`config.ts:241-245`); `fs_message` is the
/// Node `err.message` of the failed `readFileSync` (see
/// `xorgate_hal::io::node_fs_message`).
pub fn creds_error(fs_message: &str) -> String {
    format!(
        "failed to read creds ({fs_message}). Provision the device into ./creds, or run with --offline."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_matches_node_path_resolve() {
        assert_eq!(
            resolve_path("/var/lib/xorgate", "./creds/cert.crt"),
            "/var/lib/xorgate/creds/cert.crt"
        );
        assert_eq!(resolve_path("/a/b", "../c/"), "/a/c");
        assert_eq!(resolve_path("/a/b", ""), "/a/b");
        assert_eq!(resolve_path("/", "../../x"), "/x");
        assert_eq!(
            resolve_path("/a", "/etc//xorgate/./kvs.json"),
            "/etc/xorgate/kvs.json"
        );
    }

    #[test]
    fn parse_args_consumes_dash_values_and_trailing_flags() {
        let argv: Vec<String> = ["--rate", "--mode", "sim", "--offline", "x", "--mode"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse_args(&argv, BOOLEAN_FLAGS);
        assert_eq!(
            a,
            vec![
                ("rate".into(), ArgValue::Str("--mode".into())),
                ("offline".into(), ArgValue::Flag),
                ("mode".into(), ArgValue::Flag),
            ]
        );
    }

    #[test]
    fn runtime_home_or_semantics() {
        assert_eq!(runtime_home(Some(""), "/home/pi"), "/home/pi/.xorgate");
        assert_eq!(runtime_home(None, "/"), "/.xorgate");
        assert_eq!(
            runtime_home(Some("/var/lib/xorgate-rs"), "/x"),
            "/var/lib/xorgate-rs"
        );
    }
}
