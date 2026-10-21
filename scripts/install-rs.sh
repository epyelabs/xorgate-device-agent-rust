#!/usr/bin/env bash
# install-rs.sh: convert a JS-provisioned xorgate device to the Rust agent,
# and back (plans/device-agent-js-to-rust phase-5-distribution.md §4, §5, §8;
# interfaces I1-I3 in phase-5/wp-specific-briefs.md). Runbook: docs/convert.md.
#
#   sudo bash install-rs.sh [--from <bucket-url>] [--tag <ver>] [--tarball <file>] [--reinstall] [-y]
#   sudo /opt/xorgate-rs/install-rs.sh --revert [--purge [--force]]
#   sudo /opt/xorgate-rs/install-rs.sh --status
#   install-rs.sh -h | --help
#
# Self-contained on purpose: the device copy (/opt/xorgate-rs/install-rs.sh,
# shipped in every tarball) must work with no network, no working Rust binary
# and no sourced library (§5.8). It uses only what a JS-provisioned Bookworm
# device already has (§5.9): bash, coreutils, curl, GNU tar, sha256sum,
# visudo, systemctl, find, cmp (+ grep/sed). No jq, python or node.
#
# What it never touches (§4 "Never touched"): /etc/xorgate/** (read only),
# /opt/xorgate/**, /usr/local/bin/xorgate, the JS unit and its .d/, the JS
# updater unit, sudoers 020-023, the modem/system/logind/NM/udev/cron assets,
# and never systemd/bench/* from the tarball (CF67). It never power-cycles the
# device and never restarts the modem daemon.
set -euo pipefail
umask 022
export LC_ALL=C

PREFIX="[install-rs] "
AGENT_ENV=/etc/xorgate/agent.env
JS_PREFIX=/opt/xorgate
RS_PREFIX=/opt/xorgate-rs
STAGING=/opt/xorgate-rs.new
BACKUP=/opt/xorgate-rs.old
WRAPPER=/usr/local/bin/xorgate-rs
SYSD=/etc/systemd/system
WANTS=$SYSD/multi-user.target.wants
JS_SVC=xorgate-agent
RS_SVC=xorgate-agent-rs
JS_UNIT=$SYSD/$JS_SVC.service
JS_USER_CONF=$JS_UNIT.d/10-user.conf
RS_UNIT=$SYSD/$RS_SVC.service
RS_UNIT_D=$RS_UNIT.d
RS_USER_CONF=$RS_UNIT_D/10-user.conf
UPD_UNIT=$SYSD/xorgate-updater-rs.service
UPD_UNIT_D=$UPD_UNIT.d
DIST_DROPIN=$UPD_UNIT_D/10-dist.conf
SUDOERS_024=/etc/sudoers.d/024_xorgate-update-rs
REC_DIR=/var/lib/xorgate-rs-install
REC=$REC_DIR/record
PRE=$REC_DIR/pre.manifest
LOCK=/run/xorgate-rs-install.lock
# I1 step 3: what a staged build must carry before anything moves.
REQUIRED_MEMBERS=(bin/xorgate-rs build-info.json systemd/xorgate-agent-rs.service systemd/xorgate-updater-rs.service install-rs.sh)
# update_plan::SMOKE_PREFIX.
SMOKE_PREFIX="xorgate-rs "
VERIFY_SECS=90
# I3 roots (one line per path) and tree roots (one `t <digest>` line each).
MANIFEST_ROOTS=(/etc/xorgate /usr/local/bin /etc/systemd/system /etc/sudoers.d /etc/cron.d /etc/udev/rules.d
  /etc/NetworkManager/conf.d /etc/systemd/logind.conf.d /var/lib/xorgate-rs-install)
TREE_ROOTS=(/opt/xorgate /opt/xorgate-rs /opt/xorgate-rs.new /opt/xorgate-rs.old)

# ---------------------------------------------------------------------------
# Bodies. Each is pinned byte for byte by fixtures/install_rs/bodies.json,
# which crates/xorgate-core/tests/install_rs_bodies.rs ties to update_plan and
# scripts/tests/install-rs-bodies.sh ties to these functions (CF68), so the
# first `xorgate-rs update` after a conversion rewrites nothing.

# update_plan::WRAPPER_BODY (§4 row "/usr/local/bin/xorgate-rs").
rs_wrapper_body() {
  # shellcheck disable=SC2016  # the $ belong to the wrapper, written literally
  printf '%s\n' '#!/usr/bin/env bash' \
    'exec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" /opt/xorgate-rs/bin/xorgate-rs "$@"'
}
# update_plan::updater_sudoers_body(<user>) (§4 row "024", CF68).
rs_update_sudoers_body() {
  printf '%s\n' \
    "# Installed by xorgate-rs. Allows the agent (running as $1) to start the" \
    "# one-shot self-update unit on a cloud-issued 'update' command, and nothing" \
    "# else. See device-agent-rust crates/xorgate-hal/src/privileged.rs + systemd/xorgate-updater-rs.service." \
    "$1 ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater-rs.service"
}
# §8 / CF63: the updater follows xorgate-agent-rs-latest, never the JS pin.
rs_dist_dropin_body() {
  printf '%s\n' \
    "# Installed by install-rs.sh. agent.env's DEVICE_DIST_VERSION is the JS agent's pin;" \
    "# the Rust updater always follows xorgate-agent-rs-latest (README §6: the cloud update stays latest-only)." \
    "[Service]" \
    "Environment=DEVICE_DIST_VERSION=latest"
}

usage() {
  cat <<'EOF'
install-rs.sh: convert a JS-provisioned xorgate device to the Rust agent, and back.

  sudo bash install-rs.sh [--from <bucket-url>] [--tag <ver>] [--tarball <file>] [--reinstall] [-y]
      Convert (default). Installs /opt/xorgate-rs, /usr/local/bin/xorgate-rs,
      xorgate-agent-rs.service (+ a copy of the JS 10-user.conf), xorgate-updater-rs.service
      (+ 10-dist.conf) and /etc/sudoers.d/024_xorgate-update-rs; disables xorgate-agent;
      enables + starts xorgate-agent-rs (whose Conflicts= stops xorgate-agent).
      Re-running it on a converted device re-verifies and repairs that surface and
      never changes the build.
      --from <url>      dist bucket (else env DEVICE_DIST_BUCKET_URL, else
                        /etc/xorgate/agent.env); one trailing / is stripped
      --tag <ver>       artifact xorgate-agent-rs-<ver>.tar.gz (default: latest;
                        agent.env's DEVICE_DIST_VERSION is the JS pin and is ignored)
      --tarball <file>  install a local tarball; <file>.sha256 is REQUIRED (LAN/offline)
      --reinstall       on a converted device, fetch + swap the build as well
      -y                do not ask for confirmation (asked only on a terminal)
  sudo /opt/xorgate-rs/install-rs.sh --revert [--purge [--force]]
      Disable + stop xorgate-agent-rs and restore xorgate-agent as the conversion
      record found it (started if it was enabled or active). Needs no network and
      no Rust binary. The Rust files stay (inert) unless --purge, which deletes
      exactly what the conversion installed; --purge refuses when a Rust install
      predates the converter (rsPreexisting=yes) unless --force.
  sudo /opt/xorgate-rs/install-rs.sh --status
      First line state=converted|converting|reverted|not-converted; then the
      record, both units and the installed build. Always exits 0.

Exit codes: 0 done / no-op; 1 failed, device back at its pre-run state;
2 usage; 3 refused by a precondition (not root, not JS-provisioned, purge of a
pre-existing install; nothing changed); 4 started but not healthy within 90 s,
automatically reverted (JS restored).

Record: /var/lib/xorgate-rs-install/{record,pre.manifest} (root 0700).
Logs: journalctl -u xorgate-agent-rs, journalctl -u xorgate-updater-rs.

TEST-ONLY: XORGATE_RS_CONVERT_FAIL_AT=<n> exits 1 right after convert step <n>
(0 preconditions, 1 record, 2 fetch+verify, 3 stage+smoke, 4 swap, 5 wrapper,
6 units, 7 sudoers, 8 systemctl, 9 verify) with no cleanup, to simulate a
crash for the e2e. Never set it on a real device.
EOF
}

# ---------------------------------------------------------------------------
# Output and small helpers.
say() { printf '%s%s\n' "$PREFIX" "$*"; }
CHANGES=()
note() { CHANGES+=("$1"); say "  - $1"; }
summary() {
  local c
  say "changed (${#CHANGES[@]})"
  for c in ${CHANGES[@]+"${CHANGES[@]}"}; do say "  - $c"; done
  return 0
}
utc() { date -u +%Y-%m-%dT%H:%M:%SZ; }
first_line() { local l=""; IFS= read -r l || true; printf '%s' "$l"; }
unit_enabled() { local o; o=$(systemctl is-enabled "$1" 2>/dev/null) || true; printf '%s' "${o%%$'\n'*}"; }
unit_active() { local o; o=$(systemctl is-active "$1" 2>/dev/null) || true; printf '%s' "${o%%$'\n'*}"; }
# update_plan::failure_detail: trimmed stderr, else the exit code.
detail() { # <stderr file> <rc>
  local t
  t=$(tr -s '\n\t' '  ' < "$1" 2>/dev/null || true)
  t=${t#"${t%%[![:space:]]*}"}; t=${t%"${t##*[![:space:]]}"}
  if [ -n "$t" ]; then printf '%s' "$t"; else printf 'exit %s' "$2"; fi
}
json_str() { sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$1" 2>/dev/null | first_line; }

MODE=none
FINISHED=0
PHASE=none      # none (steps 0-1) | fetch (2-3) | install (4-7) | start (8-9)
LOCKED=0
finish() { FINISHED=1; exit "$1"; }
usage_error() { printf '%s%s\n' "$PREFIX" "$1" "$PREFIX" "see: install-rs.sh --help" >&2; FINISHED=1; exit 2; }
refuse() { say "refused: $1 (nothing changed)"; finish 3; }

# agent.env: update_plan::parse_env_file for one key (the last one wins; a
# value holding a CR does not match; trimmed; one leading and one trailing
# quote stripped). Prints the value; returns 0 when the key is present.
env_file_get() {
  local file=$1 key=$2 line k v found=1 out=""
  [ -r "$file" ] || return 1
  while IFS= read -r line || [ -n "$line" ]; do
    [[ $line =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)[[:space:]]*=(.*)$ ]] || continue
    k=${BASH_REMATCH[1]}; v=${BASH_REMATCH[2]}
    [ "$k" = "$key" ] || continue
    v=${v#"${v%%[![:space:]]*}"}
    case $v in *$'\r'*) continue ;; esac
    v=${v%"${v##*[![:space:]]}"}
    case $v in \"* | \'*) v=${v:1} ;; esac
    case $v in *\" | *\') v=${v%?} ;; esac
    out=$v; found=0
  done < "$file"
  [ "$found" = 0 ] || return 1
  printf '%s' "$out"
}

# update_plan::parse_sha256_sidecar: the first `<64 hex>  [*]<basename>` line
# naming exactly <basename> (no path); prints the lower-case digest.
sidecar_digest() {
  local file=$1 base=$2 line hex rest
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line#"${line%%[![:space:]]*}"}
    line=${line%"${line##*[![:space:]]}"}
    case $line in *[$' \t']*) ;; *) continue ;; esac
    hex=${line%%[$' \t']*}
    rest=${line#*[$' \t']}
    rest=${rest#"${rest%%[!$' \t']*}"}
    rest=${rest#\*}
    [ "$rest" = "$base" ] || continue
    [[ $hex =~ ^[0-9a-fA-F]{64}$ ]] || continue
    printf '%s' "${hex,,}"
    return 0
  done < "$file"
  return 1
}

# update_plan::parse_service_user: the first `User=<word>` line.
service_user_of() {
  [ -r "$1" ] || return 1
  sed -n 's/^[[:space:]]*User[[:space:]]*=[[:space:]]*\([^[:space:]]\{1,\}\).*/\1/p' "$1" | first_line
}

# ---------------------------------------------------------------------------
# I3 manifest: `<f|d|l> <octal mode> <uid>:<gid> <sha256 | - | ->target> <path>`,
# one line per path, sorted by path (C locale), over MANIFEST_ROOTS, plus one
# `t <digest> <root>` line per TREE_ROOT present. <octal mode> is find's %m
# (no leading zero: 644, 755, 4755). <digest> is the sha256 of that tree's
# own I3 lines (every entry, the root included), sorted by path, each line
# \n-terminated. Paths holding a tab or a newline are not supported.
_i3_records() { # <root> → "path<TAB>line" records, unsorted
  local root=$1 tmp rec y m o l p d
  local -A H=()
  tmp=$(mktemp -d)
  find "$root" -type f -print0 > "$tmp/files"
  xargs -0 -r sha256sum -z -- < "$tmp/files" > "$tmp/sums"
  while IFS= read -r -d '' rec; do H["${rec:66}"]=${rec:0:64}; done < "$tmp/sums"
  find "$root" -printf '%y\0%m\0%U:%G\0%l\0%p\0' > "$tmp/meta"
  while IFS= read -r -d '' y && IFS= read -r -d '' m && IFS= read -r -d '' o \
        && IFS= read -r -d '' l && IFS= read -r -d '' p; do
    case $y in
      f) d=${H[$p]:-?} ;;
      l) d="->$l" ;;
      *) d=- ;;
    esac
    printf '%s\t%s %s %s %s %s\n' "$p" "$y" "$m" "$o" "$d" "$p"
  done < "$tmp/meta"
  rm -rf "$tmp"
}
msort() { sort -t "$(printf '\t')" -k1,1 | cut -f2-; }
manifest() { # → the I3 manifest on stdout
  local r digest
  {
    for r in "${MANIFEST_ROOTS[@]}"; do
      if [ -e "$r" ] || [ -L "$r" ]; then _i3_records "$r"; fi
    done
    for r in "${TREE_ROOTS[@]}"; do
      if [ -e "$r" ] || [ -L "$r" ]; then
        digest=$(_i3_records "$r" | msort | sha256sum | cut -d' ' -f1)
        printf '%s\tt %s %s\n' "$r" "$digest" "$r"
      fi
    done
  } | msort
}
MP=""
mpath() { # <I3 line> → MP = its path field
  local l=$1
  case $l in
    t\ *) l=${l#* }; l=${l#* } ;;
    *) l=${l#* }; l=${l#* }; l=${l#* }; l=${l#* } ;;
  esac
  MP=$l
}
key_by_path() { local t; while IFS= read -r t; do mpath "$t"; printf '%s\t%s\n' "$MP" "$t"; done; }
# The Rust-owned rows (§4 NEW rows). Everything else in the manifest is the
# JS-owned surface, which must come back byte for byte.
is_rust_owned() {
  case $1 in
    "$RS_PREFIX" | "$STAGING" | "$BACKUP" | "$WRAPPER" | "$RS_UNIT" | "$RS_UNIT_D" | "$RS_UNIT_D"/* \
      | "$UPD_UNIT" | "$UPD_UNIT_D" | "$UPD_UNIT_D"/* | "$WANTS/$RS_SVC.service" | "$SUDOERS_024") return 0 ;;
  esac
  return 1
}
is_rec() { case $1 in "$REC_DIR" | "$REC_DIR"/*) return 0 ;; esac; return 1; }
# mfilter <keep|purge>: the rows a self-check compares. Never the record dir
# (§4: excluded in keep mode; in purge mode it is deleted after the check,
# and pre.manifest never holds it). keep: the JS-owned rows only.
mfilter() {
  local line
  while IFS= read -r line; do
    mpath "$line"
    is_rec "$MP" && continue
    if [ "$1" = keep ] && is_rust_owned "$MP"; then continue; fi
    printf '%s\n' "$line"
  done
}
in_manifest() { # <path> <manifest file>
  local line
  [ -f "$2" ] || return 1
  while IFS= read -r line; do mpath "$line"; [ "$MP" = "$1" ] && return 0; done < "$2"
  return 1
}
# selfcheck <pre.manifest> <keep|purge>: prints each differing path; DIFFS = count.
DIFFS=0
selfcheck() {
  local pre=$1 mode=$2 tmp line p
  local -A WAS=() NOW=()
  tmp=$(mktemp -d)
  mfilter "$mode" < "$pre" | sort > "$tmp/a"
  manifest | mfilter "$mode" | sort > "$tmp/b"
  comm -23 "$tmp/a" "$tmp/b" > "$tmp/gone"
  comm -13 "$tmp/a" "$tmp/b" > "$tmp/new"
  while IFS= read -r line; do mpath "$line"; WAS["$MP"]=1; done < "$tmp/gone"
  while IFS= read -r line; do mpath "$line"; NOW["$MP"]=1; done < "$tmp/new"
  { for p in "${!WAS[@]}" "${!NOW[@]}"; do printf '%s\n' "$p"; done; } | sort -u > "$tmp/paths"
  DIFFS=0
  while IFS= read -r p; do
    DIFFS=$((DIFFS + 1))
    if [ -n "${WAS[$p]:-}" ] && [ -n "${NOW[$p]:-}" ]; then say "  ~ $p (differs from the conversion record)"
    elif [ -n "${WAS[$p]:-}" ]; then say "  - $p (in the record, missing now)"
    else say "  + $p (not in the record)"; fi
  done < "$tmp/paths"
  rm -rf "$tmp"
}

# ---------------------------------------------------------------------------
# I2 record: key=value lines in exactly this order.
REC_KEYS=(state startedAt convertedAt version channel sha256 source jsEnabled jsActive rsPreexisting serviceUser installed)
declare -A R=()
rec_load() { # <file>
  local k
  R=()
  for k in "${REC_KEYS[@]}"; do R[$k]=$(sed -n "s/^$k=//p" "$1" | first_line); done
}
rec_save() {
  local k tmp
  tmp=$(mktemp "$REC_DIR/.record.XXXXXX")
  for k in "${REC_KEYS[@]}"; do printf '%s=%s\n' "$k" "${R[$k]:-}"; done > "$tmp"
  chmod 0600 "$tmp"
  if [ -f "$REC" ] && cmp -s "$tmp" "$REC"; then rm -f "$tmp"; else mv -f "$tmp" "$REC"; fi
}
rec_add_installed() { # <path>: a path this conversion created (saved at once: crash-safe)
  case " ${R[installed]:-} " in *" $1 "*) return 0 ;; esac
  R[installed]=${R[installed]:+${R[installed]} }$1
  rec_save
}
latest_reverted() { # → the newest record.reverted-<ts>, or nothing
  local f last=""
  for f in "$REC_DIR"/record.reverted-*; do if [ -f "$f" ]; then last=$f; fi; done
  printf '%s' "$last"
}
rec_state() {
  if [ -d "$REC_DIR" ] && [ ! -x "$REC_DIR" ]; then echo unknown; return 0; fi
  if [ -f "$REC" ]; then
    case $(sed -n 's/^state=//p' "$REC" | first_line) in converted) echo converted ;; *) echo converting ;; esac
  elif [ -n "$(latest_reverted)" ]; then echo reverted
  else echo not-converted; fi
}

# ---------------------------------------------------------------------------
lock() {
  local pid
  if ! mkdir "$LOCK" 2>/dev/null; then
    pid=$(cat "$LOCK/pid" 2>/dev/null || true)
    if [ -n "$pid" ] && [ "$pid" != "$$" ] && kill -0 "$pid" 2>/dev/null; then
      refuse "another install-rs.sh is running (pid $pid)"
    fi
    rm -rf "$LOCK"; mkdir "$LOCK"
  fi
  echo "$$" > "$LOCK/pid"; LOCKED=1
}
unlock() { if [ "$LOCKED" = 1 ]; then rm -rf "$LOCK"; LOCKED=0; fi; }

require_root() { [ "$(id -u)" = 0 ] || refuse "must run as root (sudo)"; }

# Bash reads a script while it runs: before anything can delete or replace
# this file (--purge deletes /opt/xorgate-rs/install-rs.sh; a swap replaces
# the tree), continue from a private copy.
ORIG_ARGS=()
reexec_private() {
  local self=${BASH_SOURCE[0]} tmp
  [ -z "${XORGATE_RS_INSTALL_REEXEC:-}" ] || return 0
  [ -f "$self" ] || return 0
  tmp=$(mktemp /tmp/install-rs.XXXXXX)
  cp -- "$self" "$tmp"
  XORGATE_RS_INSTALL_REEXEC=$tmp exec bash "$tmp" ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"}
}

maybe_fail_at() { # §5.3: test-only crash injection (documented in --help)
  if [ "${XORGATE_RS_CONVERT_FAIL_AT:-}" = "$1" ]; then
    say "TEST-ONLY: XORGATE_RS_CONVERT_FAIL_AT=$1: exiting 1 after step $1 with no cleanup"
    trap - EXIT
    unlock
    exit 1
  fi
}

# ---------------------------------------------------------------------------
# Convert (§4, §5, I1 steps 0-9).
FROM=""; TAG=""; TARBALL=""; REINSTALL=0; YES=0
DL=""; TGZ=""; SHA=""; SOURCE=""; VER=""; CHANNEL=""
RUN_CREATED=(); REC_CREATED=0; REC_DIR_CREATED=0; HAD_PREV=0; SWAPPED=0; NEED_RESTART=0

created() { RUN_CREATED+=("$1"); rec_add_installed "$1"; }
# put_file <dest> <mode> <src>: cmp-before-write (the bench install.sh
# idiom); returns 0 when it wrote, 1 when the file was already right.
put_file() {
  local dest=$1 mode=$2 src=$3 existed=0
  if [ -L "$dest" ]; then rm -f "$dest"   # never write through a symlink
  elif [ -e "$dest" ]; then existed=1; fi
  if [ "$existed" = 1 ] && cmp -s "$src" "$dest" && [ "$(stat -c '%a %u:%g' "$dest")" = "${mode#0} 0:0" ]; then
    say "  = $dest unchanged"
    return 1
  fi
  install -m "$mode" -o root -g root "$src" "$dest" || abort_convert "cannot write $dest"
  if [ "$existed" = 0 ]; then created "$dest"; fi
  note "$dest written"
  return 0
}
put_body() { # <dest> <mode> <body function> [args]: put_file of a generated body
  local dest=$1 mode=$2 tmp rc=0
  shift 2
  tmp=$(mktemp)
  "$@" > "$tmp"
  put_file "$dest" "$mode" "$tmp" || rc=$?
  rm -f "$tmp"
  return "$rc"
}
ensure_dir() {
  [ ! -d "$1" ] || return 0
  install -d -m 0755 -o root -g root "$1" || abort_convert "cannot create $1"
  created "$1"
  note "$1/ created"
}

# Failure handling (I1): steps 2-3 leave the device unchanged; steps 4-7 roll
# back to the pre-run state; steps 8-9 revert to the JS agent (exit 4).
abort_convert() { # <message>
  set +e
  local i p
  case $PHASE in
    none | fetch)
      [ -z "$DL" ] || rm -rf "$DL"
      rm -rf "$STAGING"
      if [ "$REC_CREATED" = 1 ]; then rm -f "$REC" "$PRE" "$REC_DIR"/.record.*; fi
      if [ "$REC_DIR_CREATED" = 1 ]; then rm -rf "$REC_DIR"; fi
      say "failed: $1"
      say "nothing was changed (exit 1)"
      FINISHED=1; unlock; exit 1 ;;
    install)
      if [ "$SWAPPED" = 1 ]; then
        rm -rf "$RS_PREFIX"
        if [ "$HAD_PREV" = 1 ]; then mv "$BACKUP" "$RS_PREFIX" && say "  $RS_PREFIX restored from $BACKUP"; fi
      fi
      rm -rf "$STAGING"
      for ((i = ${#RUN_CREATED[@]} - 1; i >= 0; i--)); do
        p=${RUN_CREATED[$i]}
        [ "$p" != "$RS_PREFIX" ] || continue
        if [ -d "$p" ] && [ ! -L "$p" ]; then rmdir "$p" 2>/dev/null; else rm -f "$p"; fi
      done
      if [ "$REC_CREATED" = 1 ]; then rm -f "$REC" "$PRE" "$REC_DIR"/.record.*; fi
      if [ "$REC_DIR_CREATED" = 1 ]; then rm -rf "$REC_DIR"; fi
      [ -z "$DL" ] || rm -rf "$DL"
      say "failed: $1"
      say "rolled back: the files this run created are removed and $RS_PREFIX is as it was (exit 1)"
      FINISHED=1; unlock; exit 1 ;;
    start)
      PHASE=none
      say "failed: $1"
      if command -v journalctl >/dev/null 2>&1; then
        say "journal tail (journalctl -u $RS_SVC):"
        journalctl -u "$RS_SVC" -n 30 --no-pager 2>&1 | sed "s/^/$PREFIX    /"
      fi
      say "automatic keep-mode revert (§5.5):"
      REVERT_FINAL=0
      revert_main
      say "FAILED: $RS_SVC did not become healthy; reverted to $JS_SVC ($(unit_enabled "$JS_SVC") + $(unit_active "$JS_SVC")). see: journalctl -u $RS_SVC (exit 4)"
      FINISHED=1; unlock; exit 4 ;;
  esac
}
on_exit() {
  local rc=$1
  if [ "$FINISHED" = 0 ] && [ "$rc" != 0 ] && [ "$MODE" = convert ]; then
    if [ "$PHASE" != none ] || [ "$REC_CREATED" = 1 ]; then
      FINISHED=1
      abort_convert "unexpected error (exit $rc)"
    fi
  fi
  unlock
  if [ -n "${XORGATE_RS_INSTALL_REEXEC:-}" ]; then rm -f "$XORGATE_RS_INSTALL_REEXEC"; fi
}

fetch_and_verify() { # step 2 (§5.4, §5.7) → TGZ, SHA, SOURCE
  local base url sumf want have bucket rc
  DL=$(mktemp -d /tmp/xg-install-rs-XXXXXX)
  if [ -n "$TARBALL" ]; then
    if [ -n "$FROM$TAG" ]; then say "  note: --tarball given: --from/--tag ignored"; fi
    [ -f "$TARBALL" ] || abort_convert "no such tarball: $TARBALL"
    base=$(basename -- "$TARBALL")
    sumf="$TARBALL.sha256"
    [ -f "$sumf" ] || abort_convert "no checksum published for $base ($sumf missing); refusing an unverified build"
    TGZ=$TARBALL
    SOURCE="$(cd "$(dirname -- "$TARBALL")" && pwd)/$base"
    say "  local tarball $SOURCE"
  else
    # --from → env DEVICE_DIST_BUCKET_URL → agent.env (update_plan::resolve_source
    # order; an env var set to "" wins and fails); ONE trailing / stripped.
    if [ -n "$FROM" ]; then bucket=$FROM
    elif [ -n "${DEVICE_DIST_BUCKET_URL+set}" ]; then bucket=$DEVICE_DIST_BUCKET_URL
    else bucket=$(env_file_get "$AGENT_ENV" DEVICE_DIST_BUCKET_URL || true); fi
    bucket=${bucket%/}
    [ -n "$bucket" ] || abort_convert "no dist bucket URL — set DEVICE_DIST_BUCKET_URL in $AGENT_ENV or pass --from <url>"
    # §5.7 / Q8: --tag, else latest. NEVER DEVICE_DIST_VERSION: that is the
    # JS agent's tag and names no -rs artifact.
    base="xorgate-agent-rs-${TAG:-latest}.tar.gz"
    url="$bucket/$base"
    TGZ="$DL/$base"
    sumf="$DL/$base.sha256"
    say "  downloading $url"
    rc=0; curl -fsSL --retry 3 --retry-delay 2 "$url" -o "$TGZ" 2> "$DL/err" || rc=$?
    [ "$rc" = 0 ] || abort_convert "download failed ($(detail "$DL/err" "$rc")); check the version/tag and bucket"
    say "  downloading $url.sha256"
    rc=0; curl -fsSL --retry 3 --retry-delay 2 "$url.sha256" -o "$sumf" 2> "$DL/err" || rc=$?
    [ "$rc" = 0 ] || abort_convert "no checksum published for $base ($(detail "$DL/err" "$rc")); refusing an unverified build"
    SOURCE=$url
  fi
  want=$(sidecar_digest "$sumf" "$base") \
    || abort_convert "no checksum published for $base ($(basename -- "$sumf") has no entry for it); refusing an unverified build"
  have=$(sha256sum < "$TGZ" | cut -d' ' -f1)
  [ "$have" = "$want" ] || abort_convert "checksum mismatch for $base (published $want, downloaded $have)"
  say "  checksum ok (sha256 $have)"
  SHA=$have
}

stage_and_smoke() { # step 3 (§5.4): nothing outside $STAGING changes here
  local m out rc
  rm -rf "$STAGING"
  mkdir -p "$STAGING"
  rc=0; tar --no-same-owner -xzf "$TGZ" -C "$STAGING" 2> "$DL/err" || rc=$?
  [ "$rc" = 0 ] || abort_convert "extract failed: $(detail "$DL/err" "$rc")"
  for m in "${REQUIRED_MEMBERS[@]}"; do
    [ -e "$STAGING/$m" ] || abort_convert "the build is missing $m (required: ${REQUIRED_MEMBERS[*]})"
  done
  say "  checking the staged build runs on this device"
  rc=0; out=$(timeout 5 "$STAGING/bin/xorgate-rs" -v 2> "$DL/err") || rc=$?
  [ "$rc" = 0 ] || abort_convert "staged build does not run on this device ($(detail "$DL/err" "$rc")); not installed"
  case $out in
    "$SMOKE_PREFIX"*) ;;
    *) abort_convert "staged build does not run on this device (printed \"$(printf '%s' "$out" | first_line)\"); not installed" ;;
  esac
  say "  staged $(printf '%s' "$out" | first_line)"
  VER=$(json_str "$STAGING/build-info.json" version)
  CHANNEL=$(json_str "$STAGING/build-info.json" channel)
}

swap_tree() { # step 4: same-filesystem renames, the previous build kept as .old
  rm -rf "$BACKUP"
  HAD_PREV=0
  if [ -e "$RS_PREFIX" ]; then mv "$RS_PREFIX" "$BACKUP" || abort_convert "cannot move $RS_PREFIX aside"; HAD_PREV=1; fi
  SWAPPED=1
  mv "$STAGING" "$RS_PREFIX" || abort_convert "cannot move $STAGING to $RS_PREFIX"
  if [ "$HAD_PREV" = 1 ]; then note "$RS_PREFIX swapped to $VER (previous build at $BACKUP until verified)"
  else created "$RS_PREFIX"; note "$RS_PREFIX installed ($VER)"; fi
  NEED_RESTART=1
}

convert_main() {
  local user js_pin live_ok=1 m pre_tmp prev prev_pre was_converted=0 rs_exists ans t ra ja deadline first=0
  local missing=()
  # ---- step 0: preconditions (§5.6: refuse with exit 3, change nothing) ----
  require_root
  for m in "$AGENT_ENV" "$JS_PREFIX" "$JS_UNIT"; do [ -e "$m" ] || missing+=("$m"); done
  [ ${#missing[@]} = 0 ] || refuse "not a JS-provisioned device: missing ${missing[*]}"
  lock
  if [ -f "$REC" ] && [ "$(sed -n 's/^state=//p' "$REC" | first_line)" = converted ]; then was_converted=1; fi
  if [ "$was_converted" = 0 ] && [ "$YES" = 0 ] && [ -t 0 ]; then
    printf '%sconvert this device to the Rust agent? %s will be stopped and disabled [y/N] ' "$PREFIX" "$JS_SVC"
    IFS= read -r ans || ans=""
    case $ans in y | Y | yes | YES) ;; *) say "not converted (no confirmation; -y skips the question)"; finish 0 ;; esac
  fi
  maybe_fail_at 0

  # ---- step 1: the record, written ONCE (§5.1) ----
  say "== 1. conversion record $REC"
  if [ -f "$REC" ]; then
    rec_load "$REC"
    say "  = record exists (state=${R[state]}, started ${R[startedAt]}): jsEnabled/jsActive/pre.manifest kept"
  else
    user=$(service_user_of "$JS_USER_CONF" || true)
    rs_exists=no
    if [ -e "$RS_PREFIX" ] || [ -e "$RS_UNIT" ]; then rs_exists=yes; fi
    R=([state]=converting [startedAt]="$(utc)" [convertedAt]="" [version]="" [channel]="" [sha256]="" [source]=""
       [jsEnabled]="$(unit_enabled "$JS_SVC")" [jsActive]="$(unit_active "$JS_SVC")" [rsPreexisting]="$rs_exists"
       [serviceUser]="${user:-root}" [installed]="")
    pre_tmp=$(mktemp)
    prev=$(latest_reverted)
    prev_pre=${prev/record.reverted-/pre.manifest.reverted-}
    if [ "$rs_exists" = yes ] && [ -n "$prev" ] && [ -f "$prev_pre" ]; then
      # A keep-mode revert by THIS converter left the Rust files: they are
      # ours, not pre-existing. Inherit rsPreexisting + installed from that
      # record, and build pre.manifest from the JS rows as they are now plus
      # the Rust rows of the earlier pre.manifest (the device before ANY
      # conversion), so a later --purge still lands on it (wp1.md D-WP1-3).
      R[rsPreexisting]=$(sed -n 's/^rsPreexisting=//p' "$prev" | first_line)
      R[installed]=$(sed -n 's/^installed=//p' "$prev" | first_line)
      {
        manifest | mfilter keep
        while IFS= read -r t; do mpath "$t"; if is_rust_owned "$MP"; then printf '%s\n' "$t"; fi; done < "$prev_pre"
      } | key_by_path | msort > "$pre_tmp"
      say "  re-conversion after a keep-mode revert ($(basename -- "$prev")): rsPreexisting=${R[rsPreexisting]} inherited"
    else
      manifest | mfilter purge > "$pre_tmp"
    fi
    if [ ! -d "$REC_DIR" ]; then install -d -m 0700 -o root -g root "$REC_DIR"; REC_DIR_CREATED=1; fi
    REC_CREATED=1
    install -m 0600 -o root -g root "$pre_tmp" "$PRE"
    rm -f "$pre_tmp"
    rec_save
    note "$REC written (state=converting jsEnabled=${R[jsEnabled]} jsActive=${R[jsActive]} rsPreexisting=${R[rsPreexisting]} serviceUser=${R[serviceUser]})"
    if [ "${R[rsPreexisting]}" = yes ]; then
      say "  note: a Rust install predates this conversion (rsPreexisting=yes): --revert --purge will refuse without --force"
    fi
  fi
  maybe_fail_at 1

  # ---- steps 2-4: only when the build must change (§5.2) ----
  for m in "${REQUIRED_MEMBERS[@]}"; do [ -e "$RS_PREFIX/$m" ] || live_ok=0; done
  if [ "${R[state]}" = converted ] && [ "$REINSTALL" = 0 ] && [ "$live_ok" = 1 ]; then
    VER=$(json_str "$RS_PREFIX/build-info.json" version)
    CHANNEL=$(json_str "$RS_PREFIX/build-info.json" channel)
    say "== 2-4. build unchanged ($VER, $CHANNEL): verifying the installed surface"
  else
    if [ "${R[state]}" = converted ] && [ "$live_ok" = 0 ]; then say "  note: $RS_PREFIX is incomplete: fetching the build again"; fi
    PHASE=fetch
    say "== 2. fetch + verify"
    fetch_and_verify
    maybe_fail_at 2
    say "== 3. stage $STAGING + smoke-exec"
    stage_and_smoke
    maybe_fail_at 3
    PHASE=install
    say "== 4. swap $RS_PREFIX"
    swap_tree
    R[version]=$VER; R[channel]=$CHANNEL; R[sha256]=$SHA; R[source]=$SOURCE
    rec_save
    maybe_fail_at 4
  fi
  PHASE=install

  # ---- step 5: wrapper (update_plan::WRAPPER_BODY) ----
  say "== 5. wrapper $WRAPPER"
  put_body "$WRAPPER" 0755 rs_wrapper_body || true
  maybe_fail_at 5

  # ---- step 6: units; the shipped unit byte-equal, WITH Conflicts= (§4) ----
  say "== 6. units"
  if put_file "$RS_UNIT" 0644 "$RS_PREFIX/systemd/xorgate-agent-rs.service"; then NEED_RESTART=1; fi
  if [ -f "$JS_USER_CONF" ]; then
    # Byte copy of the JS drop-in: the same service user (§4).
    ensure_dir "$RS_UNIT_D"
    if put_file "$RS_USER_CONF" 0644 "$JS_USER_CONF"; then NEED_RESTART=1; fi
  else
    say "  = no $JS_USER_CONF (the JS agent runs as root): no copy, $RS_SVC runs as root"
  fi
  if [ -d "$RS_UNIT_D" ]; then
    for m in "$RS_UNIT_D"/*; do
      if [ -e "$m" ] && { [ "$m" != "$RS_USER_CONF" ] || [ ! -f "$JS_USER_CONF" ]; }; then
        say "  foreign drop-in kept (not installed by install-rs.sh): $m"
      fi
    done
  fi
  put_file "$UPD_UNIT" 0644 "$RS_PREFIX/systemd/xorgate-updater-rs.service" || true
  ensure_dir "$UPD_UNIT_D"
  put_body "$DIST_DROPIN" 0644 rs_dist_dropin_body || true    # §8 / CF63
  for m in "$UPD_UNIT_D"/*; do
    if [ -e "$m" ] && [ "$m" != "$DIST_DROPIN" ]; then say "  foreign drop-in kept (not installed by install-rs.sh): $m"; fi
  done
  js_pin=$(env_file_get "$AGENT_ENV" DEVICE_DIST_VERSION || true)
  if [ -n "$js_pin" ] && [ "$js_pin" != latest ]; then    # Q11
    say "WARNING: $AGENT_ENV pins DEVICE_DIST_VERSION=$js_pin (the JS agent's build); the Rust updater follows 'latest' through xorgate-updater-rs.service.d/10-dist.conf — a hand-run 'sudo xorgate-rs update' would request xorgate-agent-rs-$js_pin (use --tag latest)"
  fi
  maybe_fail_at 6

  # ---- step 7: sudoers 024, visudo -cf BEFORE it lands; a refusal warns ----
  say "== 7. sudoers $SUDOERS_024"
  user=$(service_user_of "$RS_USER_CONF" || service_user_of "$JS_USER_CONF" || true)   # update.rs service_user()
  if [ -z "$user" ] || [ "$user" = root ]; then
    say "  = service user '${user:-root}' needs no grant: no $SUDOERS_024"
  else
    t=$(mktemp -d)
    rs_update_sudoers_body "$user" > "$t/024_xorgate-update-rs"
    chmod 0440 "$t/024_xorgate-update-rs"
    if [ -f "$SUDOERS_024" ] && [ ! -L "$SUDOERS_024" ] && cmp -s "$t/024_xorgate-update-rs" "$SUDOERS_024" \
       && [ "$(stat -c '%a %u:%g' "$SUDOERS_024")" = "440 0:0" ]; then
      say "  = $SUDOERS_024 unchanged"
    elif command -v visudo >/dev/null 2>&1 && visudo -cf "$t/024_xorgate-update-rs" >/dev/null 2>&1; then
      put_file "$SUDOERS_024" 0440 "$t/024_xorgate-update-rs" || true
    else
      say "WARNING: update sudoers drop-in failed visudo validation; remote updates will not work"
    fi
    rm -rf "$t"
  fi
  maybe_fail_at 7

  # ---- step 8: hand over the identity (§4: JS stopped by Conflicts=, disabled) ----
  PHASE=start
  say "== 8. systemctl"
  systemctl daemon-reload || abort_convert "systemctl daemon-reload failed"
  if [ "$(unit_enabled "$JS_SVC")" = enabled ]; then
    systemctl disable "$JS_SVC" >/dev/null 2>&1 || abort_convert "systemctl disable $JS_SVC failed"
    note "$JS_SVC disabled"
  fi
  if [ "$(unit_enabled "$RS_SVC")" != enabled ]; then
    if [ ! -e "$WANTS/$RS_SVC.service" ] && [ ! -L "$WANTS/$RS_SVC.service" ]; then created "$WANTS/$RS_SVC.service"; fi
    systemctl enable "$RS_SVC" >/dev/null 2>&1 || abort_convert "systemctl enable $RS_SVC failed"
    note "$RS_SVC enabled"
  fi
  if [ "$(unit_active "$RS_SVC")" = active ]; then
    if [ "$NEED_RESTART" = 1 ]; then
      timeout 100 systemctl restart "$RS_SVC" || abort_convert "systemctl restart $RS_SVC failed"
      note "$RS_SVC restarted (new build or unit)"
    fi
  else
    timeout 100 systemctl start "$RS_SVC" || abort_convert "systemctl start $RS_SVC failed"
    note "$RS_SVC started"
  fi
  maybe_fail_at 8

  # ---- step 9: ≤ 90 s for rs active + JS inactive, else auto-revert (§5.5) ----
  say "== 9. verify"
  deadline=$((SECONDS + VERIFY_SECS))
  while :; do
    ra=$(unit_active "$RS_SVC"); ja=$(unit_active "$JS_SVC")
    if [ "$ra" = active ] && { [ "$ja" = inactive ] || [ "$ja" = failed ]; }; then break; fi
    [ "$SECONDS" -lt "$deadline" ] || abort_convert "after ${VERIFY_SECS}s $RS_SVC is '$ra' and $JS_SVC is '$ja' (want active + inactive)"
    sleep 2
  done
  say "  $RS_SVC active; $JS_SVC $ja"
  maybe_fail_at 9
  [ "${R[state]}" = converted ] || first=1
  R[state]=converted
  [ -n "${R[convertedAt]}" ] || R[convertedAt]=$(utc)
  R[version]=$VER; R[channel]=$CHANNEL
  rec_save
  rm -rf "$BACKUP"
  [ -z "$DL" ] || rm -rf "$DL"
  PHASE=none
  summary
  if [ "$first" = 1 ] || [ "$SWAPPED" = 1 ]; then
    say "converted: xorgate-agent-rs $VER ($CHANNEL) active; xorgate-agent stopped + disabled. revert with: sudo /opt/xorgate-rs/install-rs.sh --revert"
  else
    say "already converted (${R[convertedAt]}, $VER); install verified, changed (${#CHANGES[@]}). change the build with: sudo systemctl start xorgate-updater-rs"
  fi
  finish 0
}

# ---------------------------------------------------------------------------
# Revert (§4 "--revert" / "--revert --purge"; §5.8: no network, no Rust binary).
PURGE=0; FORCE=0; REVERT_FINAL=1
revert_main() {
  local recf pref already=0 ts dest p d en mode=keep files=kept rest ok=yes
  local cands=() dirs=()
  if [ -f "$REC" ]; then recf=$REC; pref=$PRE
  else
    recf=$(latest_reverted)
    if [ -z "$recf" ]; then
      say "nothing to revert: no conversion record in $REC_DIR (state=not-converted)"
      finish 0
    fi
    pref=${recf/record.reverted-/pre.manifest.reverted-}
    already=1
  fi
  rec_load "$recf"
  if [ "$PURGE" = 1 ] && [ "${R[rsPreexisting]}" = yes ] && [ "$FORCE" = 0 ]; then   # Q12
    refuse "--purge: a Rust install predated this conversion (rsPreexisting=yes in $recf); use keep mode (plain --revert), or --force to delete only what this conversion created"
  fi

  # 1. the Rust agent off (idempotent).
  if [ "$already" = 1 ]; then say "already reverted ($(basename -- "$recf")); the JS agent is left as it is"
  else say "== reverting the conversion started ${R[startedAt]} (state=${R[state]})"; fi
  if [ "$(unit_enabled "$RS_SVC")" = enabled ]; then
    systemctl disable "$RS_SVC" >/dev/null 2>&1 || say "  ! systemctl disable $RS_SVC failed"
    note "$RS_SVC disabled"
  fi
  case $(unit_active "$RS_SVC") in
    inactive | failed | unknown | "") ;;
    *) systemctl stop "$RS_SVC" >/dev/null 2>&1 || say "  ! systemctl stop $RS_SVC failed"; note "$RS_SVC stopped" ;;
  esac
  if [ -e "$STAGING" ]; then rm -rf "$STAGING"; note "$STAGING (an incomplete staging) removed"; fi

  if [ "$already" = 0 ]; then
    # 2. the JS enablement exactly as recorded.
    en=$(unit_enabled "$JS_SVC")
    case ${R[jsEnabled]} in
      enabled)
        if [ "$en" != enabled ]; then
          systemctl enable "$JS_SVC" >/dev/null 2>&1 || say "  ! systemctl enable $JS_SVC failed"
          note "$JS_SVC enabled (as recorded)"
        fi ;;
      disabled)
        if [ "$en" = enabled ]; then
          systemctl disable "$JS_SVC" >/dev/null 2>&1 || say "  ! systemctl disable $JS_SVC failed"
          note "$JS_SVC disabled (as recorded)"
        fi ;;
      *) say "  = recorded jsEnabled='${R[jsEnabled]}': enablement left as it is ($en)" ;;
    esac
    # 3. JS started iff it was enabled OR active (§4: an enabled JS agent is meant to run).
    if [ "${R[jsEnabled]}" = enabled ] || [ "${R[jsActive]}" = active ]; then
      if [ "$(unit_active "$JS_SVC")" != active ]; then
        timeout 100 systemctl start "$JS_SVC" || say "  ! systemctl start $JS_SVC failed"
        note "$JS_SVC started"
      fi
    fi
  fi

  # 4. --purge: delete what this conversion installed, never anything else.
  if [ "$PURGE" = 1 ]; then
    files=purged
    [ "${R[rsPreexisting]}" = yes ] || mode=purge
    say "== purge (rsPreexisting=${R[rsPreexisting]})"
    # shellcheck disable=SC2206  # `installed` is a space-separated path list (I2)
    cands=(${R[installed]})
    if [ "${R[rsPreexisting]}" != yes ]; then
      cands+=("$WANTS/$RS_SVC.service" "$SUDOERS_024" "$DIST_DROPIN" "$UPD_UNIT" "$RS_USER_CONF" "$RS_UNIT" "$WRAPPER"
              "$BACKUP" "$STAGING" "$RS_PREFIX" "$UPD_UNIT_D" "$RS_UNIT_D")
    fi
    for p in ${cands[@]+"${cands[@]}"}; do
      if [ ! -e "$p" ] && [ ! -L "$p" ]; then continue; fi
      if ! is_rust_owned "$p"; then say "  ! $p is not a Rust-owned path: not deleted"; continue; fi
      case " ${R[installed]} " in
        *" $p "*) ;;
        *) if in_manifest "$p" "$pref"; then say "  = $p predates the conversion: kept"; continue; fi ;;
      esac
      case $p in
        "$RS_PREFIX" | "$STAGING" | "$BACKUP") rm -rf "$p"; note "$p removed" ;;
        *) if [ -d "$p" ] && [ ! -L "$p" ]; then dirs+=("$p"); else rm -f "$p"; note "$p removed"; fi ;;
      esac
    done
    for d in ${dirs[@]+"${dirs[@]}"}; do
      if rmdir "$d" 2>/dev/null; then note "$d/ removed"
      else
        for rest in "$d"/*; do
          if [ -e "$rest" ]; then say "  foreign drop-in kept (not installed by install-rs.sh): $rest"; fi
        done
      fi
    done
    systemctl daemon-reload || say "  ! systemctl daemon-reload failed"
  fi

  # 5. self-check against pre.manifest: the JS-owned rows (keep), or every
  #    row but the record dir (purge). The e2e checks the same independently.
  if [ -f "$pref" ]; then
    say "== self-check against $pref ($mode rows)"
    selfcheck "$pref" "$mode"
    [ "$DIFFS" = 0 ] || ok="NO ($DIFFS differences, listed above)"
  else
    say "  - $pref missing: nothing to compare against"
    ok="NO (1 differences, listed above)"
  fi

  # 6. the record (I2): renamed in keep mode; the whole directory last on purge.
  if [ "$PURGE" = 1 ]; then
    rm -rf "$REC_DIR"; note "$REC_DIR removed"
  elif [ "$already" = 0 ]; then
    ts=$(date -u +%Y%m%dT%H%M%SZ)
    while [ -e "$REC_DIR/record.reverted-$ts" ]; do ts="${ts}x"; done
    dest="$REC_DIR/record.reverted-$ts"
    mv -f "$REC" "$dest"
    sed -i 's/^state=.*/state=reverted/' "$dest"
    if [ -f "$PRE" ]; then mv -f "$PRE" "$REC_DIR/pre.manifest.reverted-$ts"; fi
    note "record moved to $dest"
  fi
  [ "$REVERT_FINAL" = 1 ] || return 0
  summary
  say "reverted: xorgate-agent $(unit_enabled "$JS_SVC") + $(unit_active "$JS_SVC"); xorgate-agent-rs disabled (files $files). JS install matches the pre-conversion record: $ok"
  case $ok in yes) finish 0 ;; *) finish 1 ;; esac
}

# ---------------------------------------------------------------------------
# --status: plain key=value lines (machine-read; the one output without the
# [install-rs] prefix, so that its first line is exactly state=…).
status_main() {
  local st k
  st=$(rec_state)
  echo "state=$st"
  if [ "$st" = unknown ]; then echo "(cannot read $REC_DIR: run with sudo)"; fi
  if [ -f "$REC" ]; then echo "--- record $REC"; cat "$REC"
  elif [ "$st" = reverted ]; then k=$(latest_reverted); echo "--- record $k"; cat "$k"; fi
  echo "--- units"
  for k in "$JS_SVC" "$RS_SVC"; do echo "$k: is-enabled=$(unit_enabled "$k") is-active=$(unit_active "$k")"; done
  echo "--- build $RS_PREFIX/build-info.json"
  if [ -r "$RS_PREFIX/build-info.json" ]; then tr -d '\n' < "$RS_PREFIX/build-info.json"; echo; else echo "(none)"; fi
  finish 0
}

# ---------------------------------------------------------------------------
main() {
  local seen=""
  ORIG_ARGS=("$@")
  MODE=convert
  while [ $# -gt 0 ]; do
    case $1 in
      --from) { [ $# -ge 2 ] && [ -n "$2" ]; } || usage_error "--from needs a URL"; FROM=$2; shift 2 ;;
      --tag) { [ $# -ge 2 ] && [ -n "$2" ]; } || usage_error "--tag needs a version"; TAG=$2; shift 2 ;;
      --tarball) { [ $# -ge 2 ] && [ -n "$2" ]; } || usage_error "--tarball needs a file"; TARBALL=$2; shift 2 ;;
      --reinstall) REINSTALL=1; shift ;;
      -y | --yes) YES=1; shift ;;
      --revert | --status)
        [ -z "$seen" ] || usage_error "$seen and $1 are exclusive"
        seen=$1; MODE=${1#--}; shift ;;
      --purge) PURGE=1; shift ;;
      --force) FORCE=1; shift ;;
      -h | --help) usage; finish 0 ;;
      *) usage_error "unknown argument: $1" ;;
    esac
  done
  if [ "$PURGE" = 1 ] && [ "$MODE" != revert ]; then usage_error "--purge only goes with --revert"; fi
  if [ "$FORCE" = 1 ] && [ "$PURGE" = 0 ]; then usage_error "--force only goes with --revert --purge"; fi
  if [ "$MODE" != convert ] && [ -n "$FROM$TAG$TARBALL" ]; then usage_error "--from/--tag/--tarball only go with a convert"; fi
  if [ "$MODE" != convert ] && [ "$REINSTALL$YES" != 00 ]; then usage_error "--reinstall/-y only go with a convert"; fi
  case $MODE in
    status) status_main ;;
    revert)
      require_root
      if [ "$PURGE" = 1 ]; then reexec_private; fi
      lock
      revert_main ;;
    convert)
      case ${BASH_SOURCE[0]} in "$RS_PREFIX"/* | "$STAGING"/* | "$BACKUP"/*) reexec_private ;; esac
      convert_main ;;
  esac
}

# Sourcing (scripts/tests/install-rs-bodies.sh) defines the functions only.
if [ "${BASH_SOURCE[0]}" = "$0" ] || [ -n "${XORGATE_RS_INSTALL_REEXEC:-}" ]; then
  trap 'on_exit $?' EXIT
  main "$@"
fi
