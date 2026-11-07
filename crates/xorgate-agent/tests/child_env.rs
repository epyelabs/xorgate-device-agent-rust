//! F-P6-ENV (Phase 6 window 1), binary level: the real `xorgate-rs run`,
//! started the way systemd starts a `Type=notify` + `WatchdogSec=` unit
//! (`NOTIFY_SOCKET`, `WATCHDOG_PID` = its own pid, `WATCHDOG_USEC`), must
//!
//! 1. keep feeding the watchdog: `READY=1`, then `WATCHDOG=1` every second on
//!    `$NOTIFY_SOCKET` (the three keys stay in the AGENT's environment);
//! 2. start its children without them: the OBD python helper (`child::spawn`,
//!    the path that sent the 56 stray notifications on PROD) and the
//!    `timedatectl` / `vcgencmd` one-shots (`exec::run`) each record their
//!    initial environment (`/proc/self/environ`), which must lack all three
//!    keys and carry every other key the agent was given, unchanged.

#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

use std::collections::BTreeMap;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");
const NOTIFY_KEYS: [&str; 3] = ["NOTIFY_SOCKET", "WATCHDOG_PID", "WATCHDOG_USEC"];

const HW_OBD: &str = r#"{"v":2,"platform":"rpi-cm5","encoder":"hw-h264","rails":false,"statusLed":null,
 "imu":null,"video":[],"peripherals":{"obd":{"adapter":"elm327","transport":"bt-spp"}}}"#;

/// Records its initial environment, goes live, exits on the first signal.
const ENV_HELPER: &str = r#"import json, os, signal, sys, time
with open("/proc/self/environ", "rb") as f:
    raw = f.read()
tmp = os.environ["XG_ENV_DIR"] + "/obd_read.%d.tmp" % os.getpid()
with open(tmp, "wb") as f:
    f.write(raw)
os.rename(tmp, tmp[:-4])
got = []
signal.signal(signal.SIGINT, lambda s, f: got.append(s))
signal.signal(signal.SIGTERM, lambda s, f: got.append(s))
print(json.dumps({"type": "ready", "helper": "fake", "version": "env"}), flush=True)
print(json.dumps({"type": "status", "state": "live", "link": 2}), flush=True)
while not got:
    time.sleep(0.02)
sys.exit(0)
"#;

/// A shim that records its environment (`env -0` prints exactly what exec
/// gave it) and then answers like the real tool.
fn shim_body(name: &str, answer: &str) -> String {
    format!(
        "#!/bin/sh\nt=\"$XG_ENV_DIR/{name}.$$\"\nenv -0 > \"$t.tmp\" && mv \"$t.tmp\" \"$t\"\necho '{answer}'\n"
    )
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xg-child-env-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn parse_nul(raw: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(raw)
        .split('\0')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_string(), v.to_string())
        })
        .collect()
}

/// The recorded envs whose file name starts with `prefix`.
fn recorded(dir: &Path, prefix: &str) -> Vec<(String, BTreeMap<String, String>)> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix) && !name.ends_with(".tmp") {
            out.push((name, parse_nul(&std::fs::read(e.path()).unwrap())));
        }
    }
    out
}

#[test]
fn f_p6_env_children_never_see_the_notify_env_and_the_agent_still_feeds_its_watchdog() {
    let dir = scratch("e2e");
    let home = dir.join("home");
    let envs = dir.join("envs");
    let py = dir.join("python");
    let shims = dir.join("shims");
    for d in [&envs, &py, &shims] {
        std::fs::create_dir_all(d).unwrap();
    }
    let hw = dir.join("hardware.json");
    std::fs::write(&hw, HW_OBD).unwrap();
    std::fs::write(py.join("obd_read.py"), ENV_HELPER).unwrap();
    xorgate_hal::test_support::write_executable(
        &shims.join("timedatectl"),
        &shim_body("timedatectl", "yes"),
    )
    .unwrap();
    xorgate_hal::test_support::write_executable(
        &shims.join("vcgencmd"),
        &shim_body("vcgencmd", "throttled=0x0"),
    )
    .unwrap();
    let mut path = std::ffi::OsString::from(&shims);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());

    let sock_path = dir.join("notify.sock");
    let sock = UnixDatagram::bind(&sock_path).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    // The agent's environment, minus the three systemd keys (set below).
    let given: Vec<(&str, String)> = vec![
        ("PATH", path.to_string_lossy().into_owned()),
        ("HOME", dir.display().to_string()),
        ("XORGATE_HOME", home.display().to_string()),
        ("HARDWARE_MANIFEST", hw.display().to_string()),
        ("MODE", "real".into()),
        ("GPS_NMEA_PORT", dir.join("no-tty").display().to_string()),
        (
            "LTE_TELEMETRY_FILE",
            dir.join("no-lte.json").display().to_string(),
        ),
        ("XORGATE_PYTHON_DIR", py.display().to_string()),
        ("XORGATE_OBD_SERIAL", "/dev/pts/fake".into()),
        ("XG_ENV_DIR", envs.display().to_string()),
        ("XG_P6_PASSTHROUGH", "kept = as is".into()),
    ];
    // `sh` execs the agent in place, so WATCHDOG_PID is the agent's own pid,
    // exactly as systemd sets it for the main process.
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        "WATCHDOG_PID=$$; export WATCHDOG_PID; exec \"$0\" run --offline",
        BIN,
    ])
    .env_clear()
    .env("NOTIFY_SOCKET", &sock_path)
    .env("WATCHDOG_USEC", "60000000")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(std::fs::File::create(dir.join("stderr.log")).unwrap());
    for (k, v) in &given {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn the agent");
    let agent_pid = child.id();

    // Watch the notify socket until the children have recorded their env
    // and the watchdog has been fed at least 3 times (1 s heartbeat).
    let (mut ready, mut pings, mut other) = (0usize, 0usize, vec![]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok(n) = sock.recv(&mut buf) {
            let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
            for line in msg.lines() {
                match line {
                    "READY=1" => ready += 1,
                    "WATCHDOG=1" => pings += 1,
                    l => other.push(l.to_string()),
                }
            }
        }
        if pings >= 3
            && !recorded(&envs, "obd_read.").is_empty()
            && !recorded(&envs, "timedatectl.").is_empty()
        {
            break;
        }
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(agent_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = child.wait().unwrap();
    let stderr = std::fs::read_to_string(dir.join("stderr.log")).unwrap_or_default();

    // 1. The agent itself still notifies (its own env kept the keys).
    assert_eq!(
        ready, 1,
        "READY=1 once; other datagrams {other:?}; stderr:\n{stderr}"
    );
    assert!(
        pings >= 3,
        "watchdog fed {pings}× (want ≥ 3); stderr:\n{stderr}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "clean SIGTERM exit; stderr:\n{stderr}"
    );

    // 2. Every recorded child env: no notify key, every given key unchanged.
    let mut checked = vec![];
    for prefix in ["obd_read.", "timedatectl.", "vcgencmd."] {
        let rec = recorded(&envs, prefix);
        if prefix != "vcgencmd." {
            assert!(
                !rec.is_empty(),
                "no {prefix}* env recorded; stderr:\n{stderr}"
            );
        }
        for (name, env) in rec {
            for k in NOTIFY_KEYS {
                assert!(!env.contains_key(k), "{name} inherited {k}: {env:?}");
            }
            for (k, v) in &given {
                assert_eq!(env.get(*k), Some(v), "{name}: {k} passes through");
            }
            checked.push(name);
        }
    }
    eprintln!("checked child envs: {checked:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
