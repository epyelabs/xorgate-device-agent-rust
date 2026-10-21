//! RGB status LED — the channel driver. Port of the I/O half of
//! `src/hardware/led.ts` (433 lines @ 89b5c64): `pinctrlWriter` (`:217-225`),
//! `LedDriver` (`:249-418`) and `probeHost` (`:420-433`). The pure half
//! (colours, patterns, timelines, `validatePinMap`, `parseBusyPins`) is
//! `xorgate_core::led`.
//!
//! This is a RENDERER only — colour + pattern in, GPIO writes out; it decides
//! nothing about what the device is doing (`led_state` / the agent's
//! `LedManager` do).
//!
//! Two writers (README Q11, phase-4 brief I1):
//! - [`PinctrlWriter`] — `pinctrl set <a,b> op dh|dl`, exactly the TS. The CLI
//!   verbs and the two systemd hooks ALWAYS use it (the agent may be holding
//!   the lines; `pinctrl` writes the registers directly, as in the TS).
//! - [`GpiocdevWriter`] — the agent's default: ONE line request for r,g,b on
//!   the chip found by label (`pinctrl-rp1`, then `pinctrl-bcm2711`) unless
//!   `XORGATE_LED_GPIOCHIP` names one, consumer [`LED_CONSUMER`], outputs,
//!   held until drop. `XORGATE_LED_WRITER=pinctrl` makes the agent use
//!   [`PinctrlWriter`] instead (rig, Q6).
//!
//! No sudo anywhere here (`led.ts:10-12`): the agent runs as the provisioning
//! user, and neither `pinctrl` nor the character device needs root.
//!
//! Timing: pattern steps sleep on tokio time (monotonic, `pause()`-testable).

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use xorgate_core::js::number_to_string;
use xorgate_core::led::{
    led_timeline, parse_busy_pins, validate_pin_map, LedChannels, LedColour, LedPattern, LedPinMap,
    PinMapCheck,
};

use crate::io::{GpioVia, NullTape, TapeSink};

/// Same shape as `xorgate_agent::ctx::BoxFut` (a type alias, so the two are
/// interchangeable).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The gpiod consumer label the agent's line request carries; a `[used]`
/// line with this consumer is OURS, not "busy" (CF50).
pub const LED_CONSUMER: &str = "xorgate-rs";

/// `run("pinctrl", …, { timeoutMs: 2000 })` (`led.ts:221-224`).
pub const PINCTRL_TIMEOUT: Duration = Duration::from_millis(2000);
/// `run("gpioinfo", ["gpiochip0"], { timeoutMs: 2000 })` (`led.ts:430`).
pub const GPIOINFO_TIMEOUT: Duration = Duration::from_millis(2000);
/// The chip labels the gpiocdev writer looks for, in order (CM5/Pi 5 RP1,
/// then CM4/Pi 4).
pub const CHIP_LABELS: [&str; 2] = ["pinctrl-rp1", "pinctrl-bcm2711"];
/// Inert reason when no chip is found (or `XORGATE_LED_GPIOCHIP` names a
/// path that does not exist) — phase-4 brief I1.
pub const CHIP_NOT_FOUND: &str = "gpio chip not found";
/// `XORGATE_LED_WRITER` (Q6).
pub const ENV_LED_WRITER: &str = "XORGATE_LED_WRITER";
/// `XORGATE_LED_GPIOCHIP` (Q6).
pub const ENV_LED_GPIOCHIP: &str = "XORGATE_LED_GPIOCHIP";

/// `PinWriter` (`led.ts:217`): drives one or more lines to the SAME physical
/// level in a single call. Resolves `false` on any failure — never panics
/// (a panicking writer is still survived: [`LedDriver`] catches a panic on
/// the call, runs the returned future as its own task and reads a
/// `JoinError` as `false` — the TS `catch` of `writeGroup`,
/// `led.ts:383-390`). `write` is called at the moment the TS calls
/// `this.write` (synchronously), so the returned future must own its data.
///
/// **Lazy contract:** `write` itself must not touch a pin; the write is
/// issued when the returned future is first polled. A future dropped
/// unpolled is a write that never ran, and [`TapedWriter`] does not tape it
/// (D31: a pattern superseded before its task is first polled).
///
/// Batched deliberately (`led.ts:206-216`): a transition is one call (two
/// only when some channels rise while others fall).
pub trait PinWriter: Send + Sync + 'static {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool>;
    /// The tape `gpio.via` tag; `None` = an injected test writer.
    fn via(&self) -> Option<GpioVia> {
        None
    }
}

/// `pinctrlWriter` (`led.ts:219-225`): `pinctrl set <pins.join(",")> op
/// dh|dl`, 2 s timeout, `!res.failed`. No pins = `true` without a spawn.
#[derive(Debug, Default, Clone, Copy)]
pub struct PinctrlWriter;

impl PinctrlWriter {
    /// The argv after `pinctrl` (pure, so the rig/tests can compare it).
    pub fn args(pins: &[u32], high: bool) -> [String; 4] {
        let list: Vec<String> = pins.iter().map(u32::to_string).collect();
        [
            "set".to_string(),
            list.join(","),
            "op".to_string(),
            if high { "dh" } else { "dl" }.to_string(),
        ]
    }
}

impl PinWriter for PinctrlWriter {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        let pins = pins.to_vec();
        Box::pin(async move {
            if pins.is_empty() {
                return true;
            }
            let args = Self::args(&pins, high);
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let res = crate::exec::run("pinctrl", &argv, Some(PINCTRL_TIMEOUT)).await;
            !res.failed
        })
    }
    fn via(&self) -> Option<GpioVia> {
        Some(GpioVia::Pinctrl)
    }
}

/// The agent's writer: one gpiocdev line request for r, g, b, held for the
/// writer's lifetime (released on drop = process exit, so the systemd hooks
/// that run after the agent never see it). Rust-only (Q11).
pub struct GpiocdevWriter {
    req: Arc<gpiocdev::Request>,
    chip: PathBuf,
}

impl std::fmt::Debug for GpiocdevWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpiocdevWriter")
            .field("chip", &self.chip)
            .finish_non_exhaustive()
    }
}

impl GpiocdevWriter {
    /// Find the chip (`chip_override`, else by label [`CHIP_LABELS`]),
    /// request the three lines with consumer [`LED_CONSUMER`] and make them
    /// outputs. Never panics; any failure is the inert reason.
    ///
    /// The lines are requested AS-IS first and their current levels read
    /// back, then reconfigured as outputs at those levels: requesting them
    /// straight as outputs would force a level (dark) between the
    /// `ExecStartPre` hook's white and the manager's first render.
    pub fn open(map: &LedPinMap, chip_override: Option<&Path>) -> Result<Self, String> {
        let chip = match chip_override {
            Some(p) => {
                if !p.exists() {
                    return Err(CHIP_NOT_FOUND.to_string());
                }
                p.to_path_buf()
            }
            None => find_chip_by_label(&CHIP_LABELS).ok_or_else(|| CHIP_NOT_FOUND.to_string())?,
        };
        let offsets = map_offsets(map);
        let req = gpiocdev::Request::builder()
            .on_chip(&chip)
            .with_consumer(LED_CONSUMER)
            .with_lines(&offsets)
            .as_is()
            .request()
            .map_err(|e| format!("gpio line request on {} failed: {e}", chip.display()))?;
        let mut current = gpiocdev::line::Values::from_offsets(&offsets);
        if req.values(&mut current).is_err() {
            current = offsets
                .iter()
                .map(|o| (*o, gpiocdev::line::Value::Inactive))
                .collect();
        }
        let mut cfg = req.config();
        cfg.with_output_lines(&current);
        req.reconfigure(&cfg)
            .map_err(|e| format!("gpio line output config on {} failed: {e}", chip.display()))?;
        Ok(Self {
            req: Arc::new(req),
            chip,
        })
    }

    /// The chip the lines were requested on.
    pub fn chip(&self) -> &Path {
        &self.chip
    }
}

impl PinWriter for GpiocdevWriter {
    /// One `set_values` for the group (= one TS `pinctrl` call). The ioctl is
    /// immediate, but it is issued only when the future is polled (lazy
    /// contract of [`PinWriter`]), like the `pinctrl` spawn.
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        let req = self.req.clone();
        let pins = pins.to_vec();
        Box::pin(async move {
            if pins.is_empty() {
                return true;
            }
            let value = if high {
                gpiocdev::line::Value::Active
            } else {
                gpiocdev::line::Value::Inactive
            };
            let values: gpiocdev::line::Values = pins.iter().map(|p| (*p, value)).collect();
            req.set_values(&values).is_ok()
        })
    }
    fn via(&self) -> Option<GpioVia> {
        Some(GpioVia::Gpiocdev)
    }
}

/// The first chip (in `/dev/gpiochip*` order) whose label is `labels[0]`,
/// else `labels[1]`, …
pub fn find_chip_by_label(labels: &[&str]) -> Option<PathBuf> {
    let chips = gpiocdev::chip::chips().ok()?;
    let labelled: Vec<(PathBuf, String)> = chips
        .into_iter()
        .filter_map(|p| {
            let label = gpiocdev::Chip::from_path(&p).ok()?.info().ok()?.label;
            Some((p, label))
        })
        .collect();
    labels.iter().find_map(|want| {
        labelled
            .iter()
            .find(|(_, l)| l == want)
            .map(|(p, _)| p.clone())
    })
}

/// `XORGATE_LED_WRITER=gpiocdev|pinctrl` (Q6; default `gpiocdev`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterChoice {
    Gpiocdev,
    Pinctrl,
}

impl WriterChoice {
    /// Only the exact value `pinctrl` selects pinctrl; anything else (unset,
    /// empty, a typo) keeps the default.
    pub fn from_env_value(v: Option<&str>) -> Self {
        match v {
            Some("pinctrl") => WriterChoice::Pinctrl,
            _ => WriterChoice::Gpiocdev,
        }
    }
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var(ENV_LED_WRITER).ok().as_deref())
    }
}

/// `XORGATE_LED_GPIOCHIP` (Q6): a set, non-empty value overrides the
/// by-label lookup.
pub fn chip_override_from_env() -> Option<PathBuf> {
    std::env::var_os(ENV_LED_GPIOCHIP)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `LedInit` (`led.ts:245-247`): the probe verdict plus the map.
#[derive(Debug, Clone, PartialEq)]
pub struct LedInit {
    pub ok: bool,
    pub reason: Option<String>,
    pub map: LedPinMap,
}

pub type LedLog = Box<dyn Fn(&str) + Send + Sync>;
pub type LedProbe = Box<dyn Fn() -> BoxFut<'static, PinMapCheck> + Send + Sync>;

/// `LedDriverOptions` (`led.ts:236-243`).
#[derive(Default)]
pub struct LedDriverOptions {
    /// Default: `STATUS_LED_MAP`.
    pub map: Option<LedPinMap>,
    /// Default: [`PinctrlWriter`] (the TS default; the agent passes its
    /// writer explicitly, see [`agent_driver`]).
    pub writer: Option<Arc<dyn PinWriter>>,
    /// Default: [`probe_host`] of the map.
    pub probe: Option<LedProbe>,
    /// Default: `[led] <msg>` at info (`console.error` in the TS; both
    /// streams land in the journal at the same priority).
    pub log: Option<LedLog>,
}

#[derive(Default)]
struct State {
    ready: bool,
    initialised: bool,
    logged_failure: bool,
    /// Bumped by stop()/set() so an in-flight pattern knows it was superseded.
    generation: u64,
    /// The current sleep's wake (id, sender); `None` = nobody sleeping.
    wake: Option<(u64, oneshot::Sender<()>)>,
    wake_seq: u64,
    /// Last levels we know we wrote; `None` = unknown, write everything.
    last_levels: Option<LedChannels>,
}

struct Inner {
    map: LedPinMap,
    /// r, g, b line offsets.
    pins: [u32; 3],
    write: Arc<dyn PinWriter>,
    log: LedLog,
    probe: LedProbe,
    st: Mutex<State>,
    /// Writer calls whose future has not finished yet (see
    /// [`LedDriver::writes_settled`]).
    in_flight: watch::Sender<usize>,
    /// Whether the write that FINISHED last drove all three channels dark
    /// (`None` = none finished yet). Finish order = the order the writes
    /// took effect (each is one `pinctrl` / line-request call).
    last_landed_dark: Arc<Mutex<Option<bool>>>,
}

/// Decrements [`Inner::in_flight`] when a writer task ends — completion,
/// panic or abort alike.
struct InFlight(watch::Sender<usize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// `LedDriver` (`led.ts:249-418`): colour + pattern → pins. Cheap to clone
/// (shared state).
///
/// Two rules are load-bearing (`led.ts:252-257`):
/// - **Fail safe, not fail-lit.** Any write failure blanks all three channels.
/// - **Self-gating.** With a failed probe the driver is inert and every call
///   is a silent no-op.
///
/// Every flow method ([`set`](Self::set), [`blank`](Self::blank),
/// [`play`](Self::play), [`init`](Self::init)) runs its SYNCHRONOUS TS prefix
/// (`stop()` + the generation capture, the `initialised` latch) at call time
/// and returns the rest as a `'static` future — so a caller that spawns it
/// (the manager's fire-and-forget `void driver.play(…)`) supersedes the old
/// pattern in call order, as the TS does.
#[derive(Clone)]
pub struct LedDriver {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LedDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedDriver")
            .field("map", &self.inner.map)
            .finish_non_exhaustive()
    }
}

fn map_offsets(map: &LedPinMap) -> [u32; 3] {
    // Pins are JS numbers in the map; a probe that passed `validate_pin_map`
    // guarantees integers in 0..=53 (an injected probe may not — the cast
    // saturates and the write simply fails).
    [map.pins.r as u32, map.pins.g as u32, map.pins.b as u32]
}

impl LedDriver {
    /// `new LedDriver(opts)` (`led.ts:271-276`).
    pub fn new(opts: LedDriverOptions) -> Self {
        let map = opts.map.unwrap_or_else(LedPinMap::status_led_map);
        let probe = opts.probe.unwrap_or_else(|| {
            let m = map.clone();
            Box::new(move || {
                let m = m.clone();
                Box::pin(async move { probe_host(&m).await })
            })
        });
        let pins = map_offsets(&map);
        LedDriver {
            inner: Arc::new(Inner {
                map,
                pins,
                write: opts.writer.unwrap_or_else(|| Arc::new(PinctrlWriter)),
                log: opts
                    .log
                    .unwrap_or_else(|| Box::new(|m| tracing::info!("[led] {m}"))),
                probe,
                st: Mutex::new(State::default()),
                in_flight: watch::Sender::new(0),
                last_landed_dark: Arc::new(Mutex::new(None)),
            }),
        }
    }

    pub fn map(&self) -> &LedPinMap {
        &self.inner.map
    }

    /// `get available()` (`led.ts:279-281`): true once init() found a usable
    /// map and toolchain.
    pub fn available(&self) -> bool {
        self.inner.lock().ready
    }

    /// `init()` (`led.ts:289-302`): probe once, log the resolved map once
    /// (`${kind} r=GPIO… g=GPIO… b=GPIO… (active high|low)`). Going inert is
    /// not logged here; the caller gets `reason`. A second call returns the
    /// latched verdict WITHOUT a reason, as the TS does.
    pub fn init(&self) -> BoxFut<'static, LedInit> {
        let inner = self.inner.clone();
        let latched = {
            let mut st = inner.lock();
            if st.initialised {
                Some(st.ready)
            } else {
                st.initialised = true;
                None
            }
        };
        if let Some(ok) = latched {
            let map = inner.map.clone();
            return Box::pin(async move {
                LedInit {
                    ok,
                    reason: None,
                    map,
                }
            });
        }
        let probe = (inner.probe)();
        Box::pin(async move {
            let check = probe.await;
            inner.lock().ready = check.ok;
            if check.ok {
                let p = &inner.map.pins;
                (inner.log)(&format!(
                    "{} r=GPIO{} g=GPIO{} b=GPIO{} (active {})",
                    inner.map.kind,
                    number_to_string(p.r),
                    number_to_string(p.g),
                    number_to_string(p.b),
                    if inner.map.active_high { "high" } else { "low" }
                ));
            }
            LedInit {
                ok: check.ok,
                reason: check.reason,
                map: inner.map.clone(),
            }
        })
    }

    /// `set(colour)` (`led.ts:305-308`): show a colour and hold it; cancels
    /// any running pattern.
    pub fn set(&self, colour: LedColour) -> BoxFut<'static, ()> {
        self.stop();
        Inner::apply(&self.inner, colour, false)
    }

    /// `blank({ quiet })` (`led.ts:318-321`): drive every channel low. `quiet`
    /// suppresses the write-failure log (the shutdown path races exit).
    pub fn blank(&self, quiet: bool) -> BoxFut<'static, ()> {
        self.stop();
        Inner::apply(&self.inner, LedColour::Off, quiet)
    }

    /// Resolves once every write started so far (its future polled — the
    /// lazy [`PinWriter`] contract) has finished; at once when none is in
    /// flight. A `pinctrl` child the TS has already spawned is
    /// not killed by `process.exit` and still lands; a Rust process exit
    /// drops the runtime, and `kill_on_drop` would kill it. The CLI's
    /// shutdown awaits this before it exits (F-E3-1, Phase 4 gate). Bounded
    /// by the writer's own timeout (`pinctrl`: 2 s). Writes started while it
    /// waits are waited for too — the caller stops driving the flow first.
    ///
    /// It waits for a LIT write too, which may then land AFTER a blank: a
    /// caller that must end dark checks [`last_write_dark`](Self::last_write_dark)
    /// afterwards and blanks again (review-gate-fixes F-R1).
    pub fn writes_settled(&self) -> BoxFut<'static, ()> {
        let mut rx = self.inner.in_flight.subscribe();
        Box::pin(async move {
            let _ = rx.wait_for(|n| *n == 0).await;
        })
    }

    /// True when the write that finished LAST drove all three channels dark
    /// and succeeded (or nothing was written yet). Rust-only; the CLI's
    /// shutdown uses it after [`writes_settled`](Self::writes_settled).
    pub fn last_write_dark(&self) -> bool {
        let g = self
            .inner
            .last_landed_dark
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *g != Some(false)
    }

    /// `stop()` (`led.ts:324-332`): cancel a running pattern; leaves the LED
    /// wherever the pattern left it.
    pub fn stop(&self) {
        let wake = {
            let mut st = self.inner.lock();
            st.generation += 1;
            // Every new colour/pattern starts from "levels unknown".
            st.last_levels = None;
            st.wake.take()
        };
        if let Some((_, tx)) = wake {
            let _ = tx.send(());
        }
    }

    /// `play(colour, pattern)` (`led.ts:339-354`): resolves when the pattern
    /// finishes (`flash2`) or is superseded — repeating patterns and `solid`
    /// never finish on their own. A non-repeating timeline ends dark.
    ///
    /// The first step's `apply` starts at call time, as in the TS (the loop
    /// runs synchronously up to the first writer call), so the writer is
    /// CALLED for the first step's rise group at once — but under the lazy
    /// [`PinWriter`] contract nothing is written until this future is
    /// polled.
    ///
    /// **Accepted divergence D31** (review-wp1 M1): a pattern superseded
    /// before its task is first polled drops its first step — the pending
    /// rise never runs (and is not taped) and its fall group is never
    /// called. The TS (`led.ts:339-381`) always finishes that step: its fall
    /// group lands AFTER the new pattern's rise, so `play(red, blink-fast)`
    /// immediately followed by `play(green, solid)` ends DARK (writes
    /// `[12]H, [21]H, [21,16]L, [12,16]L`); Rust calls `[12]H, [21]H,
    /// [12,16]L`, runs `[21]H, [12,16]L` and ends green. A superseded step
    /// that is already running (its task polled) still finishes exactly as
    /// in the TS. Pinned by `tests/led_driver.rs::d31_*`.
    pub fn play(&self, colour: LedColour, pattern: LedPattern) -> BoxFut<'static, ()> {
        self.stop();
        let inner = self.inner.clone();
        let gen = inner.lock().generation;
        let tl = led_timeline(pattern);
        let shade = move |on: bool| if on { colour } else { LedColour::Off };
        let mut first = tl
            .steps
            .first()
            .map(|s| Inner::apply(&inner, shade(s.on), false));
        Box::pin(async move {
            loop {
                for step in &tl.steps {
                    if gen != inner.generation() {
                        return;
                    }
                    match first.take() {
                        Some(f) => f.await,
                        None => Inner::apply(&inner, shade(step.on), false).await,
                    }
                    if gen != inner.generation() {
                        return;
                    }
                    inner.sleep(step.ms, gen).await;
                }
                if !(tl.repeat && gen == inner.generation()) {
                    break;
                }
            }
            // A non-repeating timeline whose last step held indefinitely
            // (`solid`) was cancelled by stop(); anything else ends dark.
            if gen == inner.generation() {
                Inner::apply(&inner, LedColour::Off, false).await;
            }
        })
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// `apply(colour, quiet)` (`led.ts:357-381`): write one colour to the
    /// three channels, eliding channels already at the wanted level (cache
    /// dropped on every stop()) and batching the rest by level; on failure,
    /// cancel, blank all three in one call (cache ignored) and log once.
    ///
    /// Runs the TS synchronous prefix at call time — the `ready` check, the
    /// rise/fall split against the cache, and the rise group's writer call —
    /// and returns the rest.
    fn apply(inner: &Arc<Inner>, colour: LedColour, quiet: bool) -> BoxFut<'static, ()> {
        let want = colour.channels();
        let (rise, fall) = {
            let st = inner.lock();
            if !st.ready {
                return Box::pin(async {});
            }
            let mut rise = Vec::new();
            let mut fall = Vec::new();
            let chans = [
                (st.last_levels.map(|l| l.r), want.r, inner.pins[0]),
                (st.last_levels.map(|l| l.g), want.g, inner.pins[1]),
                (st.last_levels.map(|l| l.b), want.b, inner.pins[2]),
            ];
            for (last, w, pin) in chans {
                if last == Some(w) {
                    continue;
                }
                if w {
                    rise.push(pin);
                } else {
                    fall.push(pin);
                }
            }
            (rise, fall)
        };
        let rise_done = inner.write_group(rise, true);
        let inner = inner.clone();
        Box::pin(async move {
            // `(await writeGroup(rise, true)) && (await writeGroup(fall,
            // false))` — short-circuits: a failed rise skips the fall.
            let ok = rise_done.await && inner.write_group(fall, false).await;
            let wake = {
                let mut st = inner.lock();
                if ok {
                    st.last_levels = Some(want);
                    return;
                }
                st.last_levels = None; // the pins are in an unknown state
                st.generation += 1;
                st.wake.take()
            };
            if let Some((_, tx)) = wake {
                let _ = tx.send(());
            }
            inner.write_group(inner.pins.to_vec(), false).await;
            if !quiet {
                let first = {
                    let mut st = inner.lock();
                    !std::mem::replace(&mut st.logged_failure, true)
                };
                if first {
                    (inner.log)("pin write failed — LED blanked (further failures not logged)");
                }
            }
        })
    }

    /// `writeGroup(pins, lit)` (`led.ts:383-390`): no pins = `true`; the
    /// physical level honours `activeHigh`. The writer is CALLED now (the TS
    /// calls `this.write` synchronously); a writer that panics — on the call
    /// or while its future runs (own task, `JoinError`) — reads as `false`,
    /// the TS `catch`.
    fn write_group(&self, pins: Vec<u32>, lit: bool) -> BoxFut<'static, bool> {
        if pins.is_empty() {
            return Box::pin(async { true });
        }
        let high = if self.map.active_high { lit } else { !lit };
        match catch_unwind(AssertUnwindSafe(|| self.write.write(&pins, high))) {
            Ok(fut) => {
                let in_flight = self.in_flight.clone();
                let landed = self.last_landed_dark.clone();
                let mut all = self.pins.to_vec();
                all.sort_unstable();
                let mut these = pins.clone();
                these.sort_unstable();
                let dark_all = !lit && these == all;
                Box::pin(async move {
                    // Lazy, as before: the write starts (and counts as in
                    // flight) when this future is first polled (D31).
                    in_flight.send_modify(|n| *n += 1);
                    let guard = InFlight(in_flight);
                    tokio::spawn(async move {
                        let _guard = guard;
                        let ok = fut.await;
                        *landed.lock().unwrap_or_else(|p| p.into_inner()) = Some(ok && dark_all);
                        ok
                    })
                    .await
                    .unwrap_or(false)
                })
            }
            Err(_) => Box::pin(async { false }),
        }
    }

    /// `sleep(ms, gen)` (`led.ts:393-417`): cancellable; `ms: 0` waits until
    /// stop(). A sleep whose wake was replaced without being fired (never
    /// happens today: every superseding path fires it) keeps waiting on its
    /// timer only, like the TS's orphaned `finish`.
    async fn sleep(&self, ms: u64, gen: u64) {
        let (id, mut rx) = {
            let mut st = self.lock();
            if gen != st.generation {
                return;
            }
            st.wake_seq += 1;
            let id = st.wake_seq;
            let (tx, rx) = oneshot::channel();
            st.wake = Some((id, tx));
            (id, rx)
        };
        if ms > 0 {
            let timer = tokio::time::sleep(Duration::from_millis(ms));
            tokio::pin!(timer);
            tokio::select! {
                biased;
                r = &mut rx => {
                    if r.is_err() {
                        timer.await;
                    }
                }
                _ = &mut timer => {}
            }
        } else if rx.await.is_err() {
            std::future::pending::<()>().await;
        }
        let mut st = self.lock();
        if st.wake.as_ref().map(|(i, _)| *i) == Some(id) {
            st.wake = None;
        }
    }
}

/// Host facts [`probe_host`] reads; injectable for tests.
pub trait HostProbe: Send + Sync {
    /// `isMock()` (`platform.ts:37-42`).
    fn is_mock(&self) -> bool;
    /// `isRaspberryPi()` (`platform.ts:29-32`).
    fn is_raspberry_pi(&self) -> bool;
    /// `commandExists(cmd)` (`exec.ts:151-157`).
    fn command_exists(&self, cmd: &str) -> bool;
    /// `run("gpioinfo", ["gpiochip0"], { timeoutMs: 2000 })` → stdout, or
    /// `None` when the run failed.
    fn gpioinfo(&self) -> BoxFut<'_, Option<String>>;
}

/// The real host. `tape` receives the `gpioinfo gpiochip0` run
/// (`exec_argv_stderr`, phase-4 §8).
pub struct SystemHost {
    pub tape: Arc<dyn TapeSink>,
}

impl Default for SystemHost {
    fn default() -> Self {
        SystemHost {
            tape: Arc::new(NullTape),
        }
    }
}

/// `isMock()` over an env value + the platform: `XORGATE_MOCK` `1`/`true` →
/// mock, `0`/`false` → not, else mock exactly when not on Linux.
pub fn is_mock_from(flag: Option<&str>, linux: bool) -> bool {
    match flag {
        Some("1") | Some("true") => true,
        Some("0") | Some("false") => false,
        _ => !linux,
    }
}

/// `deviceTreeModel()` + `/raspberry pi/i` (`platform.ts:15-32`) over the
/// raw bytes of `/proc/device-tree/model` (NULs stripped, trimmed).
pub fn is_raspberry_pi_model(bytes: Option<&[u8]>) -> bool {
    let Some(b) = bytes else {
        return false;
    };
    let model = String::from_utf8_lossy(b).replace('\0', "");
    model.trim().to_lowercase().contains("raspberry pi")
}

/// The device-tree model file (`platform.ts:21`).
pub const DT_MODEL: &str = "/proc/device-tree/model";

impl HostProbe for SystemHost {
    fn is_mock(&self) -> bool {
        is_mock_from(
            std::env::var("XORGATE_MOCK").ok().as_deref(),
            cfg!(target_os = "linux"),
        )
    }
    fn is_raspberry_pi(&self) -> bool {
        is_raspberry_pi_model(std::fs::read(DT_MODEL).ok().as_deref())
    }
    fn command_exists(&self, cmd: &str) -> bool {
        crate::exec::command_exists(cmd)
    }
    fn gpioinfo(&self) -> BoxFut<'_, Option<String>> {
        Box::pin(async move {
            const ARGV: [&str; 2] = ["gpioinfo", "gpiochip0"];
            let res = crate::exec::run(ARGV[0], &ARGV[1..], Some(GPIOINFO_TIMEOUT)).await;
            self.tape
                .exec_with_stderr(&ARGV, &res.stdout, &res.stderr, res.exit_code);
            (!res.failed).then_some(res.stdout)
        })
    }
}

/// The `[used]` adjustment (CF50): drop every `gpioinfo` line whose consumer
/// is [`LED_CONSUMER`] — the agent's own held request — before the TS
/// `parseBusyPins` (core, unchanged) sees the text. Any other consumer still
/// counts, so a foreign holder refuses with the TS reason text verbatim.
///
/// libgpiod 1.6 line shape (bench): `\tline  12:      unnamed  "xorgate-rs"
/// output  active-high [used]` — after `line N:` come the name (quoted or the
/// bare `unnamed`) and the consumer (quoted or the bare `unused`). libgpiod
/// 2.x prints `consumer="xorgate-rs"` (and no `[used]`, so the TS parser
/// reports nothing busy there anyway); both spellings are recognised.
pub fn strip_own_consumer(gpioinfo_output: &str) -> String {
    gpioinfo_output
        .split('\n')
        .filter(|line| line_consumer(line).as_deref() != Some(LED_CONSUMER))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The consumer of one `gpioinfo` line, if it has a quoted one.
fn line_consumer(line: &str) -> Option<String> {
    let needle = format!("consumer=\"{LED_CONSUMER}\"");
    if line.contains(&needle) {
        return Some(LED_CONSUMER.to_string());
    }
    let rest = line.trim_start().strip_prefix("line")?;
    let colon = rest.find(':')?;
    if !rest[..colon].trim().chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut rest = &rest[colon + 1..];
    // Token 1: the name; token 2: the consumer.
    let _name = next_token(&mut rest)?;
    let consumer = next_token(&mut rest)?;
    consumer
        .strip_prefix('"')
        .and_then(|c| c.strip_suffix('"'))
        .map(str::to_string)
}

/// One whitespace-separated token; a `"…"` token may contain spaces.
fn next_token<'a>(rest: &mut &'a str) -> Option<&'a str> {
    let s = rest.trim_start();
    if s.is_empty() {
        return None;
    }
    let end = if let Some(body) = s.strip_prefix('"') {
        body.find('"').map(|i| i + 2)?
    } else {
        s.find(char::is_whitespace).unwrap_or(s.len())
    };
    let (tok, tail) = s.split_at(end);
    *rest = tail;
    Some(tok)
}

/// `probeHost(map)` (`led.ts:420-433`) on the real host: shape → mock → not a
/// Pi → `pinctrl is not installed` → `gpioinfo gpiochip0` busy pins (our own
/// consumer excluded, CF50). Never panics.
pub async fn probe_host(map: &LedPinMap) -> PinMapCheck {
    probe_host_with(map, &SystemHost::default()).await
}

/// [`probe_host`] over injected host facts. The order and reason texts are
/// the TS's verbatim; `gpioinfo` is optional (missing or failing = skip the
/// busy check, `led.ts:428-431`).
pub async fn probe_host_with(map: &LedPinMap, host: &dyn HostProbe) -> PinMapCheck {
    let shape = validate_pin_map(map, &[]);
    if !shape.ok {
        return shape;
    }
    if host.is_mock() {
        return fail("mock mode (XORGATE_MOCK)");
    }
    if !host.is_raspberry_pi() {
        return fail("not a Raspberry Pi");
    }
    if !host.command_exists("pinctrl") {
        return fail("pinctrl is not installed");
    }
    if host.command_exists("gpioinfo") {
        if let Some(stdout) = host.gpioinfo().await {
            return validate_pin_map(map, &parse_busy_pins(&strip_own_consumer(&stdout)));
        }
    }
    PinMapCheck {
        ok: true,
        reason: None,
    }
}

fn fail(reason: &str) -> PinMapCheck {
    PinMapCheck {
        ok: false,
        reason: Some(reason.to_string()),
    }
}

/// The agent's driver for `map` (the worker's `new LedDriver({ map })`,
/// `worker.ts:603-610`, with the Rust writer choice):
/// - [`WriterChoice::Pinctrl`] — exactly the TS driver ([`probe_host`] +
///   [`PinctrlWriter`]).
/// - [`WriterChoice::Gpiocdev`] — [`probe_host`] first (same inert reasons
///   in the same order as the TS), then [`GpiocdevWriter::open`]; an open
///   failure is the inert reason (no retry). The request is held by the
///   returned driver until it is dropped.
///
/// `tape` receives the probe's `gpioinfo` run and every pin write
/// ([`TapedWriter`]).
pub fn agent_driver(
    map: LedPinMap,
    choice: WriterChoice,
    chip_override: Option<PathBuf>,
    tape: Arc<dyn TapeSink>,
    log: Option<LedLog>,
) -> LedDriver {
    let host = Arc::new(SystemHost { tape: tape.clone() });
    match choice {
        WriterChoice::Pinctrl => {
            let m = map.clone();
            LedDriver::new(LedDriverOptions {
                map: Some(map),
                writer: Some(Arc::new(TapedWriter {
                    inner: Arc::new(PinctrlWriter),
                    tape,
                })),
                probe: Some(Box::new(move || {
                    let (m, host) = (m.clone(), host.clone());
                    Box::pin(async move { probe_host_with(&m, host.as_ref()).await })
                })),
                log,
            })
        }
        WriterChoice::Gpiocdev => {
            let slot = Arc::new(LazyWriter::default());
            let m = map.clone();
            let s = slot.clone();
            LedDriver::new(LedDriverOptions {
                map: Some(map),
                writer: Some(Arc::new(TapedWriter { inner: slot, tape })),
                probe: Some(Box::new(move || {
                    let (m, host, s, chip) =
                        (m.clone(), host.clone(), s.clone(), chip_override.clone());
                    Box::pin(async move {
                        let check = probe_host_with(&m, host.as_ref()).await;
                        if !check.ok {
                            return check;
                        }
                        match GpiocdevWriter::open(&m, chip.as_deref()) {
                            Ok(w) => {
                                s.set(Arc::new(w));
                                check
                            }
                            Err(reason) => PinMapCheck {
                                ok: false,
                                reason: Some(reason),
                            },
                        }
                    })
                })),
                log,
            })
        }
    }
}

/// Reports every write to the tape (`gpio {pins, high, via}`, phase-4 §8)
/// when the write actually RUNS — the first poll of the returned future,
/// which is when the (lazy, see [`PinWriter`]) inner writer issues it, the
/// same instant the rig's `pinctrl` shim logs the spawn. A write whose
/// future is dropped unpolled (D31) never ran and is not taped. Empty groups
/// are never taped. A writer without a `via` tag (test fakes) is recorded as
/// `gpiocdev`.
pub struct TapedWriter {
    pub inner: Arc<dyn PinWriter>,
    pub tape: Arc<dyn TapeSink>,
}

impl PinWriter for TapedWriter {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        let fut = self.inner.write(pins, high);
        if pins.is_empty() {
            return fut;
        }
        let via = self.inner.via().unwrap_or(GpioVia::Gpiocdev);
        let tape = self.tape.clone();
        let pins = pins.to_vec();
        Box::pin(async move {
            tape.gpio(&pins, high, via);
            fut.await
        })
    }
    fn via(&self) -> Option<GpioVia> {
        self.inner.via()
    }
}

/// A writer filled in by the probe (the gpiocdev request is opened only once
/// the host probe passed). Unfilled = every write fails (unreachable: the
/// driver writes only after a successful probe).
#[derive(Default)]
pub struct LazyWriter {
    inner: std::sync::OnceLock<Arc<dyn PinWriter>>,
}

impl LazyWriter {
    pub fn set(&self, w: Arc<dyn PinWriter>) {
        let _ = self.inner.set(w);
    }
}

impl PinWriter for LazyWriter {
    fn write(&self, pins: &[u32], high: bool) -> BoxFut<'static, bool> {
        match self.inner.get() {
            Some(w) => w.write(pins, high),
            None => Box::pin(async { false }),
        }
    }
    fn via(&self) -> Option<GpioVia> {
        self.inner
            .get()
            .map_or(Some(GpioVia::Gpiocdev), |w| w.via())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinctrl_argv_is_the_ts_argv() {
        assert_eq!(
            PinctrlWriter::args(&[12, 16], true),
            ["set", "12,16", "op", "dh"].map(String::from)
        );
        assert_eq!(
            PinctrlWriter::args(&[21], false),
            ["set", "21", "op", "dl"].map(String::from)
        );
    }

    #[test]
    fn writer_choice_env() {
        assert_eq!(WriterChoice::from_env_value(None), WriterChoice::Gpiocdev);
        assert_eq!(
            WriterChoice::from_env_value(Some("pinctrl")),
            WriterChoice::Pinctrl
        );
        assert_eq!(
            WriterChoice::from_env_value(Some("gpiocdev")),
            WriterChoice::Gpiocdev
        );
        assert_eq!(
            WriterChoice::from_env_value(Some("")),
            WriterChoice::Gpiocdev
        );
    }

    #[test]
    fn mock_and_model() {
        assert!(is_mock_from(Some("1"), true));
        assert!(is_mock_from(Some("true"), true));
        assert!(!is_mock_from(Some("0"), false));
        assert!(!is_mock_from(Some("false"), false));
        assert!(!is_mock_from(None, true));
        assert!(is_mock_from(Some("yes"), false));
        assert!(is_raspberry_pi_model(Some(
            b"Raspberry Pi Compute Module 5 Rev 1.0\0"
        )));
        assert!(is_raspberry_pi_model(Some(b"  RASPBERRY PI 4 Model B\n")));
        assert!(!is_raspberry_pi_model(Some(b"Generic ARM board\0")));
        assert!(!is_raspberry_pi_model(None));
    }

    #[derive(Default)]
    struct GpioTape(Mutex<Vec<(Vec<u32>, bool, GpioVia)>>);
    impl TapeSink for GpioTape {
        fn gpio(&self, pins: &[u32], high: bool, via: GpioVia) {
            self.0.lock().unwrap().push((pins.to_vec(), high, via));
        }
    }
    struct Ok_;
    impl PinWriter for Ok_ {
        fn write(&self, _pins: &[u32], _high: bool) -> BoxFut<'static, bool> {
            Box::pin(async { true })
        }
        fn via(&self) -> Option<GpioVia> {
            Some(GpioVia::Pinctrl)
        }
    }

    #[tokio::test]
    async fn taped_writer_reports_every_non_empty_write_when_it_runs() {
        let tape = Arc::new(GpioTape::default());
        let w = TapedWriter {
            inner: Arc::new(Ok_),
            tape: tape.clone(),
        };
        let fut = w.write(&[12, 16], true);
        assert!(
            tape.0.lock().unwrap().is_empty(),
            "not taped until the write runs"
        );
        assert!(fut.await);
        assert!(w.write(&[], false).await);
        assert_eq!(
            *tape.0.lock().unwrap(),
            vec![(vec![12, 16], true, GpioVia::Pinctrl)]
        );
        let lazy = LazyWriter::default();
        assert_eq!(lazy.via(), Some(GpioVia::Gpiocdev));
        assert!(
            !lazy.write(&[12], true).await,
            "unfilled lazy writer fails closed"
        );
    }

    /// D31: a write whose future is dropped unpolled (a superseded first
    /// step) never ran — the tape must not claim it did.
    #[test]
    fn taped_writer_leaves_no_entry_for_a_dropped_write() {
        let tape = Arc::new(GpioTape::default());
        let w = TapedWriter {
            inner: Arc::new(PinctrlWriter),
            tape: tape.clone(),
        };
        drop(w.write(&[12], true));
        assert!(tape.0.lock().unwrap().is_empty());
    }

    #[test]
    fn open_with_a_missing_chip_override_is_inert_not_a_panic() {
        let err = GpiocdevWriter::open(
            &LedPinMap::status_led_map(),
            Some(Path::new("/run/xorgate-rs-no-gpio")),
        )
        .unwrap_err();
        assert_eq!(err, CHIP_NOT_FOUND);
    }

    #[test]
    fn open_on_a_non_chip_path_fails_with_a_reason() {
        let err = GpiocdevWriter::open(&LedPinMap::status_led_map(), Some(Path::new("/dev/null")))
            .unwrap_err();
        assert!(err.contains("/dev/null"), "{err}");
    }
}
