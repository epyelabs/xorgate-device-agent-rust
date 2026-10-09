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
        let needle = format!("\n{banned} ");
        let needle_eq = format!("\n{banned}=");
        assert!(
            !manifest.contains(&needle) && !manifest.contains(&needle_eq),
            "xorgate-core must not depend on `{banned}`"
        );
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
    let mut files = Vec::new();
    rust_sources(&crate_dir().join("src"), &mut files);
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        for (lineno, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            assert!(
                !code.contains("unsafe "),
                "{}:{}: unsafe in core",
                f.display(),
                lineno + 1
            );
        }
    }
}
