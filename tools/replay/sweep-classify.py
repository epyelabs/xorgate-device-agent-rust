#!/usr/bin/env python3
"""Phase 5b step A (H5): group the UNTOLERATED rows of cross-ref JS-vs-JS runs
and pre-classify each group against the expected-delta list X1-X9
(phase-5b-js-catchup.md §8). A group the rules do not match is `?` and must be
read by a human (it becomes EXPECTED with a written reason, or UNEXPECTED).

    python3 -I tools/replay/sweep-classify.py <run dir>... [--md]

A group = (tape, stream, path with indices/numbers folded); per group: row
count, the first A/B sample. The rules only look at stream + path + the A/B
values, never at the tolerance table.
"""
import json
import os
import re
import sys

RULES = [
    # (class, stream regex, path regex, a/b predicate or None, note)
    ("X1", r"^(telemetry|rec-telemetry)$", r"system\.(led_state|health_flags)", None,
     "system.led_state/health_flags on every live + recorded system sample"),
    ("X2", r"^rec-telemetry$", r"(^|\.)system($|\.)|groups|count", None,
     "recorder system group synthesised when absent"),
    ("X3", r"^report$", r"effective\.agent\.capabilities", None, "capabilities 1 -> 4"),
    ("X4", r"^sms$", r"(^|\.)action$", None, "action:\"read\" on …/sms"),
    ("X5", r"^events$", r"\[clock\]|telemetry recording held|clock trust", None,
     "clock trust events"),
    ("X5", r"^report$", r"timeSync|clockTrust|hints", None, "timeSync held detail / hints.clockTrust"),
    ("X5", r"^state$", r"clockHold", None, "recording.clockHold"),
    ("X8", r"^events$", r"signal read failed", None, "LedManager resolves on LED-less hosts"),
    # X8 side effect (sweep-A finding): LedManager.tick now runs signals() on an
    # LED-less host every second -> source.gnssView() -> skyNow(), which writes
    # gnss-sky.json on a state change (ts:src/agent/led-manager.ts:101-120,
    # ts:src/worker.ts:684-689, ts:src/agent/collectors/gps.ts:232-258,328 @09da38d):
    # the end-of-tape NMEA silence -> "unknown" is written ~15 s after the last
    # line instead of on the next NMEA/publish entry point.
    ("X8", r"^file:gnss-sky\.json$", r".", None,
     "gnss-sky.json written on the LED tick (signals() -> gnssView() -> skyNow())"),
]


def fold(path):
    p = re.sub(r"\[\d+\]", "[*]", path)
    p = re.sub(r"(?<=[ .#])\d{6,}", "<n>", p)
    return p


def classify(stream, path, a, b):
    for cls, s_re, p_re, _pred, note in RULES:
        if re.search(s_re, stream) and re.search(p_re, path):
            return cls, note
    return "?", ""


def rows_of(run):
    d = json.load(open(os.path.join(run, "diff.json")))
    for m in d.get("mismatches", []):
        if m.get("tol") in (None, "", "UNTOLERATED"):
            yield m


def main():
    runs = [a for a in sys.argv[1:] if not a.startswith("--")]
    md = "--md" in sys.argv
    out = []
    for run in runs:
        tape = os.path.basename(os.path.normpath(run))
        groups = {}
        for m in rows_of(run):
            key = (m["stream"], fold(m["path"]))
            g = groups.setdefault(key, {"n": 0, "a": m.get("a"), "b": m.get("b"), "at": m.get("at")})
            g["n"] += 1
        for (stream, path), g in sorted(groups.items()):
            cls, note = classify(stream, path, g["a"], g["b"])
            out.append({"tape": tape, "stream": stream, "path": path, "n": g["n"], "cls": cls,
                        "note": note, "a": g["a"], "b": g["b"], "at": g["at"]})
    if md:
        print("| tape | stream | path | rows | class | sample A → B |")
        print("|---|---|---|---|---|---|")
        for r in out:
            ab = f"`{str(r['a'])[:80]}` → `{str(r['b'])[:80]}`".replace("|", "\\|")
            print(f"| {r['tape']} | {r['stream']} | `{r['path'][:120]}` | {r['n']} | {r['cls']} | {ab} |")
    else:
        print(json.dumps(out, indent=1, ensure_ascii=False))


if __name__ == "__main__":
    main()
