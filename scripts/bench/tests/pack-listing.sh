#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015  # test assertions: `[ … ]; expect $?` and `a && pass || fail`
# scripts/pack.sh on a tarball of MORE than 30 entries (F-E1-1, Phase 4 gate:
# `tar -tzf | head -30` under pipefail exited 141 once system/ was vendored,
# breaking every device-rs-deploy-dev run since 1b5db2a). Self-contained: a
# fake repo (pack.sh copied in, a dummy binary, vendored dirs with real
# SHA256SUMS) in a temp dir — the real target/ is never touched. Linux GNU tar
# is the failing case; run in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/pack-listing.sh
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
fail=0
pass() { echo "PASS $*"; }
failc() { echo "FAIL $*"; fail=1; }
expect() { [ "$1" = 0 ] && pass "$2" || failc "$2"; }
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

FAKE=$(mktemp -d); SHIMS=$(mktemp -d); LOGS=$(mktemp -d)
trap 'rm -rf "$FAKE" "$SHIMS" "$LOGS"' EXIT
mkdir -p "$FAKE/scripts" "$FAKE/systemd/bench" "$FAKE/python" "$FAKE/target/pack" "$FAKE/bin"
cp "$REPO/scripts/pack.sh" "$FAKE/scripts/pack.sh"
printf '#!/usr/bin/env bash\necho fake converter\n' > "$FAKE/scripts/install-rs.sh"; chmod 0644 "$FAKE/scripts/install-rs.sh"
printf '#!/bin/sh\necho fake\n' > "$FAKE/bin/xorgate-rs"
echo '[Unit]' > "$FAKE/systemd/xorgate-agent-rs.service"
echo '[Unit]' > "$FAKE/systemd/xorgate-updater-rs.service"
echo '[Service]' > "$FAKE/systemd/bench/90-bench-dev.conf"
echo '# imu' > "$FAKE/python/bno085_read.py"
printf '{"version": "t3st", "channel": "ci", "builtAt": "2026-10-21T00:00:00Z", "packageVersion": "0.0.0"}\n' \
  > "$FAKE/target/pack/build-info.json"
vendor() { # <dir> <n files>
  local d=$1 n=$2 i
  mkdir -p "$FAKE/$d"
  for i in $(seq 1 "$n"); do echo "$d $i" > "$FAKE/$d/f$i"; done
  (cd "$FAKE/$d" && sha f* > SHA256SUMS)
}
vendor media 20; vendor modem 6; vendor system 6
pack() { bash "$FAKE/scripts/pack.sh" --bin "$FAKE/bin/xorgate-rs" --out-dir "$FAKE/dist"; }

echo "== > 30 entries packs with exit 0"
pack >"$LOGS/out" 2>"$LOGS/err"; rc=$?
expect "$rc" "pack.sh rc=$rc (want 0; 141 = the SIGPIPE regression)"
T="$FAKE/dist/xorgate-agent-rs-t3st.tar.gz"
n=$(tar -tzf "$T" 2>/dev/null | wc -l | tr -d ' ')
[ "$n" -gt 30 ]; expect $? "tarball has $n entries (> 30)"
[ -f "$T.sha256" ] && (cd "$FAKE/dist" && sha -c "xorgate-agent-rs-t3st.tar.gz.sha256" >/dev/null); expect $? ".sha256 sidecar written and verifies"
grep -qx "sha256=$(cut -d' ' -f1 "$T.sha256")" "$LOGS/out"; expect $? "key=value outputs printed (script ran to the end)"
grep -q "^Contents ($n entries; first 30):" "$LOGS/err" && [ "$(tar -tzf "$T" | grep -cxFf - "$LOGS/err")" = 30 ]
expect $? "stderr shows the entry count + exactly the first 30 entries"

LV=$(tar -tvzf "$T" 2>/dev/null); grep -qE '^-rwxr-xr-x .* install-rs\.sh$' <<<"$LV"; expect $? "install-rs.sh packed at the tarball root, mode 0755 (Phase 5 WP1)"

echo "== a real listing error still fails the pack"
cat > "$SHIMS/tar" <<S
#!/usr/bin/env bash
[ "\${1:-}" = -tzf ] && { echo "tar: simulated listing failure" >&2; exit 2; }
exec $(command -v tar) "\$@"
S
chmod +x "$SHIMS/tar"
rm -rf "$FAKE/dist"
PATH="$SHIMS:$PATH" pack >"$LOGS/out2" 2>"$LOGS/err2"; rc=$?
[ "$rc" != 0 ]; expect $? "pack.sh rc=$rc when tar -tzf fails (want non-zero)"
[ ! -f "$FAKE/dist/xorgate-agent-rs-t3st.tar.gz.sha256" ]; expect $? "no .sha256 sidecar after a listing failure"

echo "== a missing vendored member still fails the pack"
rm -rf "$FAKE/dist"; echo "0000  ghost" >> "$FAKE/system/SHA256SUMS"
pack >"$LOGS/out3" 2>"$LOGS/err3"; rc=$?
[ "$rc" != 0 ] && grep -q "pack: missing system/ghost" "$LOGS/err3"; expect $? "pack.sh rc=$rc + 'missing system/ghost' on a SHA256SUMS entry not in the tarball"

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
