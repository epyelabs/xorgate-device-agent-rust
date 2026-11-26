//! The IMU collector end to end — REAL detection (`i2cdetect -y 1` through
//! `xorgate_hal::imu_detect`) and a REAL supervised child (`python3 … --address
//! 0x4a` through `xorgate_hal::child`) — replayed against the scenarios the TS
//! `createImuCollector` produced (`fixtures/collectors/imu-scenarios.json`,
//! `phase-2/evidence/wp2-tools/dump-imu-scenarios.mts`, 89b5c64 scratch copy).
//!
//! Both sides run with a PATH that holds ONLY two shims (so "missing" really
//! is missing): `i2cdetect` prints the scenario's grid / stderr / exit code,
//! and `python3` is the line relay the dump script defines — it records its
//! argv, bumps `spawn.count`, relays a FIFO line by line (`E:` lines go to
//! stderr) and exits with the code in `exit.code` (`K` = SIGKILL itself).
//!
//! Real time (the helper is a real process; tokio's paused clock would
//! auto-advance while the runtime waits on its pipes): the steps sleep like
//! the dump script does, the mapped groups are compared verbatim, the
//! `[imu]` lines as one ordered sequence with their console level, and
//! `updatedMono` by its change pattern (the TS clock was mocked; what the
//! recorder keys on is "did the stamp move", CF18).
//!
//! Phase 7c (WP3): every case runs TWICE, once per `XORGATE_IMU_BACKEND`.
//! Under `rust` the same relay shim is the reader program
//! (`XORGATE_IMU_READER_BIN=<bin>/xorgate-imu-reader`, args `__imu-reader
//! --address 0x4a`) and there is no `python3` on PATH: the TS scenarios'
//! `[imu]` lines — detection, `reader stream ended; will re-detect`, the
//! verbatim park line, SIGKILL (`K`) → `Done(None)` → Redetect/Park — must be
//! identical under both backends (only the spawn differs).
//!
//! Phase 7c (review-wp3 FC2): the collector reads the device tree (R9's SoC
//! gate) ONLY under `rust`. Every case runs through [`DtCounting`], which
//! counts `ImuIo::dt_compatible` calls: 0 on every python case (mutant N2, an
//! eager read on every reader start, is RED), and > 0 over the rust cases
//! (the positive control: the gate really reads through the seam).
//!
//! ONE test function: PATH (and the IMU knobs) are process-global, so this
//! binary holds nothing else.

mod common;
mod log_capture;

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use xorgate_agent::collectors::imu::{ImuCollector, ImuIo, RealImuIo};
use xorgate_agent::collectors::Collector;
use xorgate_agent::ctx::{BoxFut, NoopHooks};
use xorgate_core::imu::ImuDetect;
use xorgate_hal::child::{ChildEvent, ChildHandle, ChildSpec};
use xorgate_hal::io::{null_tape, TapeSink};

type Logs = Arc<Mutex<Vec<(String, String)>>>;

/// The reader shim's file name under the `rust` backend.
const RUST_READER: &str = "xorgate-imu-reader";

/// Set (or clear) the IMU knobs for one backend (this binary only).
fn set_backend(rust: bool, bin: &Path) {
    if rust {
        std::env::set_var("XORGATE_IMU_BACKEND", "rust");
        std::env::set_var("XORGATE_IMU_READER_BIN", bin.join(RUST_READER));
        // R9: no BCM2712 device tree in a test container (the rig override)
        std::env::set_var("XORGATE_IMU_SOC_CHECK", "off");
    } else {
        std::env::remove_var("XORGATE_IMU_BACKEND");
        std::env::remove_var("XORGATE_IMU_READER_BIN");
        std::env::remove_var("XORGATE_IMU_SOC_CHECK");
    }
}

/// The REAL detection + child edges, with the device-tree read counted (FC2).
#[derive(Default)]
struct DtCounting {
    real: RealImuIo,
    dt_reads: AtomicUsize,
}
impl ImuIo for DtCounting {
    fn detect<'a>(&'a self, tape: &'a dyn TapeSink) -> BoxFut<'a, ImuDetect> {
        self.real.detect(tape)
    }
    fn spawn(&self, spec: &ChildSpec) -> (ChildHandle, mpsc::UnboundedReceiver<ChildEvent>) {
        self.real.spawn(spec)
    }
    fn dt_compatible(&self) -> std::io::Result<Vec<u8>> {
        self.dt_reads.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }
}

struct Capture(Logs);
struct Msg(String);
impl Visit for Msg {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        }
    }
}
impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, ev: &Event<'_>, _: Context<'_, S>) {
        let mut m = Msg(String::new());
        ev.record(&mut m);
        if !m.0.starts_with("[imu]") {
            return;
        }
        let level = match *ev.metadata().level() {
            Level::ERROR => "error",
            Level::WARN => "warn",
            _ => "log",
        };
        self.0.lock().unwrap().push((level.to_string(), m.0));
    }
}

fn shim(path: &Path, body: &str) {
    xorgate_hal::test_support::write_executable(path, body).unwrap();
}

async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn spawns(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join("spawn.count"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The case's PATH (`bin/`: the `i2cdetect` / `python3` shims), python dir,
/// home and the helper's FIFO under `dir`.
fn rig(
    dir: &Path,
    input: &Value,
    rust: bool,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let bin = dir.join("bin");
    let py = dir.join("python");
    let home = dir.join("home");
    for d in [&bin, &py, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let fifo = dir.join("imu.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    let d = dir.display();
    if let Some(i2c) = input["i2c"].as_object() {
        std::fs::write(dir.join("i2c.out"), i2c["stdout"].as_str().unwrap()).unwrap();
        std::fs::write(
            dir.join("i2c.err"),
            i2c.get("stderr").and_then(Value::as_str).unwrap_or(""),
        )
        .unwrap();
        std::fs::write(
            dir.join("i2c.code"),
            i2c.get("code")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .to_string(),
        )
        .unwrap();
        shim(
            &bin.join("i2cdetect"),
            &format!(
                "#!/bin/sh\n/bin/cat \"{d}/i2c.out\"\n/bin/cat \"{d}/i2c.err\" >&2\nexit $(/bin/cat \"{d}/i2c.code\")\n"
            ),
        );
    }
    if input["python"].as_bool().unwrap() {
        shim(
            &bin.join(if rust { RUST_READER } else { "python3" }),
            &[
                "#!/bin/sh".to_string(),
                format!("D=\"{d}\""),
                r#"printf "%s\n" "$*" >> "$D/argv.log""#.into(),
                r#"n=$(/bin/cat "$D/spawn.count" 2>/dev/null || echo 0); echo $((n+1)) > "$D/spawn.count""#.into(),
                r#"while IFS= read -r l; do case "$l" in E:*) printf "%s\n" "${l#E:}" >&2;; *) printf "%s\n" "$l";; esac; done < "$D/imu.fifo""#.into(),
                r#"c=$(/bin/cat "$D/exit.code" 2>/dev/null || echo 0)"#.into(),
                r#"[ "$c" = K ] && kill -KILL $$"#.into(),
                r#"exit "$c""#.into(),
                String::new(),
            ]
            .join("\n"),
        );
    }
    (bin, py, home, fifo)
}

/// D129: the line the Rust collector logs at a reader run's first sample
/// (`[imu] BNO08x at 0x4a — streaming`: the relay shim's `ready` lines carry
/// no identity, and these cases have no hardware block).
fn is_streaming_line(m: &str) -> bool {
    m.strip_prefix("[imu] BNO08x at 0x")
        .and_then(|r| r.strip_suffix(" — streaming"))
        .is_some_and(|hex| hex.len() == 2 && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The TS line, in D129's wording: the chip is `BNO08x` (no hardware block
/// in these cases), and "detected" no longer claims "streaming".
fn d129(ts: &str) -> String {
    if ts == "[imu] BNO085 not detected on I2C bus 1 (0x4a/0x4b)" {
        return "[imu] BNO08x not detected on I2C bus 1 (0x4a/0x4b)".to_string();
    }
    if let Some(addr) = ts
        .strip_prefix("[imu] BNO085 detected at ")
        .and_then(|r| r.strip_suffix(" — streaming"))
    {
        return format!("[imu] BNO08x detected at {addr} — starting the reader");
    }
    assert!(!ts.contains("BNO085"), "an unmapped TS line: {ts}");
    ts.to_string()
}

/// The reader runs of a case (one per `waitSpawn`) whose relay sent at
/// least one line the collector takes as a sample.
fn runs_with_a_sample(case: &Value) -> usize {
    let mut runs = 0;
    let mut sampled = false;
    for step in case["steps"].as_array().unwrap() {
        match step["op"].as_str().unwrap() {
            "waitSpawn" => sampled = false,
            "line" => {
                let text = step["text"].as_str().unwrap();
                if !sampled
                    && matches!(
                        xorgate_core::imu::parse_imu_line(text),
                        Some(xorgate_core::imu::ImuMessage::Sample { .. })
                    )
                {
                    sampled = true;
                    runs += 1;
                }
            }
            _ => {}
        }
    }
    runs
}

#[tokio::test]
async fn imu_scenarios_match_the_ts_collector() {
    let fx: Value = serde_json::from_slice(
        &std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/collectors/imu-scenarios.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let cases = fx["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 7);
    let logs: Logs = Arc::default();
    let _guard = log_capture::install(Capture(logs.clone()));
    let saved_path = std::env::var_os("PATH");
    let mut rust_dt_reads = 0;

    for rust in [false, true] {
        let backend = if rust { "rust" } else { "python" };
        for (ci, case) in cases.iter().enumerate() {
            let name = format!("{backend}/{}", case["name"].as_str().unwrap());
            let input = &case["input"];
            let dir = scratch(&format!("imu-{backend}-{ci}"));
            let (bin, py, home, fifo) = rig(&dir, input, rust);
            std::env::set_var("PATH", &bin);
            set_backend(rust, &bin);

            let mut ctx = test_ctx(&home, "dev-wp2", Arc::new(NoopHooks), null_tape());
            let mut paths = (*ctx.paths).clone();
            paths.python_dir = py.clone();
            ctx.paths = Arc::new(paths);
            let settings = ctx.paths.settings.clone();
            let io = Arc::new(DtCounting::default());
            let c = ImuCollector::new(&ctx, io.clone());

            let logs_from = logs.lock().unwrap().len();
            let mut writer: Option<std::fs::File> = None;
            // (fixture stamp, rust stamp) of the previous non-null sample
            let mut prev_stamp: Option<(u64, tokio::time::Instant)> = None;
            for (si, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                let at = format!("[{name}] step {si} {}", step["op"]);
                match step["op"].as_str().unwrap() {
                    "read" => {
                        let g = c.read().await;
                        assert_eq!(
                            serde_json::to_string(&g).unwrap(),
                            serde_json::to_string(&step["result"]).unwrap(),
                            "{at}"
                        );
                        sleep(30).await;
                    }
                    "sample" => {
                        let s = c.sample().await;
                        let want = &step["result"];
                        match (&s, want.is_null()) {
                            (None, true) => {}
                            (Some(s), false) => {
                                assert_eq!(
                                    serde_json::to_string(&s.fields).unwrap(),
                                    serde_json::to_string(&want["fields"]).unwrap(),
                                    "{at}: fields"
                                );
                                let ts = want["updatedMonoMs"].as_u64().unwrap();
                                if let Some((pts, prs)) = prev_stamp {
                                    assert_eq!(
                                        ts == pts,
                                        s.updated_mono == prs,
                                        "{at}: updatedMono moved in one implementation only"
                                    );
                                }
                                prev_stamp = Some((ts, s.updated_mono));
                            }
                            (got, _) => panic!("{at}: got {got:?}, want {want}"),
                        }
                        sleep(30).await;
                    }
                    "waitSpawn" => {
                        let n = step["n"].as_u64().unwrap();
                        for _ in 0..500 {
                            if spawns(&dir) >= n {
                                break;
                            }
                            sleep(10).await;
                        }
                        assert_eq!(spawns(&dir), n, "{at}: spawn count");
                        let _ = std::fs::remove_file(dir.join("exit.code"));
                        let f = fifo.clone();
                        writer = Some(
                            tokio::task::spawn_blocking(move || {
                                std::fs::OpenOptions::new().write(true).open(f).unwrap()
                            })
                            .await
                            .unwrap(),
                        );
                        sleep(50).await;
                    }
                    "line" => {
                        let w = writer.as_mut().expect("open writer");
                        w.write_all(format!("{}\n", step["text"].as_str().unwrap()).as_bytes())
                            .unwrap();
                        w.flush().unwrap();
                        sleep(120).await;
                    }
                    "exit" => {
                        std::fs::write(dir.join("exit.code"), step["code"].as_str().unwrap())
                            .unwrap();
                        writer = None;
                        sleep(400).await;
                    }
                    "settings" => {
                        if step["json"].is_null() {
                            let _ = std::fs::remove_file(&settings);
                        } else {
                            std::fs::write(&settings, step["json"].to_string()).unwrap();
                        }
                    }
                    "advance" => {}
                    "settle" => sleep(400).await,
                    other => panic!("{at}: unknown op {other}"),
                }
            }
            sleep(200).await;

            // the [imu] lines, in order, with their console level; the TS
            // wording mapped to D129's (no hardware block here: `BNO08x`)
            let want: Vec<(String, String)> = case["steps"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|s| s["logs"].as_array().unwrap().iter())
                .chain(case["logsAfterLastStep"].as_array().unwrap().iter())
                .map(|l| {
                    (
                        l["level"].as_str().unwrap().to_string(),
                        d129(l["msg"].as_str().unwrap()),
                    )
                })
                .collect();
            // D129's one Rust-only line, at the first sample of each reader
            // run: as many as the case has runs that produced a sample.
            let got: Vec<(String, String)> = logs.lock().unwrap()[logs_from..].to_vec();
            let (streaming, rest): (Vec<_>, Vec<_>) =
                got.into_iter().partition(|(_, m)| is_streaming_line(m));
            assert_eq!(rest, want, "[{name}] [imu] lines");
            assert_eq!(
                streaming.len(),
                runs_with_a_sample(case),
                "[{name}] one streaming line per reader run that sampled"
            );
            assert!(
                streaming.iter().all(|(l, _)| l == "log"),
                "[{name}]: info level"
            );

            // the helper's argv: `<python dir>/bno085_read.py --address 0x4a`
            let argv: Vec<String> = std::fs::read_to_string(dir.join("argv.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect();
            // (rust: `__imu-reader --address 0x4a`; argv[0] = xorgate-imu is
            // lost through the shell shim's interpreter and pinned by the
            // collector's unit test and the rig relay instead)
            let script = if rust {
                "__imu-reader".to_string()
            } else {
                format!("{}/bno085_read.py", py.display())
            };
            let want_args: Vec<String> = input["helperArgs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| format!("{script} {}", a.as_str().unwrap()))
                .collect();
            assert_eq!(argv, want_args, "[{name}] helper argv");
            assert_eq!(
                spawns(&dir),
                input["spawnCount"].as_u64().unwrap(),
                "[{name}]"
            );
            // every case ends with the helper gone, and its handle dropped (Park
            // included), so `stop()` has nothing left to signal
            assert_eq!(c.helper_pid(), None, "[{name}] no helper handle left");
            c.stop();
            // FC2: the python path never consults the device tree
            let dt_reads = io.dt_reads.load(Ordering::SeqCst);
            if rust {
                rust_dt_reads += dt_reads;
            } else {
                assert_eq!(dt_reads, 0, "[{name}] the python path read the DT");
            }
        }
    }
    set_backend(false, Path::new("/"));
    assert!(
        rust_dt_reads > 0,
        "control: the rust cases never read the DT through ImuIo::dt_compatible"
    );

    // SHUTDOWN while the helper streams (orchestrator close-out b): the TS
    // logs nothing there (the process exits, execa's cleanup kills the
    // helper), so `stop()` must not turn the helper's death into
    // `[imu] reader stream ended; will re-detect` (nor the park line).
    let case = cases
        .iter()
        .find(|c| c["input"]["python"] == true && c["input"]["spawnCount"].as_u64() >= Some(1))
        .expect("a streaming case");
    let dir = scratch("imu-shutdown");
    let (bin, py, home, fifo) = rig(&dir, &case["input"], false);
    std::env::set_var("PATH", &bin);
    let mut ctx = test_ctx(&home, "dev-wp2", Arc::new(NoopHooks), null_tape());
    let mut paths = (*ctx.paths).clone();
    paths.python_dir = py.clone();
    ctx.paths = Arc::new(paths);
    let c = ImuCollector::new(&ctx, Arc::new(RealImuIo::default()));
    let logs_from = logs.lock().unwrap().len();
    let _ = c.read().await;
    for _ in 0..500 {
        if spawns(&dir) >= 1 {
            break;
        }
        sleep(10).await;
    }
    let f = fifo.clone();
    let mut w = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new().write(true).open(f).unwrap()
    })
    .await
    .unwrap();
    sleep(50).await;
    w.write_all(b"{\"type\": \"sample\", \"quat\": {\"r\": 1.0, \"i\": 0.0, \"j\": 0.0, \"k\": 0.0}, \"linaccel\": {\"x\": 0.0, \"y\": 0.0, \"z\": 9.8}}\n")
        .unwrap();
    w.flush().unwrap();
    sleep(200).await;
    assert!(c.sample().await.is_some(), "the helper streams");
    assert!(c.helper_pid().is_some());
    c.stop();
    for _ in 0..200 {
        if c.helper_pid().is_none() {
            break;
        }
        sleep(10).await;
    }
    sleep(200).await;
    drop(w);
    let after: Vec<(String, String)> = logs.lock().unwrap()[logs_from..].to_vec();
    assert!(
        !after
            .iter()
            .any(|(_, m)| m.contains("reader stream ended") || m.contains("reader exited")),
        "{after:?}"
    );
    assert_eq!(
        c.helper_pid(),
        None,
        "the stopped helper's handle is dropped"
    );

    match saved_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
}
