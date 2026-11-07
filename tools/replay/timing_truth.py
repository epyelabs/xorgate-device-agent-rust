#!/usr/bin/env python3
"""Ground-truth timing checker (phase-7a-inprocess-gstreamer.md WP3 brief, K5).

Independent of the code under test: plain Python over what the rig's shims
logged and what the agent wrote. Nothing here imports, calls or mirrors the
agent's timing code; the only Rust-produced input besides the sidecars is the
host's own `XG-FRAGMENT` line, used for the SECOND reference only.

The model. The rig's camera is `videotestsrc is-live=true` (both shims swap it
in), x264enc emits a keyframe every `key-int-max` = 2 x fps frames (one GOP =
2 s, `video_capture.rs:266`) from running time 0, and splitmuxsink rotates at
the first keyframe at/after `max-size-time`. So fragment N's first frame is at
running time N x step, where step = ceil(segSecs / GOP) x GOP (= segSecs for
every builder preset: 10 s rig, 60 s field), and its true wall time is

    expected(N) = PLAYING wall + N x step x 1000

for two independent PLAYING references:

- `shim`: the wall instant at which the shim's OWN tee read the program's
  `New clock:` line (the `w` of that line in `shim.ndjson`, stamped by the
  shim, not by the program);
- `host`: the host's own base-time mapping, per fragment N: its line's
  wall/mono pair maps fragment 0's first-frame pipeline instant to wall,
  `cbWallMs(N) - (cbMonoNs(N) - frameMonoNs(0)) / 1e6`.

K5 bounds (per session, every fragment N >= 0, fragment 0 included):
- exact (`anchor:"pts"` sidecars, or the host's lines in `--source lines`):
  |startTs(N) - expected(N)| <= one frame + 20 ms, against BOTH references;
- legacy control (`fstat` / `boundary` sidecars of the same run):
  startTs(N) - expected_shim(N) in 2100 +/- 300 ms, and fragment 0's anchor
  is `boundary`.

Usage:
  timing_truth.py <side dir> --expect exact|legacy [--source sidecars|lines] [--json OUT] [--min-fragments N]
<side dir> is a rig run's `<run>/<label>/` (shim.ndjson, final/recordings/**,
s3/**) or a capture-host harness dir holding only `shim.ndjson` (use
`--source lines`). Exit 0 = every checked session PASSES, 1 = a FAIL, 2 = no
session to check / bad input.
"""
import argparse
import json
import math
import os
import re
import sys

GOP_S = 2
SLACK_MS = 20
LEGACY_LAG_MS = 2100
LEGACY_BAND_MS = 300
UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
FRAG_PREFIX = "XG-FRAGMENT "
MISSING_ANCHOR = "<missing>"


def read_ndjson(path):
    out = []
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    out.append(json.loads(line))
                except ValueError:
                    pass
    except OSError:
        pass
    return out


def argv_value(argv, key):
    for a in argv:
        if a.startswith(key + "="):
            return a[len(key) + 1:]
    return None


def fps_of(argv):
    """The record caps' framerate (`video/x-raw,format=I420,…,framerate=F/1`,
    the token right after the camera source); None if absent."""
    for a in argv:
        if a.startswith("video/x-raw") and "format=I420" in a:
            m = re.search(r"framerate=(\d+)/(\d+)", a)
            if m and int(m.group(2)) > 0:
                return int(m.group(1)) / int(m.group(2))
    return None


def sessions_of(shim):
    """Every record-mode capture spawn: pid, tool, sid, segSecs, fps and the
    shim's `New clock:` wall."""
    out = {}
    for v in shim:
        if v.get("k") != "spawn" or v.get("tool") not in ("gst-launch-1.0", "xorgate-capture"):
            continue
        argv = v.get("argv") or []
        loc = argv_value(argv, "location")
        mst = argv_value(argv, "max-size-time")
        if not loc or not mst:
            continue  # bridge-only: no fragments
        m = UUID.search(loc)
        out[v["pid"]] = {
            "pid": v["pid"],
            "tool": v["tool"],
            "sid": m.group(0) if m else os.path.basename(os.path.dirname(loc)),
            "seg_s": int(mst) / 1e9,
            "fps": fps_of(argv),
            "playing_shim": None,
            "eos_shim": None,
            "lines": [],
        }
    for v in shim:
        s = out.get(v.get("pid"))
        if s is None or v.get("k") != "line":
            continue
        line = v.get("line", "")
        if "New clock:" in line and s["playing_shim"] is None:
            s["playing_shim"] = v.get("w")
        # The stop's instant as the shim's tee saw it (SIGINT handled, or the
        # EOS itself): bounds the legacy `cut` class (WP3 review F4).
        if ("handling interrupt" in line or "Got EOS" in line) and s["eos_shim"] is None:
            s["eos_shim"] = v.get("w")
        if v.get("fd") == 1 and line.startswith(FRAG_PREFIX):
            try:
                s["lines"].append(json.loads(line[len(FRAG_PREFIX):]))
            except ValueError:
                pass
    return out


def sidecars_of(side_dir):
    """{sid: {seq: sidecar}} from final/recordings/** and s3/** (the uploader
    deletes local copies; the fake S3 keeps the bodies)."""
    found = {}
    for root in ("final/recordings", "s3"):
        base = os.path.join(side_dir, root)
        for d, _, files in os.walk(base):
            for f in files:
                m = re.fullmatch(r"(\d{5})\.json", f)
                if not m:
                    continue
                sid = os.path.basename(d)
                try:
                    with open(os.path.join(d, f), encoding="utf-8") as fh:
                        v = json.load(fh)
                except (OSError, ValueError):
                    continue
                found.setdefault(sid, {})[int(m.group(1))] = v
    return found


def last_mp4s(side_dir):
    """{sid: the highest seq of a NON-EMPTY mp4 the player ever saw} from the
    side's recordings.json (empty when absent: then no fragment is a `cut`)."""
    out = {}
    try:
        files = json.load(open(os.path.join(side_dir, "recordings.json"), encoding="utf-8"))["files"]
    except (OSError, ValueError, KeyError):
        return out
    for rel, st in files.items():
        m = re.search(r"/([^/]+)/(\d{5})\.mp4$", rel)
        if m and (st or {}).get("size", 0) > 0:
            out[m.group(1)] = max(out.get(m.group(1), -1), int(m.group(2)))
    return out


def check_session(s, starts, anchors, min_fragments, expect=None, last_mp4=None):
    """One session: `starts` {seq: startTs}, `anchors` {seq: anchor or None}.
    `expect` ("exact" | "legacy"): the mode the RUN was configured for (WP3
    review F1); every fragment must then be of that kind. `None` infers it
    from the data (only the `--source lines` harness, which is exact)."""
    rows = []
    errors = []
    fps = s["fps"]
    if not fps:
        return {"verdict": "SKIP", "why": "no record framerate in the argv", "rows": rows}
    frame_ms = 1000.0 / fps
    step_ms = math.ceil(s["seg_s"] / GOP_S) * GOP_S * 1000
    p_shim = s["playing_shim"]
    if p_shim is None:
        return {"verdict": "SKIP", "why": "no teed New clock: line", "rows": rows}
    by_id = {ln.get("id"): ln for ln in s["lines"]}
    f0 = by_id.get(0, {}).get("frameMonoNs")
    seqs = sorted(starts)
    kinds = {("pts" if anchors.get(q) in (None, "pts") else "legacy") for q in seqs}
    if len(kinds) != 1:
        errors.append("mixed exact and legacy fragments in one session: %s" % sorted(anchors.items()))
    missing = [q for q in seqs if anchors.get(q) == MISSING_ANCHOR]
    if missing:
        errors.append("sidecar(s) without an anchor: %s" % missing)
    exact = kinds == {"pts"} if expect is None else expect == "exact"
    if expect is not None and kinds != {"pts" if exact else "legacy"}:
        errors.append("--expect %s but the session holds %s fragments: %s" % (expect, "/".join(sorted(kinds)), sorted(anchors.items())))
    bound = frame_ms + SLACK_MS
    for q in seqs:
        st = starts[q]
        d_shim = st - (p_shim + q * step_ms)
        row = {"seq": q, "startTs": st, "anchor": anchors.get(q), "d_shim": round(d_shim, 1)}
        if exact:
            ln = by_id.get(q)
            if ln is None or f0 is None:
                row["d_host"] = None
                errors.append("fragment %d: no host line for the host reference" % q)
            else:
                p_host = ln["cbWallMs"] - (ln["cbMonoNs"] - f0) / 1e6
                row["d_host"] = round(st - (p_host + q * step_ms), 1)
                if abs(row["d_host"]) > bound:
                    errors.append("fragment %d: %.1f ms off the host reference (> %.1f)" % (q, row["d_host"], bound))
            if abs(d_shim) > bound:
                errors.append("fragment %d: %.1f ms off the shim reference (> %.1f)" % (q, d_shim, bound))
        else:
            # The session's LAST fragment, cut by the stop's EOS less than one
            # GOP after its rotation keyframe, is created when splitmuxsink
            # flushes that partial GOP at EOS, i.e. EARLIER than one GOP after
            # its first frame (K6(a): +404 / +702 ms on Rust). Its first frame
            # is still at N x step; only the file opened early. WP3 review F4:
            # "last" is the session's last NON-EMPTY mp4 (recordings.json, not
            # the last sidecar), the shim's stop instant e = eos − expected(N)
            # lies in [0, one GOP), and the file opened at it (|d − e| ≤ slack).
            e = None if s.get("eos_shim") is None else s["eos_shim"] - (p_shim + q * step_ms)
            cut = (
                last_mp4 is not None
                and q == last_mp4
                and 0 <= d_shim < LEGACY_LAG_MS - LEGACY_BAND_MS
                and e is not None
                and 0 <= e < GOP_S * 1000
                and abs(d_shim - e) <= SLACK_MS
            )
            if cut:
                row["cut"] = True
            elif abs(d_shim - LEGACY_LAG_MS) > LEGACY_BAND_MS:
                errors.append("legacy fragment %d: %.1f ms after its frame (want %d +/- %d)" % (q, d_shim, LEGACY_LAG_MS, LEGACY_BAND_MS))
            if q == 0 and anchors.get(0) != "boundary":
                errors.append("legacy fragment 0 anchor %r (want boundary)" % anchors.get(0))
        rows.append(row)
    if seqs != list(range(len(seqs))):
        errors.append("fragments are not 0..N contiguous: %s" % seqs)
    if len(seqs) < min_fragments:
        errors.append("%d fragment(s) < %d" % (len(seqs), min_fragments))
    return {
        "verdict": "FAIL" if errors else "PASS",
        "mode": "exact" if exact else "legacy",
        "bound_ms": round(bound, 1) if exact else [LEGACY_LAG_MS - LEGACY_BAND_MS, LEGACY_LAG_MS + LEGACY_BAND_MS],
        "fps": fps,
        "step_ms": step_ms,
        "errors": errors,
        "rows": rows,
    }


def check(side_dir, source="sidecars", min_fragments=1, expect=None):
    shim = read_ndjson(os.path.join(side_dir, "shim.ndjson"))
    sess = sessions_of(shim)
    cars = sidecars_of(side_dir) if source == "sidecars" else {}
    last_mp4 = last_mp4s(side_dir)
    out = []
    for pid in sorted(sess):
        s = sess[pid]
        if source == "lines":
            starts = {ln["id"]: ln["startTs"] for ln in s["lines"] if "startTs" in ln}
            anchors = {q: None for q in starts}
        else:
            c = cars.get(s["sid"], {})
            starts = {q: v.get("startTs") for q, v in c.items() if isinstance(v.get("startTs"), (int, float))}
            # Round-2 N5b: a sidecar WITHOUT an anchor is never `pts`.
            anchors = {q: c[q].get("anchor", MISSING_ANCHOR) for q in starts}
        if not starts:
            out.append({"pid": pid, "sid": s["sid"], "tool": s["tool"], "verdict": "SKIP", "why": "no fragments", "rows": []})
            continue
        r = check_session(s, starts, anchors, min_fragments, expect, last_mp4.get(s["sid"]))
        r.update({"pid": pid, "sid": s["sid"], "tool": s["tool"], "seg_s": s["seg_s"]})
        out.append(r)
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("side_dir")
    ap.add_argument("--source", choices=("sidecars", "lines"), default="sidecars")
    ap.add_argument("--json")
    ap.add_argument("--min-fragments", type=int, default=1)
    ap.add_argument("--expect", choices=("exact", "legacy"),
                    help="the sidecar mode the run was configured for (required with --source sidecars)")
    a = ap.parse_args(argv)
    if a.source == "sidecars" and a.expect is None:
        ap.error("--expect exact|legacy is required with --source sidecars (the mode is never inferred)")
    res = check(a.side_dir, a.source, a.min_fragments, a.expect if a.source == "sidecars" else "exact")
    if a.json:
        with open(a.json, "w", encoding="utf-8") as f:
            json.dump(res, f, indent=1)
    checked = [r for r in res if r["verdict"] in ("PASS", "FAIL")]
    for r in res:
        head = "%s pid=%s sid=%s tool=%s" % (r["verdict"], r["pid"], r["sid"], r["tool"])
        if r["verdict"] == "SKIP":
            print(head + " (%s)" % r["why"])
            continue
        print(head + " mode=%s fps=%s step=%d ms bound=%s" % (r["mode"], r["fps"], r["step_ms"], r["bound_ms"]))
        for row in r["rows"]:
            print("  seq %(seq)d anchor=%(anchor)s d_shim=%(d_shim)s" % row + (" d_host=%s" % row["d_host"] if "d_host" in row else ""))
        for e in r["errors"]:
            print("  ERROR " + e)
    if not checked:
        print("NO SESSION CHECKED")
        return 2
    return 1 if any(r["verdict"] == "FAIL" for r in checked) else 0


if __name__ == "__main__":
    sys.exit(main())
