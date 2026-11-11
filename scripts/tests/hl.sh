#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016  # test assertions: `a && ok || bad`; literal $ in patterns
# The homelab execution helper (scripts/remote/), offline: ssh, rsync, docker
# and tmux are stubs, the "homelab" is a temp HOME. Checks:
#   tags/jobs validated; sync ships git's file set (no ignored files, no
#   target/), records HEAD + dirty state, prunes deleted files but keeps
#   remote-generated (git-ignored) ones, and verify-src catches a changed byte;
#   a dir the helper did not create is never synced into or cleaned; run
#   forwards caps and every container carries the tag label, the caps and
#   /target/<tag>; the CPU / memory guards refuse; pack is refused; the touch
#   rule (ONE TREE PER TARGET DIR); clean removes only that tag's sessions,
#   containers and dirs; build-bins-in.sh's cargo/cc lines = build-bins.sh's;
#   the rig override (amd64 FROM rewrite, caps) and refusals; lanes rows,
#   preflight skip, purge, ab refusal; hl-summ.py never prints a value;
#   and lints (shellcheck) every helper script.
# Run in a throwaway container:
#   docker run --rm -v "$PWD:/src:ro" -w /src debian:bookworm bash -c \
#     'apt-get update -qq && apt-get install -y -qq git python3 shellcheck >/dev/null; bash scripts/tests/hl.sh'
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
export HL_MIN_FREE_GB=0   # the test host's memory is not the homelab's
git config --global user.email t@example.invalid 2>/dev/null; git config --global user.name t 2>/dev/null
git config --global --add safe.directory '*' 2>/dev/null

# ---- stubs
cat > "$STUB/ssh" <<'EOF'
#!/usr/bin/env bash
# ssh [-o X]... host cmd...  ->  the cmd in the fake homelab (HOME=$FAKEHOME, cwd there)
while [ "${1:-}" = -o ]; do shift 2; done
shift   # host
cd "$FAKEHOME" && HOME=$FAKEHOME PATH="$(dirname "$0"):$PATH" exec bash -c "$*"
EOF
cat > "$STUB/rsync" <<'EOF'
#!/usr/bin/env bash
# The rsync subset hl.sh uses: -a, --delete, --files-from=F, --exclude=/runs/, -e X.
del=0; ff=""; ex=""; args=()
while [ $# -gt 0 ]; do
  case "$1" in
    -a) ;; --delete) del=1 ;; --files-from=*) ff=${1#--files-from=} ;; --exclude=*) ex=${1#--exclude=} ;;
    -e) shift ;; *) args+=("$1") ;;
  esac; shift
done
map() { case "$1" in *@*:*) echo "$FAKEHOME/${1#*:}" ;; *) echo "$1" ;; esac; }
dest=$(map "${args[${#args[@]}-1]}"); unset 'args[${#args[@]}-1]'
echo "rsync del=$del ff=${ff:+yes} ex=$ex ${args[*]} -> $dest" >> "$STUB_LOG/rsync.log"
for s in "${args[@]}"; do
  s=$(map "$s")
  if [ -n "$ff" ]; then
    while IFS= read -r f; do mkdir -p "$dest/$(dirname "$f")"; cp -p "$s/$f" "$dest/$f"; done < "$ff"
  elif [ -d "$s" ]; then
    mkdir -p "$dest"; [ $del = 1 ] && rm -rf "${dest:?}"/*; cp -pR "$s/." "$dest/"
    if [ "$ex" = /runs/ ]; then rm -rf "$dest/runs"; fi
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
  "network ls"|"volume ls") exit 0 ;;
  "run "*) exit 0 ;;
  *) exit 0 ;;
esac
EOF
cat > "$STUB/tmux" <<'EOF'
#!/usr/bin/env bash
echo "tmux $*" >> "$STUB_LOG/tmux.log"
case "$1" in
  has-session) grep -qx -- "${3#=}" "$STUB_LOG/sessions" 2>/dev/null ;;
  ls) cat "$STUB_LOG/sessions" 2>/dev/null ;;
  kill-session) echo "killed ${3#=}" >> "$STUB_LOG/killed" ;;
  new-session) shift 4; bash -c "$*" ;;   # new-session -d -s <name> <cmd>: run the job synchronously
esac
EOF
chmod +x "$STUB"/*
export PATH="$STUB:$PATH"
: > "$LOG/sessions"

# ---- a tree to ship
R=$W/repo
mkdir -p "$R/crates/a/src" "$R/target/release" "$R/tools/replay/.cache" "$R/scripts" "$R/tools/replay/statvfs-shim"
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
printf '# rig\nFROM --platform=linux/arm64 node:20-bookworm\nRUN true\n' > tools/replay/Dockerfile.rig
printf 'services: {}\n' > tools/replay/docker-compose.yml
printf '#!/bin/sh\necho certs\n' > tools/replay/gen-certs.sh; chmod +x tools/replay/gen-certs.sh
git add -A && git commit -qm init
echo untracked > notes.txt                      # untracked, not ignored: shipped
echo ignored > tools/replay/.cache/big          # ignored: not shipped
echo junk > target/release/xorgate-rs           # target/: never shipped
HEAD1=$(git rev-parse HEAD)

echo "== tags"
for t in Hl a_b _shared ../x "$(printf 'a%.0s' $(seq 41))" ""; do
  "$HL" sync "$t" "$R" >/dev/null 2>&1 && bad "tag '$t' accepted" || ok "tag '$t' refused"
done

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

echo "== re-sync: prune deleted, keep remote-generated"
mkdir -p "$T1/src/tools/replay/.cache/bin" && echo gen > "$T1/src/tools/replay/.cache/bin/xorgate-rs"
git -C "$R" rm -q crates/a/old.rs && git -C "$R" commit -qm rm
echo 'fn main() { }' > "$R/crates/a/src/main.rs"
out=$("$HL" sync t1 "$R" 2>&1)
[ ! -e "$T1/src/crates/a/old.rs" ] && ok "deleted file pruned" || bad "deleted file still on the remote"
[ -f "$T1/src/tools/replay/.cache/bin/xorgate-rs" ] && ok "remote-generated ignored file kept" || bad "generated file pruned"
grep -q 'fn main() { }' "$T1/src/crates/a/src/main.rs" && ok "changed file shipped" || bad "changed file not shipped"
grep -qx "dirty_entries=2" "$T1/source.txt" && ok "dirty_entries=2 (modified + untracked)" || bad "dirty count after change"
[ "$(grep -c '^tag=t1' "$T1/source.log")" = 2 ] && ok "source.log keeps every sync" || bad "source.log"
echo tampered >> "$T1/src/Cargo.toml"
HOME=$FH bash "$T1/bin/hl-job.sh" verify-src t1 >/dev/null 2>&1 && bad "verify-src missed a changed byte" || ok "verify-src catches a changed byte"
"$HL" sync t1 "$R" >/dev/null 2>&1 && ok "re-sync repairs it" || bad "re-sync after tamper"

echo "== foreign dirs"
mkdir -p "$FH/xorgate-rs-p7/foreign/src" && echo theirs > "$FH/xorgate-rs-p7/foreign/src/x"
"$HL" sync foreign "$R" >/dev/null 2>&1 && bad "synced into a foreign dir" || ok "sync refuses a dir without .hl-tag"
"$HL" clean foreign >/dev/null 2>&1; [ -f "$FH/xorgate-rs-p7/foreign/src/x" ] && ok "clean refuses a dir without .hl-tag" || bad "clean removed a foreign dir"

echo "== run: caps, labels, target dir"
: > "$LOG/docker.log"
out=$("$HL" run t1 test 2>&1)
printf '%s\n' "$out" | grep -q 'started hl_t1_test' && ok "run starts session hl_t1_test" || { bad "run test"; echo "$out"; }
d=$(grep '^docker run' "$LOG/docker.log" | head -1)
for want in '--label xorgate.agent=t1' '--label xorgate.hl=1' '--cpus 16' '--memory 32g' '-v xorgate-rs-p7-target:/target' \
            '-e CARGO_TARGET_DIR=/target/t1' '-e CARGO_BUILD_JOBS=16' 'xorgate-rs-build-amd64:1.99.0-test' '/hl/bin/gate-in.sh test'; do
  case "$d" in *"$want"*) ok "container has '$want'" ;; *) bad "container lacks '$want': $d" ;; esac
done
[ "$(cat "$T1/logs/test.rc" 2>/dev/null)" = 0 ] && [ -f "$T1/logs/test.log" ] && [ -f "$T1/logs/test.env" ] && ok "rc/log/env files" || bad "rc/log/env files"
[ ! -e "$FH/xorgate-rs-p7/_shared/res/t1_test" ] && ok "reservation released at the end" || bad "reservation left behind"
: > "$LOG/docker.log"
HL_CPUS=6 HL_MEM=8g HL_JOB_NAME=t2 "$HL" run t1 cargo test -p a >/dev/null 2>&1
d=$(grep '^docker run' "$LOG/docker.log" | head -1)
case "$d" in *'--cpus 6 --memory 8g'*'CARGO_BUILD_JOBS=6'*'/hl/bin/cargo-in.sh test -p a'*) ok "HL_CPUS/HL_MEM/HL_JOB_NAME forwarded, cargo args kept" ;; *) bad "overrides: $d" ;; esac
grep -q '^HL_CPUS=6$' "$T1/logs/t2.env" && ok "env file records the overrides" || bad "env file overrides"

echo "== touch rule (one tree per target dir)"
: > "$LOG/docker.log"; "$HL" run t1 test >/dev/null 2>&1
grep '^docker run' "$LOG/docker.log" | grep -q 'HL_TOUCH=0' && ok "same tree again: no touch" || bad "touch on an unchanged tree"
echo 'fn main() {  }' > "$R/crates/a/src/main.rs"; "$HL" sync t1 "$R" >/dev/null 2>&1
: > "$LOG/docker.log"; "$HL" run t1 test >/dev/null 2>&1
grep '^docker run' "$LOG/docker.log" | grep -q 'HL_TOUCH=1' && ok "re-synced tree: every .rs touched" || bad "no touch after a tree change"

echo "== guards"
echo "hl_t1_busy" >> "$LOG/sessions"; echo "20 8" > "$FH/xorgate-rs-p7/_shared/res/t1_busy"
"$HL" run t1 test >/dev/null 2>&1 && bad "CPU guard: 20 + 16 > 24 accepted" || ok "CPU guard refuses 20 + 16 > 24"
HL_CPUS=4 "$HL" run t1 test >/dev/null 2>&1 && ok "CPU guard admits 20 + 4" || bad "CPU guard refused 20 + 4"
sed -i '/^hl_t1_busy$/d' "$LOG/sessions"
HL_CPUS=4 HL_TOTAL_CPUS=24 "$HL" run t1 test >/dev/null 2>&1 && ok "a reservation whose session is gone is ignored" || bad "stale reservation counted"
rm -f "$FH/xorgate-rs-p7/_shared/res/t1_busy"
HL_MIN_FREE_GB=100000 "$HL" run t1 test >/dev/null 2>&1 && bad "memory guard accepted" || ok "memory guard refuses"
HL_CPUS=0 "$HL" run t1 test >/dev/null 2>&1 && bad "HL_CPUS=0 accepted" || ok "HL_CPUS=0 refused"
HL_MEM=lots "$HL" run t1 test >/dev/null 2>&1 && bad "bad HL_MEM accepted" || ok "bad HL_MEM refused"
out=$("$HL" run t1 pack 2>&1); case "$out" in *aarch64-only*) ok "pack refused (aarch64-only)" ;; *) bad "pack not refused: $out" ;; esac
"$HL" run t1 rm-everything >/dev/null 2>&1 && bad "unknown job accepted" || ok "unknown job refused"
echo "hl_t1_test" >> "$LOG/sessions"
"$HL" run t1 test >/dev/null 2>&1 && bad "second session with the same name accepted" || ok "a running session name is refused"
sed -i '/^hl_t1_test$/d' "$LOG/sessions"

echo "== build-bins-in.sh = build-bins.sh (drift guard)"
for pat in 'cargo build --profile $PROFILE -p xorgate-agent -p xorgate-replay && cp $TARGET_DIR/$OUTDIR/xorgate-rs $TARGET_DIR/$OUTDIR/xorgate-replay /src/tools/replay/.cache/bin/' \
           'cc -shared -fPIC -O2 -Wall -Wextra -o /src/tools/replay/.cache/bin/rig-statvfs.so /src/tools/replay/statvfs-shim/rig_statvfs.c -ldl'; do
  grep -qF -- "$pat" "$SRC/tools/replay/build-bins.sh" && grep -qF -- "$pat" "$SRC/scripts/remote/build-bins-in.sh" \
    && ok "same line in both: ${pat:0:40}…" || bad "drift: '$pat' not in both build-bins.sh and build-bins-in.sh"
done

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
B=$T1/src/tools/replay/.cache/bin; TP=$T1/src/tools/replay/.cache/tapes
mkdir -p "$B" "$TP"; printf '\177ELFstub' > "$B/xorgate-rs"; : > "$TP/smoke.ndjson.gz"; : > "$TP/20261010T211828Z.ndjson.gz"
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
HL_PREFLIGHT=none "$HL" run t1 lanes "$W/items.txt" >/dev/null 2>&1
Q=$T1/out/q.tsv
r1=$(grep '^L1	' "$Q"); r2=$(grep '^L2	' "$Q")
case "$r1" in *'rc=1/diff=1	FAIL	1	7	T1:5,T3:2	'*'	rescued') ok "lane row: rc + the diff's exit, verdict, counts, ids with hits > 0, copy rescued" ;; *) bad "lane row: $r1"; cat "$T1/logs/lanes.log"; esac
grep -q 'docker run --rm --platform linux/amd64 --label xorgate.agent=t1 -v .*/.cache/runs:/r .* xorgate-rs-script-tests:bookworm-[0-9a-f]* sh -c .*cp -a .* L1 run-x ' "$LOG/docker.log" \
  && ok "the run is re-copied as root in a labelled container (0600 root files)" || { bad "no rescue copy"; grep cp "$LOG/docker.log"; }
[ -f "$T1/out/runs/L1/bins.sha256" ] && ok "bins.sha256 written when run-rig.sh's copy stopped" || bad "bins.sha256 missing"
[ -n "$r2" ] && [ ! -e "$T1/out/runs/L2" ] && [ -d "$T1/out/runs/L1" ] && ok "purge removes only that run dir (O8)" || bad "purge"
grep -q $'telemetry\tmetrics.gps.lat.value\tts' "$T1/out/untol/L1.txt" && ok "untolerated path recorded" || bad "untol file"
grep -rq SENTINEL "$Q" "$T1/out/untol" "$T1/logs/lanes.log" && bad "a VALUE leaked into the summaries" || ok "no value in q.tsv / untol / the job log"
HL_PREFLIGHT=none "$HL" run t1 lanes "$W/items.txt" >/dev/null 2>&1
[ "$(grep -c '^L1	' "$Q")" = 1 ] && ok "a finished lane is not re-run" || bad "lane re-run"
printf 'P1 smoke cal\n' > "$W/items2.txt"
HL_MAX_LOAD=-1 HL_PREFLIGHT_STEP=0 HL_PREFLIGHT_MAX=0 HL_JOB_NAME=lanes2 "$HL" run t1 lanes "$W/items2.txt" >/dev/null 2>&1
grep -q $'^P1\tsmoke\tcal\tSKIPPED' "$Q" && [ "$(cat "$T1/logs/lanes2.rc")" = 3 ] && ok "loaded host: lane SKIPPED, job rc 3" || bad "preflight skip"
printf 'X1 smoke ab\n' > "$W/items3.txt"
HL_PREFLIGHT=none HL_JOB_NAME=lanes3 "$HL" run t1 lanes "$W/items3.txt" >/dev/null 2>&1
[ "$(cat "$T1/logs/lanes3.rc")" = 2 ] && ok "ab without xorgate-rs-a/-b refused" || bad "ab refusal"
printf 'X2 smoke ab\nX3 smoke jsrs\n' > "$W/items4.txt"; printf x > "$B/xorgate-rs-a"; printf x > "$B/xorgate-rs-b"; chmod +x "$B"/xorgate-rs-?
HL_PREFLIGHT=none HL_JOB_NAME=lanes4 "$HL" run t1 lanes "$W/items4.txt" >/dev/null 2>&1
[ "$(cat "$T1/logs/lanes4.rc")" = 2 ] && ok "ab mixed with other modes refused" || bad "mixed ab accepted"

echo "== import-bins"
T2=$FH/xorgate-rs-p7/t2; "$HL" sync t2 "$R" >/dev/null 2>&1
mkdir -p "$T1/out/bins" && (cd "$T1/out/bins" && printf A > xorgate-rs && printf B > xorgate-replay && printf C > rig-statvfs.so && sha256sum xorgate-rs xorgate-replay rig-statvfs.so > SHA256SUMS && echo '# built from git x' > HEAD)
"$HL" run t2 import-bins t1 >/dev/null 2>&1; [ "$(cat "$T2/src/tools/replay/.cache/bin/xorgate-rs" 2>/dev/null)" = A ] && ok "import-bins copies" || bad "import-bins"
"$HL" run t2 import-bins t1 a >/dev/null 2>&1; [ "$(cat "$T2/src/tools/replay/.cache/bin/xorgate-rs-a" 2>/dev/null)" = A ] && ok "import-bins as side a" || bad "import-bins side a"
printf Z > "$T1/out/bins/xorgate-replay"
"$HL" run t2 import-bins t1 >/dev/null 2>&1; [ "$(cat "$T2/logs/import-bins.rc")" != 0 ] && ok "import-bins refuses a sha mismatch" || bad "import-bins ignored a sha mismatch"

echo "== clean (only this tag)"
printf 'hl_t1_x\nhl_t1-2_y\nhl_t11_z\nother\n' > "$LOG/sessions"
printf 'c1\nc2\n' > "$LOG/ps.out"; : > "$LOG/docker.log"; : > "$LOG/killed"
STUB_PS_GREP=c "$HL" clean t1 >/dev/null 2>&1
[ "$(cat "$LOG/killed")" = "killed hl_t1_x" ] && ok "only session hl_t1_x killed" || { bad "sessions killed:"; cat "$LOG/killed"; }
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
