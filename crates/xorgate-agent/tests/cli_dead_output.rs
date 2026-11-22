//! The REAL binary with nobody reading its output (review of `a61be46`,
//! F1/F2). `xorgate-rs camera requirements` and `xorgate-rs update` print
//! while an apt transaction may be in flight; the SSH session that started
//! them can drop at any moment, and then every write fails (EIO on a
//! hung-up terminal, EPIPE on a pipe). `println!` PANICS on a failed
//! write, and the unwind used to run the cleanup under the running apt.
//!
//! These tests give the binary a stdout (and stderr) whose reader is
//! already gone, so every line it prints fails to write, and check that it
//! still runs to its ordinary exit code instead of panicking (101). The
//! in-process tests (`camera_packages.rs`) drive `run()` with a collecting
//! closure and cannot see this.

use std::path::{Path, PathBuf};
use std::process::Stdio;

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-deadout-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A pipe's write end whose read end is closed: every write to it fails
/// with EPIPE (the Rust runtime ignores SIGPIPE).
fn dead() -> Stdio {
    let (reader, writer) = std::io::pipe().expect("a pipe");
    drop(reader);
    Stdio::from(writer)
}

/// Run the binary with a dead stdout, and a dead stderr too when
/// `dead_stderr`; returns `(exit code, stderr)`.
fn run(home: &Path, args: &[&str], dead_stderr: bool) -> (Option<i32>, String) {
    let mut cmd = xorgate_hal::child::std_command(BIN);
    cmd.args(args)
        .env("XORGATE_HOME", home)
        .env_remove("INVOCATION_ID")
        .stdin(Stdio::null())
        .stdout(dead());
    if dead_stderr {
        cmd.stderr(dead());
    } else {
        cmd.stderr(Stdio::piped());
    }
    let out = cmd.output().expect("the binary runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The same command with its output READ: the exit code a healthy
/// terminal gets, and the lines it prints.
fn run_read(home: &Path, args: &[&str]) -> (Option<i32>, String) {
    let out = xorgate_hal::child::std_command(BIN)
        .args(args)
        .env("XORGATE_HOME", home)
        .env_remove("INVOCATION_ID")
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn camera_requirements_does_not_panic_when_nobody_reads_its_output() {
    let home = scratch("campkg");
    // A declared sensor that needs a package set, so the command has
    // something to say on every path.
    std::fs::write(
        home.join("settings.json"),
        r#"{"cameraSensors":{"cameras":{"cam0":{"sensor":"imx335"}}}}"#,
    )
    .unwrap();
    for args in [
        &["camera", "requirements"][..],
        // With --install the command stops before changing anything on
        // every machine these tests run on: not root, or root on a machine
        // whose OS release / architecture the catalog has no packages for,
        // or (a real arm64 bookworm device) the 5-second limit, which
        // leaves no room for an install.
        &[
            "camera",
            "requirements",
            "--install",
            "--no-restart",
            "--time-limit",
            "5",
        ],
    ] {
        // It prints lines when someone reads…
        let (healthy, printed) = run_read(&home, args);
        assert!(!printed.trim().is_empty(), "{args:?} prints something");
        // …and with nobody reading it ends with the SAME exit code: no
        // panic (101), no unwind.
        let (code, stderr) = run(&home, args, false);
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
        assert_ne!(code, Some(101), "{args:?}: {stderr}");
        assert_eq!(code, healthy, "{args:?}: {stderr}");
        // Stderr dead as well.
        let (code, _) = run(&home, args, true);
        assert_eq!(code, healthy, "{args:?}, stderr dead too");
    }
    // The usage line goes to stderr: a dead stderr does not panic either.
    let (code, _) = run(&home, &["camera", "requirements", "--nope"], true);
    assert_eq!(code, Some(2));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn update_does_not_panic_when_nobody_reads_its_output() {
    let home = scratch("update");
    // A source nothing listens on: the run fails at its first download
    // (or, when not root, at once) before it has touched anything, and it
    // has printed by then.
    let args = [
        "update",
        "--from",
        "http://127.0.0.1:9/dist",
        "--tag",
        "v0-dead-output",
    ];
    let (code, stderr) = run(&home, &args, false);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("[xorgate-rs]"),
        "its failure line: {stderr}"
    );
    // Stdout AND stderr dead: the failure line itself cannot be written.
    let (code, _) = run(&home, &args, true);
    assert_eq!(code, Some(1));
    let _ = std::fs::remove_dir_all(&home);
}
