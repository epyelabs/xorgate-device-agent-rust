#!/usr/bin/env bash
# Rust-only smoke replay for CI (WP7; plan phase-2-cloud-plane.md §Record/replay,
# "A CI job runs the smoke tape Rust-only"). There is no in-process ReplayHal
# seam: the collectors read the OS (tty, files, child processes, PATH tools),
# so the replay goes through the same OS-boundary knobs as the Docker rig, just
# without Docker (the CI job already runs inside debian:bookworm):
#
#   - a local TLS mosquitto (rig PKI from gen-certs.sh, client certs required);
#   - `xorgate-replay play --a rust --b rust` on fixtures/replay/smoke.ndjson.gz
#     (60 s, real time; B starts 300 ms after A so the hold gate pairs two
#     distinct tick phases);
#   - `xorgate-replay diff` under tolerances.toml → exit non-zero on any
#     UNTOLERATED row; plus a check that every input reached the agents: the
#     telemetry carries gps / imu / lte / system and the agent's own re-recorded
#     tape holds nmea / imu / lte_file / exec / i2cdetect.
#
#   tools/replay/ci-smoke.sh <xorgate-rs> <xorgate-replay> [out dir] [smoke|media]
#
# Needs: mosquitto, openssl, python3, procps (apt). Run as root (the player
# writes /etc/hosts `broker` → 127.0.0.1 when it is missing). The JS-vs-Rust
# parity run stays the Docker rig's job (run-rig.sh).
#
# `media` (Phase 3, WP6): the synthetic `media` scenario (cut to 58 s) (`xorgate-replay
# synth media`: both cameras record 720p15 10 s fragments, two fake KVS
# masters, telemetry + video uploads to the fake S3) Rust-vs-Rust through the
# same knobs. Extra needs: gstreamer1.0-tools + -plugins-{base,good,bad,ugly}
# (the real gst-launch-1.0 behind the shim: videotestsrc, x264enc,
# splitmuxsink, shmsink) and ca-certificates; /dev/shm ≥ ~128 MiB (two agents
# × two 20 MiB shmsink bridges — the job container passes --shm-size); root,
# because the player puts the rig CA into the system store
# (update-ca-certificates) and maps the fake S3 / IoT-creds host names to
# 127.0.0.1 in /etc/hosts, and the fake AWS listens on 127.0.0.1:443.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
RS=$(realpath "${1:?usage: ci-smoke.sh <xorgate-rs> <xorgate-replay> [out]}")
RP=$(realpath "${2:?usage: ci-smoke.sh <xorgate-rs> <xorgate-replay> [out]}")
OUT=${3:-/tmp/replay-ci-smoke}
MODE=${4:-smoke}
case "$MODE" in
  smoke) TAPE="$REPO/fixtures/replay/smoke.ndjson.gz"; MEDIA_ARGS=(); STAGGER_MS=300 ;;
  media)
    TAPE=/tmp/replay-ci-media.ndjson.gz
    # 58 s, not the scenario's 60: its last LTE-file rewrite (t = 59.5 s)
    # lands on the telemetry segment's 60 s rotation, so whether that
    # sample's `lte` group falls in segment 0 or 1 is a coin flip per agent
    # (local run 2026-10-19: seg 1 lte on B only). Cut at 58 s the last
    # rewrite is 49.5 s; the rotation (and the telemetry PUT) still happen
    # inside the 10 s settle.
    "$RP" synth media --duration-s 58 -o "$TAPE" --fixtures "$REPO/fixtures"
    MEDIA_ARGS=(--media-dir "$HERE/media" --aws-listen 127.0.0.1:443)
    # Both agents are SIGTERMed together at the end, so a start stagger
    # shortens B's LAST fragment by the stagger (CI run 39267397686: sidecar
    # 00006 durationMs 7465 vs 7186, > T12's ±150 ms). The rig starts both
    # at once too (play.rs default 0).
    STAGGER_MS=0
    ;;
  *) echo "ci-smoke: unknown mode $MODE (smoke|media)" >&2; exit 2 ;;
esac

"$HERE/gen-certs.sh" >/dev/null
CERTS="$HERE/.cache/certs"
grep -qE '^127\.0\.0\.1[[:space:]].*\bbroker\b' /etc/hosts || echo "127.0.0.1 broker" >> /etc/hosts

CONF=$(mktemp)
cat > "$CONF" <<EOF
per_listener_settings false
persistence false
log_dest stdout
listener 8884 127.0.0.1
protocol mqtt
cafile $CERTS/ca.pem
certfile $CERTS/server.crt
keyfile $CERTS/server.key
require_certificate true
use_identity_as_username true
allow_anonymous true
EOF
mosquitto -c "$CONF" > /tmp/replay-ci-mosquitto.log 2>&1 &
MQ=$!
# WP6 (the load-only media flake): CI_SMOKE_KEEP=<host dir> keeps the run's
# diff + small logs on EVERY exit (a `docker run --rm` loses $OUT otherwise):
# <keep>/<basename $OUT>-<UTC ts>-rc<code>/ (recordings/s3 trees left out).
keep_run() {
  local rc=$1 d
  [ -n "${CI_SMOKE_KEEP:-}" ] && [ -d "$OUT" ] || return 0
  d="$CI_SMOKE_KEEP/$(basename "$OUT")-$(date -u +%Y%m%dT%H%M%SZ)-rc$rc"
  mkdir -p "$d"
  (cd "$OUT" && find . -maxdepth 2 \( -name '*.md' -o -name '*.json' -o -name '*.ndjson' -o -name '*.log' \) \
     ! -name 'mqtt.ndjson' -print0 | xargs -0 -I{} cp --parents {} "$d/") 2>/dev/null || true
  [ -f "$OUT/a/mqtt.ndjson" ] && [ "$MODE" = smoke ] && cp --parents "$OUT"/*/mqtt.ndjson "$d/" 2>/dev/null
  # The agents' own tapes (input read instants) + the final/s3 trees' small
  # files (telemetry segments, sidecars; no mp4): enough to re-run the diff.
  (cd "$OUT" && { ls -d */agent-tape.ndjson.gz 2>/dev/null; find */final */s3 -type f -size -2M ! -name '*.mp4' 2>/dev/null; } \
     | while read -r f; do cp --parents "$f" "$d/"; done) 2>/dev/null || true
  echo "ci-smoke: run kept in $d" >&2
}
trap 'rc=$?; kill $MQ 2>/dev/null || true; keep_run $rc' EXIT
for _ in $(seq 1 50); do
  (exec 3<>/dev/tcp/127.0.0.1/8884) 2>/dev/null && break
  sleep 0.1
done

rm -rf "$OUT"
"$RP" play --tape "$TAPE" --out "$OUT" --a rust --b rust --rs-bin "$RS" \
  --broker-host localhost --broker-port 8884 --proxy-listen 127.0.0.1:8883 \
  --certs "$CERTS" --shims "$HERE/shims" --fake-python "$HERE/fake-python" \
  --work "/tmp/replay-ci-work-$MODE" --settle-s 10 --stagger-ms "$STAGGER_MS" ${MEDIA_ARGS[@]+"${MEDIA_ARGS[@]}"}
# SMOKE ONLY: the host-load-dependent system fields are excluded. Both agents
# read the CI runner's live /proc/stat, /proc/meminfo and statfs, so under a
# loaded shared runner cpu_usage swings by tens of points between the two
# agents' tick instants (seen: 51 vs 8.2, 300 ms apart, at 9c6a1d7 and 630ffe0)
# and T3/host did not cite it. A Rust-vs-Rust smoke is a plumbing check, not
# parity: the JS-vs-Rust rig runs (run-rig.sh) keep these fields under T3 +
# T3/host unchanged. throttled and rail_5v are tape-fed (exact) and stay compared.
TOL=$(mktemp)
cat "$HERE/tolerances.toml" - > "$TOL" <<'TOML'

[[rule]]
id = "SMOKE-host"
title = "CI smoke only (ci-smoke.sh): host-load-dependent system fields not compared"
basis = "ci-smoke.sh: Rust-vs-Rust on a shared CI runner; the JS-vs-Rust rig runs keep T3"
kind = "exclude"
streams = ["telemetry", "state"]
paths = ["metrics.system.cpu_usage.value", "metrics.system.ram_usage.value", "metrics.system.disk_usage.value", "latest.metrics.system.cpu_usage.value", "latest.metrics.system.ram_usage.value", "latest.metrics.system.disk_usage.value"]
TOML
# SMOKE ONLY (orchestrator 2026-10-17; CI run 38097053830: mean tick 998.23 vs
# 999.91 ms): the T8 tick-drift row's per-side period bound [999.5, 1010] is a
# RIG bound calibrated on the quiet Mac VM; on a shared GitHub runner a mean
# can dip below 999.5. The smoke widens the per-side floor/ceiling to
# [995, 1015]; the |ΔP| ≤ 4 ms pair check (max_period_delta_ms) is unchanged,
# and the rig's JS-vs-Rust bound in tolerances.toml is untouched.
grep -q '^  period_ms = \[999.5, 1010.0\]$' "$TOL" || { echo "ci-smoke: tick-drift period_ms row not found in tolerances.toml" >&2; exit 2; }
sed -i 's/^  period_ms = \[999.5, 1010.0\]$/  period_ms = [995.0, 1015.0]/' "$TOL"
set +e
"$RP" diff "$OUT" --tolerances "$TOL"
RC=$?
set -e
echo "== diff exit $RC ($OUT/diff.md)"

if [ "$MODE" = media ]; then
  # The media plane really ran on both agents (a self-diff alone would pass on
  # two empty agents): video + telemetry objects PUT and SigV4-accepted by the
  # fake S3, real gst captures spawned through the shim, and the Rust agent's
  # own tape carries the Phase 3 kinds.
  python3 - "$OUT" <<'PY'
import json, sys
out = sys.argv[1]
for side in ("a", "b"):
    puts = [json.loads(l) for l in open(f"{out}/{side}/s3.ndjson")]
    ok = [p for p in puts if p.get("status") == 200 and p.get("sig") == "ok"]
    mp4 = [p for p in ok if p["key"].endswith(".mp4")]
    tel = [p for p in ok if p["key"].startswith("telemetry/")]
    print(f"{side}: s3 PUTs {len(puts)} (200+sig ok {len(ok)}; mp4 {len(mp4)}, telemetry {len(tel)})")
    if len(ok) != len(puts) or len(mp4) < 4 or len(tel) < 1:
        sys.exit(f"{side}: media uploads missing or rejected")
    gst = [l for l in open(f"{out}/{side}/shim.ndjson") if l.strip()]
    print(f"{side}: gst-launch-1.0 invocations {len(gst)}")
    if len(gst) < 2:
        sys.exit(f"{side}: no capture/bridge pipelines spawned")
PY
  KINDS=$("$RP" inspect "$OUT/b/agent-tape.ndjson.gz" | sed -n '/^kinds:/,/^[^ ]/p')
  echo "$KINDS"
  for k in child_spawn child_line child_exit fs_out s3_put; do
    grep -qE "^  $k " <<<"$KINDS" || { echo "agent tape lacks '$k'"; exit 1; }
  done
  exit $RC
fi

# Inputs reached the agents (a self-diff alone would pass on two empty agents).
python3 - "$OUT" <<'PY'
import json, sys
out = sys.argv[1]
groups = set()
for line in open(f"{out}/b/mqtt.ndjson"):
    m = json.loads(line)
    if m["topic"].endswith("/telemetry"):
        groups |= set(json.loads(m["payload"]).get("metrics", {}).keys())
missing = {"gps", "imu", "lte", "system"} - groups
print("telemetry groups:", sorted(groups))
if missing:
    sys.exit(f"telemetry never carried {sorted(missing)}")
PY
KINDS=$("$RP" inspect "$OUT/b/agent-tape.ndjson.gz" | sed -n '/^kinds:/,/^[^ ]/p')
echo "$KINDS"
for k in nmea imu lte_file exec i2cdetect; do
  grep -qE "^  $k " <<<"$KINDS" || { echo "agent tape lacks '$k'"; exit 1; }
done
exit $RC
