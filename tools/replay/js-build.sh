#!/usr/bin/env bash
# Build the JS agent @ $JS_SHA (default 09da38d, H7 flip) for the rig WITHOUT touching
# the JS repo (phase file §Record/replay route 1): `git archive` into
# $RIG_JS_CACHE/js-<sha> (default tools/replay/.cache), then `npm ci && npm run
# build` inside node:20-bookworm (linux/arm64, the rig's runtime). Idempotent:
# an existing build is reused (Phase 5b DISK BUDGET: ONE shared copy per ref —
# point every worktree's RIG_JS_CACHE at it instead of building another).
#
#   tools/replay/js-build.sh [--force]
#   JS_REPO=… JS_SHA=… RIG_JS_CACHE=<abs dir> tools/replay/js-build.sh
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
JS_REPO="${JS_REPO:-$(cd "$HERE/../../.." && pwd)/xorgate-device-agent}"
JS_SHA="${JS_SHA:-09da38d}"
DEST="${RIG_JS_CACHE:-$HERE/.cache}/js-$JS_SHA"
if [ "${1:-}" != "--force" ] && [ -f "$DEST/dist/worker.js" ] && [ -f "$DEST/dist/cli.js" ]; then
  echo "js: $DEST (exists)"
  exit 0
fi
[ -d "$JS_REPO/.git" ] || [ -f "$JS_REPO/.git" ] || { echo "no JS repo at $JS_REPO (set JS_REPO)" >&2; exit 1; }
before=$(git -C "$JS_REPO" status --porcelain)
rm -rf "$DEST"
mkdir -p "$DEST"
git -C "$JS_REPO" archive "$JS_SHA" | tar -x -C "$DEST"
docker run --rm --label "xorgate.agent=${XORGATE_AGENT_LABEL:-rig}" --platform linux/arm64 -v "$DEST:/js" -w /js node:20-bookworm \
  sh -c 'npm ci --no-audit --no-fund --loglevel=error && npm run build >/dev/null && node --version'
after=$(git -C "$JS_REPO" status --porcelain)
[ "$before" = "$after" ] || { echo "JS repo working tree changed during the build — investigate" >&2; exit 1; }
echo "js: $DEST (built from $JS_SHA; JS repo untouched)"
