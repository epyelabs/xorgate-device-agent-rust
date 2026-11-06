//! The KVS WebRTC master runtime (`xorgate-kvs-webrtc`) — the port of
//! `ts:src/hardware/kvs-webrtc.ts`: `detectRuntime` (:56-77) and the
//! `streamChannel` launch contract (:85-109): argv `[channel, mediaType,
//! source]`, the IoT creds + `LD_LIBRARY_PATH` merged onto the inherited
//! environment, `KVS_GST_PIPELINE` only when a pipeline resolved. Stop is
//! SIGINT, the runtime's graceful-shutdown signal (the supervisor's job).
//!
//! The decisions of the supervisor are `xorgate_core::media_ladder`; this
//! module only detects and builds the [`ChildSpec`].

use std::path::Path;

use xorgate_core::media_ladder::RuntimeDetect;

use crate::child::ChildSpec;

/// `KvsRuntimeEnv` (`kvs-webrtc.ts:27-49`), mirroring run-master.sh's exports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KvsRuntimeEnv {
    pub region: String,
    pub credential_endpoint: String,
    pub role_alias: String,
    /// IoT thing name == the device id.
    pub thing_name: String,
    pub cert_path: String,
    pub key_path: String,
    /// `AWS_KVS_CACERT_PATH` (media/cert.pem).
    pub ca_cert_path: String,
    /// `LD_LIBRARY_PATH` head (the vendored `.so` libs).
    pub lib_dir: String,
    /// `KVS_GST_PIPELINE` file, when one resolved.
    pub pipeline_path: Option<String>,
    /// `AWS_KVS_LOG_LEVEL`; `None` → `"3"`.
    pub log_level: Option<String>,
    /// `KVS_IDLE_TEARDOWN_SECS`; `None` → not set.
    pub idle_teardown_secs: Option<String>,
}

/// The child env of `streamChannel` (`kvs-webrtc.ts:93-106`), in the TS
/// object's insertion order. `inherited_ld` is the agent's own
/// `process.env.LD_LIBRARY_PATH`; `[libDir, inherited].filter(Boolean)`
/// drops empty parts, so the value may be `""`.
pub fn master_env(env: &KvsRuntimeEnv, inherited_ld: Option<&str>) -> Vec<(String, String)> {
    let ld: Vec<&str> = [Some(env.lib_dir.as_str()), inherited_ld]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let mut out: Vec<(String, String)> = [
        ("AWS_DEFAULT_REGION", env.region.as_str()),
        (
            "AWS_IOT_CORE_CREDENTIAL_ENDPOINT",
            env.credential_endpoint.as_str(),
        ),
        ("AWS_IOT_CORE_CERT", env.cert_path.as_str()),
        ("AWS_IOT_CORE_PRIVATE_KEY", env.key_path.as_str()),
        ("AWS_IOT_CORE_ROLE_ALIAS", env.role_alias.as_str()),
        ("AWS_IOT_CORE_THING_NAME", env.thing_name.as_str()),
        ("AWS_KVS_CACERT_PATH", env.ca_cert_path.as_str()),
        ("AWS_KVS_LOG_LEVEL", env.log_level.as_deref().unwrap_or("3")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    out.push(("LD_LIBRARY_PATH".into(), ld.join(":")));
    // `if (env.pipelinePath)` — truthy: an empty path is not set.
    if let Some(p) = env.pipeline_path.as_deref().filter(|p| !p.is_empty()) {
        out.push(("KVS_GST_PIPELINE".into(), p.to_string()));
    }
    if let Some(s) = &env.idle_teardown_secs {
        out.push(("KVS_IDLE_TEARDOWN_SECS".into(), s.clone()));
    }
    out
}

/// `streamChannel(channelName, mediaType, source, binaryPath, env)`
/// (`kvs-webrtc.ts:85-109`) as a spawn spec: `runStream(binaryPath,
/// [channelName, mediaType, source], …, {env})`. No `cwd`: like execa, the
/// child inherits the agent's working directory, where the runtime drops its
/// signalling cache (`/var/lib/xorgate-rs` on the bench).
pub fn master_spec(
    channel: &str,
    media_type: &str,
    source: &str,
    binary_path: &str,
    env: &KvsRuntimeEnv,
    inherited_ld: Option<&str>,
) -> ChildSpec {
    ChildSpec {
        program: binary_path.to_string(),
        args: vec![channel.into(), media_type.into(), source.into()],
        env: master_env(env, inherited_ld),
        cwd: None,
        arg0: None,
    }
}

/// Node's `os.arch()` for this build's target.
pub fn node_arch() -> &'static str {
    node_arch_of(std::env::consts::ARCH)
}

/// Rust `target_arch` → Node `process.arch`.
pub fn node_arch_of(arch: &'static str) -> &'static str {
    match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        "loongarch64" => "loong64",
        other => other,
    }
}

/// Node's `os.platform()` for this build's target (`"linux"`, `"darwin"`, …).
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// `detectRuntime(binaryPath)` (`kvs-webrtc.ts:56-77`) with the host facts
/// injected: `platform`/`arch` as Node names them, `kvs_runtime_bin_env` =
/// `process.env.KVS_RUNTIME_BIN` (truthy check), `gst_on_path` =
/// `commandExists("gst-launch-1.0")` (asked only when the binary exists).
pub fn detect_runtime_with(
    binary_path: &str,
    platform: &str,
    arch: &str,
    kvs_runtime_bin_env: Option<&str>,
    gst_on_path: impl FnOnce() -> bool,
) -> RuntimeDetect {
    if platform != "linux" {
        return RuntimeDetect {
            available: false,
            gstreamer: false,
            reason: Some("media runtime needs Linux (aarch64 device)".into()),
        };
    }
    if arch != "arm64" && kvs_runtime_bin_env.is_none_or(str::is_empty) {
        return RuntimeDetect {
            available: false,
            gstreamer: false,
            reason: Some(format!(
                "media runtime binary is aarch64; host arch is {arch}"
            )),
        };
    }
    // `existsSync(p) && statSync(p).isFile()` — both follow symlinks.
    if !std::fs::metadata(Path::new(binary_path)).is_ok_and(|m| m.is_file()) {
        return RuntimeDetect {
            available: false,
            gstreamer: false,
            reason: Some(format!("runtime binary not found at {binary_path}")),
        };
    }
    RuntimeDetect {
        available: true,
        gstreamer: gst_on_path(),
        reason: None,
    }
}

/// `detectRuntime(binaryPath)` on this host (`gst-launch-1.0` looked up on
/// `PATH`, like `which`).
pub fn detect_runtime(binary_path: &str) -> RuntimeDetect {
    let env_bin = std::env::var("KVS_RUNTIME_BIN").ok();
    detect_runtime_with(
        binary_path,
        node_platform(),
        node_arch(),
        env_bin.as_deref(),
        || crate::exec::command_exists("gst-launch-1.0"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_gate_and_platform_gate() {
        let d = detect_runtime_with("/nope", "darwin", "arm64", None, || true);
        assert_eq!(
            d.reason.as_deref(),
            Some("media runtime needs Linux (aarch64 device)")
        );
        let d = detect_runtime_with("/nope", "linux", "x64", None, || true);
        assert_eq!(
            d.reason.as_deref(),
            Some("media runtime binary is aarch64; host arch is x64")
        );
        // KVS_RUNTIME_BIN="" is falsy: still gated.
        let d = detect_runtime_with("/nope", "linux", "x64", Some(""), || true);
        assert!(d
            .reason
            .unwrap()
            .starts_with("media runtime binary is aarch64"));
        // Set: the arch gate opens, the binary check follows.
        let d = detect_runtime_with("/nope", "linux", "x64", Some("/nope"), || true);
        assert_eq!(
            d.reason.as_deref(),
            Some("runtime binary not found at /nope")
        );
        assert_eq!(node_arch_of("x86_64"), "x64");
        assert_eq!(node_arch_of("aarch64"), "arm64");
    }
}
