//! Plan Phase 1 rule 4 / acceptance B4: `xorgate-core` is PURE. This test
//! greps the crate's own sources and manifest so a stray `std::fs`, process
//! spawn, wall-clock read or tokio dependency fails CI instead of slipping
//! into a "pure" decider.

use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn cargo_toml_has_no_io_runtime_dependencies() {
    let manifest = std::fs::read_to_string(crate_dir().join("Cargo.toml")).unwrap();
    for banned in [
        "tokio",
        "async-std",
        "nix",
        "libc",
        "gstreamer",
        "rumqttc",
        "chrono",
        "time",
    ] {
        // Every spelling cargo accepts for a dependency on `banned`:
        //   banned = "1"            banned = { workspace = true }
        //   banned.workspace = true (dotted key)
        //   [dependencies.banned]   (table header)
        //   rt = { package = "banned" }  (renamed dep)
        let key_space = format!("{banned} ");
        let key_eq = format!("{banned}=");
        let key_dot = format!("{banned}.");
        let table = format!("dependencies.{banned}]");
        let renamed = format!("package = \"{banned}\"");
        let renamed_tight = format!("package=\"{banned}\"");
        for (lineno, line) in manifest.lines().enumerate() {
            let code = line.split('#').next().unwrap_or("").trim_start();
            let hit = code.starts_with(&key_space)
                || code.starts_with(&key_eq)
                || code.starts_with(&key_dot)
                || code.contains(&table)
                || code.contains(&renamed)
                || code.contains(&renamed_tight);
            assert!(
                !hit,
                "Cargo.toml:{}: xorgate-core must not depend on `{banned}` (plan Phase 1 rule 4)",
                lineno + 1
            );
        }
    }
}

#[test]
fn sources_do_not_touch_fs_process_or_clocks() {
    let mut files = Vec::new();
    rust_sources(&crate_dir().join("src"), &mut files);
    assert!(!files.is_empty());
    const BANNED: &[&str] = &[
        "std::fs",
        "std::process",
        "std::net",
        "std::env::var",
        "std::time::SystemTime",
        "SystemTime::now",
        "Instant::now",
        "std::thread::sleep",
        "tokio::",
        "Command::new",
        // A brace import (`use std::{fs, io};` then `fs::read_to_string`)
        // would hide the module path from the needles above, so the crate
        // spells every `std` import out in full (`use std::fmt;`).
        "std::{",
    ];
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        for (lineno, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for b in BANNED {
                assert!(
                    !code.contains(b),
                    "{}:{}: `{b}` is not allowed in xorgate-core (plan Phase 1 rule 3/4)",
                    f.display(),
                    lineno + 1
                );
            }
        }
    }
}

#[test]
fn lib_rs_forbids_unsafe() {
    let lib = std::fs::read_to_string(crate_dir().join("src/lib.rs")).unwrap();
    assert!(lib.contains("#![forbid(unsafe_code)]"));
    // `#![forbid]` on lib.rs does not reach the integration-test crates under
    // tests/, so scan both trees textually.
    let mut files = Vec::new();
    rust_sources(&crate_dir().join("src"), &mut files);
    rust_sources(&crate_dir().join("tests"), &mut files);
    assert!(files.iter().any(|f| f.ends_with("tests/purity.rs")));
    // Built at runtime so this file's own source does not trip the scan.
    let needle = ["unsafe", " "].concat();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        for (lineno, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            assert!(
                !code.contains(&needle),
                "{}:{}: `unsafe` is not allowed in xorgate-core or its tests",
                f.display(),
                lineno + 1
            );
        }
    }
}
