#!/usr/bin/env bash
# Safety test for scripts/bench/exclusive.sh (review-wp7 fix 6), run as ROOT in a
# throwaway container: `docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/exclusive-safety.sh`
# Stubs ssh/sudo/systemctl so the real script body runs locally; asserts the PROD
# JS unit (xorgate-agent) is active after every failure path.
set -uo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root (in a container)"; exit 2; }
command -v python3 >/dev/null || { apt-get update -qq >/dev/null && apt-get install -y -qq python3 >/dev/null; }
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
STUB=$(mktemp -d); STATE=$(mktemp -d)
export SYSD_STATE=$STATE
cat > "$STUB/ssh" <<'S'
#!/usr/bin/env bash
# skip ssh options and the host, run the remote command locally
while [ $# -gt 0 ]; do case "$1" in -o|-i|-p|-F) shift 2;; -*) shift;; *) shift; break;; esac; done
exec "$@"
S
cat > "$STUB/sudo" <<'S'
#!/usr/bin/env bash
exec "$@"
S
cat > "$STUB/systemctl" <<'S'
#!/usr/bin/env bash
st="$SYSD_STATE"; cmd="$1"; shift || true
case "$cmd" in
  is-active) u="$1"; s=$(cat "$st/$u" 2>/dev/null || echo inactive); echo "$s"; [ "$s" = active ];;
  start|restart) u="$1"; [ -f "$st/FAIL_START_$u" ] && { echo "stub: start $u failed" >&2; exit 1; }; echo active > "$st/$u";;
  stop) u="$1"; echo inactive > "$st/$u";;
  daemon-reload) [ -f "$st/FAIL_RELOAD" ] && { echo "stub: daemon-reload failed" >&2; exit 1; }; true;;
  show) echo "";;
  *) true;;
esac
S
chmod +x "$STUB"/*
export PATH="$STUB:$PATH" BENCH=stub@localhost
# minimal on-"bench" fixtures the script expects
mkdir -p /etc/systemd/system /etc/xorgate /var/lib/xorgate-rs/evidence /var/lib/xorgate /run/systemd/system
touch /etc/systemd/system/xorgate-agent-rs.service
echo '{"latest":{"metrics":{"system":{}}}}' > /var/lib/xorgate/state.json
id pi >/dev/null 2>&1 || useradd -M pi
fail=0
check() { local want="$1" msg="$2"; local got; got=$(cat "$STATE/xorgate-agent" 2>/dev/null || echo inactive)
  if [ "$got" = "$want" ]; then echo "PASS: $msg (xorgate-agent=$got)"; else echo "FAIL: $msg (xorgate-agent=$got, want $want)"; fail=1; fi; }
reset() { rm -f "$STATE"/*; echo active > "$STATE/xorgate-agent"; echo active > "$STATE/xorgate-agent-rs"; echo active > "$STATE/sim7600-lte"; }

echo "== case 1: end with JS stopped and the Rust restart failing (daemon-reload fails)"
reset; echo inactive > "$STATE/xorgate-agent"; touch "$STATE/FAIL_RELOAD"
bash "$REPO/scripts/bench/exclusive.sh" end >/tmp/c1.log 2>&1; echo "  exclusive.sh end rc=$?"
check active "end starts JS even though the later Rust restart fails"

echo "== case 2: end run twice (idempotent)"
reset; echo inactive > "$STATE/xorgate-agent"
bash "$REPO/scripts/bench/exclusive.sh" end >/tmp/c2a.log 2>&1; bash "$REPO/scripts/bench/exclusive.sh" end >/tmp/c2b.log 2>&1; echo "  rc=$?"
check active "double end leaves JS active"

echo "== case 3: begin aborts after JS was stopped (preflight passes, exclusive profile step fails)"
reset; mkdir -p /etc/xorgate; : > /etc/xorgate/agent-rs-dev.env
# make preflight pass, then force a failure at the restart step
echo "KVS_MANIFEST=/var/lib/xorgate-rs/etc/kvs.json" > /etc/xorgate/agent-rs-dev.env
mkdir -p /var/lib/xorgate-rs/etc
for f in agent.env kvs.json hardware.json; do echo x > /etc/xorgate/$f; done
C15=/var/lib/xorgate-rs/evidence/c15-baseline.sha256   # = _lib.sh C15_BASELINE
mkdir -p "$(dirname "$C15")"; (cd / && sha256sum etc/xorgate/agent.env etc/xorgate/kvs.json etc/xorgate/hardware.json) > "$C15"
echo '{"v":2,"imu":"bno085"}' > /var/lib/xorgate-rs/etc/hardware-exclusive.json   # = HW_EXCLUSIVE
echo '{"v":2,"imu":null}' > /var/lib/xorgate-rs/etc/hardware.json               # = HW_SHARED
touch "$STATE/FAIL_RELOAD"
bash "$REPO/scripts/bench/exclusive.sh" begin >/tmp/c3.log 2>&1; echo "  exclusive.sh begin rc=$?"
check active "aborted begin restores JS via the trap"
if grep -q "begin aborted" /tmp/c3.log; then echo "PASS: trap fired after JS was stopped"; else echo "FAIL: trap did not fire (begin refused before stopping JS?) — log:"; tail -8 /tmp/c3.log; fail=1; fi

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
