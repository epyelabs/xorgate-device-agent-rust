//! `xorgate-rs status` — port of `src/commands/status.ts:1-120` over
//! `inspect()` (`src/daemon/manager.ts:85-120`) and the small formatters of
//! `src/lib/format.ts` (`formatBytes`, `uptimeLabel`, `ageLabel`).
//!
//! Reads EITHER agent's `agent.pid` + `state.json` under `XORGATE_HOME` (C10):
//! the pidfile through `pidfile::read_pidfile` (`parsePidRecord`'s defaults
//! fill the keys the Rust record omits), the snapshot through
//! `state::read_state` as a raw JSON value, printed with JS template-literal
//! semantics so whatever the other agent wrote renders exactly as the TS
//! renders it. Exit 0 only when running.
//!
//! Text is the TS text, including its `xorgate start` / `sudo xorgate status`
//! hints (C11 diffs it against `node dist/cli.js status`). The `build` line
//! names THIS binary's build, as the TS names its own tree's.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use serde_json::Value;
use xorgate_core::js::{number_to_string, to_fixed};
use xorgate_core::round::js_math_round;

use super::process_inspect::{
    classify_identity, inspect_process, is_alive, Identity, ProcRunner, SystemRunner,
    IDENTITY_TOLERANCE_SEC,
};
use crate::pidfile::{pid_path, read_pidfile, ParsedPidRecord};
use crate::state::{is_fresh, read_state};

/// `DaemonStatus` (`manager.ts:37-46`).
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonStatus {
    Running {
        record: ParsedPidRecord,
        identity: Identity,
        state: Option<Value>,
        state_fresh: bool,
    },
    Stale {
        record: ParsedPidRecord,
        /// `"no-process"` | `"pid-reused"`.
        reason: StaleReason,
    },
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    NoProcess,
    PidReused,
}

/// `intervalMsOf` (`manager.ts:85-87`).
fn interval_ms_of(record: &ParsedPidRecord) -> f64 {
    js_math_round(
        1000.0
            / if record.rate_hz > 0.0 {
                record.rate_hz
            } else {
                1.0
            },
    )
}

/// `inspect(now, runner)` (`manager.ts:105-120`) for the home `home`.
pub fn inspect(home: &Path, now: f64, runner: &dyn ProcRunner) -> DaemonStatus {
    let Some(record) = read_pidfile(&pid_path(home)) else {
        return DaemonStatus::Stopped;
    };
    if !is_alive(record.pid) {
        return DaemonStatus::Stale {
            record,
            reason: StaleReason::NoProcess,
        };
    }
    let info = inspect_process(record.pid, runner);
    let identity = classify_identity(&record, &info, now, IDENTITY_TOLERANCE_SEC);
    if identity == Identity::Reused {
        return DaemonStatus::Stale {
            record,
            reason: StaleReason::PidReused,
        };
    }
    let state = read_state(&home.join("state.json"));
    let state_fresh = state
        .as_ref()
        .and_then(|s| s.get("lastWriteAt"))
        .and_then(Value::as_f64)
        .is_some_and(|w| is_fresh(w, now, interval_ms_of(&record)));
    DaemonStatus::Running {
        record,
        identity,
        state,
        state_fresh,
    }
}

// ---------------------------------------------------------------------------
// JS value semantics for the template literals
// ---------------------------------------------------------------------------

/// `String(v)` / `${v}` (`None` = `undefined`).
pub fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// JS truthiness.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `ToNumber(v)` (`None` = `undefined` → NaN).
pub fn to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => xorgate_core::system::js_number(s),
        Some(Value::Array(a)) if a.is_empty() => 0.0,
        Some(Value::Array(a)) if a.len() == 1 => {
            to_number(Some(&Value::String(js_string(a.first()))))
        }
        Some(_) => f64::NAN,
    }
}

/// `Math.max(0, x)` (NaN propagates).
fn js_max0(x: f64) -> f64 {
    if x.is_nan() {
        f64::NAN
    } else {
        x.max(0.0)
    }
}

/// `Math.floor` as a JS number string.
fn floor_s(x: f64) -> String {
    number_to_string(x.floor())
}

/// `formatBytes` (`format.ts:17-27`) of a JSON value (`${n} B` prints the
/// value itself).
pub fn format_bytes(v: Option<&Value>) -> String {
    let n = to_number(v);
    if n < 1024.0 {
        return format!("{} B", js_string(v));
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut x = n / 1024.0;
    let mut i = 0;
    while x >= 1024.0 && i < units.len() - 1 {
        x /= 1024.0;
        i += 1;
    }
    format!("{} {}", to_fixed(x, 1), units[i])
}

/// `uptimeLabel` (`format.ts:54-64`).
pub fn uptime_label(ms: f64) -> String {
    let s = js_max0((ms / 1000.0).floor());
    let d = (s / 86400.0).floor();
    let h = ((s % 86400.0) / 3600.0).floor();
    let m = ((s % 3600.0) / 60.0).floor();
    let sec = s % 60.0;
    if d > 0.0 {
        return format!("{}d {}h", floor_s(d), floor_s(h));
    }
    if h > 0.0 {
        return format!("{}h {}m", floor_s(h), floor_s(m));
    }
    if m > 0.0 {
        return format!("{}m {}s", floor_s(m), number_to_string(sec));
    }
    format!("{}s", number_to_string(sec))
}

/// `ageLabel` (`format.ts:67-76`).
pub fn age_label(ts_ms: f64, now: f64) -> String {
    let ms = js_max0(now - ts_ms);
    if ms < 1000.0 {
        return "just now".into();
    }
    let s = ms / 1000.0;
    if s < 10.0 {
        return format!("{}s ago", to_fixed(s, 1));
    }
    if s < 60.0 {
        return format!("{}s ago", floor_s(s));
    }
    if s < 3600.0 {
        return format!("{}m ago", floor_s(s / 60.0));
    }
    if s < 86400.0 {
        return format!("{}h ago", floor_s(s / 3600.0));
    }
    format!("{}d ago", floor_s(s / 86400.0))
}

// ---------------------------------------------------------------------------
// rendering (status.ts:35-120)
// ---------------------------------------------------------------------------

/// Everything `print` reads besides the status itself.
#[derive(Debug, Clone)]
pub struct RenderCtx {
    /// `process.stdout.isTTY && NO_COLOR === undefined` (`status.ts:11`).
    pub color: bool,
    /// `runtimeHome()`.
    pub home: PathBuf,
    /// `homePermissionDenied()` (`status.ts:19-26`) — consulted only when stopped.
    pub home_denied: bool,
    /// `formatBuildInfo()`.
    pub build: String,
}

struct Paint(bool);

impl Paint {
    fn c(&self, code: &str, s: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn green(&self, s: &str) -> String {
        self.c("32", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.c("33", s)
    }
    fn gray(&self, s: &str) -> String {
        self.c("90", s)
    }
    fn dim(&self, s: &str) -> String {
        self.c("2", s)
    }
}

/// `print(st, now)` (`status.ts:35-120`): the stdout lines.
pub fn render(st: &DaemonStatus, now: f64, ctx: &RenderCtx) -> Vec<String> {
    let p = Paint(ctx.color);
    let home = ctx.home.display().to_string();
    let mut out = Vec::new();
    match st {
        DaemonStatus::Stopped => {
            if ctx.home_denied {
                out.push(format!(
                    "{} unreadable {}",
                    p.yellow("○"),
                    p.dim(&format!("— {home} is owned by another user"))
                ));
                out.push(p.dim(
                    "  the agent likely runs under systemd as root — try: sudo xorgate status",
                ));
                return out;
            }
            out.push(format!(
                "{} stopped {}",
                p.gray("○"),
                p.dim("— daemon not running")
            ));
            out.push(p.dim("  start it with: xorgate start"));
            out
        }
        DaemonStatus::Stale { record, reason } => {
            let why = match reason {
                StaleReason::PidReused => "pid was recycled",
                StaleReason::NoProcess => "process is gone",
            };
            out.push(format!(
                "{} stale {}",
                p.yellow("◌"),
                p.dim(&format!("— pidfile for pid {}, but {why}", record.pid))
            ));
            out.push(p.dim("  it will be cleaned on the next start/stop"));
            out
        }
        DaemonStatus::Running {
            record,
            identity,
            state,
            state_fresh,
        } => {
            let id_note = if *identity == Identity::Verified {
                p.gray("(identity verified)")
            } else {
                p.yellow("(identity unverified — ps unavailable)")
            };
            out.push(format!(
                "{} running   pid {}   up {}   {id_note}",
                p.green("●"),
                record.pid,
                uptime_label(now - record.started_at)
            ));
            out.push(format!(
                "  device     {}   mode {}   rate {} Hz{}",
                record.device_id,
                record.mode,
                number_to_string(record.rate_hz),
                if record.offline {
                    p.gray("   offline")
                } else {
                    String::new()
                }
            ));
            out.push(format!("  build      {}", ctx.build));
            if let Some(state) = state.as_ref().filter(|s| truthy(Some(s))) {
                let g = |k: &str| state.get(k);
                out.push(format!("  endpoint   {}", js_string(g("endpoint"))));
                let conn = js_string(g("connection"));
                let conn_c = if g("connection").and_then(Value::as_str) == Some("connected") {
                    p.green(&conn)
                } else {
                    p.yellow(&conn)
                };
                out.push(format!(
                    "  connection {conn_c}   {}",
                    p.gray(&format!(
                        "since {}",
                        age_label(to_number(g("connectionSince")), now)
                    ))
                ));
                let last = if truthy(g("lastPublishAt")) {
                    age_label(to_number(g("lastPublishAt")), now)
                } else {
                    "never".to_string()
                };
                let fresh = if *state_fresh {
                    p.gray("(fresh)")
                } else {
                    p.yellow("(stale — worker may be wedged)")
                };
                out.push(format!(
                    "  telemetry  seq {}  published {}  errors {}  last {last}  {fresh}",
                    js_string(g("seq")),
                    js_string(g("published")),
                    js_string(g("publishErrors")),
                ));
                if truthy(g("recording")) {
                    let r = g("recording").expect("truthy");
                    let rg = |k: &str| r.get(k);
                    let dropped = to_number(rg("droppedSegments"));
                    if truthy(rg("enabled")) {
                        let session = if truthy(rg("sessionId")) {
                            let sid: Vec<u16> = js_string(rg("sessionId")).encode_utf16().collect();
                            let head = String::from_utf16_lossy(&sid[..sid.len().min(8)]);
                            format!("session {head}…")
                        } else {
                            String::new()
                        };
                        let backlog = format!(
                            "backlog {} seg / {}",
                            js_string(rg("backlogSegments")),
                            format_bytes(rg("backlogBytes"))
                        );
                        let dropped_s = if dropped > 0.0 {
                            format!(
                                "  {}",
                                p.yellow(&format!("dropped {}", js_string(rg("droppedSegments"))))
                            )
                        } else {
                            String::new()
                        };
                        out.push(format!(
                            "  recording  {}   {session}  {backlog}{dropped_s}",
                            p.green("on")
                        ));
                    } else if to_number(rg("backlogSegments")) > 0.0 || dropped > 0.0 {
                        out.push(format!(
                            "  recording  off  {}",
                            p.gray(&format!(
                                "backlog {} seg / {}{}",
                                js_string(rg("backlogSegments")),
                                format_bytes(rg("backlogBytes")),
                                if dropped > 0.0 {
                                    format!(", dropped {}", js_string(rg("droppedSegments")))
                                } else {
                                    String::new()
                                }
                            ))
                        ));
                    }
                }
                // (state.latest?.media?.streams ?? []).filter(s => s.recording !== undefined)
                let streams = g("latest")
                    .and_then(|l| l.get("media"))
                    .and_then(|m| m.get("streams"))
                    .and_then(Value::as_array);
                let parts: Vec<String> = streams
                    .into_iter()
                    .flatten()
                    .filter(|s| s.get("recording").is_some())
                    .map(|s| {
                        format!(
                            "{} {}",
                            js_string(s.get("streamKey")),
                            if truthy(s.get("recording")) {
                                p.green("recording")
                            } else {
                                p.gray("idle")
                            }
                        )
                    })
                    .collect();
                if !parts.is_empty() {
                    out.push(format!("  video      {}", parts.join("   ")));
                }
                // `if (state.obd) console.log(\`  ${obdStatusLine(state.obd)}\`)`
                // (`status.ts:116` @ 09da38d).
                if let Some(obd) = g("obd").filter(|v| truthy(Some(v))).and_then(|v| {
                    serde_json::from_value::<xorgate_core::telemetry::ObdStatus>(v.clone()).ok()
                }) {
                    out.push(format!("  {}", xorgate_core::obd::obd_status_line(&obd)));
                }
                if truthy(g("lastError")) {
                    out.push(format!(
                        "  last error {}",
                        p.yellow(&js_string(g("lastError")))
                    ));
                }
            } else {
                out.push(p.gray("  (no state snapshot yet)"));
            }
            out.push(p.dim(&format!(
                "  log        {}",
                ctx.home.join("agent.log").display()
            )));
            out
        }
    }
}

/// `homePermissionDenied` (`status.ts:19-26`): the home exists but `R_OK|X_OK`
/// fails with EACCES.
pub fn home_permission_denied(home: &Path) -> bool {
    use nix::unistd::{access, AccessFlags};
    matches!(
        access(home, AccessFlags::R_OK | AccessFlags::X_OK),
        Err(nix::errno::Errno::EACCES)
    )
}

/// `xorgate-rs status` (`status.ts:28-33`): args are ignored, as in the TS.
pub fn main(_args: &[String]) -> i32 {
    let now = crate::now_ms() as f64;
    let home = crate::env::runtime_home();
    let st = inspect(&home, now, &SystemRunner);
    let ctx = RenderCtx {
        color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        home_denied: matches!(st, DaemonStatus::Stopped) && home_permission_denied(&home),
        home,
        build: crate::build_info::load().format(),
    };
    // EPIPE (`status | head -1`): `console.log` swallows it and the exit code
    // stays the state's (`status.ts:32` sets it first) — integration review #1.
    let _ = super::logs::write_lines(render(&st, now, &ctx));
    if matches!(st, DaemonStatus::Running { .. }) {
        0
    } else {
        1
    }
}

#[allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: test harness spawns, not the agent
#[cfg(test)]
mod tests {
    //! `test/manager.test.ts` `inspect()` (5 it()s) + the formatters.
    use super::*;
    use crate::cli::process_inspect::ProcRun;

    const NOW: f64 = 2_000_000_000_000.0;

    struct Ps(f64, &'static str);
    impl ProcRunner for Ps {
        fn run(&self, _cmd: &str, _args: &[&str]) -> ProcRun {
            let s = self.0 as u64;
            ProcRun {
                stdout: format!("{:02}:{:02} {}\n", s / 60, s % 60, self.1),
                failed: false,
                not_found: false,
            }
        }
    }

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-status-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn pidfile(home: &Path, pid: u32, started_at: f64) {
        std::fs::write(
            pid_path(home),
            serde_json::json!({"pid": pid, "startedAt": started_at, "deviceId": "d", "mode": "sim",
                "rateHz": 1, "offline": true, "worker": "/x/worker.js", "argv": [], "nodeVersion": "v20"})
            .to_string(),
        )
        .unwrap();
    }

    fn dead_pid() -> u32 {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    #[test]
    fn reports_stopped_with_no_pidfile() {
        let h = home("stopped");
        assert_eq!(
            inspect(&h, NOW, &Ps(1.0, "node /x/worker.js")),
            DaemonStatus::Stopped
        );
    }

    #[test]
    fn reports_running_and_identity_verified_for_our_own_live_pid() {
        let h = home("running");
        pidfile(&h, std::process::id(), NOW);
        match inspect(&h, NOW, &Ps(1.0, "node /x/worker.js")) {
            DaemonStatus::Running { identity, .. } => assert_eq!(identity, Identity::Verified),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reports_stale_no_process_for_a_dead_pid() {
        let h = home("dead");
        pidfile(&h, dead_pid(), NOW);
        match inspect(&h, NOW, &Ps(1.0, "node /x/worker.js")) {
            DaemonStatus::Stale { reason, .. } => assert_eq!(reason, StaleReason::NoProcess),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reports_stale_pid_reused_when_the_live_pid_runs_some_other_program() {
        let h = home("reused");
        pidfile(&h, std::process::id(), NOW - 100_000.0);
        match inspect(&h, NOW, &Ps(3.0, "vim notes.txt")) {
            DaemonStatus::Stale { reason, .. } => assert_eq!(reason, StaleReason::PidReused),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reports_running_but_not_fresh_when_the_state_file_is_old() {
        let h = home("oldstate");
        pidfile(&h, std::process::id(), NOW);
        std::fs::write(
            h.join("state.json"),
            serde_json::json!({"v":1,"writtenBy":"test","deviceId":"d","mode":"sim","rateHz":1,
                "endpoint":"offline","offline":true,"pid":1,"startedAt":NOW,"lastWriteAt":NOW-999_999.0,
                "connection":"connected","connectionSince":NOW,"seq":5,"published":5,"publishErrors":0,
                "lastPublishAt":NOW,"lastError":null,"latest":null,"recentEvents":[]})
            .to_string(),
        )
        .unwrap();
        match inspect(&h, NOW, &Ps(1.0, "node /x/worker.js")) {
            DaemonStatus::Running {
                state_fresh, state, ..
            } => {
                assert!(!state_fresh);
                assert_eq!(state.unwrap()["seq"], 5);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn formatters_match_format_ts() {
        assert_eq!(uptime_label(12_000.0), "12s");
        assert_eq!(uptime_label(252_000.0), "4m 12s");
        assert_eq!(
            uptime_label(3_720_000.0 * 3.0 / 3.0 + 3_600_000.0 * 2.0),
            "3h 2m"
        );
        assert_eq!(uptime_label(-5.0), "0s");
        assert_eq!(age_label(NOW - 900.0, NOW), "just now");
        assert_eq!(age_label(NOW - 900.0 - 100.0, NOW), "1.0s ago");
        assert_eq!(age_label(NOW - 9_960.0, NOW), "10.0s ago");
        assert_eq!(age_label(NOW - 30_000.0, NOW), "30s ago");
        assert_eq!(age_label(NOW - 7_200_000.0, NOW), "2h ago");
        assert_eq!(age_label(f64::NAN, NOW), "NaNd ago");
        assert_eq!(format_bytes(Some(&serde_json::json!(512))), "512 B");
        assert_eq!(format_bytes(Some(&serde_json::json!(1536))), "1.5 KB");
        assert_eq!(
            format_bytes(Some(&serde_json::json!(5_368_709_120u64))),
            "5.0 GB"
        );
    }
}
