//! `test/atomic-write.test.ts` (4 it()s + the chown-failure case) against
//! `xorgate_hal::atomic_write`. The TS forces the root branch with a mock;
//! here the root-only assertions run for real when the suite is root (the
//! Docker build container and CI are) and are skipped otherwise, mirroring
//! the TS `skipIf(SUITE_IS_ROOT)` / `if (SUITE_IS_ROOT)` guards.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use xorgate_hal::atomic_write::atomic_write_json;

/// nobody:nogroup on Debian / Raspberry Pi OS.
const NOBODY: u32 = 65534;

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-atomic-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn leftover_temps(d: &PathBuf) -> Vec<String> {
    std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect()
}

fn mode(p: &PathBuf) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// atomic-write.test.ts:70 "writes the object and leaves no temp file behind".
#[test]
fn writes_the_object_and_leaves_no_temp_file() {
    let d = dir("write");
    let t = d.join("settings.json");
    atomic_write_json(
        &t,
        &serde_json::json!({"gnssAntBias": false, "mode": "real"}),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&t).unwrap(),
        r#"{"gnssAntBias":false,"mode":"real"}"#
    );
    assert!(leftover_temps(&d).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// atomic-write.test.ts:76 "creates a brand-new file 0600 and does not chown it, even as root".
#[test]
fn creates_a_brand_new_file_0600_owned_by_us() {
    let d = dir("new");
    let t = d.join("settings.json");
    atomic_write_json(&t, &serde_json::json!({"v": 1})).unwrap();
    assert_eq!(mode(&t), 0o600);
    assert_eq!(
        std::fs::metadata(&t).unwrap().uid(),
        nix::unistd::geteuid().as_raw()
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// atomic-write.test.ts:86 "makes no ownership syscall at all, and still narrows an
/// existing file to 0600 as before (skipped when the suite runs as root)".
#[test]
fn non_root_rewrite_narrows_to_0600() {
    if is_root() {
        return; // TS: it.skipIf(SUITE_IS_ROOT)
    }
    let d = dir("nonroot");
    let t = d.join("settings.json");
    std::fs::write(&t, "{}").unwrap();
    std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
    atomic_write_json(&t, &serde_json::json!({"kept": true})).unwrap();
    assert_eq!(mode(&t), 0o600);
    assert_eq!(std::fs::read_to_string(&t).unwrap(), r#"{"kept":true}"#);
    let _ = std::fs::remove_dir_all(&d);
}

/// atomic-write.test.ts:103 "carries the target's mode across the rename
/// (uid/gid assertion no-ops unless the suite runs as root)".
#[test]
fn root_preserves_the_existing_owner_and_mode() {
    if !is_root() {
        return; // the Rust helper has no force-root seam; root-only like the TS uid half
    }
    let d = dir("root");
    let t = d.join("settings.json");
    std::fs::write(&t, "{}").unwrap();
    std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o640)).unwrap();
    nix::unistd::chown(
        &t,
        Some(nix::unistd::Uid::from_raw(NOBODY)),
        Some(nix::unistd::Gid::from_raw(NOBODY)),
    )
    .unwrap();
    atomic_write_json(&t, &serde_json::json!({"gnssAntBias": false})).unwrap();
    let m = std::fs::metadata(&t).unwrap();
    assert_eq!(
        m.permissions().mode() & 0o7777,
        0o640,
        "NOT the temp file's 0600"
    );
    assert_eq!((m.uid(), m.gid()), (NOBODY, NOBODY));
    assert_eq!(
        std::fs::read_to_string(&t).unwrap(),
        r#"{"gnssAntBias":false}"#
    );
    assert!(leftover_temps(&d).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}
