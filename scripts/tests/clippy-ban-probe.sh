#!/usr/bin/env bash
# shellcheck disable=SC2015  # test assertions: `a && ok || bad`
# R-P6F-3 (review of phase-6/fixes): prove that clippy.toml's
# `disallowed-methods` flags every banned spawn path, one by one.
# The probe crates/xorgate-hal/probes/clippy_ban.rs is in no crate; this
# script copies the tree, mounts the probe into the COPY's xorgate-hal lib,
# runs clippy there and checks that each `// PROBE <path>` line is reported as
# `use of a disallowed method `<path>`` on exactly that line, that the
# `// CONTROL` line (the sanctioned helper) is not, and that clippy printed no
# config warning (a path in clippy.toml that does not resolve). Run in the
# build image (the source stays read-only):
#   docker run --rm --label xorgate.agent=<tag> -v "$PWD:/src:ro" \
#     -v xorgate-rs-cargo-registry:/usr/local/cargo/registry -v xorgate-rs-cargo-git:/usr/local/cargo/git \
#     -v xorgate-rs-target:/target -e CARGO_TARGET_DIR=/target/<branch> -e CARGO_BUILD_JOBS=2 -w /src \
#     xorgate-rs-build:1.99.0-c813d06bc873 bash scripts/tests/clippy-ban-probe.sh
set -uo pipefail
SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
W=$(mktemp -d)
mkdir -p "$W/repo"
(cd "$SRC" && tar --exclude=./target --exclude=./.git -cf - .) | tar -xf - -C "$W/repo"
PROBE=crates/xorgate-hal/probes/clippy_ban.rs
printf '\n#[path = "../probes/clippy_ban.rs"]\nmod clippy_ban_probe;\n' >> "$W/repo/crates/xorgate-hal/src/lib.rs"
(cd "$W/repo" && cargo clippy --locked -p xorgate-hal --lib --message-format short -- -D warnings) > "$W/clippy.log" 2>&1
echo "  clippy rc=$? (non-zero is expected: every probe line is an error)"
hits() { grep -E "clippy_ban\.rs:$1:[0-9]+: error: use of a disallowed method \`$2\`" "$W/clippy.log"; }
n=0
while IFS=: read -r line rest; do
  path=${rest##*// PROBE }
  n=$((n + 1))
  hits "$line" "$path" >/dev/null && ok "flagged: $path (probe line $line)" || bad "NOT flagged: $path (probe line $line)"
done < <(grep -n '// PROBE ' "$SRC/$PROBE")
[ "$n" -ge 20 ] && ok "$n probe lines checked" || bad "only $n probe lines found"
cl=$(grep -n '// CONTROL ' "$SRC/$PROBE" | cut -d: -f1)
! grep -qE "clippy_ban\.rs:$cl:" "$W/clippy.log" && ok "control: crate::child::std_command (line $cl) is not flagged" || bad "control line $cl flagged"
other=$(grep -E 'clippy_ban\.rs:[0-9]+:[0-9]+: (error|warning)' "$W/clippy.log" | grep -v 'use of a disallowed method' || true)
[ -z "$other" ] && ok "no other diagnostic in the probe (it type-checks)" || { bad "other diagnostics in the probe:"; echo "$other"; }
cfg=$(grep -iE 'clippy\.toml|does not refer to|expected a (function|method)|unresolved' "$W/clippy.log" || true)
[ -z "$cfg" ] && ok "no clippy.toml config warning (every banned path resolves)" || { bad "config warnings:"; echo "$cfg"; }
rm -rf "$W"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
