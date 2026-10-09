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
//! ONE test function: PATH is process-global, so this binary holds nothing else.

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use serde_json::Value;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use xorgate_agent::collectors::imu::{ImuCollector, RealImuIo};
use xorgate_agent::collectors::Collector;
use xorgate_agent::ctx::NoopHooks;
use xorgate_hal::io::null_tape;

type Logs = Arc<Mutex<Vec<(String, String)>>>;

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
    std::fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Capture(logs.clone())),
    );
    let saved_path = std::env::var_os("PATH");

    for (ci, case) in cases.iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let dir = scratch(&format!("imu-{ci}"));
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
                &bin.join("python3"),
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
        std::env::set_var("PATH", &bin);

        let mut ctx = test_ctx(&home, "dev-wp2", Arc::new(NoopHooks), null_tape());
        let mut paths = (*ctx.paths).clone();
        paths.python_dir = py.clone();
        ctx.paths = Arc::new(paths);
        let settings = ctx.paths.settings.clone();
        let c = ImuCollector::new(&ctx, Arc::new(RealImuIo::default()));

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
                    std::fs::write(dir.join("exit.code"), step["code"].as_str().unwrap()).unwrap();
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

        // the [imu] lines, in order, with their console level
        let want: Vec<(String, String)> = case["steps"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["logs"].as_array().unwrap().iter())
            .chain(case["logsAfterLastStep"].as_array().unwrap().iter())
            .map(|l| {
                (
                    l["level"].as_str().unwrap().to_string(),
                    l["msg"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            logs.lock().unwrap()[logs_from..],
            want[..],
            "[{name}] [imu] lines"
        );

        // the helper's argv: `<python dir>/bno085_read.py --address 0x4a`
        let argv: Vec<String> = std::fs::read_to_string(dir.join("argv.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let script = format!("{}/bno085_read.py", py.display());
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
    }
    match saved_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
}
