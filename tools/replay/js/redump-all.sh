#!/usr/bin/env bash
# Phase 5b (H2, CF78): re-run EVERY fixture driver of tools/replay/js/ against
# the shared JS build @ $JS_SHA in ONE node:20-bookworm container and write the
# outputs under <outdir>/<fixture path> (the committed fixture's path), plus
# the raw stdout of the print-only probes under <outdir>/_print/. Then compare
# with tools/replay/js/redump-compare.py.
#
#   JS_SHA=09da38d RIG_JS_CACHE=<abs> tools/replay/js/redump-all.sh <outdir>
#
# Each driver runs the way its header says (tsx from <js>/.rig-drivers/, the
# `./src` importer at the scratch root, vitest files from <js>/test/ — removed
# again afterwards, non-root where the fixture says "non-root"). The JS copy is
# the shared one (DISK BUDGET rule 3): the script only adds `.rig-drivers/`
# files and, for the run's duration, three `test/zz-*` files.
# NOT covered (need a harness, see delta-inventory.md): sms-pty-read.mts (the
# Rust pty test drives it), wp5-cli/e2e-cli-wp5.sh (run it with JS_REF instead).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
OUT=${1:?usage: redump-all.sh <outdir>}
mkdir -p "$OUT"; OUT=$(cd "$OUT" && pwd)
JS_SHA="${JS_SHA:-89b5c64}"
JS_SHA="$JS_SHA" "$HERE/../js-build.sh" >/dev/null
JS="${RIG_JS_CACHE:-$HERE/../.cache}/js-$JS_SHA"
[ -f "$JS/dist/cli.js" ] || { echo "no JS build at $JS" >&2; exit 1; }
docker run --rm --label "xorgate.agent=${XORGATE_AGENT_LABEL:-rig}" --platform linux/arm64 -e TZ=UTC \
  -v "$JS:/js" -v "$HERE:/drivers:ro" -v "$OUT:/out" -w /js node:20-bookworm bash -s <<'IN'
set -u
mkdir -p .rig-drivers /out/_print /out/_log
cp /drivers/*.mts /drivers/*.ts /drivers/wp1-scenarios.json /drivers/wp5-cli/*.mts .rig-drivers/
cp /drivers/dump-gps-scenarios.mts .rig-driver-dump-gps-scenarios.mts
chmod -R a+rwX .rig-drivers .rig-driver-dump-gps-scenarios.mts
FAIL=0
# tsx <who root|node> <driver path> <dest|-> [args…]: stdout → /out/<dest>
tsx() {
  local who=$1 drv=$2 dest=$3; shift 3
  local name; name=$(basename "$drv")
  local o=/out/_print/$name.out; [ "$dest" = - ] || { mkdir -p "$(dirname "/out/$dest")"; o=/out/$dest; }
  local rc=0
  if [ "$who" = node ]; then
    setpriv --reuid=node --regid=node --init-groups env HOME=/tmp npx --no-install tsx "$drv" "$@" > "$o" 2> "/out/_log/$name.err" || rc=$?
  else
    npx --no-install tsx "$drv" "$@" > "$o" 2> "/out/_log/$name.err" || rc=$?
  fi
  echo "$name rc=$rc" | tee -a /out/_log/summary.txt
  [ $rc = 0 ] || FAIL=1
}
tsx root .rig-drivers/dump-camera-mount.mts camera_mount/camera-mount.json
tsx root .rig-drivers/dump-capture-ladder.mts capture_ladder/ladder.json
tsx node .rig-drivers/dump-cli-set.mts cli_set/set-command.json
tsx node .rig-drivers/dump-cli-status.mts cli_set/status-render.json
tsx root .rig-drivers/dump-command-flow.mts command_flow/handle-command.json
mkdir -p /out/config_sync_rules; tsx root .rig-drivers/dump-config-sync.mts - /out/config_sync_rules
tsx root .rig-drivers/dump-disk.mts - /out
tsx root .rig-driver-dump-gps-scenarios.mts collectors/gps-scenarios.json
tsx root .rig-drivers/dump-hardware-reconcile.mts hardware_reconcile/reconcile.json
tsx root .rig-drivers/dump-kvs-runtime.mts media_ladder/kvs-runtime.json
tsx root .rig-drivers/dump-load-config.mts agent_config/load-config.json
tsx root .rig-drivers/dump-media-ladder.mts media_ladder/ladder.json
tsx root .rig-drivers/dump-recorder.mts - /out
tsx root .rig-drivers/dump-scope-plane.mts agent_plane/scope-plane.json
tsx root .rig-drivers/dump-sms-cli.mts sms_cli/sms-cli.json
tsx root .rig-drivers/dump-sms-modem.mts replay/p4-sms-modem.json
tsx root .rig-drivers/dump-sms-reader.mts sms_reader/sms-modem-io.json
tsx root .rig-drivers/dump-stream-guard.mts stream_guard/verdict.json
tsx root .rig-drivers/dump-update-plan.mts update_plan/update.json
tsx root .rig-drivers/dump-upload-keys.mts upload_keys/upload-keys.json
tsx root .rig-drivers/dump-upload-step.mts upload_step/uploader-drain.json
mkdir -p /out/recording_flow; tsx root .rig-drivers/wp1-dump-recording-flow.ts - .rig-drivers/wp1-scenarios.json /out/recording_flow/manager.json
tsx root .rig-drivers/dump-cli-pipeline.mts cli_pipeline/cli-pipeline.json
tsx root .rig-drivers/dump-iface-for-ip.mts network/iface_for_ip.json
FORCE_COLOR=0 tsx root .rig-drivers/dump-monitor-frames.mts -
tsx root .rig-drivers/probe-exec-timeout.mts -
tsx root .rig-drivers/probe-exec.mts -
tsx root .rig-drivers/example-cloud-config.mts -
tsx root .rig-drivers/wp1-rate-quirk.ts -
# vitest drivers: into test/, run, move the JSON out, remove again.
for t in zz-review-supersede zz-wp1-dump-led-cli zz-wp1-dump-led-traces; do
  cp "/drivers/$t.test.ts" test/
  rc=0; npx --no-install vitest run "test/$t.test.ts" > "/out/_log/$t.log" 2>&1 || rc=$?
  echo "$t rc=$rc" | tee -a /out/_log/summary.txt; [ $rc = 0 ] || FAIL=1
  rm -f "test/$t.test.ts"
done
mkdir -p /out/led_driver /out/led_manager
mv test/zz-review-supersede.json /out/led_driver/led-supersede.json
mv test/zz-wp1-led-cli.json /out/led_driver/led-cli.json
mv test/zz-wp1-led-driver.json /out/led_driver/led-driver.json
mv test/zz-wp1-led-manager.json /out/led_manager/led-manager.json
chmod -R a+rwX /out
echo "redump done FAIL=$FAIL"
IN
echo "redump @ $JS_SHA → $OUT"
