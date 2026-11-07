#!/usr/bin/env bash
# shellcheck disable=SC2015  # test assertions: `a && ok || bad`
# O-P6-1 (Phase 6 window 1): with Conflicts= alone, `systemctl start
# xorgate-agent-rs` ran in PARALLEL with the JS agent's stop. This suite runs
# the REAL shipped unit under a REAL systemd 252 (PID 1 in a privileged
# bookworm container) next to a JS stand-in with the JS unit's shape, and
# proves the job ORDER from what the stub processes log:
#   A. the pre-fix unit (no After=) reproduces the overlap        (control)
#   B. convert (step 8: enable rs, disable JS, start rs): JS's stop, incl.
#      its ExecStopPost, completes before Rust's ExecStartPre
#   C. reverse: `systemctl start xorgate-agent` while Rust runs: Rust's stop,
#      incl. its ExecStopPost, completes before JS's ExecStartPre (no JS-side
#      After= needed: the rule is symmetric)
#   D. bench shape (Conflicts= stripped, as scripts/bench/install.sh does):
#      both start in one transaction (boot-like), both run, Rust after JS's
#      start job; a JS restart leaves Rust alone
#   F. a boot with BOTH enabled (install-rs.sh's M2 power-cut window):
#      exactly one agent runs, the same one as with the pre-fix unit
#   E. the updater's path: `systemctl restart xorgate-agent-rs` with JS
#      inactive is not delayed
# Run (systemd must be PID 1):
#   docker run -d --name xg-order --label xorgate.agent=<tag> --privileged --cgroupns=private \
#     --tmpfs /run --tmpfs /run/lock -v "$PWD:/src:ro" debian:bookworm \
#     bash -c 'apt-get update -qq && apt-get install -y -qq systemd >/dev/null && exec /lib/systemd/systemd'
#   docker exec xg-order bash /src/scripts/tests/unit-ordering.sh
#   docker rm -f xg-order
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
[ "$(cat /proc/1/comm)" = systemd ] || { echo "FAIL: systemd is not PID 1 (see the header)"; exit 2; }
for _ in $(seq 60); do s=$(systemctl is-system-running 2>/dev/null); case $s in running|degraded) break ;; esac; sleep 1; done
echo "  $(systemctl --version | head -1); system $s"

LOG=/var/log/xg-order.log
D=/etc/systemd/system
mkdir -p /opt/xorgate-rs/bin /usr/local/bin /etc/xorgate
touch /etc/xorgate/agent.env
# One stub for every binary both units run. $0 tells which one it is.
cat > /usr/local/lib-xg-stub <<'EOF'
#!/bin/bash
who=rs; case $0 in */xorgate|*/js-agent) who=js ;; esac
log() { echo "$who $*" >> /var/log/xg-order.log; }
if [ "${1:-}" = led ]; then log "led $2"; sleep 0.2; exit 0; fi
log start
# A slow SIGTERM path (1 s) makes any overlap visible.
trap 'log sigterm; sleep 1; log exit; exit 0' TERM
if [ "$who" = rs ]; then systemd-notify --ready; fi   # Type=notify, sent with our pid
while :; do sleep 1 & wait $!; [ "$who" = rs ] && systemd-notify WATCHDOG=1; done
EOF
chmod 0755 /usr/local/lib-xg-stub
for b in /opt/xorgate-rs/bin/xorgate-rs /usr/local/bin/xorgate-rs /usr/local/bin/xorgate /usr/local/bin/js-agent; do
  ln -sf /usr/local/lib-xg-stub "$b"
done
# The JS unit's shape (device/xorgate-device-agent systemd/xorgate-agent.service @ 09da38d).
cat > $D/xorgate-agent.service <<'EOF'
[Unit]
Description=JS agent stand-in
After=network-online.target
Wants=network-online.target
[Service]
Type=simple
EnvironmentFile=/etc/xorgate/agent.env
ExecStartPre=-/usr/local/bin/xorgate led boot
ExecStart=/usr/local/bin/js-agent
ExecStopPost=-/usr/local/bin/xorgate led exit-state
Restart=always
RestartSec=5s
[Install]
WantedBy=multi-user.target
EOF
SHIPPED="$REPO/systemd/xorgate-agent-rs.service"
BENCH_SED='s|^Conflicts=xorgate-agent.service$|# BENCH (scripts/bench/install.sh): Conflicts=xorgate-agent.service stripped — both agents coexist on the bench|'

use_unit() { # <label> <file>
  systemctl stop xorgate-agent-rs xorgate-agent 2>/dev/null
  systemctl disable -q xorgate-agent-rs xorgate-agent 2>/dev/null
  install -m 0644 "$2" $D/xorgate-agent-rs.service
  systemctl daemon-reload; systemctl reset-failed 2>/dev/null
  echo "== $1"
}
wait_active() { for _ in $(seq 50); do [ "$(systemctl is-active "$1")" = active ] && return 0; sleep 0.2; done; return 1; }
settle() { sleep 2; }
# idx <line>: the log line number of the first exact match (0 = absent).
idx() { grep -nxF -- "$1" "$LOG" | head -1 | cut -d: -f1 | sed 's/^$/0/'; }
before() { # <a> <b>: a logged, b logged, a before b
  local a b; a=$(idx "$1"); b=$(idx "$2"); a=${a:-0}; b=${b:-0}
  [ "$a" -gt 0 ] && [ "$b" -gt 0 ] && [ "$a" -lt "$b" ]
}
show() { sed 's/^/    | /' "$LOG"; }

# ---- A: control, the pre-fix unit (Conflicts=, no After=) ----
sed '/^After=xorgate-agent.service$/d' "$SHIPPED" > /tmp/prefix.service
grep -q '^After=xorgate-agent.service' /tmp/prefix.service && bad "control unit still has After="
use_unit "A. control: pre-fix unit (no After=), convert with JS active" /tmp/prefix.service
systemctl enable -q xorgate-agent; systemctl start xorgate-agent; wait_active xorgate-agent; settle
: > "$LOG"
systemctl enable -q xorgate-agent-rs; systemctl disable -q xorgate-agent; systemctl start xorgate-agent-rs
wait_active xorgate-agent-rs; settle; show
before "rs led boot" "js led exit-state" && ok "control reproduces O-P6-1: Rust's ExecStartPre ran before JS's ExecStopPost (parallel jobs)" \
  || bad "control did not reproduce the overlap (the demo proves nothing)"

# ---- B: convert with the shipped unit ----
use_unit "B. shipped unit (After=), convert with JS active (install-rs.sh step 8)" "$SHIPPED"
systemctl enable -q xorgate-agent; systemctl start xorgate-agent; wait_active xorgate-agent; settle
: > "$LOG"
systemctl enable -q xorgate-agent-rs; systemctl disable -q xorgate-agent; systemctl start xorgate-agent-rs
wait_active xorgate-agent-rs; settle; show
before "js exit" "rs led boot" && before "js led exit-state" "rs led boot" && before "rs led boot" "rs start" \
  && ok "convert: JS stopped (exit + ExecStopPost) before Rust's ExecStartPre and start" || bad "convert: jobs overlapped"
[ "$(systemctl is-active xorgate-agent)/$(systemctl is-active xorgate-agent-rs)" = inactive/active ] && ok "convert: JS inactive, Rust active" || bad "convert: end state"

# ---- C: the reverse direction (a JS start that conflicts Rust out) ----
echo "== C. shipped unit, 'systemctl start xorgate-agent' while Rust is active"
: > "$LOG"
systemctl start xorgate-agent; wait_active xorgate-agent; settle; show
before "rs exit" "js led boot" && before "rs led exit-state" "js led boot" \
  && ok "reverse: Rust stopped (exit + ExecStopPost) before JS's ExecStartPre (no JS-side After= needed)" || bad "reverse: jobs overlapped"
[ "$(systemctl is-active xorgate-agent)/$(systemctl is-active xorgate-agent-rs)" = active/inactive ] && ok "reverse: JS active, Rust inactive" || bad "reverse: end state"

# ---- D: bench shape ----
sed -e "$BENCH_SED" "$SHIPPED" > /tmp/bench.service
use_unit "D. bench shape (Conflicts= stripped, After= kept): both pulled in by ONE target (a boot)" /tmp/bench.service
# A boot is ONE transaction: multi-user.target Wants= both (WantedBy=). A
# stand-in target does the same here. (`systemctl start a b` would not:
# systemctl enqueues one transaction per unit, and ordering only binds jobs
# that are pending together.)
printf '[Unit]\nDescription=boot stand-in\nWants=xorgate-agent-rs.service xorgate-agent.service\n' > $D/xg-boot.target
systemctl daemon-reload
: > "$LOG"
systemctl start xg-boot.target; wait_active xorgate-agent; wait_active xorgate-agent-rs; settle; show
[ "$(systemctl is-active xorgate-agent)/$(systemctl is-active xorgate-agent-rs)" = active/active ] && ok "bench: both agents active (no conflict, no deadlock)" || bad "bench: not both active"
before "js start" "rs led boot" && ok "bench: Rust starts after JS's start job (Type=simple: immediate)" || bad "bench: order"
systemctl stop xg-boot.target
rs_pid=$(systemctl show -p MainPID --value xorgate-agent-rs)
systemctl restart xorgate-agent; wait_active xorgate-agent; settle
[ "$(systemctl show -p MainPID --value xorgate-agent-rs)" = "$rs_pid" ] && [ "$(systemctl is-active xorgate-agent-rs)" = active ] \
  && ok "bench: restarting JS leaves Rust running (MainPID $rs_pid unchanged)" || bad "bench: a JS restart touched Rust"
t0=$(date +%s%N); systemctl restart xorgate-agent-rs; wait_active xorgate-agent-rs; t1=$(date +%s%N)
[ "$(systemctl is-active xorgate-agent)" = active ] && ok "bench: restarting Rust leaves JS running ($(( (t1 - t0) / 1000000 )) ms)" || bad "bench: a Rust restart touched JS"

# ---- E: the updater path on a converted device ----
use_unit "E. shipped unit, JS disabled + inactive: 'systemctl restart xorgate-agent-rs' (xorgate-rs update)" "$SHIPPED"
systemctl enable -q xorgate-agent-rs; systemctl start xorgate-agent-rs; wait_active xorgate-agent-rs; settle
: > "$LOG"
t0=$(date +%s%N); systemctl restart xorgate-agent-rs; wait_active xorgate-agent-rs; t1=$(date +%s%N); settle; show
ms=$(( (t1 - t0) / 1000000 ))
! grep -q '^js ' "$LOG" && [ "$ms" -lt 5000 ] && ok "restart: no JS job, not delayed (${ms} ms, of which ~1 s is the stub's own SIGTERM path)" || bad "restart: JS involved or slow (${ms} ms)"

# ---- F: a boot with BOTH enabled (install-rs.sh M2 power-cut window) ----
boot_both() { # <label> <unit>: prints "<js>/<rs>" after a one-transaction boot
  use_unit "$1" "$2" >&2
  systemctl enable -q xorgate-agent xorgate-agent-rs
  printf '[Unit]\nDescription=boot stand-in\nWants=xorgate-agent-rs.service xorgate-agent.service\n' > $D/xg-boot.target
  systemctl daemon-reload; : > "$LOG"
  systemctl start xg-boot.target 2>&1 | sed 's/^/    ! /' >&2
  sleep 3; show >&2
  echo "$(systemctl is-active xorgate-agent)/$(systemctl is-active xorgate-agent-rs)"
  systemctl stop xg-boot.target 2>/dev/null
}
pre=$(boot_both "F1. both enabled, pre-fix unit: one-transaction boot" /tmp/prefix.service)
post=$(boot_both "F2. both enabled, shipped unit: one-transaction boot" "$SHIPPED")
echo "    pre-fix js/rs = $pre; shipped js/rs = $post"
case $post in active/inactive|inactive/active) ok "power-cut boot: exactly one agent runs ($post)" ;; *) bad "power-cut boot: $post" ;; esac
[ "$pre" = "$post" ] && ok "power-cut boot: After= does not change which agent the transaction keeps" || bad "power-cut boot: After= changed the winner ($pre -> $post)"

systemctl stop xorgate-agent-rs xorgate-agent 2>/dev/null
rm -f $D/xg-boot.target
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
