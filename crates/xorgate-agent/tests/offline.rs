//! `xorgate-rs run --offline` end to end (Phase 0 A2/A5 kept by the WP1
//! refactor): the offline stub "connects" at once, the first payload is
//! printed at +interval, one JSON line per tick; SIGTERM shuts down in the
//! TS order with exit 0, the pidfile removed and state.json closed.
#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");

fn run_offline(tag: &str, mode: Option<&str>) -> (Vec<serde_json::Value>, serde_json::Value, i32) {
    run_offline_ntp(tag, mode, "yes")
}

/// `ntp` = what a `timedatectl` shim first on PATH answers for
/// `NTPSynchronized` (the real-mode clock trust gate probes it, Phase 5b).
fn run_offline_ntp(
    tag: &str,
    mode: Option<&str>,
    ntp: &str,
) -> (Vec<serde_json::Value>, serde_json::Value, i32) {
    let dir = std::env::temp_dir().join(format!("xg-offline-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join("home");
    let shims = dir.join("shims");
    std::fs::create_dir_all(&shims).unwrap();
    let shim = shims.join("timedatectl");
    xorgate_hal::test_support::write_executable(&shim, &format!("#!/bin/sh\necho {ntp}\n"))
        .unwrap();
    let mut path = std::ffi::OsString::from(&shims);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let mut cmd = Command::new(BIN);
    cmd.args(["run", "--offline"])
        .env_clear()
        .env("PATH", path)
        .env("HOME", &dir)
        .env("XORGATE_HOME", &home)
        .env("HARDWARE_MANIFEST", dir.join("none.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(m) = mode {
        cmd.env("MODE", m);
    }
    let mut child = cmd.spawn().expect("spawn");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut lines = vec![];
    let deadline = Instant::now() + Duration::from_secs(10);
    while lines.len() < 2 && Instant::now() < deadline {
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
            lines.push(serde_json::from_str::<serde_json::Value>(&l).expect("a JSON line"));
        }
    }
    let pid_present = home.join("agent.pid").exists();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = child.wait().unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("state.json")).unwrap()).unwrap();
    let pid_after = home.join("agent.pid").exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        pid_present && !pid_after,
        "pidfile written at start, removed at stop"
    );
    (lines, state, status.code().unwrap_or(-1))
}

#[test]
fn offline_prints_one_payload_per_tick_and_stops_cleanly() {
    let (lines, state, code) = run_offline("sim", None);
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 2);
    for (i, p) in lines.iter().enumerate() {
        assert_eq!(p["v"], 1);
        assert_eq!(p["deviceId"], "sim-local");
        assert_eq!(p["seq"], i);
        // `sim` mode (TS default): collectors off (O7) → only the LED leaves
        // (`LED_TELEMETRY_SIM`, always healthy; `worker.ts:367-379` @
        // 09da38d, Phase 5b X1), idle media.
        assert_eq!(
            p["metrics"],
            serde_json::json!({"system": {"led_state": {"value": 0}, "health_flags": {"value": 0}}})
        );
        assert_eq!(
            p["media"],
            serde_json::json!({"kvs":{"value":false},"recording":{"value":false},"streams":[]})
        );
        assert_eq!(p["recording"]["enabled"], false);
    }
    let msgs: Vec<&str> = state["recentEvents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["msg"].as_str().unwrap())
        .collect();
    assert_eq!(
        msgs,
        [
            "agent starting (mode=sim)",
            "connected to offline",
            "shutting down (SIGTERM)",
        ]
    );
    // The offline stub's end() never emits `close` (mqtt.ts:97-100).
    assert_eq!(state["connection"], "closed");
    assert_eq!(state["mediaEnabled"], false);
    assert_eq!(state["mode"], "sim");
    assert_eq!(state["published"], 2);
}

#[test]
fn offline_real_mode_publishes_the_system_group() {
    if !std::path::Path::new("/proc/stat").exists() {
        return; // off-Linux dev box
    }
    let (lines, state, code) = run_offline("real", Some("real"));
    assert_eq!(code, 0);
    assert!(lines[1]["metrics"]["system"]["cpu_usage"]["value"].is_number());
    assert_eq!(state["mode"], "real");
}

/// Phase 5b clock trust gate (`worker.ts:1406-1412` @ 09da38d): in real mode
/// with `NTPSynchronized=no` the local snapshot advances (`seq`, `latest`)
/// but nothing is published within the 5 min grace window.
#[test]
fn offline_real_mode_holds_the_publish_until_the_clock_is_trusted() {
    if !std::path::Path::new("/proc/stat").exists() {
        return; // off-Linux dev box
    }
    let (lines, state, code) = run_offline_ntp("real-held", Some("real"), "no");
    assert_eq!(code, 0);
    assert!(lines.is_empty(), "published while held: {lines:?}");
    assert_eq!(state["published"], 0);
    assert!(
        state["latest"]["seq"].as_u64().unwrap() >= 1,
        "seq advances across held ticks"
    );
}
