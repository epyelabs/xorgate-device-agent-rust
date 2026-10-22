#!/usr/bin/env python3
"""Phase 5b (H2): compare a redump-all.sh output tree against a reference tree
(the committed fixtures, or another ref's redump).

    python3 -I tools/replay/js/redump-compare.py <redump dir> <reference root> [--json]

For every file the redump produced (except _print/ and _log/), the verdict is
  BYTE      byte-identical
  STAMP     identical after dropping every `jsCommit` / `generatedBy` key
            (the provenance stamp; CF78 re-stamps it)
  DIFF      anything else: the differing JSON paths are listed (first 40),
            each with both values abbreviated
  MISSING   the reference has no such file
_print/* (the print-only probes) are compared as text when the reference tree
has a `_print/` too (redump vs redump). Exit 0 always; the caller reads it.
"""
import json
import os
import sys

STAMP = {"jsCommit", "generatedBy"}


def strip(o):
    if isinstance(o, dict):
        return {k: strip(v) for k, v in o.items() if k not in STAMP}
    if isinstance(o, list):
        return [strip(v) for v in o]
    return o


def short(v, n=140):
    s = json.dumps(v, ensure_ascii=False)
    return s if len(s) <= n else s[: n - 1] + "…"


def walk(a, b, path, out):
    if len(out) >= 40:
        return
    if type(a) is not type(b):
        out.append((path, a, b))
        return
    if isinstance(a, dict):
        ka, kb = list(a), list(b)
        for k in ka:
            if k not in b:
                out.append((f"{path}.{k}", a[k], "<absent>"))
            else:
                walk(a[k], b[k], f"{path}.{k}", out)
        for k in kb:
            if k not in a:
                out.append((f"{path}.{k}", "<absent>", b[k]))
        if not out and ka != kb and sorted(ka) == sorted(kb):
            out.append((path + " (key order)", ka, kb))
    elif isinstance(a, list):
        for i in range(min(len(a), len(b))):
            walk(a[i], b[i], f"{path}[{i}]", out)
        if len(a) != len(b):
            out.append((f"{path} (len)", len(a), len(b)))
    elif a != b:
        out.append((path, a, b))


def main():
    red, ref = sys.argv[1], sys.argv[2]
    as_json = "--json" in sys.argv
    rows = []
    for root, _dirs, files in os.walk(red):
        rel_root = os.path.relpath(root, red)
        if rel_root.split(os.sep)[0] == "_log":
            continue
        for f in sorted(files):
            rel = os.path.normpath(os.path.join(rel_root, f))
            pa, pb = os.path.join(red, rel), os.path.join(ref, rel)
            if not os.path.exists(pb):
                if not rel.startswith("_print"):
                    rows.append({"file": rel, "verdict": "MISSING"})
                continue
            ba, bb = open(pa, "rb").read(), open(pb, "rb").read()
            if ba == bb:
                rows.append({"file": rel, "verdict": "BYTE"})
                continue
            try:
                ja, jb = json.loads(ba), json.loads(bb)
            except ValueError:
                la, lb = ba.decode(errors="replace").splitlines(), bb.decode(errors="replace").splitlines()
                d = [(f"line {i + 1}", x, y) for i, (x, y) in enumerate(zip(la, lb)) if x != y][:40]
                if len(la) != len(lb):
                    d.append(("(lines)", len(la), len(lb)))
                rows.append({"file": rel, "verdict": "DIFF", "paths": d})
                continue
            sa, sb = strip(ja), strip(jb)
            if sa == sb:
                rows.append({"file": rel, "verdict": "STAMP"})
                continue
            d = []
            walk(sa, sb, "$", d)
            rows.append({"file": rel, "verdict": "DIFF", "paths": d})
    rows.sort(key=lambda r: r["file"])
    if as_json:
        print(json.dumps(rows, indent=1, ensure_ascii=False, default=str))
        return
    for r in rows:
        print(f"{r['verdict']:8} {r['file']}")
        for p, x, y in r.get("paths", []):
            print(f"           {p}\n             redump: {short(x)}\n             ref:    {short(y)}")


if __name__ == "__main__":
    main()
