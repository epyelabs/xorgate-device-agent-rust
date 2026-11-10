#!/usr/bin/env bash
# Entrypoint for the simulator image (plans/xorgate-device-simulator, Phase 2):
# the real xorgate-device-agent in `--mode replay` — the RUST agent (Phase 7e
# WP2). A copy of the JS image's sim/entrypoint.sh @ 09da38d with exactly three
# changes, each marked [7e]: the worker is `xorgate-rs run`, the boot-id
# fallback needs no node, and `docker stop` keeps the agent's own exit status
# (D74: the block between `[7e] D74 begin` and `end`, which replaces the JS
# lines from `child=""` to `set -e`; the worker line sits inside it).
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
# Exit codes come from the agent's command mapping (src/agent/commands.ts
# REPLAY_EXIT_CODES): `reboot` → 0 (Docker's restart policy brings the
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
# [7e] no node in this image: the kernel's uuid source is the only one.
export XORGATE_BOOT_ID="$(cat /proc/sys/kernel/random/uuid)"

echo "[sim] device=$DEVICE_ID assets=$ASSETS_DIR scenario=${SCENARIO:-<settings.json|catalog>} home=$XORGATE_HOME bucket=${XORGATE_UPLOAD_BUCKET:-<kvs.json|config>} bootId=$XORGATE_BOOT_ID"

# Run the worker as a child so a `docker stop` (SIGTERM) still reaches it for a
# graceful shutdown, while this shell survives to translate the exit code.
# [7e] D74 begin: the agent's exit status is recorded by an UNTRAPPED wrapper,
# never read from a `wait` a trap interrupted. In the JS shape (the lines this
# block replaces) a `docker stop` makes the single `wait` return 143 at once,
# while the agent is still finalising, so the container dies before the
# agent's graceful shutdown. Re-waiting after a trap is not enough either: when
# the TERM races the agent's own exit, bash 5.2 can lose the status (the
# re-wait returns -1 → `exit -1` = 255, and a power_off's marker is lost;
# review-wp2-fixes N6). So a wrapper subshell (no trap: it is never signalled)
# runs the agent, records its pid in .agent-pid and its exact status (3, 0,
# 128+n for a signal-killed agent) in .agent-exit. `forward` signals the agent
# itself; this shell waits until the wrapper has recorded the status (or is
# gone), and exits with the recorded code. Once it is recorded, TERM/INT are
# ignored: the agent is gone, and the marker write + exit must not be cut.
# Stale files from an earlier start (a persistent /state) go first, before
# the trap can read them (review-wp2-fixes T1).
rm -f "$XORGATE_HOME/.agent-pid" "$XORGATE_HOME/.agent-exit"
child=""
forward() {
  local p=""
  # Before the wrapper exists there is nothing to signal (the JS shape too).
  [[ -n "$child" ]] || return 0
  # The wrapper writes the pid right after the fork; until then, retry
  # briefly. Every step must survive `set -e` (it is on outside this block's
  # `set +e`; a failing command in a trap would exit 1, review-wp2-fixes N8).
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    { read -r p < "$XORGATE_HOME/.agent-pid"; } 2>/dev/null || true
    [[ -n "$p" ]] && break
    sleep 0.05
  done
  # No pid recorded: signal the wrapper's child (the agent), not the wrapper
  # (procps is in the image; N9).
  { [[ -n "$p" ]] && kill -TERM "$p"; } 2>/dev/null || pkill -TERM -P "$child" 2>/dev/null || true
}
trap forward TERM INT
(
  set +e
  # [7e] the Rust agent replaces `node /opt/xorgate/dist/worker.js`.
  /opt/xorgate-rs/bin/xorgate-rs run --mode "${MODE:-replay}" &
  agent=$!
  echo "$agent" > "$XORGATE_HOME/.agent-pid"
  wait "$agent"
  rc=$?
  echo "$rc" > "$XORGATE_HOME/.agent-exit"
  exit "$rc"
) &
child=$!
set +e
wait "$child"
code=$?
# A trapped signal makes `wait` return 128+n at once: wait again until the
# wrapper has recorded the status or is gone. A `wait` that cannot wait (127
# in POSIX mode, or -1 when bash lost the status) sleeps instead of spinning.
until [[ -s "$XORGATE_HOME/.agent-exit" ]] || ! kill -0 "$child" 2>/dev/null; do
  wait "$child"
  c=$?
  if (( c == 127 || c < 0 )); then sleep 0.05; else code=$c; fi
done
trap '' TERM INT
rec=""
{ read -r rec < "$XORGATE_HOME/.agent-exit"; } 2>/dev/null
[[ "$rec" =~ ^[0-9]+$ ]] && code=$rec
rm -f "$XORGATE_HOME/.agent-pid" "$XORGATE_HOME/.agent-exit"
set -e
# [7e] D74 end
if [[ $code -eq 3 ]]; then
  date -u +%FT%TZ > "$XORGATE_HOME/power-off"
  echo "[sim] power_off: parking this container (exit 3; marker written)"
fi
exit "$code"
