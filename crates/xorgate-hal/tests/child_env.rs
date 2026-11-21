//! F-P6-ENV (Phase 6 window 1): a child the agent spawns must not inherit
//! the systemd notify/watchdog variables of the agent's `Type=notify` unit,
//! and must inherit everything else unchanged.
//!
//! The parent environment really carries the three keys here, so this file
//! holds ONE test: it sets them before any other thread of this binary can
//! read the environment (edition 2021 `set_var`), then drives every spawn
//! helper of `xorgate-hal`: `exec::run` (vcgencmd, df, timedatectl, pinctrl,
//! …), `child::spawn` (gst capture, KVS master, python IMU/OBD helpers),
//! `child::std_command` (the CLI's ps / journalctl), and (review R-P6F-4)
//! through fakes first on `PATH`: `privileged::SudoRunner` (both runners:
//! `run_argv` for a planned command, `run_argv_timeout_grace` for the 20 s
//! camrotate helper), `at_port::open_serial_transport` (`stty`, on
//! `/dev/null`) and `RealObdIo::unbond` (`bluetoothctl`). `clippy.toml`
//! makes a raw spawn anywhere else a `-D warnings` error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use xorgate_hal::child::{self, ChildEvent, ChildSpec, NOTIFY_ENV_KEYS};
use xorgate_hal::obd_io::{ObdIo, RealObdIo};
use xorgate_hal::privileged::{HelperArgv, PrivilegedArgv, PrivilegedRunner, SudoRunner};

/// A fake that records its environment (`env -0` prints exactly what exec gave
/// it) as `<dir>/<name>.<pid>`, then exits 0.
fn fake(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join("bin").join(name);
    let body = format!(
        "#!/bin/sh\nenv -0 > \"{}/{name}.$$.tmp\" && mv \"{}/{name}.$$.tmp\" \"{}/{name}.$$\"\n",
        dir.display(),
        dir.display(),
        dir.display()
    );
    xorgate_hal::test_support::write_executable(&p, &body).unwrap();
    p
}

/// Every env a fake named `name` recorded.
fn recorded(dir: &Path, name: &str) -> Vec<Vec<(String, String)>> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        if f.starts_with(&format!("{name}.")) && !f.ends_with(".tmp") {
            out.push(parse_nul(&String::from_utf8_lossy(
                &std::fs::read(e.path()).unwrap(),
            )));
        }
    }
    out
}

fn parse_nul(text: &str) -> Vec<(String, String)> {
    text.split('\0')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_string(), v.to_string())
        })
        .collect()
}

fn assert_env(path: &str, got: &[(String, String)], want: &BTreeMap<String, String>) {
    for k in NOTIFY_ENV_KEYS {
        assert!(
            got.iter().all(|(g, _)| g != k),
            "{path}: the child inherited {k}: {got:?}"
        );
    }
    let got_map: BTreeMap<String, String> = got.iter().cloned().collect();
    assert_eq!(got.len(), got_map.len(), "{path}: duplicate keys");
    assert_eq!(
        &got_map, want,
        "{path}: every other key passes through unchanged"
    );
    assert_eq!(
        got_map.get("XG_P6_PASSTHROUGH").map(String::as_str),
        Some("kept = as is"),
        "{path}"
    );
    // With a key removed, std materialises the child env from a sorted map:
    // the keys arrive in byte order (documented in `child::std_command`).
    let keys: Vec<&String> = got.iter().map(|(k, _)| k).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "{path}: std's env capture is byte-ordered");
}

/// For a child that is a `/bin/sh` fake: the shell may add its own variables
/// and exports in its own order, and it resets the shell-managed `PWD` /
/// `OLDPWD` to its real cwd, so only: no notify key, and every other parent
/// key arrives unchanged.
fn assert_env_through_sh(path: &str, got: &[(String, String)], want: &BTreeMap<String, String>) {
    for k in NOTIFY_ENV_KEYS {
        assert!(
            got.iter().all(|(g, _)| g != k),
            "{path}: the child inherited {k}: {got:?}"
        );
    }
    let got_map: BTreeMap<String, String> = got.iter().cloned().collect();
    for (k, v) in want
        .iter()
        .filter(|(k, _)| k.as_str() != "PWD" && k.as_str() != "OLDPWD")
    {
        assert_eq!(
            got_map.get(k),
            Some(v),
            "{path}: {k} passes through unchanged"
        );
    }
}

#[test]
fn the_notify_keys_never_reach_a_child_and_everything_else_does() {
    // As systemd sets them for `Type=notify` + `WatchdogSec=60`.
    std::env::set_var("NOTIFY_SOCKET", "/run/systemd/notify");
    std::env::set_var("WATCHDOG_PID", std::process::id().to_string());
    std::env::set_var("WATCHDOG_USEC", "60000000");
    std::env::set_var("XG_P6_PASSTHROUGH", "kept = as is");
    // Fakes FIRST on PATH: `sudo`, `stty` (bluetoothctl is passed by path).
    let fakes = std::env::temp_dir().join(format!("xg-child-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&fakes);
    std::fs::create_dir_all(fakes.join("bin")).unwrap();
    let sudo = fake(&fakes, "sudo");
    let stty = fake(&fakes, "stty");
    let btctl = fake(&fakes, "bluetoothctl");
    let mut path = std::ffi::OsString::from(fakes.join("bin"));
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    std::env::set_var("PATH", &path);
    // Safety net: the privileged cases below must NEVER reach a real sudo.
    assert_eq!(
        xorgate_hal::exec::find_in_path("sudo"),
        Some(sudo.clone()),
        "the fake sudo must win"
    );
    assert_eq!(
        xorgate_hal::exec::find_in_path("stty"),
        Some(stty.clone()),
        "the fake stty must win"
    );
    let parent: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| !NOTIFY_ENV_KEYS.contains(&k.as_str()))
        .collect();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // 1. exec::run — the bounded one-shot helpers.
        let r = xorgate_hal::exec::run("env", &["-0"], Some(Duration::from_secs(10))).await;
        assert!(!r.failed, "env -0: {r:?}");
        assert_env("exec::run", &parse_nul(&r.stdout), &parent);

        // 2. child::spawn — the supervised children, with a spec env merged on.
        let mut spec = ChildSpec::new("env", vec![]);
        spec.env.push(("XG_SPEC_EXTRA".into(), "1".into()));
        let (_h, mut rx) = child::spawn(&spec);
        let mut lines = vec![];
        while let Some(ev) = rx.recv().await {
            match ev {
                ChildEvent::Stdout(l) => lines.push(l),
                ChildEvent::Stderr(_) => {}
                ChildEvent::Done(d) => {
                    assert!(!d.failed, "{d:?}");
                    break;
                }
            }
        }
        let got: Vec<(String, String)> = lines
            .iter()
            .map(|l| {
                let (k, v) = l.split_once('=').unwrap();
                (k.to_string(), v.to_string())
            })
            .collect();
        let mut want = parent.clone();
        want.insert("XG_SPEC_EXTRA".into(), "1".into());
        assert_env("child::spawn", &got, &want);

        // 3. child::command (tokio) — privileged / at_port / obd_io build theirs so.
        let out = child::command("env").arg("-0").output().await.unwrap();
        assert_env(
            "child::command",
            &parse_nul(&String::from_utf8(out.stdout).unwrap()),
            &parent,
        );

        // 5. privileged::SudoRunner, both runners. The `update` plan only
        //    (never reboot/poweroff), and it reaches the fake `sudo` only.
        let call =
            match xorgate_core::commands::plan(xorgate_core::commands::DeviceCommandName::Update) {
                xorgate_core::commands::CommandPlan::Privileged(c) => c,
                other => panic!("update is privileged: {other:?}"),
            };
        let r = SudoRunner.run(&PrivilegedArgv::for_rust_agent(&call)).await;
        assert!(!r.failed, "fake sudo (run_argv): {r:?}");
        let helper = xorgate_core::boot_config::camcfg_call(
            xorgate_core::boot_config::CamCfgOp::Rotate {
                port: xorgate_core::boot_config::CamPort::Cam0,
                overlay: "imx290".into(),
                rotation: 0,
            },
            xorgate_core::camera_catalog::catalog(),
        )
        .unwrap();
        let ha = HelperArgv::from(&helper);
        assert!(ha.timeout().is_some(), "camcfg takes the timeout runner");
        let r = SudoRunner.run_helper(&ha).await;
        assert!(!r.failed, "fake sudo (run_argv_timeout_grace): {r:?}");
        let got = recorded(&fakes, "sudo");
        assert_eq!(got.len(), 2, "both sudo runs recorded");
        for g in &got {
            assert_env_through_sh("privileged::SudoRunner", g, &parent);
        }

        // 6. at_port: `stty -F /dev/null raw …` through PATH, then the open.
        assert!(
            xorgate_hal::at_port::open_serial_transport(Path::new("/dev/null"))
                .await
                .is_ok(),
            "stty fake + /dev/null open"
        );
        let got = recorded(&fakes, "stty");
        assert_eq!(got.len(), 1, "the stty run recorded");
        assert_env_through_sh("at_port stty", &got[0], &parent);

        // 7. obd_io: `bluetoothctl remove <MAC>` (the fake, by path).
        let io = RealObdIo {
            python_dir: fakes.clone(),
            mock: false,
            bluetoothctl: btctl.display().to_string(),
        };
        let _ = io.unbond("AA:BB:CC:DD:EE:FF".into()).await;
        let got = recorded(&fakes, "bluetoothctl");
        assert_eq!(got.len(), 1, "the bluetoothctl run recorded");
        assert_env_through_sh("obd_io bluetoothctl", &got[0], &parent);
    });

    // 4. child::std_command — the CLI's synchronous spawns.
    let out = child::std_command("env").arg("-0").output().unwrap();
    assert_env(
        "child::std_command",
        &parse_nul(&String::from_utf8(out.stdout).unwrap()),
        &parent,
    );

    let _ = std::fs::remove_dir_all(&fakes);

    // The agent's OWN environment keeps them: it still notifies the watchdog.
    for k in NOTIFY_ENV_KEYS {
        assert!(std::env::var_os(k).is_some(), "the parent lost {k}");
    }
}
