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

#[derive(Deserialize, Debug, PartialEq)]
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
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-cli-set-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn set_command_fixture() {
    let path = format!(
        "{}/../../fixtures/cli_set/set-command.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let fx: Fixture = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(fx.cases.len() >= 40);
    let mut failures = Vec::new();
    for (i, c) in fx.cases.iter().enumerate() {
        let home = scratch(&format!("fx{i}"));
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

/// `set wifi` is Phase 4 (WP6 brief): the key is known, so not the unknown-key text.
#[test]
fn wifi_arrives_later() {
    let h = scratch("wifi");
    let (code, io) = set(&h, &["wifi", "off", "--force"]);
    assert_eq!(code, 1);
    assert_eq!(
        io.text(Stream::Err),
        "xorgate-rs: 'set wifi' arrives in a later build"
    );
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
