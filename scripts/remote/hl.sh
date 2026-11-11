#!/usr/bin/env bash
# Homelab execution helper (Phase 7, 2026-11-10): run this plan's heavy Docker
# work (cargo gates, rig builds, replay-rig lanes) on the homelab
# `ai@192.168.8.107` (x86_64, 32 cores) instead of the Mac's Docker VM.
# Runs on the Mac (bash 3.2) or any Linux box with ssh + rsync + git.
#
#   scripts/remote/hl.sh sync <tag> [worktree]      ship a tree (git's file set) to ~/xorgate-rs-p7/<tag>/src/
#   scripts/remote/hl.sh run <tag> <job> [args]     start a detached job (tmux on the homelab)
#   scripts/remote/hl.sh wait <tag> <name> [secs]   block until the job's .rc exists (default 3600 s cap)
#   scripts/remote/hl.sh status [tag]               jobs, rc files, containers, load, memory
#   scripts/remote/hl.sh fetch <tag> <dir> [--runs] copy logs/ + out/ (+ rig run dirs) back
#   scripts/remote/hl.sh push <tag> <file> [inbox|tapes]  copy a file to <tag>/inbox/ or the tree's rig tapes dir
#   scripts/remote/hl.sh js-cache <tag> [ref]       build the JS agent @ref (default 09da38d) for the amd64 rig
#   scripts/remote/hl.sh preflight                  homelab load / memory / labelled containers (rc 1 if loaded)
#   scripts/remote/hl.sh clean <tag>                remove ONLY that tag: containers, /target/<tag>, its dir
#
# Jobs (`run`; the job's log is ~/xorgate-rs-p7/<tag>/logs/<name>.log, its exit
# code <name>.rc; <name> = the job, or $HL_JOB_NAME):
#   gate         cargo-gate + suites + systemd, in that order
#   cargo-gate   fmt --check, clippy -D warnings, test --workspace, cargo-deny, clippy-ban-probe
#   test         cargo test --locked --workspace --no-fail-fast only
#   cargo ARGS   any cargo command line, e.g. `cargo test -p xorgate-agent --test capture_host`
#   suites       every plain script suite, one fresh bookworm container each
#   systemd      the two systemd-252-as-PID-1 suites (privileged bookworm)
#   build-bins   the rig's release xorgate-rs + xorgate-replay + rig-statvfs.so (tools/replay/build-bins.sh's
#                cargo line, amd64) -> the tree's tools/replay/.cache/bin and out/bins/ (+ SHA256SUMS, HEAD)
#   release      cargo build --release (amd64; the simulator image's agent) -> out/bins/release/
#   import-bins FROMTAG [a|b]   copy FROMTAG's out/bins into this tree's .cache/bin (sha-checked);
#                with a|b: as xorgate-rs-a / xorgate-rs-b, the sides of an `ab` lane
#   import-tapes FROMTAG NAME...   copy FROMTAG's rig tapes NAME.ndjson.gz (+ .cli.ndjson) into this tree
#   synth NAME...   synthesise rig tapes with this tree's xorgate-replay (out/tapes.sha256)
#   lanes FILE   rig lanes, serial: FILE (a local file, pushed first) holds `name tape mode [purge]` lines,
#                mode = cal | jsrs | rsrs | ab. Quiet-host preflight before each run; out/q.tsv
#   images       ensure the amd64 build image (scripts/docker-build.sh's recipe), the script-tests
#                image and cargo-deny (x86_64 musl, sha-checked)
#   pack         refused: the device tarball is aarch64 (see the handbook's HOMELAB EXECUTION)
#
# Caps: every container runs with --cpus "$HL_CPUS" (16) --memory "$HL_MEM" (32g); rig lanes
# with HL_RIG_CPUS (8) / HL_RIG_MEM (8g); suites with HL_SUITE_CPUS (4) / HL_SUITE_MEM (4g).
# A job is refused when the helper's running jobs would exceed HL_TOTAL_CPUS (24) or leave
# less than HL_MIN_FREE_GB (16) GiB available. Other knobs: see scripts/remote/hl-lib.sh.
# Every container carries --label xorgate.agent=<tag> (plus xorgate.hl=1) and builds into
# /target/<tag> of volume xorgate-rs-p7-target. Nothing here touches a container it did not start.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/remote/hl-lib.sh
. "$HERE/hl-lib.sh"
SSH=${HL_SSH:-ssh}
RSYNC=${HL_RSYNC:-rsync}
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4)

usage() { sed -n '2,40p' "$0"; exit 2; }
die() { echo "hl: $*" >&2; exit 2; }
rsh() { "$SSH" "${SSH_OPTS[@]}" "$HL_HOST" "$@"; }
# stdin -> its sha256 (64 hex).
sha256_stdin() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -c1-64; }
# NUL-separated paths on stdin -> `sha256sum -c` lines (the remote checks them).
hash_files0() { if command -v sha256sum >/dev/null 2>&1; then xargs -0 sha256sum; else xargs -0 shasum -a 256; fi; }
need_tag() { hl_valid_tag "${1:-}" || die "bad tag '${1:-}' (lower-case letters, digits, '-'; max 40)"; }
# The remote job runner of a tag (or the shared copy for tag-less commands).
# shellcheck disable=SC2016  # $HOME is the REMOTE shell's
job_sh() { printf '"$HOME"/%s/%s/bin/hl-job.sh' "$HL_ROOT_REL" "$1"; }
# Forwarded environment, shell-quoted, for the remote command line.
fwd_env() {
  local v out=""
  for v in $HL_FORWARD_VARS; do
    # bash 3.2: no ${!v+x}; set-but-empty is forwarded too.
    if eval "[ \"\${$v+set}\" = set ]"; then out="$out $v=$(eval "printf '%q' \"\$$v\"")"; fi
  done
  printf 'env%s' "$out"
}

cmd_sync() {
  need_tag "${1:-}"
  local tag=$1 w=${2:-} tmp head branch dirty diffsha n fp
  [ -n "$w" ] || w=$(git rev-parse --show-toplevel)
  w=$(cd "$w" && pwd)
  git -C "$w" rev-parse --git-dir >/dev/null 2>&1 || die "$w is not a git worktree"
  tmp=$(mktemp -d)
  # shellcheck disable=SC2064  # expand $tmp now
  trap "rm -rf '$tmp'" RETURN
  head=$(git -C "$w" rev-parse HEAD)
  branch=$(git -C "$w" rev-parse --abbrev-ref HEAD)
  dirty=$(git -C "$w" status --porcelain | wc -l | tr -d ' ')
  diffsha=$(git -C "$w" diff HEAD --binary | sha256_stdin)
  # git's own file set: tracked + untracked-but-not-ignored, files that exist,
  # never target/ (a stray host build) or .git.
  (cd "$w" && git ls-files -co --exclude-standard) | LC_ALL=C sort -u | while IFS= read -r f; do
    case "$f" in target/*|.git|.git/*) continue ;; esac
    { [ -f "$w/$f" ] || [ -L "$w/$f" ]; } && printf '%s\n' "$f"
  done > "$tmp/manifest.txt"
  if grep -q '[[:cntrl:]]' "$tmp/manifest.txt"; then die "a path with a control character: refusing"; fi
  n=$(wc -l < "$tmp/manifest.txt" | tr -d ' ')
  [ "$n" -gt 0 ] || die "empty file list from $w"
  (cd "$w" && tr '\n' '\0' < "$tmp/manifest.txt" | hash_files0) > "$tmp/manifest.sha256"
  fp=$(sha256_stdin < "$tmp/manifest.sha256")
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
  } > "$tmp/source.txt"
  # 1. the tag dir (refused if it exists and is not the helper's).
  rsh "bash -s" "$HL_ROOT_REL" "$tag" <<'EOF'
set -eu
R="$HOME/$1"; T="$R/$2"
mkdir -p "$R/_shared/bin"
if [ -e "$T" ] && [ ! -f "$T/.hl-tag" ]; then echo "hl: $T exists and is not a helper tag dir: refusing" >&2; exit 3; fi
mkdir -p "$T/src" "$T/bin" "$T/logs" "$T/out" "$T/inbox"
[ -f "$T/.hl-tag" ] || echo "$2 $(date -u +%FT%TZ)" > "$T/.hl-tag"
EOF
  # 2. the helper's remote side (from THIS checkout, so any tree can be run).
  "$RSYNC" -a --delete -e "$SSH ${SSH_OPTS[*]}" "$HERE/" "$HL_HOST:$HL_ROOT_REL/$tag/bin/"
  "$RSYNC" -a --delete -e "$SSH ${SSH_OPTS[*]}" "$HERE/" "$HL_HOST:$HL_ROOT_REL/_shared/bin/"
  # 3. the tree, then the record.
  "$RSYNC" -a --files-from="$tmp/manifest.txt" -e "$SSH ${SSH_OPTS[*]}" "$w/" "$HL_HOST:$HL_ROOT_REL/$tag/src/"
  "$RSYNC" -a -e "$SSH ${SSH_OPTS[*]}" "$tmp/manifest.txt" "$tmp/manifest.sha256" "$tmp/source.txt" "$HL_HOST:$HL_ROOT_REL/$tag/"
  # 4. prune files the tree no longer has (git-ignored ones = generated caches stay).
  rsh "$(job_sh "$tag") list-src $tag" > "$tmp/remote.txt"
  LC_ALL=C comm -13 "$tmp/manifest.txt" <(LC_ALL=C sort -u "$tmp/remote.txt") > "$tmp/extra.txt"
  : > "$tmp/stale.txt"
  if [ -s "$tmp/extra.txt" ]; then
    # check-ignore exits 1 when nothing is ignored.
    (cd "$w" || exit 2; git check-ignore --no-index --stdin < "$tmp/extra.txt"; true) | LC_ALL=C sort -u > "$tmp/ignored.txt"
    LC_ALL=C comm -23 "$tmp/extra.txt" "$tmp/ignored.txt" > "$tmp/stale.txt"
  fi
  if [ -s "$tmp/stale.txt" ]; then
    echo "hl: pruning $(wc -l < "$tmp/stale.txt" | tr -d ' ') stale file(s) from $tag/src" >&2
    rsh "$(job_sh "$tag") prune-src $tag" < "$tmp/stale.txt"
  fi
  # 5. verify every shipped byte.
  rsh "$(job_sh "$tag") verify-src $tag"
  echo "synced $tag: $head ($branch) dirty_entries=$dirty files=$n tree_fp=${fp:0:12}"
}

cmd_run() {
  need_tag "${1:-}"; [ $# -ge 2 ] || usage
  local tag=$1 job=$2 a q=""
  shift 2
  if [ "$job" = lanes ]; then
    if [ $# -ne 1 ] || [ ! -f "$1" ]; then die "lanes needs one local items file"; fi
    cmd_push "$tag" "$1" inbox >/dev/null
    set -- "inbox/$(basename "$1")"
  fi
  for a in "$@"; do q="$q $(printf '%q' "$a")"; done
  rsh "$(fwd_env) $(job_sh "$tag") start $tag $job$q"
}

cmd_wait() {
  need_tag "${1:-}"; [ $# -ge 2 ] || usage
  local tag=$1 name=$2 max=${3:-3600} t0
  hl_valid_job "$name" || die "bad job name $name"
  t0=$(date +%s)
  while :; do
    if rsh "test -f $HL_ROOT_REL/$tag/logs/$name.rc"; then
      echo "$name rc=$(rsh "cat $HL_ROOT_REL/$tag/logs/$name.rc")"
      return 0
    fi
    [ $(( $(date +%s) - t0 )) -lt "$max" ] || { echo "hl: $tag/$name still running after ${max}s" >&2; return 124; }
    sleep 30
  done
}

cmd_status() {
  if [ -n "${1:-}" ]; then need_tag "$1"; rsh "$(job_sh "$1") status $1"; else rsh "$(job_sh _shared) status"; fi
}

cmd_preflight() { rsh "$(fwd_env) $(job_sh _shared) preflight"; }

cmd_fetch() {
  need_tag "${1:-}"; [ $# -ge 2 ] || usage
  local tag=$1 dest=$2 runs=${3:-} ex=(--exclude=/runs/)
  [ "$runs" = --runs ] && ex=()
  mkdir -p "$dest/logs" "$dest/out"
  "$RSYNC" -a -e "$SSH ${SSH_OPTS[*]}" "$HL_HOST:$HL_ROOT_REL/$tag/source.txt" "$dest/"
  "$RSYNC" -a -e "$SSH ${SSH_OPTS[*]}" "$HL_HOST:$HL_ROOT_REL/$tag/logs/" "$dest/logs/"
  "$RSYNC" -a ${ex[@]+"${ex[@]}"} -e "$SSH ${SSH_OPTS[*]}" "$HL_HOST:$HL_ROOT_REL/$tag/out/" "$dest/out/"
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
  git -C "$repo" archive "$ref" | rsh "$(fwd_env) $(job_sh "$tag") js-cache $tag $ref"
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
  status) cmd_status "$@" ;;
  preflight) cmd_preflight ;;
  fetch) cmd_fetch "$@" ;;
  push) cmd_push "$@" ;;
  js-cache) cmd_js_cache "$@" ;;
  clean) cmd_clean "$@" ;;
  -h|--help|help) usage ;;
  *) echo "hl: unknown command $c" >&2; usage ;;
esac
