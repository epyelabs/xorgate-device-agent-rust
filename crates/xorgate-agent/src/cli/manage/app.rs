//! The `manage` TUI state machine — the state and key handling of
//! `src/monitor/MonitorApp.tsx:42-171` (+ `useDaemonState`,
//! `useInterfaceStatus`, `useLogTail`), pure: a key press mutates [`App`]
//! and returns the side effects the runtime ([`super::run`]) performs.
//!
//! Q10 (D-row): no process control from the TUI. The TS keys `s` (start,
//! not running), `x` (stop, running) and `r` (restart) drove the
//! self-managed daemon through `useDaemonControls`; here they keep their key
//! and hint and print the systemd command instead (`run: sudo systemctl
//! <verb> xorgate-agent-rs`), and the Settings run-mode apply saves
//! settings.json (the same writer as `xorgate-rs set`) then prints the
//! restart command rather than restarting.

use serde_json::Value;
use xorgate_core::imu::ImuDetect;
use xorgate_hal::network_io::{IfaceState, IfaceStatus, WifiPresence};

use super::modules::{ModemPresence, ModuleId, SettingsView, MODULES};
use crate::cli::status::{js_string, DaemonStatus};

/// `LIST_WIDTH` (`MonitorApp.tsx:33`).
pub const LIST_WIDTH: u16 = 22;
/// `NARROW_THRESHOLD` (`MonitorApp.tsx:34`).
pub const NARROW_THRESHOLD: u16 = 80;
/// `useDaemonState(1000)` (`MonitorApp.tsx:55`).
pub const DAEMON_POLL_MS: u64 = 1000;
/// `useInterfaceStatus(pollMs = 10000)` (`useInterfaceStatus.ts:9`).
pub const IFACE_POLL_MS: u64 = 10_000;
/// `useLogTail(active, maxLines = 12, pollMs = 1000)` (`useLogTail.ts:5`).
pub const LOG_POLL_MS: u64 = 1000;
pub const LOG_MAX_LINES: usize = 12;
/// `LteDetail`'s link poll (`modules.tsx:248`).
pub const LINK_POLL_MS: u64 = 2000;
/// `WIFI_ROW` (`MonitorApp.tsx:81`).
pub const WIFI_ROW: usize = 1;
/// The systemd unit the Q10 hints name.
pub const UNIT: &str = "xorgate-agent-rs";

/// One key press, as ink's `useInput` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    /// Ctrl+C (`render(…, { exitOnCtrlC: true })`, `manage.tsx:7`).
    CtrlC,
}

/// A side effect a key asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// `exit()` — quitting never stops the daemon.
    Exit,
    /// `saveSettings({ ...loadSettings(), mode })` (`MonitorApp.tsx:143`).
    SaveMode(String),
    /// `setWifi(on)` (`MonitorApp.tsx:114`); completion → [`App::wifi_settled`].
    SetWifi(bool),
}

/// Facts a key handler reads at press time.
pub trait KeyFacts {
    /// `ifaceForIp(sshServerIp(process.env.SSH_CONNECTION))`.
    fn ssh_iface(&self) -> Option<String>;
    /// `loadSettings().mode`.
    fn settings_mode(&self) -> Option<String>;
}

/// `MonitorApp`'s state.
#[derive(Debug, Clone)]
pub struct App {
    /// `useDaemonState`: starts `{kind: "stopped"}` until the first poll lands.
    pub status: DaemonStatus,
    pub now: f64,
    pub interfaces: Vec<IfaceStatus>,
    pub selected: usize,
    pub operating: bool,
    pub show_logs: bool,
    pub mode_choice: Option<String>,
    pub settings_row: usize,
    pub wifi: Option<WifiPresence>,
    pub wifi_busy: bool,
    pub wifi_confirm: bool,
    pub wifi_nonce: u64,
    pub log_lines: Vec<String>,
    /// `controls.lastMsg` — here the Q10 command hints and save results.
    pub last_msg: Option<String>,
    /// `ImuDetail` / `LteDetail` pane-local probe results (reset on unmount).
    pub imu_detect: Option<ImuDetect>,
    pub modem: Option<ModemPresence>,
    pub link: Option<Value>,
    /// Terminal width (`useTerminalSize`; 80 without a TTY).
    pub columns: u16,
}

impl Default for App {
    fn default() -> Self {
        App {
            status: DaemonStatus::Stopped,
            now: 0.0,
            interfaces: Vec::new(),
            selected: 0,
            operating: false,
            show_logs: false,
            mode_choice: None,
            settings_row: 0,
            wifi: None,
            wifi_busy: false,
            wifi_confirm: false,
            wifi_nonce: 0,
            log_lines: Vec::new(),
            last_msg: None,
            imu_detect: None,
            modem: None,
            link: None,
            columns: 80,
        }
    }
}

/// One footer hint (`components/KeyHints.tsx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub keys: &'static str,
    pub label: &'static str,
}

const fn h(keys: &'static str, label: &'static str) -> Hint {
    Hint { keys, label }
}

impl App {
    pub fn running(&self) -> bool {
        matches!(self.status, DaemonStatus::Running { .. })
    }

    /// `running ? status.state : null`.
    pub fn snapshot(&self) -> Option<&Value> {
        match &self.status {
            DaemonStatus::Running { state, .. } => state.as_ref(),
            _ => None,
        }
    }

    pub fn module(&self) -> ModuleId {
        MODULES[self.selected]
    }

    pub fn on_settings(&self) -> bool {
        self.module() == ModuleId::Settings
    }

    /// `snapshot?.mode` (`??` falls through only on null/undefined).
    fn snapshot_mode(&self) -> Option<String> {
        match self.snapshot().and_then(|s| s.get("mode")) {
            None | Some(Value::Null) => None,
            v => Some(js_string(v)),
        }
    }

    /// `snapshot?.mode === "real"` — gates the LTE/IMU pane probes.
    pub fn real(&self) -> bool {
        self.snapshot()
            .and_then(|s| s.get("mode"))
            .and_then(Value::as_str)
            == Some("real")
    }

    /// `settingsRowCount` (`MonitorApp.tsx:79`).
    pub fn settings_row_count(&self) -> usize {
        if self.wifi.as_ref().is_some_and(|w| w.present) {
            2
        } else {
            1
        }
    }

    pub fn settings_view(&self) -> SettingsView {
        SettingsView {
            mode_choice: self.mode_choice.clone(),
            row: self.settings_row,
            operating: self.operating,
            wifi: self.wifi.clone(),
            wifi_busy: self.wifi_busy,
            wifi_confirm: self.wifi_confirm,
        }
    }

    /// `toggleWifi` (`MonitorApp.tsx:99-118`): a two-press confirm when
    /// turning WiFi off would cut this SSH session or the only uplink.
    fn toggle_wifi(&mut self, facts: &dyn KeyFacts) -> Vec<Effect> {
        let Some(wifi) = self.wifi.clone().filter(|w| w.present) else {
            return Vec::new();
        };
        if self.wifi_busy {
            return Vec::new();
        }
        let device = wifi.device.clone().unwrap_or_else(|| "wlan0".to_string());
        if wifi.enabled && !self.wifi_confirm {
            let over_ssh = facts.ssh_iface().as_deref() == Some(device.as_str());
            let online: Vec<&IfaceStatus> = self
                .interfaces
                .iter()
                .filter(|i| i.state == IfaceState::Online)
                .collect();
            let only_uplink = online.len() == 1 && online[0].name == device;
            if over_ssh || only_uplink {
                self.wifi_confirm = true;
                return Vec::new();
            }
        }
        self.wifi_confirm = false;
        self.wifi_busy = true;
        vec![Effect::SetWifi(!wifi.enabled)]
    }

    /// `setWifi(...).finally(...)` (`MonitorApp.tsx:114-117`): the result
    /// message is not shown (TS parity); re-probe via the nonce.
    pub fn wifi_settled(&mut self) {
        self.wifi_busy = false;
        self.wifi_nonce += 1;
    }

    fn hint_msg(&mut self, verb: &str) {
        self.last_msg = Some(format!("run: sudo systemctl {verb} {UNIT}"));
    }

    /// `useInput((input, key) => …)` (`MonitorApp.tsx:120-171`).
    pub fn key(&mut self, k: Key, facts: &dyn KeyFacts) -> Vec<Effect> {
        let ch = |c: char| k == Key::Char(c);
        if k == Key::CtrlC {
            return vec![Effect::Exit];
        }
        if self.operating {
            if ch('q') || ch('Q') || k == Key::Esc || k == Key::Left {
                self.operating = false;
                self.wifi_confirm = false;
                return Vec::new();
            }
            if self.on_settings() {
                let n = self.settings_row_count();
                if k == Key::Up || ch('k') {
                    self.wifi_confirm = false;
                    self.settings_row = (self.settings_row + n - 1) % n;
                } else if k == Key::Down || ch('j') {
                    self.wifi_confirm = false;
                    self.settings_row = (self.settings_row + 1) % n;
                } else if self.settings_row == WIFI_ROW {
                    if ch(' ') || k == Key::Enter || ch('a') || ch('A') {
                        return self.toggle_wifi(facts);
                    }
                } else if ch(' ') {
                    let cur = self
                        .mode_choice
                        .clone()
                        .or_else(|| self.snapshot_mode())
                        .unwrap_or_else(|| "sim".to_string());
                    self.mode_choice = Some(if cur == "sim" { "real" } else { "sim" }.to_string());
                } else if k == Key::Enter || ch('a') || ch('A') {
                    let mode = self
                        .mode_choice
                        .clone()
                        .or_else(|| self.snapshot_mode())
                        .unwrap_or_else(|| "sim".to_string());
                    self.operating = false;
                    return vec![Effect::SaveMode(mode)];
                }
            }
            return Vec::new();
        }
        let n = MODULES.len();
        if k == Key::Up || ch('k') {
            self.selected = (self.selected + n - 1) % n;
        } else if k == Key::Down || ch('j') {
            self.selected = (self.selected + 1) % n;
        } else if (k == Key::Enter || k == Key::Right) && self.running() {
            if self.on_settings() {
                self.mode_choice = Some(
                    self.snapshot_mode()
                        .or_else(|| facts.settings_mode())
                        .unwrap_or_else(|| "sim".to_string()),
                );
                self.settings_row = 0;
            }
            self.operating = true;
        } else if (ch('s') || ch('S')) && !self.running() {
            self.hint_msg("start");
        } else if (ch('x') || ch('X')) && self.running() {
            self.hint_msg("stop");
        } else if ch('r') || ch('R') {
            self.hint_msg("restart");
        } else if ch('l') || ch('L') {
            self.show_logs = !self.show_logs;
        } else if ch('q') || ch('Q') {
            return vec![Effect::Exit];
        }
        Vec::new()
    }

    /// The footer hints (`MonitorApp.tsx:218-246`). The TS tests
    /// `settingsRow === 2`, which never holds (the wifi row is 1), so the
    /// operating-Settings bar always shows the run-mode hints (parity).
    pub fn hints(&self) -> Vec<Hint> {
        if !self.running() {
            return vec![h("s", "start"), h("r", "restart"), h("q", "quit")];
        }
        if self.operating {
            if self.on_settings() {
                return vec![
                    h("↑↓", "field"),
                    h("space", "toggle"),
                    h("↵", "save (restart to apply)"),
                    h("←/q", "back"),
                ];
            }
            return vec![h("←/q", "back")];
        }
        vec![
            h("↑↓", "select"),
            h("↵", "open"),
            h("x", "stop"),
            h("r", "restart"),
            h("l", if self.show_logs { "hide log" } else { "log" }),
            h("q", "quit (daemon keeps running)"),
        ]
    }

    /// The LTE pane is mounted and probing (`LteDetail` with `real`).
    pub fn lte_probing(&self) -> bool {
        self.running() && self.module() == ModuleId::Lte && self.real()
    }

    /// The IMU pane is mounted and probing (`ImuDetail` with `real`).
    pub fn imu_probing(&self) -> bool {
        self.running() && self.module() == ModuleId::Imu && self.real()
    }

    /// The Settings wifi probe key (`useEffect(…, [onSettings, wifiNonce])`).
    pub fn wifi_probe_key(&self) -> Option<u64> {
        self.on_settings().then_some(self.wifi_nonce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct F;
    impl KeyFacts for F {
        fn ssh_iface(&self) -> Option<String> {
            Some("wlan0".into())
        }
        fn settings_mode(&self) -> Option<String> {
            None
        }
    }

    fn running_app() -> App {
        let record = crate::pidfile::parse_pid_record(
            r#"{"pid":4242,"startedAt":1,"deviceId":"d","mode":"sim","rateHz":1,"offline":true,"worker":"w","argv":[],"nodeVersion":"v20"}"#,
        )
        .unwrap();
        App {
            status: DaemonStatus::Running {
                record,
                identity: crate::cli::process_inspect::Identity::Verified,
                state: Some(serde_json::json!({"mode": "sim"})),
                state_fresh: true,
            },
            ..App::default()
        }
    }

    #[test]
    fn wifi_toggle_two_press_confirm_over_ssh() {
        let mut a = running_app();
        a.selected = 6;
        a.wifi = Some(WifiPresence {
            present: true,
            enabled: true,
            device: Some("wlan0".into()),
        });
        assert!(a.key(Key::Enter, &F).is_empty());
        assert!(a.operating);
        a.key(Key::Char('j'), &F);
        assert_eq!(a.settings_row, WIFI_ROW);
        assert!(a.key(Key::Char(' '), &F).is_empty());
        assert!(a.wifi_confirm);
        assert_eq!(a.key(Key::Enter, &F), vec![Effect::SetWifi(false)]);
        assert!(a.wifi_busy && !a.wifi_confirm);
        assert!(a.key(Key::Enter, &F).is_empty()); // busy
        a.wifi_settled();
        assert_eq!((a.wifi_busy, a.wifi_nonce), (false, 1));
    }

    #[test]
    fn q10_keys_print_the_systemd_command() {
        let mut a = App::default();
        a.key(Key::Char('s'), &F);
        assert_eq!(
            a.last_msg.as_deref(),
            Some("run: sudo systemctl start xorgate-agent-rs")
        );
        a.key(Key::Char('x'), &F); // not running: ignored
        assert_eq!(
            a.last_msg.as_deref(),
            Some("run: sudo systemctl start xorgate-agent-rs")
        );
        let mut a = running_app();
        a.key(Key::Char('x'), &F);
        assert_eq!(
            a.last_msg.as_deref(),
            Some("run: sudo systemctl stop xorgate-agent-rs")
        );
        a.key(Key::Char('R'), &F);
        assert_eq!(
            a.last_msg.as_deref(),
            Some("run: sudo systemctl restart xorgate-agent-rs")
        );
        assert_eq!(a.key(Key::Char('q'), &F), vec![Effect::Exit]);
        assert_eq!(a.key(Key::CtrlC, &F), vec![Effect::Exit]);
    }

    #[test]
    fn mode_apply_saves_and_leaves_operating() {
        let mut a = running_app();
        a.selected = 6;
        a.key(Key::Right, &F);
        assert_eq!(a.mode_choice.as_deref(), Some("sim"));
        a.key(Key::Char(' '), &F);
        assert_eq!(a.mode_choice.as_deref(), Some("real"));
        assert_eq!(
            a.key(Key::Char('a'), &F),
            vec![Effect::SaveMode("real".into())]
        );
        assert!(!a.operating);
        // Enter on a non-running app does nothing.
        let mut b = App::default();
        b.key(Key::Enter, &F);
        assert!(!b.operating);
    }
}
