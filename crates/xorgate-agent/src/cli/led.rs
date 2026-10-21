//! `xorgate-rs led …` — drive the RGB status LED by hand. Port of
//! `src/commands/led.ts` (197 lines @ 89b5c64).
//!
//! A MANUAL INSTRUMENT plus the two systemd lifecycle hooks (`led boot` /
//! `led exit-state`, CF49). It ALWAYS writes through `pinctrl`
//! ([`PinctrlWriter`], README Q11): the agent may be holding the lines
//! through gpiocdev while an operator runs `xorgate-rs led test`, and
//! `pinctrl` writes the registers directly — the TS behaviour. The probe is
//! the TS `probeHost` with the CF50 adjustment (a `[used]` line held by the
//! agent's own consumer `xorgate-rs` is not busy), so `led test` still walks
//! while the agent runs. The pin map is the hardcoded Argus CM5 map, as in
//! the TS CLI (`new LedDriver({ log })`, `led.ts:59`).
//!
//! No sudo (`led.ts:10-12`). Output: stdout = `console.log`, stderr =
//! `console.error` (the driver's `[led] …` lines and the arg errors).
//!
//! Q9 (local-only hint texts name this binary): the usage block and the
//! `stays on until 'xorgate-rs led off'` hint say `xorgate-rs led` where the
//! TS says `xorgate led`; nothing here is cloud-visible.

use std::sync::Arc;
use std::time::Duration;

use xorgate_core::led::{
    is_led_colour, is_led_pattern, LedColour, LedPattern, LED_COLOUR_NAMES, LED_PATTERN_NAMES,
};
use xorgate_core::led_state::{rung, LED_LADDER};
use xorgate_hal::led_driver::{BoxFut, LedDriver, LedDriverOptions, LedLog, PinctrlWriter};

/// `HOLD_OVERRIDES` (`led.ts:29-32`): rungs that need longer than
/// [`HOLD_MS`] to judge, by machine reason.
pub const HOLD_OVERRIDES: [(&str, u64); 2] = [("healthy", 12_000), ("config-applied", 2_500)];
/// `HOLD_MS` (`led.ts:34`).
pub const HOLD_MS: u64 = 4000;
/// `GAP_MS` (`led.ts:35`).
pub const GAP_MS: u64 = 700;

/// Where the command's lines go: fd 1 = `console.log`, fd 2 = `console.error`.
pub trait CliOut: Send + Sync {
    fn line(&self, fd: u8, text: &str);
}

/// The real process streams.
pub struct StdOut;

impl CliOut for StdOut {
    fn line(&self, fd: u8, text: &str) {
        if fd == 2 {
            eprintln!("{text}");
        } else {
            println!("{text}");
        }
    }
}

/// Everything [`run`] touches outside the driver logic (tests inject fakes).
pub struct LedCliDeps {
    pub out: Arc<dyn CliOut>,
    /// Builds the driver, given its log sink (`console.error("[led] …")`).
    /// Called only after the arguments validated (`led.ts:59`).
    pub make_driver: Box<dyn FnOnce(LedLog) -> LedDriver + Send>,
    /// `process.env.SERVICE_RESULT` (systemd sets it for `ExecStopPost`).
    pub service_result: Option<String>,
    /// Registers the SIGINT/SIGTERM/SIGHUP handlers (called right after a
    /// successful init, `led.ts:68-74`) and resolves on the first signal.
    pub signal: Box<dyn FnOnce() -> BoxFut<'static, ()> + Send>,
}

/// `xorgate-rs led …` (raw args after the verb) → exit code.
pub fn main(rest: &[String]) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("xorgate-rs: led: runtime: {e}");
            return 1;
        }
    };
    rt.block_on(run(
        rest,
        LedCliDeps {
            out: Arc::new(StdOut),
            make_driver: Box::new(|log| {
                LedDriver::new(LedDriverOptions {
                    map: None,
                    writer: Some(Arc::new(PinctrlWriter)),
                    probe: None,
                    log: Some(log),
                })
            }),
            service_result: std::env::var("SERVICE_RESULT").ok(),
            signal: Box::new(process_signals),
        },
    ))
}

/// SIGINT | SIGTERM | SIGHUP (`led.ts:74`). Registering changes the
/// disposition, so it happens only once the command owns the LED. A failed
/// registration simply never fires.
fn process_signals() -> BoxFut<'static, ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let kinds = [
        SignalKind::interrupt(),
        SignalKind::terminate(),
        SignalKind::hangup(),
    ];
    let mut streams: Vec<_> = kinds.into_iter().filter_map(|k| signal(k).ok()).collect();
    Box::pin(async move {
        if streams.is_empty() {
            return std::future::pending().await;
        }
        let waits = streams.iter_mut().map(|s| Box::pin(s.recv()));
        let _ = futures_select_all(waits.collect()).await;
    })
}

/// First of several futures (no `futures` dependency in this crate).
async fn futures_select_all<F: std::future::Future + Unpin>(mut fs: Vec<F>) {
    std::future::poll_fn(|cx| {
        for f in fs.iter_mut() {
            if std::pin::Pin::new(f).poll(cx).is_ready() {
                return std::task::Poll::Ready(());
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// `ledCommand(rest)` (`led.ts:37-106`).
pub async fn run(rest: &[String], deps: LedCliDeps) -> i32 {
    let out = deps.out;
    let pos: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let sub = match pos.first() {
        None | Some(&"help") => {
            out.line(1, &usage());
            return 0;
        }
        Some(s) => *s,
    };
    let is_hook = sub == "boot" || sub == "exit-state";
    if sub != "test" && !is_hook && !is_led_colour(sub) {
        out.line(2, &format!("unknown colour or subcommand '{sub}'"));
        out.line(1, &usage());
        return 1;
    }
    if sub != "test" && !is_hook {
        if let Some(p) = pos.get(1) {
            if !is_led_pattern(p) {
                out.line(2, &format!("unknown pattern '{p}'"));
                out.line(1, &usage());
                return 1;
            }
        }
    }

    let log_out = out.clone();
    let driver = (deps.make_driver)(Box::new(move |m| log_out.line(2, &format!("[led] {m}"))));
    let init = driver.init().await;
    if !init.ok {
        // Off-device this is normal, not an error: exit 0 so the same command
        // line is harmless on a dev box.
        out.line(
            1,
            &format!(
                "led: inert — {}; nothing to drive",
                init.reason.as_deref().unwrap_or("undefined")
            ),
        );
        return 0;
    }

    // A pattern only exists while this process does, so hand the LED back
    // dark on any signalled exit (`led.ts:68-74`). SIGKILL cannot be caught.
    let mut signal = (deps.signal)();
    // `<colour> <pattern>` (not solid/off): the body is `play` + its own
    // trailing blank (`led.ts:104-105`).
    let plays_pattern = LedColour::parse(sub).is_some_and(|c| c != LedColour::Off)
        && pos
            .get(1)
            .and_then(|p| LedPattern::parse(p))
            .is_some_and(|p| p != LedPattern::Solid);
    let body = body(
        sub,
        pos.get(1).copied(),
        &driver,
        &*out,
        deps.service_result,
    );
    tokio::pin!(body);
    tokio::select! {
        biased;
        code = &mut body => return code,
        _ = &mut signal => {}
    }
    // `shutdown`: `driver.stop(); void driver.blank().finally(() =>
    // process.exit(0))` — the main flow keeps running (its pattern sees the
    // stop and unwinds into its own trailing blank) until the shutdown
    // blank lands and the process exits.
    driver.stop();
    let shut = driver.blank(false);
    tokio::pin!(shut);
    let mut body_done = false;
    loop {
        tokio::select! {
            biased;
            _ = &mut shut => break,
            _ = &mut body, if !body_done => body_done = true,
        }
    }
    // F-E3-1 / D36 + review-gate-fixes F-R1. The TS main flow unwinds the
    // stopped pattern into its own trailing blank: at once when the signal
    // lands while `play` sleeps (spawned in the same microtask drain as the
    // shutdown blank), or once the in-flight step's write(s) resolve when it
    // lands mid-`apply` (no generation check inside `apply`, led.ts:357-381);
    // whether that blank lands before `process.exit` (execa `cleanup: true`
    // kills a child still running) is a race. Here a pattern run lets the
    // body finish (bounded by the TS writer timeout, 2 s), so its trailing
    // blank is spawned only after every step write resolved and lands LAST.
    if plays_pattern && !body_done {
        let _ = tokio::time::timeout(SHUTDOWN_UNWIND, &mut body).await;
    }
    // Our exit drops the runtime (`kill_on_drop` would kill a child still
    // running): wait for every write already started — each bounded by its
    // own `pinctrl` timeout. The body is not polled any further.
    driver.writes_settled().await;
    // The LAST write is the dark blank, always: if a lit write landed after
    // the blanks (a non-pattern run's in-flight write, or a pattern whose
    // unwind hit the 2 s bound), blank once more and let that settle too.
    if !driver.last_write_dark() {
        driver.blank(false).await;
        driver.writes_settled().await;
    }
    0
}

/// How long a signalled pattern run waits for the stopped pattern to unwind
/// into its trailing blank: the TS `pinctrlWriter` timeout (`led.ts:221`,
/// `timeoutMs: 2000`).
const SHUTDOWN_UNWIND: Duration = Duration::from_millis(2000);

/// The part of `ledCommand` after init (`led.ts:76-105`).
async fn body(
    sub: &str,
    pattern_arg: Option<&str>,
    driver: &LedDriver,
    out: &dyn CliOut,
    service_result: Option<String>,
) -> i32 {
    if sub == "test" {
        walk_ladder(driver, out).await;
        return 0;
    }
    if sub == "boot" || sub == "exit-state" {
        run_hook(driver, sub, out, service_result).await;
        return 0;
    }
    let Some(colour) = LedColour::parse(sub) else {
        return 1; // unreachable: validated above
    };
    let pattern = pattern_arg
        .and_then(LedPattern::parse)
        .unwrap_or(LedPattern::Solid);
    if colour == LedColour::Off {
        driver.blank(false).await;
        out.line(1, "led: off");
        return 0;
    }
    if pattern == LedPattern::Solid {
        // Latched on purpose: for walking over and looking at it.
        driver.set(colour).await;
        out.line(
            1,
            &format!(
                "led: {} solid — stays on until 'xorgate-rs led off'",
                colour.as_str()
            ),
        );
        return 0;
    }
    out.line(
        1,
        &if pattern == LedPattern::Flash2 {
            format!("led: {} flash2 — two flashes, then dark", colour.as_str())
        } else {
            format!(
                "led: {} {} — Ctrl-C to stop (goes dark on exit)",
                colour.as_str(),
                pattern.as_str()
            )
        },
    );
    driver.play(colour, pattern).await;
    driver.blank(false).await;
    0
}

/// `runHook` (`led.ts:115-137`): `boot` = the `starting` colour (white);
/// `exit-state` = blank on `SERVICE_RESULT=success` (or unset), else the
/// `agent-down` colour (solid red) latched.
async fn run_hook(
    driver: &LedDriver,
    hook: &str,
    out: &dyn CliOut,
    service_result: Option<String>,
) {
    if hook == "boot" {
        driver.set(rung_colour("starting")).await;
        out.line(1, "led: boot (white)");
        return;
    }
    // `process.env.SERVICE_RESULT ?? "success"`: an empty value stays empty.
    let result = service_result.unwrap_or_else(|| "success".to_string());
    if result == "success" {
        driver.blank(false).await;
        out.line(1, "led: off (clean stop)");
        return;
    }
    driver.set(rung_colour("agent-down")).await;
    out.line(1, &format!("led: red solid (SERVICE_RESULT={result})"));
}

fn rung_colour(reason: &str) -> LedColour {
    rung(reason)
        .unwrap_or_else(|| panic!("no LED ladder rung for reason '{reason}'"))
        .colour
}

/// `walkLadder` (`led.ts:140-150`): every rung long enough to judge, a dark
/// gap between them.
async fn walk_ladder(driver: &LedDriver, out: &dyn CliOut) {
    out.line(1, "Walking the LED ladder — watch the device.\n");
    for state in &LED_LADDER {
        out.line(
            1,
            &format!(
                "  [{}] {} {}",
                state.pri,
                pad(&format!(
                    "{} {}",
                    state.colour.as_str(),
                    state.pattern.as_str()
                )),
                state.label
            ),
        );
        let hold = HOLD_OVERRIDES
            .iter()
            .find(|(r, _)| *r == state.reason)
            .map_or(HOLD_MS, |(_, ms)| *ms);
        show(driver, state.colour, state.pattern, hold).await;
        driver.blank(false).await;
        tokio::time::sleep(Duration::from_millis(GAP_MS)).await;
    }
    driver.blank(false).await;
    out.line(1, "\nDone — LED is dark.");
}

/// `show` (`led.ts:153-163`): run one pattern for `ms`, then cancel it and
/// wait for it to unwind. The hold timer is polled before the pattern so a
/// step boundary that coincides with the end of the hold is cancelled, not
/// drawn — the TS's timer order (the hold's `setTimeout` was armed first).
async fn show(driver: &LedDriver, colour: LedColour, pattern: LedPattern, ms: u64) {
    let running = driver.play(colour, pattern);
    tokio::pin!(running);
    let hold = tokio::time::sleep(Duration::from_millis(ms));
    tokio::pin!(hold);
    let mut done = false;
    loop {
        tokio::select! {
            biased;
            _ = &mut hold => break,
            _ = &mut running, if !done => done = true,
        }
    }
    driver.stop();
    if !done {
        running.await;
    }
}

/// `s.padEnd(18)` (`led.ts:169-171`) — the ladder strings are ASCII.
fn pad(s: &str) -> String {
    format!("{s:<18}")
}

/// `usage()` (`led.ts:173-196`), `xorgate led` → `xorgate-rs led` (Q9).
pub fn usage() -> String {
    let colours: Vec<&str> = LED_COLOUR_NAMES
        .iter()
        .filter(|c| **c != LedColour::Off)
        .map(|c| c.as_str())
        .collect();
    let patterns: Vec<&str> = LED_PATTERN_NAMES.iter().map(|p| p.as_str()).collect();
    format!(
        "usage: xorgate-rs led <colour> [pattern] | led off | led test

  <colour>   {}
             (no yellow/amber: r+g reads as green on this LED and cannot be
             told apart from it — measured, not assumed)
  [pattern]  {}   (default: solid)

  led off    turn the LED off
  led test   walk every state in the status ladder, printing each label —
             this is the one to run with someone watching the device

  led boot        systemd ExecStartPre hook — white while the agent comes up
  led exit-state  systemd ExecStopPost hook — blank on a clean stop, solid red
                  on a crash (reads $SERVICE_RESULT)

  'solid' stays lit after the command exits; every other pattern lasts as long
  as the process (Ctrl-C leaves the LED dark). No settings, no cloud, no policy:
  this is a manual instrument.

Examples:
  xorgate-rs led purple blink-fast
  xorgate-rs led white
  xorgate-rs led test",
        colours.join(" | "),
        patterns.join(" | ")
    )
}
