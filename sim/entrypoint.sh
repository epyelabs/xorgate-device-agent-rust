#!/usr/bin/env bash
# Entrypoint for the RUST simulator image (Phase 7e SPIKE): a copy of the JS
# image's sim/entrypoint.sh @ 09da38d with ONE change — the worker is the Rust
# `xorgate-rs run` instead of `node dist/worker.js`. Every mount, env var,
# marker and exit code below is the JS contract, unchanged.
#
# Mounts:  /creds      cert.crt, private.key, ca.pem, kvs.json — the provision bundle (ro)
#          /assets/v1  the scenario asset pack: catalog.json + <scenarioId>/… (ro, shared)
#          /state      XORGATE_HOME: settings.json, recordings/, shm/, pipelines/, markers (rw)
# Env:     IOT_DATA_ENDPOINT (required), DEVICE_ID (default: kvs.json's),
#          XORGATE_UPLOAD_BUCKET (dev/test devices whose kvs.json has no bucket),
#          SCENARIO (optional: scenario to replay until the cloud's `simulation`
#          namespace says otherwise; default = settings.json, else the catalog's
#          first entry), MODE (default replay), AWS_KVS_LOG_LEVEL.
#
# Exit codes come from the agent's command mapping (core `REPLAY_EXIT_CODES`,
# = the TS src/agent/commands.ts): `reboot` → 0 (Docker's restart policy brings the
# container straight back), `power_off` → 3 (parked: a marker in /state keeps
# every later start asleep until the sim host — or a human — removes it).
set -euo pipefail

CREDS_SRC="${CREDS_DIR:-/creds}"
ASSETS_DIR="${ASSETS_DIR:-/assets/v1}"
export XORGATE_HOME="${XORGATE_HOME:-/state}"
mkdir -p "$XORGATE_HOME"

# power_off parking (README Q8). Docker's `restart: unless-stopped` restarts a
# container whatever its exit code, so "stay off" is implemented here: the
# previous run left the marker, this run sleeps. "Power on" = delete the marker
# and restart the container.
if [[ -f "$XORGATE_HOME/power-off" ]]; then
  echo "[sim] parked: power_off marker present since $(cat "$XORGATE_HOME/power-off") — delete $XORGATE_HOME/power-off and restart to power on"
  exec sleep infinity
fi

# A WRITABLE copy of the bundle: the agent persists `uploadBucket` from the
# retained config into kvs.json (Phase 0 surprise 6 — EROFS on the ro mount).
CREDS="$XORGATE_HOME/creds"
mkdir -p "$CREDS"
chmod 700 "$CREDS"
for f in cert.crt private.key ca.pem kvs.json; do
  [[ -f "$CREDS_SRC/$f" ]] || { echo "[sim] missing $CREDS_SRC/$f" >&2; exit 1; }
  # Keep a bucket an earlier run already persisted; refresh everything else.
  if [[ "$f" == kvs.json && -f "$CREDS/kvs.json" ]] && jq -e '.uploadBucket' "$CREDS/kvs.json" >/dev/null 2>&1; then
    continue
  fi
  cp "$CREDS_SRC/$f" "$CREDS/$f"
done
chmod 600 "$CREDS"/*
export CERT_PATH="$CREDS/cert.crt"
export KEY_PATH="$CREDS/private.key"
export CA_PATH="$CREDS/ca.pem"
export KVS_MANIFEST="$CREDS/kvs.json"
export DEVICE_ID="${DEVICE_ID:-$(jq -r '.deviceId' "$KVS_MANIFEST")}"
[[ -n "${IOT_DATA_ENDPOINT:-}" ]] || { echo "[sim] IOT_DATA_ENDPOINT is required" >&2; exit 1; }
[[ -f "$ASSETS_DIR/catalog.json" ]] || { echo "[sim] no scenario pack at $ASSETS_DIR (catalog.json missing)" >&2; exit 1; }

# The in-image KVS master (built for this arch) — the explicit KVS_RUNTIME_BIN
# is what unlocks the agent's non-arm64 allowance in detectRuntime().
export KVS_RUNTIME_BIN=/opt/xorgate/media/xorgate-kvs-webrtc
export KVS_RUNTIME_LIB_DIR=/opt/xorgate/media/lib
export KVS_CACERT_PATH=/opt/xorgate/media/cert.pem
export KVS_PIPELINE_DIR="${KVS_PIPELINE_DIR:-$XORGATE_HOME/pipelines}"
export HARDWARE_MANIFEST="${HARDWARE_MANIFEST:-$XORGATE_HOME/hardware.json}"
mkdir -p "$KVS_PIPELINE_DIR" "$XORGATE_HOME/shm"

# Replay inputs (src/agent/config.ts ReplayConfig) + a boot id per container
# start: the kernel's boot_id is the HOST's and never changes across restarts.
export XORGATE_REPLAY_ASSETS="$ASSETS_DIR"
if [[ -n "${SCENARIO:-}" ]]; then export XORGATE_REPLAY_SCENARIO="$SCENARIO"; fi
export XORGATE_BOOT_ID="$(cat /proc/sys/kernel/random/uuid)"

echo "[sim] device=$DEVICE_ID assets=$ASSETS_DIR scenario=${SCENARIO:-<settings.json|catalog>} home=$XORGATE_HOME bucket=${XORGATE_UPLOAD_BUCKET:-<kvs.json|config>} bootId=$XORGATE_BOOT_ID"

# Run the worker as a child so a `docker stop` (SIGTERM) still reaches it for a
# graceful shutdown, while this shell survives to translate the exit code.
child=""
forward() { [[ -n "$child" ]] && kill -TERM "$child" 2>/dev/null || true; }
trap forward TERM INT
/opt/xorgate-rs/bin/xorgate-rs run --mode "${MODE:-replay}" &
child=$!
set +e
wait "$child"
code=$?
set -e
if [[ $code -eq 3 ]]; then
  date -u +%FT%TZ > "$XORGATE_HOME/power-off"
  echo "[sim] power_off: parking this container (exit 3; marker written)"
fi
exit "$code"
