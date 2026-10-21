#!/usr/bin/env python3
"""E3 LED CLI, SIGINT mid-pattern: compare the TS `xorgate led` and
`xorgate-rs led` runs of `tools/replay/led-cli-sigint.sh` (the Phase 4 gate
harness, gate-offbench/E3-led-cli/e3run3).

    python3 -I tools/replay/led-cli-sigint-cmp.py <out dir>   # holds js/ and rs/
    python3 -I tools/replay/led-cli-sigint-cmp.py --self-test

Per case (`<case>.sig` = the SIGINT wall ms the harness logged; `<case>.led`
= the rig pinctrl shim's "<wall ms> <argv>" lines, logged when each child
STARTS; .out/.err/.code), BOTH sides must satisfy:

1. stdout / stderr / exit code identical (`xorgate-rs` -> `xorgate`, Q9).
2. No all-pin blank (`set 12,21,16 op dl`) before the SIGINT.
3. After the SIGINT, besides the blanks, at most the ONE pattern step that
   was in flight when the signal came (`apply` has no generation check
   inside, led.ts:357-381, so a step already writing finishes). With the
   colour's pins C (led.ts LED_COLOURS on r=12 g=21 b=16), that is exactly
   one of:
     - `dh C` (an on-step's rise; cache: C were low);
     - `dl C` (an off-step);
     - the FIRST step's rise and/or fall (`dh C`, then `dl` of exactly the
       other pins): only as the run's 1st/2nd writes (levels unknown — the
       cache elides the other pins on every later step).
   Any other write (two rises, another pin set, a stray partial `dl`) FAILS.
4. Order: a lit (`dh`) line only BEFORE the first blank; a step `dl` only
   before the last blank. The LAST line is a blank (the LED ends dark).
5. Window: every line after the SIGINT within +2000 ms of it. The TS
   shutdown blank is spawned synchronously in the signal handler
   (led.ts:70-74), and the trailing blank once the in-flight write resolves,
   bounded by the pinctrl timeout `timeoutMs: 2000` (led.ts:221); the Rust
   CLI spawns its one blank at the signal. The shim logs at spawn.
6. D36 (accepted-divergences.md, option B): the TS lands 1 OR 2 blanks (its
   second pinctrl child races execa's `cleanup: true` kill at process.exit);
   the Rust CLI lands EXACTLY 1 — the TS's "exit wins" outcome (cli/led.rs:
   the body is not polled after the signal, so the pattern never spawns its
   own trailing blank).

Exit 1 on any failure.
"""
import os
import shutil
import sys
import tempfile

BLANK = "set 12,21,16 op dl"
ALL = frozenset({12, 21, 16})
COLOUR_PINS = {
    "red": {12}, "green": {21}, "blue": {16}, "purple": {12, 16},
    "cyan": {21, 16}, "white": {12, 21, 16},
}
WINDOW_MS = 2000
TS_BLANKS = (1, 2)  # D36
RS_BLANKS = (1,)  # D36 (option B)


def rd(p):
    try:
        with open(p) as f:
            return f.read()
    except OSError:
        return None


def lines(p):
    out = []
    for ln in (rd(p) or "").splitlines():
        t, _, argv = ln.partition(" ")
        out.append((int(t), argv))
    return out


def parse(argv):
    """`set <pins> op dh|dl` -> (frozenset pins, level) or None."""
    f = argv.split()
    if len(f) != 4 or f[0] != "set" or f[2] != "op" or f[3] not in ("dh", "dl"):
        return None
    try:
        pins = frozenset(int(x) for x in f[1].split(","))
    except ValueError:
        return None
    return (pins, f[3]) if pins and pins <= ALL else None


def step_errors(steps, led, colour):
    """`steps` = the non-blank post-SIGINT lines as (index in `led`, pins, level)."""
    c = frozenset(COLOUR_PINS.get(colour, ()))
    if not c:
        return [f"unknown colour {colour!r}"]
    shapes = [(p, lv) for _, p, lv in steps]
    idx = [i for i, _, _ in steps]
    # The run's first step = its first non-blank writes: the rise `dh C`,
    # then the fall of exactly the other pins (levels unknown at start).
    nb = [i for i, (_, a) in enumerate(led) if a != BLANK]
    first_rise = len(nb) >= 1 and parse(led[nb[0]][1]) == (c, "dh")
    first_fall_at = nb[1] if len(nb) >= 2 and first_rise else None
    ok = (
        shapes in ([], [(c, "dh")], [(c, "dl")])
        or (shapes == [(c, "dh"), (ALL - c, "dl")] and idx == nb[:2] and first_rise)
        or (shapes == [(ALL - c, "dl")] and idx == [first_fall_at])
    )
    if ok:
        return []
    return [f"after the SIGINT: {[led[i][1] for i in idx]} is not ONE in-flight {colour} step"]


def side_verdict(d, case, want):
    sig = rd(f"{d}/{case}.sig")
    if sig is None:
        return [f"no {case}.sig"], None
    sig = int(sig)
    led = lines(f"{d}/{case}.led")
    out = (rd(f"{d}/{case}.out") or "").split()
    colour = out[1] if len(out) > 1 and out[0] == "led:" else ""
    errs = []
    if any(a == BLANK for t, a in led if t < sig):
        errs.append("all-pin blank before the SIGINT")
    post = [i for i, (t, _) in enumerate(led) if t >= sig]
    blanks = [i for i in post if led[i][1] == BLANK]
    steps = []
    for i in post:
        if led[i][1] == BLANK:
            continue
        p = parse(led[i][1])
        if p is None:
            errs.append(f"not a pin write: {led[i][1]!r}")
            continue
        steps.append((i, p[0], p[1]))
    errs += step_errors(steps, led, colour)
    if blanks:
        for i, _, lv in steps:
            if lv == "dh" and i > blanks[0]:
                errs.append(f"lit write after a blank: {led[i][1]!r} at +{led[i][0] - sig} ms")
            if lv == "dl" and i > blanks[-1]:
                errs.append(f"write after the last blank: {led[i][1]!r}")
    if not post or led[post[-1]][1] != BLANK:
        errs.append("the LED does not end dark (last line is not the blank)")
    late = [f"+{led[i][0] - sig}" for i in post if led[i][0] - sig > WINDOW_MS]
    if late:
        errs.append(f"line(s) past +{WINDOW_MS} ms after the SIGINT: {late}")
    if len(blanks) not in want:
        errs.append(f"{len(blanks)} blank(s) after the SIGINT, want {' or '.join(map(str, want))}")
    return errs, {
        "blanks": [f"+{led[i][0] - sig}" for i in blanks],
        "step": [(f"+{led[i][0] - sig}", led[i][1]) for i, _, _ in steps],
    }


def compare(o, quiet=False):
    cases = sorted(f[:-4] for f in os.listdir(f"{o}/js") if f.endswith(".sig"))
    verdicts = {}
    for c in cases:
        errs = []
        for k in ("out", "err", "code"):
            a = rd(f"{o}/js/{c}.{k}")
            b = (rd(f"{o}/rs/{c}.{k}") or "").replace("xorgate-rs", "xorgate")
            if a != b:
                errs.append(f"{k} differs")
        ej, ij = side_verdict(f"{o}/js", c, TS_BLANKS)
        er, ir = side_verdict(f"{o}/rs", c, RS_BLANKS)
        errs += [f"js: {e}" for e in ej] + [f"rs: {e}" for e in er]
        verdicts[c] = errs
        if not quiet:
            tag = "FAIL" if errs else "PASS"
            print(f"{c} {tag} js {ij} rs {ir}" + (f" — {'; '.join(errs)}" if errs else " (D36)"))
    return verdicts


def self_test():
    """Synthetic cases: SIGINT at 1000, blue (pin 16) blink-fast."""
    ok_js = ["900 set 16 op dh", f"1003 {BLANK}", f"1005 {BLANK}"]
    ok_rs = ["900 set 16 op dh", f"1002 {BLANK}"]
    first = ["990 set 16 op dh"]
    cases = {
        # name: (js lines, rs lines, expect PASS)
        "ok": (ok_js, ok_rs, True),
        "ok_ts_one_blank": (["900 set 16 op dh", f"1003 {BLANK}"], ok_rs, True),
        "ok_in_flight_rise": (["900 set 16 op dl", "1000 set 16 op dh", f"1003 {BLANK}"],
                              ["900 set 16 op dl", "1001 set 16 op dh", f"1002 {BLANK}"], True),
        "ok_in_flight_off": (["900 set 16 op dh", "1001 set 16 op dl", f"1003 {BLANK}"], ok_rs, True),
        "ok_first_step_fall": (first + ["1001 set 12,21 op dl", f"1003 {BLANK}"],
                               first + ["1001 set 12,21 op dl", f"1002 {BLANK}"], True),
        "ok_ts_first_fall_after_blank": (first + [f"1003 {BLANK}", "1004 set 12,21 op dl", f"1005 {BLANK}"], ok_rs, True),
        "ok_ts_unwind_1500": (["900 set 16 op dh", f"1003 {BLANK}", f"2500 {BLANK}"], ok_rs, True),
        # the reviewer's x1-x4 (review-gate-fixes F-R2)
        # (on the TS side, whose 2 blanks are allowed: each still FAILS)
        "x1_lit_between_blanks": (["900 set 16 op dh", f"1003 {BLANK}", "1004 set 16 op dh", f"1006 {BLANK}"], ok_rs, False),
        "x2_stray_21_dl": (["900 set 16 op dh", "1002 set 21 op dl", f"1003 {BLANK}", f"1005 {BLANK}"], ok_rs, False),
        "x3_two_rises": (["900 set 16 op dh", "1001 set 16 op dh", "1002 set 16 op dh", f"1003 {BLANK}", f"1005 {BLANK}"], ok_rs, False),
        "x4_blank_at_2500": (["900 set 16 op dh", f"1003 {BLANK}", f"3500 {BLANK}"], ok_rs, False),
        # D36 option B: Rust with TWO blanks fails (so does a 2nd at +2500)
        "rs_two_blanks": (ok_js, ["900 set 16 op dh", f"1002 {BLANK}", f"1003 {BLANK}"], False),
        "rs_lit_after_blank": (ok_js, ["900 set 16 op dh", f"1002 {BLANK}", "1070 set 16 op dh"], False),
        # and the earlier ones
        "ts_no_blank": (["900 set 16 op dh"], ok_rs, False),
        "rs_no_blank": (ok_js, ["900 set 16 op dh"], False),
        "ts_three_blanks": (ok_js + [f"1007 {BLANK}"], ok_rs, False),
        "wrong_pin_rise": (ok_js, ["900 set 16 op dh", "1001 set 12 op dh", f"1002 {BLANK}"], False),
        "blank_before_sigint": ([f"990 {BLANK}", f"1003 {BLANK}"], ok_rs, False),
        "not_dark": ([f"1003 {BLANK}", "1004 set 16 op dh"], ok_rs, False),
        "first_fall_not_first": (ok_js, ["800 set 16 op dl", "900 set 16 op dh", "1001 set 12,21 op dl", f"1002 {BLANK}"], False),
        "ts_slow_lit_after_blank": (["900 set 16 op dl", f"1003 {BLANK}", "1070 set 16 op dh", f"1072 {BLANK}"], ok_rs, False),
    }
    d = tempfile.mkdtemp()
    try:
        for side in ("js", "rs"):
            os.makedirs(f"{d}/{side}")
        for name, (js, rs) in ((n, (v[0], v[1])) for n, v in cases.items()):
            for side, ls in (("js", js), ("rs", rs)):
                for k, v in (("sig", "1000\n"), ("out", "led: blue blink-fast — Ctrl-C to stop (goes dark on exit)\n"),
                             ("err", ""), ("code", "0\n"), ("led", "".join(x + "\n" for x in ls))):
                    with open(f"{d}/{side}/{name}.{k}", "w") as f:
                        f.write(v)
        got = compare(d, quiet=True)
    finally:
        shutil.rmtree(d)
    bad = 0
    for name, (_, _, want) in cases.items():
        passed = not got[name]
        mark = "ok " if passed == want else "BAD"
        bad += passed != want
        print(f"{mark} {name}: {'PASS' if passed else 'FAIL'} (want {'PASS' if want else 'FAIL'})"
              + ("" if passed else f" — {'; '.join(got[name])}"))
    print(f"== self-test: {len(cases)} case(s), {bad} wrong")
    return 1 if bad else 0


def main():
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    v = compare(sys.argv[1])
    fails = sum(bool(e) for e in v.values())
    print(f"== {len(v)} case(s), {fails} FAIL")
    return 1 if fails or not v else 0


if __name__ == "__main__":
    sys.exit(main())
