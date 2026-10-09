//! Which build is this? The pure part of `src/lib/build-info.ts`
//! (device_update_management, Phase 1): the `BuildInfo` shape the agent
//! reports in `effective.agent.build`, `formatBuildInfo` (`:51`) and the
//! manifest → `BuildInfo` mapping inside `readManifest` (`:55-70`). Reading
//! `build-info.json` / `package.json` from disk and the cache
//! (`loadBuildInfo`, `resetBuildInfoCache`) live in `xorgate-agent::build_info`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `BuildInfo` (`build-info.ts:21`). `builtAt` is written as an explicit
/// `null` when there is no manifest (the TS type is `string | null`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    /// Short sha (dev CI), release tag (prod CI), or the package version locally.
    pub version: String,
    /// "dev" | "prod" from CI; "bench" from the stamp script; else "local"
    /// (the Rust agent bakes "dev-local" for a tree without a manifest).
    pub channel: String,
    /// ISO build timestamp; null when there is no manifest to read it from.
    pub built_at: Option<String>,
    /// Package version — the human-facing release line, independent of the sha.
    pub package_version: String,
}

/// `formatBuildInfo` (`build-info.ts:51`): `0.2.0 (a1b2c3d, dev)` — the
/// one-line form for CLI banners and status output.
pub fn format_build_info(info: &BuildInfo) -> String {
    format!(
        "{} ({}, {})",
        info.package_version, info.version, info.channel
    )
}

/// `str` (`build-info.ts:88`): a non-blank string, trimmed.
fn nonblank(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The manifest → `BuildInfo` mapping of `readManifest` (`build-info.ts:55-70`)
/// over already-read text. `package_version` is the running package's own
/// version (read separately by the caller). Returns None when the manifest is
/// unreadable JSON, not a plain object, or carries no usable `version` —
/// the caller then falls back to its local identity.
pub fn parse_build_manifest(text: &str, package_version: &str) -> Option<BuildInfo> {
    let raw: Value = serde_json::from_str(text).ok()?;
    let Value::Object(raw) = raw else {
        return None;
    };
    let version = nonblank(raw.get("version"))?;
    Some(BuildInfo {
        version,
        channel: nonblank(raw.get("channel")).unwrap_or_else(|| "unknown".to_string()),
        built_at: nonblank(raw.get("builtAt")),
        package_version: nonblank(raw.get("packageVersion"))
            .unwrap_or_else(|| package_version.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_mapping_and_serde() {
        let b = parse_build_manifest(
            r#"{ "version": " abc1234 ", "channel": "dev", "builtAt": "2026-10-08T00:00:00Z", "packageVersion": "0.0.1" }"#,
            "9.9.9",
        )
        .unwrap();
        assert_eq!(format_build_info(&b), "0.0.1 (abc1234, dev)");
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"version":"abc1234","channel":"dev","builtAt":"2026-10-08T00:00:00Z","packageVersion":"0.0.1"}"#
        );
        let b = parse_build_manifest(r#"{"version":"v1"}"#, "0.2.0").unwrap();
        assert_eq!(b.channel, "unknown");
        assert_eq!(b.package_version, "0.2.0");
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"version":"v1","channel":"unknown","builtAt":null,"packageVersion":"0.2.0"}"#
        );
        assert!(parse_build_manifest(r#"{"version": "  "}"#, "x").is_none());
        assert!(parse_build_manifest("garbage", "x").is_none());
        assert!(parse_build_manifest("[]", "x").is_none());
    }
}
