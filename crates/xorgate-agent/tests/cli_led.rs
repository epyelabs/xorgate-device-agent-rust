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
        if case.input.signal_at_ms.is_some() {
            // D36 (option B): the TS CALLS the dark blank twice after the
            // signal (shutdown + the main flow's trailing blank, which then
            // races execa's exit cleanup); Rust calls and lands exactly ONE.
            let blank = |c: &CallRec| c.argv.join(" ") == "pinctrl set 12,21,16 op dl";
            let n = want.calls.len();
            assert!(
                n >= 2 && blank(&want.calls[n - 1]) && blank(&want.calls[n - 2]),
                "{}: the TS ends on two blank calls",
                case.name
            );
            want.calls.pop();
        }
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
// D36 (orchestrator option B, review-gate-fixes final check): a signalled
// pattern lands EXACTLY ONE dark blank and exits — the TS's "exit wins"
// outcome. The TS lands 1 or 2 (its trailing blank races execa's
// `cleanup: true` kill at process.exit). Modelled with a writer whose
// "child" starts when the write future is first polled and LANDS when it
// finishes; process exit = dropping the runtime the moment `run` returns
// (a child still running is killed, as `kill_on_drop` / execa cleanup do).

const BLANK: &str = "pinctrl set 12,21,16 op dl";

#[derive(Debug, Clone)]
struct Landed {
    spawned_ms: u64,
    argv: String,
    landed_seq: Option<usize>,
}

/// `dh` writes take `dh_ms`, `dl` writes 2 ms. From `hang_from_ms` on, every
/// write instead takes `hang_ms` and fails — `None` = never returns (a hung
/// pinctrl without the writer's own timeout).
struct SlowChild {
    log: Arc<Mutex<Vec<Landed>>>,
    seq: Arc<std::sync::atomic::AtomicUsize>,
    t0: Instant,
    dh_ms: u64,
    hang_from_ms: Option<u64>,
    hang_ms: Option<u64>,
}

impl PinWriter for SlowChild {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        let argv = format!("pinctrl {}", PinctrlWriter::args(pins, high).join(" "));
        let (log, seq, t0) = (self.log.clone(), self.seq.clone(), self.t0);
        let (dh_ms, hang_from, hang_ms) = (self.dh_ms, self.hang_from_ms, self.hang_ms);
        Box::pin(async move {
            let now = ms_since(t0);
            let idx = {
                let mut l = log.lock().unwrap();
                l.push(Landed {
                    spawned_ms: now,
                    argv,
                    landed_seq: None,
                });
                l.len() - 1
            };
            if hang_from.is_some_and(|h| now >= h) {
                match hang_ms {
                    Some(ms) => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        return false; // timed out: nothing written
                    }
                    None => std::future::pending::<()>().await,
                }
            }
            let ms = if high { dh_ms } else { 2 };
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let n = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            log.lock().unwrap()[idx].landed_seq = Some(n);
            true
        })
    }
}

struct Signalled {
    lines: Vec<Line>,
    code: i32,
    /// Virtual ms from start to `run` returning (the process exit).
    exit_ms: u64,
    log: Vec<Landed>,
}

fn run_signalled(args: &[String], sig_ms: u64, dh_ms: u64, hang: Option<Option<u64>>) -> Signalled {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let log: Arc<Mutex<Vec<Landed>>> = Arc::default();
    let out = Arc::new(Out::default());
    let (code, exit_ms) = {
        let (log, out, args) = (log.clone(), out.clone(), args.to_vec());
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
                hang_from_ms: hang.map(|_| sig_ms),
                hang_ms: hang.flatten(),
            });
            let code = run(
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
            .await;
            (code, ms_since(t0))
        })
    };
    drop(rt); // process exit: a child still running dies here
    let lines = out.0.lock().unwrap().clone();
    let log = log.lock().unwrap().clone();
    Signalled {
        lines,
        code,
        exit_ms,
        log,
    }
}

/// Exactly one blank was started after the signal and it landed; no other
/// write started at/after the signal landed (a step whose timer fired in the
/// same instant as the signal may start, but it is killed); and the blank
/// is the LAST write that landed (nothing — no lit step — after it).
fn assert_one_blank_ends_dark(at: &str, sig: u64, r: &Signalled) {
    assert_eq!(r.code, 0, "{at}");
    let started: Vec<&Landed> = r.log.iter().filter(|w| w.spawned_ms >= sig).collect();
    let blanks: Vec<&&Landed> = started.iter().filter(|w| w.argv == BLANK).collect();
    assert_eq!(blanks.len(), 1, "{at}: blanks after the signal {:?}", r.log);
    assert!(blanks[0].landed_seq.is_some(), "{at}: the blank landed");
    // Besides the blank, only a step whose timer fired in the signal's own
    // instant (started before the signal was handled) may land — and only
    // before the blank (checked below).
    let others: Vec<&&Landed> = started
        .iter()
        .filter(|w| w.landed_seq.is_some() && w.argv != BLANK)
        .collect();
    assert!(
        others.iter().all(|w| w.spawned_ms == sig),
        "{at}: landed after the signal {:?}",
        r.log
    );
    let last = r
        .log
        .iter()
        .filter(|w| w.landed_seq.is_some())
        .max_by_key(|w| w.landed_seq);
    assert_eq!(
        last.map(|w| w.argv.as_str()),
        Some(BLANK),
        "{at}: last landed {:?}",
        r.log
    );
}

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

/// The 9 SIGINT runs of the Phase 4 gate rig trace
/// (`fixtures/led_driver/led-cli-sigint-gate.json`, gate-offbench/E3-led-cli/
/// e3run3, where the TS happened to land 2 blanks 9/9): stdout/stderr/code
/// = the TS; Rust lands exactly the FIRST of the TS's two blanks (D36).
#[test]
fn sigint_mid_pattern_lands_exactly_one_blank_d36() {
    let p = format!(
        "{}/../../fixtures/led_driver/led-cli-sigint-gate.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let fx: GateFixture = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    assert_eq!(fx.cases.len(), 9);
    for case in &fx.cases {
        let sig = case.harness_sigint_after_ms;
        let r = run_signalled(&case.args, sig, 2, None);
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
        let mut got = r.lines.clone();
        got.sort_by_key(|l| l.fd);
        assert_eq!(got, want_lines, "{}: stdout/stderr", case.case);
        assert_eq!(r.code, case.ts.code, "{}: exit code", case.case);
        assert_eq!(
            case.expected_spawned_after_signal,
            vec![BLANK.to_string(), BLANK.to_string()],
            "{}: the TS trace",
            case.case
        );
        assert_one_blank_ends_dark(&case.case, sig, &r);
    }
}

/// Review-gate-fixes F-R1 / final check: a SLOW lit step in flight at the
/// signal (the reviewer's rig repro: `dh` children 80 ms, SIGINT 10 ms into
/// one). The TS kills that child at exit (execa cleanup) and never
/// re-lights; so does Rust now — swept over signal instants across three
/// patterns: one blank, it lands last, an in-flight `dh` that would finish
/// after it is killed (never lands).
#[test]
fn sigint_with_a_slow_lit_step_in_flight_ends_dark_without_relight() {
    let mut killed_dh = 0;
    for (args, dh_ms) in [
        (["blue", "blink-fast"], 80),
        (["cyan", "blink-slow"], 80),
        (["purple", "heartbeat"], 150),
    ] {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        for sig in (1..900).step_by(7) {
            let r = run_signalled(&args, sig, dh_ms, None);
            let at = format!("{args:?} SIGINT at {sig} ms (dh {dh_ms} ms)");
            assert_one_blank_ends_dark(&at, sig, &r);
            killed_dh += r
                .log
                .iter()
                .filter(|w| w.argv.ends_with(" dh") && w.landed_seq.is_none())
                .count();
        }
    }
    assert!(
        killed_dh >= 20,
        "the sweep must hit a lit step in flight ({killed_dh} killed)"
    );
}

/// Every pinctrl hangs after the signal. With the writer's own 2 s timeout
/// (`PinctrlWriter`, the TS `timeoutMs: 2000`) the shutdown blank fails at
/// +2 s and its fail-safe all-pins blank at +4 s — the TS's measured exit
/// +4.03 s; with no timeout at all, `SHUTDOWN_DEADLINE` still exits at +4 s.
#[test]
fn sigint_with_every_pinctrl_hung_exits_within_the_ts_bound() {
    let args: Vec<String> = ["blue", "blink-fast"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let deadline = xorgate_agent::cli::led::SHUTDOWN_DEADLINE.as_millis() as u64;
    assert_eq!(deadline, 4000);
    for (hang, label) in [(Some(2000), "2 s writer timeout"), (None, "no timeout")] {
        for sig in [3_023, 3_100, 4_300] {
            let r = run_signalled(&args, sig, 2, Some(hang));
            let at = format!("{label}, SIGINT at {sig} ms");
            assert_eq!(r.code, 0, "{at}");
            assert!(
                r.exit_ms - sig <= deadline,
                "{at}: exit at +{} ms",
                r.exit_ms - sig
            );
            let after: Vec<&str> = r
                .log
                .iter()
                .filter(|w| w.spawned_ms >= sig)
                .map(|w| w.argv.as_str())
                .collect();
            match hang {
                // the shutdown blank, then its fail-safe blank (led.ts:375-379)
                Some(_) => assert_eq!(after, vec![BLANK, BLANK], "{at}"),
                None => assert_eq!(after, vec![BLANK], "{at}"),
            }
            assert!(r.exit_ms - sig >= 3_999, "{at}: +{} ms", r.exit_ms - sig);
        }
    }
}
