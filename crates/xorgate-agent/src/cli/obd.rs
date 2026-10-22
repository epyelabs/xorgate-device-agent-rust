//! `xorgate-rs obd scan [--seconds N] [--json]` / `xorgate-rs obd status` —
//! port of `src/commands/obd.ts` @ 09da38d (Phase 5b WP2).
//!
//! - `scan` runs the SAME discovery the `obd_scan` cloud command runs
//!   (`python3 <XORGATE_PYTHON_DIR>/obd_scan.py --seconds N`, the mock pair
//!   under `XORGATE_MOCK`), never pairs, and prints the candidate table or
//!   (`--json`) the exact list `effective.obd.scan` carries.
//! - `status` reads state.json's `obd` (no helper is spawned): the status
//!   line + detail / since / helper / profile / audit.
//!
//! Strings verbatim from the TS, except the trailing local hints that name
//! the binary (`xorgate-rs set obd.adapter …`, D34).

use std::path::{Path, PathBuf};

use xorgate_core::js::{number_to_string, to_iso_string};
use xorgate_core::obd::{
    format_scan_table, obd_status_detail_lines, obd_status_line, obd_usage, parse_obd_args,
    ObdArgs, ObdScanCandidate, OBD_STATUS_ABSENT,
};
use xorgate_core::telemetry::ObdStatus;
use xorgate_hal::obd_io::{ObdIo, RealObdIo};

use crate::state::read_state;

/// The binary name the local hints use (D34).
pub const BIN: &str = "xorgate-rs";

/// What one invocation printed and its exit code.
#[derive(Debug, Default, PartialEq)]
pub struct ObdCliOut {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub code: i32,
}

/// `readState()?.obd ?? null` (a malformed `obd` reads as absent).
pub fn read_obd_status(home: &Path) -> Option<ObdStatus> {
    let state = read_state(&home.join("state.json"))?;
    serde_json::from_value(state.get("obd")?.clone()).ok()
}

/// `obdCommand(rest)` over injected edges.
pub async fn run_with(rest: &[String], home: &Path, io: &dyn ObdIo) -> ObdCliOut {
    let mut o = ObdCliOut::default();
    match parse_obd_args(rest) {
        ObdArgs::Help => o.stdout.push(obd_usage(BIN)),
        ObdArgs::Error(message) => {
            o.stderr.push(format!("obd: {message}"));
            o.stderr.push(obd_usage(BIN));
            o.code = 1;
        }
        ObdArgs::Status => match read_obd_status(home) {
            None => o.stdout.push(OBD_STATUS_ABSENT.to_string()),
            Some(obd) => {
                o.stdout.push(obd_status_line(&obd));
                o.stdout
                    .extend(obd_status_detail_lines(&obd, &to_iso_string(obd.since)));
            }
        },
        ObdArgs::Scan { seconds, json } => {
            o.stderr.push(format!(
                "scanning for {} s (never pairs)…",
                number_to_string(seconds)
            ));
            match io.scan(seconds).await {
                Err(e) => {
                    o.stderr.push(format!("obd scan failed: {e}"));
                    o.code = 1;
                }
                Ok(c) if json => o.stdout.push(pretty(&c)),
                Ok(c) => o.stdout.push(format_scan_table(&c, BIN)),
            }
        }
    }
    o
}

/// `JSON.stringify(candidates, null, 2)`.
fn pretty(c: &[ObdScanCandidate]) -> String {
    serde_json::to_string_pretty(c).unwrap_or_else(|_| "[]".into())
}

/// `XORGATE_PYTHON_DIR`, else `<install root>/python` (the agent's rule).
fn python_dir() -> PathBuf {
    std::env::var_os("XORGATE_PYTHON_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::env::install_root().join("python"))
}

/// `xorgate-rs obd …` → exit code.
pub fn main(rest: &[String]) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("✗ {e}");
            return 1;
        }
    };
    let io = RealObdIo::from_env(python_dir());
    let home = crate::env::runtime_home();
    // The scan announcement goes out BEFORE the (25 s) discovery, like the TS.
    let parsed = parse_obd_args(rest);
    if let ObdArgs::Scan { seconds, .. } = parsed {
        eprintln!(
            "scanning for {} s (never pairs)…",
            number_to_string(seconds)
        );
    }
    let out = rt.block_on(run_with(rest, &home, &io));
    let skip = usize::from(matches!(parsed, ObdArgs::Scan { .. }));
    for line in out.stderr.iter().skip(skip) {
        eprintln!("{line}");
    }
    let _ = super::logs::write_lines(out.stdout);
    out.code
}

#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_hal::obd_io::{ObdFut, ObdHelperEvent, ObdHelperHandle};

    struct Io(Result<Vec<ObdScanCandidate>, String>);
    impl ObdIo for Io {
        fn spawn(
            &self,
            _: &xorgate_core::obd::ObdHelperConfig,
        ) -> (
            ObdHelperHandle,
            tokio::sync::mpsc::UnboundedReceiver<ObdHelperEvent>,
        ) {
            unreachable!("the CLI never spawns the helper")
        }
        fn scan(&self, _: f64) -> ObdFut<Result<Vec<ObdScanCandidate>, String>> {
            let r = self.0.clone();
            Box::pin(async move { r })
        }
        fn unbond(&self, _: String) -> ObdFut<Result<(), String>> {
            unreachable!()
        }
    }

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-obd-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn two() -> Vec<ObdScanCandidate> {
        vec![
            ObdScanCandidate {
                mac: "10:21:3E:4F:33:65".into(),
                name: "OBDII".into(),
                rssi: Some(-58.0),
            },
            ObdScanCandidate {
                mac: "00:1D:A5:68:98:8B".into(),
                name: "V-LINK".into(),
                rssi: None,
            },
        ]
    }

    #[tokio::test]
    async fn status_reads_state_json_or_says_there_is_no_collector() {
        let h = home("status");
        let io = Io(Ok(vec![]));
        let o = run_with(&s(&["status"]), &h, &io).await;
        assert_eq!(o.stdout, [OBD_STATUS_ABSENT]);
        assert_eq!(o.code, 0);
        std::fs::write(
            h.join("state.json"),
            r#"{"v":1,"deviceId":"d","lastWriteAt":0,"connection":"connected","connectionSince":0,
               "seq":0,"published":0,"recentEvents":[],"obd":{"state":"live","link":2,"reqPerSec":10,"errors":0,"forbidden":0,"voltage":14.12,
               "detail":"7E8 answering","since":0,"helperVersion":"0.2.0","adapter":"10:21:3E:4F:33:65",
               "profile":"AUTO · ECUs 7E8 · 2/3 fields · ice","voltageLowEvents":1,"auditSeq":4,"lastStop":null}}"#,
        )
        .unwrap();
        let o = run_with(&s(&["status"]), &h, &io).await;
        assert_eq!(
            o.stdout,
            [
                "obd        live  link 2  10.0 req/s  err 0  forbidden 0  14.1 V  adapter 10:21:3E:4F:33:65",
                "  detail     7E8 answering",
                "  since      1970-01-01T00:00:00.000Z",
                "  helper     0.2.0",
                "  profile    AUTO · ECUs 7E8 · 2/3 fields · ice",
                "  audit      #4  voltage_low 1  last_stop —",
            ]
        );
    }

    #[tokio::test]
    async fn scan_prints_the_table_or_the_json_list_and_fails_cleanly() {
        let h = home("scan");
        let o = run_with(&s(&["scan", "--seconds", "10"]), &h, &Io(Ok(two()))).await;
        assert_eq!(o.stderr, ["scanning for 10 s (never pairs)…"]);
        assert!(o.stdout[0].contains("10:21:3E:4F:33:65   -58   OBDII"));
        assert!(o.stdout[0].contains("00:1D:A5:68:98:8B     —   V-LINK"));
        assert!(o.stdout[0].ends_with("xorgate-rs set obd.adapter <MAC>   (never pairs by name)"));
        let o = run_with(&s(&["scan", "--json"]), &h, &Io(Ok(two()))).await;
        assert_eq!(
            o.stdout,
            ["[\n  {\n    \"mac\": \"10:21:3E:4F:33:65\",\n    \"name\": \"OBDII\",\n    \"rssi\": -58\n  },\n  {\n    \"mac\": \"00:1D:A5:68:98:8B\",\n    \"name\": \"V-LINK\",\n    \"rssi\": null\n  }\n]"]
        );
        let o = run_with(&s(&["scan"]), &h, &Io(Err("exit null".into()))).await;
        assert_eq!(
            o.stderr,
            [
                "scanning for 25 s (never pairs)…",
                "obd scan failed: exit null"
            ]
        );
        assert_eq!(o.code, 1);
    }

    #[tokio::test]
    async fn bad_args_print_the_error_and_usage_with_exit_1() {
        let h = home("args");
        let io = Io(Ok(vec![]));
        let o = run_with(&s(&["scan", "--seconds", "0"]), &h, &io).await;
        assert_eq!(o.code, 1);
        assert_eq!(o.stderr[0], "obd: --seconds must be 1..120, got '0'");
        assert!(o.stderr[1].starts_with("Usage: xorgate-rs obd scan [--seconds N] [--json]"));
        let o = run_with(&s(&["pair"]), &h, &io).await;
        assert_eq!(o.stderr[0], "obd: unknown obd subcommand 'pair'");
        let o = run_with(&[], &h, &io).await;
        assert_eq!((o.code, o.stdout.len()), (0, 1));
    }
}
