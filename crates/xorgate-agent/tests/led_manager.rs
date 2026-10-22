//! `LedManager` against the REAL TS manager (`src/agent/led-manager.ts`
//! @ 09da38d since Phase 5b WP3 — the 89b5c64 it "never calls the signal
//! function when inert" was replaced by the five telemetry its
//! `led-manager.test.ts:254-321`; the other 11 traces re-dumped byte-identical):
//! `fixtures/led_manager/led-manager.json` holds the 16 it()s of
//! `test/led-manager.test.ts` re-run step by step under vitest fake timers
//! (each `settle(N)` = advance N virtual ms) with a real `LedDriver` over a
//! recording writer — every writer call `{t, pins, high}` and every manager
//! log line `{t, msg}`. Each case here replays the same steps under
//! `tokio::time::pause()` and must produce the identical trace, then
//! re-asserts what the original it() asserts.
//!
//! Signal values: the TS test's `gnss.sky: "open"` is not a `SkyState`
//! member (the LED ladder never reads `sky`); it maps to `Clear` here.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::time::Instant;
use xorgate_agent::ops::led::{LedManager, LedSignalsOwned, ReadLteFn};
use xorgate_core::led::PinMapCheck;
use xorgate_core::led_state::LteStatusView;
use xorgate_core::telemetry::{ConnectionState, LiveBlock, RecordingStatus};
use xorgate_core::types::{GnssView, SkyState};
use xorgate_hal::led_driver::{BoxFut, LedDriver, LedDriverOptions, PinWriter};

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
        "{}/../../fixtures/led_manager/led-manager.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn ms_since(t0: Instant) -> u64 {
    Instant::now().duration_since(t0).as_millis() as u64
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
}

impl PinWriter for Rec {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        self.sink.calls.lock().unwrap().push(Call {
            t: ms_since(self.t0),
            pins: pins.to_vec(),
            high,
        });
        Box::pin(async { true })
    }
}

/// The TS `LedSignals` the harness's `signals()` returns (with `lte`).
#[derive(Clone)]
struct Sig {
    connection: ConnectionState,
    recording: RecordingStatus,
    lte: Option<LteStatusView>,
    gnss: Option<GnssView>,
    starting: bool,
}

fn recording() -> RecordingStatus {
    RecordingStatus {
        enabled: true,
        session_id: Some("s1".into()),
        backlog_bytes: 0,
        backlog_segments: 0,
        dropped_segments: 0,
        uploading: false,
        last_upload_at: None,
        boot_defer: None,
        power_shed: None,
        clock_hold: false,
        live_block: None,
    }
}

fn healthy() -> Sig {
    Sig {
        connection: ConnectionState::Connected,
        recording: recording(),
        lte: Some(LteStatusView {
            status: Some("connected".into()),
        }),
        gnss: Some(GnssView {
            fix: true,
            sky: SkyState::Clear,
        }),
        starting: false,
    }
}

type SigSource = Arc<dyn Fn() -> Sig + Send + Sync>;

struct Harness {
    led: LedManager,
    sink: Arc<Sink>,
    signal_calls: Arc<AtomicUsize>,
    lte_reads: Arc<AtomicUsize>,
}

/// `harness(signals, ok, readLte?)` (`led-manager.test.ts:33-67`).
fn harness(t0: Instant, signals: SigSource, ok: bool, read_lte: Option<ReadLteFn>) -> Harness {
    let sink = Arc::new(Sink::default());
    let driver = LedDriver::new(LedDriverOptions {
        map: None,
        writer: Some(Arc::new(Rec {
            sink: sink.clone(),
            t0,
        })),
        probe: Some(Box::new(move || {
            Box::pin(async move {
                if ok {
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
        log: Some(Box::new(|_| {})),
    });
    let signal_calls = Arc::new(AtomicUsize::new(0));
    let lte_reads = Arc::new(AtomicUsize::new(0));
    let (sc, s1) = (signal_calls.clone(), signals.clone());
    let lr = lte_reads.clone();
    let default_read: ReadLteFn = {
        let s2 = signals.clone();
        Box::new(move || {
            let lte = s2().lte;
            Box::pin(async move { Ok(lte) })
        })
    };
    let read = read_lte.unwrap_or(default_read);
    let logs = sink.clone();
    let led = LedManager::new(
        driver,
        Box::new(move || {
            sc.fetch_add(1, Ordering::SeqCst);
            let s = s1();
            LedSignalsOwned {
                connection: s.connection,
                recording: s.recording,
                gnss: s.gnss,
                starting: s.starting,
            }
        }),
        Box::new(move || {
            lr.fetch_add(1, Ordering::SeqCst);
            read()
        }),
        Some(Box::new(move |m| {
            logs.logs.lock().unwrap().push(Log {
                t: ms_since(t0),
                msg: m.to_string(),
            })
        })),
    );
    Harness {
        led,
        sink,
        signal_calls,
        lte_reads,
    }
}

async fn settle(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn mark(h: &Harness, k: &str) {
    let n = h.sink.calls.lock().unwrap().len();
    h.sink.marks.lock().unwrap().insert(k.into(), n.into());
}

/// A mutable signal source (the TS `let signals = …` reassigned mid-test).
fn cell(init: Sig) -> (Arc<Mutex<Sig>>, SigSource) {
    let c = Arc::new(Mutex::new(init));
    let c2 = c.clone();
    (c, Arc::new(move || c2.lock().unwrap().clone()))
}

fn dead_zone_reader(reads: Arc<AtomicUsize>) -> ReadLteFn {
    Box::new(move || {
        reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(Some(LteStatusView {
                status: Some("dead_zone".into()),
            }))
        })
    })
}

/// `led.telemetry()` as the dump records it (`null` before the first tick).
fn tel(led: &LedManager) -> Value {
    serde_json::to_value(led.telemetry()).unwrap()
}

/// `led.snapshot()?.state.reason ?? null`.
fn reason(led: &LedManager) -> Value {
    led.snapshot()
        .map(|s| Value::from(s.state.reason))
        .unwrap_or(Value::Null)
}

/// `h.counts()`: signal calls + LTE reads so far.
fn counts(h: &Harness) -> Value {
    serde_json::json!({
        "signalCalls": h.signal_calls.load(Ordering::SeqCst),
        "lteReads": h.lte_reads.load(Ordering::SeqCst),
    })
}

async fn run_case(name: &str) -> Trace {
    let t0 = Instant::now();
    let mut extra = serde_json::Map::new();
    let closed = || Sig {
        connection: ConnectionState::Closed,
        ..healthy()
    };
    let h = match name {
        "shows the starting rung as soon as it takes over" => {
            let h = harness(t0, Arc::new(healthy), true, None);
            h.led.start().await;
            settle(20).await;
            h
        }
        "re-renders only when the resolved state actually changes" => {
            let (c, src) = cell(healthy());
            let h = harness(t0, src, true, None);
            h.led.start().await;
            settle(20).await;
            h.led.tick();
            settle(20).await;
            mark(&h, "afterFirst");
            h.led.tick();
            h.led.tick();
            h.led.tick();
            mark(&h, "afterThree");
            *c.lock().unwrap() = closed();
            h.led.tick();
            settle(20).await;
            h
        }
        "drives the colour the ladder picked for the signals" => {
            let (c, src) = cell(Sig {
                gnss: Some(GnssView {
                    fix: false,
                    sky: SkyState::Clear,
                }),
                ..healthy()
            });
            let h = harness(t0, src, true, None);
            h.led.start().await;
            settle(20).await;
            mark(&h, "cleared1");
            h.led.tick();
            settle(20).await;
            let mut rec = recording();
            rec.live_block = Some(LiveBlock {
                reason: "thermal".into(),
                detail: String::new(),
                since: 0,
            });
            *c.lock().unwrap() = Sig {
                recording: rec,
                ..healthy()
            };
            mark(&h, "cleared2");
            h.led.tick();
            settle(20).await;
            h
        }
        "flashes white for an applied config, then returns to the real state" => {
            let h = harness(t0, Arc::new(healthy), true, None);
            h.led.start().await;
            settle(20).await;
            h.led.tick();
            settle(20).await;
            mark(&h, "cleared");
            h.led.config_applied();
            settle(40).await;
            mark(&h, "at40");
            settle(700).await;
            h
        }
        "ignores ticks while the overlay is running" => {
            let (c, src) = cell(healthy());
            let h = harness(t0, src, true, None);
            h.led.start().await;
            settle(20).await;
            h.led.config_applied();
            *c.lock().unwrap() = closed();
            h.led.tick();
            settle(40).await;
            settle(700).await;
            h
        }
        "holds its current state when signal gathering throws" => {
            let boom = Arc::new(AtomicBool::new(false));
            let b = boom.clone();
            let h = harness(
                t0,
                Arc::new(move || {
                    if b.load(Ordering::SeqCst) {
                        panic!("collector exploded");
                    }
                    healthy()
                }),
                true,
                None,
            );
            h.led.start().await;
            h.led.tick();
            settle(20).await;
            boom.store(true, Ordering::SeqCst);
            let threw =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.led.tick())).is_err();
            extra.insert("threw".into(), threw.into());
            h
        }
        "blanks on stop and ignores everything afterwards" => {
            let h = harness(t0, Arc::new(healthy), true, None);
            h.led.start().await;
            settle(20).await;
            mark(&h, "cleared1");
            h.led.stop();
            settle(20).await;
            mark(&h, "cleared2");
            h.led.tick();
            h.led.config_applied();
            settle(40).await;
            h
        }
        "is a total no-op when the driver is inert" => {
            let h = harness(t0, Arc::new(healthy), false, None);
            h.led.start().await;
            h.led.tick();
            h.led.config_applied();
            h.led.stop();
            settle(40).await;
            extra.insert("available".into(), h.led.available().into());
            h
        }
        "reads LTE status for itself, not via the publish loop" => {
            let reads = Arc::new(AtomicUsize::new(0));
            let h = harness(
                t0,
                Arc::new(closed),
                true,
                Some(dead_zone_reader(reads.clone())),
            );
            h.led.start().await;
            h.led.tick();
            settle(30).await;
            extra.insert(
                "readsAfterFirst".into(),
                reads.load(Ordering::SeqCst).into(),
            );
            settle(1100).await;
            h.led.tick();
            settle(30).await;
            h.led.tick();
            settle(30).await;
            extra.insert("readsEnd".into(), reads.load(Ordering::SeqCst).into());
            h
        }
        "a dead zone with the cloud down stays green, not purple" => {
            let h = harness(
                t0,
                Arc::new(closed),
                true,
                Some(dead_zone_reader(Arc::new(AtomicUsize::new(0)))),
            );
            h.led.start().await;
            h.led.tick();
            settle(30).await;
            h.led.tick();
            settle(30).await;
            h
        }
        "an unreadable LTE file fails open" => {
            let h = harness(
                t0,
                Arc::new(healthy),
                true,
                Some(Box::new(|| Box::pin(async { Err("ENOENT".to_string()) }))),
            );
            h.led.start().await;
            h.led.tick();
            settle(30).await;
            h.led.tick();
            settle(30).await;
            h
        }
        // ---- 09da38d (Phase 5b WP3): resolve on every host ----
        "still resolves the state when inert, for telemetry — but never touches a pin" => {
            let unfixed = Sig {
                gnss: Some(GnssView {
                    fix: false,
                    sky: SkyState::Clear,
                }),
                ..healthy()
            };
            let (c, src) = cell(unfixed);
            let h = harness(t0, src, false, None);
            h.led.start().await;
            extra.insert("before".into(), tel(&h.led));
            h.led.tick();
            settle(20).await;
            extra.insert("reason1".into(), reason(&h.led));
            extra.insert("t1".into(), tel(&h.led));
            *c.lock().unwrap() = closed();
            h.led.tick();
            settle(20).await;
            extra.insert("t2".into(), tel(&h.led));
            extra.insert("counts".into(), counts(&h));
            h
        }
        "telemetry carries the LTE status the manager read for itself" => {
            let h = harness(
                t0,
                Arc::new(healthy),
                false,
                Some(Box::new(|| {
                    Box::pin(async {
                        Ok(Some(LteStatusView {
                            status: Some("disconnected".into()),
                        }))
                    })
                })),
            );
            h.led.start().await;
            h.led.tick();
            settle(30).await;
            h.led.tick();
            extra.insert("t".into(), tel(&h.led));
            h
        }
        "the config-applied overlay never shows in telemetry; the state underneath does" => {
            let (c, src) = cell(healthy());
            let h = harness(t0, src, true, None);
            h.led.start().await;
            settle(20).await;
            h.led.tick();
            extra.insert("t0".into(), tel(&h.led));
            h.led.config_applied();
            *c.lock().unwrap() = closed();
            h.led.tick();
            extra.insert("reason1".into(), reason(&h.led));
            extra.insert("t1".into(), tel(&h.led));
            settle(700).await;
            h
        }
        "holds the last resolved telemetry when signal gathering throws" => {
            let boom = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let b = boom.clone();
            let h = harness(
                t0,
                Arc::new(move || {
                    if b.load(Ordering::SeqCst) {
                        panic!("collector exploded");
                    }
                    Sig {
                        gnss: Some(GnssView {
                            fix: false,
                            sky: SkyState::Clear,
                        }),
                        ..healthy()
                    }
                }),
                false,
                None,
            );
            h.led.start().await;
            h.led.tick();
            boom.store(true, Ordering::SeqCst);
            h.led.tick();
            extra.insert("t".into(), tel(&h.led));
            h
        }
        "stops resolving after stop()" => {
            let h = harness(t0, Arc::new(healthy), false, None);
            h.led.start().await;
            h.led.stop();
            h.led.tick();
            extra.insert("counts".into(), counts(&h));
            extra.insert("t".into(), tel(&h.led));
            h
        }
        other => panic!("no Rust replay for fixture case '{other}'"),
    };
    // Stop any pattern still running so the next case starts clean.
    h.led.driver().stop();
    let calls = h.sink.calls.lock().unwrap().clone();
    let logs = h.sink.logs.lock().unwrap().clone();
    let marks = h.sink.marks.lock().unwrap().clone();
    Trace {
        calls,
        logs,
        marks,
        extra,
    }
}

fn levels(calls: &[Call]) -> std::collections::BTreeMap<u32, u8> {
    let mut out = std::collections::BTreeMap::new();
    for c in calls {
        for p in &c.pins {
            out.insert(*p, c.high as u8);
        }
    }
    out
}

fn lv(r: u8, g: u8, b: u8) -> std::collections::BTreeMap<u32, u8> {
    [(12, r), (21, g), (16, b)].into_iter().collect()
}

#[tokio::test(start_paused = true)]
async fn every_led_manager_it_matches_the_ts_trace() {
    let fx = fixture();
    assert_eq!(fx.cases.len(), 16);
    for case in &fx.cases {
        let got = run_case(&case.name).await;
        let want = &case.expected;
        let mut want_calls = want.calls.clone();
        if case.ts == "led-manager.test.ts:282" {
            // D31 (accepted, review-wp1 M1): the it() renders healthy and
            // fires the overlay in ONE tick, so the heartbeat's first step is
            // superseded before its task runs. The TS still writes that
            // step's fall group ([12,16] low) after the flash's white rise —
            // the flash shows GREEN — and the flash's own fall then lowers
            // only [21]. Rust drops the stale fall (white flash), and the
            // flash's fall lowers all three. Same logs, same telemetry, both
            // dark at 170 ms.
            let stale = Call {
                t: 20,
                pins: vec![12, 16],
                high: false,
            };
            let i = want_calls.iter().position(|c| *c == stale).expect("D31 stale fall");
            want_calls.remove(i);
            let f = want_calls
                .iter_mut()
                .find(|c| c.t == 170 && c.pins == [21] && !c.high)
                .expect("TS flash fall");
            f.pins = vec![12, 21, 16];
        }
        assert_eq!(
            got.calls, want_calls,
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
        let mark = |k: &str| got.marks[k].as_u64().unwrap() as usize;
        let logged = |s: &str| got.logs.iter().any(|l| l.msg.contains(s));
        match case.ts.as_str() {
            "led-manager.test.ts:73" => assert_eq!(levels(calls), lv(1, 1, 1)),
            "led-manager.test.ts:80" => {
                assert!(logged("→ healthy"));
                assert_eq!(mark("afterThree"), mark("afterFirst"));
                assert!(calls.len() > mark("afterFirst"));
                assert!(logged("healthy → network"));
            }
            "led-manager.test.ts:105" => {
                assert_eq!(
                    levels(&calls[mark("cleared1")..mark("cleared2")]),
                    lv(0, 0, 1)
                );
                assert_eq!(levels(&calls[mark("cleared2")..]), lv(0, 1, 1));
            }
            "led-manager.test.ts:124" => {
                assert_eq!(levels(&calls[mark("cleared")..mark("at40")]), lv(1, 1, 1));
                assert!(calls[mark("at40")..]
                    .iter()
                    .any(|c| c.pins == [21] && c.high));
                assert!(logged("→ healthy"));
            }
            "led-manager.test.ts:144" => {}
            "led-manager.test.ts:158" => {
                assert_eq!(got.extra["threw"], false);
                assert!(logged("signal read failed"));
            }
            "led-manager.test.ts:172" => {
                assert_eq!(
                    levels(&calls[mark("cleared1")..mark("cleared2")]),
                    lv(0, 0, 0)
                );
                assert_eq!(calls.len(), mark("cleared2"));
            }
            "led-manager.test.ts:188" => {
                assert!(calls.is_empty());
                assert_eq!(got.extra["available"], false);
                assert!(logged("inert"));
            }
            "led-manager.test.ts:200" => {
                assert!(got.extra["readsAfterFirst"].as_u64().unwrap() > 0);
                assert!(got.extra["readsEnd"].as_u64().unwrap() > 1);
            }
            "led-manager.test.ts:229" => {
                assert!(logged("→ healthy"));
                assert!(!logged("→ network"));
            }
            "led-manager.test.ts:242" => assert!(logged("→ healthy")),
            // 09da38d: the state resolves on every host, the pins stay put.
            "led-manager.test.ts:254" => {
                assert!(calls.is_empty());
                assert_eq!(got.extra["before"], Value::Null);
                assert_eq!(got.extra["reason1"], "gps-wait");
                assert_eq!(got.extra["t1"], serde_json::json!({"led_state": 2, "health_flags": 16}));
                assert_eq!(got.extra["t2"], serde_json::json!({"led_state": 3, "health_flags": 8}));
            }
            "led-manager.test.ts:273" => {
                assert_eq!(got.extra["t"], serde_json::json!({"led_state": 3, "health_flags": 4}));
            }
            "led-manager.test.ts:282" => {
                assert_eq!(got.extra["t0"], serde_json::json!({"led_state": 0, "health_flags": 0}));
                assert_eq!(got.extra["reason1"], "network");
                assert_eq!(got.extra["t1"], serde_json::json!({"led_state": 3, "health_flags": 8}));
            }
            "led-manager.test.ts:299" => {
                assert_eq!(got.extra["t"], serde_json::json!({"led_state": 2, "health_flags": 16}));
                assert!(logged("signal read failed"));
            }
            "led-manager.test.ts:312" => {
                assert_eq!(got.extra["counts"]["signalCalls"], 0);
                assert_eq!(got.extra["t"], Value::Null);
            }
            other => panic!("unexpected ts ref {other}"),
        }
    }
}

/// CF59 D-row pin: the LTE re-read is throttled on the MONOTONIC clock — a
/// 1 s heartbeat re-reads once a second, regardless of the wall clock.
#[tokio::test(start_paused = true)]
async fn lte_refresh_is_throttled_on_mono_time() {
    let t0 = Instant::now();
    let reads = Arc::new(AtomicUsize::new(0));
    let h = harness(
        t0,
        Arc::new(healthy),
        true,
        Some(dead_zone_reader(reads.clone())),
    );
    h.led.start().await;
    assert_eq!(reads.load(Ordering::SeqCst), 1, "start primes one read");
    settle(999).await;
    h.led.tick();
    settle(10).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "999 ms after the last read: throttled"
    );
    for _ in 0..5 {
        settle(1000).await;
        h.led.tick();
    }
    settle(10).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        6,
        "one read per >= 1000 ms tick"
    );
    h.led.driver().stop();
}

/// review-wp1 S1: `tick`/`config_applied`/`stop` called with no tokio
/// runtime current (a `Drop` or sync signal path) must not panic; they log
/// and back out (`stop` still latches `stopped` and cancels the pattern).
#[tokio::test(start_paused = true)]
async fn manager_methods_outside_the_runtime_do_not_panic() {
    let t0 = Instant::now();
    let (_c, sig) = cell(healthy());
    let h = harness(t0, sig, true, None);
    h.led.start().await;
    settle(50).await;
    let before = h.sink.calls.lock().unwrap().len();
    assert!(before > 0, "start drew `starting`");
    let led = h.led.clone();
    std::thread::spawn(move || {
        led.tick();
        led.config_applied();
        led.stop();
    })
    .join()
    .expect("no panic outside the runtime");
    settle(2000).await;
    assert_eq!(
        h.sink.calls.lock().unwrap().len(),
        before,
        "nothing written: tick/config_applied ignored, stop's blank skipped"
    );
    h.led.tick();
    settle(50).await;
    assert_eq!(
        h.sink.calls.lock().unwrap().len(),
        before,
        "stop() still latched `stopped`"
    );
}
