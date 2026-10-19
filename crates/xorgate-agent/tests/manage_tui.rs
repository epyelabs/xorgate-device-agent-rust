//! `xorgate-rs manage` — the 3 it()s of `test/monitor.test.tsx` (:87 :122
//! :167) as ratatui `TestBackend` buffer snapshots, driving the REAL loop
//! (`cli::manage::run_loop`) on paused tokio time with a fake [`ManageIo`]
//! (the TS tests inject `inspector` and render with ink-testing-library at
//! 80 columns). Compared on behaviour/screens, not bytes; Q10 adapts the
//! control texts (D-row): the not-running call-to-action names
//! `sudo systemctl start xorgate-agent-rs` instead of the `s` key.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use xorgate_agent::cli::manage::app::Key;
use xorgate_agent::cli::manage::modules::{ModemPresence, ModuleId, MODULES};
use xorgate_agent::cli::manage::view::HostInfo;
use xorgate_agent::cli::manage::{draw_to, run_loop, Input, ManageIo};
use xorgate_agent::cli::process_inspect::Identity;
use xorgate_agent::cli::status::DaemonStatus;
use xorgate_agent::ctx::BoxFut;
use xorgate_agent::pidfile::parse_pid_record;
use xorgate_core::imu::ImuDetect;
use xorgate_hal::network_io::{IfaceStatus, WifiPresence};

const NOW: f64 = 1_800_000_000_000.0;

/// `snapshot()` of `monitor.test.tsx:28-84`.
fn snapshot() -> Value {
    json!({
        "v": 1, "writtenBy": "test", "deviceId": "dev-abc-123", "mode": "sim", "rateHz": 1,
        "endpoint": "offline", "offline": true, "pid": 4242, "startedAt": NOW, "lastWriteAt": NOW,
        "connection": "connected", "connectionSince": NOW, "seq": 12, "published": 12,
        "publishErrors": 0, "lastPublishAt": NOW, "lastError": null,
        "latest": {
            "v": 1, "deviceId": "dev-abc-123", "ts": NOW, "seq": 12,
            "metrics": {
                "imu": {
                    "quat_r": {"value": 0.92}, "quat_i": {"value": 0.33}, "quat_j": {"value": 0.07},
                    "quat_k": {"value": 0.19}, "euler_roll": {"value": 40.1, "unit": "°"},
                    "euler_pitch": {"value": -0.3, "unit": "°"}, "euler_yaw": {"value": 23.6, "unit": "°"},
                    "accel_x": {"value": 0.01, "unit": "m/s^2"}, "accel_y": {"value": -0.02, "unit": "m/s^2"},
                    "accel_z": {"value": 0.03, "unit": "m/s^2"}
                },
                "gps": {"lat": {"value": 37.7749}, "lon": {"value": -122.4194},
                        "alt": {"value": 20, "unit": "m"}, "speed": {"value": 0, "unit": "km/h"}},
                "lte": {"rssi": {"value": -67, "unit": "dBm"}, "quality": {"value": 18, "unit": "asu"}},
                "system": {"cpu_usage": {"value": 31, "unit": "%"}, "cpu_temp": {"value": 54, "unit": "°C"},
                           "ram_usage": {"value": 47, "unit": "%"}, "disk_usage": {"value": 38, "unit": "%"}}
            },
            "media": {"kvs": {"value": false}, "recording": {"value": false}, "streams": []}
        },
        "recentEvents": [{"ts": NOW, "level": "info", "msg": "connected to offline"}]
    })
}

fn running() -> DaemonStatus {
    let record = parse_pid_record(&format!(
        r#"{{"pid":4242,"startedAt":{NOW},"deviceId":"dev-abc-123","mode":"sim","rateHz":1,"offline":true,"worker":"/x/dist/worker.js","argv":["--offline"],"nodeVersion":"v20"}}"#
    ))
    .unwrap();
    DaemonStatus::Running {
        record,
        identity: Identity::Verified,
        state: Some(snapshot()),
        state_fresh: true,
    }
}

/// The inspector seam + a host with no WiFi adapter (as the TS test host).
struct Fake {
    status: DaemonStatus,
    saved: Mutex<Vec<String>>,
}

impl ManageIo for Fake {
    fn inspect(&self) -> BoxFut<'static, (DaemonStatus, f64)> {
        let st = self.status.clone();
        Box::pin(async move { (st, NOW + 1000.0) })
    }
    fn interfaces(&self) -> BoxFut<'static, Vec<IfaceStatus>> {
        Box::pin(async { Vec::new() })
    }
    fn detect_wifi(&self) -> BoxFut<'static, WifiPresence> {
        Box::pin(async {
            WifiPresence {
                present: false,
                enabled: false,
                device: None,
            }
        })
    }
    fn set_wifi(&self, _on: bool) -> BoxFut<'static, ()> {
        panic!("no wifi toggles in these tests")
    }
    fn log_tail(&self) -> BoxFut<'static, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    fn detect_modem(&self) -> BoxFut<'static, ModemPresence> {
        Box::pin(async { ModemPresence::default() })
    }
    fn read_link(&self) -> BoxFut<'static, Option<Value>> {
        Box::pin(async { None })
    }
    fn detect_imu(&self) -> BoxFut<'static, ImuDetect> {
        Box::pin(async { xorgate_core::imu::build_detect(vec![]) })
    }
    fn save_mode(&self, mode: &str) -> Result<(), String> {
        self.saved.lock().unwrap().push(mode.to_string());
        Ok(())
    }
    fn ssh_iface(&self) -> Option<String> {
        None
    }
    fn settings_mode(&self) -> Option<String> {
        None
    }
}

fn host() -> HostInfo {
    HostInfo {
        host_label: "mock".into(),
        is_mock: true,
        os: "Debian Bookworm aarch64".into(),
        gps_port: "/dev/ttyUSB1".into(),
    }
}

/// The text of a TestBackend buffer, one string per row.
fn text(t: &Terminal<TestBackend>) -> String {
    let buf = t.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Run the real loop at 80×48; `script` sends keys with delays (paused
/// time) and inspects the last frame through `frame()`.
struct Harness {
    tx: mpsc::UnboundedSender<Input>,
    frame: Arc<Mutex<String>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

impl Harness {
    fn start(io: Arc<Fake>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let frame = Arc::new(Mutex::new(String::new()));
        let f2 = frame.clone();
        let task = tokio::spawn(async move {
            let mut term = Terminal::new(TestBackend::new(80, 48)).unwrap();
            run_loop(io, rx, host(), move |app, host, spin| {
                draw_to(&mut term, app, host, spin)?;
                *f2.lock().unwrap() = text(&term);
                Ok(())
            })
            .await
        });
        Harness { tx, frame, task }
    }
    fn key(&self, k: Key) {
        self.tx.send(Input::Key(k)).unwrap();
    }
    fn frame(&self) -> String {
        let f = self.frame.lock().unwrap().clone();
        if std::env::var_os("WP5_DUMP_FRAMES").is_some() {
            eprintln!("----- frame -----\n{f}");
        }
        f
    }
    async fn quit(self) {
        self.tx.send(Input::Key(Key::Char('q'))).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("loop exits on q")
            .unwrap()
            .unwrap();
    }
}

async fn delay(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// `monitor.test.tsx:87` — header + sidebar, then ↓↓↓ to GPS shows its detail.
#[tokio::test(start_paused = true)]
async fn renders_header_and_sidebar_and_opens_a_module_detail() {
    let h = Harness::start(Arc::new(Fake {
        status: running(),
        saved: Mutex::new(Vec::new()),
    }));
    delay(150).await; // let the first poll land
    let frame = h.frame();
    assert!(frame.contains("XORGATE Manager"), "{frame}");
    assert!(frame.contains("Connectivity"));
    assert!(frame.contains("mqtt"));
    assert!(frame.contains("pid"));
    for t in [
        "IMU", "GPS", "LTE", "System", "Media", "Activity", "Settings",
    ] {
        assert!(frame.contains(t), "sidebar lists {t}\n{frame}");
    }
    assert!(frame.contains("recent events")); // default selection = Activity
    assert!(frame.contains("stop")); // running → control hint (Q10: prints the systemctl command)
                                     // Sidebar statuses from the snapshot.
    assert!(frame.contains("● 1 ev"));
    assert!(frame.contains("● cpu 31%"));
    assert!(frame.contains("● rssi -67"));
    assert!(frame.contains("● 37.77,-122.42"));
    assert!(frame.contains("● yaw 24°"));
    assert!(frame.contains("○ idle"));
    assert!(frame.contains("● sim"));
    assert!(frame.contains("pid 4242 · up 1s · sim 1Hz"));
    assert!(frame.contains("offline · "));

    for _ in 0..3 {
        h.key(Key::Char('j'));
    }
    delay(80).await;
    let frame = h.frame();
    assert!(frame.contains("37.774900"), "{frame}");
    assert!(frame.contains("-122.419400"));
    assert!(frame.contains("alt 20 m · speed 0 km/h"));
    assert!(frame.contains("NMEA on /dev/ttyUSB1"));
    h.quit().await;
}

/// `monitor.test.tsx:122` — Settings: edit the run mode, then the wifi row.
#[tokio::test(start_paused = true)]
async fn opens_settings_and_edits_the_run_mode_then_moves_to_the_wifi_row() {
    let io = Arc::new(Fake {
        status: running(),
        saved: Mutex::new(Vec::new()),
    });
    let h = Harness::start(io.clone());
    delay(150).await;
    let idx = MODULES
        .iter()
        .position(|m| *m == ModuleId::Settings)
        .unwrap();
    for _ in 0..idx {
        h.key(Key::Char('j'));
    }
    delay(80).await;
    let frame = h.frame();
    assert!(frame.contains("run mode"), "{frame}");
    assert!(frame.contains("wifi"));
    assert!(!frame.contains("▶ media"));

    h.key(Key::Enter); // operating; row 0 = run mode
    delay(60).await;
    assert!(h.frame().contains("▶ run mode"), "{}", h.frame());

    h.key(Key::Char(' ')); // sim → real
    delay(60).await;
    let frame = h.frame();
    assert!(frame.contains("real"), "{frame}");
    assert!(frame.contains("run mode  real *"));

    // No WiFi adapter: run mode is the only focusable row; ↓ wraps onto it.
    h.key(Key::Char('j'));
    delay(60).await;
    let frame = h.frame();
    assert!(frame.contains("▶ run mode"));
    assert!(frame.contains("wifi      — not present"), "{frame}");

    // Q10: ↵ saves (the `set` writer) and prints the restart command.
    h.key(Key::Enter);
    delay(60).await;
    let frame = h.frame();
    assert_eq!(*io.saved.lock().unwrap(), ["real"]);
    assert!(
        frame.contains("✓ mode = real (persisted to settings.json) — restart to apply: sudo"),
        "{frame}"
    );
    assert!(!frame.contains("▶ run mode"));
    h.quit().await;
}

/// `monitor.test.tsx:167` — the not-running call-to-action (Q10 text).
#[tokio::test(start_paused = true)]
async fn shows_the_not_running_call_to_action() {
    let h = Harness::start(Arc::new(Fake {
        status: DaemonStatus::Stopped,
        saved: Mutex::new(Vec::new()),
    }));
    delay(150).await;
    let frame = h.frame();
    assert!(frame.contains("Daemon not running"), "{frame}");
    assert!(frame.contains("start"));
    assert!(
        frame.contains("Run sudo systemctl start xorgate-agent-rs to start the telemetry agent.")
    );
    assert!(frame.contains("○ daemon stopped"));
    // `s` keeps its key and prints the command instead of starting (Q10).
    h.key(Key::Char('s'));
    delay(60).await;
    assert!(h
        .frame()
        .contains("run: sudo systemctl start xorgate-agent-rs"));
    h.quit().await;
}
