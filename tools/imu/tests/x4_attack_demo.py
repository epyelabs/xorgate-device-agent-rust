#!/usr/bin/env python3
"""review-wp0-tools X4: the redraw-outlier attack, on SYNTHETIC data only.

    python3 tools/imu/tests/x4_attack_demo.py [--batches 20000]

A synthetic STATIONARY sensor (a fixed quaternion with +-2 LSB noise, linear
acceleration words in -2..2) is scrubbed twice with the same secret seed:
- "uniform": the old redraw (a uniform 16-bit value where the draw equalled the
  original), simulated by a generator subclass;
- "on-trend": the current redraw (same field, same trend index, fresh secret
  jitter).

The attacker sees ONLY the scrubbed file. Per word series (RV i/j/k/real, LA
x/y/z) it flags a word whose distance to the median of its 8 neighbours exceeds
max(3, 4 x the series' median distance), and guesses the original as that
neighbour median. The demo knows the seed, so it can label which words were
redrawn (those are exactly the words whose original equals the draw).

Printed (counts and rates only): redraws, flags, the flag rate among redrawn vs
other words, and how often a flagged word's guess lands within 9 LSB of the
original. "Chance" = the flag rate on redrawn words is the same as on the rest.
"""
import argparse
import json
import os
import random
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)

import scrub_imutx as S  # noqa: E402
import synth  # noqa: E402

SEED = 0x51DE


class UniformRedraw(S.SyntheticGen):
    """The pre-X4 redraw: uniform over the unit's range."""

    def redraw(self, kind, n, k):
        if kind in ("rv", "rv_acc", "la"):
            return self.redraw_rng.randrange(-32768, 32768)
        return self.redraw_rng.randrange(256)


def stationary(batches, rng):
    tx = synth.Tx()
    tx.probe()
    tx.send(1, b"\x01")
    for n in range(batches):
        q = tuple(v + rng.randint(-2, 2) for v in (0, 0, 11585, 11585))
        la = tuple(rng.randint(-2, 2) for _ in range(3))
        tx.packet(3, synth.base_ts() + synth.rv_report(n % 256, q, 0) + synth.la_report(n % 256, la))
    return tx.recs


def series(ops, cls):
    """{(kind, component): [(value, unit positions)]} for complete words."""
    out = {}
    for f in cls.fields:
        if f.kind not in ("rv", "la") or not all(len(u) == 2 for u in f.units):
            continue
        for c, u in enumerate(f.units):
            b = bytes(bytes.fromhex(ops[p[0]]["hex"])[p[1]] for p in u)
            out.setdefault((f.kind, c), []).append((struct.unpack("<h", b)[0], u))
    return out


def attack(scrubbed_ops, raw_ops, cls, redrawn):
    s_new, s_raw = series(scrubbed_ops, cls), series(raw_ops, cls)
    flagged = flagged_redrawn = redrawn_total = close_guess = 0
    for key, vals in s_new.items():
        xs = [v for v, _ in vals]
        devs = []
        for i in range(len(xs)):
            nb = sorted(xs[max(0, i - 4):i] + xs[i + 1:i + 5])
            devs.append((abs(xs[i] - nb[len(nb) // 2]), nb[len(nb) // 2]))
        mad = sorted(d for d, _ in devs)[len(devs) // 2]
        thr = max(3, 4 * mad)
        for i, ((d, med), (_, u)) in enumerate(zip(devs, vals, strict=True)):
            is_r = u in redrawn
            redrawn_total += is_r
            if d > thr:
                flagged += 1
                flagged_redrawn += is_r
                close_guess += abs(med - s_raw[key][i][0]) <= 9
    words = sum(len(v) for v in s_new.values())
    others = words - redrawn_total
    return {
        "words": words,
        "redrawn_words": redrawn_total,
        "flagged": flagged,
        "flagged_redrawn": flagged_redrawn,
        "flag_rate_redrawn": round(flagged_redrawn / redrawn_total, 4) if redrawn_total else None,
        "flag_rate_others": round((flagged - flagged_redrawn) / others, 4) if others else None,
        "flagged_guess_within_9_lsb": close_guess,
    }


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--batches", type=int, default=20000)
    a = ap.parse_args(argv)
    raw = stationary(a.batches, random.Random(1))
    raw_ops = S.ops_of(raw)
    cls = S.classify(raw_ops)
    # Label the redrawn units: the draws do not depend on redraws (separate stream).
    gen = S.SyntheticGen(SEED)
    redrawn = set()
    for f in cls.fields:
        for u, v in zip(f.units, S._draw_units(f, gen), strict=False):
            if f.kind in ("rv", "la") and len(u) == 2:
                b = bytes(bytes.fromhex(raw_ops[p[0]]["hex"])[p[1]] for p in u)
                if struct.unpack("<h", b)[0] == v:
                    redrawn.add(u)
    res = {"batches": a.batches}
    for label, g in (("uniform", UniformRedraw(SEED)), ("on_trend", S.SyntheticGen(SEED))):
        out, stats = S.scrub(raw, stdout_pending=True, gen=g)
        r = attack(S.ops_of(out), raw_ops, cls, redrawn)
        r["redraws"] = stats["redraws"]
        res[label] = r
    print(json.dumps(res, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
