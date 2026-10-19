//! `xorgate-rs` dispatcher — port of `src/cli.tsx:1-167`: the subcommand is
//! the first bare token that is not the value of a value-flag, may appear
//! before or after the flags, and everything else is forwarded to it; `-v` /
//! `--version` anywhere wins; no command or `help` prints the usage (exit 0);
//! an unknown command prints `unknown command: X` + the usage (exit 1).
//!
//! Verbs (README §3.3): `run` (the worker; replaces `node dist/worker.js`),
//! `status`, `logs`, `set` (Phase 2); `pipeline`, `recording`, `manage` (+ the
//! deprecated `monitor` alias) (Phase 4 WP5); `led` (WP1), `sms` (WP2),
//! `update` (WP4). The self-managed daemon verbs `start` / `stop` /
//! `restart` are systemd's job.

pub mod led;
pub mod logs;
pub mod manage;
pub mod pipeline;
pub mod process_inspect;
pub mod recording;
pub mod set;
pub mod sms;
pub mod status;
pub mod update;
pub mod version;

/// `VALUE_FLAGS` (`cli.tsx:28-39`).
pub const VALUE_FLAGS: [&str; 10] = [
    "--device-id",
    "--endpoint",
    "--mode",
    "--rate",
    "--cert",
    "--key",
    "--ca",
    "--tag",
    "--from",
    "--limit",
];

/// `splitCommand` (`cli.tsx:50-61`).
pub fn split_command(av: &[String]) -> (Option<String>, Vec<String>) {
    let mut i = 0;
    while i < av.len() {
        let a = av[i].as_str();
        if VALUE_FLAGS.contains(&a) {
            i += 2; // skip the flag's value too
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        let mut rest = av[..i].to_vec();
        rest.extend_from_slice(&av[i + 1..]);
        return (Some(a.to_string()), rest);
    }
    (None, av.to_vec())
}

/// `usage()` (`cli.tsx:120-162`), listing the verbs this binary has. The
/// TS lines are kept where they hold for the Rust binary; `start`/`stop`/
/// `restart` are systemd's (README §3.3), `manage` has no daemon controls
/// (Q10), `logs` falls back to the journal (O9); the `update` lines are
/// WP4's.
pub fn usage(version: &str) -> String {
    format!(
        "xorgate-rs {version} — device telemetry agent + manager

Usage: xorgate-rs <command> [agent flags]

Commands:
  run        Run the telemetry agent in the foreground (what xorgate-agent-rs.service runs)
  status     Show whether the daemon is running (pid, uptime, connection, telemetry)
  manage     Open the live dashboard (status + Settings from inside; start/stop/restart
             is systemd's: sudo systemctl restart xorgate-agent-rs)
  set        Persist a setting and apply it: 'set mode <sim|real>',
             'set wifi <on|off> [--force]' (runtime WiFi toggle via NetworkManager), or
             'set recording.<section>.<field> <value>' (on-device recording, applied live —
             e.g. 'set recording.telemetry.enabled on', 'set recording.telemetry.rateHz 10')
  pipeline   Manage per-stream KVS pipelines: 'pipeline list|show|set|clear <streamKey> [file]'
  recording  On-device recording maintenance: 'recording reset-counters' zeroes the
             cumulative droppedSegments counter (applied live, no restart)
  led        RGB status LED: 'led <colour> [pattern]', 'led off', or 'led test' to
             walk the whole status ladder printing each label. The agent drives it
             automatically; these are for bench work. Inert off-device.
  sms        Read the modem's stored SMS, newest first, without marking them read:
             'sms [--limit N] [--json]' (N 1..50, default 10). Bench tool; prints text.
  logs       Print the daemon log   (-f follow, -n N lines, --clear truncate;
             the journal of xorgate-agent-rs when there is no agent.log)
  update     Fetch the latest agent build, install it, and restart the service
             (sudo; --tag <version> pins a build, --from <url> overrides the bucket)

Agent flags (forwarded to run):
  --offline              Run without AWS (stub mqtt + simulator) — no creds needed
  --device-id <uuid>     DEVICE_ID
  --endpoint <host>      IOT_DATA_ENDPOINT
  --mode <sim|real>      telemetry source (default sim)
  --rate <hz>            publish rate (default 1)

Env: XORGATE_HOME (runtime dir, default ~/.xorgate), DEVICE_ID, IOT_DATA_ENDPOINT,
     MODE, RATE_HZ, CERT_PATH, KEY_PATH, CA_PATH, OFFLINE.

Examples:
  xorgate-rs run --offline
  xorgate-rs manage
  xorgate-rs set mode real
  sudo systemctl restart xorgate-agent-rs
"
    )
}

/// `main()` (`cli.tsx:63-118`) → exit code. `argv` excludes the program name.
pub fn main(argv: &[String]) -> i32 {
    if argv.iter().any(|a| a == "-v" || a == "--version") {
        println!("{}", version::line());
        return 0;
    }
    let (cmd, rest) = split_command(argv);
    match cmd.as_deref() {
        Some("run") => crate::worker::main(&rest),
        Some("status") => status::main(&rest),
        Some("logs") => logs::main(&rest),
        Some("set") => set::main(&rest),
        Some("led") => led::main(&rest),
        Some("sms") => sms::main(&rest),
        Some("update") => update::main(&rest),
        Some("pipeline") => pipeline::main(&rest),
        Some("recording") => recording::main(&rest),
        // `monitor`: deprecated alias for `manage`, kept working (cli.tsx:102-108).
        Some(c @ ("manage" | "monitor")) => {
            if c == "monitor" {
                eprintln!("[xorgate] 'monitor' is deprecated; use 'manage'");
            }
            manage::main(&rest)
        }
        None | Some("help") => {
            println!("{}", usage(&crate::build_info::load().format()));
            0
        }
        Some(c @ ("start" | "stop" | "restart")) => {
            eprintln!(
                "xorgate-rs: '{c}' is systemd's job for the Rust agent: sudo systemctl {c} xorgate-agent-rs"
            );
            1
        }
        Some(c) => {
            eprintln!("unknown command: {c}\n");
            println!("{}", usage(&crate::build_info::load().format()));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn subcommand_may_follow_flags_and_value_flags_skip_their_value() {
        assert_eq!(
            split_command(&s(&["--device-id", "status", "start"])),
            (Some("start".into()), s(&["--device-id", "status"]))
        );
        assert_eq!(
            split_command(&s(&["--offline", "run"])),
            (Some("run".into()), s(&["--offline"]))
        );
        assert_eq!(
            split_command(&s(&["set", "--mode", "x", "mode", "real"])),
            (Some("set".into()), s(&["--mode", "x", "mode", "real"]))
        );
        assert_eq!(split_command(&s(&["--offline"])), (None, s(&["--offline"])));
        assert_eq!(split_command(&s(&["--rate"])), (None, s(&["--rate"])));
    }
}
