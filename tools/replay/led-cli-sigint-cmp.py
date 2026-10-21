#!/usr/bin/env python3
"""E3 LED CLI, SIGINT mid-pattern: compare the TS `xorgate led` and
`xorgate-rs led` runs of `tools/replay/led-cli-sigint.sh` (the Phase 4 gate
harness, gate-offbench/E3-led-cli/e3run3).

    python3 -I tools/replay/led-cli-sigint-cmp.py <out dir>   # holds js/ and rs/

Per case (`<case>.sig` = the SIGINT wall ms the harness logged; `<case>.led`
= the pinctrl shim's "<wall ms> <argv>" lines; .out/.err/.code):

- stdout / stderr / exit code identical (`xorgate-rs` -> `xorgate`, Q9);
- no all-pin blank (`set 12,21,16 op dl`) BEFORE the SIGINT on either side;
- after the SIGINT, per side: besides the blanks, at most the ONE pattern
  step that was in flight when the signal came (`apply` has no generation
  check inside it, led.ts:357-381, so a step already writing finishes: its
  rise and/or fall group, <= 2 lines, each `dh` only on the colour's own
  pins and never all three pins), and the LAST line is an all-pin blank (the
  LED ends dark);
- D36 (accepted-divergences.md): the TS lands 1 OR 2 trailing blanks (its
  second pinctrl child races execa's `cleanup: true` kill at process.exit);
  the Rust CLI lands EXACTLY 2 (`LedDriver::writes_settled`, F-E3-1).

Anything else — a missing blank, a third blank, wrong pins or level, any
other line after the signal — FAILS. Exit 1 on any failure.
"""
import os
import sys

BLANK = "set 12,21,16 op dl"
# led.ts LED_COLOURS on the Argus CM5 map (r=GPIO12, g=GPIO21, b=GPIO16).
COLOUR_PINS = {
    "red": {12}, "green": {21}, "blue": {16}, "purple": {12, 16},
    "cyan": {21, 16}, "white": {12, 21, 16},
}
TS_BLANKS = (1, 2)  # D36
RS_BLANKS = (2,)  # D36


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


def is_step(argv, colour):
    """A pattern step write of `colour`: `set <pins> op dh|dl`, not all three
    pins low, `dh` only on the colour's pins."""
    f = argv.split()
    if len(f) != 4 or f[0] != "set" or f[2] != "op" or f[3] not in ("dh", "dl"):
        return False
    try:
        pins = {int(x) for x in f[1].split(",")}
    except ValueError:
        return False
    if not pins <= {12, 21, 16} or argv == BLANK:
        return False
    return f[3] == "dl" or pins <= COLOUR_PINS.get(colour, set())


def side_verdict(d, case, want):
    sig = rd(f"{d}/{case}.sig")
    if sig is None:
        return [f"no {case}.sig"], None
    sig = int(sig)
    led = lines(f"{d}/{case}.led")
    errs = []
    if any(a == BLANK for t, a in led if t < sig):
        errs.append("all-pin blank before the SIGINT")
    post = [(t - sig, a) for t, a in led if t >= sig]
    out = (rd(f"{d}/{case}.out") or "").split()
    colour = out[1] if len(out) > 1 and out[0] == "led:" else ""
    blanks = [x for x in post if x[1] == BLANK]
    in_flight = [x for x in post if x[1] != BLANK]
    bad = [x for x in in_flight if not is_step(x[1], colour)]
    if bad:
        errs.append(f"line(s) after the SIGINT that are no {colour or '?'} step: {bad}")
    if len(in_flight) > 2:
        errs.append(f"{len(in_flight)} step lines after the SIGINT (one in-flight step = <= 2)")
    if post and post[-1][1] != BLANK:
        errs.append("the LED does not end dark (last line is not the blank)")
    if len(blanks) not in want:
        errs.append(f"{len(blanks)} trailing blank(s), want {' or '.join(map(str, want))}")
    return errs, {"blanks": [f"+{t}" for t, _ in blanks], "in_flight": in_flight}


def main():
    o = sys.argv[1]
    cases = sorted(f[:-4] for f in os.listdir(f"{o}/js") if f.endswith(".sig"))
    if not cases:
        print("no SIGINT cases found")
        return 1
    fails = 0
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
        tag = "FAIL" if errs else "PASS"
        fails += bool(errs)
        print(f"{c} {tag} js {ij} rs {ir}" + (f" — {'; '.join(errs)}" if errs else " (D36)"))
    print(f"== {len(cases)} case(s), {fails} FAIL")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
