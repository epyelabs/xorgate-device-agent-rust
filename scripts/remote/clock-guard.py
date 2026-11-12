#!/usr/bin/env python3
"""clock-guard.py <suite.sh>...: refuse a privileged systemd suite that could set
the PROD host's clock (review F10, fix-check S5). Stdlib only; run by hl-job.sh
`systemd` on the homelab before any --privileged container.

A non-comment line is REFUSED when it
  - names a clock/RTC tool anywhere: hwclock, timedatectl, ntpdate, chronyc,
    chronyd, ntpd, sntp, adjtimex, settimeofday, clock_settime, xorgate-setclock;
  - runs `date` with -s / --set (any position, e.g. `date -u -s …`);
  - executes INDIRECTLY, where the guard cannot see what runs: a simple command
    whose first word is a variable expansion (`$X …`, `"$X" …`, `${X} …`), or
    `eval`, `exec <var>`, `sh|bash -c`, `xargs`, `env <var>`, `sudo <var>`,
    `command <var>` (fail closed);
unless the line carries the marker `hl-clock-mocked` (a reviewed line that only
drives a stub, or calls a script known not to set the clock; say why after it).
Exit 0 = clean; 1 = refused (the offending lines are printed).
"""
import re
import sys

TOOLS = re.compile(r"\b(hwclock|timedatectl|ntpdate|chronyc|chronyd|ntpd|sntp|adjtimex|settimeofday|clock_settime|xorgate-setclock)\b")
DATE_SET = re.compile(r"(^|[^\w-])date\b[^;&|]*\s(-s|--set)(\s|=|$)")
SEP = re.compile(r"(?:;|&&|\|\||\||\bthen\b|\bdo\b|\belse\b|\belif\b|\$\(|`|^\s*[({!]|\s[({!]\s)")
ASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=\S*$")
INDIRECT_WORD = re.compile(r'^"?\$[{A-Za-z_(@*0-9]')
WRAPPERS = {"eval": "eval", "xargs": "xargs"}
VAR_AFTER = {"exec", "env", "sudo", "command", "nohup", "timeout", "nice", "setsid", "unshare"}


def first_words(cmd):
    words = cmd.strip().split()
    while words and (ASSIGN.match(words[0]) or words[0] in ("!", "{", "(", "time")):
        words.pop(0)
    return words


def indirect(cmd):
    w = first_words(cmd)
    if not w:
        return None
    if INDIRECT_WORD.match(w[0]):
        return f"runs a variable expansion ({w[0]})"
    if w[0] in WRAPPERS:
        return f"runs through {w[0]}"
    if w[0] in ("sh", "bash") and "-c" in w[1:3]:
        return f"runs through {w[0]} -c"
    if w[0] in VAR_AFTER:
        rest = [x for x in w[1:] if not x.startswith("-") and not ASSIGN.match(x) and not re.match(r"^\d+[smhd]?$", x)]
        if rest and INDIRECT_WORD.match(rest[0]):
            return f"runs a variable expansion through {w[0]} ({rest[0]})"
    return None


def logical_lines(path):
    """(first line number, text) with backslash-continued lines joined."""
    buf, start = "", None
    with open(path) as f:
        for n, line in enumerate(f, 1):
            s = line.rstrip("\n")
            if start is None:
                start = n
            if s.endswith("\\") and not s.lstrip().startswith("#"):
                buf += s[:-1] + " "
                continue
            yield start, buf + s
            buf, start = "", None
    if start is not None:
        yield start, buf


def check(path):
    bad = []
    for n, s in logical_lines(path):
        if not s.strip() or s.lstrip().startswith("#") or "hl-clock-mocked" in s:
            continue
        code = re.sub(r"\s#.*$", "", s)  # a trailing comment
        if TOOLS.search(code):
            bad.append((n, "names a clock/RTC tool", s))
            continue
        if DATE_SET.search(code):
            bad.append((n, "runs date -s/--set", s))
            continue
        for cmd in SEP.split(code):
            why = indirect(cmd or "")
            if why:
                bad.append((n, why, s))
                break
    return bad


def main():
    rc = 0
    for p in sys.argv[1:]:
        for n, why, s in check(p):
            print(f"REFUSED: {p}:{n}: {why} (mark the line hl-clock-mocked if it is reviewed): {s.strip()[:160]}")
            rc = 1
    return rc


if __name__ == "__main__":
    sys.exit(main())
