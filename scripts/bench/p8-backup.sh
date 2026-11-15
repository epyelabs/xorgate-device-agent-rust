#!/usr/bin/env bash
# p8-backup.sh: the common backup + move-aside of a Phase 8 bench window
# (plan §7.2.2-§7.2.5, H8, H10/Q-8.21, M3/M4). Order in a window:
#
#   Mac:   p8-backup.sh stage --host pi@<bench>            -> prints W=/root/p8-<ts>
#   bench: sudo W/bin/p8-verify.sh baseline --window W     (W0 values for DA15 + the sampler)
#   bench: sudo W/bin/p8-deadman.sh arm --window W --cap-min <60|75> --label w<n>
#   bench: sudo systemctl stop xorgate-agent-rs            (T0: 58b51737 offline)
#   bench: sudo W/bin/p8-backup.sh backup --window W       (manifest + tar, root 0600)
#   Mac:   p8-backup.sh pull --host pi@<bench> --window W  (~/xorgate-p8-private/<ts>/, sha both ends)
#   bench: sudo W/bin/p8-backup.sh aside --window W        (mv -T move-aside + seed + H1 conf)
#   bench: sudo W/bin/p8-modem-sampler.sh start --window W
#   bench: sudo W/bin/p8-backup.sh run-install --window W --label w<n> -- /home/pi/install.sh [--reprovision]
#   …      the window's checks; then sudo W/bin/p8-restore.sh --window W
#
# Subcommands:
#   stage --host H [--ts <UTCts>]       (Mac) create W (root 0700) and install the p8 tools in W/bin
#   backup --window W                   (bench) W/pre.manifest (§7.2.3, minus the volatile list),
#                                       W/backup.tgz (-p --xattrs --acls --numeric-owner; WITHOUT
#                                       /var/lib/xorgate-rs), W/backup.{sha256,count}; the seed files
#   pull --host H --window W            (Mac) copy the backup to ~/xorgate-p8-private/<ts>/ (0700);
#                                       sha256 on both ends + member count == manifest; marks W/pulled.ok
#   aside --window W [--no-h1] [--no-pull-check]
#                                       (bench) move the agent surface to W/aside/ with guarded
#                                       mv -T; within the same second create /var/lib/xorgate (0700
#                                       pi) holding ONLY the seed settings.json {gnssAntBias,
#                                       lteRecovery} copied from PROD (no cellular, H10); install the
#                                       H1 conf (Q-8.20 a) unless --no-h1; daemon-reload
#   seed --window W                     (bench) (re)write W/seed-settings.json + W/align.json from
#                                       PROD's settings.json (backup runs it); prints booleans only
#   run-install --window W --label wN -- <abs script> [args]
#                                       (bench) the installer in the transient unit p8-install-wN
#                                       (SUDO_USER=pi, HOME=/root, output to W/wN-install.log, 0600;
#                                       realism caveat: no TTY). The log is never printed.
# Exit: 0 ok, 1 failure, 2 usage, 3 a guard refused (nothing moved by that step).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/bench/p8-lib.sh
. "$HERE/p8-lib.sh"
P8_SSH=${P8_SSH:-ssh}
P8_SCP=${P8_SCP:-scp}
P8_PRIVATE=${P8_PRIVATE:-$HOME/xorgate-p8-private}
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15)

usage() { sed -n '2,40p' "$0"; exit 2; }
W=""; HOST=""; TS=""; NO_H1=0; NO_PULL=0; LABEL=""
parse() {
  while [ $# -gt 0 ]; do
    case $1 in
      --window) W=$2; shift 2 ;;
      --host) HOST=$2; shift 2 ;;
      --ts) TS=$2; shift 2 ;;
      --label) LABEL=$2; shift 2 ;;
      --no-h1) NO_H1=1; shift ;;
      --no-pull-check) NO_PULL=1; shift ;;
      --) shift; REST=("$@"); return 0 ;;
      *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
  done
}
REST=()
rsh() { "$P8_SSH" "${SSH_OPTS[@]}" "$HOST" "$@"; }

# ------------------------------------------------------------------- Mac side
cmd_stage() {
  local f stage
  [ -n "$HOST" ] || usage
  [ -n "$TS" ] || TS=$(date -u +%Y%m%dT%H%M%SZ)
  [[ $TS =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || { echo "bad --ts $TS" >&2; exit 2; }
  W=/root/p8-$TS; stage=/tmp/p8-stage-$TS
  rsh "mkdir -m 0700 $stage" || { echo "stage: cannot create $stage on $HOST" >&2; exit 1; }
  for f in p8-lib.sh p8-backup.sh p8-restore.sh p8-verify.sh p8-deadman.sh p8-modem-sampler.sh; do
    "$P8_SCP" "${SSH_OPTS[@]}" -q "$HERE/$f" "$HOST:$stage/$f"
  done
  rsh "sudo -n install -d -m 0700 -o root -g root $W $W/bin && sudo -n install -m 0700 -o root -g root $stage/p8-*.sh $W/bin/ && rm -rf $stage" \
    || { echo "stage: install into $W failed" >&2; exit 1; }
  echo "staged: W=$W (tools sha: $(cat "$HERE"/p8-*.sh | p8_sha_stdin | cut -c1-12))"
}

cmd_pull() {
  local ts dest f rsha lsha rcount lcount
  if [ -z "$HOST" ] || [ -z "$W" ]; then usage; fi
  ts=${W##*/p8-}
  [[ $ts =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || { echo "bad window $W" >&2; exit 2; }
  dest=$P8_PRIVATE/$ts
  case $dest in */plans/*) echo "refusing a private copy under plans/: $dest" >&2; exit 2 ;; esac
  umask 077
  mkdir -p "$dest"; chmod 0700 "$P8_PRIVATE" "$dest"
  rsha=$(rsh "sudo -n cat $W/backup.sha256" | cut -c1-64)
  [[ $rsha =~ ^[0-9a-f]{64}$ ]] || { echo "pull: no remote $W/backup.sha256 (run backup first)" >&2; exit 1; }
  for f in backup.tgz pre.manifest backup.count baseline.env align.json; do
    rsh "sudo -n cat $W/$f" > "$dest/$f" || { echo "pull: copying $f failed" >&2; exit 1; }
    chmod 0600 "$dest/$f"
  done
  lsha=$(p8_sha_stdin < "$dest/backup.tgz")
  [ "$lsha" = "$rsha" ] || { echo "pull: sha256 MISMATCH (remote ${rsha:0:12}, local ${lsha:0:12})" >&2; exit 1; }
  rcount=$(tr -d ' \n' < "$dest/backup.count")
  lcount=$(tar -tzf "$dest/backup.tgz" | wc -l | tr -d ' ')
  [ "$lcount" = "$rcount" ] || { echo "pull: member count $lcount != manifest $rcount" >&2; exit 1; }
  rsh "echo $lsha | sudo -n tee $W/pulled.ok >/dev/null" || { echo "pull: could not mark $W/pulled.ok" >&2; exit 1; }
  echo "pulled: $dest (0700) backup.tgz sha256 ${lsha:0:12}… on both ends, $lcount members == manifest"
  echo "H8: this copy holds the PROD device private key + certificate and location-bearing agent state; delete it at WP-X with the user's OK"
}

# ----------------------------------------------------------------- bench side
cmd_seed() {
  p8_need_root; p8_check_window "$W"
  python3 -I - "$P8_ROOT/var/lib/xorgate/settings.json" "$W/seed-settings.json" "$W/align.json" <<'PY'
import json, os, sys
src, seed_out, align_out = sys.argv[1:4]
try:
    with open(src) as f:
        d = json.load(f)
    found = True
except FileNotFoundError:
    d, found = {}, False
if not isinstance(d, dict):
    sys.exit("p8: PROD settings.json is not an object")
# H10: ONLY the daemon's antenna-bias and recovery inputs cross; never cellular.
seed = {k: d[k] for k in ("gnssAntBias", "lteRecovery") if k in d}
# WP0 C1: the PROD cloud document has no lteRecovery (the local value equals
# the daemon defaults), so the DEV align write is gnssAntBias only.
align = {k: d[k] for k in ("gnssAntBias",) if k in d}
for p, body in ((seed_out, seed), (align_out, align)):
    fd = os.open(p + ".tmp", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(body, f, sort_keys=True)
        f.write("\n")
    os.replace(p + ".tmp", p)
lr = d.get("lteRecovery")
lr_s = ",".join("%s:%s" % (k, lr[k]) for k in sorted(lr)) if isinstance(lr, dict) else "absent"
print("seed: settings.json %s; gnssAntBias=%s cellular=%s lteRecovery=%s (seed keys: %s)" % (
    "read" if found else "ABSENT", json.dumps(d.get("gnssAntBias", "absent")),
    "present" if "cellular" in d else "absent", lr_s, ",".join(sorted(seed)) or "none") + "; align keys: " + (",".join(sorted(align)) or "none"))
PY
}

cmd_backup() {
  local n c
  p8_need_root; p8_check_window "$W"
  export P8_LOGFILE=$W/backup.log
  for u in $P8_PROD_UNIT $P8_JS_UNIT; do
    [ "$(p8_active "$u")" != active ] || P8_DIE_RC=3 p8_die "$u is active: stop the PROD agent first (§7.2.2, T0)"
  done
  [ ! -e "$W/pre.manifest" ] || P8_DIE_RC=3 p8_die "$W/pre.manifest exists: one backup per window"
  p8_manifest "$W/pre.manifest.tmp" || p8_die "manifest failed"
  chmod 0600 "$W/pre.manifest.tmp"; mv -f "$W/pre.manifest.tmp" "$W/pre.manifest"
  p8_tarlist "$W/pre.manifest" "$W/tar.list0"
  n=$(tr -cd '\0' < "$W/tar.list0" | wc -c | tr -d ' ')
  (umask 077; tar --create --gzip --file "$W/backup.tgz" --numeric-owner --xattrs --acls --no-recursion \
      -C "${P8_ROOT:-/}" --null --verbatim-files-from -T "$W/tar.list0") || p8_die "tar failed"
  chmod 0600 "$W/backup.tgz"
  c=$(tar -tzf "$W/backup.tgz" | wc -l | tr -d ' ')
  [ "$c" = "$n" ] || p8_die "tar holds $c members, the manifest lists $n"
  echo "$n" > "$W/backup.count"
  p8_sha "$W/backup.tgz" > "$W/backup.sha256"
  chmod 0600 "$W/backup.count" "$W/backup.sha256"
  cmd_seed
  echo "backup: $(wc -l < "$W/pre.manifest" | tr -d ' ') manifest rows; backup.tgz $n members == manifest (without /var/lib/xorgate-rs), $(du -h "$W/backup.tgz" | cut -f1), sha256 $(cut -c1-12 "$W/backup.sha256")…"
}

cmd_aside() {
  local p t0 t1 n=0 a=0 L u
  p8_need_root; p8_check_window "$W"
  export P8_LOGFILE=$W/aside.log
  L=$W/aside.list
  if [ ! -f "$W/pre.manifest" ] || [ ! -f "$W/backup.tgz" ] || [ ! -f "$W/seed-settings.json" ]; then P8_DIE_RC=3 p8_die "no backup in $W (run backup)"; fi
  [ "$(p8_sha "$W/backup.tgz")" = "$(cut -c1-64 "$W/backup.sha256")" ] || P8_DIE_RC=3 p8_die "backup.tgz does not match backup.sha256"
  [ "$NO_PULL" = 1 ] || [ "$(cut -c1-64 "$W/pulled.ok" 2>/dev/null)" = "$(cut -c1-64 "$W/backup.sha256")" ] \
    || P8_DIE_RC=3 p8_die "the Mac copy is not verified ($W/pulled.ok): run 'p8-backup.sh pull' first"
  [ "$(p8_marker_get window)" = "${W#"$P8_ROOT"}" ] || P8_DIE_RC=3 p8_die "the dead-man is not armed for $W (p8-deadman.sh arm first)"
  [ -f "$W/baseline.env" ] || P8_DIE_RC=3 p8_die "no $W/baseline.env (p8-verify.sh baseline first)"
  [ ! -e "$L" ] || P8_DIE_RC=3 p8_die "$L exists: the surface is already aside"
  for u in $P8_AGENT_UNITS; do
    [ "$(p8_active "$u")" != active ] || P8_DIE_RC=3 p8_die "$u is active: stop it first"
  done
  # M3: every destination must be absent BEFORE anything moves.
  while IFS= read -r p; do
    if [ -e "$W/aside$p" ] || [ -L "$W/aside$p" ]; then P8_DIE_RC=3 p8_die "mv -T guard: $W/aside$p exists (refusing; nothing moved)"; fi
  done <<< "$P8_SURFACE"
  install -d -m 0700 "$W/aside"
  : > "$L"; chmod 0600 "$L"
  while IFS= read -r p; do
    if [ -e "$P8_ROOT$p" ] || [ -L "$P8_ROOT$p" ]; then
      printf 'moving\t%s\n' "$p" >> "$L"
      if [ "$p" = /var/lib/xorgate ]; then
        t0=$(date +%s%N)
        p8_mvT "$P8_ROOT$p" "$W/aside$p"
        # H10/M3: the daemon re-reads settings.json every 30 s; the seed is in place within ms.
        install -d -m 0700 -o "$P8_SVC_USER" -g "$P8_SVC_USER" "$P8_ROOT$p"
        install -m 0600 -o "$P8_SVC_USER" -g "$P8_SVC_USER" "$W/seed-settings.json" "$P8_ROOT$p/.settings.json.p8"
        mv -f "$P8_ROOT$p/.settings.json.p8" "$P8_ROOT$p/settings.json"
        t1=$(date +%s%N)
        p8_log "seed: /var/lib/xorgate moved and seeded in $(( (t1 - t0) / 1000000 )) ms"
      else
        p8_mvT "$P8_ROOT$p" "$W/aside$p"
      fi
      printf 'moved\t%s\n' "$p" >> "$L"; n=$((n + 1))
    else
      printf 'absent\t%s\n' "$p" >> "$L"; a=$((a + 1))
    fi
  done <<< "$P8_SURFACE"
  if [ "$NO_H1" = 0 ]; then
    printf 'APT::Get::Upgrade "false";\n' > "$P8_H1_CONF.tmp"; chmod 0644 "$P8_H1_CONF.tmp"; mv -f "$P8_H1_CONF.tmp" "$P8_H1_CONF"
    p8_log "H1: installed ${P8_H1_CONF#"$P8_ROOT"} (window only)"
  fi
  systemctl daemon-reload
  touch "$W/aside.done"
  echo "aside: $n paths moved to $W/aside (mv -T), $a absent; /var/lib/xorgate = the seed only; H1 conf $([ "$NO_H1" = 0 ] && echo installed || echo skipped)"
  echo "realism caveat: the seed settings.json is the only non-fresh state the test install sees"
}

cmd_run_install() {
  local s rc
  p8_need_root; p8_check_window "$W"
  [[ $LABEL =~ ^w[0-9]+$ ]] || { echo "--label must be w<n>" >&2; exit 2; }
  [ ${#REST[@]} -ge 1 ] || usage
  s=${REST[0]}
  case $s in /*) ;; *) echo "the installer path must be absolute" >&2; exit 2 ;; esac
  [ -f "$s" ] || { echo "no installer at $s" >&2; exit 2; }
  [ -f "$W/aside.done" ] || P8_DIE_RC=3 p8_die "the surface is not aside ($W/aside.done missing)"
  echo "run-install: p8-install-$LABEL (log $W/$LABEL-install.log, never printed); realism caveat: transient unit, no TTY, SUDO_USER=pi, HOME=/root"
  rc=0
  systemd-run --unit="p8-install-$LABEL" --setenv=SUDO_USER="$P8_SVC_USER" --setenv=HOME=/root \
    --working-directory="$(dirname "$s")" --wait --collect -p TimeoutStopSec=60 \
    -p "StandardOutput=file:${W#"$P8_ROOT"}/$LABEL-install.log" /bin/bash "$s" "${REST[@]:1}" || rc=$?
  chmod 0600 "$W/$LABEL-install.log" 2>/dev/null || true
  echo "run-install: exit $rc"
  return "$rc"
}

[ $# -ge 1 ] || usage
cmd=$1; shift
parse "$@"
case $cmd in
  stage) cmd_stage ;;
  pull) cmd_pull ;;
  seed) cmd_seed ;;
  backup) cmd_backup ;;
  aside) cmd_aside ;;
  run-install) cmd_run_install ;;
  *) usage ;;
esac
