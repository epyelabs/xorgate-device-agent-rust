//! `xorgate-rs logs -f` against a closed stdout pipe (review-wp6 row 2): the
//! follow loop must end on the first write error (EPIPE) the way the TS
//! process dies on `process.stdout.write` (`logs.ts:54`), not poll forever.

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

/// review-wp6 row 12: the initial tail (and `status`) into an already-closed
/// pipe ends with exit 1 and no `println!` panic line (Node dies on the
/// unhandled EPIPE too).
#[test]
fn tail_and_status_into_a_closed_pipe_exit_1_without_a_panic() {
    let home = std::env::temp_dir().join(format!("xg-cli-epipe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("agent.log"), "a\nb\nc\n").unwrap();
    for args in [&["logs"][..], &["status"][..]] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader); // every write is EPIPE
        let out = Command::new(BIN)
            .args(args)
            .env("XORGATE_HOME", &home)
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {err}");
        assert!(!err.contains("panicked"), "{args:?}: {err}");
    }
    std::fs::remove_dir_all(&home).unwrap();
}
