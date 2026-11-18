#!/usr/bin/env bash
# shellcheck disable=SC2015  # test assertions: `a && ok || bad`
# Phase 8 WP-R1 DA7: a NATIVE provision (scripts/provision-rs.sh through
# I-P8-1, tools/provision-e2e harness) under a REAL systemd 252 as PID 1:
#   - xorgate-agent-rs enabled + active; Type=notify really waits for READY=1
#     from the main pid; WatchdogSec; User=pi from 10-user.conf;
#   - Conflicts=/After=xorgate-agent.service inert (no such unit);
#   - 20-native.conf: the start is skipped while /opt/xorgate exists, and the
#     boot generator stops wanting the unit (daemon-reload = the next boot);
#   - xorgate-updater-rs static (never enabled), 10-dist.conf effective, the
#     024 grant lets pi start exactly that unit and nothing else; visudo -c;
#   - the cloud update path (pi: sudo -n systemctl start --no-block
#     xorgate-updater-rs.service) runs the REAL `xorgate-rs update` (the
#     x86_64 build `hl.sh run <tag> build-bins` leaves at
#     tools/replay/.cache/bin/xorgate-rs; SKIPPED, and said so in the result
#     line, when absent) and REWRITES NOTHING: every file of the surface keeps
#     its bytes, mode and owner; the modem daemon is not restarted;
#   - v0.1.1: scripts/tests/lte-routes.sh --kernel, the LTE default route and
#     P1b on a REAL kernel FIB in a private network namespace (this container
#     is privileged; its own network is never touched).
# Run (systemd must be PID 1; ~3 min), as hl.sh `systemd` does:
#   docker run -d --name xg-prsd --label xorgate.agent=<tag> --privileged --cgroupns=private \
#     --tmpfs /run --tmpfs /run/lock -v "$PWD:/src:ro" debian:bookworm \
#     bash -c 'apt-get update -qq && apt-get install -y -qq systemd python3 sudo jq curl openssl ca-certificates libgstreamer1.0-0 iproute2 >/dev/null && exec /lib/systemd/systemd'
#   docker exec xg-prsd bash /src/scripts/tests/provision-rs-systemd.sh
#   docker rm -f xg-prsd
set -uo pipefail
# shellcheck source=tools/provision-e2e/lib.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/tools/provision-e2e/lib.sh"
for _ in $(seq 60); do s=$(systemctl is-system-running 2>/dev/null); case $s in running | degraded) break ;; esac; sleep 1; done
e2e_setup
U=xorgate-agent-rs
sp() { systemctl show -p "$1" --value "$2"; }
GEN_LINK=/run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service
wanted() { [ -L "$GEN_LINK" ] && case $(systemctl is-enabled "$U") in generated | enabled-runtime) true ;; *) false ;; esac; }

echo "== native provision (I-P8-1), the agent's READY delayed 3 s"
reset_device; echo 3 > "$E/ready-delay"
cp "$BUCKET/$TGZ" "$W/$TGZ"; cp "$BUCKET/$TGZ.sha256" "$W/"; tar -xzf "$W/$TGZ" -C "$W" provision-rs.sh
run_prov "$W/provision-rs.sh"
expect_rc 0 "native provision"
wanted && [ "$(systemctl is-active $U)" = active ] && ok "$U wanted by the native generator ($(systemctl is-enabled $U)) + active" || bad "$U $(systemctl is-enabled $U)/$(systemctl is-active $U)"
[ "$(sp Type $U)/$(sp NotifyAccess $U)/$(sp WatchdogUSec $U)" = notify/main/1min ] && ok "Type=notify, NotifyAccess=main, WatchdogSec=60" || bad "unit: $(sp Type $U)/$(sp NotifyAccess $U)/$(sp WatchdogUSec $U)"
t0=$(sp ExecMainStartTimestampMonotonic $U); t1=$(sp ActiveEnterTimestampMonotonic $U)
[ $((t1 - t0)) -ge 2900000 ] && ok "systemd held the unit in 'activating' until READY=1 ($(((t1 - t0) / 1000)) ms)" || bad "active after only $(((t1 - t0) / 1000)) ms: READY not awaited?"
sleep 5; [ "$(sp NRestarts $U)" = 0 ] && [ "$(systemctl is-active $U)" = active ] && ok "no restart (the watchdog is fed)" || bad "NRestarts $(sp NRestarts $U)"
[ "$(sp User $U)" = pi ] && [ "$(stat -c %U "/proc/$(sp MainPID $U)")" = pi ] && ok "runs as pi (10-user.conf)" || bad "user $(sp User $U)"
case $(sp DropInPaths $U) in *10-user.conf*20-native.conf*) ok "drop-ins 10-user.conf + 20-native.conf loaded" ;; *) bad "DropInPaths: $(sp DropInPaths $U)" ;; esac
[ "$(sp LoadState xorgate-agent)" = not-found ] && case "$(sp Conflicts $U) $(sp After $U)" in *xorgate-agent.service*) true ;; *) false ;; esac \
  && ok "Conflicts=/After=xorgate-agent.service present and inert (no such unit)" || bad "JS unit state $(sp LoadState xorgate-agent)"

(cd / && sh /etc/systemd/system-generators/xorgate-agent-rs-native) && [ ! -e /multi-user.target.wants ] && ok "the generator run by hand with no output dir does nothing (N3)" || { bad "the generator wrote outside a generator dir"; rm -rf /multi-user.target.wants; }

echo "== 20-native.conf"
mkdir /opt/xorgate; systemctl restart $U 2>/dev/null; sleep 1
[ "$(systemctl is-active $U)" = inactive ] && [ "$(sp ConditionResult $U)" = no ] && ok "with /opt/xorgate present the start is skipped (ConditionPathExists=!/opt/xorgate)" || bad "with /opt/xorgate: $(systemctl is-active $U), condition $(sp ConditionResult $U)"
systemctl daemon-reload
! wanted && [ ! -e "$GEN_LINK" ] \
  && ok "…and after a daemon-reload (= the next boot) the generator no longer wants it ($(systemctl is-enabled $U))" || bad "generator still wants $U with /opt/xorgate present"
rmdir /opt/xorgate; systemctl daemon-reload
wanted && ok "the generator wants it again once /opt/xorgate is gone" || bad "generator: $(systemctl is-enabled $U)"
systemctl start $U; active_wait $U active 15
[ "$(systemctl is-active $U)" = active ] && [ "$(sp ConditionResult $U)" = yes ] && ok "…and starts again once it is gone" || bad "restart after rmdir"

echo "== the update rail's units and grant"
UPD=xorgate-updater-rs
[ "$(systemctl is-enabled $UPD)" = static ] && [ "$(systemctl is-active $UPD)" = inactive ] && ok "$UPD static (no [Install]) and not running" || bad "$UPD $(systemctl is-enabled $UPD)/$(systemctl is-active $UPD)"
case " $(sp Environment $UPD) " in *" DEVICE_DIST_VERSION=latest "*) ok "10-dist.conf effective (DEVICE_DIST_VERSION=latest)" ;; *) bad "updater env: $(sp Environment $UPD)" ;; esac
visudo -c >/dev/null 2>&1 && ok "visudo -c" || bad "visudo -c"
sudo -u pi sudo -n -l /usr/bin/systemctl start --no-block xorgate-updater-rs.service >/dev/null 2>&1 && ok "pi may start the updater (024)" || bad "024 grant missing"
sudo -u pi sudo -n -l /usr/bin/systemctl start xorgate-agent-rs.service >/dev/null 2>&1 && bad "pi may start other units" || ok "…and nothing else"

echo "== the first cloud update rewrites nothing (the REAL xorgate-rs update)"
REAL=$SRC/tools/replay/.cache/bin/xorgate-rs
UPD_NOTE=""
if [ -x "$REAL" ] && [ "$(head -c 4 "$REAL" | od -An -c | tr -d ' ')" = 177ELF ]; then
  install -m 0755 "$REAL" "$E/real-xorgate-rs"
  echo "    real binary $(sha256sum "$REAL" | cut -c1-16) ($(sed -n 's/^tree_fp=//p' "$SRC/tools/replay/.cache/bin/BUILT_FROM" 2>/dev/null | cut -c1-12))"
  surface() { snap --no-inode | grep -v ' /var/lib/xorgate'; }
  B=$(surface); APID=$(sp MainPID $U); MPID=$(sp MainPID sim7600-lte)
  sudo -u pi sudo -n systemctl start --no-block xorgate-updater-rs.service; sleep 1
  for _ in $(seq 120); do [ "$(systemctl is-active $UPD)" = activating ] || break; sleep 1; done
  journalctl -u $UPD --no-pager -o cat > "$W/upd.log"
  [ "$(sp Result $UPD)" = success ] && grep -q "updated to 'latest'" "$W/upd.log" && ok "the updater ran xorgate-rs update to completion" || { bad "updater: $(sp Result $UPD)"; sed 's/^/    | /' "$W/upd.log" | tail -15; }
  grep -q 'checksum ok' "$W/upd.log" && grep -q 'modem assets refreshed' "$W/upd.log" && ! grep -q 'coexistence' "$W/upd.log" \
    && ok "Rust-only branch: checksum ok, modem assets refreshed in place (no /opt/xorgate)" || bad "update branch: $(tr '\n' ' ' < "$W/upd.log")"
  A=$(surface)
  [ "$A" = "$B" ] && ok "every file keeps its bytes, mode and owner ($(echo "$B" | wc -l) paths)" || { bad "the update changed files"; diff <(echo "$B") <(echo "$A") | head -20; }
  active_wait $U active 30
  [ "$(sp MainPID $U)" != "$APID" ] && [ "$(systemctl is-active $U)" = active ] && ok "the agent was restarted on the swapped build" || bad "agent restart ($APID -> $(sp MainPID $U))"
  [ "$(sp MainPID sim7600-lte)" = "$MPID" ] && ok "the modem daemon was not restarted" || bad "the modem daemon restarted"
  [ -f /etc/systemd/system/$U.service.d/20-native.conf ] && [ -f /etc/systemd/system/$UPD.service.d/10-dist.conf ] \
    && ok "20-native.conf + 10-dist.conf survive the update" || bad "a drop-in is gone"
else
  echo "SKIP: no real x86_64 xorgate-rs at tools/replay/.cache/bin (run 'hl.sh run <tag> build-bins' first): the update rows did not run"
  UPD_NOTE=" (update rows SKIPPED: no real binary)"
fi

reset_device

echo "== v0.1.1: the LTE default route + P1b on a real kernel FIB (scripts/tests/lte-routes.sh --kernel, private netns)"
LR=$(bash "$SRC/scripts/tests/lte-routes.sh" --kernel 2>&1); lrc=$?
printf '%s\n' "$LR" | grep -E '^(PASS|FAIL|SKIP)' | sed 's/^/    | /'
[ "$lrc" = 0 ] && ok "lte-routes.sh --kernel: $(printf '%s\n' "$LR" | grep -c '^PASS') rows pass, kernel rows ran" || bad "lte-routes.sh --kernel (rc $lrc): $(printf '%s\n' "$LR" | grep -E '^(FAIL|SKIP)' | head -3 | tr '\n' '|')"

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)$UPD_NOTE"
exit $fail
