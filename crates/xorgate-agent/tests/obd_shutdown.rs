//! Phase 5b WP2, binary level (`xorgate-rs run --offline`):
//!
//! - review-wp2 M2: at agent exit the OBD helper is never SIGKILLed mid-close.
//!   The TS `stop()` SIGINTs it, and node's exit runs execa's `cleanup`
//!   (SIGTERM); the helper finishes its close (ATZ, 2 s timeout, the `close`
//!   audit record). A fake `obd_read.py` that takes 1 s to close after its
//!   first signal must still write its marker, and must be gone, once the
//!   agent has exited.
//! - D38: `sim` mode with a boot block that declares `peripherals.obd` emits
//!   no `obd` group and writes no state.json `obd`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");

const HW_OBD: &str = r#"{"v":2,"platform":"rpi-cm5","encoder":"hw-h264","rails":false,"statusLed":null,
 "imu":null,"video":[],"peripherals":{"obd":{"adapter":"elm327","transport":"bt-spp"}}}"#;

/// A helper that says live, then on its first signal takes `close_s` to
/// "close", records the signals it got and its pid, and exits 0.
const SLOW_HELPER: &str = r#"import json, os, signal, sys, time
got = []
def on(sig, _frm):
    got.append(sig)
signal.signal(signal.SIGINT, on)
signal.signal(signal.SIGTERM, on)
print(json.dumps({"type": "ready", "helper": "fake", "version": "slow"}), flush=True)
print(json.dumps({"type": "status", "state": "live", "link": 2}), flush=True)
while not got:
    time.sleep(0.02)
time.sleep(float(os.environ.get("FAKE_CLOSE_S", "1.0")))
with open(os.environ["FAKE_MARKER"], "w") as f:
    f.write("%d %s" % (os.getpid(), ",".join(str(s) for s in got)))
sys.exit(0)
"#;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xg-obd-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn shim(dir: &Path) -> std::ffi::OsString {
    let shims = dir.join("shims");
    std::fs::create_dir_all(&shims).unwrap();
    let s = shims.join("timedatectl");
    xorgate_hal::test_support::write_executable(&s, "#!/bin/sh\necho yes\n").unwrap();
    let mut path = std::ffi::OsString::from(&shims);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
}

/// Run the agent for `lines` telemetry lines, SIGTERM it, return (payloads,
/// state.json, exit code, elapsed from SIGTERM to exit).
fn run(
    dir: &Path,
    mode: &str,
    extra: &[(&str, String)],
    lines_wanted: usize,
) -> (Vec<serde_json::Value>, serde_json::Value, i32, Duration) {
    let home = dir.join("home");
    let hw = dir.join("hardware.json");
    std::fs::write(&hw, HW_OBD).unwrap();
    let mut cmd = Command::new(BIN);
    cmd.args(["run", "--offline"])
        .env_clear()
        .env("PATH", shim(dir))
        .env("HOME", dir)
        .env("XORGATE_HOME", &home)
        .env("HARDWARE_MANIFEST", &hw)
        .env("MODE", mode)
        .env("GPS_NMEA_PORT", dir.join("no-tty"))
        .env("LTE_TELEMETRY_FILE", dir.join("no-lte.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in extra {
        cmd.env(k, v);
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
    let deadline = Instant::now() + Duration::from_secs(15);
    while lines.len() < lines_wanted && Instant::now() < deadline {
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
            lines.push(serde_json::from_str::<serde_json::Value>(&l).expect("a JSON line"));
        }
    }
    let t = Instant::now();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = child.wait().unwrap();
    let elapsed = t.elapsed();
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("state.json")).unwrap()).unwrap();
    (lines, state, status.code().unwrap_or(-1), elapsed)
}

fn pid_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

#[test]
fn m2_a_slow_closing_helper_finishes_its_close_before_the_agent_exits() {
    let dir = scratch("m2");
    let py = dir.join("python");
    std::fs::create_dir_all(&py).unwrap();
    std::fs::write(py.join("obd_read.py"), SLOW_HELPER).unwrap();
    let marker = dir.join("closed");
    let (lines, state, code, elapsed) = run(
        &dir,
        "real",
        &[
            ("XORGATE_PYTHON_DIR", py.display().to_string()),
            ("XORGATE_OBD_SERIAL", "/dev/pts/fake".into()),
            ("FAKE_MARKER", marker.display().to_string()),
            ("FAKE_CLOSE_S", "1.0".into()),
        ],
        3,
    );
    assert_eq!(code, 0);
    let obd = lines.last().unwrap()["metrics"]["obd"].clone();
    assert_eq!(obd["link"]["value"], 2, "the fake helper went live: {obd}");
    // The marker is written 1 s AFTER the helper's first signal: it exists
    // only if the agent waited for the close instead of SIGKILLing.
    let text = std::fs::read_to_string(&marker).expect("the helper finished its close");
    let (pid, sigs) = text.split_once(' ').unwrap();
    let pid: i32 = pid.parse().unwrap();
    assert!(sigs.starts_with("2"), "SIGINT first (stop()), got {sigs}");
    assert!(
        !pid_alive(pid),
        "the helper is gone once the agent has exited"
    );
    assert!(elapsed < Duration::from_secs(5), "bounded: {elapsed:?}");
    assert_eq!(state["obd"]["state"], "live", "the last tick's snapshot");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A helper that never closes in time: the wait is bounded (3 s budget), the
/// agent still exits 0.
#[test]
fn m2_the_close_wait_is_bounded() {
    let dir = scratch("m2-bounded");
    let py = dir.join("python");
    std::fs::create_dir_all(&py).unwrap();
    std::fs::write(py.join("obd_read.py"), SLOW_HELPER).unwrap();
    let marker = dir.join("closed");
    let (_lines, _state, code, elapsed) = run(
        &dir,
        "real",
        &[
            ("XORGATE_PYTHON_DIR", py.display().to_string()),
            ("XORGATE_OBD_SERIAL", "/dev/pts/fake".into()),
            ("FAKE_MARKER", marker.display().to_string()),
            ("FAKE_CLOSE_S", "30".into()),
        ],
        2,
    );
    assert_eq!(code, 0);
    assert!(
        elapsed >= Duration::from_millis(2_500) && elapsed < Duration::from_secs(8),
        "{elapsed:?}"
    );
    assert!(!marker.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// D38: sim mode + `peripherals.obd` → no `obd` group, no state.json `obd`.
#[test]
fn d38_sim_mode_with_peripherals_obd_has_no_obd_group() {
    let dir = scratch("d38");
    let (lines, state, code, _) = run(&dir, "sim", &[], 2);
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 2);
    // Sim mode still carries WP3's LED sim value (system.led_state/health_flags 0/0);
    // D38 is only about the absent `obd` group.
    for p in &lines {
        assert!(p["metrics"].get("obd").is_none(), "{p}");
        assert_eq!(
            p["metrics"],
            serde_json::json!({"system": {"led_state": {"value": 0}, "health_flags": {"value": 0}}}),
            "{p}"
        );
    }
    assert!(state.get("obd").is_none(), "{state}");
    let _ = std::fs::remove_dir_all(&dir);
}
