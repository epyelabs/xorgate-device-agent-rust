#!/usr/bin/env bash
# Run a white-box tsx driver (phase file §Record/replay route 2) against the
# SCRATCH copy of the JS agent @ 89b5c64 (tools/replay/.cache/js-89b5c64, built
# by ../js-build.sh) — never the JS repo itself.
#
#   tools/replay/js/run-tsx.sh <driver.mts> [args…]   # stdout = the driver's output
#
# The driver is copied to <scratch>/.rig-drivers/ and run with cwd = the scratch
# root inside node:20-bookworm (linux/arm64, TZ=UTC), so it imports TS modules
# as `../src/agent/config-sync.js` etc. Every WP's driver lives here (copied
# verbatim from phase-2/evidence/wp<N>-tools/): dump-load-config, dump-scope-plane
# (WP1), dump-gps-scenarios (WP2), dump-config-sync (WP3), dump-command-flow,
# probe-exec (WP4), dump-cli-set, dump-cli-status (WP6); fixtures they dump go
# to fixtures/<area>/ via the caller (most take an <outdir> argument).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DRIVER="${1:?usage: run-tsx.sh <driver.mts> [args…]}"; shift
"$HERE/../js-build.sh" >/dev/null
JS="$HERE/../.cache/js-${JS_SHA:-89b5c64}"
# Drivers import the TS three ways: `join(process.cwd(), "src/…")` and
# `"../src/…"` work from .rig-drivers/; a driver importing `"./src/…"` (WP2's
# dump-gps-scenarios.mts) is placed at the scratch root instead.
if grep -q '"\./src/' "$DRIVER"; then DEST=.rig-driver-$(basename "$DRIVER"); else DEST=.rig-drivers/$(basename "$DRIVER"); fi
mkdir -p "$JS/.rig-drivers"
cp "$DRIVER" "$JS/$DEST"
docker run --rm --platform linux/arm64 -e TZ=UTC -v "$JS:/js" -w /js node:20-bookworm \
  npx --no-install tsx "$DEST" "$@"
