#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016,SC2012  # test assertions: `a && ok || bad`; literal $ in patterns
# The homelab execution helper (scripts/remote/), offline: ssh, rsync, docker
# and tmux are stubs, the "homelab" is a temp HOME. Checks:
#   tags (reserved names too) and job words validated before any remote call;
#   sync ships git's file set (no ignored files, no target/), records HEAD +
#   dirty state, prunes deleted files but keeps remote-generated (git-ignored)
#   ones, also when EVERY extra is stale (review F1), fails LOUDLY when a step
#   fails and then leaves the tag unstartable, refuses while a job of the tag
#   runs (F8), and verify-src catches a changed byte; a dir the helper did not
#   create is never synced into or cleaned (incl. hl-job.sh clean called
#   directly); run forwards caps and every container carries the tag label,
#   the caps, /target/<tag> and the tree's provenance; the touch rule runs on
#   the host and survives a failed docker run (F7); the CPU / memory guards
#   (held CPUs, held memory, stale and MALFORMED reservations, the lock, the
#   hard limits) refuse; a refused run writes its own rc (F5); pack is
#   refused; clean removes only that tag's sessions, reservations, containers,
#   compose networks/volumes and dirs; build-bins-in.sh's cargo/cc lines =
#   build-bins.sh's; the rig override; lanes refuse bins from another tree and
#   keep results per tree + bins (F6); a failed build-bins leaves no bins;
#   import-bins checks provenance; stop TERMs the rig's process group (F4); the
#   systemd suites' clock guard (F10); fetch keeps to its dir and re-checks the
#   sha (F17); hl-summ.py never prints a value; and lints (shellcheck) every
#   helper script.
# Run in a throwaway container:
#   docker run --rm -v "$PWD:/src:ro" -w /src debian:bookworm bash -c \
#     'apt-get update -qq && apt-get install -y -qq git python3 shellcheck procps >/dev/null; bash scripts/tests/hl.sh'
set -uo pipefail
SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
FH=$W/home            # the fake homelab $HOME
STUB=$W/stub; LOG=$W/log
mkdir -p "$FH" "$STUB" "$LOG"
cp -R "$SRC/scripts/remote" "$W/remote"
HL="$W/remote/hl.sh"
export HL_HOST=fake@homelab HL_SSH=$STUB/ssh HL_RSYNC=$STUB/rsync STUB_LOG=$LOG FAKEHOME=$FH
# The test host's memory is not the homelab's: lift the hard floor for the suite
# (HL_GUARD_TESTING is never forwarded by hl.sh; the stub ssh passes the env).
export HL_GUARD_TESTING=1 HL_MIN_FREE_GB=0 HL_TEST_MEMAVAIL_GB=64
# From the temp dir: a mounted git WORKTREE's .git points at a host path, and
# git refuses even --global config inside it.
(cd "$W" && git config --global user.email t@example.invalid && git config --global user.name t \
  && git config --global --add safe.directory '*') 2>/dev/null
RES=$FH/xorgate-rs-p7/_shared/res

# ---- stubs
cat > "$STUB/ssh" <<'EOF'
#!/usr/bin/env bash
# ssh [-o X]... host cmd...  ->  the cmd in the fake homelab (HOME=$FAKEHOME, cwd there)
while [ "${1:-}" = -o ]; do shift 2; done
shift   # host
echo "ssh $*" >> "$STUB_LOG/ssh.log"
cd "$FAKEHOME" && HOME=$FAKEHOME PATH="$(dirname "$0"):$PATH" exec bash -c "$*"
EOF
cat > "$STUB/rsync" <<'EOF'
#!/usr/bin/env bash
# The rsync subset hl.sh uses: -a, --delete, --files-from=F, --exclude=P, -e X.
# STUB_RSYNC_FAIL=files makes the --files-from transfer fail (a sync stopped half-way).
del=0; ff=""; ex=(); args=()
while [ $# -gt 0 ]; do
  case "$1" in
    -a) ;; --delete) del=1 ;; --files-from=*) ff=${1#--files-from=} ;; --exclude=*) ex+=("${1#--exclude=}") ;;
    -e) shift ;; *) args+=("$1") ;;
  esac; shift
done
if [ -n "$ff" ] && [ "${STUB_RSYNC_FAIL:-}" = files ]; then echo "rsync stub: injected failure" >&2; exit 23; fi
map() { case "$1" in *@*:*) echo "$FAKEHOME/${1#*:}" ;; *) echo "$1" ;; esac; }
dest=$(map "${args[${#args[@]}-1]}"); unset 'args[${#args[@]}-1]'
echo "rsync del=$del ff=${ff:+yes} ex=${ex[*]:-} ${args[*]} -> $dest" >> "$STUB_LOG/rsync.log"
for s in "${args[@]}"; do
  s=$(map "$s")
  if [ -n "$ff" ]; then
    while IFS= read -r f; do mkdir -p "$dest/$(dirname "$f")"; cp -p "$s/$f" "$dest/$f"; done < "$ff"
  elif [ -d "$s" ]; then
    mkdir -p "$dest"; if [ $del = 1 ]; then rm -rf "${dest:?}"/*; fi; cp -pR "$s/." "$dest/"
    for e in ${ex[@]+"${ex[@]}"}; do e=${e#/}; e=${e%/}; for x in "$dest"/$e; do rm -rf "$x"; done; done
  else mkdir -p "$dest"; cp -p "$s" "$dest/"; fi
done
exit 0
EOF
cat > "$STUB/docker" <<'EOF'
#!/usr/bin/env bash
echo "docker $*" >> "$STUB_LOG/docker.log"
case "$1 ${2:-}" in
  "image inspect") exit 0 ;;
  "ps "*) [ -f "$STUB_LOG/ps.out" ] && grep -- "${STUB_PS_GREP:-.}" "$STUB_LOG/ps.out"; exit 0 ;;
  "network ls"|"volume ls")
    k=$1; shift 2
    case " $* " in *" --format "*) cat "$STUB_LOG/$k.projects" 2>/dev/null; exit 0 ;; esac
    p=$(printf '%s\n' "$@" | sed -n 's/^label=com.docker.compose.project=//p')
    [ -n "$p" ] && grep -qx -- "$p" "$STUB_LOG/$k.projects" 2>/dev/null && echo "$k-$p"
    exit 0 ;;
  "network rm"|"volume rm") echo "$1 rm $3" >> "$STUB_LOG/removed"; exit 0 ;;
  "exec "*) case "$*" in *"/proc/1/comm"*) echo systemd ;; esac; exit 0 ;;
  "run "*) exit "${STUB_DOCKER_RUN_RC:-0}" ;;
  *) exit 0 ;;
esac
EOF
cat > "$STUB/tmux" <<'EOF'
#!/usr/bin/env bash
echo "tmux $*" >> "$STUB_LOG/tmux.log"
case "$1" in
  has-session) grep -qx -- "${3#=}" "$STUB_LOG/sessions" 2>/dev/null ;;
  ls) cat "$STUB_LOG/sessions" 2>/dev/null ;;
  list-panes) cat "$STUB_LOG/pane.${3#=}" 2>/dev/null ;;
  kill-session) echo "killed ${3#=}" >> "$STUB_LOG/killed" ;;
  new-session) shift 4; bash -c "$*"; exit 0 ;;   # new-session -d -s <name> <cmd>: run the job synchronously (tmux itself returns 0)
esac
EOF
REAL_TOUCH=$(command -v touch)
cat > "$STUB/touch" <<EOF
#!/usr/bin/env bash
# STUB_TOUCH_FAIL=1: the ONE-TREE touch fails (any user, root included)
[ "\${STUB_TOUCH_FAIL:-0}" = 1 ] && { echo "touch: injected failure" >&2; exit 1; }
exec $REAL_TOUCH "\$@"
EOF
chmod +x "$STUB"/*
export PATH="$STUB:$PATH"
: > "$LOG/sessions"
mtime() { stat -c %Y "$1" 2>/dev/null || stat -f %m "$1"; }

# ---- a tree to ship
R=$W/repo
mkdir -p "$R/crates/a/src" "$R/target/release" "$R/tools/replay/.cache" "$R/scripts/tests" "$R/tools/replay/statvfs-shim"
cd "$R" && git init -q -b main . 2>/dev/null || { git init -q . && git checkout -q -b main; }
printf '/target\n/tools/replay/.cache/\n' > .gitignore
echo 'fn main() {}' > crates/a/src/main.rs
echo keep > crates/a/old.rs
echo 'pkg' > Cargo.toml
cat > scripts/docker-build.sh <<'EOF'
#!/usr/bin/env bash
[ "$1 $2 $3" = "--platform linux/amd64 image-tag" ] && echo "xorgate-rs-build-amd64:1.99.0-test" && exit 0
exit 1
EOF
printf '#!/usr/bin/env bash\necho "PASS: x"\n' > scripts/tests/unit-ordering.sh
printf '#!/usr/bin/env bash\necho "PASS: y"\n' > scripts/tests/install-rs-systemd.sh
printf '# rig\nFROM --platform=linux/arm64 node:20-bookworm\nRUN true\n' > tools/replay/Dockerfile.rig
printf 'services: {}\n' > tools/replay/docker-compose.yml
printf '#!/bin/sh\necho certs\n' > tools/replay/gen-certs.sh; chmod +x tools/replay/gen-certs.sh
git add -A && git commit -qm init
echo untracked > notes.txt                      # untracked, not ignored: shipped
echo ignored > tools/replay/.cache/big          # ignored: not shipped
echo junk > target/release/xorgate-rs           # target/: never shipped
HEAD1=$(git rev-parse HEAD)

echo "== tags and job words"
for t in Hl a_b _shared ../x "$(printf 'a%.0s' $(seq 41))" "" rig hl-shared p7e hl-bringup; do
  "$HL" sync "$t" "$R" >/dev/null 2>&1 && bad "tag '$t' accepted" || ok "tag '$t' refused"
done
: > "$LOG/ssh.log"
"$HL" run t1 'test; touch /tmp/hl-pwned' >/dev/null 2>&1 && bad "job word with ';' accepted" || ok "job word with ';' refused"
"$HL" run t1 rm-everything >/dev/null 2>&1 && bad "unknown job accepted" || ok "unknown job refused"
[ ! -s "$LOG/ssh.log" ] && [ ! -e /tmp/hl-pwned ] && ok "an invalid job word never reaches the remote shell (F12)" || bad "remote called for an invalid job word"
out=$("$HL" run t1 pack 2>&1); case "$out" in *aarch64-only*) ok "pack refused (aarch64-only)" ;; *) bad "pack not refused: $out" ;; esac

echo "== sync"
out=$("$HL" sync t1 "$R" 2>&1); rc=$?
[ $rc = 0 ] && ok "sync t1 rc 0" || { bad "sync t1 rc $rc"; echo "$out"; }
T1=$FH/xorgate-rs-p7/t1
[ -f "$T1/src/crates/a/src/main.rs" ] && [ -f "$T1/src/notes.txt" ] && ok "tracked + untracked files shipped" || bad "tracked/untracked missing"
[ ! -e "$T1/src/tools/replay/.cache/big" ] && ok "git-ignored file not shipped" || bad "ignored file shipped"
[ ! -e "$T1/src/target" ] && ok "target/ not shipped" || bad "target/ shipped"
grep -qx "head=$HEAD1" "$T1/source.txt" && grep -qx "branch=main" "$T1/source.txt" && grep -qx "dirty_entries=1" "$T1/source.txt" \
  && ok "source.txt: HEAD, branch, dirty_entries=1 (the untracked file)" || { bad "source.txt"; cat "$T1/source.txt"; }
[ -f "$T1/.hl-tag" ] && [ -x "$T1/bin/hl-job.sh" ] && [ -x "$FH/xorgate-rs-p7/_shared/bin/hl-job.sh" ] && ok "marker + helper copies" || bad "marker/helper copies"
n=$(cd "$R" && git ls-files -co --exclude-standard | grep -vc '^target/')
printf '%s\n' "$out" | grep -q "^verified $n files" && ok "verify-src ran ($n files)" || { bad "verify count (want $n)"; echo "$out"; }
[ "$(cat "$T1/verified.fp")" = "$(sed -n 's/^tree_fp=//p' "$T1/source.txt")" ] && [ ! -e "$T1/source.txt.new" ] \
  && ok "verified.fp = source.txt tree_fp; the record was installed by verify-src" || bad "verified marker"
mkdir -p "$R/untr" && echo 1 > "$R/untr/a" && echo 2 > "$R/untr/b"
"$HL" sync t1 "$R" >/dev/null 2>&1; grep -qx "dirty_entries=3" "$T1/source.txt" && ok "dirty_entries counts every untracked file (F13)" || bad "dirty count of an untracked dir: $(grep dirty "$T1/source.txt")"
rm -rf "$R/untr"

echo "== re-sync: prune deleted, keep remote-generated"
mkdir -p "$T1/src/tools/replay/.cache/bin" && echo gen > "$T1/src/tools/replay/.cache/bin/xorgate-rs"
git -C "$R" rm -q crates/a/old.rs && git -C "$R" commit -qm rm
echo 'fn main() { }' > "$R/crates/a/src/main.rs"
out=$("$HL" sync t1 "$R" 2>&1)
[ ! -e "$T1/src/crates/a/old.rs" ] && ok "deleted file pruned" || bad "deleted file still on the remote"
[ -f "$T1/src/tools/replay/.cache/bin/xorgate-rs" ] && ok "remote-generated ignored file kept" || bad "generated file pruned"
grep -q 'fn main() { }' "$T1/src/crates/a/src/main.rs" && ok "changed file shipped" || bad "changed file not shipped"
grep -qx "dirty_entries=2" "$T1/source.txt" && ok "dirty_entries=2 (modified + untracked)" || bad "dirty count after change"
[ "$(grep -c '^tag=t1' "$T1/source.log")" = 3 ] && ok "source.log keeps every sync" || bad "source.log: $(grep -c '^tag=t1' "$T1/source.log")"
echo tampered >> "$T1/src/Cargo.toml"
cp "$T1/source.txt" "$T1/source.txt.new"
HOME=$FH bash "$T1/bin/hl-job.sh" verify-src t1 >/dev/null 2>&1 && bad "verify-src missed a changed byte" || ok "verify-src catches a changed byte"
"$HL" sync t1 "$R" >/dev/null 2>&1 && ok "re-sync repairs it" || bad "re-sync after tamper"

echo "== F1: every extra stale (no git-ignored file on the remote)"
"$HL" sync t3 "$R" >/dev/null 2>&1; T3=$FH/xorgate-rs-p7/t3
echo x > "$R/crates/a/gone.rs" && git -C "$R" add crates/a/gone.rs && git -C "$R" commit -qm add
"$HL" sync t3 "$R" >/dev/null 2>&1
git -C "$R" rm -q crates/a/gone.rs && git -C "$R" commit -qm del
[ ! -e "$T3/src/tools/replay/.cache" ] && ok "precondition: t3 has no git-ignored remote file" || bad "t3 precondition"
out=$("$HL" sync t3 "$R" 2>&1); rc=$?
[ $rc = 0 ] && [ ! -e "$T3/src/crates/a/gone.rs" ] && printf '%s\n' "$out" | grep -q '^verified ' \
  && ok "all-stale extras: pruned and verified, rc 0" || { bad "all-stale sync rc $rc"; echo "$out"; }
echo 'fn x() {}' > "$R/crates/a/new.rs"
out=$(STUB_RSYNC_FAIL=files "$HL" sync t3 "$R" 2>&1); rc=$?
[ $rc != 0 ] && printf '%s\n' "$out" | grep -q 'hl: FAILED (rc' && ok "a sync that stops half-way fails loudly (rc $rc)" || { bad "silent sync failure rc $rc"; echo "$out"; }
[ ! -e "$T3/verified.fp" ] && ok "the half-way sync left no verified marker" || bad "verified.fp survived a failed sync"
out=$("$HL" run t3 test 2>&1); case "$out" in *"did not complete and verify"*) ok "start refuses an unverified tag" ;; *) bad "start on an unverified tag: $out" ;; esac
[ "$(cat "$T3/logs/test.rc")" = 75 ] && ok "the refusal wrote rc 75" || bad "refusal rc"
"$HL" sync t3 "$R" >/dev/null 2>&1 && "$HL" run t3 test >/dev/null 2>&1 && [ "$(cat "$T3/logs/test.rc")" = 0 ] && ok "a full re-sync makes it startable again" || bad "re-sync after a failed sync"
echo bogus > "$T3/verified.fp"
"$HL" run t3 test >/dev/null 2>&1 && bad "start with verified.fp != tree_fp accepted" || ok "start refuses verified.fp != tree_fp"
rm -f "$R/crates/a/new.rs"

echo "== F8: no sync while a job of the tag runs"
echo "hl_t1_busyjob" >> "$LOG/sessions"
out=$("$HL" sync t1 "$R" 2>&1) && bad "sync accepted while hl_t1_busyjob runs" || { case "$out" in *"running"*) ok "sync refused while a job of the tag runs" ;; *) bad "sync refusal text: $out" ;; esac; }
echo "hl_t11_other" >> "$LOG/sessions"; sed -i '/^hl_t1_busyjob$/d' "$LOG/sessions"
"$HL" sync t1 "$R" >/dev/null 2>&1 && ok "another tag's job (hl_t11_*) does not block t1" || bad "t11 blocked t1"
echo "hl_t1_busyjob" >> "$LOG/sessions"
"$HL" sync --force t1 "$R" >/dev/null 2>&1 && ok "sync --force overrides" || bad "sync --force"
: > "$LOG/sessions"

echo "== foreign dirs"
mkdir -p "$FH/xorgate-rs-p7/foreign/src" && echo theirs > "$FH/xorgate-rs-p7/foreign/src/x"
"$HL" sync foreign "$R" >/dev/null 2>&1 && bad "synced into a foreign dir" || ok "sync refuses a dir without .hl-tag"
HOME=$FH bash "$FH/xorgate-rs-p7/_shared/bin/hl-job.sh" clean foreign >/dev/null 2>&1
[ -f "$FH/xorgate-rs-p7/foreign/src/x" ] && ok "hl-job.sh clean (called directly) refuses a dir without .hl-tag" || bad "clean removed a foreign dir"

echo "== run: caps, labels, target dir, provenance"
: > "$LOG/docker.log"
out=$("$HL" run t1 test 2>&1)
printf '%s\n' "$out" | grep -q 'started hl_t1_test' && ok "run starts session hl_t1_test" || { bad "run test"; echo "$out"; }
d=$(grep '^docker run' "$LOG/docker.log" | head -1)
for want in '--label xorgate.agent=t1' '--label xorgate.hl=1' '--cpus 16' '--memory 32g' '-v xorgate-rs-p7-target:/target' \
            '-e CARGO_TARGET_DIR=/target/t1' '-e CARGO_BUILD_JOBS=16' 'xorgate-rs-build-amd64:1.99.0-test' '/hl/bin/gate-in.sh test' \
            "-e HL_TREE_FP=$(sed -n 's/^tree_fp=//p' "$T1/source.txt")" '-e HL_TAG=t1'; do
  case "$d" in *"$want"*) ok "container has '${want:0:60}'" ;; *) bad "container lacks '$want': $d" ;; esac
done
[ "$(cat "$T1/logs/test.rc" 2>/dev/null)" = 0 ] && [ -f "$T1/logs/test.log" ] && [ -f "$T1/logs/test.env" ] && ok "rc/log/env files" || bad "rc/log/env files"
[ ! -e "$RES/t1_test" ] && ok "reservation released at the end" || bad "reservation left behind"
: > "$LOG/docker.log"
HL_CPUS=6 HL_MEM=8g HL_JOB_NAME=t2 "$HL" run t1 cargo test -p a >/dev/null 2>&1
d=$(grep '^docker run' "$LOG/docker.log" | head -1)
case "$d" in *'--cpus 6 --memory 8g'*'CARGO_BUILD_JOBS=6'*'/hl/bin/cargo-in.sh test -p a'*) ok "HL_CPUS/HL_MEM/HL_JOB_NAME forwarded, cargo args kept" ;; *) bad "overrides: $d" ;; esac
grep -q '^HL_CPUS=6$' "$T1/logs/t2.env" && ok "env file records the overrides" || bad "env file overrides"

echo "== touch rule (one tree per target dir, on the host: F7)"
RS=$T1/src/crates/a/src/main.rs
touch -d '2020-01-01' "$RS"; "$HL" run t1 test >/dev/null 2>&1
[ "$(mtime "$RS")" -lt 1600000000 ] && ok "same tree again: no touch" || bad "touch on an unchanged tree"
echo 'fn main() {  }' > "$R/crates/a/src/main.rs"; "$HL" sync t1 "$R" >/dev/null 2>&1
touch -d '2020-01-01' "$RS"
STUB_DOCKER_RUN_RC=125 "$HL" run t1 test >/dev/null 2>&1
[ "$(cat "$T1/logs/test.rc")" != 0 ] && ok "precondition: the container failed to start (rc $(cat "$T1/logs/test.rc"))" || bad "stub docker failure"
[ "$(mtime "$RS")" -gt 1600000000 ] && ok "new tree: every .rs touched on the host even though docker run failed" || bad "no touch before a failing docker run"
[ "$(cat "$T1/target.fp")" = "$(cat "$T1/verified.fp")" ] && ok "target.fp written after the touch" || bad "target.fp"
echo 'fn main() {   }' > "$R/crates/a/src/main.rs"; "$HL" sync t1 "$R" >/dev/null 2>&1
cp "$T1/target.fp" "$W/fp.before"
STUB_TOUCH_FAIL=1 "$HL" run t1 test >/dev/null 2>&1
[ "$(cat "$T1/logs/test.rc")" != 0 ] && cmp -s "$T1/target.fp" "$W/fp.before" && ok "a failed touch fails the job and leaves target.fp alone" || bad "target.fp recorded after a failed touch"
"$HL" run t1 test >/dev/null 2>&1; [ "$(cat "$T1/target.fp")" = "$(cat "$T1/verified.fp")" ] && ok "…and the next job touches and records it" || bad "touch after a failed touch"

echo "== guards"
echo "hl_t1_busy" >> "$LOG/sessions"; echo "20 8" > "$RES/t1_busy"
"$HL" run t1 test >/dev/null 2>&1 && bad "CPU guard: 20 + 16 > 24 accepted" || ok "CPU guard refuses 20 + 16 > 24"
HL_CPUS=4 "$HL" run t1 test >/dev/null 2>&1 && ok "CPU guard admits 20 + 4" || bad "CPU guard refused 20 + 4"
sed -i '/^hl_t1_busy$/d' "$LOG/sessions"
HL_CPUS=8 "$HL" run t1 test >/dev/null 2>&1 && ok "a reservation whose session is gone is ignored (20 + 8 admitted)" || bad "stale reservation counted"
rm -f "$RES/t1_busy"
echo "hl_t1_bad" >> "$LOG/sessions"; echo "8 8g" > "$RES/t1_bad"
out=$(HL_CPUS=1 "$HL" run t1 test 2>&1) && bad "malformed reservation: guard failed open" || { case "$out" in *MALFORMED*) ok "a malformed reservation fails CLOSED (F2) and is named" ;; *) bad "malformed text: $out" ;; esac; }
sed -i '/^hl_t1_bad$/d' "$LOG/sessions"
HL_CPUS=1 "$HL" run t1 test >/dev/null 2>&1 && bad "a malformed reservation without a session was ignored" || ok "a malformed reservation counts even without its session"
printf '8 8.5\n' > "$RES/t1_bad"; HL_CPUS=1 "$HL" run t1 test >/dev/null 2>&1 && bad "'8 8.5' accepted" || ok "'8 8.5' is malformed too"
rm -f "$RES/t1_bad"
A=$HL_TEST_MEMAVAIL_GB
if [ "$A" -ge 6 ]; then
  HL_MIN_FREE_GB=$((A - 3)) HL_MEM=1g "$HL" run t1 test >/dev/null 2>&1 && ok "memory: ${A}G - 1G >= floor $((A - 3))G admitted" || bad "memory guard refused without held memory"
  echo "hl_t1_mem" >> "$LOG/sessions"; echo "1 4" > "$RES/t1_mem"
  HL_MIN_FREE_GB=$((A - 3)) HL_MEM=1g "$HL" run t1 test >/dev/null 2>&1 && bad "memory guard ignores held memory" || ok "memory guard counts held memory (${A} - 4 - 1 < $((A - 3)))"
  sed -i '/^hl_t1_mem$/d' "$LOG/sessions"; rm -f "$RES/t1_mem"
else echo "NOTE: < 6 GiB available here: the held-memory case needs more"; fi
HL_MIN_FREE_GB=100000 "$HL" run t1 test >/dev/null 2>&1 && bad "memory guard accepted" || ok "memory guard refuses"
out=$(HL_GUARD_TESTING=0 HL_TOTAL_CPUS=40 "$HL" run t1 test 2>&1) && bad "HL_TOTAL_CPUS=40 accepted" || { case "$out" in *"above the plan cap"*) ok "HL_TOTAL_CPUS cannot be raised (F3)" ;; *) bad "raise text: $out" ;; esac; }
out=$(HL_GUARD_TESTING=0 HL_TOTAL_CPUS=24 HL_MIN_FREE_GB=4 "$HL" run t1 test 2>&1) && bad "HL_MIN_FREE_GB=4 accepted" || { case "$out" in *"below the plan floor"*) ok "HL_MIN_FREE_GB cannot be lowered (F3)" ;; *) bad "floor text: $out" ;; esac; }
HL_CPUS=0 "$HL" run t1 test >/dev/null 2>&1 && bad "HL_CPUS=0 accepted" || ok "HL_CPUS=0 refused"
HL_MEM=lots "$HL" run t1 test >/dev/null 2>&1 && bad "bad HL_MEM accepted" || ok "bad HL_MEM refused"
( exec 8>>"$RES/.lock"; flock 8; sleep 4 ) & lockpid=$!
sleep 1
out=$(HL_LOCK_WAIT=1 "$HL" run t1 test 2>&1) && bad "start ran while the reservation lock was held" || { case "$out" in *lock*) ok "start waits for / refuses without the reservation lock (F3)" ;; *) bad "lock text: $out" ;; esac; }
wait "$lockpid"
echo "hl_t1_test" >> "$LOG/sessions"
"$HL" run t1 test >/dev/null 2>&1 && bad "second session with the same name accepted" || ok "a running session name is refused"
sed -i '/^hl_t1_test$/d' "$LOG/sessions"
echo "hl_t9_x" >> "$LOG/sessions"; echo "23 1" > "$RES/t9_x"
HL_JOB_NAME=imp "$HL" run t1 import-tapes t1 nothing >/dev/null 2>&1; [ "$(cat "$T1/logs/imp.refused" 2>/dev/null)" = "" ] && ok "import-* budget is 1 CPU (admitted beside 23 held: F11)" || bad "import budget: $(cat "$T1/logs/imp.refused")"
sed -i '/^hl_t9_x$/d' "$LOG/sessions"; rm -f "$RES/t9_x"

echo "== F5: wait never reports an older run's rc"
"$HL" run t1 test >/dev/null 2>&1; [ "$(cat "$T1/logs/test.rc")" = 0 ] && ok "precondition: test rc 0"
HL_MIN_FREE_GB=100000 "$HL" run t1 test >/dev/null 2>&1
out=$("$HL" wait t1 test 5 2>&1)
case "$out" in "test rc=75 (REFUSED: "*) ok "after a refused run, wait reports 75 + the reason, not the old 0" ;; *) bad "wait after a refusal: $out" ;; esac
out=$(HL_CPUS=4 HL_JOBS=2 "$HL" wait t1 nosuchjob 1 2>&1); rc=$?
case "$out" in *FAILED*) bad "a pending wait / forwarded env printed a false FAILED: $out" ;; *"still running after 1s"*) [ $rc = 124 ] && ok "wait times out with rc 124 and no false 'FAILED' (bash 3.2 ERR quirks)" || bad "wait timeout rc $rc" ;; *) bad "wait timeout text: $out" ;; esac

echo "== build-bins-in.sh = build-bins.sh (drift guard)"
for pat in 'cargo build --profile $PROFILE -p xorgate-agent -p xorgate-replay && cp $TARGET_DIR/$OUTDIR/xorgate-rs $TARGET_DIR/$OUTDIR/xorgate-replay /src/tools/replay/.cache/bin/' \
           'cc -shared -fPIC -O2 -Wall -Wextra -o /src/tools/replay/.cache/bin/rig-statvfs.so /src/tools/replay/statvfs-shim/rig_statvfs.c -ldl'; do
  grep -qF -- "$pat" "$SRC/tools/replay/build-bins.sh" && grep -qF -- "$pat" "$SRC/scripts/remote/build-bins-in.sh" \
    && ok "same line in both: ${pat:0:40}…" || bad "drift: '$pat' not in both build-bins.sh and build-bins-in.sh"
done
for k in 'tree_fp=${HL_TREE_FP' 'dirty_entries=${HL_DIRTY' 'diff_vs_head_sha256=${HL_DIFFSHA'; do
  grep -qF -- "$k" "$SRC/scripts/remote/build-bins-in.sh" && ok "out/bins/HEAD records ${k%%=*} (F6d)" || bad "HEAD lacks $k"
done

echo "== F6c: a failed build-bins leaves no bins"
B=$T1/src/tools/replay/.cache/bin
mkdir -p "$T1/out/bins" "$B" && echo old > "$T1/out/bins/xorgate-rs" && echo old > "$B/xorgate-replay" && echo "tree_fp=x" > "$B/BUILT_FROM"
STUB_DOCKER_RUN_RC=101 "$HL" run t1 build-bins >/dev/null 2>&1
[ "$(cat "$T1/logs/build-bins.rc")" != 0 ] && [ ! -e "$T1/out/bins" ] && [ ! -e "$B/xorgate-replay" ] && [ ! -e "$B/BUILT_FROM" ] \
  && ok "failed build-bins: old out/bins and .cache/bin removed" || bad "old bins survived a failed build"

echo "== rig override (synth, lanes)"
: > "$LOG/docker.log"
HL_RIG_CPUS=6 HL_RIG_MEM=6g "$HL" run t1 synth smoke >/dev/null 2>&1
G=$T1/gen
GD=$T1/src/tools/replay/.cache/hl/Dockerfile.rig.amd64
grep -q '^FROM --platform=linux/amd64 node:20-bookworm$' "$GD" && ! grep -q arm64 "$GD" && grep -q 'dockerfile: .cache/hl/Dockerfile.rig.amd64$' "$G/compose.hl.yml" \
  && ok "Dockerfile.rig: the one FROM rewritten to amd64, inside the build context" || bad "Dockerfile.rig rewrite"
grep -q 'cpus: 6$' "$G/compose.hl.yml" && grep -q 'mem_limit: 6g' "$G/compose.hl.yml" && [ "$(grep -c 'platform: linux/amd64' "$G/compose.hl.yml")" = 2 ] \
  && ok "compose override: amd64 for both services, rig caps 6/6g" || bad "compose override"
grep -q 'docker compose run --rm -T --no-deps rig synth smoke' "$LOG/docker.log" && ok "synth through compose" || bad "synth call"
printf 'FROM --platform=linux/arm64 a\nFROM --platform=linux/arm64 b\n' > "$T1/src/tools/replay/Dockerfile.rig"
"$HL" run t1 synth smoke >/dev/null 2>&1; [ "$(cat "$T1/logs/synth.rc")" != 0 ] && ok "two arm64 FROMs refused" || bad "two FROMs accepted"
"$HL" sync t1 "$R" >/dev/null 2>&1   # restore the tree

# lanes against a stub run-rig.sh (writes a diff with a sentinel value)
TP=$T1/src/tools/replay/.cache/tapes
mkdir -p "$B" "$TP"; printf '\177ELFstub' > "$B/xorgate-rs"; printf r > "$B/xorgate-replay"; printf s > "$B/rig-statvfs.so"
: > "$TP/smoke.ndjson.gz"; : > "$TP/20261010T211828Z.ndjson.gz"
cat > "$T1/src/tools/replay/run-rig.sh" <<'EOF'
#!/usr/bin/env bash
out=""; while [ $# -gt 0 ]; do [ "$1" = --out ] && out=$2; shift; done
mkdir -p "$out" "$(dirname "$0")/.cache/runs/run-x" && echo ">> play x → .cache/runs/run-x (real time)"
echo "cp: cannot open 'x' for reading: Permission denied"; echo ">> diff: $out/diff.md (exit 1)"
printf -- '- verdict: **FAIL** (1 untolerated, 7 tolerated)\n' > "$out/diff.md"
printf '{"hits":[["T1",5],["T3",2],["T9",0]],"mismatches":[{"stream":"telemetry","at":"ts A 1 / B 1","path":"metrics.gps.lat.value","a":"SENTINEL-111","b":"SENTINEL-222","tol":"UNTOLERATED"}],"stats":{}}\n' > "$out/diff.json"
exit 1
EOF
chmod +x "$T1/src/tools/replay/run-rig.sh"
printf '# comment\nL1 smoke rsrs\nL2 20261010T211828Z jsrs purge\n' > "$W/items.txt"
rm -f "$B/BUILT_FROM"
HL_PREFLIGHT=none "$HL" run t1 lanes "$W/items.txt" >/dev/null 2>&1
[ "$(cat "$T1/logs/lanes.rc")" != 0 ] && grep -q 'BUILT_FROM' "$T1/logs/lanes.log" && ok "lanes refuse bins without provenance (F6b)" || bad "lanes ran on bins of unknown provenance"
echo "tree_fp=0000" > "$B/BUILT_FROM"
HL_PREFLIGHT=none "$HL" run t1 lanes "$W/items.txt" >/dev/null 2>&1
[ "$(cat "$T1/logs/lanes.rc")" != 0 ] && grep -q 'built from tree' "$T1/logs/lanes.log" && ok "lanes refuse bins built from another tree (F6b)" || bad "lanes ran on another tree's bins"
FP1=$(cat "$T1/verified.fp"); printf 'tree_fp=%s\nvia=test\n' "$FP1" > "$B/BUILT_FROM"
HL_PREFLIGHT=none "$HL" run t1 lanes "$W/items.txt" >/dev/null 2>&1
OD=$(ls -d "$T1/out/lanes/${FP1:0:12}-"* 2>/dev/null | head -1); Q=$OD/q.tsv
[ -n "$OD" ] && [ -f "$Q" ] && ok "lanes write to out/lanes/<tree_fp12>-<bins sha12>/" || { bad "lanes output dir"; ls -R "$T1/out" | head -20; }
r1=$(grep '^L1	' "$Q"); r2=$(grep '^L2	' "$Q")
case "$r1" in *'rc=1/diff=1	FAIL	1	7	T1:5,T3:2	'*'	rescued	'*) ok "lane row: rc + the diff's exit, verdict, counts, ids with hits > 0, copy rescued" ;; *) bad "lane row: $r1"; cat "$T1/logs/lanes.log"; esac
grep -q 'docker run --rm --platform linux/amd64 --label xorgate.agent=t1 -v .*/.cache/runs:/r .* xorgate-rs-script-tests:bookworm-[0-9a-f]* sh -c .*cp -a .* L1 run-x ' "$LOG/docker.log" \
  && ok "the run is re-copied as root in a labelled container (0600 root files)" || { bad "no rescue copy"; grep cp "$LOG/docker.log"; }
[ -f "$OD/runs/L1/bins.sha256" ] && ok "bins.sha256 written when run-rig.sh's copy stopped" || bad "bins.sha256 missing"
grep -q "^# tree_fp=$FP1" "$OD/lanes-bins.sha256" && ok "the lane record names its tree_fp" || bad "lanes-bins.sha256 provenance"
[ -n "$r2" ] && [ ! -e "$OD/runs/L2" ] && [ -d "$OD/runs/L1" ] && ok "purge removes only that run dir (O8)" || bad "purge"
grep -q $'telemetry\tmetrics.gps.lat.value\tts' "$OD/untol/L1.txt" && ok "untolerated path recorded" || bad "untol file"
grep -rq SENTINEL "$Q" "$OD/untol" "$T1/logs/lanes.log" && bad "a VALUE leaked into the summaries" || ok "no value in q.tsv / untol / the job log"
HL_PREFLIGHT=none "$HL" run t1 lanes "$W/items.txt" >/dev/null 2>&1
[ "$(grep -c '^L1	' "$Q")" = 1 ] && ok "same tree + same bins: a finished lane is not re-run" || bad "lane re-run"
echo 'fn main() {    }' > "$R/crates/a/src/main.rs"; "$HL" sync t1 "$R" >/dev/null 2>&1
HL_PREFLIGHT=none "$HL" run t1 lanes "$W/items.txt" >/dev/null 2>&1
[ "$(cat "$T1/logs/lanes.rc")" != 0 ] && ok "after a re-sync the old bins are refused (F6a/b)" || bad "lanes reused the old tree's bins"
FP2=$(cat "$T1/verified.fp"); printf 'tree_fp=%s\nvia=test\n' "$FP2" > "$B/BUILT_FROM"
HL_PREFLIGHT=none "$HL" run t1 lanes "$W/items.txt" >/dev/null 2>&1
OD2=$(ls -d "$T1/out/lanes/${FP2:0:12}-"* 2>/dev/null | head -1)
[ -n "$OD2" ] && [ "$OD2" != "$OD" ] && grep -q '^L1	' "$OD2/q.tsv" && [ "$(grep -c '^L1	' "$Q")" = 1 ] \
  && ok "a new tree re-runs L1 in its own dir; the old tree's row is untouched (F6a)" || bad "F6a namespace"
printf 'P1 smoke cal\n' > "$W/items2.txt"
HL_MAX_LOAD=-1 HL_PREFLIGHT_STEP=0 HL_PREFLIGHT_MAX=0 HL_JOB_NAME=lanes2 "$HL" run t1 lanes "$W/items2.txt" >/dev/null 2>&1
grep -q $'^P1\tsmoke\tcal\tSKIPPED' "$OD2/q.tsv" && [ "$(cat "$T1/logs/lanes2.rc")" = 3 ] && ok "loaded host: lane SKIPPED, job rc 3" || bad "preflight skip"
printf 'X1 smoke ab\n' > "$W/items3.txt"
HL_PREFLIGHT=none HL_JOB_NAME=lanes3 "$HL" run t1 lanes "$W/items3.txt" >/dev/null 2>&1
[ "$(cat "$T1/logs/lanes3.rc")" = 2 ] && ok "ab without xorgate-rs-a/-b refused" || bad "ab refusal"
printf 'X2 smoke ab\nX3 smoke jsrs\n' > "$W/items4.txt"; printf x > "$B/xorgate-rs-a"; printf x > "$B/xorgate-rs-b"; chmod +x "$B"/xorgate-rs-?
HL_PREFLIGHT=none HL_JOB_NAME=lanes4 "$HL" run t1 lanes "$W/items4.txt" >/dev/null 2>&1
[ "$(cat "$T1/logs/lanes4.rc")" = 2 ] && ok "ab mixed with other modes refused" || bad "mixed ab accepted"

echo "== import-bins (provenance)"
T2=$FH/xorgate-rs-p7/t2; "$HL" sync t2 "$R" >/dev/null 2>&1
mkdir -p "$T1/out/bins" && (cd "$T1/out/bins" && printf A > xorgate-rs && printf B > xorgate-replay && printf C > rig-statvfs.so \
  && sha256sum xorgate-rs xorgate-replay rig-statvfs.so > SHA256SUMS && printf 'head=x\ntree_fp=%s\n' "$FP2" > HEAD)
"$HL" run t2 import-bins t1 >/dev/null 2>&1; [ "$(cat "$T2/src/tools/replay/.cache/bin/xorgate-rs" 2>/dev/null)" = A ] && grep -qx "tree_fp=$FP2" "$T2/src/tools/replay/.cache/bin/BUILT_FROM" \
  && ok "import-bins copies + records the source tree" || bad "import-bins"
"$HL" run t2 import-bins t1 a >/dev/null 2>&1; [ "$(cat "$T2/src/tools/replay/.cache/bin/xorgate-rs-a" 2>/dev/null)" = A ] && [ -f "$T2/src/tools/replay/.cache/bin/BUILT_FROM.a" ] && ok "import-bins as side a" || bad "import-bins side a"
printf 'head=x\ntree_fp=%s\n' "$FP1" > "$T1/out/bins/HEAD"
"$HL" run t2 import-bins t1 >/dev/null 2>&1; [ "$(cat "$T2/logs/import-bins.rc")" != 0 ] && ok "import-bins refuses bins from the source's OLD tree" || bad "import-bins took stale bins"
printf 'head=x\n' > "$T1/out/bins/HEAD"
"$HL" run t2 import-bins t1 >/dev/null 2>&1; [ "$(cat "$T2/logs/import-bins.rc")" != 0 ] && ok "import-bins refuses bins without a tree_fp" || bad "import-bins took bins without provenance"
printf 'head=x\ntree_fp=%s\n' "$FP2" > "$T1/out/bins/HEAD"
mkdir -p "$T1/src/tools/replay/.cache/tapes" && printf tape > "$T1/src/tools/replay/.cache/tapes/m1.ndjson.gz"
"$HL" run t2 import-tapes t1 m1 >/dev/null 2>&1; [ "$(cat "$T2/src/tools/replay/.cache/tapes/m1.ndjson.gz" 2>/dev/null)" = tape ] && grep -q '  m1.ndjson.gz$' "$T2/out/tapes.sha256" && ok "import-tapes copies + records sha" || bad "import-tapes"
HL_JOB_NAME=it2 "$HL" run t2 import-tapes t1 nope >/dev/null 2>&1; [ "$(cat "$T2/logs/it2.rc")" != 0 ] && ok "import-tapes refuses a missing tape" || bad "import-tapes missing tape"
printf Z > "$T1/out/bins/xorgate-replay"
"$HL" run t2 import-bins t1 >/dev/null 2>&1; [ "$(cat "$T2/logs/import-bins.rc")" != 0 ] && ok "import-bins refuses a sha mismatch" || bad "import-bins ignored a sha mismatch"

echo "== fetch (F17)"
mkdir -p "$T2/out/bins" && (cd "$T2/out/bins" && printf A > xorgate-rs && sha256sum xorgate-rs > SHA256SUMS)
out=$("$HL" fetch t2 "$W/f2" 2>&1) && printf '%s\n' "$out" | grep -q 'sha256 OK: out/bins/SHA256SUMS' && [ "$(cat "$W/f2/.hl-fetch")" = t2 ] \
  && ok "fetch re-checks SHA256SUMS and marks its dir" || { bad "fetch: $out"; }
echo stray > "$W/f2/out/stray"; "$HL" fetch t2 "$W/f2" >/dev/null 2>&1
[ ! -e "$W/f2/out/stray" ] && ok "fetch --delete keeps the dir in step (inside the tag's fetch dir)" || bad "fetch kept a stray file"
printf B > "$T2/out/bins/xorgate-rs"
"$HL" fetch t2 "$W/f2" >/dev/null 2>&1 && bad "fetch accepted a sha mismatch" || ok "fetch refuses bins that fail their SHA256SUMS"
mkdir -p "$W/mine" && echo keep > "$W/mine/notes"
"$HL" fetch t2 "$W/mine" >/dev/null 2>&1 && bad "fetch into a non-empty foreign dir accepted" || ok "fetch refuses a non-empty dir that is not its own"
[ -f "$W/mine/notes" ] && ok "…and deleted nothing there" || bad "fetch deleted a foreign file"

echo "== stop (F4): the rig's process group, then the session"
: > "$LOG/ps.out"; : > "$LOG/killed"
# The rig stand-in traps TERM like run-rig.sh (which tears its compose project
# down on TERM): only a TERM to its process group leaves the marker; a KILL does not.
export TERM_MARK=$W/termed
bash -c 'timeout 300 bash -c "trap \"echo termed > \$TERM_MARK; exit 0\" TERM; sleep 299 & wait"; exit 0' & jobp=$!
sleep 1
tpid=$(ps -o pid= --ppid "$jobp" | tr -d ' ' | head -1); spid=$(ps -o pid= --ppid "$tpid" | tr -d ' ' | head -1)
[ -n "$spid" ] && [ "$(ps -o pgid= -p "$tpid" | tr -d ' ')" != "$(ps -o pgid= -p "$jobp" | tr -d ' ')" ] \
  && ok "precondition: timeout runs in its own process group (why a tmux kill alone misses it)" || bad "precondition: process tree"
echo "hl_t2_lanesx" >> "$LOG/sessions"; echo "$jobp" > "$LOG/pane.hl_t2_lanesx"; echo "8 8" > "$RES/t2_lanesx"
mkdir -p "$T2/logs" && date -u +%s > "$T2/logs/lanesx.start"
out=$(HL_STOP_WAIT=4 "$HL" stop t2 lanesx 2>&1)
sleep 1
gone() { local st; st=$(ps -o stat= -p "$1" 2>/dev/null | tr -d ' '); [ -z "$st" ] || [ "${st#Z}" != "$st" ]; }   # absent or a zombie
gone "$spid" && ok "stop ends the timeout's process group: the rig child is gone" || { bad "rig child survived stop: $out"; kill "$spid" 2>/dev/null; }
[ -f "$TERM_MARK" ] && ! printf '%s\n' "$out" | grep -q KILL && ok "…by TERM (the rig's teardown trap ran), no KILL needed" || bad "stop did not TERM the rig gracefully: $out"
orphan=$(ps -o pid= --ppid "$spid" 2>/dev/null | tr -d ' '); [ -z "$orphan" ] || kill "$orphan" 2>/dev/null
grep -qx "killed hl_t2_lanesx" "$LOG/killed" && ok "…and kills the session" || bad "session not killed"
[ "$(cat "$T2/logs/lanesx.rc" 2>/dev/null)" = 143 ] && [ -f "$T2/logs/lanesx.stopped" ] && [ ! -e "$RES/t2_lanesx" ] && ok "…writes rc 143 + .stopped and frees the reservation" || bad "stop bookkeeping"
wait "$jobp" 2>/dev/null
sed -i '/^hl_t2_lanesx$/d' "$LOG/sessions"

echo "== systemd suites: clock guard (F10)"
: > "$LOG/docker.log"
"$HL" run t1 systemd >/dev/null 2>&1; [ "$(cat "$T1/logs/systemd.rc")" = 0 ] && grep -q 'docker run -d' "$LOG/docker.log" && ok "benign systemd suites run" || bad "benign systemd suites: $(tail -3 "$T1/logs/systemd.log")"
echo 'hwclock --systohc' >> "$R/scripts/tests/install-rs-systemd.sh"; "$HL" sync t1 "$R" >/dev/null 2>&1; : > "$LOG/docker.log"
"$HL" run t1 systemd >/dev/null 2>&1
[ "$(cat "$T1/logs/systemd.rc")" != 0 ] && ! grep -q 'docker run -d' "$LOG/docker.log" && grep -q 'REFUSED' "$T1/logs/systemd.log" \
  && ok "a suite that would run hwclock is refused before any privileged container" || bad "clock guard"
sed -i 's/^hwclock --systohc$/hwclock --systohc  # hl-clock-mocked (a stub on PATH)/' "$R/scripts/tests/install-rs-systemd.sh"; "$HL" sync t1 "$R" >/dev/null 2>&1
"$HL" run t1 systemd >/dev/null 2>&1; [ "$(cat "$T1/logs/systemd.rc")" = 0 ] && ok "…unless the line is marked hl-clock-mocked" || bad "mocked line refused"
git -C "$R" checkout -q -- scripts/tests/install-rs-systemd.sh

echo "== clean (only this tag)"
printf 'hl_t1_x\nhl_t1-2_y\nhl_t11_z\nother\n' > "$LOG/sessions"
printf 'c1\nc2\n' > "$LOG/ps.out"; : > "$LOG/docker.log"; : > "$LOG/killed"; : > "$LOG/removed"
for r in t1_x t11_x t1-2_y; do echo "1 1" > "$RES/$r"; done
printf 'hl_t1_lanes-a\nhl_t11_lanes-b\nhl_t1-2_lanes-c\n' > "$LOG/network.projects"
printf 'hl_t1_lanes-a\nhl_t1_onlyvol\nhl_t11_lanes-b\nhl_t1-2_lanes-c\n' > "$LOG/volume.projects"
STUB_PS_GREP=c "$HL" clean t1 >/dev/null 2>&1
[ "$(cat "$LOG/killed")" = "killed hl_t1_x" ] && ok "only session hl_t1_x killed" || { bad "sessions killed:"; cat "$LOG/killed"; }
[ ! -e "$RES/t1_x" ] && [ -e "$RES/t11_x" ] && [ -e "$RES/t1-2_y" ] && ok "only t1_* reservations removed (t11_*, t1-2_* kept)" || bad "reservations removed: $(ls "$RES")"
sort "$LOG/removed" > "$W/removed.sorted"
printf 'network rm network-hl_t1_lanes-a\nvolume rm volume-hl_t1_lanes-a\nvolume rm volume-hl_t1_onlyvol\n' | diff - "$W/removed.sorted" >/dev/null \
  && ok "compose networks/volumes: exactly hl_t1_*'s, incl. a volume whose project has no network (F15)" || { bad "compose removal:"; cat "$W/removed.sorted"; }
grep -q '^docker ps -aq --filter label=xorgate.agent=t1$' "$LOG/docker.log" && ok "container filter = exactly label xorgate.agent=t1" || bad "container filter"
grep -q '^docker rm -f c1 c2$' "$LOG/docker.log" && ok "removes the labelled containers" || bad "docker rm"
grep -q 'run --rm --platform linux/amd64 --label xorgate.agent=t1 -v xorgate-rs-p7-target:/target xorgate-rs-script-tests:bookworm-[0-9a-f]* rm -rf /target/t1$' "$LOG/docker.log" && ok "removes /target/t1 only (amd64 helper image)" || { bad "target rm"; grep 'rm -rf' "$LOG/docker.log"; }
[ ! -e "$T1" ] && [ -d "$T2" ] && [ -d "$FH/xorgate-rs-p7/_shared" ] && ok "t1 dir gone; t2 and _shared kept" || bad "dirs after clean"
! grep -q 'docker \(system\|image\|builder\|volume\) prune' "$LOG/docker.log" && ok "no prune of any kind" || bad "a prune ran"

echo "== hl-summ.py"
mkdir -p "$W/run"; printf -- '- verdict: **PASS** (0 untolerated, 3 tolerated)\n' > "$W/run/diff.md"
printf '{"hits":[["T2",1],["T1",2],["T0",0]],"mismatches":[{"stream":"s","at":"x","path":"p","a":"SECRET","b":"SECRET2","tol":"T1"}]}\n' > "$W/run/diff.json"
[ "$(python3 -I "$SRC/scripts/remote/hl-summ.py" row "$W/run")" = $'PASS\t0\t3\tT1:2,T2:1' ] && ok "summ row" || bad "summ row"
[ -z "$(python3 -I "$SRC/scripts/remote/hl-summ.py" untol "$W/run")" ] && ok "summ untol: tolerated rows not listed" || bad "summ untol"

echo "== lint"
if command -v shellcheck >/dev/null; then
  (cd "$SRC" && shellcheck -x scripts/remote/*.sh scripts/tests/hl.sh) > "$W/sc.log" 2>&1 && ok "shellcheck -x scripts/remote/*.sh + this suite" || { bad "shellcheck:"; cat "$W/sc.log"; }
else echo "NOTE: no shellcheck, bash -n only"; fi
for f in "$SRC"/scripts/remote/*.sh; do bash -n "$f" || bad "bash -n $f"; done
python3 -c 'import ast, sys; ast.parse(open(sys.argv[1]).read())' "$SRC/scripts/remote/hl-summ.py" && ok "hl-summ.py parses" || bad "hl-summ.py parse"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
