//! `xorgate-rs status` against the REAL TS `statusCommand`
//! (`status.ts:28-120` over `manager.ts inspect :105-120`):
//! `fixtures/cli_set/status-render.json` (tsx-dumped) — stdout lines and exit
//! code for pidfile × state.json scenarios written by either agent (C10).
//! `ps` is injected (the tsx run used the real one on its own pid): a
//! `verified` case answers with the record's worker in the command line, a
//! `reused` one with another program. The unreadable-home case (EACCES needs
//! a non-root run) feeds `home_denied` directly.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
use xorgate_agent::cli::process_inspect::{ProcRun, ProcRunner};
use xorgate_agent::cli::status::{inspect, render, DaemonStatus, RenderCtx};

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    input: Input,
    expected: Expected,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    pidfile: Option<Value>,
    state: Option<Value>,
    state_text: Option<String>,
    identity: String,
    home_denied: bool,
    now: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Expected {
    lines: Vec<String>,
    exit_code: i32,
}

struct Ps(String);
impl ProcRunner for Ps {
    fn run(&self, _cmd: &str, _args: &[&str]) -> ProcRun {
        ProcRun {
            stdout: format!("   01:02:05 {}\n", self.0),
            failed: false,
            not_found: false,
        }
    }
}

#[test]
fn status_render_fixture() {
    let path = format!(
        "{}/../../fixtures/cli_set/status-render.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let fx: Fixture = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let me = std::process::id();
    let mut failures = Vec::new();
    for (i, c) in fx.cases.iter().enumerate() {
        let home: PathBuf =
            std::env::temp_dir().join(format!("xg-cli-status-{i}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        if let Some(mut p) = c.input.pidfile.clone() {
            // the TS used its own live pid; this test uses ours
            if p["pid"] != serde_json::json!(2147483000u32) {
                p["pid"] = serde_json::json!(me);
            }
            std::fs::write(home.join("agent.pid"), p.to_string()).unwrap();
        }
        if let Some(s) = &c.input.state {
            std::fs::write(home.join("state.json"), s.to_string()).unwrap();
        }
        if let Some(t) = &c.input.state_text {
            std::fs::write(home.join("state.json"), t).unwrap();
        }
        let worker = c
            .input
            .pidfile
            .as_ref()
            .and_then(|p| p["worker"].as_str())
            .unwrap_or("")
            .to_string();
        let cmd = match c.input.identity.as_str() {
            "reused" => "vim notes.txt".to_string(),
            _ => format!("node {worker}"),
        };
        let st = inspect(&home, c.input.now, &Ps(cmd));
        let ctx = RenderCtx {
            color: false,
            home: home.clone(),
            home_denied: c.input.home_denied,
            build: "<BUILD>".into(),
        };
        let home_s = home.display().to_string();
        let got: Vec<String> = render(&st, c.input.now, &ctx)
            .into_iter()
            .map(|l| {
                l.replace(&home_s, "<HOME>")
                    .replace(&format!("pid {me}"), "pid <PID>")
            })
            .collect();
        let code = if matches!(st, DaemonStatus::Running { .. }) {
            0
        } else {
            1
        };
        if got != c.expected.lines || code != c.expected.exit_code {
            failures.push(format!(
                "{}:\n  got  {got:?} ({code})\n  want {:?} ({})",
                c.name, c.expected.lines, c.expected.exit_code
            ));
        }
        let _ = std::fs::remove_dir_all(&home);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// ANSI colours as the TS `c(code, s)` (`status.ts:11-16`).
#[test]
fn colours_wrap_like_the_ts() {
    let ctx = RenderCtx {
        color: true,
        home: PathBuf::from("/h"),
        home_denied: false,
        build: "b".into(),
    };
    assert_eq!(
        render(&DaemonStatus::Stopped, 0.0, &ctx),
        vec![
            "\x1b[90m○\x1b[0m stopped \x1b[2m— daemon not running\x1b[0m".to_string(),
            "\x1b[2m  start it with: xorgate start\x1b[0m".to_string(),
        ]
    );
}
