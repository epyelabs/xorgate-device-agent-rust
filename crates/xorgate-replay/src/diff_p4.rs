//! Phase 4 diff streams (WP6, `phase-4-operations.md` §8 "Diff streams
//! added"): `led` (the pinctrl shim's pin-level timeline + the `[led] …`
//! transition lines), `at` (the fake modem's command log). `sudo` (argv exact)
//! stays in `diff.rs`; its set-clock epoch is normalised there
//! ([`normalize_setclock`]).
//!
//! | stream | exact | tolerated |
//! |---|---|---|
//! | `led` lines | the ordered `[led] a → b (colour pattern)` transitions; the multiset of every other `[led] …` line (map, inert reason, write failure) | — |
//! | `led` runs | run count; per run, the groups (one `apply()` = its rise + fall `pinctrl set` writes) of the common prefix | T15: per-step interval ±`step_ms` (`setTimeout` vs tokio scheduling + the pinctrl spawn); a run's start ±`phase_ms` and its length (a repeating pattern runs until the next state, which each agent sees on its OWN 1 s heartbeat phase, worker.ts:633) |
//! | `at` | the command sequence; the modem's end state | — (a forbidden command or a text-mode CMGRO FAILS on either side) |
//!
//! A run starts at every group that drives all three channels: each new
//! colour/pattern starts from "levels unknown" and writes all of r, g, b
//! (`led.ts` `stop()` → `lastLevels = null`), as do white toggles and blanks —
//! identically on both sides.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::diff::{Capture, Mismatch, Report, UNTOLERATED};
use crate::tolerances::Table;

/// Writes closer than this belong to one `apply()` (rise then fall; the
/// shortest pattern leg is 120 ms, `led.ts` `ledTimeline`).
const GROUP_GAP_MS: i64 = 60;

/// One `pinctrl set <pins> op dh|dl`.
#[derive(Debug, Clone, PartialEq)]
pub struct PinWrite {
    pub w: i64,
    pub pins: Vec<u32>,
    pub high: bool,
}

/// One fake-modem log record.
#[derive(Debug, Clone, PartialEq)]
pub struct AtRec {
    pub cmd: String,
    pub state: Value,
    pub forbidden: bool,
    pub fault: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct P4Capture {
    pub writes: Vec<PinWrite>,
    /// pinctrl calls that are not `set … op dh|dl`.
    pub pinctrl_other: Vec<String>,
    /// `[led] … → …` lines, in order.
    pub transitions: Vec<String>,
    /// Every other `[led] …` line.
    pub led_other: Vec<String>,
    pub at: Vec<AtRec>,
}

/// The `[led] …` text of a log line (any prefix the logger adds dropped).
fn led_line(l: &str) -> Option<&str> {
    l.find("[led] ").map(|i| l[i..].trim_end())
}

impl P4Capture {
    pub fn load(dir: &Path) -> Self {
        let mut c = P4Capture::default();
        for l in std::fs::read_to_string(dir.join("led.log"))
            .unwrap_or_default()
            .lines()
        {
            let Some((w, argv)) = l.split_once(' ') else {
                continue;
            };
            let w: i64 = w.parse().unwrap_or(0);
            let a: Vec<&str> = argv.split_whitespace().collect();
            match a.as_slice() {
                ["set", pins, "op", lvl @ ("dh" | "dl")] => c.writes.push(PinWrite {
                    w,
                    pins: pins.split(',').filter_map(|p| p.parse().ok()).collect(),
                    high: *lvl == "dh",
                }),
                _ => c.pinctrl_other.push(argv.to_string()),
            }
        }
        for f in ["stdout.log", "stderr.log"] {
            for l in std::fs::read_to_string(dir.join(f))
                .unwrap_or_default()
                .lines()
            {
                if let Some(t) = led_line(l) {
                    if t.contains(" → ") {
                        c.transitions.push(t.to_string());
                    } else {
                        c.led_other.push(t.to_string());
                    }
                }
            }
        }
        c.led_other.sort();
        for l in std::fs::read_to_string(dir.join("at.ndjson"))
            .unwrap_or_default()
            .lines()
        {
            let Ok(v) = serde_json::from_str::<Value>(l) else {
                continue;
            };
            c.at.push(AtRec {
                cmd: v["cmd"].as_str().unwrap_or("").to_string(),
                state: v["state"].clone(),
                forbidden: v["forbidden"].as_bool().unwrap_or(false),
                fault: v["fault"].as_str().map(str::to_string),
            });
        }
        c
    }
}

/// One `apply()`: its writes, rendered `+12,16 -21`.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub w: i64,
    pub text: String,
    pub pins: Vec<u32>,
}

pub fn groups(writes: &[PinWrite]) -> Vec<Group> {
    // One `apply()` writes at most ONE rise group then at most ONE fall group,
    // on disjoint pins (`led.ts` apply → writeGroup(rise) && writeGroup(fall)):
    // a write joins the previous one only in exactly that shape (a lone rise
    // followed within GROUP_GAP_MS by a fall on other pins). Two rises, or a
    // fall then anything, are two applies — e.g. a blink step's `+12,16` and
    // the next state's `+21 -12,16` 30 ms later (WP6 JS-vs-Rust p4-led-ladder).
    let mut out: Vec<Group> = Vec::new();
    let mut open: Option<(i64, bool)> = None; // (last w, group is a lone rise)
    for wr in writes {
        let part = format!(
            "{}{}",
            if wr.high { '+' } else { '-' },
            wr.pins
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        let joins = match (out.last(), open) {
            (Some(g), Some((lw, lone_rise))) => {
                lone_rise
                    && !wr.high
                    && wr.w - lw < GROUP_GAP_MS
                    && !wr.pins.iter().any(|p| g.pins.contains(p))
            }
            _ => false,
        };
        if joins {
            let g = out.last_mut().expect("group");
            g.text.push(' ');
            g.text.push_str(&part);
            g.pins.extend(&wr.pins);
            open = Some((wr.w, false));
        } else {
            out.push(Group {
                w: wr.w,
                text: part,
                pins: wr.pins.clone(),
            });
            open = Some((wr.w, wr.high));
        }
    }
    out
}

/// Runs: a group driving three distinct pins starts a new one.
pub fn runs(groups: &[Group]) -> Vec<Vec<Group>> {
    let mut out: Vec<Vec<Group>> = Vec::new();
    for g in groups {
        let mut p = g.pins.clone();
        p.sort_unstable();
        p.dedup();
        if p.len() >= 3 || out.is_empty() {
            out.push(vec![g.clone()]);
        } else if let Some(r) = out.last_mut() {
            r.push(g.clone());
        }
    }
    out
}

fn mm(stream: &str, at: String, path: &str, a: String, b: String, tol: String) -> Mismatch {
    Mismatch {
        stream: stream.into(),
        at,
        path: path.into(),
        a,
        b,
        tol,
    }
}

/// The `led` / `at` comparison.
pub fn diff_p4(a: &P4Capture, b: &P4Capture, table: &Table, rep: &mut Report) {
    diff_led(a, b, table, rep);
    diff_at(a, b, rep);
}

/// T15 bounds: `(row id, step_ms, drift_ms_per_tick, spawn_ms)`.
fn led_bounds(table: &Table) -> Option<(String, f64, f64, f64)> {
    let r = table.of_kind("p4", "led").next()?;
    Some((
        r.id.clone(),
        *r.tolerance.get("step_ms")?,
        *r.tolerance.get("drift_ms_per_tick")?,
        *r.tolerance.get("spawn_ms")?,
    ))
}

/// The LED pin map the driver logged (`[led] rgb-gpio r=GPIO12 g=GPIO21
/// b=GPIO16 …`, led.ts:291-296), else the Argus CM5 default.
fn pin_map(c: &P4Capture) -> [u32; 3] {
    let num = |l: &str, k: &str| -> Option<u32> {
        let i = l.find(k)? + k.len();
        l[i..]
            .split(|ch: char| !ch.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    c.led_other
        .iter()
        .find_map(|l| Some([num(l, "r=GPIO")?, num(l, "g=GPIO")?, num(l, "b=GPIO")?]))
        .unwrap_or([12, 21, 16])
}

/// The first `apply()` of a `play(colour, …)` from "levels unknown": the
/// colour's pins high, the others low, each listed r, g, b (led.ts:351-381).
fn first_group(colour: &str, pins: [u32; 3]) -> Option<String> {
    let ch: [bool; 3] = match colour {
        "off" => [false, false, false],
        "red" => [true, false, false],
        "green" => [false, true, false],
        "blue" => [false, false, true],
        "purple" => [true, false, true],
        "cyan" => [false, true, true],
        "white" => [true, true, true],
        _ => return None,
    };
    let join = |on: bool| {
        (0..3)
            .filter(|&i| ch[i] == on)
            .map(|i| pins[i].to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let (rise, fall) = (join(true), join(false));
    Some(match (rise.is_empty(), fall.is_empty()) {
        (false, false) => format!("+{rise} -{fall}"),
        (false, true) => format!("+{rise}"),
        (true, _) => format!("-{fall}"),
    })
}

/// `[led] <from> → <to> (<colour> <pattern>)` → (from, to, colour).
fn parse_transition(t: &str) -> Option<(String, String, String)> {
    let body = t.strip_prefix("[led] ")?;
    let (from, rest) = body.split_once(" → ")?;
    let (to, paren) = rest.split_once(" (")?;
    let colour = paren.split_whitespace().next()?;
    Some((from.to_string(), to.to_string(), colour.to_string()))
}

/// Heartbeat ticks a transition may legitimately land apart on the two
/// agents (T15 basis): 0 for a directly-fed rung; for a rung whose input is
/// itself produced on an own-phase timer, the hops of that chain —
/// undervoltage / thermal: the shed tick (manager.ts:612-615, 1 s) → the
/// publish loop's noteRecordingStatus (worker.ts:1207, 1 s) → the heartbeat:
/// 2; leaving `network` on reconnect: mqtt.js `reconnectPeriod: 2000`
/// (mqtt.ts:63) → up to 2.
fn chain_ticks(from: &str, to: &str) -> i64 {
    let shed = |r: &str| matches!(r, "undervoltage" | "thermal");
    if shed(from) || shed(to) || from == "network" {
        2
    } else {
        0
    }
}

/// Per side: (run index → transition index) for the runs a logged render
/// started (matched in order by the colour's first group); other runs are
/// unlogged event-driven ones (the config-applied flash, a blank).
fn logged_runs(runs: &[Vec<Group>], transitions: &[String], pins: [u32; 3]) -> Vec<Option<usize>> {
    let mut out = vec![None; runs.len()];
    let mut t = 0;
    for (i, r) in runs.iter().enumerate() {
        let Some((_, _, colour)) = transitions.get(t).and_then(|x| parse_transition(x)) else {
            break;
        };
        if first_group(&colour, pins).as_deref() == Some(r[0].text.as_str()) {
            out[i] = Some(t);
            t += 1;
        }
    }
    out
}

fn diff_led(a: &P4Capture, b: &P4Capture, table: &Table, rep: &mut Report) {
    let n = a.transitions.len().max(b.transitions.len());
    for i in 0..n {
        let (x, y) = (a.transitions.get(i), b.transitions.get(i));
        if x != y {
            rep.push(mm(
                "led",
                format!("transition #{i}"),
                "[led]",
                x.cloned().unwrap_or_default(),
                y.cloned().unwrap_or_default(),
                UNTOLERATED.into(),
            ));
        }
    }
    if a.led_other != b.led_other {
        rep.push(mm(
            "led",
            "lines".into(),
            "[led] (other)",
            a.led_other.join(" | "),
            b.led_other.join(" | "),
            UNTOLERATED.into(),
        ));
    }
    if a.pinctrl_other != b.pinctrl_other {
        rep.push(mm(
            "led",
            "pinctrl".into(),
            "(argv)",
            a.pinctrl_other.join(" | "),
            b.pinctrl_other.join(" | "),
            UNTOLERATED.into(),
        ));
    }
    let (ga, gb) = (groups(&a.writes), groups(&b.writes));
    let (ra, rb) = (runs(&ga), runs(&gb));
    let mut compared = a.transitions.len().min(b.transitions.len());
    if ra.len() != rb.len() {
        rep.push(mm(
            "led",
            "runs".into(),
            "(count)",
            ra.len().to_string(),
            rb.len().to_string(),
            UNTOLERATED.into(),
        ));
    }
    let bounds = led_bounds(table);
    let id = bounds.as_ref().map(|(i, ..)| i.clone());
    let (step_ms, drift, spawn_ms) = bounds
        .as_ref()
        .map(|(_, s, d, sp)| (*s as i64, *d as i64, *sp as i64))
        .unwrap_or((0, 0, 0));
    // Per run: the signed sum of the per-step interval differences (A − B)
    // over the compared legs — what the measured leg jitter accumulates to.
    let mut cum: Vec<i64> = vec![0; ra.len().min(rb.len())];
    let mut max_step_delta = 0i64;
    // Groups + per-step intervals of every run (structure, both kinds).
    for i in 0..ra.len().min(rb.len()) {
        let (x, y) = (&ra[i], &rb[i]);
        compared += 1;
        for j in 0..x.len().min(y.len()) {
            if x[j].text != y[j].text {
                rep.push(mm(
                    "led",
                    format!("run #{i} group #{j}"),
                    "(pins)",
                    x[j].text.clone(),
                    y[j].text.clone(),
                    UNTOLERATED.into(),
                ));
                break;
            }
            if j > 0 {
                let (ia, ib) = (x[j].w - x[j - 1].w, y[j].w - y[j - 1].w);
                compared += 1;
                max_step_delta = max_step_delta.max((ia - ib).abs());
                cum[i] += ia - ib;
                if (ia - ib).abs() > step_ms {
                    rep.push(mm(
                        "led",
                        format!("run #{i} step #{j}"),
                        "(interval ms)",
                        ia.to_string(),
                        ib.to_string(),
                        UNTOLERATED.into(),
                    ));
                }
            }
        }
    }
    // review-wp6 M1: run START instants anchored to each agent's own
    // heartbeat grid. A logged render that is not `none → …` is drawn by
    // tick() from the 1 s heartbeat (worker.ts:633-638, led-manager.ts:56-73),
    // so on each side consecutive such starts are k·1000 ms apart, ± the
    // render's first `pinctrl` spawn latency (`spawn_ms`, led.ts:219-225: the
    // shim stamps the write after the process spawn, host-load dependent) +
    // k·drift; and the cross-agent offset of a run equals the base one (the
    // two phases) ± the same slack. `spawn_ms` < 500 keeps a whole-heartbeat
    // shift (1000 ± spawn_ms) disjoint from an on-phase render (0 ± spawn_ms). A render one heartbeat late
    // shifts that offset by 1000 ms: UNTOLERATED, unless the rung's input is
    // produced on an own-phase timer (`chain_ticks`, cited T15). Event-driven
    // runs (`none → …` after start()/the overlay, the flash itself, a blank)
    // are anchored to that agent's own async event, not comparable on the
    // LED stream: their structure and steps are compared above, their
    // anchors elsewhere (connection, config-apply events).
    let (pa, pb) = (pin_map(a), pin_map(b));
    let (la, lb) = (
        logged_runs(&ra, &a.transitions, pa),
        logged_runs(&rb, &b.transitions, pb),
    );
    let hb = |l: &[Option<usize>], tr: &[String]| -> BTreeMap<usize, usize> {
        l.iter()
            .enumerate()
            .filter_map(|(run, t)| {
                let t = (*t)?;
                (!tr.get(t)?.starts_with("[led] none → ")).then_some((t, run))
            })
            .collect()
    };
    let (ha, hbm) = (hb(&la, &a.transitions), hb(&lb, &b.transitions));
    let mut prev: Option<(usize, i64, i64, i64)> = None; // (t, start A, start B, Δ)
    let mut base: Option<(usize, i64, i64, i64)> = None; // the last on-phase render
    for (&t, &rai) in &ha {
        let Some(&rbi) = hbm.get(&t) else { continue };
        let (sa, sb) = (ra[rai][0].w, rb[rbi][0].w);
        let d = sa - sb;
        compared += 1;
        let Some((pt, psa, psb, pd)) = prev else {
            prev = Some((t, sa, sb, d));
            base = Some((t, sa, sb, d));
            rep.inputs
                .entry("led heartbeat offset A−B (first heartbeat render)".into())
                .or_insert_with(|| format!("{d} ms"));
            continue;
        };
        let (from, to, _) = parse_transition(&a.transitions[t]).unwrap_or_default();
        // Own grids: k heartbeats since the previous logged render.
        for (side, s, ps) in [("A", sa, psa), ("B", sb, psb)] {
            let k = ((s - ps) as f64 / 1000.0).round() as i64;
            let off = (s - ps) - 1000 * k;
            if off.abs() > spawn_ms + drift * k.abs() {
                rep.push(mm(
                    "led",
                    format!("transition #{t} start ({side} grid)"),
                    &format!("{from} → {to}"),
                    format!("{} ms after #{pt}", s - ps),
                    format!("{k} heartbeats ± {}", spawn_ms + drift * k.abs()),
                    UNTOLERATED.into(),
                ));
            }
        }
        // The offset reference is the last render judged ON the base phase
        // (`base`): a chained late render does not move it, so the next
        // direct render is judged against the agents' real phase again.
        let (_, bsa, _, bd) = base.unwrap_or((pt, psa, psb, pd));
        let k = ((sa - bsa) as f64 / 1000.0).round().abs() as i64;
        let slack = spawn_ms + drift * k;
        let shift = d - bd;
        let m = (shift as f64 / 1000.0).round() as i64;
        if shift.abs() > slack {
            let tol = match &id {
                Some(id)
                    if m != 0
                        && (shift - 1000 * m).abs() <= slack
                        && m.abs() <= chain_ticks(&from, &to) =>
                {
                    id.clone()
                }
                _ => UNTOLERATED.to_string(),
            };
            rep.push(mm(
                "led",
                format!("transition #{t} start"),
                &format!("{from} → {to}"),
                format!("A−B {d} ms"),
                format!(
                    "base A−B {bd} ms ({m:+} heartbeat(s), chain allows {})",
                    chain_ticks(&from, &to)
                ),
                tol,
            ));
        } else {
            base = Some((t, sa, sb, d));
        }
        prev = Some((t, sa, sb, d));
    }
    // A repeating pattern runs until the next state, so a run's group count
    // follows its duration (the gap to the next run's start, whose offset
    // the heartbeat rule judged): tolerated only when the count difference is
    // what that duration difference buys at the pattern's shortest leg (+1
    // for the boundary).
    for i in 0..ra.len().min(rb.len()) {
        let (x, y) = (&ra[i], &rb[i]);
        if x.len() == y.len() {
            continue;
        }
        let (da, db) = match (ra.get(i + 1), rb.get(i + 1)) {
            (Some(nx), Some(ny)) => (nx[0].w - x[0].w, ny[0].w - y[0].w),
            // The last run: cut by the stop (both agents together).
            _ => (
                ga.last().map(|g| g.w).unwrap_or(0) - x[0].w,
                gb.last().map(|g| g.w).unwrap_or(0) - y[0].w,
            ),
        };
        let leg = x
            .windows(2)
            .chain(y.windows(2))
            .map(|w| w[1].w - w[0].w)
            .filter(|d| *d > 0)
            .min()
            .unwrap_or(120)
            .max(1);
        let span = (da - db).abs() + cum.get(i).copied().unwrap_or(0).abs();
        let allowed = (span + leg - 1) / leg + 1;
        let ok = (x.len() as i64 - y.len() as i64).abs() <= allowed;
        rep.push(mm(
            "led",
            format!("run #{i} length"),
            "(groups / ms)",
            format!("{} / {da}", x.len()),
            format!("{} / {db}", y.len()),
            match (&id, ok) {
                (Some(id), true) => id.clone(),
                _ => UNTOLERATED.into(),
            },
        ));
    }
    rep.inputs
        .entry("led max per-step |Δ| (T15 step_ms)".into())
        .or_insert_with(|| format!("{max_step_delta} ms (bound {step_ms})"));
    rep.stats.entry("led".into()).or_default().compared += compared;
}

fn diff_at(a: &P4Capture, b: &P4Capture, rep: &mut Report) {
    for (side, c) in [("A", a), ("B", b)] {
        for (i, r) in c.at.iter().enumerate() {
            if r.forbidden || r.fault.is_some() {
                rep.push(mm(
                    "at",
                    format!("{side} command #{i}"),
                    "(read-only contract)",
                    if side == "A" {
                        r.cmd.clone()
                    } else {
                        String::new()
                    },
                    if side == "B" {
                        r.cmd.clone()
                    } else {
                        String::new()
                    },
                    UNTOLERATED.into(),
                ));
            }
        }
    }
    let n = a.at.len().max(b.at.len());
    for i in 0..n {
        let (x, y) = (a.at.get(i), b.at.get(i));
        if x.map(|r| &r.cmd) != y.map(|r| &r.cmd) {
            rep.push(mm(
                "at",
                format!("command #{i}"),
                "(cmd)",
                x.map(|r| r.cmd.clone()).unwrap_or_default(),
                y.map(|r| r.cmd.clone()).unwrap_or_default(),
                UNTOLERATED.into(),
            ));
            break;
        }
    }
    let (ea, eb) = (
        a.at.last().map(|r| r.state.to_string()),
        b.at.last().map(|r| r.state.to_string()),
    );
    if ea != eb {
        rep.push(mm(
            "at",
            "end".into(),
            "(modem state)",
            ea.unwrap_or_default(),
            eb.unwrap_or_default(),
            UNTOLERATED.into(),
        ));
    }
    rep.stats.entry("at".into()).or_default().compared += a.at.len().min(b.at.len());
}

/// `sudo -n /usr/local/bin/xorgate-setclock <epoch>` logged at `w` ms →
/// `… <epoch:now+N>` with `N = epoch − round(w / 1000)`: the target is the
/// source's time at THAT agent's tick (`clock-step.ts:186`
/// `Math.round(targetMs / 1000)`), and the two agents tick on their own 30 s
/// phases, so only the offset is comparable. Other argv unchanged.
pub fn normalize_setclock(w: i64, argv: &str) -> String {
    let parts: Vec<&str> = argv.split(' ').collect();
    match parts.as_slice() {
        [flag, helper, epoch]
            if helper.ends_with("xorgate-setclock")
                && epoch.bytes().all(|c| c.is_ascii_digit()) =>
        {
            let e: i64 = epoch.parse().unwrap_or(0);
            let now = (w as f64 / 1000.0).round() as i64;
            let d = e - now;
            format!(
                "{flag} {helper} <epoch:now{}{d}>",
                if d >= 0 { "+" } else { "" }
            )
        }
        _ => argv.to_string(),
    }
}

/// The offset of a normalised set-clock argv.
pub fn setclock_offset(argv: &str) -> Option<i64> {
    let i = argv.find("<epoch:now")?;
    argv[i + "<epoch:now".len()..]
        .trim_end_matches('>')
        .trim_start_matches('+')
        .parse()
        .ok()
}

/// T16: two normalised set-clock argv whose offsets differ by ≤ `epoch_s`.
pub fn setclock_tolerated(table: &Table, x: &str, y: &str) -> Option<String> {
    let r = table.of_kind("p4", "sudo").next()?;
    let lim = *r.tolerance.get("epoch_s")? as i64;
    let (ox, oy) = (setclock_offset(x)?, setclock_offset(y)?);
    let stem = |s: &str| s.split("<epoch:").next().unwrap_or("").to_string();
    (stem(x) == stem(y) && (ox - oy).abs() <= lim).then(|| r.id.clone())
}

/// T16 (events half, review-wp6 S): `[clock] ignoring implausible <src> time
/// <ISO>` (`clock-step.ts:272-276`) prints `targetMs = now() + modem.offsetMs`
/// (NITZ, `:262`; GPS: `utcMs + ageMs`) taken in the SAME tick as the event's
/// own state.json stamp (`onEvent` → `Date.now()`, worker.ts:546-551). So
/// `ISO − the event's own ts` is the source offset — the same file / NMEA on
/// both agents — whatever each agent's tick phase. An A-only and a B-only
/// line of that shape (same level + source) whose anchored offsets agree
/// within `anchor_ms` are re-cited to T16; nothing is compared across agents
/// by wall time.
pub fn retag_clock_iso(table: &Table, a: &Capture, b: &Capture, rep: &mut Report) {
    // T16 itself, never another p4 events rule (T16/trust-published).
    let Some(r) = table.of_kind("p4", "events").find(|r| r.id == "T16") else {
        return;
    };
    let Some(lim) = r.tolerance.get("anchor_ms").map(|v| *v as i64) else {
        return;
    };
    // `level: msg` → (head, ISO − own ts) for that capture's event.
    let anchored = |c: &Capture, path: &str| -> Option<(String, i64)> {
        path.find("[clock] ignoring implausible ")?;
        let (head, iso) = path.rsplit_once(" time ")?;
        let (level, msg) = path.split_once(": ")?;
        let at = c.events.iter().find(|(_, l, m)| l == level && m == msg)?.0;
        Some((head.to_string(), iso_ms(iso)? - at))
    };
    let idx: Vec<usize> = (0..rep.mismatches.len())
        .filter(|&i| {
            let m = &rep.mismatches[i];
            m.stream == "events"
                && m.tol == UNTOLERATED
                && m.path.contains("[clock] ignoring implausible ")
        })
        .collect();
    let mut used = vec![false; rep.mismatches.len()];
    for &i in &idx {
        for &j in &idx {
            if i == j || used[i] || used[j] {
                continue;
            }
            let (mi, mj) = (&rep.mismatches[i], &rep.mismatches[j]);
            // i: on A only; j: on B only.
            if !(mi.a == "1" && mi.b == "0" && mj.a == "0" && mj.b == "1") {
                continue;
            }
            if let (Some((hi, oi)), Some((hj, oj))) = (anchored(a, &mi.path), anchored(b, &mj.path))
            {
                if hi == hj && (oi - oj).abs() <= lim {
                    used[i] = true;
                    used[j] = true;
                }
            }
        }
    }
    for (k, u) in used.iter().enumerate() {
        if *u {
            rep.retag(k, r.id.clone());
        }
    }
}

/// The id of the clock-trust "published N s" row (F-P2-ROUND): a rule of its
/// own in the T16 family, so T16's own bounds are untouched.
pub const TRUST_PUBLISHED_ID: &str = "T16/trust-published";

/// `[clock] <src> synchronized <T> s after boot — clock trusted (published
/// <P> s on an untrusted clock)` (ts:src/agent/clock-trust.ts:121 @09da38d ↔
/// crates/xorgate-core/src/clock_trust.rs `late_trusted_event`) →
/// `(src, T, P)`, canonical digits only (the text must re-format to itself);
/// any other text → None.
pub fn late_trust_text(m: &str) -> Option<(String, i64, i64)> {
    let r = m.strip_prefix("[clock] ")?;
    let (src, r) = r.split_once(" synchronized ")?;
    let (t, r) = r.split_once(" s after boot — clock trusted (published ")?;
    let p = r.strip_suffix(" s on an untrusted clock)")?;
    // unsigned: toFixed(0) of a monotonic elapsed never prints a sign
    let (t, p): (u64, u64) = (t.parse().ok()?, p.parse().ok()?);
    let canon = format!(
        "[clock] {src} synchronized {t} s after boot — clock trusted (published {p} s on an untrusted clock)"
    );
    (canon == m && matches!(src, "ntp" | "gps" | "nitz"))
        .then(|| (src.to_string(), t as i64, p as i64))
}

/// T16/trust-published (F-P2-ROUND): the late-trust text's P is
/// `secs(now − start − heldMs)` (ts:src/agent/clock-trust.ts:121) with
/// `heldMs` = the elapsed of the grace flip (:129-132) — so the raw P is the
/// side's OWN trust instant minus its OWN grace-flip instant (the start
/// cancels), each read in the same synchronous call as that event's own
/// state.json stamp (`onEvent` → `Date.now()`, ts:src/worker.ts:477-483;
/// Rust `agent_event_sink` → `clock.wall_ms()`). The trust instant is the
/// step at THAT agent's 30 s stepper tick (ts:src/agent/clock-step.ts:291),
/// whose phase differs per agent (the T16 boot skew), so a raw P near x.5 s
/// rounds to either side of it (`secs` = toFixed(0), :104-106).
///
/// Re-cited to the row ONLY when ALL of these hold; anything else stays
/// UNTOLERATED:
/// - no handoff (one era); the only UNTOLERATED `events` rows naming a
///   late-trust text are exactly two: one A-only (1/0), one B-only (0/1);
/// - each side has exactly ONE `clock trusted` event (it is that row's
///   text) and exactly ONE grace warning ([`crate::diff::grace_text`]), the
///   trust after the warning;
/// - per side, P is what that side's OWN instants print: P ∈
///   [secs(d − instant_ms), secs(d + instant_ms)] with d = the trust
///   event's stamp − the grace event's stamp (`trust_secs` = the pinned
///   toFixed(0));
/// - per side, |T − G − P| ≤ 1 against that side's OWN grace G (each of T,
///   G, P is toFixed(0) of its own raw ms and raw T = raw G + raw P);
/// - across sides: the same src and the same T (the texts differ ONLY in P),
///   |P_A − P_B| ≤ `published_s`, and the two trust instants and the two
///   grace instants each < `apart_ms` apart (one tick, own phases).
pub fn retag_trust_published(
    table: &Table,
    a: &Capture,
    b: &Capture,
    play: &crate::diff::PlayInfo,
    rep: &mut Report,
) {
    let Some(r) = table.rules.iter().find(|r| {
        r.id == TRUST_PUBLISHED_ID
            && r.kind == "p4"
            && crate::tolerances::stream_matches(&r.streams, "events")
    }) else {
        return;
    };
    let tol = |k: &str| r.tolerance.get(k).map(|v| *v as i64);
    let (Some(lim), Some(slack), Some(apart)) =
        (tol("published_s"), tol("instant_ms"), tol("apart_ms"))
    else {
        return;
    };
    if play.handoff.is_some() {
        return;
    }
    let idx: Vec<usize> = (0..rep.mismatches.len())
        .filter(|&i| {
            let m = &rep.mismatches[i];
            m.stream == "events" && m.tol == UNTOLERATED && m.path.contains("clock trusted")
        })
        .collect();
    let [i, j] = idx.as_slice() else { return };
    // (A count, B count) of a multiset row.
    let counts = |k: usize| {
        let m = &rep.mismatches[k];
        (m.at == "multiset").then(|| (m.a.clone(), m.b.clone()))
    };
    let one = |x: &str, y: &str| Some((x.to_string(), y.to_string()));
    // i: on A only; j: on B only.
    let (i, j) = if counts(*i) == one("1", "0") && counts(*j) == one("0", "1") {
        (*i, *j)
    } else if counts(*i) == one("0", "1") && counts(*j) == one("1", "0") {
        (*j, *i)
    } else {
        return;
    };
    struct Side {
        src: String,
        t: i64,
        p: i64,
        trust_at: i64,
        grace_at: i64,
    }
    // `key`: this side's only row, `info: <text>`.
    let side = |c: &Capture, key: &str| -> Option<Side> {
        let trust: Vec<&(i64, String, String)> = c
            .events
            .iter()
            .filter(|(_, _, m)| m.contains("clock trusted"))
            .collect();
        let grace: Vec<&(i64, String, String)> = c
            .events
            .iter()
            .filter(|(_, _, m)| m.starts_with("[clock] no NTP/GPS/NITZ sync "))
            .collect();
        let ([tr], [gr]) = (trust.as_slice(), grace.as_slice()) else {
            return None;
        };
        if tr.1 != "info" || gr.1 != "warn" || format!("{}: {}", tr.1, tr.2) != key {
            return None;
        }
        let (src, t, p) = late_trust_text(&tr.2)?;
        let (g, _n) = crate::diff::grace_text(&gr.2)?;
        let d = tr.0 - gr.0;
        if d <= 0 {
            return None;
        }
        let secs = |ms: i64| -> i64 {
            xorgate_core::clock_trust::trust_secs(ms.max(0) as u64)
                .parse()
                .unwrap_or(i64::MIN)
        };
        let own = (secs(d - slack)..=secs(d + slack)).contains(&p);
        let algebra = crate::diff::trust_tgp_ok(t, g, p);
        (own && algebra).then_some(Side {
            src,
            t,
            p,
            trust_at: tr.0,
            grace_at: gr.0,
        })
    };
    let (Some(x), Some(y)) = (
        side(a, &rep.mismatches[i].path),
        side(b, &rep.mismatches[j].path),
    ) else {
        return;
    };
    if x.src != y.src
        || x.t != y.t
        || x.p == y.p
        || (x.p - y.p).abs() > lim
        || (x.trust_at - y.trust_at).abs() >= apart
        || (x.grace_at - y.grace_at).abs() >= apart
    {
        return;
    }
    rep.retag(i, r.id.clone());
    rep.retag(j, r.id.clone());
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` → epoch ms.
fn iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 24 || b[23] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, se, ms) = (n(11..13)?, n(14..16)?, n(17..19)?, n(20..23)?);
    // days_from_civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + se * 1000 + ms)
}

/// Review-wp4 R4-4: the OBD relay's barrier (obd-gate.py) aligns the two
/// helpers' edges, so the EARLIER side waits for the later one — a latency on
/// either side's helper lifecycle (a late spawn, a late SIGINT) would vanish
/// from every compared stream. The rig owns the evidence instead: each wait
/// is logged in `<run>/<label>/obd-gate.ndjson`, and a barrier TIMEOUT, or a
/// wait above [`OBD_BARRIER_WAIT_MAX_MS`], is UNTOLERATED (stream `obd-gate`).
pub const OBD_BARRIER_WAIT_MAX_MS: i64 = 400;

/// Review-wp4 R5-2: every edge is delivered by both relays at ONE shared GO
/// (obd-gate.py `shared_go`); a relay that reached the GO later than this
/// (descheduled) delivered the edge apart from the other side: UNTOLERATED.
pub const OBD_GO_LATE_MAX_MS: i64 = 50;

/// Review-wp4 R6-1: the ELM327 emulator's ATZ reset sleep (`reset(0.5)`,
/// obd_message.py:547) + 100 ms margin — the extra barrier wait allowed ONLY
/// at the `stopped` status of a helper whose close cut a request in flight.
pub const OBD_ATZ_RESET_MS: i64 = 500;

/// Review-wp4 R7 SHOULD-FIX: at such an edge the cut side must be the WAITER,
/// by at least the smallest early stop measured (454 ms) less the 150 ms a
/// same-instant edge may take: below it, the cut side lagged.
pub const OBD_CUT_WAIT_MIN_MS: i64 = 300;

/// The ELM327 emulator's ATZ reply `ELM327 v1.5\r\r>` (14 B): the ONE stale
/// reply a cut close leaves on the line (review-wp4 R7 NIT).
pub const OBD_ATZ_REPLY_BYTES: i64 = 14;

/// R6-1: the edge numbers of the final `stopped` status (the status edge
/// right before the exit edge) of every helper session (relay pid) the agent
/// signalled while a request was in flight (its last audit requests − ok = 1)
/// — MID-RUN stops only. At the run's final stop (the agents' own shutdown)
/// no side waits out the ATZ reset: every cut final stop seen waited 0–7 ms
/// (stepC p5b-obd-config.r32.jsrs1 edge 35, r33.jsrs1 edge 35,
/// p5b-obd-audit.r33.jsrs1 / p5b-obd-emulator.r33.jsrs1 edge 8), so there the
/// normal 400 ms bound applies and nothing is extended or required.
fn cut_stop_edges(log: &[Value]) -> BTreeSet<i64> {
    let mut last_audit: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    let mut signalled: BTreeSet<i64> = BTreeSet::new();
    let mut prev_status: BTreeMap<i64, i64> = BTreeMap::new();
    let mut stop_edges: BTreeMap<i64, i64> = BTreeMap::new();
    for w in log {
        let pid = w["pid"].as_i64().unwrap_or(-1);
        match w["kind"].as_str() {
            Some("audit-line") => {
                if let (Some(r), Some(o)) = (w["requests"].as_i64(), w["ok"].as_i64()) {
                    last_audit.insert(pid, (r, o));
                }
            }
            Some("helper-exit") if w["signalled"] == true => {
                signalled.insert(pid);
            }
            Some("status") if w["edge"].is_number() => {
                prev_status.insert(pid, w["edge"].as_i64().unwrap_or(-1));
            }
            Some("exit") if w["edge"].is_number() => {
                if let Some(e) = prev_status.get(&pid) {
                    stop_edges.insert(pid, *e);
                }
            }
            _ => {}
        }
    }
    let last_edge = log.iter().filter_map(|w| w["edge"].as_i64()).max();
    let exit_of: BTreeMap<i64, i64> = log
        .iter()
        .filter(|w| w["kind"] == "exit")
        .filter_map(|w| Some((w["pid"].as_i64()?, w["edge"].as_i64()?)))
        .collect();
    stop_edges
        .into_iter()
        .filter(|(pid, _)| {
            signalled.contains(pid)
                && last_audit.get(pid).is_some_and(|(r, o)| r - o == 1)
                && exit_of.get(pid).copied() != last_edge
        })
        .map(|(_, e)| e)
        .collect()
}

/// `requestRate(prev, cur)` (ts:src/agent/collectors/obd.ts:270-278):
/// `Math.round((Δrequests / Δts) * 10) / 10`, None when Δts ≤ 0 or Δreq < 0.
fn request_rate(prev: (f64, f64), cur: (f64, f64)) -> Option<f64> {
    let dt = cur.0 - prev.0;
    let dr = cur.1 - prev.1;
    if dt.is_nan() || dt <= 0.0 || dr < 0.0 {
        return None;
    }
    // JS Math.round: half up (the value is ≥ 0 here)
    Some(((dr / dt) * 10.0 + 0.5).floor() / 10.0)
}

pub fn diff_obd_gate(run: &Path, rep: &mut Report) {
    let load = |label: &str| -> Option<Vec<serde_json::Value>> {
        let text = std::fs::read_to_string(run.join(label).join(crate::p5b::OBD_GATE_LOG)).ok()?;
        Some(
            text.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect(),
        )
    };
    let (a, b) = (load("a"), load("b"));
    if a.is_none() && b.is_none() {
        // Review-wp4 R6 NIT: an OBD run (either side has the rig's OBD
        // emulator serial) with NO relay log on either side would compare the
        // T17 fields without their carry checks: UNTOLERATED, unless the
        // player ran with `--no-gate` (play.json `noGate`).
        let obd = ["a", "b"].iter().any(|l| {
            std::fs::read(run.join(l).join("meta.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|m| m["env"]["XORGATE_OBD_SERIAL"].as_str().is_some())
        });
        let no_gate = std::fs::read(run.join("play.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .is_some_and(|p| p["noGate"] == true);
        if obd && !no_gate {
            rep.push(Mismatch {
                stream: "obd-gate".into(),
                at: "relay logs".into(),
                path: "(relay log present on both sides)".into(),
                a: "missing".into(),
                b: "missing".into(),
                tol: UNTOLERATED.into(),
            });
        }
        return;
    }
    // Review-wp4 R5 NIT: an OBD run (one side has a relay log) whose other
    // side has none is not judged by half: UNTOLERATED.
    for (label, log) in [("A", &a), ("B", &b)] {
        if log.is_none() {
            rep.push(Mismatch {
                stream: "obd-gate".into(),
                at: format!("{label} relay log"),
                path: "(relay log present on both sides)".into(),
                a: "missing".into(),
                b: "present".into(),
                tol: UNTOLERATED.into(),
            });
        }
    }
    let mut summary = Vec::new();
    let mqtt = |dir: &str| -> Vec<Value> {
        std::fs::read_to_string(run.join(dir).join("mqtt.ndjson"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    };
    for (label, dir, log) in [("A", "a", &a), ("B", "b", &b)] {
        let Some(log) = log else { continue };
        // The side's own helper rates, as logged by its relay at receipt.
        let profiles: Vec<(i64, f64)> = log
            .iter()
            .filter(|w| w["kind"] == "profile-rate")
            .filter_map(|w| Some((w["w"].as_i64()?, w["rate_req_s"].as_f64()?)))
            .collect();
        let rates: Vec<f64> = profiles.iter().map(|(_, r)| *r).collect();
        // T17 (orchestrator ruling 2026-10-28): with no cross-side band, a
        // broken helper must still FAIL on its own — every probe rate it
        // printed inside the plausibility band (5–40 req/s; all 398 logged
        // probes lie in 16.8–19.7).
        for (w, r) in profiles.iter().filter(|(_, r)| !(5.0..=40.0).contains(r)) {
            rep.push(Mismatch {
                stream: "obd-gate".into(),
                at: format!("{label} probe at {w}"),
                path: "(helper probe rate plausible)".into(),
                a: r.to_string(),
                b: "5–40 req/s".into(),
                tol: UNTOLERATED.into(),
            });
        }
        // T17's carry check (report path): every
        // `effective.obd.profile.rate_req_s` a side reports must be
        // (numerically exactly) a rate its OWN helper printed. A side that
        // reports one with no profile logged FAILS (R5 NIT: no skip).
        let reported: Vec<(i64, Value)> = mqtt(dir)
            .into_iter()
            .filter(|m| {
                m["topic"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("/config/reported"))
            })
            .filter_map(|m| {
                let p: Value = serde_json::from_str(m["payload"].as_str()?).ok()?;
                Some((
                    m["w"].as_i64()?,
                    p.pointer("/effective/obd/profile/rate_req_s")?.clone(),
                ))
            })
            .collect();
        for (w, v) in reported {
            if !v.as_f64().is_some_and(|x| rates.contains(&x)) {
                rep.push(Mismatch {
                    stream: "obd-gate".into(),
                    at: format!("{label} report at {w}"),
                    path: "(profile rate carried from its helper)".into(),
                    a: v.to_string(),
                    b: format!("one of {rates:?}"),
                    tol: UNTOLERATED.into(),
                });
            }
        }
        // Review-wp4 R5-1: the state path `obd.reqPerSec` is COMPUTED by the
        // agent (ts:src/agent/collectors/obd.ts:666-677 `reqPerSec`): null
        // without a helper; else requestRate() over its two latest audits;
        // else the seed `Math.round(profile.rate_req_s * 10) / 10`. Each value
        // a state snapshot shows must be null, one of the two latest profile
        // rates its own helper printed by then (the newest may not be
        // delivered yet: the relay logs a line on receipt), or the rate of a
        // pair of consecutive audits its own helper printed by then.
        let audits: Vec<(i64, f64, f64)> = log
            .iter()
            .filter(|w| w["kind"] == "audit-line")
            .filter_map(|w| Some((w["w"].as_i64()?, w["ts"].as_f64()?, w["requests"].as_f64()?)))
            .collect();
        let pair_rates: Vec<(i64, f64)> = audits
            .windows(2)
            .filter_map(|p| Some((p[1].0, request_rate((p[0].1, p[0].2), (p[1].1, p[1].2))?)))
            .collect();
        let mut last: Option<Value> = None;
        let text = std::fs::read_to_string(run.join(dir).join("files.ndjson")).unwrap_or_default();
        for l in text.lines() {
            let Ok(f) = serde_json::from_str::<Value>(l) else {
                continue;
            };
            if f["name"] != "state.json" {
                continue;
            }
            let (Some(w), Some(t)) = (f["w"].as_i64(), f["text"].as_str()) else {
                continue;
            };
            let Ok(st) = serde_json::from_str::<Value>(t) else {
                continue;
            };
            let Some(v) = st.pointer("/obd/reqPerSec").cloned() else {
                continue;
            };
            if last.as_ref() == Some(&v) || v.is_null() {
                last = Some(v);
                continue;
            }
            last = Some(v.clone());
            let seen: Vec<f64> = profiles
                .iter()
                .filter(|(pw, _)| *pw <= w)
                .map(|(_, r)| *r)
                .collect();
            let mut ok: Vec<f64> = seen.iter().rev().take(2).copied().collect();
            ok.extend(
                pair_rates
                    .iter()
                    .filter(|(pw, _)| *pw <= w)
                    .map(|(_, r)| *r),
            );
            if !v
                .as_f64()
                .is_some_and(|x| ok.iter().any(|r| (r - x).abs() < 1e-9))
            {
                rep.push(Mismatch {
                    stream: "obd-gate".into(),
                    at: format!("{label} state at {w}"),
                    path: "(state obd.reqPerSec carried from its helper)".into(),
                    a: v.to_string(),
                    b: format!("null or one of {ok:?}"),
                    tol: UNTOLERATED.into(),
                });
            }
        }
    }
    // Review-wp4 R5 NIT: the stale replies each side's drain found, compared
    // across sides (an agent whose stop leaves MORE in-flight replies — e.g.
    // a helper killed before its close — stays visible).
    // A stale reply is EXPLAINED when the side's previous helper was stopped
    // (signalled) with a request in flight (review-wp4 R6-1): the stop
    // signal raises Shutdown out of that request's read, the helper's close
    // ATZ then reads the in-flight request's reply as its own, and the real
    // ATZ reply is left on the line for the next start (python/obd/gate.py
    // :131-136, :187; session.py:90; its last audit has requests − ok = 1).
    // Only UNEXPLAINED stale replies are compared across sides.
    let stale = |log: &Option<Vec<Value>>| -> usize {
        let log: Vec<&Value> = log.iter().flatten().collect();
        let mut n = 0;
        for (i, w) in log.iter().enumerate() {
            if w["kind"] != "drain" || w["bytes"].as_i64().unwrap_or(0) == 0 {
                continue;
            }
            // ONE ATZ reply only (review-wp4 R7 NIT): any other size counts.
            if w["bytes"].as_i64() != Some(OBD_ATZ_REPLY_BYTES) {
                n += 1;
                continue;
            }
            let prev = log[..i]
                .iter()
                .rev()
                .find(|x| x["kind"] == "helper-exit")
                .filter(|x| x["signalled"] == true);
            let cut = prev.and_then(|x| {
                let pid = x["pid"].as_i64()?;
                let a = log[..i]
                    .iter()
                    .rev()
                    .find(|y| y["kind"] == "audit-line" && y["pid"].as_i64() == Some(pid))?;
                Some(a["requests"].as_i64()? - a["ok"].as_i64()? == 1)
            });
            if cut != Some(true) {
                n += 1;
            }
        }
        n
    };
    let (sa, sb) = (stale(&a), stale(&b));
    rep.stats.entry("obd-gate".into()).or_default().compared += 1;
    if sa != sb {
        rep.push(Mismatch {
            stream: "obd-gate".into(),
            at: "drains".into(),
            path: "(helper starts with a stale adapter reply)".into(),
            a: sa.to_string(),
            b: sb.to_string(),
            tol: UNTOLERATED.into(),
        });
    }
    let cut_edges = (
        cut_stop_edges(a.as_deref().unwrap_or(&[])),
        cut_stop_edges(b.as_deref().unwrap_or(&[])),
    );
    for (label, waits) in [("A", a.unwrap_or_default()), ("B", b.unwrap_or_default())] {
        // The relay's pre-spawn drain of the adapter line (fresh state per
        // helper start): summarised; one that never went quiet FAILS.
        let drains: Vec<&serde_json::Value> =
            waits.iter().filter(|w| w["kind"] == "drain").collect();
        if !drains.is_empty() {
            let bytes: i64 = drains.iter().filter_map(|w| w["bytes"].as_i64()).sum();
            let stale = drains
                .iter()
                .filter(|w| w["bytes"].as_i64().unwrap_or(0) > 0)
                .count();
            summary.push(format!(
                "{label} {} helper start(s) drained, {stale} with stale bytes ({bytes} B)",
                drains.len()
            ));
            for w in drains.iter().filter(|w| w["quiet"] == false) {
                rep.push(Mismatch {
                    stream: "obd-gate".into(),
                    at: format!("{label} drain at {}", w["w"]),
                    path: "(adapter line quiet before the helper start)".into(),
                    a: format!("{} B in {} ms, never quiet", w["bytes"], w["ms"]),
                    b: "quiet".into(),
                    tol: UNTOLERATED.into(),
                });
            }
        }
        // R5-2: each edge's shared GO, reached late by a descheduled relay.
        let gos: Vec<serde_json::Value> = waits
            .iter()
            .filter(|w| w["kind"] == "go")
            .cloned()
            .collect();
        let late_max = gos
            .iter()
            .filter_map(|w| w["late_ms"].as_i64())
            .max()
            .unwrap_or(0);
        if !gos.is_empty() {
            summary.push(format!(
                "{label} {} GO(s), max late {late_max} ms",
                gos.len()
            ));
        }
        for w in gos
            .iter()
            .filter(|w| w["late_ms"].as_i64().unwrap_or(0) > OBD_GO_LATE_MAX_MS)
        {
            rep.push(Mismatch {
                stream: "obd-gate".into(),
                at: format!("{label} edge #{} GO", w["edge"]),
                path: "(edge delivered late after its shared GO)".into(),
                a: format!("{} ms", w["late_ms"]),
                b: format!("≤ {OBD_GO_LATE_MAX_MS} ms"),
                tol: UNTOLERATED.into(),
            });
        }
        let waits: Vec<serde_json::Value> = waits
            .into_iter()
            .filter(|w| w["edge"].is_number() && w["kind"] != "go")
            .collect();
        let max = waits
            .iter()
            .filter_map(|w| w["wait_ms"].as_i64())
            .max()
            .unwrap_or(0);
        summary.push(format!(
            "{label} {} edge(s), max wait {max} ms",
            waits.len()
        ));
        let st = rep.stats.entry("obd-gate".into()).or_default();
        st.compared += waits.len() + gos.len();
        for w in &waits {
            let (ms, timeout) = (w["wait_ms"].as_i64().unwrap_or(0), w["timeout"] == true);
            // R6-1 (b): at the final `stopped` status of a helper whose close
            // cut a request in flight — on THIS side only — the cut side is
            // early by the emulator's ATZ reset, so it must be the waiter:
            // 300..=900 ms (review-wp4 R7 SHOULD-FIX), else UNTOLERATED.
            let edge = w["edge"].as_i64().unwrap_or(-1);
            let (own, other) = if label == "A" {
                (&cut_edges.0, &cut_edges.1)
            } else {
                (&cut_edges.1, &cut_edges.0)
            };
            if own.contains(&edge) && !other.contains(&edge) {
                let ok = !timeout
                    && (OBD_CUT_WAIT_MIN_MS..=OBD_BARRIER_WAIT_MAX_MS + OBD_ATZ_RESET_MS)
                        .contains(&ms);
                if ms > OBD_BARRIER_WAIT_MAX_MS || !ok {
                    rep.push(Mismatch {
                        stream: "obd-gate".into(),
                        at: format!("{label} edge #{edge} stopped status"),
                        path: "(barrier wait: own close cut a request in flight)".into(),
                        a: format!("{ms} ms"),
                        b: format!(
                            "{OBD_CUT_WAIT_MIN_MS}–{} ms",
                            OBD_BARRIER_WAIT_MAX_MS + OBD_ATZ_RESET_MS
                        ),
                        tol: if ok {
                            "T8/obd-audit-cycle".into()
                        } else {
                            UNTOLERATED.into()
                        },
                    });
                }
                continue;
            }
            if timeout || ms > OBD_BARRIER_WAIT_MAX_MS {
                rep.push(Mismatch {
                    stream: "obd-gate".into(),
                    at: format!(
                        "{label} edge #{} {}",
                        w["edge"],
                        w["kind"].as_str().unwrap_or("?")
                    ),
                    path: if timeout {
                        "(barrier timeout)".into()
                    } else {
                        "(barrier wait over bound)".into()
                    },
                    a: format!("{ms} ms"),
                    b: format!("≤ {OBD_BARRIER_WAIT_MAX_MS} ms"),
                    tol: UNTOLERATED.into(),
                });
            }
        }
    }
    rep.inputs.insert(
        "obd-gate barrier (rig-owned waits)".into(),
        format!(
            "{}; bounds: wait {OBD_BARRIER_WAIT_MAX_MS} ms, GO late {OBD_GO_LATE_MAX_MS} ms",
            summary.join(", ")
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review-wp4 R4-4: a barrier timeout or a wait above the bound in the
    /// relay's rig-owned log is UNTOLERATED (the reviewer's 2 s-late Rust
    /// helper launch makes the JS side wait ~2 s); small waits pass.
    #[test]
    fn obd_gate_waits_over_the_bound_or_timeouts_fail() {
        let d = std::env::temp_dir().join(format!("obd-gate-diff-{}", std::process::id()));
        for l in ["a", "b"] {
            std::fs::create_dir_all(d.join(l)).unwrap();
        }
        let write = |l: &str, lines: &[&str]| {
            std::fs::write(d.join(l).join(crate::p5b::OBD_GATE_LOG), lines.join("\n")).unwrap()
        };
        let run = || {
            let mut rep = Report::default();
            diff_obd_gate(&d, &mut rep);
            rep.mismatches
                .iter()
                .map(|m| m.path.clone())
                .collect::<Vec<_>>()
        };
        write(
            "a",
            &[r#"{"w":1,"side":"a","edge":1,"kind":"ready","wait_ms":12,"timeout":false}"#],
        );
        write(
            "b",
            &[r#"{"w":1,"side":"b","edge":1,"kind":"ready","wait_ms":0,"timeout":false}"#],
        );
        assert!(run().is_empty());
        write(
            "a",
            &[r#"{"w":1,"side":"a","edge":1,"kind":"ready","wait_ms":2104,"timeout":false}"#],
        );
        assert_eq!(run(), vec!["(barrier wait over bound)".to_string()]);
        write(
            "a",
            &[r#"{"w":1,"side":"a","edge":3,"kind":"exit","wait_ms":3001,"timeout":true}"#],
        );
        assert_eq!(run(), vec!["(barrier timeout)".to_string()]);
        // The pre-spawn drain: quiet (with or without stale bytes) is only
        // summarised and is not an edge; one that never went quiet FAILS.
        write(
            "a",
            &[
                r#"{"w":1,"side":"a","kind":"drain","bytes":3,"ms":320,"quiet":true}"#,
                r#"{"w":2,"side":"a","edge":1,"kind":"ready","wait_ms":0,"timeout":false}"#,
            ],
        );
        write(
            "b",
            &[r#"{"w":1,"side":"b","kind":"drain","bytes":5,"ms":310,"quiet":true}"#],
        );
        assert!(run().is_empty());
        write(
            "b",
            &[r#"{"w":1,"side":"b","edge":1,"kind":"ready","wait_ms":0,"timeout":false}"#],
        );
        write(
            "a",
            &[r#"{"w":1,"side":"a","kind":"drain","bytes":0,"ms":2000,"quiet":false}"#],
        );
        assert_eq!(
            run(),
            vec!["(adapter line quiet before the helper start)".to_string()]
        );
        // T17 carry check: B's report must carry ITS helper's 19.2 exactly.
        write("a", &[]);
        write(
            "b",
            &[r#"{"w":1,"side":"b","kind":"profile-rate","rate_req_s":19.2}"#],
        );
        let rep = |v: &str| {
            let payload =
                format!(r#"{{"effective":{{"obd":{{"profile":{{"rate_req_s":{v}}}}}}}}}"#);
            let line = serde_json::json!({"w": 5, "topic": "xorgate/devices/x/config/reported", "payload": payload});
            std::fs::write(d.join("b").join("mqtt.ndjson"), line.to_string()).unwrap();
        };
        rep("19.2");
        assert!(run().is_empty());
        for bad in ["19", "19.200000762939453", "19.3"] {
            rep(bad);
            assert_eq!(
                run(),
                vec!["(profile rate carried from its helper)".to_string()],
                "{bad}"
            );
        }
        // T17 ruling 2026-10-28: a helper probe rate outside 5–40 FAILS even
        // when both sides carry it.
        write(
            "b",
            &[r#"{"w":1,"side":"b","kind":"profile-rate","rate_req_s":0.0}"#],
        );
        rep("0.0");
        assert_eq!(run(), vec!["(helper probe rate plausible)".to_string()]);
        // R5 NIT: a report carrying a rate with NO profile in the relay log
        // FAILS (no pre-R4-3 skip).
        write("b", &[]);
        rep("19.2");
        assert_eq!(
            run(),
            vec!["(profile rate carried from its helper)".to_string()]
        );
        std::fs::remove_file(d.join("b").join("mqtt.ndjson")).unwrap();
        // R5 NIT: a side with no relay log at all FAILS.
        std::fs::remove_file(d.join("b").join(crate::p5b::OBD_GATE_LOG)).unwrap();
        assert_eq!(run(), vec!["(relay log present on both sides)".to_string()]);
        write("b", &[]);
        // R6 NIT: BOTH logs missing on an OBD run FAILS, unless --no-gate.
        std::fs::remove_file(d.join("a").join(crate::p5b::OBD_GATE_LOG)).unwrap();
        std::fs::remove_file(d.join("b").join(crate::p5b::OBD_GATE_LOG)).unwrap();
        assert!(run().is_empty(), "not an OBD run");
        std::fs::write(
            d.join("a").join("meta.json"),
            r#"{"env":{"XORGATE_OBD_SERIAL":"/dev/pts/1"}}"#,
        )
        .unwrap();
        assert_eq!(run(), vec!["(relay log present on both sides)".to_string()]);
        std::fs::write(d.join("play.json"), r#"{"noGate":true}"#).unwrap();
        assert!(run().is_empty(), "--no-gate");
        std::fs::remove_file(d.join("play.json")).unwrap();
        std::fs::remove_file(d.join("a").join("meta.json")).unwrap();
        write("a", &[]);
        write("b", &[]);
        // R5 NIT: stale-reply drains compared across sides.
        write(
            "a",
            &[r#"{"w":1,"side":"a","kind":"drain","bytes":14,"ms":310,"quiet":true}"#],
        );
        assert_eq!(
            run(),
            vec!["(helper starts with a stale adapter reply)".to_string()]
        );
        // R6-1: explained when the previous helper was stopped with one
        // request in flight (its last audit requests − ok = 1) …
        let stop = |ok: i64, signalled: bool| {
            vec![
                format!(
                    r#"{{"w":1,"side":"a","kind":"audit-line","ts":1.0,"requests":387,"ok":{ok},"pid":7}}"#
                ),
                format!(
                    r#"{{"w":1,"side":"a","kind":"helper-exit","rc":0,"signalled":{signalled},"pid":7}}"#
                ),
                r#"{"w":2,"side":"a","kind":"drain","bytes":14,"ms":310,"quiet":true,"pid":8}"#
                    .to_string(),
            ]
        };
        let lines = stop(386, true);
        write("a", &lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(run().is_empty());
        // R7 NIT: only ONE ATZ reply (14 B) is explained; 42 B still counts
        let lines: Vec<String> = stop(386, true)
            .into_iter()
            .map(|l| l.replace(r#""bytes":14"#, r#""bytes":42"#))
            .collect();
        write("a", &lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(
            run(),
            vec!["(helper starts with a stale adapter reply)".to_string()]
        );
        // … not after a clean stop (requests = ok), nor an unsignalled exit
        for (ok, sig) in [(387, true), (386, false)] {
            let lines = stop(ok, sig);
            write("a", &lines.iter().map(String::as_str).collect::<Vec<_>>());
            assert_eq!(
                run(),
                vec!["(helper starts with a stale adapter reply)".to_string()],
                "ok {ok} signalled {sig}"
            );
        }
        write("a", &[]);
        // R6-1: a wait over the bound at the `stopped` status of a helper
        // stopped with a request in flight is explained up to + the ATZ
        // reset; the same wait on another edge, a clean stop, or beyond
        // 1000 ms FAILS.
        let cut_stop = |wait: i64, ok: i64, edge_kind: &str| {
            vec![
                format!(
                    r#"{{"w":1,"side":"a","edge":5,"kind":"{edge_kind}","wait_ms":{wait},"timeout":false,"pid":9}}"#
                ),
                format!(
                    r#"{{"w":2,"side":"a","kind":"audit-line","ts":2.0,"requests":383,"ok":{ok},"pid":9}}"#
                ),
                r#"{"w":2,"side":"a","kind":"helper-exit","rc":0,"signalled":true,"pid":9}"#
                    .to_string(),
                r#"{"w":3,"side":"a","edge":6,"kind":"exit","wait_ms":3,"timeout":false,"pid":9}"#
                    .to_string(),
                // the next helper's start: the stop above is MID-RUN
                r#"{"w":4,"side":"a","edge":7,"kind":"ready","wait_ms":2,"timeout":false,"pid":10}"#
                    .to_string(),
            ]
        };
        let lines = cut_stop(454, 382, "status");
        write("a", &lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(
            run(),
            vec!["(barrier wait: own close cut a request in flight)".to_string()],
            "cited (tolerated), not UNTOLERATED"
        );
        // NEGATIVE: a clean stop, another edge → the normal 400 ms bound
        for (wait, ok, kind) in [(454, 383, "status"), (454, 382, "profile")] {
            let lines = cut_stop(wait, ok, kind);
            write("a", &lines.iter().map(String::as_str).collect::<Vec<_>>());
            assert_eq!(
                run(),
                vec!["(barrier wait over bound)".to_string()],
                "{wait} {ok} {kind}"
            );
        }
        // R7 SHOULD-FIX: at the cut stop the cut side must WAIT 300–900 ms:
        // 950 (beyond the 0.5 s reset) and 250 (the cut side lagged) FAIL.
        let tol_of = || {
            let mut rep = Report::default();
            diff_obd_gate(&d, &mut rep);
            rep.mismatches
                .iter()
                .map(|m| (m.path.clone(), m.tol.clone()))
                .collect::<Vec<_>>()
        };
        for (wait, want) in [
            (900, "T8/obd-audit-cycle"),
            (950, UNTOLERATED),
            (250, UNTOLERATED),
        ] {
            let lines = cut_stop(wait, 382, "status");
            write("a", &lines.iter().map(String::as_str).collect::<Vec<_>>());
            assert_eq!(
                tol_of(),
                vec![(
                    "(barrier wait: own close cut a request in flight)".to_string(),
                    want.to_string()
                )],
                "{wait}"
            );
        }
        // The run's FINAL stop (no later edge): a cut final stop waits ~0 on
        // the rig (both agents' own shutdown); there the normal bound applies
        // and nothing is required — 0 ms passes, 454 ms FAILS as usual.
        for (wait, want) in [
            (0, vec![]),
            (454, vec!["(barrier wait over bound)".to_string()]),
        ] {
            let lines: Vec<String> = cut_stop(wait, 382, "status")
                .into_iter()
                .filter(|l| !l.contains(r#""kind":"ready""#))
                .collect();
            write("a", &lines.iter().map(String::as_str).collect::<Vec<_>>());
            assert_eq!(run(), want, "final stop {wait}");
        }
        // …and when BOTH sides' helpers were cut at that edge, the normal
        // bound applies to both (neither is early)
        let lines = cut_stop(454, 382, "status");
        write("a", &lines.iter().map(String::as_str).collect::<Vec<_>>());
        let lb: Vec<String> = cut_stop(10, 382, "status")
            .into_iter()
            .map(|l| l.replace(r#""side":"a""#, r#""side":"b""#))
            .collect();
        write("b", &lb.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(run(), vec!["(barrier wait over bound)".to_string()]);
        write("a", &[]);
        write("b", &[]);
        // R5-2: an edge a relay delivered > 50 ms after its shared GO FAILS.
        write(
            "a",
            &[r#"{"w":1,"side":"a","edge":4,"kind":"go","decider":false,"late_ms":157}"#],
        );
        assert_eq!(
            run(),
            vec!["(edge delivered late after its shared GO)".to_string()]
        );
        write(
            "a",
            &[r#"{"w":1,"side":"a","edge":4,"kind":"go","decider":false,"late_ms":6}"#],
        );
        assert!(run().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Review-wp4 R5-1: the state path `obd.reqPerSec` must be null, one of
    /// the two latest profile rates the side's own helper printed by then, or
    /// requestRate() of two consecutive audits of its own helper; the
    /// reviewer's integer-rounded 18 and shifted 19.7 FAIL.
    #[test]
    fn state_req_per_sec_is_carried_from_its_own_helper() {
        let d = std::env::temp_dir().join(format!("obd-state-rate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for l in ["a", "b"] {
            std::fs::create_dir_all(d.join(l)).unwrap();
            std::fs::write(d.join(l).join(crate::p5b::OBD_GATE_LOG), "").unwrap();
        }
        std::fs::write(
            d.join("b").join(crate::p5b::OBD_GATE_LOG),
            [
                r#"{"w":1000,"side":"b","kind":"profile-rate","rate_req_s":18.3}"#,
                r#"{"w":61000,"side":"b","kind":"audit-line","ts":61.0,"requests":1000}"#,
                r#"{"w":121000,"side":"b","kind":"audit-line","ts":121.0,"requests":2110}"#,
                r#"{"w":150000,"side":"b","kind":"profile-rate","rate_req_s":19.1}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let run = |vals: &[(i64, &str)]| {
            let lines: Vec<String> = vals
                .iter()
                .map(|(w, v)| {
                    let text = format!(r#"{{"obd":{{"reqPerSec":{v}}}}}"#);
                    serde_json::json!({"w": w, "name": "state.json", "text": text}).to_string()
                })
                .collect();
            std::fs::write(d.join("b").join("files.ndjson"), lines.join("\n")).unwrap();
            let mut rep = Report::default();
            diff_obd_gate(&d, &mut rep);
            rep.mismatches
                .iter()
                .map(|m| (m.at.clone(), m.a.clone()))
                .collect::<Vec<_>>()
        };
        // null, the seed, the audit-pair rate (1110 / 60 = 18.5), the next seed
        assert!(run(&[
            (500, "null"),
            (2000, "18.3"),
            (122_000, "18.5"),
            (151_000, "19.1")
        ])
        .is_empty());
        // the previous seed while the next profile is still in flight
        assert!(run(&[(2000, "18.3"), (151_000, "18.3")]).is_empty());
        // the reviewer's mutations: integer-rounded / shifted
        assert_eq!(
            run(&[(2000, "18")]),
            vec![("B state at 2000".into(), "18".into())]
        );
        assert_eq!(
            run(&[(2000, "19.7")]),
            vec![("B state at 2000".into(), "19.7".into())]
        );
        // a profile rate shown BEFORE its helper printed it
        assert_eq!(
            run(&[(2000, "19.1")]),
            vec![("B state at 2000".into(), "19.1".into())]
        );
        // an audit-pair rate before its second audit
        assert_eq!(
            run(&[(100_000, "18.5")]),
            vec![("B state at 100000".into(), "18.5".into())]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    fn w(w: i64, pins: &[u32], high: bool) -> PinWrite {
        PinWrite {
            w,
            pins: pins.to_vec(),
            high,
        }
    }

    /// purple blink-fast (r+b): start = all three pins, toggles = r,b only;
    /// then green heartbeat = a new run.
    #[test]
    fn groups_and_runs() {
        let ws = vec![
            w(1000, &[12, 16], true),
            w(1010, &[21], false),
            w(1135, &[12, 16], false),
            w(1260, &[12, 16], true),
            w(2000, &[21], true),
            w(2008, &[12, 16], false),
            w(2130, &[21], false),
        ];
        let g = groups(&ws);
        assert_eq!(
            g.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(),
            ["+12,16 -21", "-12,16", "+12,16", "+21 -12,16", "-21"]
        );
        // A blink step right before the next state's start stays apart.
        let tight = groups(&[
            w(0, &[12, 16], true),
            w(30, &[21], true),
            w(35, &[12, 16], false),
            w(500, &[16], true),
            w(520, &[12, 21, 16], false),
        ]);
        assert_eq!(
            tight.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(),
            ["+12,16", "+21 -12,16", "+16", "-12,21,16"]
        );
        let r = runs(&g);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].len(), 3);
        assert_eq!(r[1].len(), 2);
    }

    /// review-wp6 M1: heartbeat-anchored run starts. A blue → green render
    /// one heartbeat late on B FAILS; on a shed rung (≤ 2 heartbeats) it is
    /// T15; a constant phase offset is fine.
    #[test]
    fn led_run_starts_on_the_heartbeat_grid() {
        let table = Table::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/replay/tolerances.toml"),
        )
        .unwrap();
        let tr = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // A blink-free sketch: each render = its first group only (solid-like).
        let cap = |phase: i64, late: i64, mid: &str| {
            let mut c = P4Capture {
                transitions: tr(&[
                    "[led] starting → gps-wait (blue blink-slow)",
                    &format!("[led] gps-wait → {mid}"),
                    "[led] healthy → network (purple blink-fast)",
                ]),
                led_other: vec!["[led] rgb-gpio r=GPIO12 g=GPIO21 b=GPIO16 (active high)".into()],
                ..Default::default()
            };
            let (mid_rise, mid_fall) = if mid.contains("red") {
                (vec![12], vec![21, 16])
            } else {
                (vec![21], vec![12, 16])
            };
            c.writes = vec![
                w(1000 + phase, &[16], true),
                w(1005 + phase, &[12, 21], false),
                w(5000 + phase + late, &mid_rise, true),
                w(5005 + phase + late, &mid_fall, false),
                w(9000 + phase, &[12, 16], true),
                w(9005 + phase, &[21], false),
            ];
            c
        };
        let unt = |a: &P4Capture, b: &P4Capture| {
            let mut rep = Report::default();
            diff_led(a, b, &table, &mut rep);
            rep.mismatches
                .iter()
                .filter(|m| m.at.contains("start"))
                .map(|m| m.tol.clone())
                .collect::<Vec<_>>()
        };
        let healthy = "healthy (green heartbeat)";
        // B 140 ms later on every render: a constant phase, nothing to report.
        assert!(unt(&cap(0, 0, healthy), &cap(140, 0, healthy)).is_empty());
        // B one heartbeat late on gps-wait → healthy (directly fed): FAIL.
        let r = unt(&cap(0, 0, healthy), &cap(140, 1000, healthy));
        assert!(r.contains(&UNTOLERATED.to_string()), "{r:?}");
        // The same on a shed rung (gps-wait → undervoltage, ≤ 2): T15.
        let uv = "undervoltage (red blink-fast)";
        let r = unt(&cap(0, 0, uv), &cap(140, 1000, uv));
        assert!(r.iter().all(|t| t == "T15") && !r.is_empty(), "{r:?}");
        // Off its own grid by 600 ms (> spawn_ms 400, < a heartbeat): FAIL.
        let r = unt(&cap(0, 0, healthy), &cap(140, 600, healthy));
        assert!(r.contains(&UNTOLERATED.to_string()), "{r:?}");
        assert_eq!(
            first_group("purple", [12, 21, 16]).as_deref(),
            Some("+12,16 -21")
        );
        assert_eq!(
            first_group("white", [12, 21, 16]).as_deref(),
            Some("+12,21,16")
        );
        assert_eq!(
            first_group("off", [12, 21, 16]).as_deref(),
            Some("-12,21,16")
        );
    }

    #[test]
    fn setclock_normalisation() {
        let n = normalize_setclock(
            1_760_000_000_400,
            "-n /usr/local/bin/xorgate-setclock 1760000600",
        );
        assert_eq!(n, "-n /usr/local/bin/xorgate-setclock <epoch:now+600>");
        assert_eq!(setclock_offset(&n), Some(600));
        let m = normalize_setclock(
            1_760_000_000_600,
            "-n /usr/local/bin/xorgate-setclock 1760000600",
        );
        assert_eq!(setclock_offset(&m), Some(599));
        assert_eq!(
            normalize_setclock(5, "-n systemctl reboot"),
            "-n systemctl reboot"
        );
        let t = Table::parse(
            r#"version = 1
[[rule]]
id = "T1"
title = "x"
kind = "exclude"
[[rule]]
id = "T2"
title = "x"
kind = "exclude"
[[rule]]
id = "T3"
title = "x"
kind = "exclude"
[[rule]]
id = "T4"
title = "x"
kind = "exclude"
[[rule]]
id = "T5"
title = "x"
kind = "exclude"
[[rule]]
id = "T6"
title = "x"
kind = "exclude"
[[rule]]
id = "T7"
title = "x"
kind = "exclude"
[[rule]]
id = "T8"
title = "x"
kind = "exclude"
[[rule]]
id = "T9"
title = "x"
kind = "exclude"
[[rule]]
id = "T16"
title = "x"
kind = "p4"
streams = ["sudo"]
tolerance = { epoch_s = 1 }
"#,
        )
        .unwrap();
        assert_eq!(setclock_tolerated(&t, &n, &m).as_deref(), Some("T16"));
        let far = normalize_setclock(
            1_760_000_003_000,
            "-n /usr/local/bin/xorgate-setclock 1760000600",
        );
        assert_eq!(setclock_tolerated(&t, &n, &far), None);
    }

    #[test]
    fn iso_and_clock_retag() {
        assert_eq!(iso_ms("1970-01-01T00:00:01.250Z"), Some(1250));
        assert_eq!(iso_ms("2026-10-20T00:00:00.000Z"), Some(1_792_454_400_000));
        let mut toml = String::from("version = 1\n");
        for id in ["T1", "T2", "T3", "T4", "T5", "T7", "T8", "T9"] {
            toml += &format!("[[rule]]\nid = \"{id}\"\ntitle = \"x\"\nkind = \"exclude\"\n");
        }
        toml += "[[rule]]\nid = \"T16\"\ntitle = \"x\"\nkind = \"p4\"\nstreams = [\"sudo\", \"events\"]\ntolerance = { epoch_s = 1, anchor_ms = 50 }\n";
        let t = Table::parse(&toml).unwrap();
        let line = |iso: &str| format!("[clock] ignoring implausible nitz time {iso}");
        let ev = |iso: &str, a: &str, b: &str| Mismatch {
            stream: "events".into(),
            at: "multiset".into(),
            path: format!("warn: {}", line(iso)),
            a: a.into(),
            b: b.into(),
            tol: UNTOLERATED.into(),
        };
        // Each event's own state.json stamp: ISO − ts = the modem offset.
        let cap = |iso: &str, ts: i64| Capture {
            events: vec![(ts, "warn".into(), line(iso))],
            ..Default::default()
        };
        let (ia, ib) = ("1980-03-28T06:18:58.466Z", "1980-03-28T06:18:58.317Z");
        let (ta, tb) = (
            iso_ms(ia).unwrap() + 1_400_000_000_000,
            iso_ms(ib).unwrap() + 1_400_000_000_003,
        );
        let mut rep = Report::default();
        rep.push(ev(ib, "0", "1"));
        rep.push(ev(ia, "1", "0"));
        retag_clock_iso(&t, &cap(ia, ta), &cap(ib, tb), &mut rep);
        assert_eq!(rep.untolerated(), 0, "{:?}", rep.mismatches);
        // Different offsets (a different modem clock): never.
        let mut far = Report::default();
        far.push(ev(ib, "0", "1"));
        far.push(ev(ia, "1", "0"));
        retag_clock_iso(&t, &cap(ia, ta), &cap(ib, tb + 5_000), &mut far);
        assert_eq!(far.untolerated(), 2);
    }

    #[test]
    fn at_forbidden_and_sequence() {
        let rec = |cmd: &str, forbidden: bool| AtRec {
            cmd: cmd.into(),
            state: serde_json::json!({"cmgf": 0, "mem1": "SR"}),
            forbidden,
            fault: None,
        };
        let a = P4Capture {
            at: vec![rec("ATE0", false), rec("AT+CPMS?", false)],
            ..Default::default()
        };
        let mut rep = Report::default();
        diff_at(&a, &a.clone(), &mut rep);
        assert_eq!(rep.untolerated(), 0);
        let mut b = a.clone();
        b.at.push(rec("AT+CMGD=1", true));
        let mut rep = Report::default();
        diff_at(&a, &b, &mut rep);
        assert!(rep.untolerated() >= 2, "{:?}", rep.mismatches);
    }

    // ---- T16/trust-published (F-P2-ROUND) --------------------------------

    fn repo_table() -> Table {
        Table::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/replay/tolerances.toml"),
        )
        .unwrap()
    }

    fn grace(g: i64, n: i64) -> String {
        format!("[clock] no NTP/GPS/NITZ sync {g} s after start — publishing on an UNTRUSTED clock ({n} sample(s) held); timestamps may be early until a sync lands")
    }

    fn late(src: &str, t: i64, p: i64) -> String {
        format!("[clock] {src} synchronized {t} s after boot — clock trusted (published {p} s on an untrusted clock)")
    }

    /// One side: `(stamp, level, text)` events plus the one `connected to`.
    fn tcap(kind: &str, evs: Vec<(i64, &str, String)>) -> Capture {
        Capture {
            meta: crate::diff::AgentMeta {
                label: "x".into(),
                kind: kind.into(),
                ..Default::default()
            },
            events: evs
                .into_iter()
                .chain([(1_000, "info", "connected to broker:8883".to_string())])
                .map(|(t, l, m)| (t, l.to_string(), m))
                .collect(),
            ..Default::default()
        }
    }

    /// The usual side: grace warning at `g_at` (G, N = 298), late trust at
    /// `t_at` printing `src`, T, P.
    fn tside(kind: &str, g_at: i64, g: i64, t_at: i64, src: &str, t: i64, p: i64) -> Capture {
        tcap(
            kind,
            vec![
                (g_at, "warn", grace(g, 298)),
                (t_at, "info", late(src, t, p)),
            ],
        )
    }

    /// The real events diff, then the post-pass; the rows naming a trust
    /// text, as `(A count, B count, tol)`, and the events-stream UNTOLERATED
    /// count.
    fn trust_rows(
        a: &Capture,
        b: &Capture,
        play: &crate::diff::PlayInfo,
    ) -> (Vec<(String, String, String)>, usize) {
        let t = repo_table();
        let mut rep = crate::diff::diff(a, b, play, &[], &t);
        retag_trust_published(&t, a, b, play, &mut rep);
        let rows = rep
            .mismatches
            .iter()
            .filter(|m| m.stream == "events" && m.path.contains("clock trusted"))
            .map(|m| (m.a.clone(), m.b.clone(), m.tol.clone()))
            .collect();
        let unt = rep
            .mismatches
            .iter()
            .filter(|m| m.stream == "events" && m.tol == UNTOLERATED)
            .count();
        (rows, unt)
    }

    fn tolerated(a: &Capture, b: &Capture) -> bool {
        let (rows, unt) = trust_rows(a, b, &crate::diff::PlayInfo::default());
        rows.len() == 2 && rows.iter().all(|r| r.2 == TRUST_PUBLISHED_ID) && unt == 0
    }

    fn rejected(a: &Capture, b: &Capture) -> bool {
        let (rows, _) = trust_rows(a, b, &crate::diff::PlayInfo::default());
        !rows.is_empty() && rows.iter().all(|r| r.2 == UNTOLERATED)
    }

    /// F-P2-ROUND: the late-trust text may differ ONLY in P, by ±1, each P
    /// what its own grace→trust instants print, |T − G − P| ≤ 1 per side,
    /// the instants < 1 s apart. Everything else stays UNTOLERATED.
    #[test]
    fn trust_published_only_a_one_second_rounding_split() {
        // The combined gate's first run: A 30 554 ms → 31, B 30 403 ms → 30.
        let a = tside("js", 149_756, 300, 180_310, "gps", 331, 31);
        let b = tside("rust", 149_760, 300, 180_163, "gps", 331, 30);
        assert!(tolerated(&a, &b));
        // either column order, any kind pair (per-agent instants)
        assert!(tolerated(&b, &a));
        let b_js = tside("js", 149_760, 300, 180_163, "gps", 331, 30);
        assert!(tolerated(&a, &b_js));
        // The reverse direction (the re-run): A 30 162 → 30, B 30 520 → 31.
        assert!(tolerated(
            &tside("js", 282_766, 300, 312_928, "gps", 331, 30),
            &tside("rust", 282_285, 300, 312_805, "gps", 331, 31),
        ));
        // A P on its own instants' x.5 edge, inside instant_ms: either print.
        assert!(tolerated(
            &tside("js", 150_000, 300, 180_520, "gps", 331, 30),
            &tside("rust", 150_010, 300, 180_610, "gps", 331, 31),
        ));
        // The texts equal: no row at all (nothing to tolerate).
        let (rows, unt) = trust_rows(&a, &a.clone(), &crate::diff::PlayInfo::default());
        assert!(rows.is_empty() && unt == 0);

        // NEGATIVE ±2: everything else consistent (A 30 400 → 30, B 31 600
        // → 32, T − G − P = 1 / −1, trust 700 ms and grace 500 ms apart).
        assert!(rejected(
            &tside("js", 150_000, 300, 180_400, "gps", 331, 30),
            &tside("rust", 149_500, 300, 181_100, "gps", 331, 32),
        ));
        // NEGATIVE: another source.
        assert!(rejected(
            &a,
            &tside("rust", 149_760, 300, 180_163, "nitz", 331, 30)
        ));
        // NEGATIVE: another T (P consistent on both sides).
        assert!(rejected(
            &a,
            &tside("rust", 149_760, 300, 180_163, "gps", 330, 30)
        ));
        // NEGATIVE: the message missing on one side.
        let none = tcap("rust", vec![(149_760, "warn", grace(300, 298))]);
        assert!(rejected(&a, &none));
        // NEGATIVE: an extra message (B twice, same text) …
        let twice = tcap(
            "rust",
            vec![
                (149_760, "warn", grace(300, 298)),
                (180_163, "info", late("gps", 331, 30)),
                (180_170, "info", late("gps", 331, 30)),
            ],
        );
        assert!(rejected(&a, &twice));
        // … or the other P as well (one row only is left, B-only).
        let both = tcap(
            "rust",
            vec![
                (149_760, "warn", grace(300, 298)),
                (180_163, "info", late("gps", 331, 30)),
                (180_170, "info", late("gps", 331, 31)),
            ],
        );
        assert!(rejected(&a, &both));
        // … or an early-form trust text beside the late one.
        let early = tcap(
            "rust",
            vec![
                (149_760, "warn", grace(300, 298)),
                (180_163, "info", late("gps", 331, 30)),
                (
                    180_170,
                    "info",
                    "[clock] gps synchronized — clock trusted (telemetry held 300 s, 298 sample(s))"
                        .into(),
                ),
            ],
        );
        assert!(rejected(&a, &early));
        // NEGATIVE: no grace warning on one side (P has no instant to derive from).
        let no_grace = tcap("rust", vec![(180_163, "info", late("gps", 331, 30))]);
        assert!(rejected(&a, &no_grace));
        // NEGATIVE (cross-field): B prints 30 but its own instants say 31
        // (30 700 ms, beyond instant_ms of the edge).
        assert!(rejected(
            &a,
            &tside("rust", 149_760, 300, 180_460, "gps", 331, 30)
        ));
        // NEGATIVE (cross-field): |T − G − P| = 2 on B's OWN grace G 299
        // (P 30 is what its instants print).
        assert!(rejected(
            &a,
            &tside("rust", 149_760, 299, 180_163, "gps", 331, 30)
        ));
        // NEGATIVE: the trust before the grace warning on one side.
        assert!(rejected(
            &a,
            &tcap(
                "rust",
                vec![
                    (180_163, "warn", grace(300, 298)),
                    (149_760, "info", late("gps", 331, 30)),
                ],
            )
        ));
        // NEGATIVE: the trust instants ≥ 1 s apart (grace 456 ms apart;
        // B 30 000 ms → 30).
        assert!(rejected(
            &a,
            &tside("rust", 149_300, 300, 179_300, "gps", 331, 30)
        ));
        // NEGATIVE: the grace instants ≥ 1 s apart (1 044 ms; trust 890 ms
        // apart; B 30 400 ms → 30, T − G − P = 1: only the instants fail).
        assert!(rejected(
            &a,
            &tside("rust", 150_800, 300, 181_200, "gps", 331, 30)
        ));
        // NEGATIVE: non-canonical digits.
        let pad = tcap(
            "rust",
            vec![
                (149_760, "warn", grace(300, 298)),
                (
                    180_163,
                    "info",
                    late("gps", 331, 30).replace("(published 30", "(published 030"),
                ),
            ],
        );
        assert!(rejected(&a, &pad));
        // NEGATIVE: a handoff run (two eras) is never this row.
        let play = crate::diff::PlayInfo {
            handoff: Some(crate::takeover::HandoffInfo {
                restart_w: 1_000_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        let (rows, _) = trust_rows(&a, &b, &play);
        assert!(!rows.is_empty() && rows.iter().all(|r| r.2 == UNTOLERATED));
        // The parser: the late form only, canonical, known sources.
        assert_eq!(
            late_trust_text(&late("gps", 331, 30)),
            Some(("gps".into(), 331, 30))
        );
        assert_eq!(late_trust_text(&late("modem", 331, 30)), None);
        assert_eq!(late_trust_text(&late("gps", 331, -1)), None);
        assert_eq!(
            late_trust_text(
                "[clock] gps synchronized — clock trusted (telemetry held 300 s, 298 sample(s))"
            ),
            None
        );
    }

    /// The two held FAILs of the Phase 7e combined gate, rebuilt from their
    /// own `[clock]` event stamps and texts (the implausible-NITZ lines, the
    /// step and the step-cap lines left out; synthetic tape): the two trust
    /// rows are this row's; the re-run's differing grace warnings are NOT
    /// (they stay with T8/grace-fallback-phase, which needs telemetry).
    #[test]
    fn trust_published_reproduces_the_combined_gate_runs() {
        // B-p4-clock-cap: A (JS) 31, B (Rust) 30.
        let (rows, unt) = trust_rows(
            &tside(
                "js",
                1_794_330_149_756,
                300,
                1_794_330_180_310,
                "gps",
                331,
                31,
            ),
            &tside(
                "rust",
                1_794_330_149_760,
                300,
                1_794_330_180_163,
                "gps",
                331,
                30,
            ),
            &crate::diff::PlayInfo::default(),
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.2 == TRUST_PUBLISHED_ID));
        assert_eq!(unt, 0);
        // B-p4-clock-cap.r2: A (JS) 30 with N 299, B (Rust) 31 with N 298.
        let a = tcap(
            "js",
            vec![
                (1_794_333_282_766, "warn", grace(300, 299)),
                (1_794_333_312_928, "info", late("gps", 331, 30)),
            ],
        );
        let b = tcap(
            "rust",
            vec![
                (1_794_333_282_285, "warn", grace(300, 298)),
                (1_794_333_312_805, "info", late("gps", 331, 31)),
            ],
        );
        let (rows, unt) = trust_rows(&a, &b, &crate::diff::PlayInfo::default());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.2 == TRUST_PUBLISHED_ID));
        assert_eq!(unt, 2, "the two grace rows are not this row's");
    }
}
