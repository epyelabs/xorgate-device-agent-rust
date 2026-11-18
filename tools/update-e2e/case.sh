#!/bin/bash
# One E9 case of the self-update rail, INSIDE a fresh e2e container (run.sh
# starts one container per case, so every case begins from an empty device).
#
#   case.sh <name>
#
# Mounts (run.sh): /bucket (the fake dist bucket, prep-bucket.sh), /e2e (this
# directory), /fixtures (the repo's fixtures/, for the TS-dumped sudoers
# bodies), /in/jsys (the JS repo's system/ @ 89b5c64).
#
# Every case: serve /bucket on 127.0.0.1:8080, write /etc/xorgate/agent.env
# (bucket URL with a trailing slash — stripped by the resolver), bootstrap
# build v1 with v1's own binary (first install: no previous tree), then run
# `xorgate-rs update …` through the installed wrapper as root, exactly as
# xorgate-updater-rs.service would, and check the outcome. Prints one line
# per check and a final `CASE <name>: PASS <n> FAIL <m>`; exit 1 on any FAIL.
set -uo pipefail
CASE=${1:?case name}
PASS=0
FAIL=0
E=/run/e2e
mkdir -p "$E"
ok() { echo "  ok   $*"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }
check() { local d=$1; shift; if "$@"; then ok "$d"; else bad "$d"; fi; }
mode_is() { [ "$(stat -c %a "$1" 2>/dev/null)" = "$2" ]; }
absent() { [ ! -e "$1" ]; }
same() { cmp -s "$1" "$2"; }
grep_line() { grep -qxF -- "$2" "$1"; }
# debian's sudo package ships /etc/sudoers.d/README; a grant is any 02x file.
no_grants() { ! ls /etc/sudoers.d | grep -q '^02'; }
no_tmp_left() { ! ls -d /tmp/xg-update-* /tmp/xg-sudoers-* >/dev/null 2>&1; }
P='[xorgate-rs] '
F='[xorgate-rs] update failed: '

BUCKET=http://127.0.0.1:8080
(cd /bucket && exec python3 -m http.server 8080 --bind 127.0.0.1 >"$E/http.log" 2>&1) &
for _ in $(seq 50); do curl -fs -o /dev/null "$BUCKET/" && break; sleep 0.1; done

agent_env() {
  mkdir -p /etc/xorgate
  printf 'DEVICE_ID=e2e-device\nDEVICE_DIST_BUCKET_URL="%s/"\n' "$BUCKET" >/etc/xorgate/agent.env
}

# The JS install a coexisting device carries (README §6): /opt/xorgate, the
# JS wrapper + unit + its User drop-in, sudoers 020–023 and NEWER-than-89b5c64
# modem/system assets. Q1: none of it may change.
fake_js() {
  mkdir -p /opt/xorgate/dist /etc/systemd/system/xorgate-agent.service.d /etc/sudoers.d \
    /etc/udev/rules.d /etc/NetworkManager/conf.d /etc/cron.d /usr/local/bin
  echo 'js worker (prod v0.1.2)' >/opt/xorgate/dist/worker.js
  printf '#!/usr/bin/env bash\nexec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" node /opt/xorgate/dist/cli.js "$@"\n' >/usr/local/bin/xorgate
  chmod 0755 /usr/local/bin/xorgate
  echo '# JS unit' >/etc/systemd/system/xorgate-agent.service
  printf '[Service]\nUser=pi\n' >/etc/systemd/system/xorgate-agent.service.d/10-user.conf
  for f in 020_xorgate-power 021_xorgate-clock 022_xorgate-update 023_xorgate-camrotate; do
    echo "# JS-installed $f (newer than 89b5c64)" >"/etc/sudoers.d/$f"
    chmod 0440 "/etc/sudoers.d/$f"
  done
  modem_daemon_newer
  for f in /usr/local/bin/xorgate-setclock /usr/local/bin/xorgate-camrotate; do
    printf '#!/bin/sh\n# JS-installed NEWER %s\n' "$f" >"$f"
    chmod 0755 "$f"
  done
  echo '# JS-installed NEWER cron' >/etc/cron.d/xorgate-fake-hwclock
}
modem_daemon_newer() {
  mkdir -p /usr/local/bin /etc/systemd/system /etc/udev/rules.d /etc/NetworkManager/conf.d
  printf '#!/bin/sh\n# NEWER sim7600-lte.sh than the 89b5c64 vendored copy\n' >/usr/local/bin/sim7600-lte.sh
  chmod 0755 /usr/local/bin/sim7600-lte.sh
  echo '# newer sim7600-lte.service' >/etc/systemd/system/sim7600-lte.service
  echo '# newer rules' >/etc/udev/rules.d/99-sim7600-lte.rules
  echo '# newer nm conf' >/etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf
}
SHARED=(/opt/xorgate /usr/local/bin/xorgate /etc/systemd/system/xorgate-agent.service
  /etc/systemd/system/xorgate-agent.service.d /etc/sudoers.d/020_xorgate-power
  /etc/sudoers.d/021_xorgate-clock /etc/sudoers.d/022_xorgate-update
  /etc/sudoers.d/023_xorgate-camrotate /usr/local/bin/sim7600-lte.sh
  /etc/systemd/system/sim7600-lte.service /etc/udev/rules.d /etc/NetworkManager/conf.d
  /usr/local/bin/xorgate-setclock /usr/local/bin/xorgate-camrotate /etc/cron.d /etc/xorgate)
DEVICE=(/opt /etc/systemd/system /usr/local/bin /etc/sudoers.d /etc/udev/rules.d
  /etc/NetworkManager/conf.d /etc/cron.d /etc/xorgate)

# snap <out> <paths…>: every file/link under the paths with mode, owner, size
# and sha256 — "byte-unchanged" means two snaps are identical.
snap() {
  local out=$1; shift
  : >"$out"
  for d in "$@"; do
    [ -e "$d" ] || { echo "ABSENT $d" >>"$out"; continue; }
    find "$d" -xdev \( -type f -o -type l -o -type d \) -print0 | sort -z |
      xargs -0 -r stat -c '%n %F %a %u:%g %s' >>"$out"
    find "$d" -xdev -type f -print0 | sort -z | xargs -0 -r sha256sum >>"$out"
  done
}

bootstrap() {
  local t
  t=$(mktemp -d /var/tmp/boot.XXXXXX)
  tar -xzf /bucket/xorgate-agent-rs-v1.tar.gz -C "$t"
  "$t/bin/xorgate-rs" update --tag v1 >"$E/boot.out" 2>"$E/boot.err"
  echo $? >"$E/boot.rc"
  rm -rf "$t"
  check "bootstrap v1 (first install, no previous tree) exits 0" [ "$(cat "$E/boot.rc")" = 0 ]
  check "bootstrap ends with updated to 'v1'" grep_line "$E/boot.out" "${P}updated to 'v1'. check it with: xorgate-rs status"
  : >"$E/systemctl.log"
}

# upd <args…>: the update as the updater unit runs it (root, via the wrapper).
upd() {
  /usr/local/bin/xorgate-rs update "$@" >"$E/out" 2>"$E/err"
  echo $? >"$E/rc"
  echo "  --- stdout"; sed 's/^/  | /' "$E/out"
  echo "  --- stderr"; sed 's/^/  | /' "$E/err"
}
rc_is() { [ "$(cat "$E/rc")" = "$1" ]; }
err_is() { [ "$(cat "$E/err")" = "$1" ]; }
err_starts() { case "$(cat "$E/err")" in "$1"*) return 0 ;; esac; return 1; }
sc_log_is() { [ "$(cat "$E/systemctl.log")" = "$1" ]; }

WRAPPER_EXPECTED=$(printf '%s\n%s' '#!/usr/bin/env bash' 'exec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" /opt/xorgate-rs/bin/xorgate-rs "$@"')

# ts_body <clock|camrotate|update> <user>: the body the REAL TS install*Sudoers
# wrote (fixtures/update_plan/update.json); `update` is mapped 022 → 024 with
# the three documented substitutions (O3/Q11).
ts_body() {
  python3 -I - "$1" "$2" <<'PY'
import json, sys
grant, user = sys.argv[1], sys.argv[2]
f = json.load(open("/fixtures/update_plan/update.json"))
for c in f["cases"]:
    i = c["input"]
    if c["op"] == "sudoers" and i["grant"] == grant and i["user"] == user and i["visudoExit"] == 0:
        b = c["expected"]["body"]
        if grant == "update":
            b = b.replace("# Installed by xorgate. ", "# Installed by xorgate-rs. ", 1)
            b = b.replace("# else. See device-agent src/agent/commands.ts + systemd/xorgate-updater.service.\n",
                          "# else. See device-agent-rust crates/xorgate-hal/src/privileged.rs + systemd/xorgate-updater-rs.service.\n", 1)
            b = b.replace(" --no-block xorgate-updater.service\n", " --no-block xorgate-updater-rs.service\n", 1)
        sys.stdout.write(b)
        break
else:
    sys.exit("no fixture body for %s/%s" % (grant, user))
PY
}
grant_is() { # <dest> <grant> <user>
  ts_body "$2" "$3" >"$E/expect-$2" && same "$1" "$E/expect-$2" && mode_is "$1" 440 && visudo -cf "$1" >/dev/null
}

# tree_is_tarball <dir> <tarball>: the live tree is exactly the tarball.
tree_is_tarball() {
  local t
  t=$(mktemp -d /var/tmp/tree.XXXXXX)
  tar -xzf "$2" -C "$t"
  diff -r --no-dereference "$1" "$t" >/dev/null && [ "$(cd "$1" && find . | sort)" = "$(cd "$t" && find . | sort)" ]
  local r=$?
  rm -rf "$t"
  return $r
}
member() { # <tarball> <member> <out>
  tar -xzf "$1" -O "$2" >"$3"
}

expect_failure_unchanged() { # <tag> <expected stderr prefix> [exact]
  local tag=$1 want=$2 exact=${3:-}
  agent_env
  fake_js
  bootstrap
  touch "$E/active-xorgate-agent-rs"
  snap "$E/before" "${DEVICE[@]}"
  upd --tag "$tag"
  snap "$E/after" "${DEVICE[@]}"
  check "exit 1" rc_is 1
  if [ -n "$exact" ]; then
    check "stderr is exactly: $want" err_is "$want"
  else
    check "stderr starts: $want" err_starts "$want"
  fi
  check "device byte-unchanged (/opt, units, wrappers, sudoers, assets)" diff -u "$E/before" "$E/after"
  check "no /opt/xorgate-rs.new left" absent /opt/xorgate-rs.new
  check "no /opt/xorgate-rs.old left" absent /opt/xorgate-rs.old
  check "no swap, no reload, no restart (systemctl not even asked)" sc_log_is ""
  check "download/sudoers scratch dirs removed" no_tmp_left
  check "live build still v1" grep -q '"version": "v1"' /opt/xorgate-rs/build-info.json
}

echo "CASE $CASE"
case "$CASE" in
happy-coexist)
  agent_env
  fake_js
  bootstrap
  check "bootstrap: inactive agent → installed; not started" grep_line "$E/boot.out" "${P}installed; not started (xorgate-agent-rs is inactive)"
  check "bootstrap: coexistence skip line" grep_line "$E/boot.out" "${P}shared assets owned by the JS agent (coexistence) — not refreshed"
  touch "$E/active-xorgate-agent-rs"
  snap "$E/shared-before" "${SHARED[@]}"
  upd
  snap "$E/shared-after" "${SHARED[@]}"
  L=/bucket/xorgate-agent-rs-latest.tar.gz
  HEX=$(cut -d' ' -f1 "$L.sha256")
  VLINE=$(/opt/xorgate-rs/bin/xorgate-rs -v)
  printf '%s\n' \
    "${P}downloading $BUCKET/xorgate-agent-rs-latest.tar.gz" \
    "${P}downloading $BUCKET/xorgate-agent-rs-latest.tar.gz.sha256" \
    "${P}checksum ok (sha256 $HEX)" \
    "${P}checking the staged build runs on this device" \
    "${P}staged $VLINE" \
    "${P}shared assets owned by the JS agent (coexistence) — not refreshed" \
    "${P}updated to 'latest'. check it with: xorgate-rs status" >"$E/want-out"
  check "exit 0" rc_is 0
  check "stdout is exactly the 7 expected lines (download → checksum ok → smoke → skip → done)" diff -u "$E/want-out" "$E/out"
  check "stderr empty" err_is ""
  check "staged build is v2 (latest alias)" grep -q '"version": "v2"' /opt/xorgate-rs/build-info.json
  check "-v of the live binary names v2" grep -q '(v2, bench)' <<<"$VLINE"
  check "/opt/xorgate-rs is exactly the tarball (modes, members, bytes)" tree_is_tarball /opt/xorgate-rs "$L"
  check "/opt/xorgate-rs is root-owned (tar as root keeps the CI's 0:0 entries)" bash -c '[ -z "$(find /opt/xorgate-rs ! -user 0)" ]'

  member "$L" systemd/xorgate-agent-rs.service "$E/unit"
  member "$L" systemd/xorgate-updater-rs.service "$E/updater"
  check "unit bytes == tarball's, 0644" bash -c "cmp -s $E/unit /etc/systemd/system/xorgate-agent-rs.service && [ \$(stat -c %a /etc/systemd/system/xorgate-agent-rs.service) = 644 ]"
  check "updater unit bytes == tarball's, 0644" bash -c "cmp -s $E/updater /etc/systemd/system/xorgate-updater-rs.service && [ \$(stat -c %a /etc/systemd/system/xorgate-updater-rs.service) = 644 ]"
  check "wrapper bytes exact, 0755, regular file" bash -c '[ "$(cat /usr/local/bin/xorgate-rs)" = "$0" ] && [ -f /usr/local/bin/xorgate-rs ] && [ ! -L /usr/local/bin/xorgate-rs ] && [ $(stat -c %a /usr/local/bin/xorgate-rs) = 755 ] && [ "$(tail -c1 /usr/local/bin/xorgate-rs | od -An -c | tr -d " ")" = "\n" ]' "$WRAPPER_EXPECTED"
  check "024 == TS 022 body (user pi, JS drop-in fallback) with the O3 substitutions, 0440, visudo -cf ok" grant_is /etc/sudoers.d/024_xorgate-update-rs update pi
  check "024 grant line is the exact-args line" grep_line /etc/sudoers.d/024_xorgate-update-rs "pi ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater-rs.service"
  check ".old removed" absent /opt/xorgate-rs.old
  check ".new gone" absent /opt/xorgate-rs.new
  check "systemctl: is-active, daemon-reload, restart (active before the swap)" sc_log_is "$(printf 'is-active --quiet xorgate-agent-rs\ndaemon-reload\nrestart xorgate-agent-rs')"
  check "JS install + shared assets + sudoers 020–023 byte-unchanged (Q1, CF62)" diff -u "$E/shared-before" "$E/shared-after"
  check "scratch dirs removed" no_tmp_left
  ;;
rust-only)
  agent_env
  modem_daemon_newer
  mkdir -p /etc/systemd/system/xorgate-agent.service.d
  printf '[Service]\nUser=pi\n' >/etc/systemd/system/xorgate-agent.service.d/10-user.conf
  bootstrap
  upd --tag v2sys
  L=/bucket/xorgate-agent-rs-v2sys.tar.gz
  check "exit 0" rc_is 0
  check "stderr empty" err_is ""
  check "no coexistence skip line" bash -c "! grep -qF 'coexistence' $E/out"
  check "modem refreshed line (TS verbatim)" grep_line "$E/out" "${P}modem assets refreshed — applies on next boot, or now with: sudo systemctl restart sim7600-lte"
  check "inactive → installed; not started" grep_line "$E/out" "${P}installed; not started (xorgate-agent-rs is inactive)"
  check "final line" grep_line "$E/out" "${P}updated to 'v2sys'. check it with: xorgate-rs status"
  while read -r src dest m; do
    member "$L" "$src" "$E/m"
    check "$dest == tarball $src, mode $m" bash -c "cmp -s $E/m $dest && [ \$(stat -c %a $dest) = $m ]"
  done <<'EOF'
modem/sim7600-lte.sh /usr/local/bin/sim7600-lte.sh 755
modem/sim7600-lte.service /etc/systemd/system/sim7600-lte.service 644
modem/99-sim7600-lte.rules /etc/udev/rules.d/99-sim7600-lte.rules 644
modem/99-sim7600-unmanaged.conf /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf 644
EOF
  while read -r src dest m; do
    check "$dest == JS 89b5c64 $src, mode $m" bash -c "cmp -s /in/jsys/$src $dest && [ \$(stat -c %a $dest) = $m ]"
  done <<'EOF'
system/xorgate-setclock /usr/local/bin/xorgate-setclock 755
system/xorgate-fake-hwclock.cron /etc/cron.d/xorgate-fake-hwclock 644
system/xorgate-camrotate /usr/local/bin/xorgate-camrotate 755
EOF
  check "021 == TS installClockSudoers body (pi), 0440, visudo ok" grant_is /etc/sudoers.d/021_xorgate-clock clock pi
  check "023 == TS installCamRotateSudoers body (pi), 0440, visudo ok" grant_is /etc/sudoers.d/023_xorgate-camrotate camrotate pi
  check "024 written, visudo ok" grant_is /etc/sudoers.d/024_xorgate-update-rs update pi
  check "022 (the JS updater grant) never written" absent /etc/sudoers.d/022_xorgate-update
  check "no restart (inactive)" sc_log_is "$(printf 'is-active --quiet xorgate-agent-rs\ndaemon-reload')"
  check "scratch dirs removed" no_tmp_left
  ;;
rust-only-root-no-modem)
  agent_env
  mkdir -p /etc/systemd/system/xorgate-agent-rs.service.d /etc/systemd/system/xorgate-agent.service.d
  printf '[Service]\nUser=root\n' >/etc/systemd/system/xorgate-agent-rs.service.d/10-user.conf
  printf '[Service]\nUser=pi\n' >/etc/systemd/system/xorgate-agent.service.d/10-user.conf
  bootstrap
  upd --tag v2sys
  check "exit 0" rc_is 0
  check "no modem daemon → modem assets not installed" absent /etc/udev/rules.d/99-sim7600-lte.rules
  check "no modem refreshed line" bash -c "! grep -qF 'modem assets' $E/out"
  check "system assets installed" bash -c "cmp -s /in/jsys/system/xorgate-setclock /usr/local/bin/xorgate-setclock"
  check "Rust drop-in (User=root) wins over the JS one → no sudoers grant at all (only the distro README)" no_grants
  ;;
rust-only-no-user)
  agent_env
  bootstrap
  upd --tag v2sys
  check "exit 0" rc_is 0
  check "no drop-in → no sudoers grant at all (root fallback)" no_grants
  check "updater unit still installed" bash -c "[ -f /etc/systemd/system/xorgate-updater-rs.service ]"
  ;;
restart-fails)
  agent_env
  fake_js
  bootstrap
  touch "$E/active-xorgate-agent-rs" "$E/restart-fails"
  upd
  check "exit 0 (restart failure is a warning)" rc_is 0
  check "warning line" grep_line "$E/out" "${P}warning: 'systemctl restart xorgate-agent-rs' failed: Job for xorgate-agent-rs.service failed because the control process exited with error code."
  check "manual hint" grep_line "$E/out" "${P}the new build is installed; restart manually: sudo systemctl restart xorgate-agent-rs"
  check "final line still printed" grep_line "$E/out" "${P}updated to 'latest'. check it with: xorgate-rs status"
  check "new build installed" grep -q '"version": "v2"' /opt/xorgate-rs/build-info.json
  check ".old removed" absent /opt/xorgate-rs.old
  ;;
visudo-refuses)
  agent_env
  fake_js
  bootstrap
  rm -f /etc/sudoers.d/024_xorgate-update-rs
  PATH=/e2e/shims-refuse:$PATH upd
  check "exit 0 (refusal is a warning)" rc_is 0
  check "warning line (TS text)" grep_line "$E/out" "${P}warning: update sudoers drop-in failed visudo validation; remote updates will not work"
  check "no 024 written without a passing visudo -cf" absent /etc/sudoers.d/024_xorgate-update-rs
  check "update otherwise complete" grep_line "$E/out" "${P}updated to 'latest'. check it with: xorgate-rs status"
  check "sudoers draft dir removed" no_tmp_left
  ;;
bad-sha)
  H2=$(cut -d' ' -f1 /bucket/xorgate-agent-rs-v2.tar.gz.sha256)
  HB=$(sha256sum /bucket/xorgate-agent-rs-badsha.tar.gz | cut -d' ' -f1)
  expect_failure_unchanged badsha "${F}checksum mismatch for xorgate-agent-rs-badsha.tar.gz (published $H2, downloaded $HB)" exact
  ;;
missing-sidecar)
  expect_failure_unchanged nosum "${F}no checksum published for xorgate-agent-rs-nosum.tar.gz (curl: (22) The requested URL returned error: 404); refusing an unverified build" exact
  ;;
sidecar-wrong-name)
  expect_failure_unchanged wrongname "${F}no checksum published for xorgate-agent-rs-wrongname.tar.gz (xorgate-agent-rs-wrongname.tar.gz.sha256 has no entry for it); refusing an unverified build" exact
  ;;
missing-member)
  expect_failure_unchanged nomember "${F}downloaded build is missing bin/xorgate-rs, systemd/xorgate-agent-rs.service, build-info.json" exact
  ;;
wrong-arch)
  expect_failure_unchanged wrongarch "${F}staged build does not run on this device ("
  ;;
truncated-binary)
  expect_failure_unchanged truncated "${F}staged build does not run on this device (killed by a signal); not installed" exact
  ;;
not-a-tarball)
  expect_failure_unchanged garbage "${F}extract failed: "
  ;;
download-404)
  expect_failure_unchanged nosuch "${F}download failed (curl: (22) The requested URL returned error: 404); check the version/tag and bucket" exact
  ;;
no-bucket)
  agent_env
  bootstrap
  echo 'DEVICE_ID=e2e-device' >/etc/xorgate/agent.env
  snap "$E/before" "${DEVICE[@]}"
  upd
  snap "$E/after" "${DEVICE[@]}"
  check "exit 1" rc_is 1
  check "stderr exact" err_is "${F}no dist bucket URL — set DEVICE_DIST_BUCKET_URL in /etc/xorgate/agent.env or pass --from <url>"
  check "device unchanged" diff -u "$E/before" "$E/after"
  ;;
not-root)
  agent_env
  bootstrap
  snap "$E/before" "${DEVICE[@]}"
  runuser -u pi -- /usr/local/bin/xorgate-rs update >"$E/out" 2>"$E/err"
  echo $? >"$E/rc"
  snap "$E/after" "${DEVICE[@]}"
  check "exit 1" rc_is 1
  check "stderr exact (Q9 hint)" err_is "${F}run with sudo (writes /opt, /usr/local/bin, /etc/systemd): sudo xorgate-rs update"
  check "device unchanged" diff -u "$E/before" "$E/after"
  ;;
ro-systemd)
  # Injected failure AFTER the swap: the first install step (unit copy) hits a
  # read-only /etc/systemd/system → rollback restores v1.
  agent_env
  fake_js
  bootstrap
  touch "$E/active-xorgate-agent-rs"
  snap "$E/opt-before" /opt/xorgate-rs /usr/local/bin/xorgate-rs
  mount --bind /etc/systemd/system /etc/systemd/system && mount -o remount,bind,ro /etc/systemd/system
  check "injection armed (/etc/systemd/system read-only)" bash -c '! touch /etc/systemd/system/.probe 2>/dev/null'
  upd
  snap "$E/opt-after" /opt/xorgate-rs /usr/local/bin/xorgate-rs
  check "exit 1" rc_is 1
  check "stderr exact (rolled back)" err_is "${F}install step failed, rolled back: Read-only file system (os error 30), copyfile '/opt/xorgate-rs/systemd/xorgate-agent-rs.service' -> '/etc/systemd/system/xorgate-agent-rs.service'"
  check "/opt/xorgate-rs + wrapper byte-identical to v1" diff -u "$E/opt-before" "$E/opt-after"
  check "no .old left (renamed back)" absent /opt/xorgate-rs.old
  check "no .new left" absent /opt/xorgate-rs.new
  check "no reload/restart after a failed install" sc_log_is "is-active --quiet xorgate-agent-rs"
  ;;
ro-sudoers)
  # Injected failure LATE in the install (the 024 copy, after unit, wrapper
  # and updater unit were rewritten): /opt rolls back to v1. Since v0.1.1
  # (review-v011-wp-a MUST-2) the rollback also restores every file the run
  # replaced (the TS restored ONLY /opt); pinned by the unit test
  # cli::update::tests::a_failed_install_restores_the_replaced_files_too.
  agent_env
  fake_js
  bootstrap
  touch "$E/active-xorgate-agent-rs"
  snap "$E/opt-before" /opt/xorgate-rs
  mount --bind /etc/sudoers.d /etc/sudoers.d && mount -o remount,bind,ro /etc/sudoers.d
  upd
  snap "$E/opt-after" /opt/xorgate-rs
  check "exit 1" rc_is 1
  check "stderr: rolled back at the 024 copy" err_starts "${F}install step failed, rolled back: Read-only file system (os error 30), copyfile '/tmp/xg-sudoers-"
  check "/opt/xorgate-rs byte-identical to v1" diff -u "$E/opt-before" "$E/opt-after"
  check "no .old/.new left" bash -c '[ ! -e /opt/xorgate-rs.old ] && [ ! -e /opt/xorgate-rs.new ]'
  check "sudoers draft dir removed" no_tmp_left
  ;;
uid501-tarball)
  # D-WP4-11: a tarball whose entries are owned by uid 501 (a Mac/bsdtar pack)
  # still installs a root-owned /opt/xorgate-rs (tar --no-same-owner).
  agent_env
  fake_js
  bootstrap
  L=/bucket/xorgate-agent-rs-uid501.tar.gz
  check "precondition: the tarball's entries are owned by 501/20" bash -c "tar -tvzf $L --numeric-owner | awk '{print \$2}' | sort -u | grep -qx 501/20 && [ \$(tar -tvzf $L --numeric-owner | awk '{print \$2}' | sort -u | wc -l) = 1 ]"
  upd --tag uid501
  check "exit 0" rc_is 0
  check "installed build is uid501" grep -q '"version": "uid501"' /opt/xorgate-rs/build-info.json
  check "/opt/xorgate-rs is root:root throughout (no uid 501 left)" bash -c '[ -z "$(find /opt/xorgate-rs \( ! -user 0 -o ! -group 0 \))" ]'
  check "content == tarball" tree_is_tarball /opt/xorgate-rs "$L"
  check "modes kept (bin 0755, unit 0644)" bash -c '[ $(stat -c %a /opt/xorgate-rs/bin/xorgate-rs) = 755 ] && [ $(stat -c %a /opt/xorgate-rs/systemd/xorgate-agent-rs.service) = 644 ]'
  ;;
old-busy)
  # D-WP4-8: moving the live tree aside fails. `.old` is an (empty, read-only)
  # mountpoint: rm -rf of it fails silently (EBUSY), then rename(live → .old)
  # fails with EBUSY → staged tree dropped, live tree untouched, one fail line.
  agent_env
  fake_js
  bootstrap
  touch "$E/active-xorgate-agent-rs"
  mkdir -p /opt/xorgate-rs.old
  mount -t tmpfs -o ro,size=1m tmpfs /opt/xorgate-rs.old
  check "injection armed (.old is a mountpoint)" mountpoint -q /opt/xorgate-rs.old
  snap "$E/before" "${DEVICE[@]}"
  upd
  snap "$E/after" "${DEVICE[@]}"
  check "exit 1" rc_is 1
  check "stderr exact (one fail line, no uncaught throw)" err_is "${F}Device or resource busy (os error 16), rename '/opt/xorgate-rs' -> '/opt/xorgate-rs.old'"
  check "device byte-unchanged" diff -u "$E/before" "$E/after"
  check "staged tree removed" absent /opt/xorgate-rs.new
  check "live build still v1" grep -q '"version": "v1"' /opt/xorgate-rs/build-info.json
  check "no reload/restart (only the pre-swap is-active)" sc_log_is "is-active --quiet xorgate-agent-rs"
  check "scratch dirs removed" no_tmp_left
  ;;
*)
  echo "unknown case $CASE" >&2
  exit 2
  ;;
esac
echo "CASE $CASE: PASS $PASS FAIL $FAIL"
[ "$FAIL" = 0 ]
