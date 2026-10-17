//! `MediaConfig` (`ts:src/agent/config.ts:26-87`, resolved at `:244-275`).
//!
//! Every key is read exactly as the TS `get(flag, ENV, default)` does
//! (`--flag value` ?? `process.env[ENV]` ?? default, paths through
//! `path.resolve`) — `xorgate_core::agent_config::load_config` already
//! resolved them into [`AgentConfig::media`]. This module only swaps the
//! three install-root defaults (P4, accepted divergence D25): when neither
//! the flag nor the env var is given, the vendored runtime is looked up
//! under the Rust install root (`/opt/xorgate-rs/media/…`, like
//! `XORGATE_PYTHON_DIR`) instead of the JS root `/opt/xorgate/media/…`.
//!
//! | field | flag | env | default |
//! |---|---|---|---|
//! | `binary_path` | `--media-bin` | `KVS_RUNTIME_BIN` | `<install root>/media/xorgate-kvs-webrtc` (D25) |
//! | `lib_dir` | `--media-lib` | `KVS_RUNTIME_LIB_DIR` | `<install root>/media/lib` (D25) |
//! | `manifest_path` | `--media-manifest` | `KVS_MANIFEST` | `./creds/kvs.json` |
//! | `source` | `--media-source` | `MEDIA_SOURCE` | `testsrc` |
//! | `media_type` | `--media-type` | `MEDIA_TYPE` | `video-only` |
//! | `pipeline_dir` | `--media-pipeline-dir` | `KVS_PIPELINE_DIR` | `/etc/xorgate/pipelines` |
//! | `pipeline_path` | — | `KVS_GST_PIPELINE` (truthy) | unset |
//! | `log_level` | `--media-log` | `AWS_KVS_LOG_LEVEL` | `3` |
//! | `idle_teardown_secs` | `--media-idle-teardown` | `KVS_IDLE_TEARDOWN_SECS` | `30` |
//! | `cert_path` / `key_path` | `--cert` / `--key` | `CERT_PATH` / `KEY_PATH` | `./creds/…` |
//! | `ca_cert_path` | `--media-cacert` | `KVS_CACERT_PATH` | `dirname(binary_path)/cert.pem` (D25 via the binary) |
//!
//! `enabled` = `!offline` (`config.ts:254`): there is no operator switch.
//!
//! [`AgentConfig::media`]: xorgate_core::agent_config::AgentConfig

use std::path::Path;

use xorgate_core::agent_config::{parse_args, resolve_path, ArgValue, BOOLEAN_FLAGS};

use crate::env::AgentEnv;

/// The media runtime's configuration (`MediaConfig` minus `streamGuard`,
/// which the supervisor takes as its own argument).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaConfig {
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

/// `posix dirname` of an absolute path (`path.dirname`).
fn dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

impl MediaConfig {
    /// `loadConfig(argv).media` with the P4 install-root defaults.
    pub fn from_env(env: &AgentEnv, offline: bool) -> Self {
        let var = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string());
        Self::resolve(env, offline, &var, &crate::env::install_root(), &cwd)
    }

    /// [`Self::from_env`] with the process facts injected (tests).
    pub fn resolve(
        env: &AgentEnv,
        offline: bool,
        var: &dyn Fn(&str) -> Option<String>,
        install_root: &Path,
        cwd: &str,
    ) -> Self {
        let m = &env.config.media;
        let args = parse_args(&env.argv, BOOLEAN_FLAGS);
        // `get(flag, ENV)` gave a value (the flag only when it is a string).
        let given = |flag: &str, key: &str| {
            args.iter()
                .any(|(k, v)| k == flag && matches!(v, ArgValue::Str(_)))
                || var(key).is_some()
        };
        let root = install_root.to_string_lossy();
        let binary_path = if given("media-bin", "KVS_RUNTIME_BIN") {
            m.binary_path.clone()
        } else {
            resolve_path(cwd, &format!("{root}/media/xorgate-kvs-webrtc"))
        };
        let lib_dir = if given("media-lib", "KVS_RUNTIME_LIB_DIR") {
            m.lib_dir.clone()
        } else {
            resolve_path(cwd, &format!("{root}/media/lib"))
        };
        let ca_cert_path = if given("media-cacert", "KVS_CACERT_PATH") {
            m.ca_cert_path.clone()
        } else {
            resolve_path(cwd, &format!("{}/cert.pem", dirname(&binary_path)))
        };
        Self {
            enabled: !offline,
            binary_path,
            lib_dir,
            manifest_path: m.manifest_path.clone(),
            source: m.source.clone(),
            media_type: m.media_type.clone(),
            pipeline_dir: m.pipeline_dir.clone(),
            pipeline_path: m.pipeline_path.clone(),
            log_level: m.log_level.clone(),
            idle_teardown_secs: m.idle_teardown_secs.clone(),
            cert_path: m.cert_path.clone(),
            key_path: m.key_path.clone(),
            ca_cert_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_core::agent_config::{load_config, ConfigInputs};

    fn env_with(
        argv: &[&str],
        vars: &[(&str, &str)],
    ) -> (AgentEnv, impl Fn(&str) -> Option<String>) {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let lookup = move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let config = load_config(&ConfigInputs {
            argv: &argv,
            env: &lookup,
            settings_mode: None,
            cwd: "/var/lib/xorgate-rs",
        })
        .unwrap();
        (
            AgentEnv {
                config,
                home: "/var/lib/xorgate-rs".into(),
                self_managed: true,
                upload_bucket_override: None,
                argv,
            },
            lookup,
        )
    }

    #[test]
    fn install_root_defaults_and_explicit_overrides() {
        let base = [("DEVICE_ID", "d"), ("IOT_DATA_ENDPOINT", "e")];
        let (env, var) = env_with(&[], &base);
        let c = MediaConfig::resolve(
            &env,
            false,
            &var,
            Path::new("/opt/xorgate-rs"),
            "/var/lib/xorgate-rs",
        );
        assert!(c.enabled);
        assert_eq!(c.binary_path, "/opt/xorgate-rs/media/xorgate-kvs-webrtc");
        assert_eq!(c.lib_dir, "/opt/xorgate-rs/media/lib");
        assert_eq!(c.ca_cert_path, "/opt/xorgate-rs/media/cert.pem");
        assert_eq!(c.manifest_path, "/var/lib/xorgate-rs/creds/kvs.json");
        assert_eq!(c.pipeline_dir, "/etc/xorgate/pipelines");
        assert_eq!(
            (c.source.as_str(), c.media_type.as_str()),
            ("testsrc", "video-only")
        );
        assert_eq!(
            (c.log_level.as_str(), c.idle_teardown_secs.as_str()),
            ("3", "30")
        );
        assert_eq!(c.pipeline_path, None);

        // The env binary moves the CA default with it; an empty env value is still "given".
        let mut vars = base.to_vec();
        vars.extend([
            ("KVS_RUNTIME_BIN", "/x/kvs"),
            ("KVS_RUNTIME_LIB_DIR", ""),
            ("KVS_GST_PIPELINE", "g.gst"),
        ]);
        let (env, var) = env_with(&[], &vars);
        let c = MediaConfig::resolve(
            &env,
            true,
            &var,
            Path::new("/opt/xorgate-rs"),
            "/var/lib/xorgate-rs",
        );
        assert!(!c.enabled);
        assert_eq!(c.binary_path, "/x/kvs");
        assert_eq!(c.lib_dir, "/var/lib/xorgate-rs"); // path.resolve("") = cwd
        assert_eq!(c.ca_cert_path, "/x/cert.pem");
        assert_eq!(
            c.pipeline_path.as_deref(),
            Some("/var/lib/xorgate-rs/g.gst")
        );

        // Flags win; a valueless trailing flag is not a string (default kept).
        let (env, var) = env_with(&["--media-cacert", "/c.pem", "--media-bin"], &base);
        let c = MediaConfig::resolve(&env, false, &var, Path::new("/opt/xorgate-rs"), "/");
        assert_eq!(c.binary_path, "/opt/xorgate-rs/media/xorgate-kvs-webrtc");
        assert_eq!(c.ca_cert_path, "/c.pem");
    }
}
