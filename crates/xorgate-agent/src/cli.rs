//! `xorgate-rs` dispatcher — port of `src/cli.tsx:1-167`: the subcommand is
//! the first bare token that is not the value of a value-flag, may appear
//! before or after the flags, and everything else is forwarded to it; `-v` /
//! `--version` anywhere wins; no command or `help` prints the usage (exit 0);
//! an unknown command prints `unknown command: X` + the usage (exit 1).
//!
//! Verbs (README §3.3): `run` (the worker; replaces `node dist/worker.js`),
//! `status`, `logs`, `set` (Phase 2). The TS's other verbs arrive in later
//! phases (`pipeline`, `recording`, `led`, `sms`, `update`, `manage`); the
//! self-managed daemon verbs `start` / `stop` / `restart` are systemd's job.

pub mod logs;
pub mod process_inspect;
pub mod set;
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

/// `usage()` (`cli.tsx:120-162`), listing the verbs this binary has.
pub fn usage(version: &str) -> String {
    format!(
        "xorgate-rs {version} — device telemetry agent + manager

Usage: xorgate-rs <command> [agent flags]

Commands:
  run        Run the telemetry agent in the foreground (what xorgate-agent-rs.service runs)
  status     Show whether the daemon is running (pid, uptime, connection, telemetry)
  set        Persist a setting and apply it: 'set mode <sim|real>',
             'set wifi <on|off> [--force]' (runtime WiFi toggle via NetworkManager), or
             'set recording.<section>.<field> <value>' (on-device recording, applied live —
             e.g. 'set recording.telemetry.enabled on', 'set recording.telemetry.rateHz 10')
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
        Some("update") => update::main(&rest),
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
        Some(c @ ("pipeline" | "recording" | "led" | "sms" | "manage" | "monitor")) => {
            eprintln!("xorgate-rs: '{c}' arrives in a later build");
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
