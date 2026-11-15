#!/usr/bin/env bash
# p8-restore.sh: put 58b51737 back exactly after a Phase 8 bench window
# (plan §7.3.6, DA15; PROGRESS F1/F3). Root, on the bench. IDEMPOTENT: run it
# again at any point and it finishes what is left. Shares `flock /run/p8.lock`
# with the dead-man units (which run it with --auto / --auto --boot).
#
#   p8-restore.sh [--window /root/p8-<ts>] [--label w<n>] [--auto] [--boot] [--force-start] [--again]
#     --window   default: the armed window in /root/p8-window.active
#     --label    the test dir name W/test-<label>/ (default: the marker's label)
#     --auto     unattended (the dead-man): start PROD even when the pre-start
#                verify reports differences, as long as /etc/xorgate came back
#     --boot     the boot oneshot (F1): ordered Before= both agents, so the
#                final start is --no-block and nothing waits on it
#     --force-start  manual: start PROD although the pre-start verify FAILED
#     (a manual restore that does not start PROD re-arms a 10-min --auto
#     FAILSAFE dead-man first: review T2; P8_FAILSAFE_MIN overrides)
#     --again    re-run a FINISHED restore (refused while PROD runs). Without it,
#                a finished restore (W/restore.done) is a no-op: a late dead-man
#                firing must never re-extract over the live PROD agent's state
#
# Steps (each skipped when already done):
#   0. flock /run/p8.lock (<= 15 min)
#   1. F1: cancel the queued jobs of every agent/updater unit
#   2. F3: wait (<= 120 s) for the dpkg/apt locks, THEN stop p8-install-*, then
#      dpkg --audit -> dpkg --configure -a -> apt-get -f install --no-upgrade
#   3. stop the test agent/updater units
#   4. the test occupants of every surface path (moved or absent at aside) ->
#      W/test-<label>/ (DEV keys stay there); /opt/xorgate-rs.{new,old} swept
#   5. W/aside/* back with guarded mv -T (anything that appeared in the gap is
#      test state: W/test-<label>/late/)
#   6. against pre.manifest: changed/missing system rows re-extracted from the
#      tar; new rows -> W/test-<label>/extra/; pi groups + unit enablement fixed
#   7. H1 conf, dead-man units, marker and timer stamp removed; daemon-reload;
#      udev rules reload; NetworkManager reload ONLY if an NM file changed
#   8. p8-verify.sh pre (manifest == pre.manifest minus volatile; no leftovers;
#      the certified binary) BEFORE the start
#   9. start xorgate-agent-rs --no-block; unless --boot, wait <= 90 s for active
#  10. --auto without --boot: stop the modem sampler, run p8-verify.sh post
# Exit: 0 restored + verify PASS; 1 otherwise (the log says what is left).
# Never deletes config-sync.json, a backup, or anything in W.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/bench/p8-lib.sh
. "$HERE/p8-lib.sh"

W=""; LABEL=""; AUTO=0; BOOT=0; FORCE=0; AGAIN=0; ORIG_ARGS=("$@")
while [ $# -gt 0 ]; do
  case $1 in
    --window) W=$2; shift 2 ;;
    --label) LABEL=$2; shift 2 ;;
    --auto) AUTO=1; shift ;;
    --boot) BOOT=1; AUTO=1; shift ;;
    --force-start) FORCE=1; shift ;;
    --again) AGAIN=1; shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
p8_need_root
trap '' PIPE      # a closed caller pipe must never kill a restore (P1)
# ORCHESTRATOR RULING (T2 residual): a manual restore ALWAYS runs as a
# transient unit, so an ssh drop (pipe close, SIGHUP, pty hang-up) cannot kill
# it. This client only starts it, waits, and prints a short status. Sandbox
# tests (P8_ROOT) run inline.
if [ -z "${P8_SELF_UNIT:-}" ] && [ -z "$P8_ROOT" ]; then
  ww=$W; [ -n "$ww" ] || ww=$(p8_marker_get window || true)
  [ -n "$ww" ] || { echo "restore: no --window and no armed marker: nothing to restore"; exit 0; }
  ts=${ww##*/p8-}
  [[ $ts =~ ^[0-9A-Za-z]+$ ]] || p8_die "bad window $ww"
  u=p8-restore-$ts
  envs=(--setenv=HOME=/root --setenv="P8_SELF_UNIT=$u.service")
  for v in P8_FAILSAFE_MIN P8_DPKG_WAIT_S P8_START_WAIT_S P8_UNIT_STOP_WAIT_S P8_CERT_BIN_SHA P8_CERT_VERSION; do
    [ -z "${!v:-}" ] || envs+=(--setenv="$v=${!v}")
  done
  # Fix-check ab4420d: NOT --wait (a Ctrl-C on a --wait client stops the
  # unit). The unit is started detached; this client only polls it and tails
  # the log, so interrupting or losing the client never touches the restore.
  rm -f "$ww/restore.rc"
  n0=$(wc -l < "$ww/restore.log" 2>/dev/null || echo 0)
  systemd-run --quiet --no-block --collect --unit="$u" "${envs[@]}" /bin/bash "$HERE/p8-restore.sh" "${ORIG_ARGS[@]}" \
    || p8_die "could not start the restore unit $u (one already running?)"
  { echo "restore: running as the transient unit $u (log $ww/restore.log); Ctrl-C here does NOT stop it"; } 2>/dev/null || true
  for _ in $(seq 3600); do
    st=$(systemctl is-active "$u" 2>/dev/null || true)
    n1=$(wc -l < "$ww/restore.log" 2>/dev/null || echo 0)
    if [ "$n1" -gt "$n0" ]; then { sed -n "$((n0 + 1)),${n1}p" "$ww/restore.log"; } 2>/dev/null || true; n0=$n1; fi
    case $st in active | activating | deactivating | reloading) sleep 1 ;; *) [ -e "$ww/restore.rc" ] && break; sleep 1 ;; esac
  done
  rrc=$(cat "$ww/restore.rc" 2>/dev/null || echo 1)
  { echo "restore: unit $u exit $rrc"; } 2>/dev/null || true
  exit "$rrc"
fi
mkdir -p "$(dirname "$P8_LOCK")"
exec 9>>"$P8_LOCK"
flock -w 900 9 || p8_die "could not take $P8_LOCK within 15 min"

if [ -z "$W" ]; then
  W=$(p8_marker_get window || true)
  [ -n "$W" ] && W=$P8_ROOT$W
fi
if [ -z "$W" ]; then
  echo "restore: no --window and no armed marker: nothing to restore"
  exit 0
fi
p8_check_window "$W"
[ -n "$LABEL" ] || LABEL=$(p8_marker_get label || true)
[ -n "$LABEL" ] || LABEL=$(cat "$W/label" 2>/dev/null || echo w)
[[ $LABEL =~ ^[a-z0-9-]+$ ]] || p8_die "bad label $LABEL"
echo "$LABEL" > "$W/label"
export P8_LOGFILE=$W/restore.log
TD=$W/test-$LABEL
# The unit's result for its detached client (and for the record).
trap 'echo $? > "$W/restore.rc"' EXIT
if [ -f "$W/restore.done" ]; then
  if [ "$AGAIN" = 0 ]; then
    p8_log "restore already finished ($(cat "$W/restore.done")): nothing to do"
    [ ! -e "$P8_MARKER" ] || bash "$HERE/p8-deadman.sh" disarm >/dev/null
    # review T9: a no-op reports the finished restore's own verdict
    if grep -q 'verify=PASS' "$W/restore.done"; then exit 0; fi
    p8_log "the finished restore's pre-start verify was NOT PASS: not green (see $W/verify-pre.out)"; exit 1
  fi
  [ "$(p8_active "$P8_PROD_UNIT")" != active ] || p8_die "--again refused: $P8_PROD_UNIT is running (its live state would be overwritten)"
  rm -f "$W/restore.done"
fi
if [ -f "$W/aside.restored" ] && [ "$(p8_active "$P8_PROD_UNIT")" = active ] && [ "$AGAIN" = 0 ]; then
  # PROD was started by an earlier run that died before writing restore.done:
  # never re-extract over its live state. Record that run's own verdict.
  v=FAIL; grep -q '^DA15 pre: PASS' "$W/verify-pre.out" 2>/dev/null && v=PASS
  echo "$(p8_ts) verify=$v (PROD found running)" > "$W/restore.done"
  [ ! -e "$P8_MARKER" ] || bash "$HERE/p8-deadman.sh" disarm >/dev/null
  p8_log "PROD is already running on the restored surface: recorded verify=$v, nothing else done"
  [ "$v" = PASS ]; exit $?
fi
p8_log "restore start: window=${W#"$P8_ROOT"} label=$LABEL auto=$AUTO boot=$BOOT self=${P8_SELF_UNIT:-shell}"
rc=0

# 1. F1
p8_cancel_agent_jobs
# 2. F3 + MUST-2: dpkg lock first, then EVERY window unit (the provisioner
#    xorgate-provision-rs first, then p8-install-*), verified inactive, then
#    the repair. If one survives, NOTHING is moved back.
p8_stop_installers || p8_die "a window unit (xorgate-provision-rs / p8-install-*) could not be stopped: moving NOTHING back (see the log)"
date +%s.%N > "$W/units-stopped.epoch"
if [ -e "$P8_PROV_LOG" ]; then   # a window artifact: kept as evidence in the test dir, never left behind
  install -d -m 0700 "$TD/var/log"
  dst=$TD/var/log/xorgate-provision-rs.log; [ ! -e "$dst" ] || dst=$dst.$(date +%s)
  p8_mvT "$P8_PROV_LOG" "$dst"; p8_log "moved the provisioner log to ${dst#"$W/"}"
fi

# 3-5. the agent surface
L=$W/aside.list
if [ -f "$L" ] && [ ! -f "$W/aside.restored" ]; then
  for u in $P8_AGENT_UNITS; do
    if [ "$(p8_active "$u")" = active ] || [ "$(p8_active "$u")" = activating ]; then
      p8_log "stopping test unit $u"; timeout 180 systemctl stop "$u" || p8_log "stop $u failed"
    fi
  done
  install -d -m 0700 "$TD"
  # Which surface paths the window owns: moved, absent, or mid-move AND found
  # in W/aside. Anything else was never moved (a partial aside): untouched.
  owned=$(awk -F '\t' '$1 == "moved" || $1 == "absent" {print $2}' "$L")
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if [ -e "$W/aside$p" ] || [ -L "$W/aside$p" ]; then owned=$(printf '%s\n%s' "$owned" "$p"); fi
  done <<< "$(awk -F '\t' '$1 == "moving" {print $2}' "$L")"
  owned=$(printf '%s\n' "$owned" | awk 'NF && !s[$0]++')
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if { [ -e "$P8_ROOT$p" ] || [ -L "$P8_ROOT$p" ]; } && [ ! -e "$TD$p" ] && [ ! -L "$TD$p" ]; then
      p8_mvT "$P8_ROOT$p" "$TD$p"; p8_log "test state out: $p"
    fi
  done <<< "$owned"
  p8_test_stop 4
  for p in /opt/xorgate-rs.new /opt/xorgate-rs.old; do
    if [ -e "$P8_ROOT$p" ] || [ -L "$P8_ROOT$p" ]; then
      if grep -qxF "$(printf 'moved\t%s' "$p")" "$L"; then continue; fi
      p8_mvT "$P8_ROOT$p" "$TD/sweep$p"; p8_log "swept $p"
    fi
  done
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if [ -e "$W/aside$p" ] || [ -L "$W/aside$p" ]; then
      if [ -e "$P8_ROOT$p" ] || [ -L "$P8_ROOT$p" ]; then
        # Appeared since step 4 (M3: e.g. the daemon's mkdir -p): test state.
        if [ -d "$P8_ROOT$p" ] && [ ! -L "$P8_ROOT$p" ] && [ -z "$(ls -A "$P8_ROOT$p")" ]; then rmdir "$P8_ROOT$p"
        else p8_mvT "$P8_ROOT$p" "$TD/late$p"; fi
        p8_log "M3: $p reappeared in the gap: moved to test-$LABEL/late"
      fi
      p8_mvT "$W/aside$p" "$P8_ROOT$p"; p8_log "PROD back: $p"
    fi
  done <<< "$(awk -F '\t' '$1 == "moving" || $1 == "moved" {print $2}' "$L" | awk '!s[$0]++')"
  touch "$W/aside.restored"
  systemctl daemon-reload || true
  p8_test_stop 5
fi
# A test native install's boot generator went out with the surface (step 4);
# its /run output must not survive either (the bench is a CONVERTED device).
if [ ! -e "$P8_ROOT$P8_GENERATOR" ]; then
  for g in "$P8_ROOT"/run/systemd/generator "$P8_ROOT"/run/systemd/generator.early "$P8_ROOT"/run/systemd/generator.late; do
    if [ -L "$g/$P8_GENERATOR_LINK" ]; then rm -f "$g/$P8_GENERATOR_LINK"; p8_log "removed the native generator's ${g#"$P8_ROOT"}/$P8_GENERATOR_LINK"; fi
  done
  systemctl daemon-reload || true
fi

# 6. system files against pre.manifest
nm_changed=0
if [ -f "$W/pre.manifest" ] && [ -f "$W/backup.tgz" ]; then
  p8_manifest "$W/now.manifest"
  p8_mdiff "$W/pre.manifest" "$W/now.manifest" > "$W/restore.diff" || true
  if grep -q '/etc/NetworkManager/' "$W/restore.diff"; then nm_changed=1; fi
  # new rows (shortest first; a moved dir takes its children along)
  while IFS=$'\t' read -r k p; do
    [ "$k" = new ] || continue
    case $p in @*) continue ;; esac
    if [ -e "$P8_ROOT$p" ] || [ -L "$P8_ROOT$p" ]; then p8_mvT "$P8_ROOT$p" "$TD/extra$p"; p8_log "new path out: $(printf '%s' "$p" | p8_redact)"; fi
  done < <(awk -F '\t' '{ print length($2) "\t" $0 }' "$W/restore.diff" | sort -n -k1,1 | cut -f2-)
  # changed / missing real rows -> from the tar (never /var/lib/xorgate-rs: not in it)
  : > "$W/restore.list0"
  while IFS=$'\t' read -r k p; do
    case $k in changed | missing) ;; *) continue ;; esac
    case $p in
      @*) continue ;;
      /var/lib/xorgate-rs | /var/lib/xorgate-rs/*) p8_log "UNRESTORABLE ($k, not in the tar): $p"; rc=1; continue ;;
    esac
    printf '%s\0' "${p#/}" >> "$W/restore.list0"
    p8_log "from the tar ($k): $(printf '%s' "$p" | p8_redact)"
  done < "$W/restore.diff"
  if [ -s "$W/restore.list0" ]; then
    # sorted, so a missing dir is created (with its mode/owner) before its children
    tr '\0' '\n' < "$W/restore.list0" | LC_ALL=C sort | tr '\n' '\0' > "$W/restore.sorted0"
    # Review T8: extract into a stage dir, then put each entry in place by
    # rename (files/links) or mkdir + reference chmod/chown (dirs). Nothing is
    # removed first, so a power cut never leaves e.g. config.txt (vfat) missing.
    rm -rf "$W/restore-stage"; install -d -m 0700 "$W/restore-stage"
    # a crash of an earlier run may have left temp files: remove them first
    awk -F '\t' '$2 == "d" {print $1} $2 == "f" {sub(/\/[^\/]*$/, "", $1); print $1}' "$W/pre.manifest" | sort -u | while IFS= read -r dd; do
      [ -d "$P8_ROOT$dd" ] && find "$P8_ROOT$dd" -maxdepth 1 -name '.p8new-*~' -delete 2>/dev/null; true
    done
    if tar --extract --gzip --file "$W/backup.tgz" --numeric-owner --same-permissions --xattrs --acls --no-recursion \
         -C "$W/restore-stage" --null --verbatim-files-from -T "$W/restore.sorted0"; then
      while IFS= read -r -d '' r; do
        st=$W/restore-stage/$r; d=$P8_ROOT/$r
        if [ -d "$st" ] && [ ! -L "$st" ]; then
          if [ -L "$d" ] || { [ -e "$d" ] && [ ! -d "$d" ]; }; then p8_mvT "$d" "$TD/replaced/$r"; fi
          [ -d "$d" ] || mkdir "$d"
          chown --reference="$st" "$d" 2>/dev/null || true; chmod --reference="$st" "$d" 2>/dev/null || true
        else
          if [ -d "$d" ] && [ ! -L "$d" ]; then p8_mvT "$d" "$TD/replaced/$r"; fi
          # The temp name starts with "." and ends with "~": NM keyfiles, sudo,
          # cron, apt, udev and systemd all skip such names (review T8 NIT).
          t=$(dirname "$d")/.p8new-$(basename "$d")~
          if ! cp -a "$st" "$t" 2>/dev/null; then
            if [ -L "$st" ]; then cp -P "$st" "$t" || { p8_log "cannot stage $r"; rc=1; continue; }
            else
              # mode and owner kept (cp -a fails e.g. on vfat ownership; there they are mount-fixed)
              install -m "$(stat -c %a "$st")" -o "$(stat -c %u "$st")" -g "$(stat -c %g "$st")" "$st" "$t" 2>/dev/null \
                || { case $(stat -f -c %T "$(dirname "$d")" 2>/dev/null) in msdos | vfat) cp "$st" "$t" ;; *) false ;; esac; } \
                || { p8_log "cannot stage $r with its mode/owner"; rc=1; continue; }
            fi
          fi
          mv -f "$t" "$d" || { p8_log "cannot rename $r into place"; rm -f "$t"; rc=1; }
        fi
      done < "$W/restore.sorted0"
    else p8_log "tar extract failed"; rc=1; fi
    rm -rf "$W/restore-stage"
  fi
  # pseudo rows: the service user's groups and unit enablement
  while IFS=$'\t' read -r k p; do
    [ "$k" = changed ] || continue
    pre=$(awk -F '\t' -v p="$p" '$1 == p {print $6}' "$W/pre.manifest")
    case $p in
      @groups:*)
        u=${p#@groups:}
        now=$(id -nG "$u" | tr ' ' '\n' | LC_ALL=C sort)
        for g in $now; do case ",$pre," in *",$g,"*) ;; *) gpasswd -d "$u" "$g" >/dev/null && p8_log "groups: removed $u from $g" ;; esac; done
        for g in ${pre//,/ }; do printf '%s\n' "$now" | grep -qx "$g" || { usermod -aG "$g" "$u" && p8_log "groups: re-added $u to $g"; }; done ;;
      @enabled:*)
        u=${p#@enabled:}
        case $pre in
          enabled) systemctl enable "$u" >/dev/null 2>&1 && p8_log "enablement: re-enabled $u" ;;
          disabled) systemctl disable "$u" >/dev/null 2>&1 && p8_log "enablement: re-disabled $u" ;;
          *) p8_log "enablement of $u was '$pre' (not fixed automatically)"; rc=1 ;;
        esac ;;
    esac
  done < "$W/restore.diff"
fi

# Review T2: a manual restore that does NOT start PROD re-arms a short --auto
# dead-man first, so a lost ssh session can never leave 58b51737 down.
# `failsafe sig` (the HUP/INT/TERM trap between the disarm and the start, T2
# residual) arms it in EVERY mode: the units are gone by then.
failsafe() {
  if { [ "$AUTO" = 0 ] || [ "${1:-}" = sig ]; } && bash "$HERE/p8-deadman.sh" failsafe --window "$W" --label "$LABEL" --in-min "${P8_FAILSAFE_MIN:-10}" >/dev/null 2>&1; then
    p8_log "NOT GREEN: PROD not started; a FAILSAFE --auto dead-man is armed for +${P8_FAILSAFE_MIN:-10} min (it starts PROD). Fix and re-run, or --force-start"
  elif [ "$AUTO" = 0 ] || [ "${1:-}" = sig ]; then
    p8_log "NOT GREEN: PROD not started AND the failsafe dead-man could not be armed: start PROD by hand"
  fi
}
# shellcheck disable=SC2317,SC2329  # invoked by the trap below
on_sig() { set +e; trap - HUP INT TERM; failsafe sig; p8_log "signal between the disarm and the start: the failsafe was armed first"; exit 1; }

# 7. window artifacts
rm -f "$P8_H1_CONF"
trap on_sig HUP INT TERM
bash "$HERE/p8-deadman.sh" disarm >/dev/null
p8_test_pause
systemctl daemon-reload || true
udevadm control --reload-rules >/dev/null 2>&1 || true
if [ "$nm_changed" = 1 ]; then p8_log "an NM file changed: reloading NetworkManager"; systemctl reload NetworkManager >/dev/null 2>&1 || true; fi

# 8. verify BEFORE the start (F1: inside the oneshot on the boot path)
vrc=0
bash "$HERE/p8-verify.sh" pre --window "$W" > "$W/verify-pre.out" 2>&1 || vrc=$?
p8_redact < "$W/verify-pre.out" | sed 's/^/  /' >&2
if [ "$vrc" = 0 ]; then p8_log "verify pre: PASS"; else p8_log "verify pre: FAIL"; rc=1; fi

# 9. start PROD
if [ ! -d "$P8_ROOT/etc/xorgate" ]; then
  p8_log "NOT starting $P8_PROD_UNIT: /etc/xorgate is not back"; failsafe; exit 1
fi
if [ "$vrc" != 0 ] && [ "$AUTO" = 0 ] && [ "$FORCE" = 0 ]; then
  p8_log "NOT starting $P8_PROD_UNIT: the pre-start verify failed"; failsafe; exit 1
fi
p8_cancel_agent_jobs    # F1 again: nothing but our own start may be queued
systemctl start --no-block "$P8_PROD_UNIT" || { p8_log "start --no-block $P8_PROD_UNIT failed"; rc=1; }
p8_test_stop start
echo "$(p8_ts) verify=$([ "$vrc" = 0 ] && echo PASS || echo FAIL)" > "$W/restore.done"   # only AFTER the start is queued
trap - HUP INT TERM
p8_log "start --no-block $P8_PROD_UNIT queued"
if [ "$BOOT" = 1 ]; then
  p8_log "boot path: the agent starts when this oneshot exits; run p8-verify.sh post afterwards"
  exit "$rc"
fi
s=""
for _ in $(seq "$P8_START_WAIT_S"); do s=$(p8_active "$P8_PROD_UNIT"); [ "$s" = active ] && break; sleep 1; done
if [ "$s" = active ]; then p8_log "$P8_PROD_UNIT active"; else p8_log "$P8_PROD_UNIT is '$s' after ${P8_START_WAIT_S}s"; rc=1; fi

# 10. unattended: finish the window
if [ "$AUTO" = 1 ]; then
  systemctl stop "$P8_SAMPLER_UNIT" >/dev/null 2>&1 || true
  if bash "$HERE/p8-verify.sh" post --window "$W" > "$W/verify-post.out" 2>&1; then p8_log "verify post: PASS"; else p8_log "verify post: FAIL (see $W/verify-post.out)"; rc=1; fi
fi
p8_log "restore done rc=$rc"
exit "$rc"
