//! `xorgate-rs manage` (and the deprecated `monitor` alias) — the live TUI,
//! ported from the ink app of `src/commands/manage.tsx` +
//! `src/monitor/{MonitorApp,MonitorHeader,modules,format,useDaemonState,
//! useInterfaceStatus,useLogTail}` + `src/lib/useTerminalSize.ts` to ratatui
//! over crossterm.
//!
//! Runtime: one `tokio::select!` over the crossterm `EventStream`, the
//! poll timers (`timer::SetInterval`, Node `setInterval` re-arm-from-fire
//! semantics, CF55) and a channel the probes report on. Every probe (`ps` for
//! the daemon identity, `ping`, `nmcli`, `lsusb`, `i2cdetect`, file reads)
//! runs in a spawned task, so the runtime never blocks; a result from a pane
//! that has since closed is dropped by generation (the TS `alive` guard).
//! Cadences are the TS ones: daemon status 1 s (first poll at once),
//! interfaces 10 s (at once), log tail 1 s while shown, LTE link 2 s while the
//! LTE pane is open in real mode, `lsusb`/`i2cdetect` once per pane open,
//! the Settings wifi probe on open and after each toggle.
//!
//! Terminal: alternate screen + raw mode via `ratatui::try_init` (whose panic
//! hook restores the terminal before the panic message); restored on `q`,
//! Ctrl+C, SIGTERM/SIGHUP/SIGINT and errors. Quitting never stops the daemon.
//! `components/Header.tsx` / `lib/useSystemMetrics.ts` are not part of the
//! `manage` tree @89b5c64 (dead code) and are not ported.

pub mod app;
pub mod format;
pub mod modules;
pub mod view;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ratatui::backend::Backend;
use ratatui::Terminal;
use serde_json::Value;
use tokio::sync::mpsc;
use xorgate_core::imu::ImuDetect;
use xorgate_hal::network_io::{IfaceStatus, WifiPresence};

use self::app::{
    App, Effect, Key, KeyFacts, DAEMON_POLL_MS, IFACE_POLL_MS, LINK_POLL_MS, LOG_MAX_LINES,
    LOG_POLL_MS, UNIT,
};
use self::modules::ModemPresence;
use self::view::HostInfo;
use crate::cli::status::DaemonStatus;
use crate::ctx::BoxFut;
use crate::timer::SetInterval;

/// Everything the TUI reads or does outside itself (tests inject a fake).
pub trait ManageIo: Send + Sync + 'static {
    /// `inspect(t)` → `(status, t)` (`useDaemonState.ts:26-32`).
    fn inspect(&self) -> BoxFut<'static, (DaemonStatus, f64)>;
    /// `interfaceStatuses()` (`useInterfaceStatus.ts:14`).
    fn interfaces(&self) -> BoxFut<'static, Vec<IfaceStatus>>;
    /// `detectWifi()` (`MonitorApp.tsx:91`).
    fn detect_wifi(&self) -> BoxFut<'static, WifiPresence>;
    /// `setWifi(on)` (`MonitorApp.tsx:114`; its message is not shown).
    fn set_wifi(&self, on: bool) -> BoxFut<'static, ()>;
    /// `readLogTail(12)` (`useLogTail.ts:11`) — journal fallback (O9).
    fn log_tail(&self) -> BoxFut<'static, Vec<String>>;
    /// `detectModem()` (`modules.tsx:243`).
    fn detect_modem(&self) -> BoxFut<'static, ModemPresence>;
    /// `readModemTelemetry()` (`modules.tsx:245`).
    fn read_link(&self) -> BoxFut<'static, Option<Value>>;
    /// `detectImu()` (`modules.tsx:101`).
    fn detect_imu(&self) -> BoxFut<'static, ImuDetect>;
    /// `saveSettings({ ...loadSettings(), mode })` (`MonitorApp.tsx:143`).
    fn save_mode(&self, mode: &str) -> Result<(), String>;
    /// `ifaceForIp(sshServerIp(process.env.SSH_CONNECTION))`.
    fn ssh_iface(&self) -> Option<String>;
    /// `loadSettings().mode`.
    fn settings_mode(&self) -> Option<String>;
}

struct Facts<'a>(&'a dyn ManageIo);

impl KeyFacts for Facts<'_> {
    fn ssh_iface(&self) -> Option<String> {
        self.0.ssh_iface()
    }
    fn settings_mode(&self) -> Option<String> {
        self.0.settings_mode()
    }
}

/// What the runtime feeds the loop besides timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    /// Terminal resized: redraw.
    Resize,
    /// SIGINT/SIGTERM/SIGHUP or the event stream ended.
    Quit,
}

/// A probe result.
enum Msg {
    Status(DaemonStatus, f64),
    Interfaces(Vec<IfaceStatus>),
    Wifi(u64, WifiPresence),
    WifiSettled,
    Logs(Vec<String>),
    Modem(u64, ModemPresence),
    Link(u64, Option<Value>),
    Imu(u64, ImuDetect),
}

/// ink-spinner `dots` (cli-spinners): 80 ms per frame.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_MS: u64 = 80;

async fn tick_opt(iv: &mut Option<SetInterval>) {
    match iv {
        Some(i) => {
            i.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Mount tracking of the pane-local effects.
#[derive(Default)]
struct Mounts {
    wifi_key: Option<Option<u64>>,
    wifi_gen: u64,
    lte_mounted: bool,
    lte_real: bool,
    lte_gen: u64,
    imu_mounted: bool,
    imu_real: bool,
    imu_gen: u64,
    logs: bool,
}

/// Draw the app into a ratatui terminal (any backend; `TestBackend` in the
/// tests).
pub fn draw_to<B: Backend>(
    term: &mut Terminal<B>,
    app: &App,
    host: &HostInfo,
    spinner: &str,
) -> Result<(), String> {
    term.draw(|f| view::draw(f, app, host, spinner))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The TUI loop; `draw` renders one frame (the production [`draw_to`] on the
/// real terminal, a `TestBackend` recorder in the tests). Returns when the
/// user quits or `input` says so.
pub async fn run_loop(
    io: Arc<dyn ManageIo>,
    mut input: mpsc::UnboundedReceiver<Input>,
    host: HostInfo,
    mut draw: impl FnMut(&App, &HostInfo, &str) -> Result<(), String>,
) -> Result<(), String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut app = App::default();
    let mut m = Mounts::default();
    let started = tokio::time::Instant::now();

    let poll_status = |io: &Arc<dyn ManageIo>, tx: &mpsc::UnboundedSender<Msg>| {
        let (io, tx) = (io.clone(), tx.clone());
        tokio::spawn(async move {
            let (st, t) = io.inspect().await;
            let _ = tx.send(Msg::Status(st, t));
        });
    };
    let poll_ifaces = |io: &Arc<dyn ManageIo>, tx: &mpsc::UnboundedSender<Msg>| {
        let (io, tx) = (io.clone(), tx.clone());
        tokio::spawn(async move {
            let s = io.interfaces().await;
            let _ = tx.send(Msg::Interfaces(s));
        });
    };
    let poll_logs = |io: &Arc<dyn ManageIo>, tx: &mpsc::UnboundedSender<Msg>| {
        let (io, tx) = (io.clone(), tx.clone());
        tokio::spawn(async move {
            let l = io.log_tail().await;
            let _ = tx.send(Msg::Logs(l));
        });
    };
    let poll_link = |io: &Arc<dyn ManageIo>, tx: &mpsc::UnboundedSender<Msg>, gen: u64| {
        let (io, tx) = (io.clone(), tx.clone());
        tokio::spawn(async move {
            let l = io.read_link().await;
            let _ = tx.send(Msg::Link(gen, l));
        });
    };

    // `void poll(); setInterval(poll, pollMs)` for both header hooks.
    poll_status(&io, &tx);
    let mut status_iv = SetInterval::new(Duration::from_millis(DAEMON_POLL_MS));
    poll_ifaces(&io, &tx);
    let mut iface_iv = SetInterval::new(Duration::from_millis(IFACE_POLL_MS));
    let mut log_iv: Option<SetInterval> = None;
    let mut link_iv: Option<SetInterval> = None;
    let mut spin_iv = SetInterval::new(Duration::from_millis(SPINNER_MS));

    loop {
        // --- effects keyed on state (the React `useEffect` dependencies) ---
        let wifi_key = app.wifi_probe_key();
        if m.wifi_key != Some(wifi_key) {
            m.wifi_key = Some(wifi_key);
            m.wifi_gen += 1;
            match wifi_key {
                None => app.wifi = None,
                Some(_) => {
                    let (io, tx, gen) = (io.clone(), tx.clone(), m.wifi_gen);
                    tokio::spawn(async move {
                        let w = io.detect_wifi().await;
                        let _ = tx.send(Msg::Wifi(gen, w));
                    });
                }
            }
        }
        let lte_mounted = app.running() && app.module() == modules::ModuleId::Lte;
        let lte_real = lte_mounted && app.real();
        if lte_mounted != m.lte_mounted && !lte_mounted {
            app.modem = None;
            app.link = None;
        }
        if lte_real != m.lte_real {
            m.lte_gen += 1;
            if lte_real {
                let (io2, tx2, gen) = (io.clone(), tx.clone(), m.lte_gen);
                tokio::spawn(async move {
                    let d = io2.detect_modem().await;
                    let _ = tx2.send(Msg::Modem(gen, d));
                });
                poll_link(&io, &tx, m.lte_gen);
                link_iv = Some(SetInterval::new(Duration::from_millis(LINK_POLL_MS)));
            } else {
                link_iv = None;
            }
        }
        m.lte_mounted = lte_mounted;
        m.lte_real = lte_real;
        let imu_mounted = app.running() && app.module() == modules::ModuleId::Imu;
        let imu_real = imu_mounted && app.real();
        if imu_mounted != m.imu_mounted && !imu_mounted {
            app.imu_detect = None;
        }
        if imu_real != m.imu_real {
            m.imu_gen += 1;
            if imu_real {
                let (io2, tx2, gen) = (io.clone(), tx.clone(), m.imu_gen);
                tokio::spawn(async move {
                    let d = io2.detect_imu().await;
                    let _ = tx2.send(Msg::Imu(gen, d));
                });
            }
        }
        m.imu_mounted = imu_mounted;
        m.imu_real = imu_real;
        if app.show_logs != m.logs {
            m.logs = app.show_logs;
            if app.show_logs {
                poll_logs(&io, &tx);
                log_iv = Some(SetInterval::new(Duration::from_millis(LOG_POLL_MS)));
            } else {
                log_iv = None;
            }
        }

        // --- draw ---
        let frame_i = (started.elapsed().as_millis() / SPINNER_MS as u128) as usize % SPINNER.len();
        draw(&app, &host, SPINNER[frame_i])?;

        let spinning = (app.imu_probing() && app.imu_detect.is_none())
            || (app.lte_probing() && app.modem.is_none());
        tokio::select! {
            inp = input.recv() => {
                let Some(inp) = inp else { return Ok(()) };
                match inp {
                    Input::Quit => return Ok(()),
                    Input::Resize => {}
                    Input::Key(k) => {
                        for eff in app.key(k, &Facts(io.as_ref())) {
                            match eff {
                                Effect::Exit => return Ok(()),
                                Effect::SaveMode(mode) => {
                                    app.last_msg = Some(match io.save_mode(&mode) {
                                        Ok(()) => format!(
                                            "✓ mode = {mode} (persisted to settings.json) — restart to apply: sudo systemctl restart {UNIT}"
                                        ),
                                        Err(e) => e,
                                    });
                                }
                                Effect::SetWifi(on) => {
                                    let (io, tx) = (io.clone(), tx.clone());
                                    tokio::spawn(async move {
                                        io.set_wifi(on).await;
                                        let _ = tx.send(Msg::WifiSettled);
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ = status_iv.tick() => poll_status(&io, &tx),
            _ = iface_iv.tick() => poll_ifaces(&io, &tx),
            _ = tick_opt(&mut log_iv) => poll_logs(&io, &tx),
            _ = tick_opt(&mut link_iv) => poll_link(&io, &tx, m.lte_gen),
            _ = spin_iv.tick(), if spinning => {}
            Some(msg) = rx.recv() => match msg {
                Msg::Status(st, t) => {
                    app.status = st;
                    app.now = t;
                }
                Msg::Interfaces(s) => app.interfaces = s,
                Msg::Wifi(gen, w) => {
                    if gen == m.wifi_gen && app.on_settings() {
                        app.wifi = Some(w);
                    }
                }
                Msg::WifiSettled => app.wifi_settled(),
                Msg::Logs(l) => {
                    if app.show_logs {
                        app.log_lines = l;
                    }
                }
                Msg::Modem(gen, d) => {
                    if gen == m.lte_gen && m.lte_real {
                        app.modem = Some(d);
                    }
                }
                Msg::Link(gen, l) => {
                    if gen == m.lte_gen && m.lte_real {
                        app.link = l;
                    }
                }
                Msg::Imu(gen, d) => {
                    if gen == m.imu_gen && m.imu_real {
                        app.imu_detect = Some(d);
                    }
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// the real world
// ---------------------------------------------------------------------------

/// The production [`ManageIo`].
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

/// `isMock()` (`lib/platform.ts:37-42`): `XORGATE_MOCK=1|true` forces mock,
/// `0|false` forces real, else mock off-Linux.
pub fn is_mock() -> bool {
    match std::env::var("XORGATE_MOCK").as_deref() {
        Ok("1") | Ok("true") => true,
        Ok("0") | Ok("false") => false,
        _ => !cfg!(target_os = "linux"),
    }
}

/// `hostLabel()` (`lib/platform.ts:45-48`): `mock`, the device-tree model
/// (NULs stripped, trimmed), or `<platform> host`.
pub fn host_label() -> String {
    if is_mock() {
        return "mock".to_string();
    }
    match std::fs::read("/proc/device-tree/model") {
        Ok(b) => {
            let s = String::from_utf8_lossy(&b).replace('\0', "");
            xorgate_core::js::trim(&s).to_string()
        }
        Err(_) => format!("{} host", std::env::consts::OS),
    }
}

/// `parseOsRelease(text)` (`hardware/system.ts:137-155`).
pub fn parse_os_release(text: &str) -> Option<String> {
    let mut map: Vec<(String, String)> = Vec::new();
    for line in text.split('\n') {
        let Some(eq) = line.find('=') else { continue };
        let (k, rest) = (&line[..eq], &line[eq + 1..]);
        // `.` matches no line terminator: a CR (or U+2028/9) fails the regex.
        if rest.contains(['\r', '\u{2028}', '\u{2029}']) {
            continue;
        }
        if k.is_empty()
            || !k
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            continue;
        }
        let mut v = xorgate_core::js::trim(rest).to_string();
        if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            v = v[1..v.len() - 1].to_string();
        } else if v == "\"" || v == "'" {
            // `"x".slice(1, -1)` of a one-char quote is "".
            v = String::new();
        }
        match map.iter_mut().find(|(mk, _)| mk == k) {
            Some(e) => e.1 = v,
            None => map.push((k.to_string(), v)),
        }
    }
    let get = |k: &str| map.iter().find(|(mk, _)| mk == k).map(|(_, v)| v.clone());
    let name = get("NAME");
    let codename = get("VERSION_CODENAME");
    if let (Some(n), Some(c)) = (
        name.as_ref().filter(|s| !s.is_empty()),
        codename.filter(|s| !s.is_empty()),
    ) {
        let first = n.split_whitespace().next().unwrap_or("");
        let mut cs = c.chars();
        let cap = match cs.next() {
            Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
            None => String::new(),
        };
        return Some(format!("{first} {cap}"));
    }
    get("PRETTY_NAME").or(name)
}

/// `osInfo()` (`hardware/system.ts:158-178`) as `"<pretty> <arch>"`.
pub fn os_info() -> String {
    let pretty = std::fs::read("/etc/os-release")
        .ok()
        .and_then(|b| parse_os_release(&String::from_utf8_lossy(&b)))
        .unwrap_or_else(|| match std::env::consts::OS {
            "linux" => "Linux".to_string(),
            "macos" => "Darwin".to_string(),
            o => o.to_string(),
        });
    // `archLabel()`: node's arm64 → aarch64, x64 → x86_64 (= Rust's names).
    format!("{pretty} {}", std::env::consts::ARCH)
}

/// The host facts the header prints, read once.
pub fn host_info() -> HostInfo {
    HostInfo {
        host_label: host_label(),
        is_mock: is_mock(),
        os: os_info(),
        gps_port: std::env::var("GPS_NMEA_PORT").unwrap_or_else(|_| "/dev/ttyUSB1".to_string()),
    }
}

/// `listTtyUsb()` (`hardware/modem.ts:231-241`).
fn list_tty_usb() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut v: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.strip_prefix("ttyUSB")
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|n| format!("/dev/{n}"))
        .collect();
    v.sort();
    v
}

impl ManageIo for SystemIo {
    fn inspect(&self) -> BoxFut<'static, (DaemonStatus, f64)> {
        let home = self.home.clone();
        Box::pin(async move {
            let t = crate::now_ms() as f64;
            let st = tokio::task::spawn_blocking(move || {
                crate::cli::status::inspect(&home, t, &crate::cli::process_inspect::SystemRunner)
            })
            .await
            .unwrap_or(DaemonStatus::Stopped);
            (st, t)
        })
    }
    fn interfaces(&self) -> BoxFut<'static, Vec<IfaceStatus>> {
        Box::pin(xorgate_hal::network_io::interface_statuses())
    }
    fn detect_wifi(&self) -> BoxFut<'static, WifiPresence> {
        Box::pin(xorgate_hal::network_io::detect_wifi())
    }
    fn set_wifi(&self, on: bool) -> BoxFut<'static, ()> {
        Box::pin(async move {
            let _ = xorgate_hal::network_io::set_wifi(on).await;
        })
    }
    fn log_tail(&self) -> BoxFut<'static, Vec<String>> {
        let log = self.home.join("agent.log");
        Box::pin(async move {
            if log.exists() {
                let p = log.clone();
                return tokio::task::spawn_blocking(move || {
                    crate::cli::logs::read_log_tail(LOG_MAX_LINES as f64, &p)
                })
                .await
                .unwrap_or_default();
            }
            // O9: a systemd agent logs to the journal (`xorgate-rs logs` does
            // the same fallback).
            let mut args = crate::cli::logs::journal_args(LOG_MAX_LINES as f64, false);
            args.push("--no-pager".to_string());
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let res =
                xorgate_hal::exec::run("journalctl", &argv, Some(Duration::from_millis(3000)))
                    .await;
            if res.failed {
                return Vec::new();
            }
            let mut lines: Vec<String> = res.stdout.split('\n').map(str::to_string).collect();
            if lines.last().is_some_and(String::is_empty) {
                lines.pop();
            }
            lines
        })
    }
    fn detect_modem(&self) -> BoxFut<'static, ModemPresence> {
        Box::pin(async move {
            let tty_ports = list_tty_usb();
            let res = xorgate_hal::exec::run("lsusb", &[], None).await;
            if res.failed {
                return ModemPresence {
                    present: !tty_ports.is_empty(),
                    tty_ports,
                    ..Default::default()
                };
            }
            let m = xorgate_core::modem::find_modem_line_default(&res.stdout);
            ModemPresence {
                present: m.is_some(),
                usb_id: m.as_ref().map(|m| m.usb_id.clone()),
                description: m.map(|m| m.description),
                tty_ports,
            }
        })
    }
    fn read_link(&self) -> BoxFut<'static, Option<Value>> {
        let f = self.lte_file.clone();
        Box::pin(async move {
            xorgate_hal::modem_file::read(&f, &xorgate_hal::io::NullTape)
                .await
                .raw
        })
    }
    fn detect_imu(&self) -> BoxFut<'static, ImuDetect> {
        Box::pin(async move {
            if is_mock() {
                // `detectImu()` on a mock host (`imu.ts:126-128`).
                let mut d =
                    xorgate_core::imu::build_detect(vec![0x4b, xorgate_core::imu::IMU_BMS_ADDRESS]);
                d.imu_address = Some(0x4b);
                return d;
            }
            xorgate_hal::imu_detect::ImuDetector::new()
                .detect(&xorgate_hal::io::NullTape)
                .await
        })
    }
    fn save_mode(&self, mode: &str) -> Result<(), String> {
        save_mode_in(&self.home, mode)
    }
    fn ssh_iface(&self) -> Option<String> {
        let ssh = std::env::var("SSH_CONNECTION").ok();
        xorgate_hal::network_io::iface_for_ip(xorgate_core::network::ssh_server_ip(ssh.as_deref()))
    }
    fn settings_mode(&self) -> Option<String> {
        let s = xorgate_hal::settings_io::load_settings(&xorgate_hal::settings_io::settings_path(
            &self.home,
        ));
        xorgate_core::cli_set::document(&s)
            .get("mode")
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// `saveSettings({ ...loadSettings(), mode })` for the home `home`: the
/// allowlisted loaded document with `mode` set in its slot (appended when
/// new), written atomically as `JSON.stringify` bytes (CF40 — the writer of
/// `xorgate-rs set`); a failure is Node's `err.message`.
pub fn save_mode_in(home: &std::path::Path, mode: &str) -> Result<(), String> {
    let path = xorgate_hal::settings_io::settings_path(home);
    let loaded = xorgate_hal::settings_io::load_settings(&path);
    let mut doc = xorgate_core::cli_set::document(&loaded);
    doc.insert("mode".to_string(), Value::String(mode.to_string()));
    let text = xorgate_core::js::map_to_json_string(&doc);
    xorgate_hal::atomic_write::atomic_write_bytes_traced(&path, text.as_bytes())
        .map_err(|e| e.node_message())
}

/// crossterm key → ink's `useInput` view.
fn map_key(k: crossterm::event::KeyEvent) -> Option<Key> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if k.kind != KeyEventKind::Press {
        return None;
    }
    Some(match k.code {
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Key::CtrlC,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        _ => return None,
    })
}

/// SIGTERM/SIGINT/SIGHUP, registered BEFORE the terminal goes raw so a
/// signal in the init window is not lost (review-wp5 N1).
type Signals = (
    tokio::signal::unix::Signal,
    tokio::signal::unix::Signal,
    tokio::signal::unix::Signal,
);

fn register_signals() -> Option<Signals> {
    use tokio::signal::unix::{signal, SignalKind};
    Some((
        signal(SignalKind::terminate()).ok()?,
        signal(SignalKind::interrupt()).ok()?,
        signal(SignalKind::hangup()).ok()?,
    ))
}

/// Forward crossterm events + the registered termination signals into the
/// loop's channel.
fn spawn_inputs(tx: mpsc::UnboundedSender<Input>, signals: Option<Signals>) {
    let ev_tx = tx.clone();
    tokio::spawn(async move {
        use crossterm::event::{Event, EventStream};
        use futures_core::Stream;
        let mut events = EventStream::new();
        loop {
            let next =
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut events).poll_next(cx)).await;
            let inp = match next {
                None | Some(Err(_)) => Input::Quit,
                Some(Ok(Event::Key(k))) => match map_key(k) {
                    Some(k) => Input::Key(k),
                    None => continue,
                },
                Some(Ok(Event::Resize(..))) => Input::Resize,
                Some(Ok(_)) => continue,
            };
            let quit = inp == Input::Quit;
            if ev_tx.send(inp).is_err() || quit {
                return;
            }
        }
    });
    let Some((mut term, mut int, mut hup)) = signals else {
        return;
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = hup.recv() => {}
        }
        let _ = tx.send(Input::Quit);
    });
}

/// `manageCommand(rest)` (`commands/manage.tsx:5-9`) → exit code. Agent
/// flags are accepted and ignored (they only fed the TS daemon controls).
pub fn main(_args: &[String]) -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let host = host_info();
    let res = rt.block_on(async move {
        let signals = register_signals();
        let mut term = ratatui::try_init().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::unbounded_channel();
        spawn_inputs(tx, signals);
        let out = run_loop(Arc::new(SystemIo::new()), rx, host, |app, host, spin| {
            draw_to(&mut term, app, host, spin)
        })
        .await;
        ratatui::restore();
        out
    });
    rt.shutdown_timeout(Duration::from_millis(200));
    match res {
        Ok(()) => 0,
        Err(e) => {
            ratatui::restore();
            eprintln!("{e}");
            1
        }
    }
}
