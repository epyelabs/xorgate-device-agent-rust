//! `xorgate-rs set <key> <value>` — the I/O half of `src/commands/set.ts`
//! (`setCommand :391-531`, `setApnCommand + watch :533-671`), calling the pure
//! decisions of `xorgate_core::cli_set` in the TS order:
//!
//! 1. no key → USAGE (exit 1);
//! 2. `wifi` → Phase 4 (prints [`cli_set::WIFI_LATER`], exit 1; T8);
//! 3. the sudo note (`warnIfNeedlessSudo`, `:382-389`) — before every
//!    settings.json write path, including the failing ones;
//! 4. the key handler: validate → `loadSettings()` → mutate → `saveSettings`
//!    → the success lines (a write error is printed as Node's `err.message`
//!    and exits 1, as `main().catch` does in `cli.tsx:164-167`);
//! 5. `mode` without `--no-restart`: `inspect()`; the Rust agent cannot
//!    restart a systemd unit itself (see `cli_set::MODE_RESTART_UNAVAILABLE`);
//! 6. `apn`: validate → read the modem's APN block → write → verdict / watch
//!    (3 s polls, 90 s old-daemon cutoff, 8 min cap).
//!
//! settings.json is written with `xorgate_hal::atomic_write` — the helper
//! `settings_io::save_settings` wraps (temp + fsync + rename + dir fsync +
//! chown-back under sudo) — but from the JSON document, not the typed
//! `AgentSettings`, because the TS appends new keys where the typed struct
//! would re-order them (WP6 evidence ambiguity A1).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use xorgate_core::cli_set::{
    self, apn_after_save, apn_block, apn_document, apn_validate, parse_set_args, route, sudo_note,
    ApnWatch, KeyOutcome, Line, Route, Then, WatchStep, APN_WATCH_MAX_MS, APN_WATCH_POLL_MS,
};
use xorgate_core::config_report::ApnBlock;
use xorgate_core::js::JsJsonMap;
use xorgate_core::settings::AgentSettings;

/// Everything `set` touches outside its own logic, so tests can fake the
/// clock, the modem daemon's file and the daemon status.
pub trait SetIo {
    /// `runtimeHome()`.
    fn home(&self) -> PathBuf;
    /// `Date.now()`.
    fn now_ms(&self) -> i64;
    /// `await sleepMs(ms)`.
    fn sleep_ms(&self, ms: i64);
    /// `readModemTelemetry()`: the parsed file, `None` when absent/unparseable.
    fn read_modem(&self) -> Option<Value>;
    /// `warnIfNeedlessSudo`'s facts: `(isRoot(), owner uid, owner name)`.
    fn sudo_facts(&self) -> (bool, Option<u32>, Option<String>);
    /// `inspect().kind === "running"` for the `mode` restart branch.
    fn daemon_running(&self) -> bool;
    /// One `console.log` / `console.error` call.
    fn emit(&self, line: &Line);
}

/// The production [`SetIo`].
pub struct SystemIo {
    home: PathBuf,
    lte_file: PathBuf,
}

impl SystemIo {
    pub fn new() -> Self {
        SystemIo {
            home: crate::env::runtime_home(),
            lte_file: xorgate_hal::modem_file::lte_telemetry_file(
                std::env::var_os("LTE_TELEMETRY_FILE").map(|v| v.to_string_lossy().into_owned()),
            ),
        }
    }
}

impl Default for SystemIo {
    fn default() -> Self {
        Self::new()
    }
}

/// `fileOwner(path)?.uid` (`privilege.ts`).
fn file_uid(p: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.uid())
}

impl SetIo for SystemIo {
    fn home(&self) -> PathBuf {
        self.home.clone()
    }
    fn now_ms(&self) -> i64 {
        crate::now_ms()
    }
    fn sleep_ms(&self, ms: i64) {
        std::thread::sleep(std::time::Duration::from_millis(ms.max(0) as u64));
    }
    fn read_modem(&self) -> Option<Value> {
        xorgate_hal::modem_file::read_blocking(&self.lte_file, &xorgate_hal::io::NullTape).raw
    }
    fn sudo_facts(&self) -> (bool, Option<u32>, Option<String>) {
        let is_root = nix::unistd::geteuid().is_root();
        if !is_root {
            return (false, None, None);
        }
        // fileOwner(pidPath()) ?? fileOwner(runtimeHome()) ?? fileOwner(settingsPath())
        let uid = file_uid(&crate::pidfile::pid_path(&self.home))
            .or_else(|| file_uid(&self.home))
            .or_else(|| file_uid(&xorgate_hal::settings_io::settings_path(&self.home)));
        let name = uid.and_then(|u| {
            let passwd = xorgate_hal::read_lossy::read_text(Path::new("/etc/passwd")).ok()?;
            cli_set::username_for_uid(&passwd, u)
        });
        (true, uid, name)
    }
    fn daemon_running(&self) -> bool {
        matches!(
            super::status::inspect(
                &self.home,
                crate::now_ms() as f64,
                &super::process_inspect::SystemRunner
            ),
            super::status::DaemonStatus::Running { .. }
        )
    }
    fn emit(&self, line: &Line) {
        match line.stream {
            cli_set::Stream::Out => println!("{}", line.text),
            cli_set::Stream::Err => eprintln!("{}", line.text),
        }
    }
}

/// `xorgate-rs set …` (frozen signature): raw args after the verb.
pub fn main(args: &[String]) -> i32 {
    tape_cli(args);
    run(args, &SystemIo::new())
}

/// The `cli` tape event (`phase-2-cloud-plane.md` §Record/replay): when
/// `XORGATE_RS_TAPE` is set, append `{"w":<ms>,"k":"cli","argv":["set",…]}`
/// to `<tape>.cli.ndjson`. The agent's gz tape belongs to the agent's process
/// (one gzip stream, one writer), so the CLI uses a plain side file the
/// replay reader merges by wall time (handed to WP7, evidence A6).
/// Best-effort and silent.
fn tape_cli(args: &[String]) {
    let Some(tape) = std::env::var_os("XORGATE_RS_TAPE").filter(|t| !t.is_empty()) else {
        return;
    };
    let mut argv = vec!["set".to_string()];
    argv.extend(args.iter().cloned());
    let line = serde_json::json!({"w": crate::now_ms(), "k": "cli", "argv": argv});
    let mut path = PathBuf::from(tape).into_os_string();
    path.push(".cli.ndjson");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// `saveSettings(doc)`; on failure the `main().catch` line (Node's
/// `err.message` of the failing step: `open '<dir>/.settings.json.tmp.<pid>.<n>'`,
/// or `rename '<tmp>' -> '<path>'`; review-wp6 #7).
fn save(io: &dyn SetIo, doc: &Map<String, Value>) -> Result<(), String> {
    let path = xorgate_hal::settings_io::settings_path(&io.home());
    let bytes = serde_json::to_vec(&JsJsonMap(doc)).map_err(|e| e.to_string())?;
    xorgate_hal::atomic_write::atomic_write_bytes_traced(&path, &bytes)
        .map_err(|e| e.node_message())
}

fn load(io: &dyn SetIo) -> AgentSettings {
    xorgate_hal::settings_io::load_settings(&xorgate_hal::settings_io::settings_path(&io.home()))
}

fn emit_all(io: &dyn SetIo, lines: impl IntoIterator<Item = Line>) {
    for l in lines {
        io.emit(&l);
    }
}

/// `setCommand(rest)` (`set.ts:391-531`) → exit code.
pub fn run(rest: &[String], io: &dyn SetIo) -> i32 {
    let args = parse_set_args(rest);
    // `if (!key)` (`set.ts:398`): an empty positional is falsy too.
    let Some(key) = args.key.clone().filter(|k| !k.is_empty()) else {
        io.emit(&Line::err(cli_set::USAGE));
        return 1;
    };
    let r = route(&key);
    if r == Route::Wifi {
        io.emit(&Line::err(cli_set::WIFI_LATER));
        return 1;
    }
    let (is_root, uid, name) = io.sudo_facts();
    emit_all(io, sudo_note(is_root, uid, name.as_deref()));

    if r == Route::Apn {
        return set_apn(args.value.as_deref(), args.no_wait, io);
    }

    match cli_set::apply(&args, &load(io)) {
        KeyOutcome::Fail(lines) => {
            emit_all(io, lines.into_iter().map(Line::err));
            1
        }
        KeyOutcome::Save { doc, stdout, then } => {
            if let Err(msg) = save(io, &doc) {
                io.emit(&Line::err(msg));
                return 1;
            }
            emit_all(io, stdout.into_iter().map(Line::out));
            match then {
                Then::Done => 0,
                Then::ModeRestart => {
                    if !io.daemon_running() {
                        io.emit(&Line::out(cli_set::MODE_NOT_RUNNING));
                        return 0;
                    }
                    for l in cli_set::MODE_RESTART_UNAVAILABLE {
                        io.emit(&Line::err(l));
                    }
                    1
                }
            }
        }
    }
}

/// `readApnBlock()` (`set.ts:541-544`).
fn read_apn_block(io: &dyn SetIo) -> Option<ApnBlock> {
    apn_block(io.read_modem().as_ref())
}

/// `setApnCommand` (`set.ts:567-671`).
fn set_apn(value: Option<&str>, no_wait: bool, io: &dyn SetIo) -> i32 {
    let value = match apn_validate(value) {
        Ok(v) => v,
        Err(lines) => {
            emit_all(io, lines.into_iter().map(Line::err));
            return 1;
        }
    };
    let before = read_apn_block(io);
    if let Err(msg) = save(io, &apn_document(&load(io), value)) {
        io.emit(&Line::err(msg));
        return 1;
    }
    let after = apn_after_save(value, before.as_ref(), no_wait);
    emit_all(io, after.lines);
    if !after.watch {
        return after.exit_code;
    }

    let started = io.now_ms();
    let mut watch = ApnWatch::new(value, before);
    while io.now_ms() - started < APN_WATCH_MAX_MS {
        io.sleep_ms(APN_WATCH_POLL_MS);
        let cur = read_apn_block(io);
        match watch.poll(io.now_ms() - started, cur) {
            WatchStep::Continue(lines) => emit_all(io, lines),
            WatchStep::Finish(lines, code) => {
                emit_all(io, lines);
                return code;
            }
        }
    }
    io.emit(&ApnWatch::timeout_line());
    0
}
