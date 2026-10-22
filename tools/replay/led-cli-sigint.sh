#!/bin/bash
# E3 LED CLI, SIGINT mid-pattern (Phase 4 gate harness, gate-offbench/E3-led-cli/e3run3;
# F-E3-1 / D36): the TS `xorgate led` (node dist/cli.js @$JS_REF (default 89b5c64; `run -e JS_REF=<sha>`)) and `xorgate-rs led`
# (/rig/cache/bin) each run 9× under the rig's Pi namespace with the pinctrl shim
# logging "<wall ms> <argv>"; the harness logs the SIGINT wall ms and sends it.
# Runs INSIDE the rig container, writing to /rig/out (= $RIG_RUNS):
#   RIG_RUNS=<dir> docker compose -p <proj> -f tools/replay/docker-compose.yml \
#     run --rm -T --entrypoint bash rig /rig/tools/led-cli-sigint.sh
#   docker compose -p <proj> -f tools/replay/docker-compose.yml down -v --remove-orphans
#   python3 -I tools/replay/led-cli-sigint-cmp.py <dir>      # D36 comparator
set -u
JS_REF="${JS_REF:-89b5c64}"; JS_DIR="/rig/jscache/js-$JS_REF"   # Phase 5b: shared build per ref
O=/rig/out; P=$O/pi; mkdir -p /boot/firmware
mkdir -p $P/boot $P/dt $O/sh-full $O/sh-nopinctrl $O/st-free/exec $O/st-foreign/exec $O/st-own/exec
printf 'Raspberry Pi Compute Module 5 Rev 1.0\0' > $P/dt/model
cp /rig/tools/shims/pinctrl /rig/tools/shims/gpioinfo $O/sh-full/; cp /rig/tools/shims/gpioinfo $O/sh-nopinctrl/
mk() { # <dir> <consumer for lines 12,16,21 or empty>
  { echo "gpiochip0 - 54 lines:"; for i in $(seq 0 53); do
      if [ -n "$2" ] && { [ $i = 12 ] || [ $i = 16 ] || [ $i = 21 ]; }; then printf '\tline %3d:      "GPIO%d"       "%s"   output  active-high [used]\n' $i $i "$2";
      else printf '\tline %3d:      "GPIO%d"       unused   input  active-high \n' $i $i; fi; done; } > $1/exec/gpioinfo__gpiochip0.out
  echo 0 > $1/exec/gpioinfo__gpiochip0.code; : > $1/exec/gpioinfo__gpiochip0.err; }
mk $O/st-free ""; mk $O/st-foreign "pigpio"; mk $O/st-own "xorgate-rs"
side() { # <side js|rs> <case> <shims> <state> <pi 1|0> <sigint-after-s or 0> <SERVICE_RESULT or -> args…
  local s=$1 c=$2 sh=$3 st=$4 pi=$5 kill=$6 sr=$7; shift 7
  local d=$O/$s; mkdir -p $d $O/home-$s
  local cmd; if [ $s = js ]; then cmd=(node "$JS_DIR"/dist/cli.js); else cmd=(/rig/cache/bin/xorgate-rs); fi
  local pre=(); [ $pi = 1 ] && pre=(unshare -m --propagation private -- /rig/tools/pi-ns.sh $P)
  local to=(); [ $kill != 0 ] && to=(timeout -s INT $kill)
  : > $d/$c.led
  local envs=(env -i PATH=$O/$sh:/usr/local/bin:/usr/bin:/bin HOME=/root TZ=UTC RIG_LED_LOG=$d/$c.led RIG_STATE=$O/$st XORGATE_LED_WRITER=pinctrl XORGATE_HOME=$O/home-$s)
  [ "$sr" != - ] && envs+=(SERVICE_RESULT=$sr)
  "${to[@]}" "${pre[@]}" "${envs[@]}" "${cmd[@]}" led "$@" > $d/$c.out 2> $d/$c.err; echo $? > $d/$c.code
}
both() { side js "$@"; side rs "$@"; echo "case $1 done js=$(cat $O/js/$1.code) rs=$(cat $O/rs/$1.code) writes js=$(wc -l < $O/js/$1.led) rs=$(wc -l < $O/rs/$1.led)"; }
sig() { # <side> <case> <delay-s> args… : start, wait, log the SIGINT wall ms, send it
  local s=$1 c=$2 dl=$3; shift 3; local d=$O/$s; mkdir -p $d $O/home-$s; : > $d/$c.led
  local cmd; if [ $s = js ]; then cmd=(node "$JS_DIR"/dist/cli.js); else cmd=(/rig/cache/bin/xorgate-rs); fi
  env -i PATH=$O/sh-full:/usr/local/bin:/usr/bin:/bin HOME=/root TZ=UTC RIG_LED_LOG=$d/$c.led RIG_STATE=$O/st-free XORGATE_LED_WRITER=pinctrl XORGATE_HOME=$O/home-$s \
    unshare -m --propagation private -- /rig/tools/pi-ns.sh $P "${cmd[@]}" led "$@" > $d/$c.out 2> $d/$c.err & local pid=$!
  sleep $dl; date +%s%3N > $d/$c.sig; kill -INT $pid; wait $pid; echo $? > $d/$c.code; sleep 0.5
}
for i in 1 2 3 4 5 6; do d=$(awk -v i=$i 'BEGIN{printf "%.2f", 3 + i*0.023}'); sig js b$i $d blue blink-fast; sig rs b$i $d blue blink-fast; done
for i in 1 2 3; do sig js s$i 4.3 cyan blink-slow; sig rs s$i 4.3 cyan blink-slow; done
echo E3 DONE
