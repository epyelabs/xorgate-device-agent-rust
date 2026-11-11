#!/usr/bin/env bash
# Homelab execution helper (Phase 7, 2026-11-10): run this plan's heavy Docker
# work (cargo gates, rig builds, replay-rig lanes) on the homelab
# `ai@192.168.8.107` (x86_64, 32 cores) instead of the Mac's Docker VM.
# Runs on the Mac (bash 3.2) or any Linux box with ssh + rsync + git.
#
#   scripts/remote/hl.sh sync [--force] <tag> [worktree]  ship a tree (git's file set) to ~/xorgate-rs-p7/<tag>/src/
#   scripts/remote/hl.sh run <tag> <job> [args]     start a detached job (tmux on the homelab)
#   scripts/remote/hl.sh wait <tag> <name> [secs]   block until the job's .rc exists (default 3600 s cap)
#   scripts/remote/hl.sh stop <tag> [name]          stop a job properly (rig process group first, then the session)
#   scripts/remote/hl.sh status [tag]               jobs, rc files, containers, load, memory
#   scripts/remote/hl.sh fetch <tag> <dir> [--runs] copy logs/ + out/ (+ rig run dirs) back (sha re-checked)
#   scripts/remote/hl.sh push <tag> <file> [inbox|tapes]  copy a file to <tag>/inbox/ or the tree's rig tapes dir
#   scripts/remote/hl.sh js-cache <tag> [ref]       build the JS agent @ref (default 09da38d) for the amd64 rig
#   scripts/remote/hl.sh preflight                  homelab load / memory / labelled containers (rc 1 if loaded)
#   scripts/remote/hl.sh clean <tag>                remove ONLY that tag: containers, /target/<tag>, its dir
#
# Every step exits non-zero WITH a message (an ERR trap names the failing line).
# `sync` ends with `synced <tag>: … tree_fp=…`; a job only starts on a tag whose
# last sync completed and was verified (`verified.fp` = source.txt's tree_fp).
#
# Jobs (`run`; the job's log is ~/xorgate-rs-p7/<tag>/logs/<name>.log, its exit
# code <name>.rc; <name> = the job, or $HL_JOB_NAME). A refused start writes
# <name>.rc = 75 and <name>.refused (the reason), so `wait` never reports an
# older run's rc:
#   gate         cargo-gate + suites + systemd, in that order
#   cargo-gate   fmt --check, clippy -D warnings, test --workspace, cargo-deny, clippy-ban-probe
#   test         cargo test --locked --workspace --no-fail-fast only
#   cargo ARGS   any cargo command line, e.g. `cargo test -p xorgate-agent --test capture_host`
#   suites       every plain script suite, one fresh bookworm container each
#                (HL_SUITES=<paths> for a subset; HL_MUTANTS=<ids> for scripts/remote/mutations.sh)
#   systemd      the two systemd-252-as-PID-1 suites (privileged bookworm; refused if a
#                suite would run a clock/RTC-setting tool)
#   build-bins   the rig's release xorgate-rs + xorgate-replay + rig-statvfs.so (tools/replay/build-bins.sh's
#                cargo line, amd64) -> the tree's tools/replay/.cache/bin and out/bins/ (+ SHA256SUMS,
#                HEAD with tree_fp/dirty); the old bins are removed FIRST
#   release      cargo build --release (amd64; the simulator image's agent) -> out/bins/release/
#   import-bins FROMTAG [a|b]   copy FROMTAG's out/bins into this tree's .cache/bin (sha-checked, and
#                only if FROMTAG's bins were built from FROMTAG's current verified tree);
#                with a|b: as xorgate-rs-a / xorgate-rs-b, the sides of an `ab` lane
#   import-tapes FROMTAG NAME...   copy FROMTAG's rig tapes NAME.ndjson.gz (+ .cli.ndjson) into this tree
#   synth NAME...   synthesise rig tapes with this tree's xorgate-replay (out/tapes.sha256)
#   lanes FILE   rig lanes, serial: FILE (a local file, pushed first) holds `name tape mode [purge]` lines,
#                mode = cal | jsrs | rsrs | ab. Refused unless .cache/bin was built from (or imported
#                for) THIS tree_fp. Results go to out/lanes/<tree_fp12>-<bins sha12>/ (q.tsv, runs/,
#                untol/), so a re-sync or a rebuild never reuses another tree's rows
#   images       ensure the amd64 build image (scripts/docker-build.sh's recipe), the script-tests
#                image and cargo-deny (x86_64 musl, sha-checked)
#   pack         refused: the device tarball is aarch64 (see the handbook's HOMELAB EXECUTION)
#
# Caps: every container runs with --cpus "$HL_CPUS" (16) --memory "$HL_MEM" (32g); rig lanes
# with HL_RIG_CPUS (8; use 7 for concurrent lanes) / HL_RIG_MEM (8g); suites with
# HL_SUITE_CPUS (4) / HL_SUITE_MEM (4g); import-* 1 CPU / 1g; js-cache HL_JS_CPUS (4) / HL_JS_MEM (8g).
# A job is refused when the helper's running jobs would exceed HL_TOTAL_CPUS (24) or leave
# less than HL_MIN_FREE_GB (16) GiB available; the check + reservation run under a lock,
# and those two limits can only be made stricter. Other knobs: scripts/remote/hl-lib.sh.
# Every container carries --label xorgate.agent=<tag> (plus xorgate.hl=1) and builds into
# /target/<tag> of volume xorgate-rs-p7-target. Nothing here touches a container it did not start.
set -Eeuo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/remote/hl-lib.sh
. "$HERE/hl-lib.sh"
SSH=${HL_SSH:-ssh}
RSYNC=${HL_RSYNC:-rsync}
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4)
# Review F1: no step may fail silently.
trap 'echo "hl: FAILED (rc $?) at line $LINENO: $BASH_COMMAND" >&2' ERR

usage() { sed -n '2,57p' "$0"; exit 2; }
die() { echo "hl: $*" >&2; if [ "${STATE_PENDING_OK:-1}" = 1 ] && [ -n "${STATEF:-}" ] && grep -q '^state=pending$' "$STATEF" 2>/dev/null; then set_state refused-local "$*"; fi; exit 2; }
rsh() { "$SSH" "${SSH_OPTS[@]}" "$HL_HOST" "$@"; }
# stdin -> its sha256 (64 hex).
sha256_stdin() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -c1-64; }
sha256_check() { if command -v sha256sum >/dev/null 2>&1; then sha256sum -c --quiet "$@"; else shasum -a 256 -c --quiet "$@"; fi; }
# NUL-separated paths on stdin -> `sha256sum -c` lines (the remote checks them).
hash_files0() { if command -v sha256sum >/dev/null 2>&1; then xargs -0 sha256sum; else xargs -0 shasum -a 256; fi; }
need_tag() { hl_valid_tag "${1:-}" || die "bad tag '${1:-}' ([a-z0-9-], max 40; reserved: $HL_RESERVED_TAGS)"; }
# The remote job runner of a tag (or the shared copy for tag-less commands).
# shellcheck disable=SC2016  # $HOME is the REMOTE shell's
job_sh() { printf '"$HOME"/%s/%s/bin/hl-job.sh' "$HL_ROOT_REL" "$1"; }
# Forwarded environment, shell-quoted, for the remote command line.
fwd_env() {
  local v out="" isset
  for v in $HL_FORWARD_VARS; do
    # bash 3.2: no ${!v+x}; set-but-empty is forwarded too. (An assignment, not
    # an `if eval [ … ]`: bash 3.2 fires the ERR trap inside eval in conditions.)
    eval "isset=\${$v+set}"
    if [ "$isset" = set ]; then out="$out $v=$(eval "printf '%q' \"\$$v\"")"; fi
  done
  printf 'env%s' "$out"
}
# The worktree's identity, read twice (before and after hashing: review F13).
tree_state() { # <worktree>
  printf '%s %s ' "$(git -C "$1" rev-parse HEAD)" "$(git -C "$1" status --porcelain --untracked-files=all | wc -l | tr -d ' ')"
  git -C "$1" status --porcelain --untracked-files=all | sha256_stdin
}

cmd_sync() {
  local force=0
  if [ "${1:-}" = --force ]; then force=1; shift; fi
  need_tag "${1:-}"
  local tag=$1 w=${2:-} tmp head branch dirty diffsha n fp st0 st1
  [ -n "$w" ] || w=$(git rev-parse --show-toplevel)
  w=$(cd "$w" && pwd)
  git -C "$w" rev-parse --git-dir >/dev/null 2>&1 || die "$w is not a git worktree"
  tmp=$(mktemp -d)
  # shellcheck disable=SC2064  # expand $tmp now
  trap "rm -rf '$tmp'" RETURN
  st0=$(tree_state "$w")
  head=$(git -C "$w" rev-parse HEAD)
  branch=$(git -C "$w" rev-parse --abbrev-ref HEAD)
  dirty=$(git -C "$w" status --porcelain --untracked-files=all | wc -l | tr -d ' ')
  diffsha=$(git -C "$w" diff HEAD --binary | sha256_stdin)
  # git's own file set: tracked + untracked-but-not-ignored, files that exist,
  # never target/ (a stray host build) or .git.
  (cd "$w" && git ls-files -co --exclude-standard) | LC_ALL=C sort -u | while IFS= read -r f; do
    case "$f" in target/*|.git|.git/*) continue ;; esac
    if [ -f "$w/$f" ] || [ -L "$w/$f" ]; then printf '%s\n' "$f"; fi
  done > "$tmp/manifest.txt"
  if grep -q '[[:cntrl:]]' "$tmp/manifest.txt"; then die "a path with a control character: refusing"; fi
  n=$(wc -l < "$tmp/manifest.txt" | tr -d ' ')
  [ "$n" -gt 0 ] || die "empty file list from $w"
  (cd "$w" && tr '\n' '\0' < "$tmp/manifest.txt" | hash_files0) > "$tmp/manifest.sha256"
  fp=$(sha256_stdin < "$tmp/manifest.sha256")
  st1=$(tree_state "$w")
  [ "$st0" = "$st1" ] || die "the worktree changed while it was being hashed (HEAD or status moved): sync again"
  {
    echo "tag=$tag"
    echo "worktree=$w"
    echo "head=$head"
    echo "branch=$branch"
    echo "dirty_entries=$dirty"
    echo "diff_vs_head_sha256=$diffsha"
    echo "files=$n"
    echo "tree_fp=$fp"
    echo "synced_at=$(date -u +%FT%TZ)"
    echo "helper=$(git -C "$HERE" rev-parse HEAD 2>/dev/null || echo unknown)$([ -z "$(git -C "$HERE" status --porcelain -- . 2>/dev/null)" ] || echo ' (+ uncommitted helper changes)')"
  } > "$tmp/source.txt.new"
  # 1. the tag dir (refused if it exists and is not the helper's, or while a job
  #    of the tag runs: review F8); the verified marker is invalidated FIRST, so
  #    a sync that stops anywhere below leaves the tag unstartable (review F1).
  rsh "bash -s" "$HL_ROOT_REL" "$tag" "$force" <<'EOF'
set -eu
R="$HOME/$1"; T="$R/$2"
mkdir -p "$R/_shared/bin" "$R/_shared/res"
if [ -e "$T" ] && [ ! -f "$T/.hl-tag" ]; then echo "hl: $T exists and is not a helper tag dir: refusing" >&2; exit 3; fi
# Under the reservation lock (fix-check M4/P1): a `run` checks verified.fp under
# the same lock, so it either started before this point (and the session check
# below refuses the sync) or sees verified.fp gone (and refuses itself).
exec 9>>"$R/_shared/res/.lock"
flock -w 60 9 || { echo "hl: could not take the reservation lock: refusing to sync" >&2; exit 3; }
live=$(tmux ls -F '#{session_name}' 2>/dev/null | grep "^hl_$2_" || true)
if [ -n "$live" ] && [ "$3" != 1 ]; then echo "hl: job(s) of $2 running ($(echo $live)): refusing to change its tree (stop them, or sync --force)" >&2; exit 3; fi
mkdir -p "$T/src" "$T/bin" "$T/logs" "$T/out" "$T/inbox"
[ -f "$T/.hl-tag" ] || echo "$2 $(date -u +%FT%TZ)" > "$T/.hl-tag"
rm -f "$T/verified.fp" "$T/source.txt.new" "$T/ignored.txt"
EOF
  # 2. the helper's remote side (from THIS checkout, so any tree can be run).
  "$RSYNC" -a --delete -e "$SSH ${SSH_OPTS[*]}" "$HERE/" "$HL_HOST:$HL_ROOT_REL/$tag/bin/"
  "$RSYNC" -a --delete -e "$SSH ${SSH_OPTS[*]}" "$HERE/" "$HL_HOST:$HL_ROOT_REL/_shared/bin/"
  # 3. the tree, then the record (as source.txt.new: verify-src installs it).
  "$RSYNC" -a --files-from="$tmp/manifest.txt" -e "$SSH ${SSH_OPTS[*]}" "$w/" "$HL_HOST:$HL_ROOT_REL/$tag/src/"
  "$RSYNC" -a -e "$SSH ${SSH_OPTS[*]}" "$tmp/manifest.txt" "$tmp/manifest.sha256" "$tmp/source.txt.new" "$HL_HOST:$HL_ROOT_REL/$tag/"
  # 4. prune files the tree no longer has (git-ignored ones = generated caches stay).
  rsh "$(job_sh "$tag") list-src $tag" > "$tmp/remote.txt"
  LC_ALL=C comm -13 "$tmp/manifest.txt" <(LC_ALL=C sort -u "$tmp/remote.txt") > "$tmp/extra.txt"
  : > "$tmp/stale.txt"
  if [ -s "$tmp/extra.txt" ]; then
    # check-ignore exits 1 when NOTHING is ignored: that is a result, not an
    # error (review F1); 128 (a real error) still fails the sync.
    { (cd "$w" && git check-ignore --no-index --stdin < "$tmp/extra.txt") || [ $? -eq 1 ]; } | LC_ALL=C sort -u > "$tmp/ignored.txt"
    LC_ALL=C comm -23 "$tmp/extra.txt" "$tmp/ignored.txt" > "$tmp/stale.txt"
  fi
  # verify-src accepts no file outside the manifest + this ignore list (M4/P2).
  [ -f "$tmp/ignored.txt" ] || : > "$tmp/ignored.txt"
  "$RSYNC" -a -e "$SSH ${SSH_OPTS[*]}" "$tmp/ignored.txt" "$HL_HOST:$HL_ROOT_REL/$tag/"
  if [ -s "$tmp/stale.txt" ]; then
    echo "hl: pruning $(wc -l < "$tmp/stale.txt" | tr -d ' ') stale file(s) from $tag/src" >&2
    rsh "$(job_sh "$tag") prune-src $tag" < "$tmp/stale.txt"
  fi
  # 5. verify every shipped byte; only then is the record installed + marked verified.
  rsh "$(job_sh "$tag") verify-src $tag"
  echo "synced $tag: $head ($branch) dirty_entries=$dirty files=$n tree_fp=${fp:0:12}"
}

# Fix-check S1: every `run` gets a run id. A local record
# ($HL_STATE_DIR/<host>/<tag>.<name>) says whether the run reached the homelab;
# the homelab ties <name>.rc to the run id (<name>.rcid). `wait` reports an rc
# only for THIS run id, and a run refused on the Mac or lost to ssh as such.
state_file() { printf '%s/%s/%s.%s' "${HL_STATE_DIR:-$HOME/.cache/xorgate-hl}" "$(printf '%s' "$HL_HOST" | tr -c 'A-Za-z0-9._@-' _)" "$1" "$2"; }
set_state() { # <state> [msg]
  [ -n "${STATEF:-}" ] || return 0
  mkdir -p "$(dirname "$STATEF")"
  printf 'runid=%s\nstate=%s\nmsg=%s\n' "$RUNID" "$1" "$(printf '%s' "${2:-}" | tr '\n' ' ' | cut -c1-300)" > "$STATEF"
}

cmd_run() {
  need_tag "${1:-}"; [ $# -ge 2 ] || usage
  local tag=$1 job=$2 a q="" name rc out
  shift 2
  # Review F12: the job word is checked here, before any remote shell sees it.
  hl_known_job "$job" || { [ "$job" = pack ] && die "pack is aarch64-only (the device tarball): build it on the Mac"; die "unknown job '$job' (one of: $HL_JOBS_ALLOWED)"; }
  if [ -n "${HL_JOB_NAME:-}" ]; then hl_valid_job "$HL_JOB_NAME" || die "bad HL_JOB_NAME '$HL_JOB_NAME'"; fi
  name=${HL_JOB_NAME:-$job}
  RUNID="$(date -u +%Y%m%dT%H%M%SZ)-$$-${RANDOM}"
  STATEF=$(state_file "$tag" "$name")
  set_state pending
  if [ "$job" = lanes ]; then
    if [ $# -ne 1 ] || [ ! -f "$1" ]; then die "lanes needs one local items file"; fi
    cmd_push "$tag" "$1" inbox >/dev/null
    set -- "inbox/$(basename "$1")"
  fi
  for a in "$@"; do q="$q $(printf '%q' "$a")"; done
  if out=$(rsh "$(fwd_env) HL_RUN_ID=$RUNID $(job_sh "$tag") start $tag $(printf '%q' "$job")$q" 2>&1); then rc=0; else rc=$?; fi
  [ -z "$out" ] || printf '%s\n' "$out"
  case $rc in
    0) set_state started ;;
    255) set_state start-failed "ssh failed (rc 255): $(printf '%s' "$out" | tail -1)"; exit 255 ;;
    *) set_state refused-remote "$(printf '%s' "$out" | tail -1)"; exit "$rc" ;;
  esac
}

cmd_wait() {
  need_tag "${1:-}"; [ $# -ge 2 ] || usage
  local tag=$1 name=$2 max=${3:-3600} t0 out left sf rid="" st="" msg=""
  hl_valid_job "$name" || die "bad job name $name"
  sf=$(state_file "$tag" "$name")
  if [ -f "$sf" ]; then
    rid=$(sed -n 's/^runid=//p' "$sf"); st=$(sed -n 's/^state=//p' "$sf"); msg=$(sed -n 's/^msg=//p' "$sf")
    case "$st" in
      refused-local) echo "$name rc=75 (REFUSED on the Mac before the remote start: $msg)"; return 0 ;;
      start-failed) echo "$name rc=75 (NOT STARTED: $msg)"; return 0 ;;
    esac
  fi
  t0=$(date +%s)
  while :; do
    # (a poll that finds no .rc yet is not a failure: no ERR trap in here)
    if out=$(trap - ERR; rsh "cd $HL_ROOT_REL/$tag/logs && test -f $name.rc && printf '%s|%s|%s' \"\$(cat $name.rc)\" \"\$(cat $name.rcid 2>/dev/null)\" \"\$(cat $name.refused 2>/dev/null)\""); then
      local rrc=${out%%|*} rest=${out#*|}; local rcid=${rest%%|*} refused=${rest#*|}
      # With a local run id, only THIS run's rc counts (fix-check S1).
      if [ -z "$rid" ] || [ "$rcid" = "$rid" ]; then
        if [ -n "$refused" ]; then echo "$name rc=$rrc (REFUSED: $refused)"; else echo "$name rc=$rrc"; fi
        return 0
      fi
    fi
    if [ "$st" = refused-remote ]; then echo "$name rc=75 (REFUSED: $msg)"; return 0; fi
    left=$(( max - ($(date +%s) - t0) ))
    [ "$left" -gt 0 ] || { echo "hl: $tag/$name still running after ${max}s${rid:+ (run id $rid)}" >&2; exit 124; }
    sleep $(( left < 30 ? left : 30 ))
  done
}

cmd_stop() {
  need_tag "${1:-}"
  if [ -n "${2:-}" ]; then hl_valid_job "$2" || die "bad job name $2"; fi
  rsh "$(fwd_env) $(job_sh "$1") stop $1 ${2:-}"
}

cmd_status() {
  if [ -n "${1:-}" ]; then need_tag "$1"; rsh "$(job_sh "$1") status $1"; else rsh "$(job_sh _shared) status"; fi
}

cmd_preflight() { rsh "$(fwd_env) $(job_sh _shared) preflight"; }

# Review F17: the fetch dir belongs to ONE tag (a `.hl-fetch` marker), is kept
# in step with the homelab (--delete inside it only), and every SHA256SUMS that
# came back is re-checked.
cmd_fetch() {
  need_tag "${1:-}"; [ $# -ge 2 ] || usage
  local tag=$1 dest=$2 runs=${3:-} ex=(--exclude=/runs/ --exclude=/lanes/*/runs/) s
  [ "$runs" = --runs ] && ex=()
  if [ -e "$dest" ] && [ -n "$(ls -A "$dest" 2>/dev/null)" ]; then
    [ "$(cat "$dest/.hl-fetch" 2>/dev/null)" = "$tag" ] || die "$dest is not empty and not a fetch dir of $tag: refusing (use a fresh dir)"
  fi
  mkdir -p "$dest/logs" "$dest/out"
  echo "$tag" > "$dest/.hl-fetch"
  "$RSYNC" -a -e "$SSH ${SSH_OPTS[*]}" "$HL_HOST:$HL_ROOT_REL/$tag/source.txt" "$dest/"
  "$RSYNC" -a --delete -e "$SSH ${SSH_OPTS[*]}" "$HL_HOST:$HL_ROOT_REL/$tag/logs/" "$dest/logs/"
  "$RSYNC" -a --delete ${ex[@]+"${ex[@]}"} -e "$SSH ${SSH_OPTS[*]}" "$HL_HOST:$HL_ROOT_REL/$tag/out/" "$dest/out/"
  for s in "$dest/out/bins/SHA256SUMS" "$dest/out/bins/release/SHA256SUMS"; do
    [ -f "$s" ] || continue
    (cd "$(dirname "$s")" && sha256_check SHA256SUMS) || die "fetched $(dirname "$s") does NOT match its SHA256SUMS"
    echo "sha256 OK: ${s#"$dest"/}"
  done
  echo "fetched $tag -> $dest$([ "$runs" = --runs ] && echo ' (with run dirs)')"
}

cmd_push() {
  need_tag "${1:-}"; [ $# -ge 2 ] || usage
  local tag=$1 f=$2 where=${3:-inbox} rel
  [ -f "$f" ] || die "no such file: $f"
  case "$where" in
    inbox) rel=inbox/ ;;
    tapes) rel=src/tools/replay/.cache/tapes/ ;;
    *) die "push destination must be inbox or tapes" ;;
  esac
  rsh "test -f $HL_ROOT_REL/$tag/.hl-tag && mkdir -p $HL_ROOT_REL/$tag/$rel" || die "$tag is not synced yet"
  "$RSYNC" -a -e "$SSH ${SSH_OPTS[*]}" "$f" "$HL_HOST:$HL_ROOT_REL/$tag/$rel"
  echo "pushed $(basename "$f") -> $tag/$rel"
}

cmd_js_cache() {
  need_tag "${1:-}"
  local tag=$1 ref=${2:-$HL_JS_REF} repo before after
  repo=${JS_REPO:-$(cd "$HERE/../../.." && pwd)/xorgate-device-agent}
  [ -e "$repo/.git" ] || die "no JS repo at $repo (set JS_REPO)"
  git -C "$repo" rev-parse --verify -q "$ref^{commit}" >/dev/null || die "no commit $ref in $repo"
  before=$(git -C "$repo" status --porcelain)
  git -C "$repo" archive "$ref" | rsh "$(fwd_env) $(job_sh "$tag") js-cache $tag $(printf '%q' "$ref")"
  after=$(git -C "$repo" status --porcelain)
  [ "$before" = "$after" ] || die "the JS repo's working tree changed during the archive: investigate"
}

cmd_clean() { need_tag "${1:-}"; rsh "$(job_sh "$1") clean $1"; }

[ $# -ge 1 ] || usage
c=$1; shift
case "$c" in
  sync) cmd_sync "$@" ;;
  run) cmd_run "$@" ;;
  wait) cmd_wait "$@" ;;
  stop) cmd_stop "$@" ;;
  status) cmd_status "$@" ;;
  preflight) cmd_preflight ;;
  fetch) cmd_fetch "$@" ;;
  push) cmd_push "$@" ;;
  js-cache) cmd_js_cache "$@" ;;
  clean) cmd_clean "$@" ;;
  -h|--help|help) usage ;;
  *) echo "hl: unknown command $c" >&2; usage ;;
esac
