#!/bin/sh
# Phase 5b WP3 — the stdlib FakeModem (sms_fake_modem.py) against BOTH
# readers, in the rig image (node + python3 + stty), before H15 uses it on the
# bench: the Rust CLI `xorgate-rs sms clear --yes` then `sms --json`, and the
# REAL TS @ 09da38d (`sms-pty-clear.mts`: clear, then read), each on a fresh
# modem; the two command logs must be identical. Then the rig rule: without
# --allow-clear the fake answers the delete ERROR, nothing is deleted, and it
# exits 3 (violation). And the CLI refuses without a TTY / --yes.
#
#   docker run --rm --label xorgate.agent=<tag> -v $PWD:/src -v xorgate-rs-target:/target:ro \
#     -v $RIG_JS_CACHE/js-09da38d:/js xorgate-replay-rig:node20-bookworm \
#     sh /src/tools/sms-fake-modem/e2e-rig.sh /target/<dir>/debug/xorgate-rs
set -eu
BIN=${1:?usage: e2e-rig.sh <xorgate-rs binary>}
HERE=$(cd "$(dirname "$0")" && pwd)
INBOX=$HERE/../../fixtures/replay/p4-sms-modem.json
W=$(mktemp -d)
export XORGATE_HOME=$W/home XORGATE_SMS_PORT=$W/pty
mkdir -p "$XORGATE_HOME"

start() { # $1 = log, $2.. = extra flags
  log=$1; shift
  python3 -I "$HERE/sms_fake_modem.py" --inbox "$INBOX" --link "$XORGATE_SMS_PORT" --log "$log" "$@" > "$log.out" 2>&1 &
  PID=$!
  while [ ! -L "$XORGATE_SMS_PORT" ]; do sleep 0.1; done
}
stop() { kill -TERM "$PID"; set +e; wait "$PID"; RC=$?; set -e; cat "$1.out"; }

echo "== rust: sms clear --yes, then sms --json"
start "$W/rust.log" --allow-clear
"$BIN" sms clear --yes | tee "$W/rust.out"
"$BIN" sms --json --limit 1 | head -12
stop "$W/rust.log"; [ "$RC" = 0 ]

echo "== ts @09da38d: SmsReader.clear() then read()"
start "$W/ts.log" --allow-clear
(cd /js && npx --no-install tsx .rig-drivers/sms-pty-clear.mts 1 1760000000000) | grep -E '"(action|status|cleared|SM|ME)"' | head -12
stop "$W/ts.log"; [ "$RC" = 0 ]

echo "== command logs (Rust clear + read == TS clear + read)"
if cmp "$W/rust.log" "$W/ts.log"; then echo "IDENTICAL ($(wc -l < "$W/rust.log") commands)"; else diff "$W/rust.log" "$W/ts.log"; exit 1; fi

echo "== rig rule: no --allow-clear → the delete is refused, nothing deleted, exit 3"
start "$W/deny.log"
set +e; "$BIN" sms clear --yes; CLI=$?; set -e
stop "$W/deny.log"; echo "cli exit $CLI, fake exit $RC"; [ "$CLI" = 1 ] && [ "$RC" = 3 ]

echo "== no TTY, no --yes: refused before any port access"
start "$W/refuse.log" --allow-clear
set +e; "$BIN" sms clear < /dev/null; CLI=$?; set -e
stop "$W/refuse.log"; echo "cli exit $CLI, commands seen: $(wc -l < "$W/refuse.log" 2>/dev/null || echo 0)"
[ "$CLI" = 1 ] && [ ! -s "$W/refuse.log" ]
echo "E2E OK"
