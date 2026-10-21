//! `xorgate-rs led …` against the REAL TS `ledCommand` (`src/commands/led.ts`
//! @ 89b5c64) over the real `LedDriver` + `probeHost`:
//! `fixtures/led_driver/led-cli.json` (vitest fake timers; only
//! `isRaspberryPi` and `exec.run`/`commandExists` faked) — stdout/stderr
//! lines, exit code and the full `pinctrl` / `gpioinfo` call timeline in
//! virtual ms, for every verb incl. `led test` (113 calls over 44.1 s), the
//! hooks × `SERVICE_RESULT`, the inert reasons, a failing `pinctrl`, and
//! SIGINT mid-pattern. Replayed here under `tokio::time::pause()` with the
//! same fakes (a recording `PinWriter` emitting `PinctrlWriter::args`, a
//! fake `HostProbe`).
//!
//! Deliberate differences, applied to the TS expectation before comparing:
//! - Q9: local-only hints name this binary — `xorgate led` → `xorgate-rs led`
//!   (usage block + the `stays on until '…'` hint).
//! - CF50: lines held by the agent's own consumer `xorgate-rs` are NOT busy
//!   (the TS refuses them): that one case expects the free-chip result.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::time::Instant;
use xorgate_agent::cli::led::{run, CliOut, LedCliDeps};
use xorgate_core::led::LedPinMap;
use xorgate_hal::led_driver::{
    is_mock_from, probe_host_with, BoxFut, HostProbe, LedDriver, LedDriverOptions, PinWriter,
    PinctrlWriter,
};

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
#[serde(rename_all = "camelCase")]
struct Input {
    args: Vec<String>,
    host: HostIn,
    env: serde_json::Map<String, Value>,
    signal_at_ms: Option<u64>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
struct HostIn {
    pi: Option<bool>,
    pinctrl: Option<bool>,
    gpioinfo: Option<String>,
    gpioinfo_fails: Option<bool>,
    pinctrl_fails: Option<bool>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Expected {
    lines: Vec<Line>,
    exit_code: i32,
    calls: Vec<CallRec>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
struct Line {
    fd: u8,
    text: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
struct CallRec {
    t: u64,
    argv: Vec<String>,
}

fn fixture() -> Fixture {
    let p = format!(
        "{}/../../fixtures/led_driver/led-cli.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

type Calls = Arc<Mutex<Vec<CallRec>>>;

fn ms_since(t0: Instant) -> u64 {
    Instant::now().duration_since(t0).as_millis() as u64
}

struct FakePinctrl {
    calls: Calls,
    t0: Instant,
    fails: bool,
}

impl PinWriter for FakePinctrl {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        let mut argv = vec!["pinctrl".to_string()];
        argv.extend(PinctrlWriter::args(pins, high));
        self.calls.lock().unwrap().push(CallRec {
            t: ms_since(self.t0),
            argv,
        });
        let ok = !self.fails;
        Box::pin(async move { ok })
    }
}

struct FakeHost {
    h: HostIn,
    mock: bool,
    calls: Calls,
    t0: Instant,
}

impl HostProbe for FakeHost {
    fn is_mock(&self) -> bool {
        self.mock
    }
    fn is_raspberry_pi(&self) -> bool {
        self.h.pi.unwrap_or(true)
    }
    fn command_exists(&self, cmd: &str) -> bool {
        match cmd {
            "pinctrl" => self.h.pinctrl.unwrap_or(true),
            "gpioinfo" => self.h.gpioinfo.is_some(),
            _ => false,
        }
    }
    fn gpioinfo(&self) -> BoxFut<'_, Option<String>> {
        self.calls.lock().unwrap().push(CallRec {
            t: ms_since(self.t0),
            argv: vec!["gpioinfo".into(), "gpiochip0".into()],
        });
        let out = if self.h.gpioinfo_fails.unwrap_or(false) {
            None
        } else {
            self.h.gpioinfo.clone()
        };
        Box::pin(async move { out })
    }
}

#[derive(Default)]
struct Out(Mutex<Vec<Line>>);

impl CliOut for Out {
    fn line(&self, fd: u8, text: &str) {
        self.0.lock().unwrap().push(Line {
            fd,
            text: text.to_string(),
        });
    }
}

fn env_str(env: &serde_json::Map<String, Value>, k: &str) -> Option<String> {
    env.get(k).and_then(Value::as_str).map(str::to_string)
}

async fn run_case(case: &Case) -> (Vec<Line>, i32, Vec<CallRec>) {
    let t0 = Instant::now();
    let calls: Calls = Arc::default();
    let out = Arc::new(Out::default());
    let env = &case.input.env;
    // The dump ran with XORGATE_MOCK=0 on Linux unless the case set it.
    let mock = is_mock_from(
        Some(env_str(env, "XORGATE_MOCK").as_deref().unwrap_or("0")),
        true,
    );
    let host = Arc::new(FakeHost {
        h: case.input.host.clone(),
        mock,
        calls: calls.clone(),
        t0,
    });
    let writer = Arc::new(FakePinctrl {
        calls: calls.clone(),
        t0,
        fails: case.input.host.pinctrl_fails.unwrap_or(false),
    });
    let signal_at = case.input.signal_at_ms;
    let code = run(
        &case.input.args,
        LedCliDeps {
            out: out.clone(),
            make_driver: Box::new(move |log| {
                let map = LedPinMap::status_led_map();
                LedDriver::new(LedDriverOptions {
                    map: Some(map.clone()),
                    writer: Some(writer),
                    probe: Some(Box::new(move || {
                        let (map, host) = (map.clone(), host.clone());
                        Box::pin(async move { probe_host_with(&map, host.as_ref()).await })
                    })),
                    log: Some(log),
                })
            }),
            service_result: env_str(env, "SERVICE_RESULT"),
            signal: Box::new(move || {
                Box::pin(async move {
                    match signal_at {
                        Some(ms) => {
                            let at = t0 + Duration::from_millis(ms);
                            tokio::time::sleep_until(at).await
                        }
                        None => std::future::pending().await,
                    }
                })
            }),
        },
    )
    .await;
    let lines = out.0.lock().unwrap().clone();
    let calls = calls.lock().unwrap().clone();
    (lines, code, calls)
}

/// Q9: `xorgate led` → `xorgate-rs led` in local-only texts.
fn q9(s: &str) -> String {
    s.replace("xorgate led", "xorgate-rs led")
}

const OWN_CASE: &str = "own consumer xorgate-rs holds all three (TS refuses; Rust CF50 does not)";

#[tokio::test(start_paused = true)]
async fn led_cli_matches_the_ts_command() {
    let fx = fixture();
    assert_eq!(fx.cases.len(), 26);
    let free = fx
        .cases
        .iter()
        .find(|c| c.name == "gpioinfo all free")
        .unwrap()
        .expected
        .clone();
    for case in &fx.cases {
        let (lines, code, calls) = run_case(case).await;
        let mut want = case.expected.clone();
        if case.name == OWN_CASE {
            // CF50: not busy — exactly the free-chip outcome.
            assert!(want.lines[0]
                .text
                .contains("already held by another driver"));
            want = free.clone();
        }
        let want_lines: Vec<Line> = want
            .lines
            .iter()
            .map(|l| Line {
                fd: l.fd,
                text: q9(&l.text),
            })
            .collect();
        assert_eq!(lines, want_lines, "{}: lines", case.name);
        assert_eq!(code, want.exit_code, "{}: exit code", case.name);
        assert_eq!(
            calls, want.calls,
            "{}: pinctrl/gpioinfo timeline",
            case.name
        );
    }
}

#[test]
fn usage_differs_from_the_ts_only_by_the_binary_name() {
    let fx = fixture();
    let ts = &fx.cases[0].expected.lines[0].text;
    assert!(ts.starts_with("usage: xorgate led "));
    let rs = xorgate_agent::cli::led::usage();
    assert_eq!(rs, q9(ts));
    assert!(!rs.contains("xorgate led"));
}

// ---------------------------------------------------------------------------
// F-E3-1 (Phase 4 gate): SIGINT mid-pattern in the REAL process, pinned to
// the gate's rig trace `fixtures/led_driver/led-cli-sigint-gate.json`
// (gate-offbench/E3-led-cli/e3run3: TS @89b5c64 vs xorgate-rs 08ff720, 9
// runs). The fixture above records writer CALLS under fake time, where both
// sides already made two blank calls; in the real process the TS's second
// `pinctrl` child (spawned in the same microtask drain as the shutdown
// blank) outlives `process.exit` and lands, while 08ff720 returned as soon
// as the shutdown blank landed and the runtime drop killed the second child
// (`kill_on_drop`). Modelled here: a writer whose "child" starts when the
// write future is first polled and LANDS only after it ran for a while (2 ms
// + 1 ms per child already running — the trace's +3/+5 ms stagger), and a
// process exit = dropping the runtime the moment `run` returns.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GateFixture {
    cases: Vec<GateCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GateCase {
    case: String,
    args: Vec<String>,
    harness_sigint_after_ms: u64,
    ts: GateSide,
    expected_spawned_after_signal: Vec<String>,
}

#[derive(Deserialize)]
struct GateSide {
    code: i32,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

#[derive(Debug, Clone)]
struct Child {
    spawned_ms: u64,
    argv: String,
    landed: bool,
}

type Children = Arc<Mutex<Vec<Child>>>;

struct ChildPinctrl {
    children: Children,
    running: Arc<std::sync::atomic::AtomicU64>,
    t0: Instant,
}

impl PinWriter for ChildPinctrl {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        use std::sync::atomic::Ordering;
        let argv = format!("pinctrl {}", PinctrlWriter::args(pins, high).join(" "));
        let (children, running, t0) = (self.children.clone(), self.running.clone(), self.t0);
        Box::pin(async move {
            let idx = {
                let mut c = children.lock().unwrap();
                c.push(Child {
                    spawned_ms: ms_since(t0),
                    argv,
                    landed: false,
                });
                c.len() - 1
            };
            let busy = running.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(2 + busy)).await;
            running.fetch_sub(1, Ordering::SeqCst);
            children.lock().unwrap()[idx].landed = true;
            true
        })
    }
}

fn run_gate_case(case: &GateCase) -> (Vec<Line>, i32, Vec<Child>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let children: Children = Arc::default();
    let out = Arc::new(Out::default());
    let code = {
        let (children, out) = (children.clone(), out.clone());
        let sig_ms = case.harness_sigint_after_ms;
        let args = case.args.clone();
        rt.block_on(async move {
            let t0 = Instant::now();
            let calls: Calls = Arc::default();
            let host = Arc::new(FakeHost {
                h: HostIn::default(),
                mock: false,
                calls,
                t0,
            });
            let writer = Arc::new(ChildPinctrl {
                children,
                running: Arc::default(),
                t0,
            });
            run(
                &args,
                LedCliDeps {
                    out,
                    make_driver: Box::new(move |log| {
                        let map = LedPinMap::status_led_map();
                        LedDriver::new(LedDriverOptions {
                            map: Some(map.clone()),
                            writer: Some(writer),
                            probe: Some(Box::new(move || {
                                let (map, host) = (map.clone(), host.clone());
                                Box::pin(async move { probe_host_with(&map, host.as_ref()).await })
                            })),
                            log: Some(log),
                        })
                    }),
                    service_result: None,
                    signal: Box::new(move || {
                        Box::pin(async move {
                            tokio::time::sleep_until(t0 + Duration::from_millis(sig_ms)).await
                        })
                    }),
                },
            )
            .await
        })
    };
    // Process exit: every task still running (a child not yet landed) dies.
    drop(rt);
    let lines = out.0.lock().unwrap().clone();
    let children = children.lock().unwrap().clone();
    (lines, code, children)
}

#[test]
fn sigint_mid_pattern_lands_the_blank_twice_like_the_ts_gate_trace() {
    let p = format!(
        "{}/../../fixtures/led_driver/led-cli-sigint-gate.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let fx: GateFixture = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    assert_eq!(fx.cases.len(), 9);
    for case in &fx.cases {
        let (lines, code, children) = run_gate_case(case);
        let sig = case.harness_sigint_after_ms;
        let want_lines: Vec<Line> = case
            .ts
            .stdout
            .iter()
            .map(|t| Line { fd: 1, text: q9(t) })
            .chain(case.ts.stderr.iter().map(|t| Line {
                fd: 2,
                text: t.clone(),
            }))
            .collect();
        let mut got_lines = lines.clone();
        got_lines.sort_by_key(|l| l.fd);
        assert_eq!(got_lines, want_lines, "{}: stdout/stderr", case.case);
        assert_eq!(code, case.ts.code, "{}: exit code", case.case);
        // Nothing the process started is killed by its exit ...
        let killed: Vec<&Child> = children.iter().filter(|c| !c.landed).collect();
        assert!(
            killed.is_empty(),
            "{}: killed children {killed:?}",
            case.case
        );
        // ... and what it started after the signal is exactly the TS's.
        let after: Vec<String> = children
            .iter()
            .filter(|c| c.spawned_ms >= sig)
            .map(|c| c.argv.clone())
            .collect();
        assert_eq!(
            after, case.expected_spawned_after_signal,
            "{}: pinctrl children after SIGINT at {sig} ms",
            case.case
        );
    }
}

// ---------------------------------------------------------------------------
// Review-gate-fixes F-R1: a SLOW lit step in flight at the signal. The
// reviewer's rig repro (scratchpad `adv/`): a pinctrl shim whose `dh` takes
// 80 ms (a write lands when its child FINISHES), SIGINT 10 ms into a `dh`,
// `blue blink-fast` — TS @89b5c64 and 08ff720 ended dark, 6e6ca52 ended on
// `set 16 op dh`. Swept here over signal instants across the pattern (incl.
// mid-`dh`): the LAST landed write is always the all-pin blank, exactly two
// blanks land after the signal (D36), nothing is killed by the exit.

#[derive(Debug, Clone)]
struct Landed {
    spawned_ms: u64,
    argv: String,
    landed_seq: Option<usize>,
}

struct SlowChild {
    log: Arc<Mutex<Vec<Landed>>>,
    seq: Arc<std::sync::atomic::AtomicUsize>,
    t0: Instant,
    dh_ms: u64,
    dl_ms: u64,
}

impl PinWriter for SlowChild {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        let argv = format!("pinctrl {}", PinctrlWriter::args(pins, high).join(" "));
        let (log, seq, t0) = (self.log.clone(), self.seq.clone(), self.t0);
        let ms = if high { self.dh_ms } else { self.dl_ms };
        Box::pin(async move {
            let idx = {
                let mut l = log.lock().unwrap();
                l.push(Landed {
                    spawned_ms: ms_since(t0),
                    argv,
                    landed_seq: None,
                });
                l.len() - 1
            };
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let n = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            log.lock().unwrap()[idx].landed_seq = Some(n);
            true
        })
    }
}

fn run_slow_case(args: &[&str], sig_ms: u64, dh_ms: u64) -> (i32, Vec<Landed>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let log: Arc<Mutex<Vec<Landed>>> = Arc::default();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let code = {
        let log = log.clone();
        rt.block_on(async move {
            let t0 = Instant::now();
            let host = Arc::new(FakeHost {
                h: HostIn::default(),
                mock: false,
                calls: Calls::default(),
                t0,
            });
            let writer = Arc::new(SlowChild {
                log,
                seq: Arc::default(),
                t0,
                dh_ms,
                dl_ms: 2,
            });
            run(
                &args,
                LedCliDeps {
                    out: Arc::new(Out::default()),
                    make_driver: Box::new(move |log| {
                        let map = LedPinMap::status_led_map();
                        LedDriver::new(LedDriverOptions {
                            map: Some(map.clone()),
                            writer: Some(writer),
                            probe: Some(Box::new(move || {
                                let (map, host) = (map.clone(), host.clone());
                                Box::pin(async move { probe_host_with(&map, host.as_ref()).await })
                            })),
                            log: Some(log),
                        })
                    }),
                    service_result: None,
                    signal: Box::new(move || {
                        Box::pin(async move {
                            tokio::time::sleep_until(t0 + Duration::from_millis(sig_ms)).await
                        })
                    }),
                },
            )
            .await
        })
    };
    drop(rt); // process exit: a child not yet landed dies here
    let log = log.lock().unwrap().clone();
    (code, log)
}

#[test]
fn sigint_with_a_slow_lit_step_in_flight_still_ends_dark() {
    const BLANK: &str = "pinctrl set 12,21,16 op dl";
    let mut mid_dh = 0;
    for (args, dh_ms) in [
        (["blue", "blink-fast"], 80),
        (["cyan", "blink-slow"], 80),
        (["purple", "heartbeat"], 150),
    ] {
        for sig in (1..900).step_by(7) {
            let (code, log) = run_slow_case(&args, sig, dh_ms);
            let at = format!("{args:?} SIGINT at {sig} ms (dh {dh_ms} ms)");
            assert_eq!(code, 0, "{at}");
            let killed: Vec<&Landed> = log.iter().filter(|w| w.landed_seq.is_none()).collect();
            assert!(killed.is_empty(), "{at}: killed {killed:?}");
            let mut by_land = log.clone();
            by_land.sort_by_key(|w| w.landed_seq);
            assert_eq!(
                by_land.last().map(|w| w.argv.as_str()),
                Some(BLANK),
                "{at}: last landed write {by_land:?}"
            );
            let blanks = log
                .iter()
                .filter(|w| w.spawned_ms >= sig && w.argv == BLANK)
                .count();
            assert_eq!(blanks, 2, "{at}: blanks after the signal {log:?}");
            // A `dh` child started before the signal and still running at it:
            // the F-R1 case (it lands after the shutdown blank).
            if log.iter().any(|w| {
                w.argv.ends_with(" dh") && w.spawned_ms < sig && w.spawned_ms + dh_ms > sig
            }) {
                mid_dh += 1;
            }
        }
    }
    assert!(
        mid_dh >= 20,
        "the sweep must hit a lit step in flight ({mid_dh} cases)"
    );
}
