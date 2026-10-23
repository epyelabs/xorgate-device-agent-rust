#!/usr/bin/env bash
# Run a white-box tsx driver (phase file §Record/replay route 2) against the
# SCRATCH copy of the JS agent @ $JS_SHA (default 09da38d since the Phase 5b H7 flip;
# $RIG_JS_CACHE/js-<sha>, default tools/replay/.cache, built by ../js-build.sh)
# — never the JS repo itself.
#
#   tools/replay/js/run-tsx.sh <driver.mts> [args…]   # stdout = the driver's output
#   JS_SHA=09da38d RIG_JS_CACHE=<abs> tools/replay/js/run-tsx.sh <driver.mts>
#   (Phase 5b: point RIG_JS_CACHE at the ONE shared build per ref; the
#   container carries `--label xorgate.agent=$XORGATE_AGENT_LABEL`, default rig)
#
# The driver is copied to <scratch>/.rig-drivers/ and run with cwd = the scratch
# root inside node:20-bookworm (linux/arm64, TZ=UTC), so it imports TS modules
# as `../src/agent/config-sync.js` etc. Every WP's driver lives here (copied
# verbatim from phase-2/evidence/wp<N>-tools/): dump-load-config, dump-scope-plane
# (WP1), dump-gps-scenarios (WP2), dump-config-sync (WP3), dump-command-flow,
# probe-exec (WP4), dump-cli-set, dump-cli-status (WP6); fixtures they dump go
# to fixtures/<area>/ via the caller (most take an <outdir> argument).
# Phase 3 drivers (copied verbatim from phase-3/evidence/wp<N>-tools/ by WP6):
#   WP1 wp1-dump-recording-flow.ts <wp1-scenarios.json> <out>  → fixtures/recording_flow/manager.json
#       wp1-rate-quirk.ts                                      (TS quirk probe, prints only)
#   WP2 dump-recorder.mts <outdir> → fixtures/recorder_gate/; dump-disk.mts <outdir> → fixtures/disk_plan/
#   WP3 dump-upload-keys.mts > fixtures/upload_keys/upload-keys.json;
#       dump-upload-step.mts > fixtures/upload_step/uploader-drain.json
#   WP4 dump-capture-ladder.mts → fixtures/capture_ladder/ladder.json
#   WP5 dump-media-ladder.mts → fixtures/media_ladder/ladder.json; dump-kvs-runtime.mts →
#       fixtures/media_ladder/kvs-runtime.json; dump-hardware-reconcile.mts, dump-stream-guard.mts
# Phase 4 drivers (copied verbatim from phase-4/evidence/wp<N>-tools/ by WP6):
#   WP1 zz-wp1-dump-led-traces.test.ts → fixtures/led_driver/, fixtures/led_manager/;
#       zz-wp1-dump-led-cli.test.ts → fixtures/led_cli/; zz-review-supersede.test.ts
#       (review probe). These are VITEST files, not tsx: copy into <scratch>/test/
#       and run `npx --no-install vitest run test/<file>` (header of each file).
#   WP2 dump-sms-reader.mts, dump-sms-cli.mts, sms-pty-read.mts (see their headers)
#   WP3 dump-camera-mount.mts → fixtures/camera_mount/; probe-exec-timeout.mts
#   WP4 dump-update-plan.mts → fixtures/update_plan/
#   WP5 wp5-cli/ (dump-cli-pipeline, dump-iface-for-ip, dump-monitor-frames,
#       e2e-cli-wp5.sh + run-side.sh + cases.txt)
#   WP6 dump-sms-modem.mts → fixtures/replay/p4-sms-modem.json (the p4-sms tape's FakeModem)
# Phase 5b WP3 drivers (JS_SHA=09da38d, JS_COMMIT=<full sha> for the stamp):
#   dump-sms-p5b.mts <old fixture> → fixtures/sms/sms-modem.json; dump-sms-clear.mts →
#   fixtures/sms_reader/sms-clear-io.json; dump-led-telemetry.mts → fixtures/led_telemetry/;
#   dump-sms-cli.mts and dump-command-flow.mts now run at either ref; sms-pty-clear.mts
#   (driven by crates/xorgate-agent/tests/sms_clear_pty.rs); zz-wp1-dump-led-traces.test.ts
#   gains the 09da38d led-manager its (vitest, as above).
# Phase 5b WP2 drivers (JS_SHA=09da38d):
#   WP2 zz-p5b-dump-obd.test.ts (VITEST: copy into <scratch>/test/, `npx --no-install vitest run
#       test/zz-p5b-dump-obd.test.ts`) → test/zz-p5b-obd-{pure,traces,mock}.json →
#       fixtures/obd/{pure,supervisor-traces,mock-stream}.json
# A driver taking a data file (WP1's scenarios) gets it by its /js-relative path:
#   cp wp1-scenarios.json into the scratch copy first, or pass `.rig-drivers/<name>` after
#   copying it there (run-tsx.sh copies only the driver itself).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DRIVER="${1:?usage: run-tsx.sh <driver.mts> [args…]}"; shift
"$HERE/../js-build.sh" >/dev/null
JS="${RIG_JS_CACHE:-$HERE/../.cache}/js-${JS_SHA:-09da38d}"
# Drivers import the TS three ways: `join(process.cwd(), "src/…")` and
# `"../src/…"` work from .rig-drivers/; a driver importing `"./src/…"` (WP2's
# dump-gps-scenarios.mts) is placed at the scratch root instead.
if grep -q '"\./src/' "$DRIVER"; then DEST=.rig-driver-$(basename "$DRIVER"); else DEST=.rig-drivers/$(basename "$DRIVER"); fi
mkdir -p "$JS/.rig-drivers"
cp "$DRIVER" "$JS/$DEST"
# The full sha of the ref (drivers that pin per-ref line slices read it).
JS_REPO="${JS_REPO:-$(cd "$HERE/../../../.." && pwd)/xorgate-device-agent}"
JS_COMMIT_FULL=$(git -C "$JS_REPO" rev-parse "${JS_SHA:-09da38d}" 2>/dev/null || echo "${JS_SHA:-09da38d}")
docker run --rm --label "xorgate.agent=${XORGATE_AGENT_LABEL:-rig}" --platform linux/arm64 -e TZ=UTC -e JS_COMMIT_FULL="$JS_COMMIT_FULL" -v "$JS:/js" -w /js node:20-bookworm \
  npx --no-install tsx "$DEST" "$@"
