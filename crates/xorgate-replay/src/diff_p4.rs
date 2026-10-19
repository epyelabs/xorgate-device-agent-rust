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

use std::path::Path;

use serde_json::Value;

use crate::diff::{Mismatch, Report, UNTOLERATED};
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

fn led_bounds(table: &Table) -> Option<(String, f64, f64)> {
    let r = table.of_kind("p4", "led").next()?;
    Some((
        r.id.clone(),
        *r.tolerance.get("step_ms")?,
        *r.tolerance.get("phase_ms")?,
    ))
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
    let (ra, rb) = (runs(&groups(&a.writes)), runs(&groups(&b.writes)));
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
    let tol_or_fail = |within: bool| -> String {
        match (&bounds, within) {
            (Some((id, ..)), true) => id.clone(),
            _ => UNTOLERATED.into(),
        }
    };
    let (step_ms, phase_ms) = bounds
        .as_ref()
        .map(|(_, s, p)| (*s as i64, *p as i64))
        .unwrap_or((0, 0));
    let end = |r: &[Vec<Group>], i: usize, all: &[Group]| -> i64 {
        r.get(i + 1)
            .map(|n| n[0].w)
            .unwrap_or_else(|| all.last().map(|g| g.w).unwrap_or(0))
    };
    let (ga, gb) = (groups(&a.writes), groups(&b.writes));
    for i in 0..ra.len().min(rb.len()) {
        let (x, y) = (&ra[i], &rb[i]);
        compared += 1;
        let dstart = x[0].w - y[0].w;
        if dstart.abs() > step_ms {
            rep.push(mm(
                "led",
                format!("run #{i} start"),
                &x[0].text,
                x[0].w.to_string(),
                y[0].w.to_string(),
                tol_or_fail(dstart.abs() <= phase_ms),
            ));
        }
        let k = x.len().min(y.len());
        for j in 0..k {
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
        if x.len() != y.len() {
            // A repeating pattern runs until the next state: its length
            // follows the run's duration, which differs by the two heartbeat
            // phases at its two ends.
            let (da, db) = (end(&ra, i, &ga) - x[0].w, end(&rb, i, &gb) - y[0].w);
            rep.push(mm(
                "led",
                format!("run #{i} length"),
                "(groups / ms)",
                format!("{} / {da}", x.len()),
                format!("{} / {db}", y.len()),
                tol_or_fail((da - db).abs() <= 2 * phase_ms),
            ));
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
