//! `LedDriver` against the REAL TS driver (`src/hardware/led.ts:249-418`
//! @ 89b5c64): `fixtures/led_driver/led-driver.json` holds the 14 LedDriver
//! it()s of `test/led.test.ts` (`:72` expanded per colour) re-run step by
//! step under vitest fake timers with a recording writer — every writer call
//! `{t, pins, high}` and log line `{t, msg}` in virtual ms. Each case here
//! replays the same steps under `tokio::time::pause()` and must produce the
//! identical trace, then re-asserts what the original it() asserts.
//!
//! Plus the host probe (`probeHost`, `led.ts:420-433`) over injected host
//! facts, and the CF50 `[used]` adjustment over recorded libgpiod text.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::time::Instant;
use xorgate_core::led::{LedColour, LedPattern, LedPinMap, PinMapCheck};
use xorgate_hal::io::{GpioVia, TapeSink};
use xorgate_hal::led_driver::{
    probe_host_with, strip_own_consumer, BoxFut, HostProbe, LedDriver, LedDriverOptions, PinWriter,
    TapedWriter, LED_CONSUMER,
};

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    ts: String,
    expected: Trace,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
struct Trace {
    calls: Vec<Call>,
    logs: Vec<Log>,
    marks: serde_json::Map<String, Value>,
    #[serde(default)]
    extra: serde_json::Map<String, Value>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
struct Call {
    t: u64,
    pins: Vec<u32>,
    high: bool,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
struct Log {
    t: u64,
    msg: String,
}

fn fixture() -> Fixture {
    let p = format!(
        "{}/../../fixtures/led_driver/led-driver.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[derive(Default)]
struct Sink {
    calls: Mutex<Vec<Call>>,
    logs: Mutex<Vec<Log>>,
    marks: Mutex<serde_json::Map<String, Value>>,
}

struct Rec {
    sink: Arc<Sink>,
    t0: Instant,
    ok: OkFn,
    panics: bool,
}

type OkFn = Box<dyn Fn(&[u32], bool) -> bool + Send + Sync>;

fn ms_since(t0: Instant) -> u64 {
    Instant::now().duration_since(t0).as_millis() as u64
}

impl PinWriter for Rec {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        self.sink.calls.lock().unwrap().push(Call {
            t: ms_since(self.t0),
            pins: pins.to_vec(),
            high,
        });
        if self.panics {
            panic!("pinctrl exploded");
        }
        let ok = (self.ok)(pins, high);
        Box::pin(async move { ok })
    }
}

struct Opts {
    map: LedPinMap,
    ok: OkFn,
    probe_ok: bool,
    panics: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            map: LedPinMap::status_led_map(),
            ok: Box::new(|_, _| true),
            probe_ok: true,
            panics: false,
        }
    }
}

fn driver(sink: &Arc<Sink>, t0: Instant, o: Opts) -> LedDriver {
    let probe_ok = o.probe_ok;
    let logs = sink.clone();
    LedDriver::new(LedDriverOptions {
        map: Some(o.map),
        writer: Some(Arc::new(Rec {
            sink: sink.clone(),
            t0,
            ok: o.ok,
            panics: o.panics,
        })),
        probe: Some(Box::new(move || {
            Box::pin(async move {
                if probe_ok {
                    PinMapCheck {
                        ok: true,
                        reason: None,
                    }
                } else {
                    PinMapCheck {
                        ok: false,
                        reason: Some("not a Raspberry Pi".into()),
                    }
                }
            })
        })),
        log: Some(Box::new(move |m| {
            logs.logs.lock().unwrap().push(Log {
                t: ms_since(t0),
                msg: m.to_string(),
            })
        })),
    })
}

async fn adv(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn mark(sink: &Sink, k: &str) {
    let n = sink.calls.lock().unwrap().len();
    sink.marks.lock().unwrap().insert(k.into(), n.into());
}

fn init_json(i: &xorgate_hal::led_driver::LedInit) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("ok".into(), i.ok.into());
    if let Some(r) = &i.reason {
        m.insert("reason".into(), r.clone().into());
    }
    let p = &i.map.pins;
    m.insert(
        "map".into(),
        serde_json::json!({
            "kind": i.map.kind,
            "pins": { "r": p.r as u64, "g": p.g as u64, "b": p.b as u64 },
            "activeHigh": i.map.active_high,
        }),
    );
    Value::Object(m)
}

/// Replays one TS scenario; returns the trace + the extras it records.
async fn run_case(name: &str) -> Trace {
    let sink = Arc::new(Sink::default());
    let t0 = Instant::now();
    let mut extra = serde_json::Map::new();
    let colour_case = name
        .strip_prefix("colour ")
        .and_then(|r| r.split(' ').next())
        .and_then(LedColour::parse);
    if let Some(colour) = colour_case {
        let d = driver(&sink, t0, Opts::default());
        d.init().await;
        d.set(colour).await;
    } else {
        match name {
            "active low inverts the channels" => {
                let mut map = LedPinMap::status_led_map();
                map.active_high = false;
                let d = driver(
                    &sink,
                    t0,
                    Opts {
                        map,
                        ..Opts::default()
                    },
                );
                d.init().await;
                d.set(LedColour::Red).await;
            }
            "non-repeating pattern ends dark" => {
                let d = driver(&sink, t0, Opts::default());
                d.init().await;
                d.play(LedColour::White, LedPattern::Flash2).await;
            }
            "solid stays lit until stop()" => {
                let d = driver(&sink, t0, Opts::default());
                d.init().await;
                let running = tokio::spawn(d.play(LedColour::Red, LedPattern::Solid));
                adv(10).await;
                d.stop();
                running.await.unwrap();
            }
            "stop a repeating pattern, then blank" => {
                let d = driver(&sink, t0, Opts::default());
                d.init().await;
                let running = tokio::spawn(d.play(LedColour::Purple, LedPattern::BlinkFast));
                adv(300).await;
                d.stop();
                running.await.unwrap();
                d.blank(false).await;
            }
            "only changing channels are written within a pattern" => {
                let d = driver(&sink, t0, Opts::default());
                d.init().await;
                let running = tokio::spawn(d.play(LedColour::Green, LedPattern::BlinkSlow));
                adv(1300).await;
                d.stop();
                running.await.unwrap();
            }
            "one call per transition for a two-channel colour" => {
                let d = driver(&sink, t0, Opts::default());
                d.init().await;
                let running = tokio::spawn(d.play(LedColour::Purple, LedPattern::BlinkFast));
                adv(700).await;
                d.stop();
                running.await.unwrap();
            }
            "a new state re-drives all three" => {
                let d = driver(&sink, t0, Opts::default());
                d.init().await;
                d.set(LedColour::Green).await;
                mark(&sink, "cleared");
                d.set(LedColour::Green).await;
            }
            "fail-safe blank writes all three" => {
                let fail = Arc::new(AtomicBool::new(false));
                let f = fail.clone();
                let d = driver(
                    &sink,
                    t0,
                    Opts {
                        ok: Box::new(move |_, _| !f.load(Ordering::SeqCst)),
                        ..Opts::default()
                    },
                );
                d.init().await;
                d.set(LedColour::Off).await;
                fail.store(true, Ordering::SeqCst);
                mark(&sink, "cleared");
                d.set(LedColour::Red).await;
            }
            "a failed write blanks all three, logged once" => {
                let d = driver(
                    &sink,
                    t0,
                    Opts {
                        ok: Box::new(|pins, _| !pins.contains(&21)),
                        ..Opts::default()
                    },
                );
                d.init().await;
                d.set(LedColour::White).await;
                mark(&sink, "firstSet");
                d.set(LedColour::White).await;
            }
            "a write failure aborts a running pattern" => {
                let fail = Arc::new(AtomicBool::new(false));
                let f = fail.clone();
                let d = driver(
                    &sink,
                    t0,
                    Opts {
                        ok: Box::new(move |_, _| !f.load(Ordering::SeqCst)),
                        ..Opts::default()
                    },
                );
                d.init().await;
                let running = tokio::spawn(d.play(LedColour::Green, LedPattern::BlinkFast));
                adv(60).await;
                fail.store(true, Ordering::SeqCst);
                while !running.is_finished() {
                    adv(1).await;
                }
                running.await.unwrap();
                mark(&sink, "after");
                adv(400).await;
            }
            "a throwing writer is survived" => {
                let d = driver(
                    &sink,
                    t0,
                    Opts {
                        panics: true,
                        ..Opts::default()
                    },
                );
                d.init().await;
                d.set(LedColour::Red).await;
                extra.insert("resolved".into(), true.into());
            }
            "inert when the probe fails: writes nothing" => {
                let d = driver(
                    &sink,
                    t0,
                    Opts {
                        probe_ok: false,
                        ..Opts::default()
                    },
                );
                let init = d.init().await;
                extra.insert("init".into(), init_json(&init));
                extra.insert("available".into(), d.available().into());
                d.set(LedColour::Red).await;
                d.play(LedColour::White, LedPattern::Flash2).await;
                d.blank(false).await;
            }
            "logs the resolved map exactly once" => {
                let d = driver(&sink, t0, Opts::default());
                let i1 = d.init().await;
                let i2 = d.init().await;
                extra.insert("init1".into(), init_json(&i1));
                extra.insert("init2".into(), init_json(&i2));
            }
            other => panic!("no Rust replay for fixture case '{other}'"),
        }
    }
    let calls = sink.calls.lock().unwrap().clone();
    let logs = sink.logs.lock().unwrap().clone();
    let marks = sink.marks.lock().unwrap().clone();
    Trace {
        calls,
        logs,
        marks,
        extra,
    }
}

/// Latest level per pin, as `{pin: 0|1}` (the TS `levels()` helper).
fn levels(calls: &[Call]) -> std::collections::BTreeMap<u32, u8> {
    let mut out = std::collections::BTreeMap::new();
    for c in calls {
        for p in &c.pins {
            out.insert(*p, c.high as u8);
        }
    }
    out
}

fn triple(c: LedColour) -> std::collections::BTreeMap<u32, u8> {
    let ch = c.channels();
    [(12, ch.r as u8), (21, ch.g as u8), (16, ch.b as u8)]
        .into_iter()
        .collect()
}

/// Per-pin writes `"<pin>=<0|1>"` in order (the TS `writes` array).
fn writes(calls: &[Call]) -> Vec<String> {
    calls
        .iter()
        .flat_map(|c| c.pins.iter().map(move |p| format!("{p}={}", c.high as u8)))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn every_led_driver_it_matches_the_ts_trace() {
    let fx = fixture();
    assert_eq!(fx.cases.len(), 20, "14 its, :72 expanded to 7 colours");
    for case in &fx.cases {
        let got = run_case(&case.name).await;
        let want = &case.expected;
        assert_eq!(
            got.calls, want.calls,
            "{} ({}): writer calls",
            case.name, case.ts
        );
        assert_eq!(got.logs, want.logs, "{} ({}): logs", case.name, case.ts);
        assert_eq!(got.marks, want.marks, "{} ({}): marks", case.name, case.ts);
        for (k, v) in &got.extra {
            assert_eq!(
                Some(v),
                want.extra.get(k),
                "{} ({}): extra {k}",
                case.name,
                case.ts
            );
        }

        // The original it()'s own assertions, on the Rust trace.
        let calls = &got.calls;
        let cstr: Vec<String> = calls
            .iter()
            .map(|c| {
                let p: Vec<String> = c.pins.iter().map(u32::to_string).collect();
                format!("{}={}", p.join(","), c.high as u8)
            })
            .collect();
        let mark = |k: &str| got.marks[k].as_u64().unwrap() as usize;
        match case.ts.as_str() {
            "led.test.ts:72" => {
                let c = LedColour::parse(case.name.split(' ').nth(1).unwrap()).unwrap();
                assert_eq!(levels(calls), triple(c));
            }
            "led.test.ts:81" => {
                assert_eq!(
                    levels(calls),
                    [(12, 0), (21, 1), (16, 1)].into_iter().collect()
                )
            }
            "led.test.ts:126" => assert_eq!(levels(calls), triple(LedColour::Off)),
            "led.test.ts:133" => assert_eq!(levels(calls), triple(LedColour::Red)),
            "led.test.ts:143" => {
                assert!(writes(calls).len() > 3);
                assert_eq!(levels(calls), triple(LedColour::Off));
            }
            "led.test.ts:157" => {
                let w = writes(calls);
                assert!(w.len() > 3 && w[3..].iter().all(|w| w.starts_with("21=")));
                assert_eq!(
                    cstr.iter()
                        .filter(|c| c.contains("12") || c.contains("16"))
                        .count(),
                    1
                );
                assert!(cstr[2..].iter().all(|c| c == "21=1" || c == "21=0"));
            }
            "led.test.ts:175" => {
                assert!(cstr.len() - 2 > 3);
                assert!(cstr[2..].iter().all(|c| c == "12,16=1" || c == "12,16=0"));
            }
            "led.test.ts:197" => {
                assert_eq!(writes(&calls[mark("cleared")..]).len(), 3);
                assert_eq!(levels(calls), triple(LedColour::Green));
            }
            "led.test.ts:209" => {
                let w = writes(calls);
                assert_eq!(w[w.len() - 3..], ["12=0", "21=0", "16=0"]);
            }
            "led.test.ts:280" => {
                let w = writes(calls);
                assert_eq!(w[w.len() - 3..], ["12=0", "21=0", "16=0"]);
                assert_eq!(
                    got.logs.iter().filter(|l| l.msg.contains("failed")).count(),
                    1
                );
            }
            "led.test.ts:304" => {
                assert_eq!(calls.len(), mark("after"), "nothing kept toggling");
                let w = writes(calls);
                assert_eq!(w[w.len() - 3..], ["12=0", "21=0", "16=0"]);
            }
            "led.test.ts:327" => {}
            "led.test.ts:342" => {
                assert!(calls.is_empty());
                assert_eq!(got.extra["available"], false);
            }
            "led.test.ts:362" => {
                assert_eq!(got.logs.len(), 1);
                for s in ["r=GPIO12", "g=GPIO21", "b=GPIO16"] {
                    assert!(got.logs[0].msg.contains(s));
                }
            }
            other => panic!("unexpected ts ref {other}"),
        }
    }
}

// ---------------- probe_host + the CF50 [used] adjustment ----------------

struct FakeHost {
    mock: bool,
    pi: bool,
    pinctrl: bool,
    gpioinfo: Option<Option<String>>, // None = not installed; Some(None) = run failed
}

impl HostProbe for FakeHost {
    fn is_mock(&self) -> bool {
        self.mock
    }
    fn is_raspberry_pi(&self) -> bool {
        self.pi
    }
    fn command_exists(&self, cmd: &str) -> bool {
        match cmd {
            "pinctrl" => self.pinctrl,
            "gpioinfo" => self.gpioinfo.is_some(),
            _ => false,
        }
    }
    fn gpioinfo(&self) -> BoxFut<'_, Option<String>> {
        let out = self.gpioinfo.clone().flatten();
        Box::pin(async move { out })
    }
}

fn host(gpioinfo: Option<Option<&str>>) -> FakeHost {
    FakeHost {
        mock: false,
        pi: true,
        pinctrl: true,
        gpioinfo: gpioinfo.map(|o| o.map(str::to_string)),
    }
}

/// libgpiod 1.6 on the bench (`gpioinfo gpiochip0`), Argus lines only.
const OWN_16: &str = "\tline  12:      unnamed  \"xorgate-rs\"  output  active-high [used]\n\tline  16:      unnamed  \"xorgate-rs\"  output  active-high [used]\n\tline  21:      unnamed  \"xorgate-rs\"  output  active-high [used]";
const FOREIGN_16: &str = "\tline  12:      \"GPIO12\"       unused   input  active-high \n\tline  16:      \"GPIO16\"       \"sdmode\"  output  active-high [used]\n\tline  21:      \"GPIO21\"       unused   input  active-high ";
const MIXED_16: &str = "\tline  12:      unnamed  \"xorgate-rs\"  output  active-high [used]\n\tline  16:      unnamed  \"spi0 CS0\"  output  active-low [used]\n\tline  21:      unnamed  \"xorgate-rs\"  output  active-high [used]";
/// libgpiod 2.x spelling (no `[used]`; the TS parser sees nothing busy).
const OWN_V2: &str = "\tline  12:\t\"GPIO12\"\toutput consumer=\"xorgate-rs\"\n\tline  16:\t\"GPIO16\"\toutput consumer=\"xorgate-rs\"";

#[tokio::test]
async fn probe_host_order_and_reasons_are_the_ts() {
    let map = LedPinMap::status_led_map();
    let fail = |r: &str| PinMapCheck {
        ok: false,
        reason: Some(r.to_string()),
    };
    let ok = PinMapCheck {
        ok: true,
        reason: None,
    };
    let mut bad = map.clone();
    bad.kind = "pwm-rgb".into();
    assert_eq!(
        probe_host_with(
            &bad,
            &FakeHost {
                mock: true,
                ..host(None)
            }
        )
        .await,
        fail("unimplemented LED kind 'pwm-rgb'"),
        "shape first"
    );
    assert_eq!(
        probe_host_with(
            &map,
            &FakeHost {
                mock: true,
                pi: false,
                ..host(None)
            }
        )
        .await,
        fail("mock mode (XORGATE_MOCK)")
    );
    assert_eq!(
        probe_host_with(
            &map,
            &FakeHost {
                pi: false,
                pinctrl: false,
                ..host(None)
            }
        )
        .await,
        fail("not a Raspberry Pi")
    );
    assert_eq!(
        probe_host_with(
            &map,
            &FakeHost {
                pinctrl: false,
                ..host(Some(Some(FOREIGN_16)))
            }
        )
        .await,
        fail("pinctrl is not installed")
    );
    assert_eq!(
        probe_host_with(&map, &host(None)).await,
        ok,
        "no gpioinfo = skip"
    );
    assert_eq!(
        probe_host_with(&map, &host(Some(None))).await,
        ok,
        "gpioinfo failed = skip"
    );
    assert_eq!(
        probe_host_with(&map, &host(Some(Some(FOREIGN_16)))).await,
        fail("b=GPIO16 already held by another driver (gpioinfo: [used])"),
        "a foreign consumer still refuses, TS text verbatim"
    );
}

#[tokio::test]
async fn our_own_consumer_is_not_busy_but_a_foreign_one_still_is() {
    let map = LedPinMap::status_led_map();
    assert_eq!(LED_CONSUMER, "xorgate-rs");
    assert_eq!(
        probe_host_with(&map, &host(Some(Some(OWN_16)))).await,
        PinMapCheck {
            ok: true,
            reason: None
        },
        "lines held by the agent's own request (bench gpioinfo while it runs)"
    );
    assert_eq!(
        probe_host_with(&map, &host(Some(Some(MIXED_16)))).await,
        PinMapCheck {
            ok: false,
            reason: Some("b=GPIO16 already held by another driver (gpioinfo: [used])".into())
        },
        "own lines excused, the foreign one still named"
    );
    assert_eq!(strip_own_consumer(OWN_16), "");
    assert_eq!(strip_own_consumer(FOREIGN_16), FOREIGN_16);
    assert_eq!(strip_own_consumer(OWN_V2), "");
    // A consumer that merely CONTAINS our label is foreign.
    let lookalike = "\tline  12:      unnamed  \"xorgate-rs2\"  output  active-high [used]";
    assert_eq!(strip_own_consumer(lookalike), lookalike);
    let named_own = "\tline  12:      \"LED R\"  \"xorgate-rs\"  output  active-high [used]";
    assert_eq!(strip_own_consumer(named_own), "");
}

// ---------------------------------------------------------------------------
// D31 (review-wp1 M1): supersession before vs after the play task is polled.
// `fixtures/led_driver/led-supersede.json` = the reviewer's TS dump
// (`phase-4/evidence/wp1-tools/zz-review-supersede.test.ts`, REAL LedDriver
// @ 89b5c64, writer calls `{t, pins, high}` from one shared t0).

#[derive(Deserialize)]
struct SupersedeFixture {
    cases: Vec<SupersedeCase>,
}

#[derive(Deserialize)]
struct SupersedeCase {
    expected: SupersedeTrace,
}

#[derive(Deserialize)]
struct SupersedeTrace {
    calls: Vec<Call>,
}

/// Lazy writer (the [`PinWriter`] contract): records the CALL at call time
/// (what the TS writer records — the TS spawns `pinctrl` synchronously) and
/// the WRITE when the future is first polled (what reaches the pins and the
/// rig's `pinctrl` shim). Resolves `true` after `delay_ms`.
struct RunRec {
    t0: Instant,
    delay_ms: u64,
    calls: Arc<Mutex<Vec<Call>>>,
    ran: Arc<Mutex<Vec<Call>>>,
}

impl PinWriter for RunRec {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        let call = Call {
            t: ms_since(self.t0),
            pins: pins.to_vec(),
            high,
        };
        self.calls.lock().unwrap().push(call);
        let (t0, delay, ran, pins) = (self.t0, self.delay_ms, self.ran.clone(), pins.to_vec());
        Box::pin(async move {
            ran.lock().unwrap().push(Call {
                t: ms_since(t0),
                pins,
                high,
            });
            if delay > 0 {
                adv(delay).await;
            }
            true
        })
    }
}

#[derive(Default)]
struct GpioTape(Mutex<Vec<Call>>, Mutex<Option<Instant>>);
impl TapeSink for GpioTape {
    fn gpio(&self, pins: &[u32], high: bool, _via: GpioVia) {
        let t0 = self.1.lock().unwrap().expect("t0 set");
        self.0.lock().unwrap().push(Call {
            t: ms_since(t0),
            pins: pins.to_vec(),
            high,
        });
    }
}

struct Rig {
    driver: LedDriver,
    calls: Arc<Mutex<Vec<Call>>>,
    ran: Arc<Mutex<Vec<Call>>>,
    tape: Arc<GpioTape>,
}

fn supersede_rig(t0: Instant, delay_ms: u64) -> Rig {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let ran = Arc::new(Mutex::new(Vec::new()));
    let tape = Arc::new(GpioTape::default());
    *tape.1.lock().unwrap() = Some(t0);
    let driver = LedDriver::new(LedDriverOptions {
        map: Some(LedPinMap::status_led_map()),
        writer: Some(Arc::new(TapedWriter {
            inner: Arc::new(RunRec {
                t0,
                delay_ms,
                calls: calls.clone(),
                ran: ran.clone(),
            }),
            tape: tape.clone(),
        })),
        probe: Some(Box::new(|| {
            Box::pin(async {
                PinMapCheck {
                    ok: true,
                    reason: None,
                }
            })
        })),
        log: Some(Box::new(|_| {})),
    });
    Rig {
        driver,
        calls,
        ran,
        tape,
    }
}

fn c(t: u64, pins: &[u32], high: bool) -> Call {
    Call {
        t,
        pins: pins.to_vec(),
        high,
    }
}

/// D31 pinning test: the reviewer's exact TS scenario, both cases.
/// - Immediate (`void play(red, blink-fast); void play(green, solid)` in one
///   tick): the TS finishes red's first step after green's rise and ends
///   DARK; Rust drops red's unpolled first step — its rise is CALLED (TS
///   synchronous prefix) but never RUNS, is not taped, and its fall group is
///   never called — and ends GREEN. This is the accepted divergence.
/// - Running (green heartbeat polled, superseded 2 ms later while its 5 ms
///   rise is in flight): the step finishes as in the TS — byte-equal calls.
#[tokio::test(start_paused = true)]
async fn d31_superseded_first_step_is_dropped_only_before_its_task_runs() {
    let p = format!(
        "{}/../../fixtures/led_driver/led-supersede.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let fx: SupersedeFixture = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    assert_eq!(fx.cases.len(), 2);
    let t0 = Instant::now();

    // Immediate supersede.
    let ts = &fx.cases[0].expected.calls;
    assert_eq!(
        *ts,
        vec![
            c(0, &[12], true),
            c(0, &[21], true),
            c(0, &[21, 16], false),
            c(0, &[12, 16], false),
        ],
        "TS trace (fixture)"
    );
    assert_eq!(levels(ts), triple(LedColour::Off), "the TS race ends DARK");
    let r = supersede_rig(t0, 0);
    r.driver.init().await;
    tokio::spawn(r.driver.play(LedColour::Red, LedPattern::BlinkFast));
    tokio::spawn(r.driver.play(LedColour::Green, LedPattern::Solid));
    adv(50).await;
    let calls = r.calls.lock().unwrap().clone();
    let ran = r.ran.lock().unwrap().clone();
    let taped = r.tape.0.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![c(0, &[12], true), c(0, &[21], true), c(0, &[12, 16], false)],
        "Rust writer calls: red's rise called, its fall never"
    );
    assert_eq!(
        ran,
        vec![c(0, &[21], true), c(0, &[12, 16], false)],
        "only green's writes run"
    );
    assert_eq!(taped, ran, "the tape records exactly the writes that ran");
    assert_eq!(levels(&ran), triple(LedColour::Green), "Rust ends green");

    // Running supersede: parity.
    let ts = &fx.cases[1].expected.calls;
    let r = supersede_rig(t0, 5);
    r.driver.init().await;
    tokio::spawn(r.driver.play(LedColour::Green, LedPattern::Heartbeat));
    adv(2).await;
    tokio::spawn(r.driver.play(LedColour::White, LedPattern::Flash2));
    adv(50).await;
    let calls = r.calls.lock().unwrap().clone();
    let ran = r.ran.lock().unwrap().clone();
    let taped = r.tape.0.lock().unwrap().clone();
    assert_eq!(
        calls, *ts,
        "a running superseded step finishes as in the TS"
    );
    assert_eq!(ran, calls, "every called write ran");
    assert_eq!(taped, ran);
}
