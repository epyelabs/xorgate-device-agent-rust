//! Which build is this? Port of `src/lib/build-info.ts`.
//!
//! CI / `scripts/build-info.sh` write `build-info.json` `{version, channel,
//! builtAt, packageVersion}` into the install root; at runtime it is read from
//! `<exe dir>/../build-info.json` (`/opt/xorgate-rs/bin/xorgate-rs` →
//! `/opt/xorgate-rs/build-info.json`). Without a manifest the binary falls
//! back to the git sha baked by `build.rs` with `channel: "dev-local"` — never
//! a stale sha from somewhere else, so a bench build cannot masquerade as CI.

use serde::{Deserialize, Serialize};

pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const BAKED_SHA: &str = env!("XORGATE_GIT_SHA");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    /// Short sha (dev CI), release tag (prod CI), or the baked sha locally.
    pub version: String,
    /// "dev" | "prod" from CI; "bench" from scripts/build-info.sh; "dev-local" when baked.
    pub channel: String,
    /// ISO build timestamp; null when there is no manifest.
    pub built_at: Option<String>,
    /// Cargo package version — the human-facing release line.
    pub package_version: String,
}

impl BuildInfo {
    pub fn fallback() -> Self {
        Self {
            version: BAKED_SHA.to_string(),
            channel: "dev-local".to_string(),
            built_at: None,
            package_version: PACKAGE_VERSION.to_string(),
        }
    }

    /// `0.0.1 (a1b2c3d, dev)` — the TS `formatBuildInfo` form.
    pub fn format(&self) -> String {
        format!(
            "{} ({}, {})",
            self.package_version, self.version, self.channel
        )
    }
}

/// Where the manifest lives relative to the running executable.
pub fn manifest_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin_dir = exe.parent()?;
    Some(bin_dir.parent().unwrap_or(bin_dir).join("build-info.json"))
}

pub fn load() -> BuildInfo {
    let Some(path) = manifest_path() else {
        return BuildInfo::fallback();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BuildInfo::fallback();
    };
    parse_manifest(&text).unwrap_or_else(BuildInfo::fallback)
}

fn parse_manifest(text: &str) -> Option<BuildInfo> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        version: Option<String>,
        channel: Option<String>,
        built_at: Option<String>,
        package_version: Option<String>,
    }
    let raw: Raw = serde_json::from_str(text).ok()?;
    let nonempty = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let version = nonempty(raw.version)?;
    Some(BuildInfo {
        version,
        channel: nonempty(raw.channel).unwrap_or_else(|| "unknown".into()),
        built_at: nonempty(raw.built_at),
        package_version: nonempty(raw.package_version).unwrap_or_else(|| PACKAGE_VERSION.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_ci_manifest() {
        let b = parse_manifest(r#"{ "version": "abc1234", "channel": "dev", "builtAt": "2026-10-08T00:00:00Z", "packageVersion": "0.0.1" }"#).unwrap();
        assert_eq!(b.format(), "0.0.1 (abc1234, dev)");
        assert_eq!(b.built_at.as_deref(), Some("2026-10-08T00:00:00Z"));
    }

    #[test]
    fn empty_version_means_fallback() {
        assert!(parse_manifest(r#"{"version": "  "}"#).is_none());
        assert!(parse_manifest("garbage").is_none());
        assert_eq!(BuildInfo::fallback().channel, "dev-local");
    }
}
