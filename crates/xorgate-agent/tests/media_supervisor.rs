//! `MediaSupervisor` over the same scripts as the core ladder fixture
//! (`fixtures/media_ladder/ladder.json`, dumped from the REAL
//! `createMediaCollector` with a fake runtime and virtual timers), here
//! under `tokio::time::pause` with a fake [`MasterBackend`]: real tokio
//! timers, the real pump tasks, the real `[media …]` log lines (captured
//! from `tracing`), the real `resolveChannelPipeline` over the fixture's
//! pipeline directory, and a recording tape.
//!
//! tokio does not order same-instant timers by creation like Node does, so
//! events are compared per virtual millisecond as a multiset (the core test
//! `media_ladder_fixtures` pins the exact order); everything else — times,
//! texts, levels, spawns with their pipeline, signals, every `read()`
//! snapshot — is exact.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
mod log_capture;
use tracing_subscriber::layer::{Context, Layer};
use xorgate_agent::media::{
    GuardFn, MasterBackend, MasterProc, MediaConfig, MediaSupervisor, StreamGuard,
};
use xorgate_core::media_ladder::RuntimeDetect;
use xorgate_hal::child::{ChildDone, ChildEvent, ChildSpec};
use xorgate_hal::io::{ChildExitRec, ChildSpawnRec, TapeSink};

type Trace = Arc<Mutex<Vec<Value>>>;

struct Capture {
    trace: Trace,
    origin: tokio::time::Instant,
}

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
        if !m.0.starts_with("[media") {
            return;
        }
        let level = match *ev.metadata().level() {
            Level::ERROR => "error",
            Level::WARN => "warn",
            _ => "log",
        };
        let t = self.origin.elapsed().as_millis() as u64;
        self.trace
            .lock()
            .unwrap()
            .push(json!({"t": t, "k": "log", "level": level, "line": m.0}));
    }
}

#[derive(Default)]
struct FakeState {
    /// channel → (event sender, exiting, done)
    procs: HashMap<String, (mpsc::UnboundedSender<ChildEvent>, bool, bool)>,
}

struct Fake {
    detect: RuntimeDetect,
    origin: tokio::time::Instant,
    trace: Trace,
    sigint_exit_ms: u64,
    st: Arc<Mutex<FakeState>>,
    specs: Mutex<Vec<ChildSpec>>,
}

impl Fake {
    fn t(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

struct FakeProc {
    channel: String,
    fake: Arc<Fake>,
}

impl MasterProc for FakeProc {
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
    fn signal(&self, sig: i32) {
        assert_eq!(sig, nix::libc::SIGINT, "masters are only ever SIGINTed");
        let f = &self.fake;
        f.trace
            .lock()
            .unwrap()
            .push(json!({"t": f.t(), "k": "signal", "channel": self.channel, "sig": "SIGINT"}));
        let mut st = f.st.lock().unwrap();
        let e = st.procs.get_mut(&self.channel).unwrap();
        if !e.1 && !e.2 {
            e.1 = true;
            let tx = e.0.clone();
            let ms = f.sigint_exit_ms;
            let st2 = f.st.clone();
            let ch = self.channel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                st2.lock().unwrap().procs.get_mut(&ch).unwrap().2 = true;
                let _ = tx.send(ChildEvent::Done(ChildDone {
                    exit_code: Some(0),
                    ..Default::default()
                }));
            });
        }
    }
}

struct Backend(Arc<Fake>);

impl MasterBackend for Backend {
    fn detect(&self, _binary_path: &str) -> RuntimeDetect {
        self.0.detect.clone()
    }
    fn spawn(
        &self,
        spec: &ChildSpec,
    ) -> (Arc<dyn MasterProc>, mpsc::UnboundedReceiver<ChildEvent>) {
        let f = &self.0;
        let channel = spec.args[0].clone();
        let pipeline = spec
            .env
            .iter()
            .find(|(k, _)| k == "KVS_GST_PIPELINE")
            .map(|(_, v)| v.clone());
        f.trace
            .lock()
            .unwrap()
            .push(json!({"t": f.t(), "k": "spawn", "channel": channel, "pipeline": pipeline}));
        f.specs.lock().unwrap().push(spec.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        // Drained output: a blank line is skipped, a real one logged.
        let _ = tx.send(ChildEvent::Stdout("   ".into()));
        let mut st = f.st.lock().unwrap();
        if let Some(prev) = st.procs.get(&channel) {
            assert!(prev.2, "double spawn {channel}");
        }
        st.procs.insert(channel.clone(), (tx, false, false));
        (
            Arc::new(FakeProc {
                channel,
                fake: f.clone(),
            }),
            rx,
        )
    }
}

#[derive(Default)]
struct RecTape {
    spawns: Mutex<Vec<ChildSpawnRec>>,
    signals: Mutex<Vec<(String, String, i32)>>,
    exits: Mutex<Vec<ChildExitRec>>,
    lines: Mutex<usize>,
}

impl TapeSink for RecTape {
    fn child_spawn(&self, r: &ChildSpawnRec) {
        self.spawns.lock().unwrap().push(r.clone());
    }
    fn child_line(&self, role: &str, _key: &str, _fd: u8, _line: &str) {
        assert_eq!(role, "kvs");
        *self.lines.lock().unwrap() += 1;
    }
    fn child_signal(&self, role: &str, key: &str, _pid: u32, sig: i32) {
        self.signals
            .lock()
            .unwrap()
            .push((role.into(), key.into(), sig));
    }
    fn child_exit(&self, r: &ChildExitRec) {
        self.exits.lock().unwrap().push(r.clone());
    }
}

async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Group by `t` and sort each group (see the module doc).
fn by_ms(v: &[Value]) -> BTreeMap<u64, Vec<String>> {
    let mut m: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for e in v {
        m.entry(e["t"].as_u64().unwrap())
            .or_default()
            .push(e.to_string());
    }
    for g in m.values_mut() {
        g.sort();
    }
    m
}

#[tokio::test(start_paused = true)]
async fn supervisor_follows_the_ts_scripts_under_paused_time() {
    let fx: Value = serde_json::from_slice(
        &std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/media_ladder/ladder.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let cases = fx["cases"].as_array().unwrap();
    assert!(cases.len() >= 14);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let inp = &c["input"];
        let origin = tokio::time::Instant::now();
        let trace: Trace = Arc::default();
        let _g = log_capture::install(Capture {
            trace: trace.clone(),
            origin,
        });

        // The driver's world, at the driver's own paths.
        let pipeline_dir = inp["pipelineDir"].as_str().unwrap().to_string();
        let base = Path::new(&pipeline_dir).parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&pipeline_dir).unwrap();
        for k in inp["pipelines"].as_array().unwrap() {
            std::fs::write(format!("{pipeline_dir}/{}.gst", k.as_str().unwrap()), "x\n").unwrap();
        }
        let manifest_path = inp["manifestPath"].as_str().unwrap().to_string();
        match &inp["manifest"] {
            Value::Null => {}
            Value::String(t) => std::fs::write(&manifest_path, t).unwrap(),
            v => std::fs::write(&manifest_path, v.to_string()).unwrap(),
        }
        let guard_tbl: Arc<Mutex<Option<HashMap<String, String>>>> =
            Arc::new(Mutex::new(inp["guard"].as_object().map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                    .collect()
            })));
        let gt = guard_tbl.clone();
        let guard: GuardFn = Arc::new(move |k: &str| match &*gt.lock().unwrap() {
            None => StreamGuard::Ok,
            Some(m) => StreamGuard::parse(m.get(k).map_or("ok", String::as_str)).unwrap(),
        });
        let fake = Arc::new(Fake {
            detect: RuntimeDetect {
                available: inp["detect"]["available"].as_bool().unwrap(),
                gstreamer: inp["detect"]["gstreamer"].as_bool().unwrap(),
                reason: inp["detect"]["reason"].as_str().map(str::to_string),
            },
            origin,
            trace: trace.clone(),
            sigint_exit_ms: inp["sigintExitMs"].as_u64().unwrap(),
            st: Arc::default(),
            specs: Mutex::default(),
        });
        let cfg = MediaConfig {
            enabled: true,
            binary_path: "/opt/xorgate-rs/media/xorgate-kvs-webrtc".into(),
            lib_dir: inp["libDir"].as_str().unwrap().into(),
            manifest_path,
            source: inp["source"].as_str().unwrap().into(),
            media_type: "video-only".into(),
            pipeline_dir,
            pipeline_path: inp["globalPipeline"].as_str().map(str::to_string),
            log_level: "3".into(),
            idle_teardown_secs: "30".into(),
            cert_path: "/etc/xorgate/cert.crt".into(),
            key_path: "/etc/xorgate/private.key".into(),
            ca_cert_path: "/opt/xorgate-rs/media/cert.pem".into(),
        };
        let tape = Arc::new(RecTape::default());
        let sup = MediaSupervisor::with_backend(
            cfg,
            guard,
            tape.clone(),
            Arc::new(Backend(fake.clone())),
        );
        let mut script_exits = 0usize;

        for step in inp["steps"].as_array().unwrap() {
            let at = step["at"].as_u64().unwrap();
            tokio::time::sleep_until(origin + Duration::from_millis(at)).await;
            settle().await;
            match step["op"].as_str().unwrap() {
                "read" => {
                    let media = sup.read();
                    trace
                        .lock()
                        .unwrap()
                        .push(json!({"t": at, "k": "read", "media": media}));
                }
                "exit" => {
                    let ch = step["channel"].as_str().unwrap();
                    let tx = {
                        let mut st = fake.st.lock().unwrap();
                        let e = st.procs.get_mut(ch).unwrap();
                        assert!(!e.1 && !e.2, "{name}: no live {ch} at {at}");
                        e.2 = true;
                        e.0.clone()
                    };
                    script_exits += 1;
                    let _ = tx.send(ChildEvent::Done(ChildDone {
                        exit_code: step["code"].as_i64().map(|c| c as i32),
                        ..Default::default()
                    }));
                }
                "shed" => sup.set_shed(step["on"].as_bool().unwrap()),
                "disabled" => {
                    let keys: Vec<String> = step["keys"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect();
                    sup.set_disabled_streams(&keys);
                }
                "restart" => sup.restart_channel(step["key"].as_str().unwrap()),
                "stop" => sup.stop(),
                "guard" => {
                    guard_tbl.lock().unwrap().as_mut().unwrap().insert(
                        step["key"].as_str().unwrap().into(),
                        step["verdict"].as_str().unwrap().into(),
                    );
                }
                "pipeline" => {
                    let p = format!(
                        "{}/{}.gst",
                        inp["pipelineDir"].as_str().unwrap(),
                        step["key"].as_str().unwrap()
                    );
                    if step["present"].as_bool().unwrap() {
                        std::fs::write(p, "x\n").unwrap();
                    } else {
                        let _ = std::fs::remove_file(p);
                    }
                }
                other => panic!("unknown op {other}"),
            }
            settle().await;
        }
        tokio::time::sleep_until(origin + Duration::from_millis(inp["endAt"].as_u64().unwrap()))
            .await;
        settle().await;

        let got = trace.lock().unwrap().clone();
        let want: Vec<Value> = c["expected"].as_array().unwrap().clone();
        assert_eq!(by_ms(&got), by_ms(&want), "{name}");

        // The launch contract and the tape (role kvs, key = stream key).
        let specs = fake.specs.lock().unwrap().clone();
        let spawns = tape.spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), specs.len(), "{name}");
        for (s, r) in specs.iter().zip(&spawns) {
            assert_eq!(s.program, "/opt/xorgate-rs/media/xorgate-kvs-webrtc");
            assert_eq!(
                s.args[1..],
                ["video-only".to_string(), "testsrc".to_string()]
            );
            assert_eq!(s.cwd, None);
            assert_eq!(r.role, "kvs");
            assert_eq!(r.argv, s.argv());
            assert_eq!(r.key, s.args[0].rsplit('-').next().unwrap());
            assert_eq!(
                r.env.get("KVS_IDLE_TEARDOWN_SECS").map(String::as_str),
                Some("30")
            );
            assert_eq!(
                r.env.get("AWS_IOT_CORE_THING_NAME").map(String::as_str),
                Some("a73f7a36-dev")
            );
        }
        let signals = tape.signals.lock().unwrap().clone();
        assert!(signals
            .iter()
            .all(|(r, _, s)| r == "kvs" && *s == nix::libc::SIGINT));
        let exits = tape.exits.lock().unwrap().clone();
        assert_eq!(
            exits.iter().filter(|e| e.external).count(),
            script_exits,
            "{name}: external exits = the script's crashes"
        );
        assert_eq!(
            *tape.lines.lock().unwrap(),
            specs.len(),
            "{name}: one drained line per master"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
