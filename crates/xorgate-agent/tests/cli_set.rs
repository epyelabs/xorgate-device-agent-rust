//! `xorgate-rs set` against the REAL TS `setCommand` (`set.ts:391-671`):
//! `fixtures/cli_set/set-command.json` (tsx-dumped, 114 invocations incl. the
//! APN watch on fake time) — stdout/stderr lines in call order, exit code and
//! the exact settings.json bytes left behind — plus the ports of
//! `test/set-command.test.ts` (11 it()s).

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use xorgate_agent::cli::set::{run, SetIo};
use xorgate_core::cli_set::{Line, Stream};
use xorgate_core::settings::AgentSettings;
use xorgate_hal::network_io::{IfaceState, IfaceStatus, RadioResult, WifiPresence};
use xorgate_hal::settings_io::{load_settings, save_settings, settings_path};

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
struct Input {
    args: Vec<String>,
    settings: Option<String>,
    lte: Vec<Option<String>>,
    pidfile: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Expected {
    lines: Vec<ExpLine>,
    exit_code: i32,
    settings_after: Option<String>,
    elapsed_ms: i64,
}

/// The golden cases in which the TS `set` rewrites a settings.json that is
/// not valid JSON, and the Rust CLI refuses (D118).
const D118_REFUSED: [&str; 2] = [
    "corrupt settings.json is rewritten",
    "array settings.json is rewritten",
];

#[derive(Deserialize, Debug, PartialEq, Clone)]
struct ExpLine {
    stream: String,
    text: String,
}

/// The tsx driver's fake world: `setTimeout` advances the clock by its delay
/// and swaps the modem telemetry to the next scripted text.
struct FakeIo {
    home: PathBuf,
    now: Cell<i64>,
    lte: Vec<Option<String>>,
    idx: Cell<usize>,
    root: Option<(u32, String)>,
    lines: RefCell<Vec<Line>>,
    /// `set wifi` world (never nmcli/ping in tests): detectWifi's answer,
    /// $SSH_CONNECTION, the ip → iface map, the probe verdicts, setWifi's
    /// answer, and the setWifi calls made.
    wifi: WifiPresence,
    ssh: Option<String>,
    ip_ifaces: Vec<(String, String)>,
    statuses: Vec<IfaceStatus>,
    radio: RadioResult,
    radio_calls: RefCell<Vec<bool>>,
    /// CF52: `(sleep index, wall step ms)` — the clock jumps after that sleep.
    step: Option<(usize, i64)>,
    sleeps: Cell<usize>,
}

impl FakeIo {
    fn new(home: &Path, lte: Vec<Option<String>>) -> Self {
        FakeIo {
            home: home.to_path_buf(),
            now: Cell::new(1_800_000_000_000),
            lte,
            idx: Cell::new(0),
            root: None,
            lines: RefCell::new(Vec::new()),
            wifi: WifiPresence {
                present: false,
                enabled: false,
                device: None,
            },
            ssh: None,
            ip_ifaces: Vec::new(),
            statuses: Vec::new(),
            radio: RadioResult {
                ok: true,
                message: String::new(),
            },
            radio_calls: RefCell::new(Vec::new()),
            step: None,
            sleeps: Cell::new(0),
        }
    }
    fn text(&self, stream: Stream) -> String {
        self.lines
            .borrow()
            .iter()
            .filter(|l| l.stream == stream)
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl SetIo for FakeIo {
    fn home(&self) -> PathBuf {
        self.home.clone()
    }
    fn now_ms(&self) -> i64 {
        self.now.get()
    }
    fn sleep_ms(&self, ms: i64) {
        self.now.set(self.now.get() + ms);
        self.idx.set(self.idx.get() + 1);
        let n = self.sleeps.get();
        if let Some((at, delta)) = self.step {
            if at == n {
                self.now.set(self.now.get() + delta);
            }
        }
        self.sleeps.set(n + 1);
    }
    fn read_modem(&self) -> Option<Value> {
        let i = self.idx.get().min(self.lte.len().saturating_sub(1));
        let text = self.lte.get(i).cloned().flatten()?;
        serde_json::from_str(&text).ok()
    }
    fn sudo_facts(&self) -> (bool, Option<u32>, Option<String>) {
        match &self.root {
            Some((uid, name)) => (true, Some(*uid), Some(name.clone())),
            None => (false, None, None),
        }
    }
    fn daemon_running(&self) -> bool {
        matches!(
            xorgate_agent::cli::status::inspect(
                &self.home,
                self.now.get() as f64,
                &xorgate_agent::cli::process_inspect::SystemRunner
            ),
            xorgate_agent::cli::status::DaemonStatus::Running { .. }
        )
    }
    fn emit(&self, line: &Line) {
        self.lines.borrow_mut().push(line.clone());
    }
    fn detect_wifi(&self) -> WifiPresence {
        self.wifi.clone()
    }
    fn ssh_connection(&self) -> Option<String> {
        self.ssh.clone()
    }
    fn iface_for_ip(&self, ip: Option<&str>) -> Option<String> {
        let ip = ip?;
        self.ip_ifaces
            .iter()
            .find(|(i, _)| i == ip)
            .map(|(_, n)| n.clone())
    }
    fn interface_statuses(&self) -> Vec<IfaceStatus> {
        self.statuses.clone()
    }
    fn set_wifi(&self, on: bool) -> RadioResult {
        self.radio_calls.borrow_mut().push(on);
        self.radio.clone()
    }
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-cli-set-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn set_command_fixture() {
    run_fixture("set-command.json", 40);
}

/// `fixtures/cli_set/set-obd.json` — `xorgate set obd.*` + the 09da38d
/// usage / unknown-key strings (Phase 5b WP1, `dump-p5b-spine-cli-set.mts`).
#[test]
fn set_obd_fixture() {
    run_fixture("set-obd.json", 40);
}

fn run_fixture(file: &str, min_cases: usize) {
    let path = format!(
        "{}/../../fixtures/cli_set/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let fx: Fixture = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(fx.cases.len() >= min_cases);
    let mut failures = Vec::new();
    let mut d118 = 0;
    for (i, c) in fx.cases.iter().enumerate() {
        let home = scratch(&format!("fx-{file}-{i}"));
        if let Some(s) = &c.input.settings {
            std::fs::write(settings_path(&home), s).unwrap();
        }
        if let Some(p) = &c.input.pidfile {
            std::fs::write(home.join("agent.pid"), p).unwrap();
        }
        let io = FakeIo::new(&home, c.input.lte.clone());
        let start = io.now.get();
        let code = run(&c.input.args, &io);
        let got: Vec<ExpLine> = io
            .lines
            .borrow()
            .iter()
            .map(|l| ExpLine {
                stream: match l.stream {
                    Stream::Out => "out".into(),
                    Stream::Err => "err".into(),
                },
                text: l.text.clone(),
            })
            .collect();
        let after = std::fs::read_to_string(settings_path(&home)).ok();
        // D118 (RUST-ONLY): a settings.json that exists but is not valid
        // JSON is never rewritten by `set`. The TS rewrites it from `{}`
        // (these two golden cases); the Rust CLI refuses, exits 1 and
        // leaves the file as it was.
        if D118_REFUSED.contains(&c.name.as_str()) {
            d118 += 1;
            assert_eq!(c.expected.exit_code, 0, "{}: the TS wrote", c.name);
            assert_ne!(c.expected.settings_after, c.input.settings, "{}", c.name);
            let refusal = format!(
                "settings.json is not a valid JSON object — refusing to overwrite it (fix or remove {}); nothing was changed",
                settings_path(&home).display()
            );
            let last = got.last().cloned();
            if code != 1
                || last != Some(ExpLine { stream: "err".into(), text: refusal })
                || after != c.input.settings
            {
                failures.push(format!("{}: D118 refusal: exit {code}, {got:?}, {after:?}", c.name));
            }
            let _ = std::fs::remove_dir_all(&home);
            continue;
        }
        if got != c.expected.lines {
            failures.push(format!(
                "{}: lines\n  got  {got:?}\n  want {:?}",
                c.name, c.expected.lines
            ));
        }
        if code != c.expected.exit_code {
            failures.push(format!(
                "{}: exit {code} want {}",
                c.name, c.expected.exit_code
            ));
        }
        if after != c.expected.settings_after {
            failures.push(format!(
                "{}: settings.json\n  got  {after:?}\n  want {:?}",
                c.name, c.expected.settings_after
            ));
        }
        if io.now.get() - start != c.expected.elapsed_ms {
            failures.push(format!(
                "{}: elapsed {} want {}",
                c.name,
                io.now.get() - start,
                c.expected.elapsed_ms
            ));
        }
        let _ = std::fs::remove_dir_all(&home);
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
    if file == "set-command.json" {
        assert_eq!(d118, D118_REFUSED.len(), "D118: both golden cases met");
    }
}

/// D118 (RUST-ONLY): a settings.json that EXISTS but cannot be read or
/// parsed is never treated as empty by a writer. `set` (any key, `apn`
/// included) refuses, exits 1 and leaves the file untouched; an ABSENT
/// file is still a fresh start. The TS writes `{...loadSettings(), key}`,
/// which is that one key alone: every other namespace is lost, and the
/// watcher then reports them to the cloud as removed.
#[test]
fn set_refuses_a_settings_file_it_cannot_read_or_parse() {
    // Not valid JSON, and JSON that is not an object.
    for bad in ["{not json", "[1,2]", "null", "", "7"] {
        for args in [
            vec!["timeSync.maxStepsPerBoot", "3"],
            vec!["cameraSensors.cam0.sensor", "imx708"],
            vec!["mode", "real", "--no-restart"],
            vec!["apn", "new.apn", "--no-wait"],
        ] {
            let home = scratch("d118-invalid");
            std::fs::write(settings_path(&home), bad).unwrap();
            let io = FakeIo::new(&home, vec![None]);
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let code = run(&args, &io);
            assert_eq!(code, 1, "{bad:?} {args:?}");
            let lines = io.lines.borrow();
            let last = lines.last().expect("a refusal line");
            assert_eq!(last.stream, Stream::Err, "{bad:?} {args:?}");
            assert_eq!(
                last.text,
                format!(
                    "settings.json is not a valid JSON object — refusing to overwrite it (fix or remove {}); nothing was changed",
                    settings_path(&home).display()
                ),
                "{bad:?} {args:?}"
            );
            assert_eq!(
                std::fs::read_to_string(settings_path(&home)).unwrap(),
                bad,
                "untouched"
            );
            let _ = std::fs::remove_dir_all(&home);
        }
    }
    // Exists but cannot be read (a directory in its place reads as an
    // error for any user, root included).
    let home = scratch("d118-unreadable");
    std::fs::create_dir(settings_path(&home)).unwrap();
    let io = FakeIo::new(&home, vec![None]);
    let code = run(&["timeSync.maxStepsPerBoot".to_string(), "3".to_string()], &io);
    assert_eq!(code, 1);
    assert_eq!(
        io.lines.borrow().last().unwrap().text,
        format!(
            "settings.json exists but cannot be read — refusing to overwrite it (check owner/permissions on {}); nothing was changed",
            settings_path(&home).display()
        )
    );
    assert!(settings_path(&home).is_dir(), "untouched");
    let _ = std::fs::remove_dir_all(&home);
    // ABSENT: a fresh start, written as before.
    let home = scratch("d118-absent");
    let io = FakeIo::new(&home, vec![None]);
    let code = run(&["timeSync.maxStepsPerBoot".to_string(), "3".to_string()], &io);
    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(settings_path(&home)).unwrap(),
        r#"{"timeSync":{"maxStepsPerBoot":3}}"#
    );
    let _ = std::fs::remove_dir_all(&home);
    // The TUI's mode switch: the same rule.
    use xorgate_agent::cli::manage::save_mode_in;
    let home = scratch("d118-tui");
    std::fs::write(settings_path(&home), "{not json").unwrap();
    assert_eq!(
        save_mode_in(&home, "real"),
        Err(format!(
            "settings.json is not a valid JSON object — refusing to overwrite it (fix or remove {}); nothing was changed",
            settings_path(&home).display()
        ))
    );
    assert_eq!(std::fs::read_to_string(settings_path(&home)).unwrap(), "{not json");
    std::fs::remove_file(settings_path(&home)).unwrap();
    std::fs::create_dir(settings_path(&home)).unwrap();
    assert!(save_mode_in(&home, "real")
        .unwrap_err()
        .starts_with("settings.json exists but cannot be read — refusing to overwrite it"));
    std::fs::remove_dir(settings_path(&home)).unwrap();
    assert_eq!(save_mode_in(&home, "real"), Ok(()));
    assert_eq!(
        std::fs::read_to_string(settings_path(&home)).unwrap(),
        r#"{"mode":"real"}"#
    );
    std::fs::write(settings_path(&home), r#"{"gnssAntBias":true,"mode":"sim"}"#).unwrap();
    assert_eq!(save_mode_in(&home, "real"), Ok(()));
    assert_eq!(
        std::fs::read_to_string(settings_path(&home)).unwrap(),
        r#"{"mode":"real","gnssAntBias":true}"#
    );
    let _ = std::fs::remove_dir_all(&home);
}

// --- test/set-command.test.ts (11 it()s) -------------------------------------

fn set(home: &Path, args: &[&str]) -> (i32, FakeIo) {
    set_as(home, args, None)
}

fn set_as(home: &Path, args: &[&str], root: Option<(u32, &str)>) -> (i32, FakeIo) {
    let mut io = FakeIo::new(home, vec![None]);
    io.root = root.map(|(u, n)| (u, n.to_string()));
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    (run(&args, &io), io)
}

fn loaded(home: &Path) -> AgentSettings {
    load_settings(&settings_path(home))
}

fn ps(home: &Path) -> Value {
    serde_json::to_value(loaded(home).recording.unwrap().power_shedding.unwrap()).unwrap()
}

/// :47 accepts the thermal trio (cloud-settable but CLI-invisible until Phase 4)
#[test]
fn accepts_the_thermal_trio() {
    let h = scratch("trio");
    for (f, v) in [
        ("cpuTempC", "82"),
        ("cpuTempRestoreC", "73"),
        ("cpuTempSecs", "20"),
    ] {
        let (code, _) = set(&h, &[&format!("recording.powerShedding.{f}"), v]);
        assert_eq!(code, 0);
    }
    assert_eq!(
        ps(&h),
        serde_json::json!({"cpuTempSecs": 20, "cpuTempC": 82, "cpuTempRestoreC": 73})
    );
}

/// :63 clamps through the same validator loadSettings uses
#[test]
fn clamps_through_the_same_validator() {
    let h = scratch("clamp");
    set(&h, &["recording.powerShedding.cpuTempC", "999"]);
    assert_eq!(ps(&h)["cpuTempC"], 90);
}

/// :68 merges rather than replacing the namespace
#[test]
fn merges_rather_than_replacing_the_namespace() {
    let h = scratch("merge");
    let seed: AgentSettings = serde_json::from_value(
        serde_json::json!({"recording": {"powerShedding": {"enabled": true}}}),
    )
    .unwrap();
    save_settings(&settings_path(&h), &seed).unwrap();
    set(&h, &["recording.powerShedding.cpuTempSecs", "15"]);
    assert_eq!(
        ps(&h),
        serde_json::json!({"enabled": true, "cpuTempSecs": 15})
    );
}

/// :81 warns that the agent runs as somebody else, and still performs the write
#[test]
fn sudo_warns_and_still_writes() {
    let h = scratch("sudo");
    let (code, io) = set_as(&h, &["gps-antenna", "passive"], Some((1000, "pi")));
    assert_eq!(code, 0);
    assert_eq!(loaded(&h).gnss_ant_bias, Some(false));
    let err = io.text(Stream::Err);
    assert!(
        err.contains("agent runs as 'pi'") && err.contains("don't need sudo"),
        "{err}"
    );
}

/// :91 says nothing when the agent runs as root too
#[test]
fn sudo_silent_when_agent_is_root() {
    let h = scratch("sudoroot");
    let (_, io) = set_as(&h, &["timeSync.offlineSources", "off"], Some((0, "root")));
    assert_eq!(loaded(&h).time_sync.unwrap().offline_sources, Some(false));
    assert!(!io.text(Stream::Err).contains("sudo"));
}

/// :100 says nothing for an ordinary unprivileged run
#[test]
fn sudo_silent_when_unprivileged() {
    let h = scratch("nosudo");
    let (_, io) = set(&h, &["recording.telemetry.rateHz", "5"]);
    assert_eq!(
        loaded(&h).recording.unwrap().telemetry.unwrap().rate_hz,
        Some(5)
    );
    assert!(!io.text(Stream::Err).contains("sudo"));
}

/// :109 the retired `media` setting fails with a pointer to per-camera enables
#[test]
fn retired_media_fails_without_writing() {
    let h = scratch("media");
    let (code, io) = set(&h, &["media", "off"]);
    assert_eq!(code, 1);
    assert_eq!(loaded(&h), AgentSettings::default());
    assert!(io.text(Stream::Err).contains("recording.video.cameras"));
}

fn mount(h: &Path) -> Value {
    serde_json::to_value(loaded(h).camera_mount).unwrap()
}

/// :122 persists a rotation and merges into the camera map
#[test]
fn camera_mount_persists_and_merges() {
    let h = scratch("mount");
    set(&h, &["cameraMount.cam0.rotationDeg", "180"]);
    assert_eq!(
        mount(&h),
        serde_json::json!({"cameras": {"cam0": {"rotationDeg": 180}}})
    );
    let (code, _) = set(&h, &["cameraMount.cam1.rotationDeg", "0"]);
    assert_eq!(
        mount(&h),
        serde_json::json!({"cameras": {"cam0": {"rotationDeg": 180}, "cam1": {"rotationDeg": 0}}})
    );
    assert_eq!(code, 0);
}

/// :133 says out loud that a REBOOT is needed
#[test]
fn camera_mount_says_reboot() {
    let h = scratch("reboot");
    let (_, io) = set(&h, &["cameraMount.cam0.rotationDeg", "180"]);
    let out = io.text(Stream::Out);
    assert!(out.contains("REBOOT is required"));
    assert!(out.contains("never reboots itself"));
}

/// :139 takes only 0 and 180, and only a real stream key
#[test]
fn camera_mount_rejects_bad_input() {
    let h = scratch("mountbad");
    for args in [
        &["cameraMount.cam0.rotationDeg", "90"][..],
        &["cameraMount.cam0.rotationDeg", "true"],
        &["cameraMount.cam0.rotationDeg"],
        &["cameraMount.cam 0.rotationDeg", "180"],
        &["cameraMount.cam0.rotation", "180"],
        &["cameraMount.cam0", "180"],
    ] {
        let (code, _) = set(&h, args);
        assert_eq!(code, 1, "{args:?}");
        assert_eq!(loaded(&h).camera_mount, None, "{args:?}");
    }
}

/// :155 leaves the other namespaces alone
#[test]
fn camera_mount_leaves_other_namespaces_alone() {
    let h = scratch("others");
    let seed: AgentSettings = serde_json::from_value(
        serde_json::json!({"gnssAntBias": true, "recording": {"telemetry": {"rateHz": 5}}}),
    )
    .unwrap();
    save_settings(&settings_path(&h), &seed).unwrap();
    set(&h, &["cameraMount.cam0.rotationDeg", "180"]);
    let s = loaded(&h);
    assert_eq!(s.gnss_ant_bias, Some(true));
    assert_eq!(s.recording.unwrap().telemetry.unwrap().rate_hz, Some(5));
    assert_eq!(mount(&h)["cameras"]["cam0"]["rotationDeg"], 180);
}

// --- beyond the TS suite ------------------------------------------------------

/// CF52: the APN watch windows stay on the wall clock (TS parity); a step
/// in either direction neither crashes nor wedges the watch. No modem file at
/// all → the 90 s old-daemon cutoff decides (`set.ts:625-631`).
#[test]
fn apn_watch_survives_a_clock_step() {
    const OLD_DAEMON: &str = "✗ the modem daemon is not reporting any APN machinery — it likely predates this feature and will NEVER apply the change. Update the agent (xorgate update).";
    for (delta, sleeps) in [
        // forward 10 min after the first sleep: elapsed 603 s > 90 s at once.
        (600_000_i64, 1_usize),
        // backward 1 h: elapsed runs from -3597 s up past +90 s in 3 s polls.
        (-3_600_000, 1 + (3_597_000 + 90_000) / 3000 + 1),
        // no step: the cutoff after 31 polls (93 s).
        (0, 31),
    ] {
        let h = scratch(&format!("apn-step-{delta}"));
        let mut io = FakeIo::new(&h, vec![None]);
        io.step = Some((0, delta));
        let args: Vec<String> = ["apn", "internet.example"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let code = run(&args, &io);
        assert_eq!(code, 1, "step {delta}");
        assert_eq!(io.lines.borrow().last().unwrap().text, OLD_DAEMON);
        assert_eq!(io.sleeps.get(), sleeps, "step {delta}");
    }
}

// --- `set wifi` (set.ts:679-717, Phase 4 WP5, CF51) ---------------------------

fn wlan(enabled: bool) -> WifiPresence {
    WifiPresence {
        present: true,
        enabled,
        device: Some("wlan0".into()),
    }
}

fn st(name: &str, state: IfaceState) -> IfaceStatus {
    IfaceStatus {
        name: name.into(),
        state,
    }
}

fn wifi_run(h: &Path, args: &[&str], cfg: impl FnOnce(&mut FakeIo)) -> (i32, FakeIo) {
    let mut io = FakeIo::new(h, vec![None]);
    cfg(&mut io);
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    (run(&args, &io), io)
}

/// A bad value fails before any probe, without the sudo note or a write.
#[test]
fn wifi_rejects_a_bad_value() {
    let h = scratch("wifi-bad");
    let (code, io) = wifi_run(&h, &["wifi"], |io| io.root = Some((1000, "pi".into())));
    assert_eq!(code, 1);
    assert_eq!(io.text(Stream::Err), "wifi must be 'on' or 'off', got ''");
    let (code, io) = wifi_run(&h, &["wifi", "maybe"], |_| {});
    assert_eq!(code, 1);
    assert_eq!(
        io.text(Stream::Err),
        "wifi must be 'on' or 'off', got 'maybe'"
    );
    assert!(io.radio_calls.borrow().is_empty());
    assert!(!settings_path(&h).exists());
}

#[test]
fn wifi_without_an_adapter() {
    let h = scratch("wifi-none");
    let (code, io) = wifi_run(&h, &["wifi", "on"], |_| {});
    assert_eq!(code, 1);
    assert_eq!(
        io.text(Stream::Err),
        "wifi: no WiFi adapter detected on this device"
    );
    assert!(io.radio_calls.borrow().is_empty());
}

/// `set wifi off` over a WiFi SSH session REFUSES (the bench smoke, E15).
#[test]
fn wifi_off_refuses_over_its_own_ssh_session() {
    let h = scratch("wifi-ssh");
    let (code, io) = wifi_run(&h, &["wifi", "off"], |io| {
        io.wifi = wlan(true);
        io.ssh = Some("192.168.8.10 51234 192.168.8.203 22".into());
        io.ip_ifaces = vec![("192.168.8.203".into(), "wlan0".into())];
        io.statuses = vec![
            st("eth0", IfaceState::Online),
            st("wlan0", IfaceState::Online),
        ];
    });
    assert_eq!(code, 1);
    let err: Vec<String> = io.lines.borrow().iter().map(|l| l.text.clone()).collect();
    assert_eq!(
        err,
        [
            "refusing to turn WiFi off: your SSH session is over wlan0.",
            "re-run with --force if you're sure (you may lose connectivity)."
        ]
    );
    assert!(io.radio_calls.borrow().is_empty());
}

#[test]
fn wifi_off_refuses_when_it_is_the_only_uplink() {
    let h = scratch("wifi-uplink");
    let (code, io) = wifi_run(&h, &["wifi", "off"], |io| {
        io.wifi = wlan(true);
        io.ssh = Some("10.0.0.2 5 10.0.0.9 22".into()); // over eth0
        io.ip_ifaces = vec![("10.0.0.9".into(), "eth0".into())];
        io.statuses = vec![
            st("eth0", IfaceState::Offline),
            st("usb0", IfaceState::Down),
            st("wlan0", IfaceState::Online),
        ];
    });
    assert_eq!(code, 1);
    assert_eq!(
        io.lines.borrow()[0].text,
        "refusing to turn WiFi off: wlan0 is currently your only internet connection."
    );
}

/// `--force` skips the guard; `on` never runs it; the success line is `✓ <message>`.
#[test]
fn wifi_force_and_on_skip_the_guard() {
    let h = scratch("wifi-force");
    let guard = |io: &mut FakeIo| {
        io.wifi = wlan(true);
        io.ssh = Some("1 2 192.168.8.203 22".into());
        io.ip_ifaces = vec![("192.168.8.203".into(), "wlan0".into())];
        io.radio = RadioResult {
            ok: true,
            message: "wifi off (wlan0)".into(),
        };
    };
    let (code, io) = wifi_run(&h, &["wifi", "off", "--force"], guard);
    assert_eq!(code, 0);
    assert_eq!(io.text(Stream::Out), "✓ wifi off (wlan0)");
    assert_eq!(*io.radio_calls.borrow(), [false]);
    let (code, io) = wifi_run(&h, &["wifi", "on"], |io| {
        io.wifi = wlan(false);
        io.radio = RadioResult {
            ok: false,
            message: "wifi on failed: exit null".into(),
        };
    });
    assert_eq!(code, 1);
    assert_eq!(io.text(Stream::Err), "wifi on failed: exit null");
    assert_eq!(*io.radio_calls.borrow(), [true]);
    assert!(!settings_path(&h).exists());
}

/// A write that fails prints Node's `err.message` (the temp-file open) and
/// exits 1 without the success lines.
#[test]
fn write_failure_is_reported_like_node() {
    let h = scratch("nowrite").join("missing-dir");
    let (code, io) = set(&h, &["gps-antenna", "active"]);
    assert_eq!(code, 1);
    let err = io.text(Stream::Err);
    assert!(
        err.starts_with("ENOENT: no such file or directory, open '")
            && err.contains("/missing-dir/.settings.json.tmp."),
        "{err}"
    );
    assert!(io.text(Stream::Out).is_empty());
}

/// `mode` while an agent is running: the Rust agent cannot restart its unit.
/// The record names this test binary, so `ps` verifies it (or, without `ps`,
/// the identity is `unverified` — still `running`).
#[test]
fn mode_with_a_running_agent_points_at_systemctl() {
    let h = scratch("moderun");
    let exe = std::env::current_exe().unwrap();
    let worker = exe.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::write(
        h.join("agent.pid"),
        serde_json::json!({"pid": std::process::id(), "startedAt": 1_800_000_000_000i64,
            "deviceId": "d", "worker": worker})
        .to_string(),
    )
    .unwrap();
    let (code, io) = set(&h, &["mode", "real"]);
    assert_eq!(code, 1);
    assert_eq!(
        io.text(Stream::Out),
        "✓ mode set to real (persisted to settings.json)"
    );
    assert_eq!(
        io.text(Stream::Err),
        "  could not restart automatically: xorgate-rs runs the agent under systemd only   on systemd installs, run: sudo systemctl restart xorgate-agent-rs"
    );
    assert_eq!(loaded(&h).mode.map(|m| m.as_str()), Some("real"));
}
