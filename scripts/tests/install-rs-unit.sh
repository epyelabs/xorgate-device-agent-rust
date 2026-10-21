#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2319  # test assertions: `a && pass || failc`, `[ … ]; expect $?`
# Fast smoke of scripts/install-rs.sh (Phase 5 WP1): usage, exit codes,
# preconditions and the fail-closed paths, plus one convert → re-run →
# revert → re-convert → purge loop over a fake build. It WRITES TO THE
# SYSTEM PATHS it tests (/etc/xorgate, /opt/xorgate*, /etc/systemd/system …),
# so it refuses to run outside a throwaway container:
#   docker run --rm -v "$PWD:/src:ro" -w /src debian:bookworm bash scripts/tests/install-rs-unit.sh
# The full matrix (real binary, Conflicts= modelled both ways, every §5 rule)
# is tools/convert-e2e (WP2). systemctl here is a small stub: enablement =
# multi-user.target.wants symlinks, activity = flag files, `start
# xorgate-agent-rs` runs `<ExecStart>` for 1 s (alive = active) and applies
# the unit's Conflicts=.
set -uo pipefail
[ -f /.dockerenv ] && [ "$(id -u)" = 0 ] || { echo "refusing: run me as root in a throwaway container (see header)"; exit 2; }
SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
INSTALL_RS="$SRC/scripts/install-rs.sh"
T=/tmp/irs-unit
fail=0
pass() { echo "PASS $*"; }
failc() { echo "FAIL $*"; fail=1; }
expect() { [ "$1" = "$2" ] && pass "$3 (exit $1)" || failc "$3 (exit $1, want $2)"; }
has() { grep -qF -- "$2" "$1" && pass "$3" || { failc "$3"; sed 's/^/    | /' "$1"; }; }

NET=1
if ! command -v visudo >/dev/null || ! command -v curl >/dev/null || ! command -v python3 >/dev/null; then
  { apt-get update -qq && apt-get install -y -qq sudo curl python3 ca-certificates; } >/dev/null 2>&1 || NET=0
fi
command -v visudo >/dev/null && command -v curl >/dev/null && command -v python3 >/dev/null || NET=0
[ "$NET" = 1 ] || echo "NOTE: sudo/curl/python3 not installable: visudo + bucket cases skipped"
id pi >/dev/null 2>&1 || useradd -m pi

rm -rf "$T"; mkdir -p "$T/bin" "$T/state/active"
# ---- systemctl stub (before any real one on PATH) ----
cat > "$T/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
S=/tmp/irs-unit/state; D=/etc/systemd/system; W=$D/multi-user.target.wants
echo "systemctl $*" >> "$S/systemctl.log"
q=0; args=()
for a in "$@"; do case $a in --quiet|-q) q=1 ;; --no-pager|--no-block) ;; *) args+=("$a") ;; esac; done
verb=${args[0]:-}; u=${args[1]:-}; u=${u%.service}
conflicts() { sed -n 's/^Conflicts=//p' "$D/$1.service" 2>/dev/null | tr ' ' '\n' | sed 's/\.service$//'; }
stop() { rm -f "$S/active/$1"; }
start() {
  [ -f "$D/$1.service" ] || { echo "Unit $1.service not found." >&2; return 5; }
  local x c
  if [ "$1" = xorgate-agent-rs ]; then
    x=$(sed -n 's/^ExecStart=//p' "$D/$1.service" | head -n1)
    # shellcheck disable=SC2086
    timeout 1 $x >/dev/null 2>&1; [ $? = 124 ] || { stop "$1"; echo "Job for $1.service failed." >&2; return 1; }
  fi
  for c in $(conflicts "$1"); do stop "$c"; done
  for c in xorgate-agent xorgate-agent-rs; do conflicts "$c" | grep -qx "$1" && stop "$c"; done
  touch "$S/active/$1"
}
case $verb in
  daemon-reload) exit 0 ;;
  is-enabled)
    [ -f "$D/$u.service" ] || { echo "Failed to get unit file state for $u.service: No such file or directory" >&2; exit 1; }
    if [ -L "$W/$u.service" ]; then [ $q = 1 ] || echo enabled; exit 0; fi
    [ $q = 1 ] || echo disabled; exit 1 ;;
  is-active) if [ -f "$S/active/$u" ]; then [ $q = 1 ] || echo active; exit 0; fi; [ $q = 1 ] || echo inactive; exit 3 ;;
  enable) mkdir -p "$W"; ln -sfn "$D/$u.service" "$W/$u.service" ;;
  disable) rm -f "$W/$u.service" ;;
  start) start "$u" ;;
  stop) stop "$u" ;;
  restart) stop "$u"; start "$u" ;;
  *) echo "stub: unsupported: $*" >&2; exit 1 ;;
esac
STUB
chmod 0755 "$T/bin/systemctl"
export PATH="$T/bin:$PATH"

# ---- a fake build (real units + install-rs.sh; a shell "binary") ----
# shellcheck disable=SC2016  # the fake binary's $1 is written literally
mkbuild() { # <name> <banner> <run-exits:0|1> [omit-member]
  local d="$T/build-$1"
  rm -rf "$d"; mkdir -p "$d/bin" "$d/systemd/bench"
  printf '#!/bin/sh\ncase "$1" in -v) echo "%s" ;; run) [ %s = 0 ] && exec sleep 600; exit 1 ;; *) exit 0 ;; esac\n' "$2" "$3" > "$d/bin/xorgate-rs"
  chmod 0755 "$d/bin/xorgate-rs"
  printf '{\n  "version": "%s",\n  "channel": "test",\n  "builtAt": "2026-10-21T00:00:00Z",\n  "packageVersion": "0.0.0"\n}\n' "$1" > "$d/build-info.json"
  cp "$SRC/systemd/xorgate-agent-rs.service" "$SRC/systemd/xorgate-updater-rs.service" "$d/systemd/"
  cp "$SRC/systemd/bench/90-bench-dev.conf" "$d/systemd/bench/"
  install -m 0755 "$INSTALL_RS" "$d/install-rs.sh"
  [ -z "${4:-}" ] || rm -f "$d/$4"
  mkdir -p "$T/dist"
  tar --owner=0 --group=0 -czf "$T/dist/xorgate-agent-rs-$1.tar.gz" -C "$d" .
  (cd "$T/dist" && sha256sum "xorgate-agent-rs-$1.tar.gz" > "xorgate-agent-rs-$1.tar.gz.sha256")
}
mkbuild good "xorgate-rs 0.0.0-test (fake)" 0
mkbuild runfails "xorgate-rs 0.0.0-test (fake)" 1
mkbuild banner "something-else 1.0" 0
mkbuild nomember "xorgate-rs 0.0.0-test (fake)" 0 install-rs.sh
cp "$T/dist/xorgate-agent-rs-good.tar.gz" "$T/dist/xorgate-agent-rs-latest.tar.gz"
(cd "$T/dist" && sha256sum xorgate-agent-rs-latest.tar.gz > xorgate-agent-rs-latest.tar.gz.sha256)
GOOD="$T/dist/xorgate-agent-rs-good.tar.gz"

# ---- a minimal JS-provisioned device (backend install-script.ts layout) ----
wipe() {
  rm -rf /etc/xorgate /opt/xorgate /opt/xorgate-rs /opt/xorgate-rs.new /opt/xorgate-rs.old /var/lib/xorgate-rs-install \
    /usr/local/bin/xorgate /usr/local/bin/xorgate-rs /etc/systemd/system/xorgate-* /etc/sudoers.d/02[0-4]_xorgate-* \
    /etc/systemd/system/multi-user.target.wants/xorgate-* "$T/state/active"/* /run/xorgate-rs-install.lock
}
provision() { # [svc user: pi|root] [DEVICE_DIST_VERSION]
  local u=${1:-pi} pin=${2:-latest}
  wipe
  install -d -m 0700 -o "$u" -g "$u" /etc/xorgate
  for f in device.cert.pem device.key.pem root-ca.pem; do echo "dummy $f" | install -m 0600 -o "$u" -g "$u" /dev/stdin "/etc/xorgate/$f"; done
  printf 'DEVICE_ID=dev-1\nIOT_DATA_ENDPOINT=x.iot\nMODE=real\nDEVICE_DIST_BUCKET_URL=http://127.0.0.1:8765/\nDEVICE_DIST_VERSION=%s\n' "$pin" \
    | install -m 0600 -o "$u" -g "$u" /dev/stdin /etc/xorgate/agent.env
  mkdir -p /opt/xorgate/dist /opt/xorgate/node_modules/a /etc/sudoers.d /etc/systemd/system/multi-user.target.wants
  echo "js" > /opt/xorgate/dist/cli.js; echo "m" > /opt/xorgate/node_modules/a/index.js
  # shellcheck disable=SC2016  # the JS wrapper's $ are literal
  printf '#!/usr/bin/env bash\nexec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" node /opt/xorgate/dist/cli.js "$@"\n' > /usr/local/bin/xorgate
  chmod +x /usr/local/bin/xorgate
  printf '[Unit]\nDescription=js\n[Service]\nExecStart=/usr/bin/node /opt/xorgate/dist/worker.js\n[Install]\nWantedBy=multi-user.target\n' > /etc/systemd/system/xorgate-agent.service
  if [ "$u" != root ]; then
    mkdir -p /etc/systemd/system/xorgate-agent.service.d
    printf '[Service]\nUser=%s\nGroup=%s\n' "$u" "$u" > /etc/systemd/system/xorgate-agent.service.d/10-user.conf
  fi
  echo "$u ALL=(root) NOPASSWD: /usr/bin/systemctl reboot" > /etc/sudoers.d/020_xorgate-power; chmod 0440 /etc/sudoers.d/020_xorgate-power
  systemctl enable xorgate-agent; systemctl start xorgate-agent
}
snap() { # every path the converter could touch (independent of install-rs.sh's manifest)
  find /etc/xorgate /usr/local/bin /etc/systemd/system /etc/sudoers.d /var/lib/xorgate-rs-install /opt -xdev \
    \( -type f -printf '%y %m %U:%G %p ' -exec sh -c 'sha256sum < "$1" | cut -c1-64' _ {} \; \) \
    -o \( ! -type f -printf '%y %m %U:%G %p %l\n' \) 2>/dev/null | sort
}
run() { # <out> <args…>: install-rs.sh, stdin not a tty
  local o=$1; shift
  bash "$INSTALL_RS" "$@" > "$o" 2>&1 < /dev/null
}
O="$T/out"

echo "== usage (exit 2) + help (exit 0)"
run "$O" --help; expect $? 0 "--help"; has "$O" "TEST-ONLY: XORGATE_RS_CONVERT_FAIL_AT" "--help documents the test-only knob"
for a in "--bogus" "--purge" "--force" "--revert --force" "--revert --status" "--status --tag v1" "--tarball" "--revert -y" "--from"; do
  # shellcheck disable=SC2086
  run "$O" $a; expect $? 2 "usage: $a"
done

echo "== preconditions (exit 3, nothing changed)"
wipe
run "$O" -y; expect $? 3 "not JS-provisioned (clean box)"; has "$O" "[install-rs] refused: not a JS-provisioned device: missing /etc/xorgate/agent.env /opt/xorgate /etc/systemd/system/xorgate-agent.service" "names every missing piece"
[ ! -e /var/lib/xorgate-rs-install ] && [ ! -e /opt/xorgate-rs ]; expect $? 0 "nothing written"
mkdir -p /etc/xorgate && echo DEVICE_ID=x > /etc/xorgate/agent.env
run "$O" -y; expect $? 3 "partially provisioned (agent.env only)"
run "$O" --status; expect $? 0 "--status on a clean box"; [ "$(head -n1 "$O")" = state=not-converted ]; expect $? 0 "--status first line state=not-converted"
run "$O" --revert; expect $? 0 "--revert with no record"; has "$O" "nothing to revert" "no-op revert says so"
provision pi
chmod 0755 "$T" "$T/dist"; chmod 0644 "$GOOD" "$GOOD.sha256"
su -s /bin/bash nobody -c "bash '$INSTALL_RS' -y --tarball '$GOOD'" > "$O" 2>&1 < /dev/null; expect $? 3 "not root (convert)"
su -s /bin/bash nobody -c "bash '$INSTALL_RS' --revert" > "$O" 2>&1 < /dev/null; expect $? 3 "not root (--revert)"
su -s /bin/bash nobody -c "bash '$INSTALL_RS' --status" > "$O" 2>&1 < /dev/null; expect $? 0 "not root (--status)"

echo "== fail-closed (exit 1, device unchanged, no record)"
provision pi; M0=$(snap)
failclosed() { # <label> <args…>
  local l=$1; shift
  run "$O" -y "$@"; expect $? 1 "$l"
  [ "$(snap)" = "$M0" ]; expect $? 0 "$l: device byte-unchanged"
  grep -q '^\[install-rs\] nothing was changed (exit 1)$' "$O"; expect $? 0 "$l: says nothing was changed"
}
sed -i '/^DEVICE_DIST_BUCKET_URL=/d' /etc/xorgate/agent.env; M0=$(snap)
failclosed "no bucket URL anywhere"
has "$O" "no dist bucket URL — set DEVICE_DIST_BUCKET_URL in /etc/xorgate/agent.env or pass --from <url>" "no-bucket message"
cp "$GOOD" "$T/nosum.tar.gz"
failclosed "--tarball without its .sha256" --tarball "$T/nosum.tar.gz"
# shellcheck disable=SC2046  # seq output splits on purpose
cp "$GOOD" "$T/badsum.tar.gz"; echo "$(printf '0%.0s' $(seq 64))  badsum.tar.gz" > "$T/badsum.tar.gz.sha256"
failclosed "checksum mismatch" --tarball "$T/badsum.tar.gz"; has "$O" "checksum mismatch for badsum.tar.gz" "mismatch message"
cp "$GOOD" "$T/othername.tar.gz"; sed 's/xorgate-agent-rs-good/other/' "$GOOD.sha256" > "$T/othername.tar.gz.sha256"
failclosed "sidecar names another file" --tarball "$T/othername.tar.gz"
failclosed "missing member install-rs.sh" --tarball "$T/dist/xorgate-agent-rs-nomember.tar.gz"; has "$O" "the build is missing install-rs.sh" "missing-member message"
failclosed "smoke-exec banner wrong" --tarball "$T/dist/xorgate-agent-rs-banner.tar.gz"; has "$O" "staged build does not run on this device" "smoke message"

echo "== convert → re-run → revert (keep) → revert again"
provision pi v0.1.2; M0=$(snap)
run "$O" -y --tarball "$GOOD"; expect $? 0 "convert (--tarball)"
echo "-- transcript (convert):"; sed 's/^/    | /' "$O"
has "$O" "[install-rs] converted: xorgate-agent-rs good (test) active; xorgate-agent stopped + disabled. revert with: sudo /opt/xorgate-rs/install-rs.sh --revert" "I1 convert final line"
has "$O" "WARNING: /etc/xorgate/agent.env pins DEVICE_DIST_VERSION=v0.1.2 (the JS agent's build)" "JS-pin warning (Q11)"
[ "$(systemctl is-enabled xorgate-agent-rs)/$(systemctl is-active xorgate-agent-rs)/$(systemctl is-enabled xorgate-agent)/$(systemctl is-active xorgate-agent)" = enabled/active/disabled/inactive ]
expect $? 0 "rs enabled+active, JS disabled+inactive"
cmp -s /etc/systemd/system/xorgate-agent-rs.service "$SRC/systemd/xorgate-agent-rs.service" && grep -qx 'Conflicts=xorgate-agent.service' /etc/systemd/system/xorgate-agent-rs.service
expect $? 0 "shipped unit byte-equal, with Conflicts="
cmp -s /etc/systemd/system/xorgate-agent-rs.service.d/10-user.conf /etc/systemd/system/xorgate-agent.service.d/10-user.conf; expect $? 0 "10-user.conf byte copy"
[ ! -e /etc/systemd/system/xorgate-agent-rs.service.d/90-bench-dev.conf ]; expect $? 0 "CF67: bench drop-in never installed"
cmp -s <(bash -c "source '$INSTALL_RS'; rs_dist_dropin_body") /etc/systemd/system/xorgate-updater-rs.service.d/10-dist.conf \
  && grep -qx 'Environment=DEVICE_DIST_VERSION=latest' /etc/systemd/system/xorgate-updater-rs.service.d/10-dist.conf
expect $? 0 "10-dist.conf == rs_dist_dropin_body (golden-pinned, §8)"
if [ "$NET" = 1 ]; then
  [ "$(stat -c %a /etc/sudoers.d/024_xorgate-update-rs)" = 440 ] && grep -qx 'pi ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater-rs.service' /etc/sudoers.d/024_xorgate-update-rs
  expect $? 0 "024 for pi, 0440"
fi
grep -qx 'state=converted' /var/lib/xorgate-rs-install/record && grep -qx 'jsEnabled=enabled' /var/lib/xorgate-rs-install/record \
  && grep -qx 'jsActive=active' /var/lib/xorgate-rs-install/record && grep -qx 'rsPreexisting=no' /var/lib/xorgate-rs-install/record \
  && [ "$(cut -d= -f1 /var/lib/xorgate-rs-install/record | tr '\n' ' ')" = "state startedAt convertedAt version channel sha256 source jsEnabled jsActive rsPreexisting serviceUser installed " ]
expect $? 0 "I2 record keys in order + values"
M1=$(snap)
run "$O" -y --tarball "$GOOD"; expect $? 0 "re-run"
grep -q '^\[install-rs\] already converted (.*, good); install verified, changed (0). change the build with: sudo systemctl start xorgate-updater-rs$' "$O"; expect $? 0 "re-run: I1 line with changed (0)"
[ "$(snap)" = "$M1" ]; expect $? 0 "re-run: device byte-identical"
# WP3 hand-off: the dead-man runs --revert with stdin from /dev/null and no -y;
# it must never prompt. A stdin that stays open (a pipe nobody writes) would
# hang a prompt: bound it.
timeout 60 bash "$INSTALL_RS" --revert > "$O" 2>&1 < <(sleep 120); expect $? 0 "revert (keep), stdin an open pipe, no -y: no prompt"
grep -qi '\[y/N\]' "$O"; [ $? = 1 ]; expect $? 0 "revert printed no confirmation question"
has "$O" "[install-rs] reverted: xorgate-agent enabled + active; xorgate-agent-rs disabled (files kept). JS install matches the pre-conversion record: yes" "I1 revert final line"
ls /var/lib/xorgate-rs-install/record.reverted-* >/dev/null 2>&1 && [ ! -e /var/lib/xorgate-rs-install/record ]; expect $? 0 "record renamed record.reverted-<ts>"
run "$O" --status; [ "$(head -n1 "$O")" = state=reverted ]; expect $? 0 "--status: state=reverted"
run "$O" --revert; expect $? 0 "revert twice: no-op (stdin /dev/null, no -y)"

echo "== re-convert after keep revert → --revert --purge == M0"
run "$O" -y --tarball "$GOOD"; expect $? 0 "re-convert"; has "$O" "rsPreexisting=no inherited" "inherits the earlier record"
run "$O" --revert --purge; expect $? 0 "revert --purge"
echo "-- transcript (revert --purge):"; sed 's/^/    | /' "$O"
has "$O" "xorgate-agent-rs disabled (files purged). JS install matches the pre-conversion record: yes" "purge final line"
[ "$(snap)" = "$M0" ]; expect $? 0 "purge: whole device == M0"
[ -z "$(diff <(echo "$M0") <(snap))" ] || diff <(echo "$M0") <(snap) | sed 's/^/    /'

echo "== failed start → auto keep-mode revert (exit 4)"
provision pi
run "$O" -y --tarball "$T/dist/xorgate-agent-rs-runfails.tar.gz"; expect $? 4 "run fails"
echo "-- transcript (failed start):"; sed 's/^/    | /' "$O"
[ "$(systemctl is-enabled xorgate-agent)/$(systemctl is-active xorgate-agent)/$(systemctl is-enabled xorgate-agent-rs)" = enabled/active/disabled ]
expect $? 0 "JS enabled + active again, rs disabled"

echo "== interrupted after step 4 → re-run completes; root service user"
provision root; M0=$(snap)
XORGATE_RS_CONVERT_FAIL_AT=4 run "$O" -y --tarball "$GOOD"; expect $? 1 "interrupt-4"
grep -qx state=converting /var/lib/xorgate-rs-install/record; expect $? 0 "record left state=converting"
run "$O" -y --tarball "$GOOD"; expect $? 0 "re-run completes"
[ ! -e /etc/sudoers.d/024_xorgate-update-rs ] && [ ! -e /etc/systemd/system/xorgate-agent-rs.service.d ]; expect $? 0 "root svc: no 024, no 10-user copy"
run "$O" --revert --purge; expect $? 0 "purge"; [ "$(snap)" = "$M0" ]; expect $? 0 "purge after an interrupted first run == M0"

if [ "$NET" = 1 ]; then
  echo "== bucket mode (agent.env DEVICE_DIST_BUCKET_URL, trailing / stripped)"
  provision pi v0.1.2
  (cd "$T/dist" && exec python3 -m http.server 8765 --bind 127.0.0.1 >/dev/null 2>&1) & HTTP=$!
  sleep 1
  run "$O" -y; expect $? 0 "convert from the agent.env bucket (latest)"
  has "$O" "downloading http://127.0.0.1:8765/xorgate-agent-rs-latest.tar.gz" "fetches -latest, not the JS pin"
  run "$O" --revert --purge; expect $? 0 "purge"
  provision pi
  run "$O" -y --tag nope; expect $? 1 "--tag with no artifact: 404"; has "$O" "404); check the version/tag and bucket" "404: download failed, fail-closed"
  kill "$HTTP" 2>/dev/null
fi

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
