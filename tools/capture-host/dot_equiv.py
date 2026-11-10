#!/usr/bin/env python3
"""Pipeline-graph equivalence: gst-launch-1.0 vs the capture host (Phase 7a
WP0, K3 in phase-7a-inprocess-gstreamer.md §Design 3).

For every argv in fixtures/capture-host/argv-matrix.json (the real core
builders: bridge-only; record x 5 presets x sw/hw; with and without an
XORGATE_CAPTURE_SOURCE_HEAD; the finite head), run it through BOTH programs
with GST_DEBUG_DUMP_DOT_DIR set and compare the graphs both dump on each
pipeline state change (gst-launch's bus sync handler, which the host
reproduces). The PAUSED->PLAYING graph is the K3 row; NULL->READY and
READY->PAUSED are compared too.

Instrumentation (rig only, BOTH programs): tools/capture-host/dotshim.c is
LD_PRELOADed into gst-launch AND the host. It ORs SHOW_FULL_PARAMS into every
dump, so no property value is truncated with "..." (R-WP0-1), and makes
g_random_int() deterministic, so mp4mux's random faststart-file DEFAULT is
identical in both and is compared unmasked (R-WP0-2). --no-shim falls back
to the plain dumps (then see TRUNC_ALLOW and the faststart-file rule below).

Normalisation (and nothing else):
  1. the timestamp prefix of the file name is dropped;
  2. every pointer (0x...) becomes P<n>, numbered by first appearance, so the
     node/edge STRUCTURE is still compared exactly (a different link or a
     different element order renumbers and fails);
  3. read-only run-time counters, SCOPED to their element's node label
     (GstQueue current-level-*, GstVideoRate in/out/drop/duplicate), become
     "<volatile>"; the property NAME stays. Without the shim only, a
     faststart-file value that has the random default's shape
     ("/tmp/qtmux" + 6-10 digit int) is masked too;
  4. in the three TRANSITION dumps only, an edge's attribute list (its link
     caps, which the source thread is still negotiating when the dump runs)
     becomes [<link-caps>]; both endpoints stay. The SIGHUP snapshot (2 s into
     PLAYING) keeps the negotiated link caps and is compared in full.
Everything else (factories, element names, non-default properties
including mp4mux fragment-duration inside splitmuxsink, pad caps, states,
link caps) must be byte-equal, and a graph that still contains a truncated
("...") value outside TRUNC_ALLOW FAILS (with the shim, TRUNC_ALLOW is empty).

    dot_equiv.py run --gst /usr/bin/gst-launch-1.0 --host PATH --outdir DIR
        [--only NAME] [--self-check] [--mutate 'TOKEN=>REPLACEMENT' | 'A<=>B']

--self-check also runs gst-launch twice and requires gst-vs-gst equality
(the normaliser removes run-to-run noise). --mutate rewrites one host token;
the run must then FAIL (the harness cannot hide a property difference).
Exit 0 iff every compared case is equal (or, with --mutate, iff the mutated
case differs).
"""
import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import transcript as tr  # noqa: E402

DOT = "/work/dot"
REQUIRED = ("NULL_READY", "READY_PAUSED", "PAUSED_PLAYING", "snapshot")
# A transition dump of a LIVE pipeline can land while caps are still being
# negotiated on some link (measured: gst-vs-gst READY_PAUSED differed in link
# caps once in 1080p30). A real difference (a property, an element, a link)
# is deterministic and never matches on a retry; --mutate proves the harness
# still fails on one. The SIGHUP "snapshot" (2 s into PLAYING, everything
# negotiated) is the steady-state graph.
ATTEMPTS = 3

# Read-only run-time counters whose value at dump time depends on how many
# buffers happened to flow before the dump (measured: gst-vs-gst self-check),
# scoped to the element type whose node label carries them.
SCOPED_VOLATILE = {
    "GstQueue": {"current-level-buffers", "current-level-bytes", "current-level-time"},
    "GstVideoRate": {"in", "out", "drop", "duplicate"},
}
# Without the shim: mp4mux's default "/tmp/qtmux%d" of g_random_int().
RANDOM_QTMUX = re.compile(r'^\\"/tmp/qtmux-?\d{6,10}\\"$')
# Without the shim: (element type, property) whose truncated value is
# accepted, because the full value is pinned elsewhere (a capsfilter's caps
# are the argv token itself, identical by the argv matrix).
TRUNC_ALLOW_NO_SHIM = {("GstCapsFilter", "caps")}
ELLIPSIS = "\u2026"
SHIM = {"enabled": True, "path": None}

PTR = re.compile(r"0x[0-9a-fA-F]+")
FILE_RX = re.compile(r"^[0-9.]+-gst-launch\.(?P<t>[A-Z_]+|snapshot|error|warning)\.dot$")


EDGE = re.compile(r"^(\s*\S+\s*->\s*\S+)\s*\[.*\]\s*$", re.M)
LABEL = re.compile(r'label="((?:[^"\\]|\\.)*)"')


def label_parts(body):
    """A dot node label "GstQueue\\nqueue0\\n[>]\\nk=v..." -> its lines."""
    return body.split("\\n")


def truncated(text):
    """(element type, property) of every "..."-truncated value in a graph."""
    out = []
    for m in LABEL.finditer(text):
        parts = label_parts(m.group(1))
        for part in parts[1:]:
            if ELLIPSIS in part:
                out.append((parts[0], part.partition("=")[0]))
    if text.count(ELLIPSIS) > len(out):
        out.append(("<outside a node label>", "?"))
    return out


def normalise(text, mask_link_caps=False):
    ids = {}

    def ptr(m):
        k = m.group(0).lower()
        if k not in ids:
            ids[k] = f"P{len(ids) + 1}"
        return ids[k]

    text = PTR.sub(ptr, text)
    if mask_link_caps:
        # Transition dumps only: a link's caps label is still being
        # negotiated by the streaming thread when the state-change dump runs.
        # The two endpoints (the structure) stay compared.
        text = EDGE.sub(lambda m: m.group(1) + " [<link-caps>]", text)

    def fix(m):
        parts = label_parts(m.group(1))
        scoped = SCOPED_VOLATILE.get(parts[0], ())
        for k, part in enumerate(parts[1:], 1):
            key, eq, val = part.partition("=")
            if not eq:
                continue
            if key in scoped or (not SHIM["enabled"] and key == "faststart-file"
                                 and RANDOM_QTMUX.match(val)):
                parts[k] = key + "=<volatile>"
        return 'label="' + "\\n".join(parts) + '"'

    text = LABEL.sub(fix, text)
    return text


def run_side(program, arg0, prefix, argv, timeout=20):
    tr.reset_work()
    os.makedirs(DOT, exist_ok=True)
    env = dict(os.environ)
    env["GST_DEBUG_DUMP_DOT_DIR"] = DOT
    if SHIM["enabled"]:
        env["LD_PRELOAD"] = SHIM["path"]
    p = subprocess.Popen([arg0] + prefix + argv, executable=program, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, start_new_session=True)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and p.poll() is None:
        if any(n.endswith("gst-launch.PAUSED_PLAYING.dot") for n in os.listdir(DOT)):
            break
        time.sleep(0.05)
    if p.poll() is None:
        # Steady state, then gst-launch's SIGHUP dot snapshot.
        time.sleep(2.0)
        if p.poll() is None:
            os.kill(p.pid, signal.SIGHUP)
            snap_deadline = time.monotonic() + 3
            while time.monotonic() < snap_deadline and p.poll() is None:
                if any(n.endswith("gst-launch.snapshot.dot") for n in os.listdir(DOT)):
                    break
                time.sleep(0.05)
    time.sleep(0.5)
    killed = False
    if p.poll() is None:
        os.kill(p.pid, signal.SIGINT)
        try:
            p.wait(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
            killed = True
    out, err = p.communicate()
    dots = {}
    for n in sorted(os.listdir(DOT)):
        m = FILE_RX.match(n)
        if m:
            with open(os.path.join(DOT, n)) as f:
                dots.setdefault(m.group("t"), f.read())
    return {
        "exit": p.returncode,
        "killed": killed,
        "stderr": err.decode("utf-8", "replace").splitlines(),
        "dots": dots,
    }


def compare(a, b):
    """-> (equal?, {transition: equal?|missing}, first differing line)."""
    rows, first = {}, None
    for t in REQUIRED:
        da, db = a["dots"].get(t), b["dots"].get(t)
        if da is None and db is None:
            continue
        if da is None or db is None:
            rows[t] = "missing:" + ("a" if da is None else "b")
            continue
        allow = set() if SHIM["enabled"] else TRUNC_ALLOW_NO_SHIM
        cut = [x for x in truncated(da) + truncated(db) if x not in allow]
        if cut:
            rows[t] = "truncated:" + ",".join(sorted({f"{e}.{p}" for e, p in cut}))
            first = first or f"{t}: truncated values {sorted(set(cut))}"
            continue
        mask = t != "snapshot"
        na, nb = normalise(da, mask), normalise(db, mask)
        rows[t] = na == nb
        if na != nb and first is None:
            la, lb = na.splitlines(), nb.splitlines()
            for i, (x, y) in enumerate(zip(la, lb)):
                if x != y:
                    first = f"{t} line {i + 1}:\n      a: {x.strip()[:300]}\n      b: {y.strip()[:300]}"
                    break
            else:
                first = f"{t}: length {len(la)} vs {len(lb)}"
    no_dots = not rows
    if no_dots:
        # Nothing was built (e.g. v4l2h264enc is absent in the rig): the
        # parse outcome must agree instead.
        rows["parse-outcome"] = a["exit"] == b["exit"] and a["stderr"] == b["stderr"]
        if not rows["parse-outcome"]:
            first = f"exit {a['exit']} vs {b['exit']}; stderr {a['stderr'][:2]} vs {b['stderr'][:2]}"
    equal = all(v is True for v in rows.values())
    return equal, rows, first, no_dots


def slug(name):
    return name.replace("/", "_")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--gst", default="/usr/bin/gst-launch-1.0")
    r.add_argument("--host", required=True)
    r.add_argument("--outdir", required=True)
    r.add_argument("--only", action="append")
    r.add_argument("--self-check", action="store_true")
    r.add_argument("--mutate", help="'OLD=>NEW' replaces one host argv token; 'A<=>B' swaps two")
    r.add_argument("--no-shim", action="store_true", help="plain dumps (no LD_PRELOAD instrumentation)")
    a = ap.parse_args()

    SHIM["enabled"] = not a.no_shim
    if SHIM["enabled"]:
        so = "/tmp/xg-dotshim.so"
        src = os.path.join(os.path.dirname(os.path.abspath(__file__)), "dotshim.c")
        subprocess.run(["gcc", "-shared", "-fPIC", "-O2", "-o", so, src, "-ldl"], check=True)
        SHIM["path"] = so

    m, cases = tr.load_matrix()
    os.makedirs(a.outdir, exist_ok=True)
    summary, ok_all = [], True
    for case in m["cases"]:
        name = case["name"]
        if a.only and name not in a.only:
            continue
        argv = tr.swap_source(m, case["argv"])
        host_argv = list(argv)
        if a.mutate and "<=>" in a.mutate:
            x, y = a.mutate.split("<=>", 1)
            if x not in host_argv or y not in host_argv:
                print(f"[SKIP] {name}: swap tokens not in argv", flush=True)
                continue
            i, j = host_argv.index(x), host_argv.index(y)
            host_argv[i], host_argv[j] = y, x
        elif a.mutate:
            old, new = a.mutate.split("=>", 1)
            if old not in host_argv:
                print(f"[SKIP] {name}: mutation token {old!r} not in argv", flush=True)
                continue
            host_argv[host_argv.index(old)] = new
        history = []
        for attempt in range(1, ATTEMPTS + 1):
            g = run_side(a.gst, "gst-launch-1.0", [], argv)
            h = run_side(a.host, "xorgate-capture", ["__capture-host"], host_argv)
            sides = {"gst-launch": g, "host": h}
            if a.self_check:
                sides["gst-launch-2"] = run_side(a.gst, "gst-launch-1.0", [], argv)
            hv = compare(g, h)
            sv = compare(g, sides["gst-launch-2"]) if a.self_check else (True, {}, None, False)
            history.append({"attempt": attempt, "host": hv[1], "hostFirstDiff": hv[2],
                            "selfCheck": sv[1], "selfCheckFirstDiff": sv[2]})
            if (hv[0] and sv[0]) or a.mutate:
                break
        for side, res in sides.items():
            d = os.path.join(a.outdir, slug(name), side)
            os.makedirs(d, exist_ok=True)
            for t, text in res["dots"].items():
                with open(os.path.join(d, t + ".dot"), "w") as f:
                    f.write(text)
        eq, rows, first, no_dots = compare(g, h)
        row = {"case": name, "equal": eq, "rows": rows, "noDots": no_dots, "attempts": attempt, "history": history,
               "exit": {"gst": g["exit"], "host": h["exit"]}, "firstDiff": first}
        if a.self_check:
            seq, srows, sfirst, _ = compare(g, sides["gst-launch-2"])
            row["selfCheck"] = {"equal": seq, "rows": srows, "firstDiff": sfirst}
            eq = eq and seq
        summary.append(row)
        ok_all &= eq
        tag = "EQUAL" if row["equal"] else "DIFF"
        sc = "" if not a.self_check else f"  self-check={'EQUAL' if row['selfCheck']['equal'] else 'DIFF'}"
        print(f"[{tag}] {name}  {rows}{sc}  exit gst={g['exit']} host={h['exit']}  attempts={attempt}", flush=True)
        if first:
            print(f"    first diff: {first}", flush=True)
        if a.self_check and not row["selfCheck"]["equal"]:
            print(f"    self-check diff: {row['selfCheck']['firstDiff']}", flush=True)
    with open(os.path.join(a.outdir, "summary.json"), "w") as f:
        json.dump({"mutate": a.mutate, "selfCheck": a.self_check, "shim": SHIM["enabled"],
                   "scopedVolatile": {k: sorted(v) for k, v in SCOPED_VOLATILE.items()},
                   "stamp": tr.stamp(a.gst), "cases": summary}, f, indent=1)
        f.write("\n")
    if a.mutate:
        # R-WP0-7: only a graph that was BUILT on both sides and differs is a
        # detection. A host argv that no longer parses ("missing") is a
        # different parse outcome, reported apart.
        graph = any(v is False or (isinstance(v, str) and v.startswith("truncated"))
                    for c in summary for v in c["rows"].values())
        parse_only = not graph and any(not c["equal"] for c in summary)
        if graph:
            print("MUTATION DETECTED by a graph diff (harness FAILS, as required)")
            return 0
        if parse_only:
            print("MUTATION CHANGED THE PARSE OUTCOME ONLY (no graph to compare; not a graph detection)")
            return 2
        print("MUTATION NOT DETECTED")
        return 1
    return 0 if ok_all else 1


if __name__ == "__main__":
    sys.exit(main())
