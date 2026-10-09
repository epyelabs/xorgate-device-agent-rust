#!/usr/bin/env bash
# Run a white-box tsx driver (phase file §Record/replay route 2) against the
# SCRATCH copy of the JS agent @ 89b5c64 (tools/replay/.cache/js-89b5c64, built
# by ../js-build.sh) — never the JS repo itself.
#
#   tools/replay/js/run-tsx.sh <driver.mts> [args…]   # stdout = the driver's output
#
# The driver is copied to <scratch>/.rig-drivers/ and run with cwd = the scratch
# root inside node:20-bookworm (linux/arm64, TZ=UTC), so it imports TS modules
# as `../src/agent/config-sync.js` etc. Keep drivers here (WP2/WP3/WP4 add
# theirs); fixtures they dump go to fixtures/<area>/ via the caller.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DRIVER="${1:?usage: run-tsx.sh <driver.mts> [args…]}"; shift
"$HERE/../js-build.sh" >/dev/null
JS="$HERE/../.cache/js-${JS_SHA:-89b5c64}"
mkdir -p "$JS/.rig-drivers"
cp "$DRIVER" "$JS/.rig-drivers/"
docker run --rm --platform linux/arm64 -e TZ=UTC -v "$JS:/js" -w /js node:20-bookworm \
  npx --no-install tsx ".rig-drivers/$(basename "$DRIVER")" "$@"
