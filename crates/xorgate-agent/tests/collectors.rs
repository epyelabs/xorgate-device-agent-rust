//! The gps and lte collectors replayed against the scenarios the REAL TS
//! classes produced (`fixtures/collectors/{gps,lte}-scenarios.json`, dumped by
//! `phase-2/evidence/wp2-tools/dump-{gps,lte}-scenarios.mts` from a
//! `git archive 89b5c64` scratch copy with `Date.now`/`hrtime` mocked to one
//! clock, wall = `baseWallMs` + mono).
//!
//! Each fixture step is replayed on tokio's PAUSED clock (`advance` =
//! `tokio::time::advance`), and every observable is compared verbatim: the
//! mapped group (key order and number text), `sample()`'s fields + its
//! `updatedMono` (ms since the collector was built), `clock()`, `sky()`,
//! `hasFix()`, the `gnss-sky.json` bytes and the `[gps]`/`[lte]` warnings the
//! step logged.
//!
//! gps: the NMEA port is a fake [`NmeaOpener`] — one `write` step = one data
//! chunk (the TS writes one FIFO chunk per step, like a 1 Hz tty); an
//! `absent` port answers every open with Node's ENOENT `error` + `close`; a
//! silent port never answers. lte: a real `telemetry.json` the steps rewrite.
//!
//! Plus the registry (`createCollector`, `index.ts:84-192`): read order,
//! empty groups omitted, idle media, `sampleGroups`, the hardware-block gates.

mod common;
mod log_capture;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use xorgate_agent::collectors::gps::{GpsCollector, GpsOptions};
use xorgate_agent::collectors::imu::{ImuCollector, ImuIo};
use xorgate_agent::collectors::lte::LteCollector;
use xorgate_agent::collectors::{Backends, Collector, Collectors, IoCapabilities};
use xorgate_agent::ctx::{AgentClock, AgentCtx, BoxFut, NoopHooks, TelemetrySource};
use xorgate_core::imu::{unavailable_detect, ImuDetect};
use xorgate_hal::child::{ChildEvent, ChildHandle, ChildSpec};
use xorgate_hal::io::{null_tape, TapeSink};
use xorgate_hal::nmea_tty::{NmeaOpener, NmeaStream, TtyError, TtyEvent};

// ---------------------------------------------------------------------------
// log capture: the `[gps]`/`[lte]`/`[imu]` lines, with the console level the
// TS used (warn!/error! = console.warn/error, info! = console.log)

pub type Logs = Arc<Mutex<Vec<(String, String)>>>;

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
        if !["[gps]", "[lte]", "[imu]"]
            .iter()
            .any(|p| m.0.starts_with(p))
        {
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

fn capture() -> (Logs, log_capture::CaptureGuard) {
    let logs: Logs = Arc::default();
    (logs.clone(), log_capture::install(Capture(logs)))
}

fn warns_since(logs: &Logs, from: usize) -> Vec<String> {
    logs.lock().unwrap()[from..]
        .iter()
        .map(|(_, m)| m.clone())
        .collect()
}

fn fixture(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/collectors")
        .join(name);
    serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap()
}

fn json(v: &impl serde::Serialize) -> String {
    serde_json::to_string(v).unwrap()
}

/// A ctx over `home` whose wall clock is `base + paused mono` and whose mono
/// origin is NOW (the fixture's mono 0).
fn ctx_at(home: &Path, base: i64) -> (AgentCtx, Instant) {
    let mut ctx = test_ctx(home, "dev-wp2", Arc::new(NoopHooks), null_tape());
    ctx.clock = Arc::new(AgentClock::following_tokio(base));
    (ctx, Instant::now())
}

/// Let spawned tasks (the stream task) run WITHOUT moving the paused clock.
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

// ---------------------------------------------------------------------------
// gps

#[derive(Clone, Copy, PartialEq)]
enum Port {
    Fifo,
    Absent,
}

struct FakeOpener {
    port: Port,
    streams: Mutex<Vec<(NmeaStream, mpsc::UnboundedSender<TtyEvent>)>>,
}

impl FakeOpener {
    fn new(port: Port) -> Arc<Self> {
        Arc::new(Self {
            port,
            streams: Mutex::default(),
        })
    }
    fn write(&self, data: &str) {
        let s = self.streams.lock().unwrap();
        let (_, tx) = s.last().expect("an open stream");
        tx.send(TtyEvent::Data(data.as_bytes().to_vec())).unwrap();
    }
    fn opens(&self) -> usize {
        self.streams.lock().unwrap().len()
    }
    fn stopped(&self) -> Vec<bool> {
        self.streams
            .lock()
            .unwrap()
            .iter()
            .map(|(h, _)| h.is_stopped())
            .collect()
    }
}

impl NmeaOpener for FakeOpener {
    fn open(&self, port: &Path) -> (NmeaStream, mpsc::UnboundedReceiver<TtyEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let h = NmeaStream::detached();
        if self.port == Port::Absent {
            // Node's async open failure: 'error' then 'close'.
            tx.send(TtyEvent::Error(TtyError {
                code: Some("ENOENT".into()),
                message: format!(
                    "ENOENT: no such file or directory, open '{}'",
                    port.display()
                ),
            }))
            .unwrap();
            tx.send(TtyEvent::Close).unwrap();
        }
        self.streams.lock().unwrap().push((h.clone(), tx));
        (h, rx)
    }
}

#[tokio::test(start_paused = true)]
async fn gps_scenarios_match_the_ts_collector() {
    let fx = fixture("gps-scenarios.json");
    let cases = fx["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 9);
    let (logs, _guard) = capture();
    for (ci, case) in cases.iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let base = input["baseWallMs"].as_i64().unwrap();
        let home = scratch(&format!("gps-{ci}"));
        let (ctx, t0) = ctx_at(&home, base);
        let absent = input["port"] != "fifo";
        let port = if absent {
            PathBuf::from(input["port"].as_str().unwrap())
        } else {
            home.join("nmea.fifo")
        };
        let opener = FakeOpener::new(if absent { Port::Absent } else { Port::Fifo });
        let sky_path = home.join("gnss-sky.json");
        let opts = GpsOptions {
            reopen_silent_ms: input["reopenSilentMs"].as_u64().unwrap_or(30_000),
            sky_state_path: sky_path.clone(),
            port,
        };
        let c = GpsCollector::with_options(&ctx, opener.clone(), opts);
        for (si, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let at = format!("[{name}] step {si} {}", step["op"]);
            let before = logs.lock().unwrap().len();
            let result: Option<Value> = match step["op"].as_str().unwrap() {
                "openWriter" => {
                    settle().await;
                    None
                }
                "write" => {
                    opener.write(step["data"].as_str().unwrap());
                    settle().await;
                    None
                }
                "advance" => {
                    tokio::time::advance(Duration::from_millis(step["ms"].as_u64().unwrap())).await;
                    None
                }
                "settle" => {
                    settle().await;
                    None
                }
                "read" => Some(serde_json::to_value(c.read().await).unwrap()),
                "sample" => Some(match c.sample().await {
                    Some(s) => serde_json::json!({
                        "fields": s.fields,
                        "updatedMonoMs": s.updated_mono.duration_since(t0).as_millis() as u64,
                    }),
                    None => Value::Null,
                }),
                "clock" => Some(serde_json::to_value(c.clock()).unwrap()),
                "sky" => Some(serde_json::to_value(c.sky()).unwrap()),
                "hasFix" => Some(Value::Bool(c.has_fix())),
                "skyFile" => Some(match std::fs::read_to_string(&sky_path) {
                    Ok(s) => Value::String(s),
                    Err(_) => Value::Null,
                }),
                other => panic!("{at}: unknown op {other}"),
            };
            if let Some(r) = result {
                assert_eq!(json(&r), json(&step["result"]), "{at}: result");
            }
            let want: Vec<String> = step["warns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap().to_string())
                .collect();
            assert_eq!(warns_since(&logs, before), want, "{at}: warnings");
        }
        // The silent-fd watchdog destroyed every stream it gave up on.
        if input["reopenSilentMs"].is_u64() {
            assert_eq!(opener.opens(), 2, "[{name}] open, reopen");
            assert_eq!(opener.stopped(), [true, true], "[{name}]");
        }
        // An absent port is retried on every tick (onEnd → started = false)
        // but warned about once.
        if absent {
            assert_eq!(opener.opens(), 3, "[{name}] one open per read");
        }
        c.stop();
    }
}

/// `gps.ts:242`: the sky file is only written when its directory exists.
#[tokio::test(start_paused = true)]
async fn gps_sky_file_needs_an_existing_home() {
    let home = scratch("gps-nohome");
    let (ctx, _) = ctx_at(&home, 1_781_000_000_000);
    let opener = FakeOpener::new(Port::Fifo);
    let missing = home.join("no-such-dir").join("gnss-sky.json");
    let c = GpsCollector::with_options(
        &ctx,
        opener.clone(),
        GpsOptions {
            reopen_silent_ms: 30_000,
            sky_state_path: missing.clone(),
            port: home.join("p"),
        },
    );
    let _ = c.sky();
    assert!(!missing.exists());
    assert!(
        !missing.parent().unwrap().exists(),
        "never creates the home"
    );
    assert_eq!(opener.opens(), 1, "sky() alone keeps the stream open");
    // gnssView(): fix + sky state, without opening a second stream.
    let src = Collectors::new(
        &ctx,
        IoCapabilities {
            imu: false,
            gps: true,
            signal: false,
            system_rails: None,
            obd: false,
        },
        Backends {
            nmea: opener.clone(),
            imu: Arc::new(NoImu),
        },
    )
    .await;
    let view = src.gnss_view().unwrap();
    assert_eq!(json(&view), r#"{"fix":false,"sky":"unknown"}"#);
    assert_eq!(src.gps_clock(), None);
}

/// The chunk splitter (`hardware/gps.ts:61-74`): a sentence split across two
/// chunks is parsed once whole; `onFix` fires once per chunk that changed the
/// aggregate; every complete line is reported to the tape before parsing.
#[tokio::test(start_paused = true)]
async fn gps_lines_span_chunks_and_reach_the_tape() {
    #[derive(Default)]
    struct Tape(Mutex<Vec<String>>);
    impl TapeSink for Tape {
        fn enabled(&self) -> bool {
            true
        }
        fn nmea(&self, line: &str) {
            self.0.lock().unwrap().push(line.to_string());
        }
    }
    let home = scratch("gps-chunks");
    let tape = Arc::new(Tape::default());
    let mut ctx = test_ctx(&home, "dev-wp2", Arc::new(NoopHooks), tape.clone());
    ctx.clock = Arc::new(AgentClock::following_tokio(1_781_000_000_000));
    let opener = FakeOpener::new(Port::Fifo);
    let c = GpsCollector::with_options(
        &ctx,
        opener.clone(),
        GpsOptions {
            reopen_silent_ms: 30_000,
            sky_state_path: home.join("gnss-sky.json"),
            port: home.join("p"),
        },
    );
    assert!(c.read().await.is_empty());
    let gga = "$GPGGA,083745.000,1430.1234,N,12101.5678,E,1,08,0.9,545.4,M,46.9,M,,*56";
    opener.write(&gga[..20]);
    settle().await;
    assert!(c.read().await.is_empty(), "half a sentence is not a fix");
    opener.write(&format!("{}\n$GPRMC,partial", &gga[20..]));
    settle().await;
    let g = c.read().await;
    assert_eq!(g["fix_quality"].value.0, 1.0);
    assert_eq!(g["lat"].value.0, 14.502057);
    assert_eq!(*tape.0.lock().unwrap(), [gga.to_string()]);
}

// ---------------------------------------------------------------------------
// lte

#[tokio::test(start_paused = true)]
async fn lte_scenarios_match_the_ts_collector() {
    let fx = fixture("lte-scenarios.json");
    let cases = fx["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 14);
    let (logs, _guard) = capture();
    for (ci, case) in cases.iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let home = scratch(&format!("lte-{ci}"));
        let (mut ctx, t0) = ctx_at(&home, case["input"]["baseWallMs"].as_i64().unwrap());
        let file = home.join("telemetry.json");
        let mut paths = (*ctx.paths).clone();
        paths.lte_telemetry = file.clone();
        ctx.paths = Arc::new(paths);
        let c = LteCollector::new(&ctx);
        for (si, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let at = format!("[{name}] step {si} {}", step["op"]);
            let before = logs.lock().unwrap().len();
            let result: Option<Value> = match step["op"].as_str().unwrap() {
                "file" => {
                    std::fs::write(&file, step["text"].as_str().unwrap()).unwrap();
                    None
                }
                "absent" => {
                    let _ = std::fs::remove_file(&file);
                    None
                }
                "advance" => {
                    tokio::time::advance(Duration::from_millis(step["ms"].as_u64().unwrap())).await;
                    None
                }
                "read" => Some(serde_json::to_value(c.read().await).unwrap()),
                "sample" => Some(match c.sample().await {
                    Some(s) => serde_json::json!({
                        "fields": s.fields,
                        "updatedMonoMs": s.updated_mono.duration_since(t0).as_millis() as u64,
                    }),
                    None => Value::Null,
                }),
                other => panic!("{at}: unknown op {other}"),
            };
            if let Some(r) = result {
                assert_eq!(json(&r), json(&step["result"]), "{at}: result");
            }
            let want: Vec<String> = step["warns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap().to_string())
                .collect();
            assert_eq!(warns_since(&logs, before), want, "{at}: warnings");
        }
    }
}

// ---------------------------------------------------------------------------
// the registry

/// An IMU edge that never finds i2cdetect (the shared bench profile's view).
struct NoImu;
impl ImuIo for NoImu {
    fn detect<'a>(&'a self, _tape: &'a dyn TapeSink) -> BoxFut<'a, ImuDetect> {
        Box::pin(async { unavailable_detect("i2cdetect not found — install i2c-tools.") })
    }
    fn spawn(&self, _spec: &ChildSpec) -> (ChildHandle, mpsc::UnboundedReceiver<ChildEvent>) {
        unreachable!("never detected")
    }
}

#[test]
fn io_capabilities_follow_the_hardware_block() {
    // worker.ts:281-287 — no block → everything; `imu: null` → no IMU
    // collector; `rails: false` → system without the PMIC/DT probes.
    assert_eq!(
        IoCapabilities::from_hardware(None),
        IoCapabilities {
            imu: true,
            gps: true,
            signal: true,
            system_rails: Some(true),
            obd: false,
        }
    );
    let block = xorgate_core::hardware_block::parse_hardware_block(&serde_json::json!({
        "v": 2, "platform": "rpi-cm4", "encoder": "hw-h264", "rails": false,
        "statusLed": null, "imu": null, "video": []
    }))
    .expect("hardware block");
    let caps = IoCapabilities::from_hardware(Some(&block));
    assert!(!caps.imu && caps.gps && caps.signal);
    assert_eq!(caps.system_rails, Some(false));
}

/// `createCollector().read()` (`index.ts:138-150`): imu → gps → lte → system,
/// empty groups omitted, the idle media block; `sampleGroups()` the same
/// order over the non-empty samples; one warning per absent input (O6: the
/// shared bench profile = no gps port + no python → only lte/system).
#[tokio::test(start_paused = true)]
async fn registry_order_omission_and_idle_media() {
    let (logs, _guard) = capture();
    let home = scratch("registry");
    let (mut ctx, _) = ctx_at(&home, 1_781_000_000_000);
    let file = home.join("telemetry.json");
    let mut paths = (*ctx.paths).clone();
    paths.lte_telemetry = file.clone();
    ctx.paths = Arc::new(paths);
    let opener = FakeOpener::new(Port::Absent);
    let src = Collectors::new(
        &ctx,
        IoCapabilities::from_hardware(None),
        Backends {
            nmea: opener.clone(),
            imu: Arc::new(NoImu),
        },
    )
    .await;
    let r = src.read().await.unwrap();
    assert_eq!(r.metrics.keys().collect::<Vec<_>>(), ["system"]);
    assert_eq!(
        json(&r.media),
        r#"{"kvs":{"value":false},"recording":{"value":false},"streams":[]}"#
    );
    settle().await;
    std::fs::write(
        &file,
        r#"{"timestamp":"2026-06-13T08:21:45Z","csq":18,"rssi_dbm":-77}"#,
    )
    .unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    let r = src.read().await.unwrap();
    assert_eq!(r.metrics.keys().collect::<Vec<_>>(), ["lte", "system"]);
    assert!(r.metrics["system"].contains_key("cpu_usage"));
    assert!(r.metrics["system"].contains_key("ram_usage"));
    let s = src.sample_groups().await;
    assert_eq!(s.keys().collect::<Vec<_>>(), ["lte", "system"]);
    let _ = src.sample_power().await; // dev box: no vcgencmd → may be None; never panics
    settle().await;
    let mut msgs: Vec<String> = logs
        .lock()
        .unwrap()
        .iter()
        .map(|(_, m)| m.clone())
        .collect();
    msgs.sort();
    assert_eq!(
        msgs,
        [
            format!(
                "[gps] GPS port {} not present (is the modem enumerated and GNSS enabled?)",
                ctx.paths.gps_nmea_port.display()
            ),
            "[imu] i2cdetect not found — install i2c-tools.".to_string(),
            "[lte] no telemetry yet — is sim7600-lte.service running?".to_string(),
        ]
    );
    src.stop();
}

/// Poll `f` exactly once: is it still pending?
async fn polls_pending<F: std::future::Future + Unpin>(f: &mut F) -> bool {
    std::future::poll_fn(|cx| {
        std::task::Poll::Ready(
            std::future::Future::poll(std::pin::Pin::new(&mut *f), cx).is_pending(),
        )
    })
    .await
}

/// D30 pin (review-wp6 §2): `sample()` holds the recorder-side cache across
/// its compute (`collectors/system.rs`), so a second `sample()` issued while
/// the first one's compute is in flight waits and returns the NEW cache. (The
/// TS sets `recLastAt` BEFORE `await compute()` without holding anything,
/// `system.ts:107-108`, so its second caller gets the PREVIOUS `recCache`.)
/// Deterministic (WP6, CI run 39328644627 flake): the runtime has ONE
/// blocking thread and the test occupies it, so the first sample's compute
/// (`tokio::fs` → `spawn_blocking`) is queued — it cannot finish inside its
/// first poll — until the test releases the blocker after polling both.
#[test]
fn system_sample_during_an_in_flight_compute_returns_the_new_cache() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .max_blocking_threads(1)
        .build()
        .expect("runtime");
    rt.block_on(async {
        use xorgate_agent::collectors::system::SystemCollector;
        let home = scratch("system-d30");
        let (ctx, _) = ctx_at(&home, 1_781_000_000_000);
        let c = SystemCollector::new(&ctx, false).await;
        let first = c.sample().await.expect("first sample computes");
        tokio::time::advance(Duration::from_millis(1000)).await;
        // Occupy the only blocking thread until both samples are in flight.
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let (started_tx, started) = std::sync::mpsc::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            gate.recv().unwrap();
        });
        started.recv().unwrap();
        // `a` PARKS inside its compute (holding the cache: its blocking read
        // is queued behind the blocker); `b`, polled while `a` is parked, must
        // WAIT (Rust holds the cache across the compute) — TS-like code would
        // answer at once with the PREVIOUS cache.
        let mut fa = c.sample();
        let mut fb = c.sample();
        assert!(
            polls_pending(&mut fa).await,
            "the first sample() parked mid-compute"
        );
        assert!(
            polls_pending(&mut fb).await,
            "the second sample() waits for it"
        );
        release.send(()).unwrap();
        blocker.await.unwrap();
        let (a, b) = tokio::join!(fa, fb);
        let (a, b) = (a.expect("a"), b.expect("b"));
        assert!(
            a.updated_mono > first.updated_mono,
            "a recomputed (gate due)"
        );
        assert_eq!(
            b.updated_mono, a.updated_mono,
            "b got the NEW cache, not the previous one"
        );
        assert_eq!(b, a);
        // …and a caller after it inside the same second gets that cache too.
        tokio::time::advance(Duration::from_millis(999)).await;
        assert_eq!(c.sample().await.expect("c").updated_mono, a.updated_mono);
    });
}

/// D30 sequential pin (review-wp6 M2, F3 ruled inherent): the gate stamp is
/// the CALL instant (`rec.last_at = mono_ms()` when `sample()` runs, =
/// `system.ts:104-113` `recLastAt`), not the caller's timer fire. Shed tick
/// fires at 0/1000/2000/3000 and samples promptly; the recorder samples at
/// 100, then its 1100 fire is starved until 2050, then fires on time at 2100
/// and 3100. The 2100 call lands 100 ms after the shed tick's 2000 stamp →
/// no recompute → the same cache generation the recorder already wrote at
/// 2050 → the freshness gate (`telemetry-recorder.ts:102`) drops `system`
/// for that ONE sample; at 3100 it is back. Deterministic: paused time, one
/// caller at a time.
#[tokio::test(start_paused = true)]
async fn d30_a_starved_recorder_sample_drops_system_once() {
    use xorgate_agent::collectors::system::SystemCollector;
    use xorgate_core::recorder_gate::RecorderGate;
    let home = scratch("system-d30-seq");
    let (ctx, t0) = ctx_at(&home, 1_781_000_000_000);
    let c = SystemCollector::new(&ctx, false).await;
    let mut gate: RecorderGate<Instant> = RecorderGate::new(None);
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let goto = |ms: u64| async move {
        let now = Instant::now();
        if at(ms) > now {
            tokio::time::advance(at(ms) - now).await;
        }
    };
    let rec = |gate: &mut RecorderGate<Instant>, s: &xorgate_agent::ctx::GroupSample| {
        gate.admit(
            "system",
            s.updated_mono,
            Instant::now().duration_since(t0).as_millis() as u64,
            1.0,
        )
    };
    let shed0 = c.sample().await.expect("shed @0 computes");
    goto(100).await;
    let r100 = c.sample().await.expect("rec @100");
    assert_eq!(
        r100.updated_mono, shed0.updated_mono,
        "not due: the shed tick's cache"
    );
    assert!(rec(&mut gate, &r100), "rec @100 writes system (g1)");
    goto(1000).await;
    let shed1 = c.sample().await.expect("shed @1000");
    assert!(shed1.updated_mono > shed0.updated_mono, "g2");
    goto(2000).await;
    let shed2 = c.sample().await.expect("shed @2000");
    assert!(shed2.updated_mono > shed1.updated_mono, "g3");
    goto(2050).await; // the recorder's 1100 fire, starved
    let r2050 = c.sample().await.expect("rec @2050");
    assert_eq!(r2050.updated_mono, shed2.updated_mono);
    assert!(rec(&mut gate, &r2050), "the late sample writes g3");
    goto(2100).await; // the recorder's next fire, on time
    let r2100 = c.sample().await.expect("rec @2100");
    assert_eq!(
        r2100.updated_mono, shed2.updated_mono,
        "100 ms after the 2000 stamp: no recompute"
    );
    assert!(!rec(&mut gate, &r2100), "the ONE system-less sample (F3)");
    goto(3000).await;
    let shed3 = c.sample().await.expect("shed @3000");
    goto(3100).await;
    let r3100 = c.sample().await.expect("rec @3100");
    assert_eq!(r3100.updated_mono, shed3.updated_mono);
    assert!(rec(&mut gate, &r3100), "system is back on the next tick");
}

// ---------------------------------------------------------------------------
// IMU shutdown (WP2 review MUST-FIX 1 + NIT 5): `stop()` signals a running
// helper's group, and nothing at all once the helper has exited.

/// A BNO085 "found" at 0x4a whose helper is a real `sh -c <script>` child;
/// keeps each handle's delivered-signal count (it outlives the handle).
struct ShImu {
    script: &'static str,
    delivered: Mutex<Vec<xorgate_hal::child::DeliveredSignals>>,
}
impl ImuIo for ShImu {
    fn detect<'a>(&'a self, _tape: &'a dyn TapeSink) -> BoxFut<'a, ImuDetect> {
        Box::pin(async { xorgate_core::imu::build_detect(vec![0x4a]) })
    }
    fn spawn(&self, _spec: &ChildSpec) -> (ChildHandle, mpsc::UnboundedReceiver<ChildEvent>) {
        let (h, rx) =
            xorgate_hal::child::spawn(&ChildSpec::new("sh", vec!["-c".into(), self.script.into()]));
        self.delivered.lock().unwrap().push(h.delivered());
        (h, rx)
    }
}

async fn imu_with(script: &'static str, tag: &str) -> (ImuCollector, Arc<ShImu>) {
    let home = scratch(tag);
    let ctx = test_ctx(&home, "dev-wp2", Arc::new(NoopHooks), null_tape());
    let io = Arc::new(ShImu {
        script,
        delivered: Mutex::default(),
    });
    let c = ImuCollector::new(&ctx, io.clone());
    assert!(c.read().await.is_empty(), "first read only kicks detection");
    (c, io)
}

async fn wait_until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn imu_stop_after_the_helper_exited_signals_nothing() {
    let (c, io) = imu_with("exit 2", "imu-park-stop").await;
    wait_until("the spawn", || io.delivered.lock().unwrap().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await; // `exit 2` is long done
                                                          // Park (exited before any sample) drops the dead handle …
    wait_until("the park", || c.helper_pid().is_none()).await;
    // … so a (late) shutdown has nothing to signal.
    c.stop();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(io.delivered.lock().unwrap()[0].get(), 0);
    assert!(
        c.read().await.is_empty(),
        "parked: no re-detect, no respawn"
    );
    assert_eq!(io.delivered.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn imu_stop_terminates_a_running_helper_group() {
    // the background sleep holds the pipes: the helper only reaches Done
    // (and parks) if the SIGTERM reached the whole group
    let (c, io) = imu_with("sleep 30 & wait", "imu-run-stop").await;
    wait_until("the spawn", || c.helper_pid().is_some()).await;
    c.stop();
    wait_until("the helper to end", || c.helper_pid().is_none()).await;
    assert_eq!(io.delivered.lock().unwrap()[0].get(), 1);
}
