#!/usr/bin/env python3
"""hl-lanes.sh's run summary (stdlib only; values never printed).

  hl-summ.py row <run dir>     verdict<TAB>untolerated<TAB>tolerated<TAB>ids
                               (ids = every tolerance id with hits > 0, "id:count,…", sorted)
  hl-summ.py untol <run dir>   one line per UNTOLERATED mismatch: stream<TAB>path<TAB>at-kind
"""
import json
import re
import sys


def load(d):
    with open(f"{d}/diff.json") as f:
        return json.load(f)


def row(d):
    j = load(d)
    v, u, t = "?", "?", "?"
    try:
        with open(f"{d}/diff.md") as f:
            for line in f:
                m = re.match(r"- verdict: \*\*(\w+)\*\* \((\d+) untolerated, (\d+) tolerated\)", line)
                if m:
                    v, u, t = m.groups()
                    break
    except OSError:
        pass
    ids = ",".join(f"{k}:{n}" for k, n in sorted(j.get("hits", [])) if n)
    print(f"{v}\t{u}\t{t}\t{ids}")


def untol(d):
    for m in load(d).get("mismatches", []):
        if m.get("tol") == "UNTOLERATED":
            at = str(m.get("at", "")).split(" ")[0]
            print(f"{m.get('stream')}\t{m.get('path')}\t{at}")


if __name__ == "__main__":
    {"row": row, "untol": untol}[sys.argv[1]](sys.argv[2])
