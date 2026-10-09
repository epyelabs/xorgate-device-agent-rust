//! systemd watchdog liveness: the worker must keep sending `WATCHDOG=1` while it
//! has NEVER connected (review.md MUST-FIX). Without systemd on the box the
//! notify protocol is just a datagram on `$NOTIFY_SOCKET`, so each test binds a
//! Unix datagram socket, points the real `xorgate-rs run` binary at it, gives it
//! an endpoint that can never complete a connect, and counts the pings.
//!
//! Two never-connect shapes are covered:
//! - a `.invalid` host — every attempt fails fast and the mqtt task sits in its
//!   backoff loop (the LTE-not-yet-up field case);
//! - a silent `127.0.0.1:8883` listener that accepts and never answers — the
//!   TLS handshake hangs for the whole 30 s connection timeout, proving the
//!   heartbeat is not starved by a stuck connect attempt.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");
const CERT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/throwaway-selfsigned.crt"
);
const KEY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/throwaway-selfsigned.key"
);

/// How long to watch for pings. The heartbeat is 1 s, so this yields ~6 pings
/// on an idle box; the assertion only asks for 3 to tolerate a loaded CI runner.
const OBSERVE: Duration = Duration::from_secs(6);
const MIN_PINGS: usize = 3;

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("xorgate-rs-watchdog-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self { dir }
    }
    fn path(&self, leaf: &str) -> PathBuf {
        self.dir.join(leaf)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn spawn_worker(scratch: &Scratch, endpoint: &str, notify: &Path) -> Child {
    let stderr = std::fs::File::create(scratch.path("stderr.log")).expect("stderr file");
    Command::new(BIN)
        .arg("run")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &scratch.dir)
        .env("XORGATE_HOME", scratch.path("home"))
        .env("NOTIFY_SOCKET", notify)
        .env("DEVICE_ID", "watchdog-test-device")
        .env("IOT_DATA_ENDPOINT", endpoint)
        .env("CERT_PATH", CERT)
        .env("KEY_PATH", KEY)
        // The TS reads CA_PATH unconditionally (config.ts:236-240), so the Rust agent
        // does too (Phase 2 WP1). Any PEM cert will do: the endpoint never presents one.
        .env("CA_PATH", CERT)
        .env("HARDWARE_MANIFEST", scratch.path("no-hardware.json"))
        .env("XORGATE_LOG", "info,rumqttc=warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn xorgate-rs")
}

/// Count `WATCHDOG=1` lines across every datagram that arrives before the deadline.
fn count_pings(sock: &UnixDatagram, window: Duration) -> (usize, Vec<String>) {
    let deadline = Instant::now() + window;
    let mut pings = 0;
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        match sock.recv(&mut buf) {
            Ok(n) => {
                let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
                pings += msg.lines().filter(|l| *l == "WATCHDOG=1").count();
                seen.push(msg);
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("notify socket recv: {e}"),
        }
    }
    (pings, seen)
}

fn stop(child: &mut Child, budget: Duration) -> std::process::ExitStatus {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("SIGTERM");
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("worker did not exit within {budget:?} after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_scenario(name: &str, endpoint: &str) {
    let scratch = Scratch::new(name);
    let notify = scratch.path("notify.sock");
    let sock = UnixDatagram::bind(&notify).expect("bind notify socket");
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");

    let mut child = spawn_worker(&scratch, endpoint, &notify);
    let (pings, seen) = count_pings(&sock, OBSERVE);

    let alive = child.try_wait().expect("try_wait").is_none();
    let state = std::fs::read_to_string(scratch.path("home/state.json")).unwrap_or_default();
    let status = if alive {
        Some(stop(&mut child, Duration::from_secs(10)))
    } else {
        None
    };
    let log = std::fs::read_to_string(scratch.path("stderr.log")).unwrap_or_default();

    assert!(
        alive,
        "[{name}] worker exited early ({status:?}); stderr:\n{log}"
    );
    let snap: serde_json::Value = serde_json::from_str(&state).unwrap_or_else(|e| {
        panic!("[{name}] state.json unparsable ({e}): {state}\nstderr:\n{log}")
    });
    assert_ne!(
        snap["connection"], "connected",
        "[{name}] the scenario must never connect; state={state}"
    );
    assert_eq!(
        snap["published"], 0,
        "[{name}] nothing may publish; state={state}"
    );
    assert!(
        pings >= MIN_PINGS,
        "[{name}] only {pings} WATCHDOG=1 pings in {OBSERVE:?} while never connected \
         (want ≥ {MIN_PINGS}); datagrams={seen:?}; state={state}\nstderr:\n{log}"
    );
    let status = status.expect("stopped");
    assert!(
        status.success(),
        "[{name}] expected exit 0 after SIGTERM, got {status}; stderr:\n{log}"
    );
}

#[test]
fn watchdog_is_fed_while_every_connect_fails_fast() {
    // `.invalid` is reserved (RFC 2606): resolution fails, the mqtt task loops
    // in backoff, and the worker never sees a CONNACK.
    run_scenario("fail-fast", "iot.invalid");
}

#[test]
fn watchdog_is_fed_while_a_connect_attempt_is_stuck() {
    // A listener that accepts and then says nothing leaves the TLS handshake
    // hanging until rumqttc's 30 s connection timeout — far longer than the
    // observation window — so the heartbeat must be independent of it.
    let listener = TcpListener::bind("127.0.0.1:8883").ok();
    let endpoint = match listener {
        Some(listener) => {
            std::thread::spawn(move || {
                let mut held = Vec::new();
                for conn in listener.incoming() {
                    match conn {
                        Ok(stream) => held.push(stream), // keep it open, never write
                        Err(_) => break,
                    }
                }
            });
            "127.0.0.1"
        }
        // Port busy on this host: fall back to a TEST-NET-1 blackhole (RFC 5737),
        // where the SYN itself hangs. Either way nothing ever answers.
        None => "192.0.2.1",
    };
    run_scenario("stuck-connect", endpoint);
}
