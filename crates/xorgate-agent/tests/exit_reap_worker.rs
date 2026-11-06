//! D41 agent-exit cleanup at WORKER level (review-wp5-r2 R-WP5R-1/-3/-4;
//! adapted from the reviewer's `rev2_worker_exit_reap.rs`).
//!
//! `xorgate-rs run` in real mode (an unreachable broker; `--offline` would
//! turn media off), one bridged camera (`KVS_MANIFEST` + `KVS_PIPELINE_DIR`),
//! recording on, and a `gst-launch-1.0` shim whose "EOS" outlives the agent:
//! it ignores SIGINT (logging each one), creates its padded shmsink segment in
//! /dev/shm and its first fragment at its splitmuxsink `location=`. After
//! SIGTERM the agent must leave no orphan: the child dead, its segment
//! unlinked — and its fragments untouched (no sidecar for the newest: the
//! next boot's recovery stamps it `finalized:false`, as before the cleanup
//! existed and as the TS). The second test cuts the config first (a preset
//! change via the settings poll), so the old supervisor sits inside a pending
//! video transition (its `stop_and_wait`) when SIGTERM arrives (R-WP5R-3).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_xorgate-rs");

const HW: &str = r#"{"v":2,"platform":"rpi-cm5","encoder":"sw-h264","rails":true,"statusLed":null,"imu":null,
 "video":[{"key":"cam0","connector":"csi0","sensor":"imx290"}]}"#;

const CAMS: &str = "Available cameras\n-----------------\n0 : imx290 [1920x1080 12-bit RGGB] (/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a)\n    Modes: 'SRGGB10_CSI2P' : 1280x720 [30.00 fps]\n";

fn settings(preset: &str) -> String {
    format!(
        r#"{{"recording":{{"telemetry":{{"enabled":true,"rateHz":1}},"video":{{"enabled":true,"preset":"{preset}","segmentSeconds":10,"bootDefer":"off","gpsGate":"off","spawnStaggerSecs":0}},"upload":{{"mode":"off"}},"retention":{{"minFreeBytes":0}}}}}}"#
    )
}

fn pid_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn files_with_ext(root: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                files_with_ext(&p, ext, out);
            } else if p.extension().is_some_and(|x| x == ext) {
                out.push(p);
            }
        }
    }
}

fn wait_for(what: &str, secs: u64, log: &Path, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(
            t0.elapsed() < Duration::from_secs(secs),
            "{what}: {}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Agent {
    dir: PathBuf,
    home: PathBuf,
    pids: PathBuf,
    ints: PathBuf,
    log: PathBuf,
    child: Child,
}

impl Agent {
    fn start(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("xg-exit-reap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings.json"), settings("720p15-1mbps")).unwrap();
        let hw = dir.join("hardware.json");
        std::fs::write(&hw, HW).unwrap();
        let shims = dir.join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        let w = |p: &Path, b: &str| xorgate_hal::test_support::write_executable(p, b).unwrap();
        w(&shims.join("timedatectl"), "#!/bin/sh\necho yes\n");
        w(
            &shims.join("rpicam-hello"),
            &format!("#!/bin/sh\nprintf '%s' '{CAMS}'\n"),
        );
        let pids = dir.join("gst.pids");
        let ints = dir.join("gst.ints");
        w(
            &shims.join("gst-launch-1.0"),
            &format!(
                "#!/bin/sh\ntrap 'echo $$ >> \"{i}\"' INT\necho $$ >> '{p}'\nname=$(printf 'shmpipe.%5d.    0' $$)\n: > /dev/shm/\"$name\"\n\
for a in \"$@\"; do case \"$a\" in location=*) loc=${{a#location=}}; f=$(printf \"$loc\" 0); mkdir -p \"$(dirname \"$f\")\"; head -c 4096 /dev/zero > \"$f\";; esac; done\n\
while :; do sleep 0.05; done\n",
                i = ints.display(),
                p = pids.display()
            ),
        );
        let mut path = std::ffi::OsString::from(&shims);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let pipes = dir.join("pipelines");
        std::fs::create_dir_all(&pipes).unwrap();
        std::fs::write(
            pipes.join("cam0.gst"),
            "shmsrc socket-path=/x/cam0.sock ! fakesink\n",
        )
        .unwrap();
        std::fs::write(
            pipes.join("cam0.camera"),
            "/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a\n",
        )
        .unwrap();
        let kvs = dir.join("kvs.json");
        std::fs::write(
            &kvs,
            r#"{"deviceId":"exit-reap-dev","region":"us-east-1","credentialEndpoint":"127.0.0.1","roleAlias":"r","channels":[{"streamKey":"cam0","channelName":"c0"}]}"#,
        )
        .unwrap();
        let fx = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
        let log = dir.join("agent.stderr");
        let child = Command::new(BIN)
            .args(["run"])
            .env_clear()
            .env("PATH", path)
            .env("HOME", &dir)
            .env("XORGATE_HOME", &home)
            .env("HARDWARE_MANIFEST", &hw)
            .env("MODE", "real")
            .env("DEVICE_ID", "exit-reap-dev")
            .env("IOT_DATA_ENDPOINT", "127.0.0.1")
            .env("CERT_PATH", format!("{fx}throwaway-selfsigned.crt"))
            .env("KEY_PATH", format!("{fx}throwaway-selfsigned.key"))
            .env("CA_PATH", format!("{fx}throwaway-selfsigned.crt"))
            .env("KVS_MANIFEST", &kvs)
            .env("KVS_PIPELINE_DIR", &pipes)
            .env("RUST_LOG", "info,rumqttc=warn")
            .env("GPS_NMEA_PORT", dir.join("no-tty"))
            .env("LTE_TELEMETRY_FILE", dir.join("no-lte.json"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("spawn");
        Self {
            dir,
            home,
            pids,
            ints,
            log,
            child,
        }
    }

    fn gst_pids(&self) -> Vec<i32> {
        std::fs::read_to_string(&self.pids)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    fn sigints(&self) -> usize {
        std::fs::read_to_string(&self.ints)
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// SIGTERM; returns the stderr log once the agent exited.
    fn term(&mut self) -> String {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.child.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        let t = Instant::now();
        let st = self.child.wait().unwrap();
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "exit took {:?}",
            t.elapsed()
        );
        assert_eq!(st.code(), Some(0), "clean exit");
        std::thread::sleep(Duration::from_millis(200));
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn seg_of(pid: i32) -> PathBuf {
    PathBuf::from(format!("/dev/shm/shmpipe.{pid:>5}.    0"))
}

/// R-WP5R-4 + R-WP5R-1: the worker's exit cleanup kills the child still in
/// its EOS and unlinks its segment, and finalises NOTHING (no sidecar).
#[test]
fn the_worker_exit_reaps_a_capture_still_in_its_eos_and_finalizes_nothing() {
    let mut a = Agent::start("eos");
    wait_for("no capture spawned", 30, &a.log, || {
        !a.gst_pids().is_empty()
    });
    let gst = a.gst_pids()[0];
    wait_for("the shim's segment", 5, &a.log, || seg_of(gst).exists());
    let mut mp4 = Vec::new();
    wait_for("the shim's fragment", 5, &a.log, || {
        mp4.clear();
        files_with_ext(&a.home, "mp4", &mut mp4);
        !mp4.is_empty()
    });
    std::thread::sleep(Duration::from_millis(1000));
    let log = a.term();
    // (The shutdown SIGINT may still be pending in the shim's `sleep` when
    // the exit cleanup's SIGKILL lands; the log line below proves the child
    // was alive at exit.)
    let mut sidecars = Vec::new();
    files_with_ext(
        &a.home.join("recordings").join("video"),
        "json",
        &mut sidecars,
    );
    sidecars.retain(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("0000"))
    });
    let alive = pid_alive(gst);
    let seg = seg_of(gst).exists();
    let _ = std::fs::remove_file(seg_of(gst)); // never leave our own orphan behind
    assert!(!alive, "the capture child is gone");
    assert!(!seg, "its segment was unlinked at exit");
    assert!(
        sidecars.is_empty(),
        "no fragment sidecar written at exit (the newest is left to recovery): {sidecars:?}"
    );
    assert!(
        log.contains(
            "[recording] capture cam0 still running at exit: killed, 1 shm segment(s) unlinked"
        ),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&a.dir);
}

/// R-WP5R-3: a config cut (preset change, picked up by the 5 s settings
/// poll) puts the old supervisor inside a pending `Replace` transition — its
/// child got the transition's SIGINT and ignores it (still "in its EOS"; the
/// new supervisor waits for it) — when SIGTERM arrives. The exit cleanup must
/// kill and reap that RETIRING child too.
#[test]
fn a_config_cut_then_sigterm_reaps_the_retiring_capture_too() {
    let mut a = Agent::start("cut");
    wait_for("no capture spawned", 30, &a.log, || {
        !a.gst_pids().is_empty()
    });
    let old = a.gst_pids()[0];
    wait_for("the shim's segment", 5, &a.log, || seg_of(old).exists());
    std::fs::write(a.home.join("settings.json"), settings("1080p15-2mbps")).unwrap();
    // The transition's stop_and_wait SIGINTs the old child (ignored).
    wait_for("the config cut's SIGINT", 15, &a.log, || a.sigints() >= 1);
    assert_eq!(
        a.gst_pids(),
        vec![old],
        "the new supervisor waits for the old child"
    );
    let log = a.term();
    let alive = pid_alive(old);
    let seg = seg_of(old).exists();
    let _ = std::fs::remove_file(seg_of(old));
    for p in a.gst_pids() {
        let _ = std::fs::remove_file(seg_of(p));
    }
    assert!(!alive, "the retiring capture child is gone");
    assert!(!seg, "the retiring child's segment was unlinked at exit");
    assert!(
        log.contains(
            "[recording] capture cam0 still running at exit: killed, 1 shm segment(s) unlinked"
        ),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&a.dir);
}
