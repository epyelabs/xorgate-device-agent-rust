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
#   G. the updater unit (xorgate-updater-rs.service as shipped) and a running
#      apt transaction: what its start timeout and `systemctl stop` send to
#      whom, with KillMode=mixed + TimeoutStopSec=15min (see the section)
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

# ---- G: the updater unit and a running apt transaction ----
# `xorgate-rs update` ends with the camera-packages step, which may be inside
# an `apt-get install` when systemd stops the unit (its start timeout, a
# `systemctl stop`, a shutdown). This runs the REAL shipped
# xorgate-updater-rs.service (its KillMode and TimeoutStopSec as shipped;
# only ExecStart and the two timeouts are replaced, to seconds) with
# stand-ins shaped like the real processes:
#   updater (the unit's main process)
#     -> step, in its own process group   (`camera requirements --install`)
#          -> apt, in its own process group  (the `apt-get install`)
# and records what each of them was sent and whether "apt" got to finish.
#   G1 control: KillMode=control-group (the unit before): the start timeout
#      SIGTERMs apt itself.
#   G2 control: KillMode=mixed with an updater that dies on SIGTERM (the
#      default): systemd SIGKILLs what is left the moment the main process
#      is gone. mixed ALONE does not let apt finish.
#   G3 the shipped unit + an updater that passes the stop on and waits (what
#      `xorgate-rs update` does in that step): apt survives the START TIMEOUT
#      and finishes.
#   G4 the same under `systemctl stop`: the stop waits for apt.
#   G5 nothing to protect (the step is in a killable phase): `systemctl stop`
#      returns promptly.
#   G6 the bound: an apt that outlasts TimeoutStopSec is killed. (Shipped:
#      15 min, above the 12.5 min an install and what follows it may take.)
echo "== G. the updater unit and a running apt transaction"
UL=/var/log/xg-updater.log
cat > /usr/local/bin/xg-updater-standin <<'EOF'
#!/bin/bash
L=/var/log/xg-updater.log
log() { echo "$1" >> $L; }
# Each child in its own process group, as Command::process_group(0); the
# `perl … exec` keeps the pid, so $! is the child itself.
OWN='setpgrp(0,0); exec @ARGV'
case ${1:-updater} in
  apt)   # the `apt-get install`: $2 seconds of work
    trap 'log "apt: GOT SIGTERM"; exit 143' TERM
    log "apt: transaction begins"
    sleep "$2" & wait $!
    log "apt: transaction finished"; exit 0 ;;
  step)  # `camera requirements --install`
    what=$(cat /etc/xg-updater.step)
    if [ "$what" = idle ]; then   # a killable phase: a stop ends it at once
      trap 'log "step: stopped at once"; exit 1' TERM
      log "step: downloading"; sleep 600 & wait $!; exit 1
    fi
    trap 'log "step: stop noted, waiting for apt"' TERM   # in its transaction
    perl -e "$OWN" "$0" apt "${what#apt }" & apt=$!
    while kill -0 "$apt" 2>/dev/null; do wait "$apt"; done
    log "step: apt ended, hold and restart done"; exit 0 ;;
  updater)
    log "updater: update complete, the step begins"
    perl -e "$OWN" "$0" step & step=$!
    if [ "$(cat /etc/xg-updater.mode)" = passes ]; then   # what `xorgate-rs update` does
      trap 'log "updater: stop passed on to the step"; kill -TERM "$step" 2>/dev/null' TERM
      while kill -0 "$step" 2>/dev/null; do wait "$step"; done
    else                                                  # SIGTERM's default: it dies
      wait "$step"
    fi
    log "updater: exits"; exit 0 ;;
esac
EOF
chmod 0755 /usr/local/bin/xg-updater-standin
UPD="$REPO/systemd/xorgate-updater-rs.service"
cp "$UPD" $D/xorgate-updater-rs.service
mkdir -p $D/xorgate-updater-rs.service.d
# <mode> <step> <TimeoutStartSec> [extra [Service] lines]
updater_case() {
  systemctl stop xorgate-updater-rs 2>/dev/null; systemctl reset-failed xorgate-updater-rs 2>/dev/null
  : > $UL; echo "$1" > /etc/xg-updater.mode; echo "$2" > /etc/xg-updater.step
  printf '[Service]\nExecStart=\nExecStart=/usr/local/bin/xg-updater-standin\nTimeoutStartSec=%s\n%b' "$3" "${4:-}" \
    > $D/xorgate-updater-rs.service.d/10-test.conf
  systemctl daemon-reload
}
wait_log() { local i; for i in $(seq "${2:-100}"); do grep -q "$1" $UL 2>/dev/null && return 0; sleep 0.1; done; return 1; }
gone() { [ -z "$(cat "/sys/fs/cgroup/system.slice/xorgate-updater-rs.service/cgroup.procs" 2>/dev/null)" ]; }
show_log() { sed 's/^/      /' $UL; }

updater_case passes "apt 1" 60
k=$(systemctl show -p KillMode --value xorgate-updater-rs); t=$(systemctl show -p TimeoutStopUSec --value xorgate-updater-rs)
echo "    loaded: KillMode=$k TimeoutStopUSec=$t TimeoutStartUSec=$(systemctl show -p TimeoutStartUSec --value xorgate-updater-rs)"
[ "$k" = mixed ] && [ "$t" = 15min ] && ok "G0: the shipped unit loads with KillMode=mixed and TimeoutStopSec=15min" || bad "G0: KillMode=$k TimeoutStopUSec=$t"
grep -qx 'TimeoutStartSec=45min' "$UPD" && ok "G0: the shipped unit's TimeoutStartSec is 45min" || bad "G0: TimeoutStartSec"

echo "  -- G1 (control) KillMode=control-group, as the unit was: the start timeout (3 s) with apt needing 8 s"
updater_case dies "apt 8" 3 'KillMode=control-group\n'
systemctl start xorgate-updater-rs 2>/dev/null; sleep 9; show_log
grep -q 'apt: GOT SIGTERM' $UL && ! grep -q 'apt: transaction finished' $UL \
  && ok "G1: with control-group the start timeout SIGTERMs apt itself; it does not finish" || bad "G1: control-group did not signal apt"

echo "  -- G2 (control) KillMode=mixed, an updater that dies on SIGTERM: the start timeout (3 s) with apt needing 8 s"
updater_case dies "apt 8" 3
systemctl start xorgate-updater-rs 2>/dev/null; sleep 9; show_log
! grep -q 'apt: GOT SIGTERM' $UL && ! grep -q 'apt: transaction finished' $UL && ! grep -q 'updater: exits' $UL && gone \
  && ok "G2: mixed alone: apt got no SIGTERM, but it was KILLED when the main process went (it never finished)" || bad "G2: unexpected (see the log above)"

echo "  -- G3 the shipped unit, an updater that passes the stop on and waits: the start timeout (3 s) with apt needing 8 s"
updater_case passes "apt 8" 3
t0=$(date +%s.%N); systemctl start xorgate-updater-rs 2>/dev/null; rc=$?; t1=$(date +%s.%N); show_log
took=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.1f", b - a }')
echo "    systemctl start returned $rc after ${took}s; Result=$(systemctl show -p Result --value xorgate-updater-rs)"
grep -q 'updater: stop passed on to the step' $UL && grep -q 'step: stop noted, waiting for apt' $UL && ok "G3: the timeout reached the updater only; it passed the stop on, the step noted it" || bad "G3: the stop was not passed on"
grep -q 'apt: transaction finished' $UL && ! grep -q 'apt: GOT SIGTERM' $UL && ok "G3: apt was never signalled and FINISHED, 5 s after the start timeout" || bad "G3: apt did not finish"
grep -q 'step: apt ended, hold and restart done' $UL && grep -q 'updater: exits' $UL && gone && ok "G3: the step did what follows the install; then the updater exited; nothing is left" || bad "G3: the step or the updater did not end in order"
awk -v t="$took" 'BEGIN { exit !(t >= 7 && t < 14) }' && [ "$rc" != 0 ] && ok "G3: the start job failed (timeout) only once apt was done (${took}s)" || bad "G3: timing ${took}s rc=$rc"

echo "  -- G4 the same, 'systemctl stop' 1 s into an apt needing 8 s"
updater_case passes "apt 8" 60
systemctl start --no-block xorgate-updater-rs; wait_log 'apt: transaction begins' || bad "G4: apt did not begin"
sleep 1; t0=$(date +%s.%N); systemctl stop xorgate-updater-rs; t1=$(date +%s.%N); show_log
took=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.1f", b - a }')
grep -q 'apt: transaction finished' $UL && ! grep -q 'apt: GOT SIGTERM' $UL && grep -q 'updater: exits' $UL && gone \
  && ok "G4: 'systemctl stop' did not signal apt; apt finished, then the unit stopped" || bad "G4: apt did not finish under stop"
awk -v t="$took" 'BEGIN { exit !(t >= 5 && t < 12) }' && ok "G4: the stop waited for apt (${took}s)" || bad "G4: stop took ${took}s"

echo "  -- G5 nothing to protect: 'systemctl stop' while the step downloads"
updater_case passes idle 60
systemctl start --no-block xorgate-updater-rs; wait_log 'step: downloading' || bad "G5: the step did not begin"
t0=$(date +%s.%N); systemctl stop xorgate-updater-rs; t1=$(date +%s.%N); show_log
took=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.1f", b - a }')
grep -q 'step: stopped at once' $UL && gone && awk -v t="$took" 'BEGIN { exit !(t < 3) }' \
  && ok "G5: with no transaction running the unit stops promptly (${took}s)" || bad "G5: stop took ${took}s"

echo "  -- G6 the bound: TimeoutStopSec=4 s (shipped: 15 min), an apt needing 15 s"
updater_case passes "apt 15" 60 'TimeoutStopSec=4\n'
systemctl start --no-block xorgate-updater-rs; wait_log 'apt: transaction begins' || bad "G6: apt did not begin"
t0=$(date +%s.%N); systemctl stop xorgate-updater-rs; t1=$(date +%s.%N); sleep 1; show_log
took=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.1f", b - a }')
! grep -q 'apt: transaction finished' $UL && ! grep -q 'apt: GOT SIGTERM' $UL && gone && awk -v t="$took" 'BEGIN { exit !(t >= 3 && t < 8) }' \
  && ok "G6: an apt that outlasts TimeoutStopSec is killed when it runs out (${took}s): the shipped 15 min must exceed the longest install" || bad "G6: stop took ${took}s"

systemctl stop xorgate-updater-rs 2>/dev/null; systemctl reset-failed xorgate-updater-rs 2>/dev/null
rm -rf $D/xorgate-updater-rs.service $D/xorgate-updater-rs.service.d /etc/xg-updater.mode /etc/xg-updater.step; systemctl daemon-reload
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
