//! Which build is this? The agent-side half of `src/lib/build-info.ts`: the
//! manifest read + fallback. The `BuildInfo` shape, `format_build_info` and
//! the manifest → `BuildInfo` mapping live in `xorgate_core::build_info`.
//!
//! CI / `scripts/build-info.sh` write `build-info.json` `{version, channel,
//! builtAt, packageVersion}` into the install root; at runtime it is read from
//! `<exe dir>/../build-info.json` (`/opt/xorgate-rs/bin/xorgate-rs` →
//! `/opt/xorgate-rs/build-info.json`). Without a manifest the binary falls
//! back to the git sha baked by `build.rs` with `channel: "dev-local"` — never
//! a stale sha from somewhere else, so a bench build cannot masquerade as CI.

pub use xorgate_core::build_info::{parse_build_manifest, BuildInfo};

pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const BAKED_SHA: &str = env!("XORGATE_GIT_SHA");

/// The identity of a tree without a manifest.
pub fn fallback() -> BuildInfo {
    BuildInfo {
        version: BAKED_SHA.to_string(),
        channel: "dev-local".to_string(),
        built_at: None,
        package_version: PACKAGE_VERSION.to_string(),
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
        return fallback();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return fallback();
    };
    parse_build_manifest(&text, PACKAGE_VERSION).unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_ci_manifest() {
        let b = parse_build_manifest(
            r#"{ "version": "abc1234", "channel": "dev", "builtAt": "2026-10-08T00:00:00Z", "packageVersion": "0.0.1" }"#,
            PACKAGE_VERSION,
        )
        .unwrap();
        assert_eq!(b.format(), "0.0.1 (abc1234, dev)");
        assert_eq!(b.built_at.as_deref(), Some("2026-10-08T00:00:00Z"));
    }

    #[test]
    fn empty_version_means_fallback() {
        assert!(parse_build_manifest(r#"{"version": "  "}"#, PACKAGE_VERSION).is_none());
        assert!(parse_build_manifest("garbage", PACKAGE_VERSION).is_none());
        assert_eq!(fallback().channel, "dev-local");
    }
}
