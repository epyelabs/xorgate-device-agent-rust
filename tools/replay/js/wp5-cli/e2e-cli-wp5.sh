#!/bin/sh
# Phase 4 WP5 e2e: the REAL `xorgate-rs` (Docker-built debug binary) vs the REAL
# TS CLI `node dist/cli.js` @$JS_SHA (default 09da38d; tools/replay/.cache/js-<sha>, built by
# tools/replay/js-build.sh) on cases.txt: pipeline, recording, set wifi (fake
# nmcli/sudo/ping on PATH — nothing real is toggled). Same container paths on
# both sides (/w/<id>), so outputs diff directly. Expected diffs: the Q9 restart
# hint of `pipeline` (xorgate-agent-rs); nothing else.
#
#   run from the Rust worktree root:  <tools>/e2e-cli-wp5.sh <outdir>
set -eu
T=$(cd "$(dirname "$0")" && pwd); OUT=${1:?outdir}; RS=$(pwd)
rm -rf "$OUT"; mkdir -p "$OUT/ts" "$OUT/rs"
# Phase 5b: the shared build per ref ($RIG_JS_CACHE/js-$JS_REF).
JS_REF="${JS_REF:-09da38d}"
JS="${RIG_JS_CACHE:-$RS/tools/replay/.cache}/js-$JS_REF"
[ -f "$JS/dist/cli.js" ] || JS_SHA="$JS_REF" "$RS/tools/replay/js-build.sh"
LBL="xorgate.agent=${XORGATE_AGENT_LABEL:-rig}"
docker run --rm --label "$LBL" --platform linux/arm64 -v "$JS:/js:ro" -v "$T:/tools:ro" -v "$OUT/ts:/out" \
  -e RUNNER="/usr/local/bin/node /js/dist/cli.js" node:20-bookworm sh /tools/run-side.sh
docker run --rm --label "$LBL" -v xorgate-rs-target:/target:ro -v "$T:/tools:ro" -v "$OUT/rs:/out" \
  -e RUNNER="/target/${TARGET_SUB:-p4-cli}/debug/xorgate-rs" xorgate-rs-build:1.99.0-c813d06bc873 sh /tools/run-side.sh
fails=0
for d in "$OUT"/ts/*/; do
  id=$(basename "$d")
  for f in out err code tree sudo.log; do
    a="$OUT/ts/$id/$f"; b="$OUT/rs/$id/$f"
    [ -f "$a" ] || [ -f "$b" ] || continue
    sed 's/restart to apply: sudo systemctl restart xorgate-agent  (or: xorgate restart)/restart to apply: sudo systemctl restart xorgate-agent-rs/' "$a" > "$a.q9" 2>/dev/null || : > "$a.q9"
    if ! cmp -s "$a.q9" "$b" 2>/dev/null; then
      echo "DIFF $id/$f"; diff "$a.q9" "$b" | head -10; fails=$((fails+1))
    fi
  done
done
echo "cases: $(ls "$OUT/ts" | wc -l)  diffs (after the Q9 hint map): $fails"
