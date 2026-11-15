#!/usr/bin/env bash
# Homelab side of scripts/remote/hl.sh (copied to ~/xorgate-rs-p7/<tag>/bin/ and
# ~/xorgate-rs-p7/_shared/bin/ by every `hl.sh sync`). Runs ON the homelab.
#
#   hl-job.sh start <tag> <job> [args]   reserve caps, start a detached tmux session `hl_<tag>_<name>`
#   hl-job.sh exec <tag> <job> [args]    the job itself (what the session runs)
#   hl-job.sh stop <tag> [name]          stop a job: the rig's `timeout` process groups, then the session
#   hl-job.sh status [tag] | preflight | clean <tag>
#   hl-job.sh list-src|prune-src|verify-src <tag>   (hl.sh sync's steps)
#   hl-job.sh js-cache <tag> <ref>       (a `git archive` tar on stdin)
#
# Layout: ~/xorgate-rs-p7/<tag>/{.hl-tag, source.txt, verified.fp, manifest.*, src/, bin/, logs/, out/, inbox/}
# and ~/xorgate-rs-p7/_shared/{bin/, res/, jscache-amd64/, tools/}. Jobs and their knobs:
# see hl.sh's header. HL_DRYRUN=1 prints every docker/tmux command instead of running it
# (scripts/tests/hl.sh).
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/remote/hl-lib.sh
. "$HERE/hl-lib.sh"
ROOT=${HL_ROOT:-$HOME/$HL_ROOT_REL}
SHARED=$ROOT/_shared
HL_JOBS=${HL_JOBS:-}
HL_RIG_CPUS=${HL_RIG_CPUS:-8}
HL_RIG_MEM=${HL_RIG_MEM:-8g}
HL_SUITE_CPUS=${HL_SUITE_CPUS:-4}
HL_SUITE_MEM=${HL_SUITE_MEM:-4g}
HL_RUN_SECS=${HL_RUN_SECS:-2700}
HL_PREFLIGHT=${HL_PREFLIGHT:-each}
HL_WAIT_OTHERS=${HL_WAIT_OTHERS:-0}
HL_PEERS=${HL_PEERS:-}
HL_JS_CPUS=${HL_JS_CPUS:-4}
HL_JS_MEM=${HL_JS_MEM:-8g}
HL_STOP_WAIT=${HL_STOP_WAIT:-120}
HL_PROFILE=${HL_PROFILE:-release}
SCRIPT_TESTS_DF=$HERE/Dockerfile.script-tests

die() { echo "hl-job: $*" >&2; exit 2; }
ts() { date -u +%FT%TZ; }
# HL_DRYRUN: print instead of run (docker, tmux).
docker() { if [ "${HL_DRYRUN:-0}" = 1 ]; then echo "DRY docker $*"; else command docker "$@"; fi; }
tmux() { if [ "${HL_DRYRUN:-0}" = 1 ]; then echo "DRY tmux $*"; else command tmux "$@"; fi; }
load1() { cut -d' ' -f1 /proc/loadavg; }
mem_avail_gb() {
  # HL_TEST_MEMAVAIL_GB: scripts/tests/hl.sh only (honoured with HL_GUARD_TESTING=1).
  if [ "${HL_GUARD_TESTING:-0}" = 1 ] && [ -n "${HL_TEST_MEMAVAIL_GB:-}" ]; then echo "$HL_TEST_MEMAVAIL_GB"; return; fi
  awk '/^MemAvailable:/ { printf "%d\n", $2 / 1048576 }' /proc/meminfo
}

tagdir() {
  hl_valid_tag "${1:-}" || die "bad tag '${1:-}'"
  TAG=$1; T=$ROOT/$TAG; SRC=$T/src
  [ -f "$T/.hl-tag" ] || die "$T is not a helper tag dir (run hl.sh sync first)"
}

# ---------------------------------------------------------------- sync steps
cmd_list_src() { tagdir "$1"; (cd "$SRC" && find . \( -type f -o -type l \) -print | sed 's#^\./##'); }
cmd_prune_src() {
  tagdir "$1"
  local f n=0
  while IFS= read -r f; do
    case "$f" in ''|/*|*..*) die "refusing to prune '$f'" ;; esac
    # Fix-check M4/P2: a prune that cannot remove a stale file (a root-owned
    # leftover) fails the sync instead of leaving it to be compiled.
    rm -f -- "${SRC:?}/$f" || die "could not remove stale $f from $TAG/src (root-owned?): sync FAILED"
    [ ! -e "$SRC/$f" ] || die "stale $f is still in $TAG/src: sync FAILED"
    n=$((n + 1))
  done
  echo "pruned $n"
}
# Review F1: sync ships the record as source.txt.new; only a complete, verified
# tree installs it and writes verified.fp (= its tree_fp). `start` refuses a tag
# whose verified.fp is missing or differs from source.txt's tree_fp.
cmd_verify_src() {
  tagdir "$1"
  [ -f "$T/source.txt.new" ] || die "no source.txt.new: the sync did not ship its record"
  (cd "$SRC" && sha256sum -c --quiet --strict "$T/manifest.sha256") || die "the tree on the homelab does NOT match the shipped manifest"
  local fp
  fp=$(sha256sum < "$T/manifest.sha256" | cut -c1-64)
  grep -qx "tree_fp=$fp" "$T/source.txt.new" || die "source.txt.new tree_fp does not match the manifest"
  # Fix-check M4/P2: no file may be in src/ unless it is in the manifest or was
  # judged git-ignored by this sync (ignored.txt, shipped by hl.sh).
  [ -f "$T/ignored.txt" ] || die "no ignored.txt: the sync did not ship its ignore list"
  local extra
  extra=$( (cd "$SRC" && find . \( -type f -o -type l \) -print | sed 's#^\./##' | LC_ALL=C sort -u) \
    | LC_ALL=C comm -23 - <(LC_ALL=C sort -u "$T/manifest.txt" "$T/ignored.txt") )
  [ -z "$extra" ] || die "$(printf '%s\n' "$extra" | wc -l | tr -d ' ') file(s) in $TAG/src are neither in the manifest nor git-ignored (first: $(printf '%s\n' "$extra" | head -1)): sync FAILED"
  mv -f "$T/source.txt.new" "$T/source.txt"
  echo "$fp" > "$T/verified.fp"
  cat "$T/source.txt" >> "$T/source.log"
  echo "verified $(wc -l < "$T/manifest.txt") files, tree_fp ${fp:0:12}"
}
tree_fp() { sed -n 's/^tree_fp=//p' "$T/source.txt" 2>/dev/null; }
tag_verified() { [ -f "$T/verified.fp" ] && [ -n "$(tree_fp)" ] && [ "$(cat "$T/verified.fp")" = "$(tree_fp)" ]; }

# ------------------------------------------------------- reservations / caps
# One file per running job: _shared/res/<tag>_<name> holds EXACTLY
# "<cpus> <mem_gb>" (cpus: integer or 2 decimals; mem: whole GiB), optionally
# followed by " kind:cargo|lto|none" (the fat-LTO rule, hl_job_kind; this
# helper always writes one) and by " pid:<n>" for work that is not a tmux job
# (js-cache).
# - A reservation WITHOUT a kind was written by an older helper copy: it counts
#   as a possible cargo job, so a fat-LTO build is refused while one is live
#   (fail closed; orchestrator ruling on fix-check H2).
# - A well-formed reservation counts while its tmux session hl_<tag>_<name>
#   (or its pid) is alive; once that is gone it is stale and ignored.
# - A MALFORMED one (e.g. "8 8g") counts as the full CPU cap, whatever its
#   session: fail closed (review F2), with a message naming the file.
res_line_ok() { [[ ${1:-} =~ ^[0-9]+(\.[0-9]{1,2})?\ [1-9][0-9]{0,5}(\ kind:(cargo|lto|none))?(\ pid:[0-9]+)?$ ]]; }
res_live() { # <file> <line>
  case "$2" in
    *" pid:"*) kill -0 "${2##*pid:}" 2>/dev/null ;;
    *) command tmux has-session -t "=hl_$(basename "$1")" 2>/dev/null ;;
  esac
}
res_sum() { # -> "<cpus> <mem_gb> <live cargo jobs> <live lto jobs> <live no-kind reservations, comma-separated, or ->"
  local f line c m total_c=0 total_m=0 n_cargo=0 n_lto=0 legacy=
  mkdir -p "$SHARED/res"
  for f in "$SHARED/res"/*; do
    [ -f "$f" ] || continue
    line=$(head -n1 "$f" 2>/dev/null || true)
    if ! res_line_ok "$line"; then
      echo "hl-job: MALFORMED reservation $f ('$line'): counted as the full cap ($HL_CAP_TOTAL_CPUS CPUs); fix it to '<cpus> <mem_gb>'" >&2
      total_c=$(awk -v a="$total_c" -v b="$HL_CAP_TOTAL_CPUS" 'BEGIN { print a + b }')
      continue
    fi
    res_live "$f" "$line" || continue
    read -r c m _ <<<"$line"
    total_c=$(awk -v a="$total_c" -v b="$c" 'BEGIN { print a + b }')
    total_m=$((total_m + m))
    case "$line" in
      *" kind:cargo"*) n_cargo=$((n_cargo + 1)) ;;
      *" kind:lto"*) n_lto=$((n_lto + 1)) ;;
      *" kind:none"*) ;;
      *) legacy="${legacy:+$legacy,}$(basename "$f")" ;;
    esac
  done
  echo "$total_c $total_m $n_cargo $n_lto ${legacy:--}"
}
# Live (or malformed) reservations of OTHER tags, for the lanes' overlap record.
res_others() { # <me> <peers csv>
  local f line b t
  for f in "$SHARED/res"/*; do
    [ -f "$f" ] || continue
    b=$(basename "$f"); t=${b%%_*}
    [ "$t" = "$1" ] && continue
    case ",$2," in *",$t,"*) continue ;; esac
    line=$(head -n1 "$f" 2>/dev/null || true)
    if ! res_line_ok "$line" || res_live "$f" "$line"; then printf '%s\n' "$b"; fi
  done
}
job_budget() { # <job> -> "<cpus> <mem>"
  case "$1" in
    lanes) echo "$(awk -v c="$HL_RIG_CPUS" 'BEGIN { print c + 1 }') $HL_RIG_MEM" ;;
    suites|systemd) echo "$HL_SUITE_CPUS $HL_SUITE_MEM" ;;
    import-bins|import-tapes) echo "1 1g" ;;
    images) echo "4 8g" ;;
    *) echo "$HL_CPUS $HL_MEM" ;;
  esac
}
# The plan's hard limits may only be made stricter (review F3).
check_limits() {
  [[ $HL_TOTAL_CPUS =~ ^[0-9]+(\.[0-9]{1,2})?$ ]] || { echo "bad HL_TOTAL_CPUS '$HL_TOTAL_CPUS'"; return 1; }
  [[ $HL_MIN_FREE_GB =~ ^[0-9]+$ ]] || { echo "bad HL_MIN_FREE_GB '$HL_MIN_FREE_GB'"; return 1; }
  [ "${HL_GUARD_TESTING:-0}" = 1 ] && return 0
  awk -v t="$HL_TOTAL_CPUS" -v c="$HL_CAP_TOTAL_CPUS" 'BEGIN { exit !(t <= c) }' \
    || { echo "HL_TOTAL_CPUS=$HL_TOTAL_CPUS is above the plan cap $HL_CAP_TOTAL_CPUS"; return 1; }
  [ "$HL_MIN_FREE_GB" -ge "$HL_CAP_MIN_FREE_GB" ] \
    || { echo "HL_MIN_FREE_GB=$HL_MIN_FREE_GB is below the plan floor $HL_CAP_MIN_FREE_GB"; return 1; }
}
# reserve <cpus> <gb>: under the caller's lock, check the budget against every
# live reservation. Prints the refusal reason and returns 1 when over.
budget_ok() { # <cpus> <gb> [kind]
  local cpus=$1 gb=$2 kind=${3:-none} used_c used_m n_cargo n_lto legacy avail sum
  sum=$(res_sum) || { echo "could not read the reservations"; return 1; }
  read -r used_c used_m n_cargo n_lto legacy <<<"$sum"
  if [ -z "${used_c:-}" ] || [ -z "${used_m:-}" ]; then echo "could not read the reservations"; return 1; fi
  # The fat-LTO rule (handbook; fix-check H2), host-wide, across EVERY tag.
  if [ "$kind" = lto ] && [ $((n_cargo + n_lto)) -gt 0 ]; then
    echo "a fat-LTO build (build-bins/release) runs ALONE: $n_cargo cargo + $n_lto fat-LTO job(s) of some tag are running"; return 1
  fi
  if [ "$kind" = lto ] && [ "${legacy:--}" != - ]; then
    echo "a fat-LTO build (build-bins/release) runs ALONE: live reservation(s) without a kind (an older helper copy; counted as a possible cargo job): $legacy"; return 1
  fi
  if [ "$kind" = cargo ] && [ "$n_lto" -gt 0 ]; then
    echo "a fat-LTO build (build-bins/release) of some tag is running: no other cargo job until it ends"; return 1
  fi
  awk -v u="$used_c" -v c="$cpus" -v t="$HL_TOTAL_CPUS" 'BEGIN { exit !(u + c <= t) }' \
    || { echo "helper jobs already hold $used_c CPUs; +$cpus would exceed HL_TOTAL_CPUS=$HL_TOTAL_CPUS"; return 1; }
  avail=$(mem_avail_gb)
  [ $((avail - used_m - gb)) -ge "$HL_MIN_FREE_GB" ] \
    || { echo "MemAvailable ${avail}G - held ${used_m}G - this ${gb}G < HL_MIN_FREE_GB=${HL_MIN_FREE_GB}G"; return 1; }
}
# The reservation lock (review F3): check + reserve + session start are one step.
# The fd stays open in THIS process only: every long-lived child (the tmux
# server above all) is started with 9>&- (fix-check M1).
res_lock() {
  mkdir -p "$SHARED/res"
  exec 9>>"$SHARED/res/.lock"
  flock -w "${HL_LOCK_WAIT:-60}" 9
}

cmd_start() {
  tagdir "$1"; local job=$2; shift 2
  local s cpus mem gb v why kind
  NAME=${HL_JOB_NAME:-$job}
  hl_valid_job "$NAME" || die "bad job name '$NAME'"
  if ! hl_known_job "$job"; then
    [ "$job" = pack ] && die "pack is aarch64-only (the device tarball): build it on the Mac (handbook HOMELAB EXECUTION)"
    die "unknown job '$job'"
  fi
  RUNID=${HL_RUN_ID:-none}
  [[ $RUNID =~ ^[A-Za-z0-9._-]{1,80}$ ]] || die "bad HL_RUN_ID"
  s=hl_${TAG}_$NAME
  # A RUNNING job of that name keeps its files (its own rc will follow).
  command tmux has-session -t "=$s" 2>/dev/null && die "session $s is already running"
  # Review F5: from here on, a refusal writes its OWN rc (75) + reason, so
  # `wait` can never report an older run's rc; the run id ties .rc to THIS run
  # (fix-check S1: wait accepts an rc only with its run id).
  mkdir -p "$T/logs"
  rm -f "$T/logs/$NAME.rc" "$T/logs/$NAME.end" "$T/logs/$NAME.refused" "$T/logs/$NAME.stopped" "$T/logs/$NAME.rcid"
  echo "$RUNID" > "$T/logs/$NAME.runid"
  tag_verified || refuse "the last sync of $TAG did not complete and verify (verified.fp missing or != source.txt tree_fp): sync again"
  # Every cap must be a positive value (fix-check S2: docker reads 0 as no limit).
  for v in HL_CPUS HL_RIG_CPUS HL_SUITE_CPUS HL_JS_CPUS; do eval "hl_valid_cpus \"\$$v\"" || refuse "bad $v '$(eval "echo \"\$$v\"")' (a positive number)"; done
  for v in HL_MEM HL_RIG_MEM HL_SUITE_MEM HL_JS_MEM; do eval "hl_mem_gb \"\$$v\"" >/dev/null 2>&1 || refuse "bad $v '$(eval "echo \"\$$v\"")' (<n>g or <n>m, > 0)"; done
  why=$(check_limits) || refuse "$why"
  read -r cpus mem <<<"$(job_budget "$job")"
  gb=$(hl_mem_gb "$mem") || refuse "bad memory value '$mem'"
  kind=$(hl_job_kind "$job")
  res_lock || refuse "could not take the reservation lock $SHARED/res/.lock"
  # Under the lock (fix-check M4/P1): sync prep takes the same lock before it
  # invalidates verified.fp, so this check cannot race a sync; and the tree
  # must still match its manifest (a job may have written into it).
  tag_verified || refuse "the last sync of $TAG did not complete and verify (checked under the lock): sync again"
  (cd "$SRC" && sha256sum -c --quiet --strict "$T/manifest.sha256" >/dev/null 2>&1) \
    || refuse "$TAG/src no longer matches its verified manifest (a job wrote into the tree?): sync again"
  why=$(budget_ok "$cpus" "$gb" "$kind") || refuse "$why"
  {
    echo "job=$job name=$NAME args=$* started=$(ts) budget_cpus=$cpus budget_mem=$mem kind=$kind run_id=$RUNID"
    for v in $HL_FORWARD_VARS; do eval "[ \"\${$v+set}\" = set ]" && eval "echo \"$v=\${$v}\""; done
    cat "$T/source.txt"
  } > "$T/logs/$NAME.env"
  echo "$cpus $gb kind:$kind" > "$SHARED/res/${TAG}_$NAME"
  local q="" a
  for a in "$@"; do q="$q $(printf '%q' "$a")"; done
  # The tmux SERVER keeps the environment of the client that spawned it, and
  # every session inherits it (fix-check round 2, M5). So:
  #  1. the tmux CLIENT runs with every HL_* variable removed: a server it
  #     spawns has none in its GLOBAL environment;
  #  2. this job's knobs go into the SESSION only (`new-session -e`) and into
  #     the command, which first unsets every HL_* name, then sets its own;
  #  3. the expected HL_* set is recorded; `exec` refuses to run with any other.
  local envs="" v2 val allhl clean=() setargs=()
  # shellcheck disable=SC2086  # the knob list is a word list
  allhl=$( { compgen -v | grep '^HL_'; printf '%s\n' $HL_FORWARD_VARS HL_JOB_NAME HL_RUN_ID HL_GUARD_TESTING HL_TEST_MEMAVAIL_GB; } | LC_ALL=C sort -u)
  for v2 in $allhl; do envs="$envs -u $v2"; clean+=(-u "$v2"); done
  : > "$T/logs/$NAME.hlenv.expected.tmp"
  for v2 in $HL_FORWARD_VARS HL_GUARD_TESTING HL_TEST_MEMAVAIL_GB; do
    [ "$v2" = HL_JOB_NAME ] && continue
    if eval "[ \"\${$v2+set}\" = set ]"; then
      val=$(eval "printf '%s' \"\$$v2\"")
      envs="$envs $v2=$(printf '%q' "$val")"; setargs+=(-e "$v2=$val"); printf '%s=%s\n' "$v2" "$val" >> "$T/logs/$NAME.hlenv.expected.tmp"
    fi
  done
  printf 'HL_JOB_NAME=%s\nHL_RUN_ID=%s\n' "$NAME" "$RUNID" >> "$T/logs/$NAME.hlenv.expected.tmp"
  LC_ALL=C sort "$T/logs/$NAME.hlenv.expected.tmp" > "$T/logs/$NAME.hlenv.expected"; rm -f "$T/logs/$NAME.hlenv.expected.tmp"
  setargs+=(-e "HL_JOB_NAME=$NAME" -e "HL_RUN_ID=$RUNID")
  # 9>&-: a tmux SERVER spawned here must not inherit the lock fd, or it holds
  # the reservation lock for its whole life (fix-check M1).
  if ! env "${clean[@]}" tmux new-session -d -s "$s" "${setargs[@]}" "env$envs HL_JOB_NAME=$NAME HL_RUN_ID=$RUNID bash $(printf '%q' "$HERE/hl-job.sh") exec $TAG $job$q > $(printf '%q' "$T/logs/$NAME.log") 2>&1" 9>&-; then
    rm -f "$SHARED/res/${TAG}_$NAME"
    refuse "tmux could not start session $s"
  fi
  echo "started $s (log ~/$HL_ROOT_REL/$TAG/logs/$NAME.log; budget ${cpus} CPUs / ${mem}; run id $RUNID)"
}
refuse() { # <reason>: a refused start leaves rc 75 + the reason (review F5)
  echo "${RUNID:-none}" > "$T/logs/$NAME.rcid"
  echo "75" > "$T/logs/$NAME.rc"
  echo "$1" > "$T/logs/$NAME.refused"
  echo "hl-job: refused: $1" >&2
  exit 2
}

# ------------------------------------------------------------- docker helpers
build_image() {
  [ -f "$SRC/scripts/docker-build.sh" ] || die "no scripts/docker-build.sh in the tree"
  (cd "$SRC" && bash scripts/docker-build.sh --platform linux/amd64 image-tag) || die "the tree's docker-build.sh has no amd64 recipe (pre-7e tree?)"
}
# The job a container belongs to (fix-check S3: `stop <tag> <name>` stops only it).
jobl() { printf '%s' "${HL_JOB_NAME:-${NAME:-adhoc}}"; }
script_image() { echo "xorgate-rs-script-tests:bookworm-$(sha256sum < "$SCRIPT_TESTS_DF" | cut -c1-12)"; }
deny_bin() { echo "$SHARED/tools/cargo-deny-$HL_DENY_VERSION"; }
# brun <suffix> <cpus> <mem> <image> <script in bin/> [args]: one build-image
# container on this tree; outputs handed back to the ai user at the end.
brun() {
  local sfx=$1 cpus=$2 mem=$3 img=$4; shift 4
  local deny=() jobs=${HL_JOBS:-$cpus}
  jobs=${jobs%.*}
  [ -x "$(deny_bin)" ] && deny=(-v "$(deny_bin):/deny/cargo-deny:ro")
  docker run --rm --name "hl_${TAG}_${sfx}" --label "xorgate.agent=$TAG" --label xorgate.hl=1 --label "xorgate.hl.job=$(jobl)" \
    --cpus "$cpus" --memory "$mem" \
    -v "$SRC:/src" -v "$T/logs:/logs" -v "$T/out:/out" -v "$HERE:/hl/bin:ro" ${deny[@]+"${deny[@]}"} \
    -v "$HL_REG_VOL:/usr/local/cargo/registry" -v "$HL_GIT_VOL:/usr/local/cargo/git" -v "$HL_TARGET_VOL:/target" \
    -e "CARGO_TARGET_DIR=/target/$TAG" -e "CARGO_BUILD_JOBS=$jobs" -e CARGO_TERM_COLOR=never \
    -e "HL_HEAD=$(sed -n 's/^head=//p' "$T/source.txt")" -e "HL_UID=$(id -u)" -e "HL_GID=$(id -g)" \
    -e "HL_JOB_NAME=${HL_JOB_NAME:-}" -e "HL_PROFILE=$HL_PROFILE" -e "HL_TAG=$TAG" \
    -e "HL_TREE_FP=$(tree_fp)" -e "HL_DIRTY=$(sed -n 's/^dirty_entries=//p' "$T/source.txt")" \
    -e "HL_DIFFSHA=$(sed -n 's/^diff_vs_head_sha256=//p' "$T/source.txt")" \
    -w /src "$img" sh -c 'rc=0; bash "$@" || rc=$?; chown -R "$HL_UID:$HL_GID" /src /logs /out 2>/dev/null; exit $rc' hl "/hl/bin/$1" "${@:2}"
}
# ONE TREE PER TARGET DIR: /target/<tag> belongs to this tag's tree. When the
# tree changed since the last cargo job here (a re-sync), every .rs is touched
# so cargo's mtime fingerprints cannot keep a stale artifact. The touch runs on
# the HOST before any container, and target.fp is written only after it
# succeeded (review F7): a failed `docker run` can no longer skip it.
touch_if_new_tree() {
  local fp last
  fp=$(tree_fp)
  last=$(cat "$T/target.fp" 2>/dev/null || true)
  if [ "$fp" != "$last" ]; then
    echo "== tree changed (${last:0:12} -> ${fp:0:12}): touching every crates/**/*.rs"
    find "$SRC/crates" -name '*.rs' -exec touch {} + || { echo "== touch FAILED: target.fp not updated"; return 1; }
  fi
  echo "$fp" > "$T/target.fp"
}

# ---------------------------------------------------------------------- jobs
job_cargo_gate() { # [full|test]
  touch_if_new_tree || return 1
  brun "${HL_JOB_NAME:-cargo-gate}" "$HL_CPUS" "$HL_MEM" "$(build_image)" gate-in.sh "${1:-full}"
}
job_cargo() {
  touch_if_new_tree || return 1
  brun "${HL_JOB_NAME:-cargo}" "$HL_CPUS" "$HL_MEM" "$(build_image)" cargo-in.sh "$@"
}
# The plain script suites: every scripts/tests, scripts/bench/tests and
# tools/replay/tests *.sh except the helper library, the cargo probe and the two
# systemd suites (HL_SUITES overrides the list).
suite_list() {
  if [ -n "${HL_SUITES:-}" ]; then printf '%s\n' "$HL_SUITES" | tr ' ' '\n' | sed '/^$/d'; return; fi
  (cd "$SRC" && ls scripts/tests/*.sh scripts/bench/tests/*.sh tools/replay/tests/*.sh 2>/dev/null) \
    | grep -vE '/(_stubs|clippy-ban-probe|unit-ordering|install-rs-systemd|provision-e2e|provision-rs-systemd)\.sh$'
}
job_suites() {
  local img t n rc fails=0
  img=$(script_image)
  docker image inspect "$img" >/dev/null 2>&1 || die "script-tests image $img missing: run the images job"
  # Per job: two suites jobs of one tag (e.g. mutation subsets) never share a
  # container name or a log file. The default job `suites` (and `gate`) keep logs/suites/.
  local LD=$T/logs/suites
  case "$(jobl)" in suites|gate|gate-cargo) ;; *) LD=$T/logs/suites-$(jobl) ;; esac
  mkdir -p "$LD"
  echo "== suites on $(sed -n 's/^head=//p' "$T/source.txt") $(ts) image $img"
  for t in $(suite_list); do
    echo "-- $t"
    n=$(echo "$t" | tr / _)
    # --init: PID 1 reaps orphans (a bare `timeout` as PID 1 does not, so a TERMed
    # test group could linger as a zombie: the stop test's old flake)
    docker run --rm --init --name "hl_${TAG}_$(jobl)-${n%.sh}" --label "xorgate.agent=$TAG" --label xorgate.hl=1 --label "xorgate.hl.job=$(jobl)" \
      --cpus "$HL_SUITE_CPUS" --memory "$HL_SUITE_MEM" -v "$SRC:/src:ro" -v "$HERE:/hl/bin:ro" ${HL_MUTANTS:+-e "MUTANTS=$HL_MUTANTS"} "$img" \
      timeout "${HL_SUITE_TIMEOUT:-1800}" bash /hl/bin/suite-in.sh "$t" > "$LD/$n.log" 2>&1
    rc=$?
    [ $rc -eq 0 ] || fails=$((fails + 1))
    echo "$(head -1 "$LD/$n.log") (container rc=$rc)"
  done
  echo "SUITES_DONE fails=$fails"
  [ "$fails" -eq 0 ]
}
# Review F10 / fix-check S5: the systemd suites run --privileged on the PROD
# host (CAP_SYS_TIME, /dev/rtc0). clock-guard.py refuses them if a suite line
# names a clock/RTC tool, runs `date -s/--set` in any form, or executes
# indirectly (a variable, eval, sh -c, …: fail closed), unless the line is
# marked `hl-clock-mocked` (reviewed: it only drives a stub).
clock_guard() { python3 -I "$HERE/clock-guard.py" "$@"; }
job_systemd() {
  local pair name suite pkgs rc fails=0
  mkdir -p "$T/logs/suites"
  # Phase 8 WP-R1: the two provision suites source tools/provision-e2e/lib.sh,
  # which the guard reads too (it is part of what runs privileged).
  local guarded=("$SRC/scripts/tests/unit-ordering.sh" "$SRC/scripts/tests/install-rs-systemd.sh")
  [ ! -f "$SRC/scripts/tests/provision-e2e.sh" ] || guarded+=("$SRC/scripts/tests/provision-e2e.sh" "$SRC/scripts/tests/provision-rs-systemd.sh" "$SRC/tools/provision-e2e/lib.sh")
  clock_guard "${guarded[@]}" || { echo "SYSTEMD_DONE fails=REFUSED"; return 1; }
  local pairs=("unit-ordering:systemd" "install-rs-systemd:systemd python3 sudo")
  [ ! -f "$SRC/scripts/tests/provision-e2e.sh" ] || pairs+=("provision-e2e:systemd python3 sudo jq curl openssl ca-certificates"
    "provision-rs-systemd:systemd python3 sudo jq curl openssl ca-certificates libgstreamer1.0-0")
  for pair in "${pairs[@]}"; do
    suite=${pair%%:*}; pkgs=${pair#*:}; name=hl_${TAG}_sd-$suite
    # Recipe = each suite's header (systemd 252 as PID 1, privileged, private cgroupns).
    docker run -d --name "$name" --label "xorgate.agent=$TAG" --label xorgate.hl=1 --label "xorgate.hl.job=$(jobl)" \
      --cpus "$HL_SUITE_CPUS" --memory "$HL_SUITE_MEM" --privileged --cgroupns=private \
      --tmpfs /run --tmpfs /run/lock -v "$SRC:/src:ro" debian:bookworm \
      bash -c "apt-get update -qq && apt-get install -y -qq $pkgs >/dev/null && exec /lib/systemd/systemd" >/dev/null || { fails=$((fails + 1)); continue; }
    for _ in $(seq 120); do
      [ "$(docker exec "$name" cat /proc/1/comm 2>/dev/null)" = systemd ] && break
      [ "${HL_DRYRUN:-0}" = 1 ] && break
      sleep 5
    done
    docker exec "$name" timeout 1200 bash "/src/scripts/tests/$suite.sh" > "$T/logs/suites/systemd-$suite.log" 2>&1
    rc=$?
    [ $rc -eq 0 ] || fails=$((fails + 1))
    echo "$suite rc=$rc pass=$(grep -c '^PASS' "$T/logs/suites/systemd-$suite.log") fail=$(grep -c '^FAIL' "$T/logs/suites/systemd-$suite.log") | $(tail -1 "$T/logs/suites/systemd-$suite.log")"
    docker rm -f "$name" >/dev/null 2>&1
  done
  echo "SYSTEMD_DONE fails=$fails"
  [ "$fails" -eq 0 ]
}
job_gate() {
  local rc=0
  echo "== gate: cargo-gate"; HL_JOB_NAME=${HL_JOB_NAME:-gate}-cargo job_cargo_gate full || rc=1
  echo "== gate: suites"; job_suites || rc=1
  echo "== gate: systemd"; job_systemd || rc=1
  echo "GATE_DONE rc=$rc"
  return $rc
}
# Review F6: the previous bins go FIRST (a failed build leaves none), and the
# new ones record the tree they came from (out/bins/HEAD + .cache/bin/BUILT_FROM).
job_build_bins() {
  local B=$SRC/tools/replay/.cache/bin
  # The side bins of ab lanes are superseded too (fix-check M2).
  if ! rm -rf "$T/out/bins" || ! rm -f "$B/xorgate-rs" "$B/xorgate-replay" "$B/rig-statvfs.so" "$B/BUILT_FROM" \
       "$B/xorgate-rs-a" "$B/xorgate-rs-b" "$B/BUILT_FROM.a" "$B/BUILT_FROM.b"; then
    echo "could not remove the previous bins (root-owned leftovers?)"; return 1
  fi
  touch_if_new_tree || return 1
  brun "${HL_JOB_NAME:-build-bins}" "$HL_CPUS" "$HL_MEM" "$(build_image)" build-bins-in.sh "$HL_PROFILE" || return 1
  cat "$T/out/bins/SHA256SUMS" "$T/out/bins/HEAD"
}
job_release() {
  rm -rf "$T/out/bins/release" || return 1
  touch_if_new_tree || return 1
  brun "${HL_JOB_NAME:-release}" "$HL_CPUS" "$HL_MEM" "$(build_image)" release-in.sh || return 1
  cat "$T/out/bins/release/SHA256SUMS"
}
job_import_bins() { # <fromtag> [a|b]
  local from=$1 side=${2:-} F B f bfp
  hl_valid_tag "$from" || die "bad tag $from"
  F=$ROOT/$from/out/bins; B=$SRC/tools/replay/.cache/bin
  [ -f "$F/SHA256SUMS" ] || die "$from has no out/bins (run build-bins there)"
  (cd "$F" && sha256sum -c --quiet SHA256SUMS) || die "$from's out/bins fail their own SHA256SUMS"
  # Review F6: only bins built from the source tag's CURRENT verified tree.
  bfp=$(sed -n 's/^tree_fp=//p' "$F/HEAD")
  [ -n "$bfp" ] || die "$from's out/bins/HEAD has no tree_fp (built by an older helper): rebuild there"
  [ "$bfp" = "$(cat "$ROOT/$from/verified.fp" 2>/dev/null)" ] || die "$from's bins were built from tree ${bfp:0:12}, not $from's current verified tree: rebuild there"
  mkdir -p "$B"
  case "$side" in
    "") for f in xorgate-rs xorgate-replay rig-statvfs.so; do cp "$F/$f" "$B/$f"; done
        printf 'tree_fp=%s\nvia=import-bins:%s\n' "$bfp" "$from" > "$B/BUILT_FROM" ;;
    a|b) cp "$F/xorgate-rs" "$B/xorgate-rs-$side"
         printf 'tree_fp=%s\nvia=import-bins:%s\nsha256=%s\n' "$bfp" "$from" "$(sha256sum < "$F/xorgate-rs" | cut -c1-64)" > "$B/BUILT_FROM.$side" ;;
    *) die "side must be a or b" ;;
  esac
  { echo "# imported $(ts) from $from$([ -n "$side" ] && echo " as side $side")"; cat "$F/HEAD"; } >> "$B/IMPORTS"
  (cd "$B" && sha256sum xorgate-rs* xorgate-replay rig-statvfs.so 2>/dev/null)
}
job_import_tapes() { # <fromtag> <name>...: rig tapes of another tag (sha recorded)
  local from=$1 n F D; shift
  hl_valid_tag "$from" || die "bad tag $from"
  F=$ROOT/$from/src/tools/replay/.cache/tapes; D=$SRC/tools/replay/.cache/tapes
  [ $# -gt 0 ] || die "name the tapes"
  mkdir -p "$D"
  for n in "$@"; do
    [[ $n =~ ^[A-Za-z0-9._-]+$ ]] || die "bad tape name $n"
    [ -f "$F/$n.ndjson.gz" ] || die "$from has no tape $n"
    cp "$F/$n.ndjson.gz" "$D/"
    [ -f "$F/$n.ndjson.gz.cli.ndjson" ] && cp "$F/$n.ndjson.gz.cli.ndjson" "$D/"
  done
  (cd "$D" && sha256sum ./*.ndjson.gz) | sed 's#  \./#  #' | tee "$T/out/tapes.sha256"
}
# The rig on amd64 WITHOUT editing the tree: a compose override (platform,
# caps, the helper label) + Dockerfile.rig with its one arm64 FROM rewritten.
rig_env() { # <compose-project>
  local df=$SRC/tools/replay/Dockerfile.rig n
  n=$(grep -c '^FROM --platform=linux/arm64 ' "$df" || true)
  [ "$n" = 1 ] || die "Dockerfile.rig: expected exactly one 'FROM --platform=linux/arm64' line (found $n)"
  # The rewritten Dockerfile lives INSIDE the build context (git-ignored
  # .cache/): compose v5 builds through `buildx bake`, which refuses a
  # Dockerfile outside the context without an fs entitlement.
  mkdir -p "$T/gen" "$SRC/tools/replay/.cache/hl"
  sed 's#^FROM --platform=linux/arm64 #FROM --platform=linux/amd64 #' "$df" > "$SRC/tools/replay/.cache/hl/Dockerfile.rig.amd64"
  cat > "$T/gen/compose.hl.yml" <<EOF
# generated by hl-job.sh (rig on linux/amd64; caps; helper label)
services:
  mosquitto:
    platform: linux/amd64
    cpus: 1
    labels:
      xorgate.hl: "1"
      xorgate.hl.job: "$(jobl)"
  rig:
    platform: linux/amd64
    build:
      dockerfile: .cache/hl/Dockerfile.rig.amd64
    cpus: $HL_RIG_CPUS
    mem_limit: $HL_RIG_MEM
    labels:
      xorgate.hl: "1"
      xorgate.hl.job: "$(jobl)"
EOF
  export COMPOSE_FILE="$SRC/tools/replay/docker-compose.yml:$T/gen/compose.hl.yml"
  export COMPOSE_PROJECT_NAME=$1
  RIG_IMAGE="xorgate-replay-rig:hl-amd64-$(sha256sum < "$SRC/tools/replay/.cache/hl/Dockerfile.rig.amd64" | cut -c1-12)"
  export RIG_IMAGE
  export RIG_JS_CACHE=$SHARED/jscache-amd64 JS_REF=${HL_JS_REF} XORGATE_AGENT_LABEL=$TAG
}
project() { printf 'hl_%s_%s' "$TAG" "$(printf '%s' "$1" | tr '[:upper:].' '[:lower:]-' | tr -cd 'a-z0-9_-')"; }
job_synth() {
  local t rc=0
  rig_env "$(project synth)"
  mkdir -p "$SRC/tools/replay/.cache/tapes" "$SRC/tools/replay/.cache/runs"
  "$SRC/tools/replay/gen-certs.sh" >/dev/null
  (cd "$SRC/tools/replay" && docker compose build --quiet rig) || die "rig image build failed"
  for t in "$@"; do
    if (cd "$SRC/tools/replay" && docker compose run --rm -T --no-deps rig synth "$t" -o "/rig/out/$t.ndjson.gz" --fixtures /rig/fixtures >/dev/null); then
      mv "$SRC/tools/replay/.cache/runs/$t.ndjson.gz" "$SRC/tools/replay/.cache/tapes/"
    else echo "FAIL synth $t"; rc=1; fi
  done
  (cd "$SRC/tools/replay" && docker compose down -v --remove-orphans >/dev/null 2>&1)
  (cd "$SRC/tools/replay/.cache/tapes" && sha256sum ./*.ndjson.gz) | sed 's#  \./#  #' > "$T/out/tapes.sha256"
  cat "$T/out/tapes.sha256"
  return $rc
}
# Review F6: lanes run only on bins built from (or imported for) THIS tree, and
# write to out/lanes/<tree_fp12>-<bins sha12>/, so a re-sync or a rebuild never
# reuses another tree's rows ("done already" holds only inside that dir).
job_lanes() { # <items file, relative to the tag dir>
  local items=$T/$1 B=$SRC/tools/replay/.cache/bin fp bfp binsha OUTD
  [ -f "$items" ] || die "no items file $items"
  fp=$(tree_fp)
  [ -f "$B/BUILT_FROM" ] || die "no .cache/bin/BUILT_FROM: run build-bins (or import-bins) on this tree first"
  bfp=$(sed -n 's/^tree_fp=//p' "$B/BUILT_FROM")
  [ "$bfp" = "$fp" ] || die ".cache/bin was built from tree ${bfp:0:12}, this tree is ${fp:0:12}: run build-bins (or import-bins from a tag with this tree)"
  # Fix-check M2: an ab lane's side bins must still be the CURRENT bins of
  # their source tag's CURRENT verified tree (a source re-sync/rebuild makes
  # them stale), and byte-equal to them.
  if grep -vE '^[[:space:]]*(#|$)' "$items" | awk '$3 == "ab" { f = 1 } END { exit !f }'; then
    local side sf from sfp ssha
    for side in a b; do
      sf=$B/BUILT_FROM.$side
      if [ ! -f "$B/xorgate-rs-$side" ] || [ ! -f "$sf" ]; then die "ab lane: no side $side (import-bins <tag> $side)"; fi
      from=$(sed -n 's/^via=import-bins://p' "$sf"); sfp=$(sed -n 's/^tree_fp=//p' "$sf"); ssha=$(sed -n 's/^sha256=//p' "$sf")
      if ! hl_valid_tag "$from" || [ -z "$sfp" ] || [ -z "$ssha" ]; then die "ab lane: side $side has no complete provenance (re-import it)"; fi
      [ "$sfp" = "$(cat "$ROOT/$from/verified.fp" 2>/dev/null)" ] || die "ab lane: side $side came from $from's tree ${sfp:0:12}, but $from is now at another tree: re-build there and re-import"
      if [ "$sfp" != "$(sed -n 's/^tree_fp=//p' "$ROOT/$from/out/bins/HEAD" 2>/dev/null)" ] \
         || [ "$ssha" != "$(sed -n 's/  xorgate-rs$//p' "$ROOT/$from/out/bins/SHA256SUMS" 2>/dev/null)" ]; then
        die "ab lane: side $side is not $from's CURRENT bins (rebuilt since the import): re-import"
      fi
      [ "$ssha" = "$(sha256sum < "$B/xorgate-rs-$side" | cut -c1-64)" ] || die "ab lane: xorgate-rs-$side does not match its recorded sha256"
    done
  fi
  binsha=$( (cd "$B" && sha256sum xorgate-replay rig-statvfs.so xorgate-rs-a xorgate-rs-b 2>/dev/null; head -c 4 xorgate-rs | grep -q ELF && sha256sum xorgate-rs) | sha256sum | cut -c1-12)
  OUTD=$T/out/lanes/${fp:0:12}-$binsha
  mkdir -p "$OUTD/runs"
  rig_env "$(project lanes)"
  (cd "$SRC/tools/replay" && "$SRC/tools/replay/gen-certs.sh" >/dev/null && docker compose build --quiet rig) || die "rig image build failed"
  echo "== lanes $1 on $(sed -n 's/^head=//p' "$T/source.txt") tree_fp ${fp:0:12} rig image $RIG_IMAGE caps ${HL_RIG_CPUS} CPUs/${HL_RIG_MEM} -> $OUTD"
  { echo "# tree_fp=$fp"; cat "$B/BUILT_FROM"; for f in "$B"/BUILT_FROM.?; do [ -f "$f" ] && sed "s/^/${f##*.}: /" "$f"; done
    (cd "$B" && sha256sum xorgate-rs* xorgate-replay rig-statvfs.so 2>/dev/null); } | tee "$OUTD/lanes-bins.sha256"
  HL_HELPER_IMAGE=$(script_image)
  docker image inspect "$HL_HELPER_IMAGE" >/dev/null 2>&1 || die "helper image $HL_HELPER_IMAGE missing: run the images job"
  TAG=$TAG T=$T SRC=$SRC OUTD=$OUTD SHARED=$SHARED HL_HELPER_IMAGE=$HL_HELPER_IMAGE HL_JOB_NAME=$(jobl) bash "$HERE/hl-lanes.sh" "$items"
}
job_images() {
  local img sid ok=0
  # 1. the amd64 build image, by the tree's own recipe and name.
  img=$(build_image)
  if docker image inspect "$img" >/dev/null 2>&1; then echo "build image $img present ($(docker image inspect -f '{{.Id}}' "$img"))"
  else
    echo "building $img"
    (cd "$SRC" && bash scripts/docker-build.sh --platform linux/amd64 dockerfile) \
      | docker build --platform linux/amd64 --label xorgate.agent=hl-shared --label xorgate.hl=1 -f - \
          --build-arg "RUST_TOOLCHAIN=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$SRC/rust-toolchain.toml" | head -n1)" \
          -t "$img" "$SRC" || ok=1
  fi
  docker run --rm --label "xorgate.agent=$TAG" --label xorgate.hl=1 --label "xorgate.hl.job=$(jobl)" --cpus 2 --memory 2g "$img" sh -c 'rustc -V; cargo -V; pkg-config --modversion gstreamer-1.0; uname -m' || ok=1
  # 2. the script-tests image (Dockerfile.script-tests, tagged by its hash).
  sid=$(script_image)
  if docker image inspect "$sid" >/dev/null 2>&1; then echo "script-tests image $sid present"
  else docker build --platform linux/amd64 --label xorgate.agent=hl-shared --label xorgate.hl=1 -t "$sid" -f "$SCRIPT_TESTS_DF" "$HERE" || ok=1; fi
  # 3. cargo-deny, x86_64 musl, checked against the release's .sha256.
  if [ -x "$(deny_bin)" ]; then echo "cargo-deny $HL_DENY_VERSION present"
  else
    local d u
    d=$(mktemp -d); u=https://github.com/EmbarkStudios/cargo-deny/releases/download/$HL_DENY_VERSION/cargo-deny-$HL_DENY_VERSION-x86_64-unknown-linux-musl.tar.gz
    if curl -fsSL -o "$d/c.tgz" "$u" && curl -fsSL -o "$d/c.sha" "$u.sha256" \
       && [ "$(cut -d' ' -f1 < "$d/c.sha")" = "$(sha256sum < "$d/c.tgz" | cut -c1-64)" ]; then
      mkdir -p "$SHARED/tools" && tar -xzf "$d/c.tgz" -C "$d" && cp "$d"/cargo-deny-*/cargo-deny "$(deny_bin)" && chmod 0755 "$(deny_bin)"
      echo "cargo-deny $HL_DENY_VERSION installed (sha256 $(cut -c1-12 < "$d/c.sha"))"
    else echo "cargo-deny download or checksum FAILED"; ok=1; fi
    rm -rf "$d"
  fi
  return $ok
}

cmd_exec() {
  tagdir "$1"; local job=$2; shift 2
  local name=${HL_JOB_NAME:-$job} rc=0 exp extra
  # The job's own HL_* environment is recorded, and must be EXACTLY what its
  # start passed (fix-check round 2, M5): an HL_* variable from anywhere else
  # (a tmux server's global env, a stale shell) refuses the job.
  exp=$T/logs/$name.hlenv.expected
  env | grep '^HL_' | LC_ALL=C sort > "$T/logs/$name.hlenv"
  if [ -f "$exp" ] && ! cmp -s "$exp" "$T/logs/$name.hlenv"; then
    extra=$(LC_ALL=C comm -3 "$exp" "$T/logs/$name.hlenv" | sed 's/^[[:space:]]*//; s/=.*//' | sort -u | tr '\n' ' ')
    echo "${HL_RUN_ID:-none}" > "$T/logs/$name.rcid"; echo "unexpected HL_* environment (differs in: $extra): refusing to run" > "$T/logs/$name.refused"
    echo 75 > "$T/logs/$name.rc"; rm -f "$SHARED/res/${TAG}_$name"
    echo "hl-job: refused: unexpected HL_* environment (differs in: $extra)"; return 75
  fi
  echo "== $(ts) $job $* (tag $TAG, $(sed -n 's/^head=//p' "$T/source.txt"), caps ${HL_CPUS} CPUs/${HL_MEM})"
  date -u +%s > "$T/logs/$name.start"
  # A subshell: a job's `die` ends the job, never the bookkeeping below.
  (
    case "$job" in
      gate) job_gate ;;
      cargo-gate) job_cargo_gate full ;;
      test) job_cargo_gate test ;;
      cargo) job_cargo "$@" ;;
      suites) job_suites ;;
      systemd) job_systemd ;;
      build-bins) job_build_bins ;;
      release) job_release ;;
      import-bins) job_import_bins "$@" ;;
      import-tapes) job_import_tapes "$@" ;;
      synth) job_synth "$@" ;;
      lanes) job_lanes "$@" ;;
      images) job_images ;;
      *) echo "unknown job $job"; exit 2 ;;
    esac
  ) || rc=$?
  date -u +%s > "$T/logs/$name.end"
  rm -f "$SHARED/res/${TAG}_$name"
  echo "== $(ts) $job done rc=$rc ($(( $(cat "$T/logs/$name.end") - $(cat "$T/logs/$name.start") )) s)"
  echo "${HL_RUN_ID:-none}" > "$T/logs/$name.rcid"
  echo "$rc" > "$T/logs/$name.rc"
  return "$rc"
}

# --------------------------------------------------------- status / preflight
others_labelled() { # labelled containers that are not this tag's / a peer's
  docker ps --filter label=xorgate.agent --format '{{.Label "xorgate.agent"}} {{.Names}}' \
    | awk -v me="${TAG:-}" -v peers=",${HL_PEERS}," '$1 != me && index(peers, "," $1 ",") == 0'
}
cmd_preflight() {
  local l o
  l=$(load1); o=$(others_labelled | tr '\n' ';')
  echo "load1=$l (max $HL_MAX_LOAD) mem_avail=$(mem_avail_gb)G helper_held=$(res_sum) others=[$o]"
  awk -v l="$l" -v m="$HL_MAX_LOAD" 'BEGIN { exit !(l <= m) }'
}
cmd_status() {
  local t f
  echo "homelab $(hostname) $(ts) load $(cut -d' ' -f1-3 /proc/loadavg) mem_avail $(mem_avail_gb)G helper_held(cpus mem_gb)=$(res_sum)"
  for t in ${1:-$(cd "$ROOT" 2>/dev/null && for d in */.hl-tag; do [ -f "$d" ] && dirname "$d"; done)}; do
    tagdir "$t"
    echo "-- $TAG: $(sed -n 's/^head=//p' "$T/source.txt" 2>/dev/null) ($(sed -n 's/^branch=//p' "$T/source.txt" 2>/dev/null)) dirty=$(sed -n 's/^dirty_entries=//p' "$T/source.txt" 2>/dev/null) tree_fp=$(tree_fp | cut -c1-12) $(tag_verified && echo verified || echo 'NOT VERIFIED (sync again)')"
    for f in "$T"/logs/*.start; do
      [ -f "$f" ] || continue
      local n; n=$(basename "$f" .start)
      if [ -f "$T/logs/$n.rc" ] && { [ ! -f "$T/logs/$n.runid" ] || [ "$(cat "$T/logs/$n.rcid" 2>/dev/null)" = "$(cat "$T/logs/$n.runid")" ]; }; then echo "   $n: rc=$(cat "$T/logs/$n.rc")"
      elif command tmux has-session -t "=hl_${TAG}_$n" 2>/dev/null; then echo "   $n: RUNNING ($(( $(date -u +%s) - $(cat "$f") )) s) | $(tail -1 "$T/logs/$n.log" 2>/dev/null | cut -c1-160)"
      else echo "   $n: DIED (no rc, no session)"; fi
    done
    docker ps -a --filter "label=xorgate.agent=$TAG" --filter label=xorgate.hl=1 --format '   container {{.Names}} {{.Status}}'
  done
}

# --------------------------------------------------------------------- clean
cmd_clean() {
  tagdir "$1"
  local s ids p id
  # 1. this tag's tmux sessions (exact prefix hl_<tag>_: tags cannot contain '_').
  for s in $(command tmux ls -F '#{session_name}' 2>/dev/null | grep "^hl_${TAG}_" || true); do tmux kill-session -t "=$s"; echo "killed session $s"; done
  # 2. containers with EXACTLY this tag's label (running or stopped).
  ids=$(docker ps -aq --filter "label=xorgate.agent=$TAG" --filter label=xorgate.hl=1)
  # shellcheck disable=SC2086
  [ -z "$ids" ] || { docker rm -f $ids >/dev/null; echo "removed containers: $(echo "$ids" | wc -w)"; }
  # 3. compose networks/volumes of this tag's rig projects (hl_<tag>_*).
  for p in $( { docker network ls --format '{{.Label "com.docker.compose.project"}}'; docker volume ls --format '{{.Label "com.docker.compose.project"}}'; } | sort -u | grep "^hl_${TAG}_" || true); do
    for id in $(command docker network ls -q --filter "label=com.docker.compose.project=$p"); do docker network rm "$id" >/dev/null; done
    for id in $(command docker volume ls -q --filter "label=com.docker.compose.project=$p"); do docker volume rm "$id" >/dev/null; done
    echo "removed compose leftovers of $p"
  done
  # 4. its target dir, then its dir (root-owned leftovers through a labelled container).
  # The helper's own amd64 image: the homelab's `alpine:latest` is an arm64
  # image (exec format error without binfmt), and re-pulling would retag it.
  local img; img=$(script_image)
  docker image inspect "$img" >/dev/null 2>&1 || die "script-tests image $img missing (run the images job): nothing removed"
  if docker run --rm --platform linux/amd64 --label "xorgate.agent=$TAG" --label xorgate.hl=1 --label xorgate.hl.job=clean -v "$HL_TARGET_VOL:/target" "$img" rm -rf "/target/$TAG"; then
    echo "removed /target/$TAG"
  else die "could not remove /target/$TAG"; fi
  rm -f "$SHARED/res/${TAG}_"*
  if ! rm -rf "${T:?}" 2>/dev/null; then
    docker run --rm --platform linux/amd64 --label "xorgate.agent=$TAG" --label xorgate.hl=1 --label xorgate.hl.job=clean -v "$ROOT:/r" "$img" rm -rf "/r/$TAG"
    [ "${HL_DRYRUN:-0}" = 1 ] || rm -rf "${T:?}"
  fi
  echo "removed $T"
}

# ---------------------------------------------------------------------- stop
# Review F4: a tmux kill alone leaves a lane's `timeout` + run-rig.sh running
# (GNU timeout makes its own process group), still playing and no longer
# reserved. So: collect the process groups of every `timeout` under the
# session, kill the session (the job loop stops), then TERM those groups
# (run-rig.sh traps TERM and tears its compose project down), wait for them,
# and finally stop any container of the tag that is still up (exact label).
timeout_pgids() { # <root pid>: process groups of the `timeout` processes below it
  [ -n "${1:-}" ] || return 0
  ps -e -o pid=,ppid=,pgid=,comm= | awk -v root="$1" '
    { pp[$1] = $2; pg[$1] = $3; cm[$1] = $4 }
    END {
      for (p in cm) if (cm[p] == "timeout") {
        q = p; n = 0
        while (q in pp && q != root && q != 1 && n++ < 64) q = pp[q]
        if (q == root) print pg[p]
      }
    }' | sort -u
}
# pg_live <pgid> [list]: does the group have a member that is not a zombie? (A
# zombie has exited and only waits for its parent to reap it: `kill -0` still
# counts it, so a TERMed group whose last member is a zombie would be KILLed
# for nothing.) With `list`, print the live members (pid:stat:comm).
pg_live() {
  local m; m=$(ps -e -o pgid=,pid=,stat=,comm= | awk -v g="$1" '$1 == g && $3 !~ /^Z/ { printf "%s:%s:%s ", $2, $3, $4 }')
  if [ "${2:-}" = list ]; then printf '%s' "${m% }"; fi
  [ -n "$m" ]
}
# stop <tag> [name]: only that tag's helper jobs (sessions hl_<tag>_*, exactly
# hl_<tag>_<name> with a name), only process groups UNDER those sessions, and
# only containers labelled BOTH xorgate.agent=<tag> and xorgate.hl=1 (plus
# xorgate.hl.job=<name> with a name: fix-check S3/S7). Never a process group
# <= 1 or our own (defence in depth for the ancestry filter above).
cmd_stop() { # <tag> [name]
  tagdir "$1"; local want=${2:-} s sessions pane pgs pg n waited ids own filt
  own=$(ps -o pgid= -p $$ | tr -d ' ')
  sessions=$(command tmux ls -F '#{session_name}' 2>/dev/null | grep "^hl_${TAG}_" || true)
  if [ -n "$want" ]; then sessions=$(printf '%s\n' "$sessions" | grep -x "hl_${TAG}_$want" || true); fi
  [ -n "$sessions" ] || echo "no running job of $TAG${want:+ named $want}"
  for s in $sessions; do
    pane=$(command tmux list-panes -t "=$s" -F '#{pane_pid}' 2>/dev/null | head -n1)
    pgs=$(timeout_pgids "$pane" | awk -v own="$own" '$1 > 1 && $1 != own')
    tmux kill-session -t "=$s" && echo "killed session $s"
    for pg in $pgs; do echo "TERM process group $pg"; kill -TERM -- "-$pg" 2>/dev/null || true; done
    waited=0
    for pg in $pgs; do
      while pg_live "$pg" && [ "$waited" -lt "$HL_STOP_WAIT" ]; do sleep 1; waited=$((waited + 1)); done
      if pg_live "$pg"; then
        echo "process group $pg still alive after ${HL_STOP_WAIT}s: KILL (members: $(pg_live "$pg" list))"
        kill -KILL -- "-$pg" 2>/dev/null || true
      fi
    done
    n=${s#"hl_${TAG}_"}
    if [ ! -f "$T/logs/$n.rc" ]; then
      cat "$T/logs/$n.runid" > "$T/logs/$n.rcid" 2>/dev/null || echo none > "$T/logs/$n.rcid"
      echo 143 > "$T/logs/$n.rc"; echo "stopped by hl.sh stop $(ts)" > "$T/logs/$n.stopped"
    fi
    rm -f "$SHARED/res/${TAG}_$n"
  done
  filt=(--filter "label=xorgate.agent=$TAG" --filter label=xorgate.hl=1)
  [ -z "$want" ] || filt+=(--filter "label=xorgate.hl.job=$want")
  for _ in $(seq 30); do
    ids=$(docker ps -q "${filt[@]}")
    [ -z "$ids" ] && break
    [ "${HL_DRYRUN:-0}" = 1 ] && break
    sleep 2
  done
  # Running ones still up after the wait, and stopped -d ones (systemd suites).
  ids=$(docker ps -aq "${filt[@]}")
  # shellcheck disable=SC2086
  if [ -n "$ids" ]; then docker rm -f $ids >/dev/null; echo "removed containers: $(echo "$ids" | wc -w)"; fi
  echo "stop $TAG${want:+/$want}: done"
}

# ------------------------------------------------------------------ js-cache
cmd_js_cache() { # <tag> <ref>; tar on stdin
  tagdir "$1"; local ref=$2 dest tmp
  [[ $ref =~ ^[0-9a-f]{7,40}$ ]] || die "ref must be a commit sha"
  dest=$SHARED/jscache-amd64/js-$ref
  if [ -f "$dest/dist/worker.js" ] && [ -f "$dest/dist/cli.js" ]; then cat > /dev/null; echo "js: $dest (exists)"; return 0; fi
  mkdir -p "$SHARED/jscache-amd64"
  # Review F11: the build holds a reservation like any job (pid-scoped).
  local gb why res=$SHARED/res/${TAG}_js-cache
  hl_valid_cpus "$HL_JS_CPUS" || die "refused: bad HL_JS_CPUS '$HL_JS_CPUS' (a positive number)"
  gb=$(hl_mem_gb "$HL_JS_MEM") || die "refused: bad HL_JS_MEM '$HL_JS_MEM'"
  why=$(check_limits) || die "refused: $why"
  res_lock || die "refused: could not take the reservation lock"
  why=$(budget_ok "$HL_JS_CPUS" "$gb") || die "refused: $why"
  echo "$HL_JS_CPUS $gb kind:none pid:$$" > "$res"
  exec 9>&-
  # shellcheck disable=SC2064  # expand $res now
  trap "rm -f '$res'" EXIT
  tmp=$(mktemp -d "$SHARED/jscache-amd64/.tmp-$ref.XXXX")
  tar -x -C "$tmp" || die "bad archive"
  # = tools/replay/js-build.sh's build, on linux/amd64.
  docker run --rm --label "xorgate.agent=$TAG" --label xorgate.hl=1 --label xorgate.hl.job=js-cache --cpus "$HL_JS_CPUS" --memory "$HL_JS_MEM" \
    --platform linux/amd64 -v "$tmp:/js" -w /js -e "HL_UID=$(id -u)" -e "HL_GID=$(id -g)" node:20-bookworm \
    sh -c 'npm ci --no-audit --no-fund --loglevel=error && npm run build >/dev/null && node --version; rc=$?; chown -R "$HL_UID:$HL_GID" /js; exit $rc' \
    || { rm -rf "$tmp"; die "JS build failed"; }
  echo "$ref $(ts)" > "$tmp/.hl-js-ref"
  rm -rf "$dest"; mv "$tmp" "$dest"
  echo "js: $dest (built from $ref, linux/amd64)"
}

[ $# -ge 1 ] || die "usage: hl-job.sh start|exec|stop|status|preflight|clean|list-src|prune-src|verify-src|js-cache …"
c=$1; shift
case "$c" in
  start) [ $# -ge 2 ] || die "start <tag> <job> [args]"; cmd_start "$@" ;;
  stop) cmd_stop "$@" ;;
  res-others) hl_valid_tag "${1:-}" || die "bad tag"; res_others "$1" "${2:-}" ;;
  exec) cmd_exec "$@" ;;
  status) cmd_status "$@" ;;
  preflight) cmd_preflight ;;
  clean) cmd_clean "$@" ;;
  list-src) cmd_list_src "$@" ;;
  prune-src) cmd_prune_src "$@" ;;
  verify-src) cmd_verify_src "$@" ;;
  js-cache) cmd_js_cache "$@" ;;
  *) die "unknown command $c" ;;
esac
