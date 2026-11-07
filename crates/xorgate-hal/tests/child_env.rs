//! F-P6-ENV (Phase 6 window 1): a child the agent spawns must not inherit
//! the systemd notify/watchdog variables of the agent's `Type=notify` unit,
//! and must inherit everything else unchanged.
//!
//! The parent environment really carries the three keys here, so this file
//! holds ONE test: it sets them before any other thread of this binary can
//! read the environment (edition 2021 `set_var`), then drives every spawn
//! helper of `xorgate-hal`: `exec::run` (vcgencmd, df, timedatectl, pinctrl,
//! …), `child::spawn` (gst capture, KVS master, python IMU/OBD helpers) and
//! `child::std_command` (the CLI's ps / journalctl). The remaining sites
//! (`privileged` sudo runs, `at_port` stty, `obd_io` bluetoothctl) build
//! their command with `child::command` too; `clippy.toml` makes a raw
//! `Command::new` anywhere else a `-D warnings` error.

use std::collections::BTreeMap;
use std::time::Duration;

use xorgate_hal::child::{self, ChildEvent, ChildSpec, NOTIFY_ENV_KEYS};

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

#[test]
fn the_notify_keys_never_reach_a_child_and_everything_else_does() {
    // As systemd sets them for `Type=notify` + `WatchdogSec=60`.
    std::env::set_var("NOTIFY_SOCKET", "/run/systemd/notify");
    std::env::set_var("WATCHDOG_PID", std::process::id().to_string());
    std::env::set_var("WATCHDOG_USEC", "60000000");
    std::env::set_var("XG_P6_PASSTHROUGH", "kept = as is");
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
    });

    // 4. child::std_command — the CLI's synchronous spawns.
    let out = child::std_command("env").arg("-0").output().unwrap();
    assert_env(
        "child::std_command",
        &parse_nul(&String::from_utf8(out.stdout).unwrap()),
        &parent,
    );

    // The agent's OWN environment keeps them: it still notifies the watchdog.
    for k in NOTIFY_ENV_KEYS {
        assert!(std::env::var_os(k).is_some(), "the parent lost {k}");
    }
}
