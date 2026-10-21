#!/bin/bash
# D36 / review-gate-fixes F-R1 — SIGINT while a SLOW lit pattern step is in
# flight (the reviewer's adversarial repro). A pinctrl shim whose writes land
# (are logged) when the child FINISHES, `dh` taking $SLOW_DH_S (0.08 s); the
# harness waits for a `dh` to start, sends SIGINT 10 ms later. TS @89b5c64 and
# xorgate-rs `blue blink-fast`, 5 cases each. Verdict per case and side: the
# LED ends dark (the last landed write is the all-pin blank); Rust also lands
# exactly 2 blanks after the signal (D36). Runs INSIDE the rig container:
#   RIG_RUNS=<dir> docker compose -p <proj> -f tools/replay/docker-compose.yml \
#     run --rm -T --entrypoint bash rig /rig/tools/led-cli-sigint-slow.sh
#   docker compose -p <proj> -f tools/replay/docker-compose.yml down -v --remove-orphans
set -u
O=/rig/out; P=$O/pi; SH=$O/sh-slow
mkdir -p /boot/firmware $P/boot $P/dt $O/st-free/exec $SH
printf 'Raspberry Pi Compute Module 5 Rev 1.0\0' > $P/dt/model
{ echo "gpiochip0 - 54 lines:"; for i in $(seq 0 53); do printf '\tline %3d:      "GPIO%d"       unused   input  active-high \n' $i $i; done; } > $O/st-free/exec/gpioinfo__gpiochip0.out
echo 0 > $O/st-free/exec/gpioinfo__gpiochip0.code; : > $O/st-free/exec/gpioinfo__gpiochip0.err
cp /rig/tools/shims/gpioinfo $SH/
cat > $SH/pinctrl <<'S'
#!/bin/sh
# Slow shim: a write lands (is logged) when the child FINISHES; `dh` is slow
# and touches a start marker first.
case "$*" in
  *" op dh") : > "${RIG_LED_LOG}.dhstart"; sleep "${SLOW_DH_S:-0.08}" ;;
esac
printf '%s %s\n' "$(date +%s%3N)" "$*" >> "${RIG_LED_LOG:-/dev/null}"
exit 0
S
chmod +x $SH/pinctrl
sig() { # <side> <case> <cmd…>
  local s=$1 c=$2; shift 2; local d=$O/$s; mkdir -p $d $O/home-$s; : > $d/$c.led; rm -f $d/$c.led.dhstart
  env -i PATH=$SH:/usr/local/bin:/usr/bin:/bin HOME=/root TZ=UTC RIG_LED_LOG=$d/$c.led RIG_STATE=$O/st-free XORGATE_LED_WRITER=pinctrl XORGATE_HOME=$O/home-$s SLOW_DH_S=0.08 \
    unshare -m --propagation private -- /rig/tools/pi-ns.sh $P "$@" led blue blink-fast > $d/$c.out 2> $d/$c.err & local pid=$!
  sleep 1.2; rm -f $d/$c.led.dhstart
  for _ in $(seq 1 400); do [ -e $d/$c.led.dhstart ] && break; sleep 0.002; done
  sleep 0.01
  date +%s%3N > $d/$c.sig; kill -INT $pid; wait $pid; echo $? > $d/$c.code; sleep 0.3; rm -f $d/$c.led.dhstart
}
verdict() { # <side> <case> <want blanks or ->
  local d=$O/$1 c=$2 want=$3 g last n
  g=$(cat $d/$c.sig); last=$(tail -1 $d/$c.led | cut -d' ' -f2-)
  n=$(awk -v g="$g" '$1 >= g && $0 ~ / set 12,21,16 op dl$/' $d/$c.led | wc -l | tr -d ' ')
  local ok=1; [ "$last" = "set 12,21,16 op dl" ] || ok=0; [ "$want" = - ] || [ "$n" = "$want" ] || ok=0
  [ "$(cat $d/$c.code)" = 0 ] || ok=0
  echo "$1 $c last=[$last] blanks-after-SIGINT=$n code=$(cat $d/$c.code) $([ $ok = 1 ] && echo PASS || echo FAIL)"
  [ $ok = 1 ]
}
fails=0
for i in 1 2 3 4 5; do
  sig js a$i node /rig/cache/js-89b5c64/dist/cli.js
  sig rs a$i /rig/cache/bin/xorgate-rs
  verdict js a$i - || fails=$((fails + 1))
  verdict rs a$i 2 || fails=$((fails + 1))
done
echo "== slow-child: $fails FAIL"
[ $fails = 0 ]
