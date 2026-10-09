//! Bakes the compile-time fallback build identity (`XORGATE_GIT_SHA`) so a
//! binary with no `build-info.json` beside it still reports where it came
//! from (`{"version":"<sha>","channel":"dev-local"}`). CI/`scripts/
//! build-info.sh` write the real manifest at runtime location
//! `<exe dir>/../build-info.json`, which wins when present.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    // A source tree without .git (rsync'd to the bench, CI tarball) can hand
    // the sha in via the environment instead.
    println!("cargo:rerun-if-env-changed=XORGATE_GIT_SHA");
    let baked = std::env::var("XORGATE_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty());
    let sha = baked.unwrap_or_else(|| {
        let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
        let dirty = git(&["status", "--porcelain"])
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        format!("{sha}{}", if dirty { "-dirty" } else { "" })
    });
    println!("cargo:rustc-env=XORGATE_GIT_SHA={sha}");
    // Re-run when HEAD moves (best effort; a stale sha is cosmetic).
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }
}
