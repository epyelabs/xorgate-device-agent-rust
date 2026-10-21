#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015,SC2012,SC2251  # test assertions: `[ … ]; expect $?` and `a && pass || failc`
# Phase 4 (WP6) bench additions, run as ROOT in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/p4-profile.sh
# The shared profile's LED/SMS knobs + pinctrl/sudo shims (plan
# phase-4-operations.md §9, Q6), the env-file guard, the exclusive UnsetEnvironment,
# the bench-flavour guard on Rust restarts (an `xorgate-rs update` reinstalls the
# shipped unit with Conflicts=xorgate-agent.service), the updater + 024 bootstrap
# of install.sh, and ntp-unsync.sh's window precondition.
set -uo pipefail
# shellcheck source=scripts/bench/tests/_stubs.sh
source "$(dirname "$0")/_stubs.sh"
cat > "$STUB/visudo" <<'S'
#!/usr/bin/env bash
# -cf <file>: "parse" = refuse a file containing BAD; -c: always clean.
echo "visudo $*" >> "$SYSD_STATE/visudo.log"
if [ "${1:-}" = -cf ]; then ! grep -q BAD "$2"; else echo "/etc/sudoers: parsed OK"; fi
S
cat > "$STUB/timedatectl" <<'S'
#!/usr/bin/env bash
# set-ntp X → $SYSD_STATE/ntp-on; `show -p NTP` reads it (default yes).
echo "timedatectl $*" >> "$SYSD_STATE/timedatectl.log"
case "$*" in
  "set-ntp "*) echo "${2}" | sed 's/true/yes/;s/false/no/' > "$SYSD_STATE/ntp-on" ;;
  *NTPSynchronized*) cat "$SYSD_STATE/ntp" 2>/dev/null || echo yes ;;
  *"-p NTP "*|*"-p NTP") cat "$SYSD_STATE/ntp-on" 2>/dev/null || echo yes ;;
esac
S
# date: `-s` is logged, never applied (the container clock is not ours).
cat > "$STUB/date" <<'S'
#!/usr/bin/env bash
for a in "$@"; do [ "$a" = -s ] && { echo "date $*" >> "$SYSD_STATE/date.log"; exit 0; }; done
exec /bin/date "$@"
S
chmod +x "$STUB/visudo" "$STUB/timedatectl" "$STUB/date"
# A stand-in for the REAL sudo the shim execs for every other command.
REAL_SUDO_ADDED=0
if [ ! -e /usr/bin/sudo ]; then printf '#!/bin/sh\necho "realsudo $*"\n' > /usr/bin/sudo; chmod +x /usr/bin/sudo; REAL_SUDO_ADDED=1; fi
D=/etc/systemd/system/xorgate-agent-rs.service.d/95-inputs.conf
X=/run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf
SH=/var/lib/xorgate-rs/shared-shims

fixture_bench; reset_units
bash "$B/dev-manifests.sh" >"$LOGS/dm.log" 2>&1 || { failc "dev-manifests.sh"; tail -5 "$LOGS/dm.log"; }

echo "== shared profile: LED/SMS knobs + pinctrl/sudo shims"
bash "$B/profile.sh" shared >"$LOGS/s1.log" 2>&1; rc=$?
expect $((rc != 0)) "profile.sh shared rc=0"
grep -qx 'Environment=XORGATE_LED_GPIOCHIP=/run/xorgate-rs-no-gpio' "$D"; expect $? "95-inputs.conf: XORGATE_LED_GPIOCHIP=/run/xorgate-rs-no-gpio"
grep -qx 'Environment=XORGATE_SMS_PORT=/run/xorgate-rs-no-sms' "$D"; expect $? "95-inputs.conf: XORGATE_SMS_PORT=/run/xorgate-rs-no-sms"
! grep -q 'XORGATE_LED_WRITER' "$D"; expect $? "95-inputs.conf: no XORGATE_LED_WRITER (default gpiocdev, inert by the chip knob)"
out=$("$SH/pinctrl" set 12,16,21 op dl 2>&1 >/dev/null); rc=$?
[ "$rc" = 1 ] && grep -q "disabled in the shared profile" <<<"$out" && [ "$(stat -c '%U %a' "$SH/pinctrl")" = "root 755" ]
expect $? "shim pinctrl: exit 1, 'disabled in the shared profile', root 0755"
for argv in "-n /usr/local/bin/xorgate-setclock 1760000000" "-n /usr/local/bin/xorgate-camrotate imx708 cam0 180" "xorgate-setclock 1"; do
  # shellcheck disable=SC2086
  out=$("$SH/sudo" $argv 2>&1); rc=$?
  [ "$rc" = 1 ] && grep -q "disabled in the shared profile" <<<"$out" && ! grep -q realsudo <<<"$out"
  expect $? "shim sudo refuses '$argv' before anything runs"
done
out=$("$SH/sudo" -n systemctl start --no-block xorgate-updater-rs.service 2>&1); rc=$?
[ "$rc" = 0 ] && [ "$out" = "realsudo -n systemctl start --no-block xorgate-updater-rs.service" ]
expect $? "shim sudo execs the real sudo for every other argv (argv unchanged)"
[ "$(stat -c '%U %a' "$SH/sudo")" = "root 755" ]; expect $? "shim sudo root 0755"
bash "$B/exclusive.sh" status >"$LOGS/s2.log" 2>&1
grep -q "inputs: SAFE" "$LOGS/s2.log"; expect $? "inputs_safe passes with the Phase 4 knobs"
bash "$B/profile.sh" shared >"$LOGS/s3.log" 2>&1
grep -q "== done: 0 change(s)" "$LOGS/s3.log"; expect $? "profile.sh shared idempotent (0 changes)"

echo "== inputs_safe catches each missing Phase 4 piece"
cp "$D" "$LOGS/d.bak"
sed -i '/XORGATE_LED_GPIOCHIP/d' "$D"
bash "$B/exclusive.sh" status >"$LOGS/u1.log" 2>&1
grep -q "missing: XORGATE_LED_GPIOCHIP" "$LOGS/u1.log" && grep -q "inputs: NOT SAFE" "$LOGS/u1.log"; expect $? "NOT SAFE without the LED chip knob"
cp "$LOGS/d.bak" "$D"; sed -i '/XORGATE_SMS_PORT/d' "$D"
bash "$B/exclusive.sh" status >"$LOGS/u2.log" 2>&1
grep -q "missing: XORGATE_SMS_PORT" "$LOGS/u2.log"; expect $? "NOT SAFE without the SMS port knob"
cp "$LOGS/d.bak" "$D"
mkdir -p /run/xorgate-rs-no-sms
bash "$B/exclusive.sh" status >"$LOGS/u3.log" 2>&1
grep -q "UNSAFE: /run/xorgate-rs-no-sms exists" "$LOGS/u3.log"; expect $? "NOT SAFE when the no-sms path exists"
rmdir /run/xorgate-rs-no-sms
printf '#!/bin/sh\nexec /usr/bin/sudo "$@"\n' > "$SH/sudo"
bash "$B/exclusive.sh" status >"$LOGS/u4.log" 2>&1
grep -q "UNSAFE: $SH/sudo differs" "$LOGS/u4.log"; expect $? "NOT SAFE on a sudo shim that does not refuse (checked by content, never run)"
! grep -q "realsudo" "$LOGS/u4.log"; expect $? "inputs_safe never executed the sudo shim"
bash "$B/profile.sh" shared >"$LOGS/u5.log" 2>&1
bash "$B/exclusive.sh" status >"$LOGS/u6.log" 2>&1
grep -q "inputs: SAFE" "$LOGS/u6.log"; expect $? "profile.sh shared repaired the sudo shim"

echo "== env-file guard: the Phase 4 knobs never live in agent-rs-dev.env"
for bad in "XORGATE_LED_GPIOCHIP=/dev/gpiochip0" "XORGATE_LED_WRITER=pinctrl" "XORGATE_SMS_PORT=/dev/ttyUSB3"; do
  cp "$RS_ENV" "$LOGS/env.bak"; echo "$bad" >> "$RS_ENV"
  bash "$B/profile.sh" shared >"$LOGS/g.log" 2>&1; rc=$?
  [ "$rc" = 1 ] && grep -q "REFUSING: $RS_ENV sets ${bad%%=*}" "$LOGS/g.log"
  expect $? "profile.sh shared refuses $RS_ENV carrying ${bad%%=*}"
  bash "$B/exclusive.sh" status >"$LOGS/g2.log" 2>&1
  grep -q "inputs: NOT SAFE" "$LOGS/g2.log"; expect $? "inputs_safe NOT SAFE with ${bad%%=*} in the env file"
  cp "$LOGS/env.bak" "$RS_ENV"
done

echo "== exclusive drop-in drops the knobs"
reset_units; echo inactive > "$STATE/xorgate-agent"
bash "$B/profile.sh" exclusive --no-restart >"$LOGS/x.log" 2>&1
grep -qx 'UnsetEnvironment=XORGATE_LED_GPIOCHIP XORGATE_SMS_PORT XORGATE_LED_WRITER' "$X"; expect $? "99-exclusive.conf: UnsetEnvironment of the three knobs"
grep -qx 'Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' "$X"; expect $? "99-exclusive.conf: full PATH (real pinctrl + sudo)"
bash "$B/profile.sh" shared --no-restart >"$LOGS/x2.log" 2>&1; reset_units

echo "== bench-flavour guard on Rust restarts (E11 aftermath)"
cp /etc/systemd/system/xorgate-agent-rs.service "$LOGS/unit.bak"
printf '[Unit]\nConflicts=xorgate-agent.service\n' > /etc/systemd/system/xorgate-agent-rs.service
: > "$STATE/systemctl.log"
bash "$B/profile.sh" shared >"$LOGS/f1.log" 2>&1; rc=$?
[ "$rc" != 0 ] && grep -q "REFUSING to restart xorgate-agent-rs: bench flavour lost" "$LOGS/f1.log" && ! grep -q "restart xorgate-agent-rs" "$STATE/systemctl.log"
expect $? "profile.sh refuses the Rust restart while the unit carries Conflicts=xorgate-agent.service"
echo inactive > "$STATE/xorgate-agent"; : > "$STATE/systemctl.log"
bash "$B/exclusive.sh" end >"$LOGS/f2.log" 2>&1
[ "$(js_state)" = active ] && ! grep -q "restart xorgate-agent-rs" "$STATE/systemctl.log" && grep -q "not restarted (see above)" "$LOGS/f2.log"
expect $? "exclusive.sh end: JS back, Rust NOT restarted with the shipped unit, audits continue"
cp "$LOGS/unit.bak" /etc/systemd/system/xorgate-agent-rs.service
# shellcheck disable=SC2016  # the shipped wrapper's text, verbatim
printf '#!/usr/bin/env bash\nexec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" /opt/xorgate-rs/bin/xorgate-rs "$@"\n' > /usr/local/bin/xorgate-rs
bash "$B/profile.sh" shared >"$LOGS/f3.log" 2>&1; rc=$?
[ "$rc" != 0 ] && grep -q "is not the bench wrapper" "$LOGS/f3.log"; expect $? "refuses the restart with the shipped wrapper (XORGATE_HOME=/var/lib/xorgate)"
rm -f /usr/local/bin/xorgate-rs
bash "$B/profile.sh" shared >"$LOGS/f4.log" 2>&1; rc=$?
expect $((rc != 0)) "bench flavour restored: profile.sh shared restarts again"

echo "== install.sh 3b: updater unit + 024 bootstrap"
R=$(mktemp -d); mkdir -p "$R/systemd"
printf '[Unit]\nDescription=Xorgate agent self-update (Rust)\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/xorgate-rs update\nTimeoutStartSec=20min\n' > "$R/systemd/xorgate-updater-rs.service"
rm -f /etc/systemd/system/xorgate-updater-rs.service /etc/sudoers.d/024_xorgate-update-rs; mkdir -p /etc/sudoers.d
# shellcheck source=scripts/bench/_lib.sh
boot() { ( set +e; source "$B/_lib.sh"; eval "$(bench_remote_lib)"; set +e; p4_updater_bootstrap "$@" ); }
boot "$R" pi >"$LOGS/i1.log" 2>&1
cmp -s "$R/systemd/xorgate-updater-rs.service" /etc/systemd/system/xorgate-updater-rs.service && [ "$(stat -c %a /etc/systemd/system/xorgate-updater-rs.service)" = 644 ]
expect $? "updater unit installed from the tarball, 0644"
[ "$(stat -c %a /etc/sudoers.d/024_xorgate-update-rs)" = 440 ] \
  && [ "$(tail -n1 /etc/sudoers.d/024_xorgate-update-rs)" = "pi ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater-rs.service" ]
expect $? "024 grant: 0440, exactly 'pi ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater-rs.service'"
grep -q "visudo -cf .*024_xorgate-update-rs" "$STATE/visudo.log"; expect $? "024 validated with visudo -cf before it landed"
! grep -q 'xorgate-updater.service' /etc/sudoers.d/024_xorgate-update-rs; expect $? "024 never names the JS updater unit"
boot "$R" pi >"$LOGS/i2.log" 2>&1
grep -q "= xorgate-updater-rs.service unchanged" "$LOGS/i2.log" && grep -q "= /etc/sudoers.d/024_xorgate-update-rs unchanged" "$LOGS/i2.log"; expect $? "bootstrap idempotent"
rm -f /etc/sudoers.d/024_xorgate-update-rs
boot "$R" root >"$LOGS/i3.log" 2>&1
[ ! -e /etc/sudoers.d/024_xorgate-update-rs ]; expect $? "root service user: no grant (TS rule)"
rm -f "$R/systemd/xorgate-updater-rs.service" /etc/systemd/system/xorgate-updater-rs.service
boot "$R" pi >"$LOGS/i4.log" 2>&1
grep -q "no systemd/xorgate-updater-rs.service (a pre-Phase-4 build)" "$LOGS/i4.log" && [ ! -e /etc/sudoers.d/024_xorgate-update-rs ]
expect $? "pre-Phase-4 tarball: nothing bootstrapped, a NOTE"

echo "== ntp-unsync.sh: exclusive window only, bounded skew"
reset_units; : > "$STATE/timedatectl.log"
bash "$B/ntp-unsync.sh" break >"$LOGS/n1.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q "REFUSING: break needs an open exclusive window" "$LOGS/n1.log" && ! grep -q set-ntp "$STATE/timedatectl.log"
expect $? "break refuses without a window (nothing run)"
mkdir -p "$(dirname "$X")"; : > "$X"
bash "$B/ntp-unsync.sh" break >"$LOGS/n2.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q "xorgate-agent is active" "$LOGS/n2.log"; expect $? "break refuses while the JS agent is active"
echo inactive > "$STATE/xorgate-agent"
bash "$B/ntp-unsync.sh" break --dry-run >"$LOGS/n3.log" 2>&1; rc=$?
[ "$rc" = 0 ] && grep -q "would: timedatectl set-ntp false" "$LOGS/n3.log" && grep -q "would: date -u -s @" "$LOGS/n3.log" \
  && grep -q "would: systemctl stop xorgate-agent-rs" "$LOGS/n3.log" && ! grep -q set-ntp "$STATE/timedatectl.log"
expect $? "break --dry-run in a window: stop rs → set-ntp false → date -s, nothing run"
bash "$B/ntp-unsync.sh" break --secs 5000 >"$LOGS/n4.log" 2>&1; rc=$?
expect $((rc != 2)) "--secs over 900 refused (rc=2)"

echo "== ntp-unsync break records once; end / the dead-man restore NTP (review-wp6 M4)"
ST=/var/lib/xorgate-rs/ntp-unsync.state
rm -f "$ST" "$STATE/date.log" "$STATE/ntp-on"; : > "$STATE/timedatectl.log"
echo inactive > "$STATE/xorgate-agent"; mkdir -p "$(dirname "$X")"; : > "$X"
bash "$B/ntp-unsync.sh" break >"$LOGS/m1.log" 2>&1; rc=$?
[ "$rc" = 0 ] && [ -f "$ST" ] && grep -q '^secs=600$' "$ST" && [ "$(grep -c -- ' -s @' "$STATE/date.log")" = 1 ] && grep -q 'set-ntp false' "$STATE/timedatectl.log"
expect $? "break: set-ntp false, ONE date -s, the original offset recorded"
cp "$ST" "$LOGS/st.bak"
bash "$B/ntp-unsync.sh" break >"$LOGS/m2.log" 2>&1; rc=$?
[ "$rc" = 0 ] && [ "$(grep -c -- ' -s @' "$STATE/date.log")" = 1 ] && grep -q "already broken" "$LOGS/m2.log" && cmp -s "$ST" "$LOGS/st.bak"
expect $? "a second break never moves the clock again (record unchanged)"
rm -f "$ST"; echo no > "$STATE/ntp-on"
bash "$B/ntp-unsync.sh" break >"$LOGS/m3.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q "NTP is already off" "$LOGS/m3.log"; expect $? "break refuses when NTP is off without our record"
cp "$LOGS/st.bak" "$ST"; : > "$STATE/date.log"; : > "$STATE/timedatectl.log"
# The record as if the break just happened: wall = now, uptime = now.
printf 'secs=600\nwall=%s\nuptime=%s\n' "$(/bin/date +%s)" "$(cut -d' ' -f1 /proc/uptime | cut -d. -f1)" > "$ST"
bash "$B/exclusive.sh" end >"$LOGS/m4.log" 2>&1
grep -q 'set-ntp true' "$STATE/timedatectl.log" && [ "$(grep -c -- ' -s @' "$STATE/date.log")" = 1 ] && [ ! -e "$ST" ] && [ "$(js_state)" = active ]
expect $? "exclusive.sh end: set-ntp true, undoes the untouched break (+600 s), record gone, JS back"
: > "$STATE/date.log"; : > "$STATE/timedatectl.log"
# A clock stepped since the break (the stepper's E8 correction): wall moved 600 s more than uptime.
printf 'secs=600\nwall=%s\nuptime=%s\n' "$(( $(/bin/date +%s) - 600 ))" "$(cut -d' ' -f1 /proc/uptime | cut -d. -f1)" > "$ST"
bash "$B/ntp-unsync.sh" restore --dry-run >/dev/null 2>&1
bash "$B/exclusive.sh" end >"$LOGS/m5.log" 2>&1
grep -q 'set-ntp true' "$STATE/timedatectl.log" && [ ! -s "$STATE/date.log" ] && grep -q "a step corrected it" "$LOGS/m5.log"
expect $? "end after a stepper correction: NTP on, the clock NOT moved"
: > "$STATE/timedatectl.log"
bash "$B/exclusive.sh" end >"$LOGS/m6.log" 2>&1
grep -q 'set-ntp true' "$STATE/timedatectl.log" && grep -q "no ntp-unsync record" "$LOGS/m6.log"
expect $? "end without a record: still set-ntp true (idempotent)"
# The dead-man: arm via begin, then fire its script.
reset_units; : > "$STATE/date.log"; : > "$STATE/timedatectl.log"; rm -f "$X"
bash "$B/exclusive.sh" begin --deadman-min 95 >"$LOGS/m7.log" 2>&1
printf 'secs=600\nwall=%s\nuptime=%s\n' "$(/bin/date +%s)" "$(cut -d' ' -f1 /proc/uptime | cut -d. -f1)" > "$ST"
bash /run/xorgate-rs-window-deadman.sh >"$LOGS/m8.log" 2>&1
grep -q 'set-ntp true' "$STATE/timedatectl.log" && [ "$(grep -c -- ' -s @' "$STATE/date.log")" = 1 ] && [ ! -e "$ST" ] && [ "$(js_state)" = active ]
expect $? "dead-man fire: set-ntp true + undo the break, then JS back"
bash "$B/exclusive.sh" end >/dev/null 2>&1
rm -f "$X"
[ "$REAL_SUDO_ADDED" = 1 ] && rm -f /usr/bin/sudo
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
