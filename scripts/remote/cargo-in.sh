#!/usr/bin/env bash
# hl.sh `cargo ARGS`, inside the amd64 build image: any cargo command line on the synced tree.
set -u
git config --global --add safe.directory '*' 2>/dev/null
echo "== HEAD ${HL_HEAD:-unknown} | $(rustc -V) | $(uname -m) | cargo $*"
if [ "${HL_TOUCH:-0}" = 1 ]; then find crates -name '*.rs' -exec touch {} +; echo "== touched every crates/**/*.rs (new tree in this target dir)"; fi
exec cargo "$@"
