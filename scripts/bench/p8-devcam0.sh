#!/usr/bin/env bash
# p8-devcam0.sh: the DEV "cam0 live plays" check (W1 DA12 / W2 DA13) against a
# freshly claimed DEV device. It replaces the briefs' throwaway devcam0.cjs.
# It is p8-cam0.sh with P8_CAM0_STAGE=dev: one implementation, so the org fix
# (the key installed before the app boots, then a reload), the fail-fast on the
# console's own device + video-channels GETs and the K11 rule are shared.
# Every PROD request is aborted, and it REFUSES to run against a PROD host.
#
#   XG_USER=… XG_PASS=… P8_PLAYWRIGHT_NODE_PATH=… P8_DEV_DEVICE=<full id> scripts/bench/p8-devcam0.sh
#   P8_CAM0_SELFTEST=1 scripts/bench/p8-devcam0.sh      the DEV request-policy table
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
P8_CAM0_STAGE=dev exec bash "$HERE/p8-cam0.sh" "$@"
