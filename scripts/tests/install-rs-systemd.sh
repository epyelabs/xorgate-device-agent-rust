#!/usr/bin/env bash
# shellcheck disable=SC2015  # test assertions: `a && ok || bad`
# O-P6-1 follow-up (orchestrator ruling 2026-11-06): the REAL install-rs.sh,
# converting a JS-provisioned box under a REAL systemd 252 (PID 1 in a
# privileged bookworm container), with the shipped units. With
# After=xorgate-agent.service, step 8's `systemctl start xorgate-agent-rs`
# blocks for the JS agent's whole stop; a JS agent that IGNORES SIGTERM is only
# SIGKILLed after its TimeoutStopSec (systemd default 90 s; the JS unit sets
# none), and its ExecStopPost runs after that.
#   W. wedged JS (SIGTERM ignored, ExecStopPost 15 s: the stop takes ~105 s):
#      the shipped install-rs.sh converts (exit 0, the `converted:` line), with
#      NO overlap (JS's ExecStopPost done before Rust's ExecStartPre), and no
#      auto-revert
#   N. NEGATIVE: the same with install-rs.sh's start budget put back to the old
#      100 s (START_SECS=100) times out in step 8 and auto-reverts (exit 4)
#   P. prompt JS (exits 1 s after SIGTERM): converts fast, no overlap
# Run (systemd must be PID 1; takes ~5 min):
#   docker run -d --name xg-irs --label xorgate.agent=<tag> --privileged --cgroupns=private \
#     --tmpfs /run --tmpfs /run/lock -v "$PWD:/src:ro" debian:bookworm \
#     bash -c 'apt-get update -qq && apt-get install -y -qq systemd python3 sudo >/dev/null && exec /lib/systemd/systemd'
#   docker exec xg-irs bash /src/scripts/tests/install-rs-systemd.sh
#   docker rm -f xg-irs
set -uo pipefail
SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
INSTALL_RS="$SRC/scripts/install-rs.sh"
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
[ "$(cat /proc/1/comm)" = systemd ] || { echo "FAIL: systemd is not PID 1 (see the header)"; exit 2; }
command -v python3 >/dev/null || { echo "FAIL: python3 missing (see the header)"; exit 2; }
for _ in $(seq 60); do s=$(systemctl is-system-running 2>/dev/null); case $s in running | degraded) break ;; esac; sleep 1; done
echo "  $(systemctl --version | head -1); system $s; JS TimeoutStopSec default = $(systemctl show -p DefaultTimeoutStopUSec --value)"
id pi >/dev/null 2>&1 || useradd -m pi

T=/var/tmp/irs-sd
LOG=/var/log/irs-sd.log
D=/etc/systemd/system
rm -rf "$T"; mkdir -p "$T/build/bin" "$T/build/systemd/bench" "$T/dist" /var/lib/irs-sd

# ---- the fake Rust build: the shipped units + install-rs.sh, a python "binary"
# that is a real Type=notify main process (it sends READY=1 from its own pid).
cat > "$T/build/bin/xorgate-rs" <<'PY'
#!/usr/bin/python3
import os, signal, socket, sys, time
def log(m):
    with open("/var/log/irs-sd.log", "a") as f:
        f.write(m + "\n")
def notify(m):
    p = os.environ.get("NOTIFY_SOCKET")
    if p:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        s.sendto(m.encode(), "\0" + p[1:] if p.startswith("@") else p)
        s.close()
a = sys.argv[1:]
if a[:1] in (["-v"], ["--version"]):
    print("xorgate-rs 0.0.0-test (fake)")
elif a[:1] == ["led"]:
    log("rs led " + a[1])
elif a[:1] == ["run"]:
    log("rs start")
    signal.signal(signal.SIGTERM, lambda s, f: (log("rs sigterm"), sys.exit(0)))
    notify("READY=1")
    while True:
        time.sleep(1)
        notify("WATCHDOG=1")
PY
chmod 0755 "$T/build/bin/xorgate-rs"
printf '{\n  "version": "good",\n  "channel": "test",\n  "builtAt": "2026-11-06T00:00:00Z",\n  "packageVersion": "0.0.0"\n}\n' > "$T/build/build-info.json"
cp "$SRC/systemd/xorgate-agent-rs.service" "$SRC/systemd/xorgate-updater-rs.service" "$T/build/systemd/"
cp "$SRC/systemd/bench/90-bench-dev.conf" "$T/build/systemd/bench/"
install -m 0755 "$INSTALL_RS" "$T/build/install-rs.sh"
tar --owner=0 --group=0 -czf "$T/dist/xorgate-agent-rs-good.tar.gz" -C "$T/build" .
(cd "$T/dist" && sha256sum xorgate-agent-rs-good.tar.gz > xorgate-agent-rs-good.tar.gz.sha256)
GOOD="$T/dist/xorgate-agent-rs-good.tar.gz"
grep -qx 'After=xorgate-agent.service' "$T/build/systemd/xorgate-agent-rs.service" && ok "the build carries the shipped unit with After=" || bad "shipped unit lacks After="

# ---- the JS stand-ins: the JS unit's shape; how it stops is per case ----
cat > /usr/local/bin/irs-js-agent <<'EOF'
#!/bin/bash
log() { echo "js $*" >> /var/log/irs-sd.log; }
log start
if [ "$(cat /var/lib/irs-sd/js-mode)" = wedged ]; then
  trap 'log "sigterm (ignored)"' TERM
else
  trap 'log sigterm; sleep 1; log exit; exit 0' TERM
fi
while :; do sleep 1 & wait $!; done
EOF
# The JS CLI wrapper's path (/usr/local/bin/xorgate), as the JS unit's hooks call it.
cat > "$T/js-cli" <<'EOF'
#!/bin/bash
log() { echo "js $*" >> /var/log/irs-sd.log; }
if [ "${1:-}" = led ]; then
  log "led $2 begin"
  [ "$2" = exit-state ] && sleep "$(cat /var/lib/irs-sd/poststop-secs)"
  log "led $2 end"
fi
exit 0
EOF
chmod 0755 /usr/local/bin/irs-js-agent "$T/js-cli"

wipe() {
  systemctl stop xorgate-agent-rs xorgate-agent xorgate-updater-rs 2>/dev/null
  systemctl disable -q xorgate-agent-rs xorgate-agent 2>/dev/null
  rm -rf /etc/xorgate /opt/xorgate /opt/xorgate-rs /opt/xorgate-rs.new /opt/xorgate-rs.old /var/lib/xorgate-rs-install \
    /usr/local/bin/xorgate /usr/local/bin/xorgate-rs $D/xorgate-* /etc/sudoers.d/02[0-4]_xorgate-* \
    $D/multi-user.target.wants/xorgate-* /run/xorgate-rs-install.lock /var/lib/xorgate /var/lib/xorgate-rs
  systemctl daemon-reload; systemctl reset-failed 2>/dev/null
}
provision() { # <js-mode: wedged|prompt> <ExecStopPost secs>: a JS-provisioned device, JS enabled + active
  wipe
  echo "$1" > /var/lib/irs-sd/js-mode; echo "$2" > /var/lib/irs-sd/poststop-secs
  install -d -m 0700 -o pi -g pi /etc/xorgate
  for f in device.cert.pem device.key.pem root-ca.pem; do echo "dummy $f" | install -m 0600 -o pi -g pi /dev/stdin "/etc/xorgate/$f"; done
  printf 'DEVICE_ID=dev-1\nIOT_DATA_ENDPOINT=x.iot\nMODE=real\nDEVICE_DIST_VERSION=latest\n' | install -m 0600 -o pi -g pi /dev/stdin /etc/xorgate/agent.env
  mkdir -p /opt/xorgate/dist /etc/sudoers.d $D/multi-user.target.wants $D/xorgate-agent.service.d
  echo js > /opt/xorgate/dist/cli.js
  install -m 0755 "$T/js-cli" /usr/local/bin/xorgate
  cat > $D/xorgate-agent.service <<'EOF'
[Unit]
Description=Xorgate device agent (IoT MQTT telemetry) stand-in
After=network-online.target
Wants=network-online.target
[Service]
Type=simple
User=root
EnvironmentFile=/etc/xorgate/agent.env
StateDirectory=xorgate
StateDirectoryMode=0700
Environment=XORGATE_HOME=/var/lib/xorgate
WorkingDirectory=/var/lib/xorgate
ExecStartPre=-/usr/local/bin/xorgate led boot
ExecStart=/usr/local/bin/irs-js-agent
ExecStopPost=-/usr/local/bin/xorgate led exit-state
Restart=always
RestartSec=5s
[Install]
WantedBy=multi-user.target
EOF
  printf '[Service]\nUser=pi\nGroup=pi\n' > $D/xorgate-agent.service.d/10-user.conf
  install -m 0666 /dev/null "$LOG"
  systemctl daemon-reload; systemctl enable -q xorgate-agent; systemctl start xorgate-agent
  for _ in $(seq 50); do [ "$(systemctl is-active xorgate-agent)" = active ] && break; sleep 0.2; done
  sleep 1; : > "$LOG"
}
idx() { local n; n=$(grep -nxF -- "$1" "$LOG" | head -1 | cut -d: -f1); echo "${n:-0}"; }
before() { local a b; a=$(idx "$1"); b=$(idx "$2"); [ "$a" -gt 0 ] && [ "$b" -gt 0 ] && [ "$a" -lt "$b" ]; }
convert() { # <installer> -> sets RC, SECS; output in $T/out
  local t0 t1
  t0=$(date +%s%N)
  bash "$1" -y --tarball "$GOOD" > "$T/out" 2>&1 < /dev/null; RC=$?
  t1=$(date +%s%N); SECS=$(( (t1 - t0) / 1000000000 ))
  echo "    install-rs.sh exit $RC after ${SECS} s; final line: $(tail -n1 "$T/out")"
  sed 's/^/    | /' "$LOG"
}
FINAL='[install-rs] converted: xorgate-agent-rs good (test) active; xorgate-agent stopped + disabled. revert with: sudo /opt/xorgate-rs/install-rs.sh --revert'
states() { echo "$(systemctl is-active xorgate-agent)/$(systemctl is-active xorgate-agent-rs)"; }

# ---- W: wedged JS, the shipped installer ----
echo "== W. wedged JS (SIGTERM ignored, ExecStopPost 15 s), shipped install-rs.sh (START_SECS=$(sed -n 's/^START_SECS=//p' "$INSTALL_RS"))"
provision wedged 15
convert "$INSTALL_RS"
[ "$RC" = 0 ] && [ "$(tail -n1 "$T/out")" = "$FINAL" ] && ok "wedged JS: converted (exit 0, the converted: line)" || { bad "wedged JS: exit $RC"; sed 's/^/    > /' "$T/out"; }
grep -q 'auto\|revert' <(grep -v 'revert with:' "$T/out") && bad "wedged JS: the transcript mentions a revert" || ok "wedged JS: no auto-revert"
case $(states) in inactive/active | failed/active) ok "wedged JS: JS $(states | cut -d/ -f1), Rust active" ;; *) bad "wedged JS: states $(states)" ;; esac
[ "$SECS" -gt 100 ] && ok "wedged JS: step 8 really blocked past the old 100 s budget (${SECS} s total)" || bad "wedged JS: only ${SECS} s, the case does not discriminate"
before "js led exit-state end" "rs led boot" && before "rs led boot" "rs start" && ok "wedged JS: no overlap (JS's ExecStopPost done before Rust's ExecStartPre)" || bad "wedged JS: overlap"
journalctl -u xorgate-agent --no-pager | grep -q "State 'stop-sigterm' timed out. Killing." && ok "wedged JS: systemd SIGKILLed it after TimeoutStopSec" || bad "wedged JS: no stop-sigterm timeout in the journal"

# ---- N: negative, the old 100 s budget ----
sed 's/^START_SECS=150$/START_SECS=100/' "$INSTALL_RS" > "$T/install-rs-100.sh"
grep -qx 'START_SECS=100' "$T/install-rs-100.sh" || bad "negative: could not put START_SECS back to 100"
echo "== N. NEGATIVE: the same with the old 100 s start budget"
provision wedged 15
convert "$T/install-rs-100.sh"
[ "$RC" = 4 ] && ok "old 100 s budget: step 8 timed out and auto-reverted (exit 4)" || { bad "old 100 s budget: exit $RC (want 4)"; sed 's/^/    > /' "$T/out"; }
grep -qF 'systemctl start xorgate-agent-rs failed' "$T/out" && ok "old 100 s budget: the transcript names the timed-out start" || bad "old 100 s budget: no start failure in the transcript"
[ "$(tail -n1 "$T/out")" != "$FINAL" ] && ok "old 100 s budget: no converted: line" || bad "old 100 s budget: printed the converted: line"

# ---- P: prompt JS, the shipped installer ----
echo "== P. prompt JS (exits 1 s after SIGTERM, no ExecStopPost delay), shipped install-rs.sh"
provision prompt 0
convert "$INSTALL_RS"
[ "$RC" = 0 ] && [ "$(tail -n1 "$T/out")" = "$FINAL" ] && ok "prompt JS: converted (exit 0)" || { bad "prompt JS: exit $RC"; sed 's/^/    > /' "$T/out"; }
[ "$(states)" = inactive/active ] && ok "prompt JS: JS inactive, Rust active" || bad "prompt JS: states $(states)"
before "js led exit-state end" "rs led boot" && ok "prompt JS: no overlap" || bad "prompt JS: overlap"
[ "$SECS" -lt 30 ] && ok "prompt JS: not delayed (${SECS} s)" || bad "prompt JS: ${SECS} s"

wipe
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
