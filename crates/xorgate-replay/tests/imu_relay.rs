//! Phase 7c WP3: the rig's Rust-backend IMU relay as a real process
//! (`xorgate-replay __imu-reader …`, spawned the way the agent's `rust`
//! backend spawns its reader): it relays the player's lines, ends with the
//! `__EXIT__` code, exits 0 on SIGINT, and refuses a wrong argv (`arg0`
//! dropped) with an `init failed: rig argv …` line and exit 3.
#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-replay");

fn sock(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("xg-relay-{}-{tag}.sock", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// `wait()` with a 20 s cap (a hung relay fails the test, never the gate).
fn wait(child: &mut std::process::Child) -> std::process::ExitStatus {
    for _ in 0..2000 {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    panic!("the relay did not exit");
}

fn relay(arg0: &str, sock: &std::path::Path) -> std::process::Child {
    Command::new(BIN)
        .arg0(arg0)
        .args(["__imu-reader", "--address", "0x4a"])
        .env("RIG_IMU_SOCK", sock)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn relays_lines_and_exits_with_the_tapes_code() {
    let p = sock("exit");
    let l = UnixListener::bind(&p).unwrap();
    let mut child = relay("xorgate-imu", &p);
    let (mut c, _) = l.accept().unwrap();
    c.write_all(b"{\"type\": \"ready\", \"address\": \"0x4a\"}\nnot json\n__EXIT__ 4\n")
        .unwrap();
    let st = wait(&mut child);
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert_eq!(
        out,
        "{\"type\": \"ready\", \"address\": \"0x4a\"}\nnot json\n"
    );
    assert_eq!(st.code(), Some(4));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn sigint_ends_it_with_0() {
    let p = sock("int");
    let l = UnixListener::bind(&p).unwrap();
    let mut child = relay("xorgate-imu", &p);
    let (mut c, _) = l.accept().unwrap();
    c.write_all(b"line\n").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let st = wait(&mut child);
    assert_eq!((st.code(), st.signal()), (Some(0), None));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_wrong_argv0_parks_the_collector() {
    let p = sock("argv");
    let _l = UnixListener::bind(&p).unwrap();
    let mut child = relay("/rig/cache/bin/xorgate-replay", &p);
    wait(&mut child);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "{\"type\": \"error\", \"error\": \"init failed: rig argv \
         ['/rig/cache/bin/xorgate-replay', '__imu-reader', '--address', '0x4a']\"}\n"
    );
    let _ = std::fs::remove_file(&p);
}
