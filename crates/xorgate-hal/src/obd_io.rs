//! The OBD peripheral's edges — the I/O half of `src/hardware/obd.ts` @ 09da38d:
//!
//! | TS | here |
//! |---|---|
//! | `streamObdData(cfg, handlers)` (`:441-462`): `python3 <python>/obd_read.py <argv>` | [`ObdIo::spawn`] → [`ObdHelperHandle`] + [`ObdHelperEvent`]s |
//! | `scanObdAdapters(seconds)` (`:378-395`): `python3 <python>/obd_scan.py --seconds N`, the LAST stdout line is the JSON | [`ObdIo::scan`] |
//! | `removeObdBond(mac)` (`:406-414`): `bluetoothctl remove <MAC>`, "removed" / "not available" = success, the MAC re-validated | [`ObdIo::unbond`] |
//! | `mockObdStream` + the mock scan/unbond under `isMock()` (`:378-384`, `:408`, `:469-558`) | [`mock_obd_schedule`], [`RealObdIo`] with `mock: true` |
//!
//! Nothing here pairs, and nothing reaches `bluetoothctl` but a MAC that
//! passed `OBD_MAC_RE` in this very function. No sudo: both helpers run as
//! the service user.
//!
//! TS quirk reproduced (probed with tsx @ 09da38d, WP2 evidence): execa 9
//! with `reject:false` resolves a missing binary as `{stderr: "", exitCode:
//! null, notFound: false}`, so `scanObdAdapters` / `removeObdBond` never say
//! "python3 not found" / "bluetoothctl not found"; they fail with `exit null`.
//! The same for `streamObdData`'s spawn failure: no "python3 not found" line,
//! just an exit with code `null` (`code ?`).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use xorgate_core::js::{number_to_string, trim};
use xorgate_core::obd::{
    build_obd_helper_args, is_obd_mac, normalize_scan_candidates, parse_obd_line, LastStop,
    ObdHelperConfig, ObdMessage, ObdScanCandidate,
};

use crate::child::{self, ChildEvent, ChildHandle, ChildSpec};
use crate::exec::{run, RunResult};

/// A boxed `Send` future (hal-local; the agent's `BoxFut` has the same shape).
pub type ObdFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The helper script (`pythonScript("obd_read.py")`).
pub const OBD_HELPER: &str = "obd_read.py";
/// The scan helper (`pythonScript("obd_scan.py")`).
pub const OBD_SCAN_HELPER: &str = "obd_scan.py";

/// One event from a running helper, in arrival order; `Exit` is last.
#[derive(Debug, Clone, PartialEq)]
pub enum ObdHelperEvent {
    /// A stdout line that parsed (`parseObdLine` non-null; others are ignored).
    Message(ObdMessage),
    /// A trimmed, non-empty stderr line (`onStderr`).
    Stderr(String),
    /// `done` resolved: `exitCode` (`None` = killed by a signal / never spawned).
    Exit(Option<i64>),
}

/// The stop half of a helper (`StreamHandle.stop`).
pub struct ObdHelperHandle {
    inner: HandleInner,
}

enum HandleInner {
    Real(ChildHandle),
    Mock(Option<oneshot::Sender<()>>),
    /// Tests: records the signals asked for.
    Fake(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>),
}

impl ObdHelperHandle {
    /// A handle whose `sigint` appends `"SIGINT"` to `log` (tests).
    pub fn fake(log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>) -> Self {
        Self {
            inner: HandleInner::Fake(log),
        }
    }

    /// `handle.stop("SIGINT")`: the helper sends ATZ, closes and exits 0
    /// (contract 1). Only the helper itself is signalled (`child.kill`).
    pub fn sigint(&mut self) {
        match &mut self.inner {
            HandleInner::Real(h) => h.signal(libc::SIGINT),
            HandleInner::Mock(tx) => {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
            }
            HandleInner::Fake(log) => log.lock().unwrap_or_else(|p| p.into_inner()).push("SIGINT"),
        }
    }

    /// The execa `cleanup` at agent exit (`subprocess.kill()` = SIGTERM to
    /// every child still running, D36 evidence): the helper turns this second
    /// signal into "finish closing without the reset" and still writes its
    /// `close` audit record (`python/obd/session.py:229-238`).
    pub fn sigterm(&mut self) {
        match &mut self.inner {
            HandleInner::Real(h) => h.signal(libc::SIGTERM),
            HandleInner::Mock(tx) => {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
            }
            HandleInner::Fake(log) => log
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push("SIGTERM"),
        }
    }

    /// The helper's pid while it is a real process.
    pub fn pid(&self) -> Option<u32> {
        match &self.inner {
            HandleInner::Real(h) => h.pid(),
            _ => None,
        }
    }
}

/// The OBD edges, injectable for tests.
pub trait ObdIo: Send + Sync + 'static {
    /// `streamObdData(cfg, …)`.
    fn spawn(
        &self,
        cfg: &ObdHelperConfig,
    ) -> (ObdHelperHandle, mpsc::UnboundedReceiver<ObdHelperEvent>);
    /// `scanObdAdapters(seconds)`: normalized candidates, or the error text.
    fn scan(&self, seconds: f64) -> ObdFut<Result<Vec<ObdScanCandidate>, String>>;
    /// `removeObdBond(mac)`. Like the TS (`await run("bluetoothctl", …)`
    /// inside `forget()`'s synchronous prefix), the subprocess is SPAWNED at
    /// call time; the future only collects its result.
    fn unbond(&self, mac: String) -> ObdFut<Result<(), String>>;
}

/// Production edges: the vendored helpers under `python_dir`, or the mock
/// stream / scan / unbond when `mock` (`isMock()`: `XORGATE_MOCK=1|true`, or
/// a non-Linux host).
#[derive(Debug, Clone)]
pub struct RealObdIo {
    pub python_dir: PathBuf,
    pub mock: bool,
    /// The unbond program (`bluetoothctl`, resolved through `PATH`; tests
    /// point it at a fake).
    pub bluetoothctl: String,
}

impl RealObdIo {
    /// `isMock()` from the process env (`lib/platform.ts:37-42`).
    pub fn from_env(python_dir: PathBuf) -> Self {
        Self {
            python_dir,
            bluetoothctl: "bluetoothctl".into(),
            mock: crate::led_driver::is_mock_from(
                std::env::var("XORGATE_MOCK").ok().as_deref(),
                cfg!(target_os = "linux"),
            ),
        }
    }

    /// The helper argv (tests, the tape): `python3 <dir>/obd_read.py <args>`.
    pub fn helper_spec(&self, cfg: &ObdHelperConfig) -> ChildSpec {
        let mut args = vec![self.python_dir.join(OBD_HELPER).display().to_string()];
        args.extend(build_obd_helper_args(cfg));
        ChildSpec::new("python3", args)
    }
}

/// `res.stderr.trim().split("\n").pop() || \`exit ${res.exitCode}\``, with the
/// execa quirk (a spawn failure has empty stderr and a null exit code).
fn failure_text(text: &str, res: &RunResult) -> String {
    let t = trim(text);
    let last = t.rsplit('\n').next().unwrap_or("");
    if !last.is_empty() {
        return last.to_string();
    }
    match res.exit_code {
        Some(c) if !res.spawn_failed => format!("exit {c}"),
        _ => "exit null".to_string(),
    }
}

/// The execa view of a `run()` result: a spawn failure carries no stderr.
fn execa_stderr(res: &RunResult) -> &str {
    if res.spawn_failed {
        ""
    } else {
        &res.stderr
    }
}

/// `scanObdAdapters` after the mock branch (`:385-394`).
pub fn scan_result(res: &RunResult) -> Result<Vec<ObdScanCandidate>, String> {
    if res.failed {
        return Err(failure_text(execa_stderr(res), res));
    }
    let stdout = if res.spawn_failed { "" } else { &res.stdout };
    let last = trim(stdout).rsplit('\n').next().unwrap_or("");
    let parsed: serde_json::Value =
        serde_json::from_str(last).map_err(|_| "scan helper printed no JSON".to_string())?;
    Ok(normalize_scan_candidates(&parsed))
}

/// `removeObdBond` after the MAC check and the mock branch (`:409-413`).
pub fn unbond_result(res: &RunResult) -> Result<(), String> {
    let (out, err) = if res.spawn_failed {
        ("", "")
    } else {
        (res.stdout.as_str(), res.stderr.as_str())
    };
    let text = format!("{out}\n{err}");
    let lower = text.to_lowercase();
    if lower.contains("device has been removed") || lower.contains("not available") {
        return Ok(());
    }
    if res.failed {
        return Err(failure_text(&text, res));
    }
    Ok(())
}

/// The two synthetic dongles of the mock scan (`:380-383`).
pub fn mock_scan_candidates() -> Vec<ObdScanCandidate> {
    vec![
        ObdScanCandidate {
            mac: "10:21:3E:4F:33:65".into(),
            name: "OBDII".into(),
            rssi: Some(-58.0),
        },
        ObdScanCandidate {
            mac: "00:1D:A5:68:98:8B".into(),
            name: "V-LINK".into(),
            rssi: Some(-71.0),
        },
    ]
}

impl ObdIo for RealObdIo {
    fn spawn(
        &self,
        cfg: &ObdHelperConfig,
    ) -> (ObdHelperHandle, mpsc::UnboundedReceiver<ObdHelperEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        if self.mock {
            let (stop_tx, stop_rx) = oneshot::channel();
            tokio::spawn(run_mock(cfg.clone(), tx, stop_rx));
            return (
                ObdHelperHandle {
                    inner: HandleInner::Mock(Some(stop_tx)),
                },
                rx,
            );
        }
        let (handle, mut events) = child::spawn(&self.helper_spec(cfg));
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                let out = match ev {
                    ChildEvent::Stdout(line) => match parse_obd_line(&line) {
                        Some(m) => ObdHelperEvent::Message(m),
                        None => continue,
                    },
                    ChildEvent::Stderr(line) => {
                        let t = trim(&line);
                        if t.is_empty() {
                            continue;
                        }
                        ObdHelperEvent::Stderr(t.to_string())
                    }
                    ChildEvent::Done(d) => ObdHelperEvent::Exit(d.exit_code.map(i64::from)),
                };
                if tx.send(out).is_err() {
                    break;
                }
            }
        });
        (
            ObdHelperHandle {
                inner: HandleInner::Real(handle),
            },
            rx,
        )
    }

    fn scan(&self, seconds: f64) -> ObdFut<Result<Vec<ObdScanCandidate>, String>> {
        if self.mock {
            return Box::pin(async { Ok(mock_scan_candidates()) });
        }
        let script = self.python_dir.join(OBD_SCAN_HELPER).display().to_string();
        Box::pin(async move {
            let secs = number_to_string(seconds);
            let res = run("python3", &[&script, "--seconds", &secs], None).await;
            scan_result(&res)
        })
    }

    fn unbond(&self, mac: String) -> ObdFut<Result<(), String>> {
        // Validated AGAIN here, before it can reach a subprocess argv.
        if !is_obd_mac(&mac) {
            let e = format!(
                "refusing to unbond malformed MAC {}",
                serde_json::to_string(&mac).unwrap_or_default()
            );
            return Box::pin(async move { Err(e) });
        }
        if self.mock {
            return Box::pin(async { Ok(()) });
        }
        // Spawned NOW (review-wp2 S2): `bluetoothctl` runs before the
        // command's accepted ack, as in the TS.
        let child = tokio::process::Command::new(&self.bluetoothctl)
            .args(["remove", &mac])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        Box::pin(async move {
            let res = match child {
                Err(e) => RunResult {
                    stderr: e.to_string(),
                    failed: true,
                    not_found: e.kind() == std::io::ErrorKind::NotFound,
                    spawn_failed: true,
                    ..Default::default()
                },
                Ok(c) => match c.wait_with_output().await {
                    Ok(out) => RunResult {
                        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                        exit_code: out.status.code(),
                        failed: !out.status.success(),
                        not_found: false,
                        spawn_failed: false,
                    },
                    Err(e) => RunResult {
                        stderr: e.to_string(),
                        failed: true,
                        ..Default::default()
                    },
                },
            };
            unbond_result(&res)
        })
    }
}

// ---------------------------------------------------------------------------
// the mock helper (`mockObdStream`, `:469-558`)
// ---------------------------------------------------------------------------

/// The mock stream's state between messages.
#[derive(Debug, Clone)]
pub struct MockObdStream {
    cfg: ObdHelperConfig,
    period_ms: u64,
    t: f64,
    fuel: f64,
    odometer: f64,
    requests: f64,
    /// (due ms, creation order, kind) of the pending timers.
    pending: Vec<(u64, u8, MockTimer)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockTimer {
    Probing,
    Live,
    Sampler,
    Auditor,
}

fn js_round(x: f64) -> f64 {
    xorgate_core::round::js_math_round(x)
}

impl MockObdStream {
    /// `mockObdStream(cfg, …)`: the two synchronous messages are returned by
    /// [`Self::start`]; the timers are pending.
    pub fn new(cfg: &ObdHelperConfig) -> Self {
        let period_ms = (js_round(1000.0 / cfg.poll_hz) as u64).max(50);
        Self {
            cfg: cfg.clone(),
            period_ms,
            t: 0.0,
            fuel: 61.2,
            odometer: 48213.4,
            requests: 0.0,
            pending: vec![
                (300, 0, MockTimer::Probing),
                (600, 1, MockTimer::Live),
                (period_ms, 2, MockTimer::Sampler),
                (10_000, 3, MockTimer::Auditor),
            ],
        }
    }

    /// The messages emitted synchronously at spawn (`ready`, `connecting`).
    pub fn start(&self, wall_ms: f64) -> Vec<ObdMessage> {
        vec![
            ObdMessage::Ready {
                helper: Some("obd_read".into()),
                version: Some("mock".into()),
                allowlist_sha256: Some("mock".into()),
            },
            ObdMessage::Status {
                state: "connecting".into(),
                link: 0,
                detail: Some("mock".into()),
                ts: Some(wall_ms / 1000.0),
            },
        ]
    }

    /// The due time (ms since spawn) of the next timer.
    pub fn next_due(&self) -> Option<u64> {
        self.pending.iter().map(|p| p.0).min()
    }

    /// Fire the next timer (the earliest; ties by creation order) at
    /// `wall_ms`, returning its messages.
    pub fn fire_next(&mut self, wall_ms: f64) -> (u64, Vec<ObdMessage>) {
        let i = (0..self.pending.len())
            .min_by_key(|&i| (self.pending[i].0, self.pending[i].1))
            .expect("the sampler and auditor never stop");
        let (due, order, kind) = self.pending.remove(i);
        let now_s = wall_ms / 1000.0;
        let mac = self
            .cfg
            .adapter
            .clone()
            .unwrap_or_else(|| "00:00:00:00:00:00".into());
        let mut out = vec![];
        match kind {
            MockTimer::Probing => out.push(ObdMessage::Status {
                state: "probing".into(),
                link: 1,
                detail: Some("mock".into()),
                ts: Some(now_s),
            }),
            MockTimer::Live => {
                let profile = serde_json::json!({
                    "ts": now_s,
                    "adapter": { "mac": mac, "ati": "ELM327 v1.5 (mock)", "at1": "mock" },
                    "protocol": { "dpn": "A6", "dp": "AUTO, ISO 15765-4 (CAN 11/500)" },
                    "responders": ["7E8"],
                    "pids_by_ecu": { "7E8": ["0C", "0D", "05", "2F", "42", "A6"] },
                    "supported_fields": ["rpm", "speed", "coolant_temp", "fuel_level", "module_voltage", "odometer"],
                    "unsupported_fields": [],
                    "powertrain": "ice",
                    "rate_req_s": 10,
                });
                out.push(ObdMessage::Profile(
                    profile.as_object().cloned().unwrap_or_default(),
                ));
                out.push(ObdMessage::Status {
                    state: "live".into(),
                    link: 2,
                    detail: Some("mock".into()),
                    ts: Some(now_s),
                });
            }
            MockTimer::Sampler => {
                self.pending.push((due + self.period_ms, order, kind));
                self.t += self.period_ms as f64 / 1000.0;
                if self.t >= 0.6 {
                    self.fuel = (self.fuel - 0.0005).max(0.0);
                    self.odometer += 0.0001;
                    self.requests += 6.0;
                    let t = self.t;
                    let fields = [
                        ("rpm", js_round(900.0 + t.sin() * 25.0)),
                        ("speed", 0.0),
                        ("coolant_temp", js_round(90f64.min(40.0 + t * 0.5))),
                        ("fuel_level", js_round(self.fuel * 10.0) / 10.0),
                        (
                            "module_voltage",
                            js_round((14.1 + (t / 3.0).sin() * 0.05) * 100.0) / 100.0,
                        ),
                        ("odometer", js_round(self.odometer * 10.0) / 10.0),
                        ("link", 2.0),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect();
                    out.push(ObdMessage::Sample {
                        fields,
                        ts: Some(now_s),
                    });
                }
            }
            MockTimer::Auditor => {
                self.pending.push((due + 10_000, order, kind));
                let r = self.requests;
                let counters = [
                    ("requests", r),
                    ("ok", r),
                    ("no_data", 0.0),
                    ("timeouts", 0.0),
                    ("err_adapter", 0.0),
                    ("err_protocol", 0.0),
                    ("err_identity", 0.0),
                    ("err_voltage", 0.0),
                    ("reconnects", 0.0),
                    ("forbidden_attempts", 0.0),
                    ("budget_violations", 0.0),
                    ("bytes_tx", r * 6.0),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
                let inventory = [("010C1", r / 6.0), ("010D1", r / 6.0)]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect();
                out.push(ObdMessage::Audit {
                    counters,
                    inventory,
                    last_stop: None::<LastStop>,
                    ts: Some(now_s),
                });
            }
        }
        (due, out)
    }
}

fn wall_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

/// The mock helper as a task: messages on the schedule, `Exit(Some(0))` when
/// stopped (the TS `done` resolves `exitCode: 0` on `stop()`).
async fn run_mock(
    cfg: ObdHelperConfig,
    tx: mpsc::UnboundedSender<ObdHelperEvent>,
    mut stop: oneshot::Receiver<()>,
) {
    let start = tokio::time::Instant::now();
    let wall0 = wall_now_ms();
    let mut m = MockObdStream::new(&cfg);
    for msg in m.start(wall0) {
        let _ = tx.send(ObdHelperEvent::Message(msg));
    }
    loop {
        let due = m.next_due().unwrap_or(u64::MAX);
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep_until(start + Duration::from_millis(due)) => {
                let wall = wall0 + start.elapsed().as_millis() as f64;
                let (_, msgs) = m.fire_next(wall);
                for msg in msgs {
                    if tx.send(ObdHelperEvent::Message(msg)).is_err() {
                        return;
                    }
                }
            }
        }
    }
    let _ = tx.send(ObdHelperEvent::Exit(Some(0)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(stdout: &str, stderr: &str, code: Option<i32>, spawn_failed: bool) -> RunResult {
        RunResult {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code: code,
            failed: spawn_failed || code != Some(0),
            not_found: spawn_failed,
            spawn_failed,
        }
    }

    #[test]
    fn scan_takes_the_last_stdout_line_and_normalizes() {
        let r = res(
            "scanning…\n[{\"mac\":\"10:21:3e:4f:33:65\",\"name\":\"OBDII\",\"rssi\":-58.4}]\n",
            "",
            Some(0),
            false,
        );
        assert_eq!(
            scan_result(&r).unwrap(),
            vec![ObdScanCandidate {
                mac: "10:21:3E:4F:33:65".into(),
                name: "OBDII".into(),
                rssi: Some(-58.0)
            }]
        );
        assert_eq!(
            scan_result(&res("no json\n", "", Some(0), false)).unwrap_err(),
            "scan helper printed no JSON"
        );
        assert_eq!(
            scan_result(&res(
                "",
                "Traceback\nRuntimeError: no controller\n",
                Some(2),
                false
            ))
            .unwrap_err(),
            "RuntimeError: no controller"
        );
        assert_eq!(
            scan_result(&res("", "", Some(3), false)).unwrap_err(),
            "exit 3"
        );
        // execa quirk: a missing python3 is `exit null`, never "python3 not found".
        let missing = res("", "No such file or directory (os error 2)", None, true);
        assert_eq!(scan_result(&missing).unwrap_err(), "exit null");
    }

    #[test]
    fn unbond_treats_removed_and_not_available_as_success() {
        assert!(unbond_result(&res("[CHG] Device has been removed\n", "", Some(0), false)).is_ok());
        assert!(unbond_result(&res(
            "",
            "Device AA:BB:CC:DD:EE:FF not available\n",
            Some(1),
            false
        ))
        .is_ok());
        assert_eq!(
            unbond_result(&res("", "Failed to remove device\n", Some(1), false)).unwrap_err(),
            "Failed to remove device"
        );
        assert_eq!(
            unbond_result(&res("", "", None, true)).unwrap_err(),
            "exit null",
            "a missing bluetoothctl (execa quirk)"
        );
        assert!(unbond_result(&res("ok\n", "", Some(0), false)).is_ok());
    }

    #[tokio::test]
    async fn unbond_revalidates_the_mac_before_any_subprocess() {
        // `mock: false` and a hostile MAC: refused before `bluetoothctl` runs.
        let io = RealObdIo {
            python_dir: PathBuf::from("/nonexistent"),
            mock: false,
            bluetoothctl: "bluetoothctl".into(),
        };
        for bad in [
            "aa:bb:cc:dd:ee:ff",
            "AA:BB:CC:DD:EE:FF; reboot",
            "",
            "AA:BB:CC:DD:EE",
        ] {
            let e = io.unbond(bad.to_string()).await.unwrap_err();
            assert!(e.starts_with("refusing to unbond malformed MAC \""), "{e}");
        }
    }

    /// review-wp2 S2: the subprocess starts at CALL time — the marker the
    /// fake `bluetoothctl` writes appears while the future is still unpolled.
    #[tokio::test]
    async fn unbond_spawns_at_call_time() {
        let dir = std::env::temp_dir().join(format!("xg-obd-unbond-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        let fake = dir.join("bluetoothctl");
        // Never `fs::write` + chmod here: a concurrent test's fork could
        // inherit the write fd and the exec below would fail with ETXTBSY
        // (the remaining flake; `test_support::write_executable`).
        crate::test_support::write_executable(
            &fake,
            // The marker appears ATOMICALLY (written aside, then renamed):
            // `> marker` alone creates it empty before `echo` writes, so a
            // poll could see it existing with no content (the WP4-run flake).
            &format!(
                "#!/bin/sh\necho \"$@\" > {m}.tmp\nmv {m}.tmp {m}\necho 'Device has been removed'\n",
                m = marker.display()
            ),
        )
        .unwrap();
        let io = RealObdIo {
            python_dir: PathBuf::from("/nonexistent"),
            mock: false,
            bluetoothctl: fake.display().to_string(),
        };
        let fut = io.unbond("AA:BB:CC:DD:EE:FF".into());
        // The future is NOT polled while we wait: only a call-time spawn can
        // make the marker appear. Bounded but generous (a loaded CI box can
        // take seconds to fork + exec a shell).
        let t0 = std::time::Instant::now();
        while !marker.exists() && t0.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !marker.exists() {
            panic!(
                "the fake bluetoothctl never ran in 60 s; unbond -> {:?}",
                fut.await
            );
        }
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            "remove AA:BB:CC:DD:EE:FF",
            "spawned before the first poll"
        );
        assert!(fut.await.is_ok());
        // A missing binary is the execa quirk's `exit null`.
        let missing = RealObdIo {
            bluetoothctl: dir.join("absent").display().to_string(),
            ..io
        };
        assert_eq!(
            missing
                .unbond("AA:BB:CC:DD:EE:FF".into())
                .await
                .unwrap_err(),
            "exit null"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn helper_spec_is_python3_on_the_vendored_helper() {
        let io = RealObdIo {
            python_dir: PathBuf::from("/opt/xorgate-rs/python"),
            mock: false,
            bluetoothctl: "bluetoothctl".into(),
        };
        let cfg = ObdHelperConfig {
            adapter: Some("10:21:3E:4F:33:65".into()),
            serial: None,
            pin: "1234".into(),
            poll_hz: 1.0,
            fields: None,
            power_source: "switched".into(),
        };
        assert_eq!(
            io.helper_spec(&cfg).argv(),
            [
                "python3",
                "/opt/xorgate-rs/python/obd_read.py",
                "--adapter",
                "10:21:3E:4F:33:65",
                "--pin",
                "1234",
                "--poll-hz",
                "1",
                "--power-source",
                "switched"
            ]
        );
    }
}
