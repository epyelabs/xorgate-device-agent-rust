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
#   tools/replay/ci-smoke.sh <xorgate-rs> <xorgate-replay> [out dir]
#
# Needs: mosquitto, openssl, python3, procps (apt). Run as root (the player
# writes /etc/hosts `broker` → 127.0.0.1 when it is missing). The JS-vs-Rust
# parity run stays the Docker rig's job (run-rig.sh).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
RS=$(realpath "${1:?usage: ci-smoke.sh <xorgate-rs> <xorgate-replay> [out]}")
RP=$(realpath "${2:?usage: ci-smoke.sh <xorgate-rs> <xorgate-replay> [out]}")
OUT=${3:-/tmp/replay-ci-smoke}
TAPE="$REPO/fixtures/replay/smoke.ndjson.gz"

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
trap 'kill $MQ 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  (exec 3<>/dev/tcp/127.0.0.1/8884) 2>/dev/null && break
  sleep 0.1
done

rm -rf "$OUT"
"$RP" play --tape "$TAPE" --out "$OUT" --a rust --b rust --rs-bin "$RS" \
  --broker-host localhost --broker-port 8884 --proxy-listen 127.0.0.1:8883 \
  --certs "$CERTS" --shims "$HERE/shims" --fake-python "$HERE/fake-python" \
  --work /tmp/replay-ci-work --settle-s 10 --stagger-ms 300
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
