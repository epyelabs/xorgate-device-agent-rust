//! `xorgate-rs logs -f` against a closed stdout pipe (review-wp6 row 2): the
//! follow loop must end on the first write error (EPIPE) the way the TS
//! process dies on `process.stdout.write` (`logs.ts:54`), not poll forever.
#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");

#[test]
fn follow_exits_when_the_reader_is_gone() {
    let home = std::env::temp_dir().join(format!("xg-cli-logs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let log = home.join("agent.log");
    std::fs::write(&log, "first\n").unwrap();

    let mut child = Command::new(BIN)
        .args(["logs", "-f"])
        .env("XORGATE_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read the initial tail first (an EPIPE there is the separate row-12
    // `println!` path), then close the read end: the next write is EPIPE.
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut first).unwrap();
    assert_eq!(first, "first\n");
    drop(stdout);

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "line").unwrap();
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("logs -f kept running after its stdout reader went away");
    };
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    assert_eq!(status.code(), Some(1), "{status:?}: {err}");
    assert_eq!(err, "", "no panic line, no message");
    std::fs::remove_dir_all(&home).unwrap();
}

/// Run `xorgate-rs <args>` with stdout on a pipe whose reader is already
/// closed (every write is EPIPE).
fn into_closed_pipe(args: &[&str], home: &std::path::Path) -> std::process::Output {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    Command::new(BIN)
        .args(args)
        .env("XORGATE_HOME", home)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

/// review-wp6 row 12 + integration review #1: the initial tail and `status`
/// write through `console.log`, which SWALLOWS the EPIPE on Node 20
/// (`kWriteToConsole`; probed: `log` → 0, `exitCode=0; log` → 0,
/// `exitCode=1; log` → 1). So a closed pipe changes nothing: no `println!`
/// panic, and the exit code is the one a readable pipe would get.
#[test]
fn tail_and_status_into_a_closed_pipe_keep_their_normal_exit_code() {
    let home = std::env::temp_dir().join(format!("xg-cli-epipe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("agent.log"), "a\nb\nc\n").unwrap();
    let check = |args: &[&str], code: i32| {
        let out = into_closed_pipe(args, &home);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "{args:?}: {err}");
        assert!(!err.contains("panicked"), "{args:?}: {err}");
    };
    check(&["logs"], 0);
    check(&["status"], 1); // stopped: no pidfile

    // Running: a pidfile for this (live) test process, its own binary as the
    // worker, so `inspect` says Running (verified, or unverified without `ps`).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    let me = std::env::current_exe().unwrap();
    std::fs::write(
        home.join("agent.pid"),
        serde_json::json!({"pid": std::process::id(), "startedAt": now, "deviceId": "d",
            "mode": "sim", "rateHz": 1, "offline": true, "worker": me.display().to_string(),
            "argv": [], "nodeVersion": "v20"})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        home.join("state.json"),
        serde_json::json!({"lastWriteAt": now}).to_string(),
    )
    .unwrap();
    check(&["status"], 0);
    std::fs::remove_dir_all(&home).unwrap();
}

/// `logs -f` into an already-closed pipe: the tail's EPIPE is swallowed
/// (`console.log`), then the first follow `process.stdout.write` dies with
/// exit 1 — Node 20 probe: `console.log×3` then `stdout.write` → exit 1.
#[test]
fn follow_into_a_closed_pipe_exits_1_on_the_first_follow_write() {
    let home = std::env::temp_dir().join(format!("xg-cli-epipe-f-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let log = home.join("agent.log");
    std::fs::write(&log, "a\n").unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut child = Command::new(BIN)
        .args(["logs", "-f"])
        .env("XORGATE_HOME", &home)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "line").unwrap();
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("logs -f kept running into a closed pipe");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    assert_eq!(status.code(), Some(1), "{status:?}: {err}");
    assert!(!err.contains("panicked"), "{err}");
    std::fs::remove_dir_all(&home).unwrap();
}
