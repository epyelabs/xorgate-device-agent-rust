#!/bin/sh
# C11 per-case `set` runner (WP6 evidence/wp6-tools/e2e-set-run.sh, extended by
# WP7 with the @root / @running tags and #expect comments of cases.txt).
#
#   RUNNER="node /path/dist/cli.js" e2e-set-run.sh <outdir>
#   RUNNER=/path/xorgate-rs         e2e-set-run.sh <outdir>
#
# Per case: fresh home seeded from seed.json, NO_COLOR, an absent LTE file;
# writes <outdir>/<n>/{case,out,err,code,settings}. Runs anywhere (Mac scratch,
# rig, bench) — @root cases need root (else recorded as `skipped`), and the
# unprivileged user is $RUN_AS (default `node`, the rig user) when root.
set -u
E=$(cd "$(dirname "$0")" && pwd); OUT=$1
: "${RUNNER:?set RUNNER to the CLI command}"
RUN_AS=${RUN_AS:-node}
rm -rf "$OUT"; mkdir -p "$OUT"; i=0
as_user() { # run "$@" as $RUN_AS when we are root, else as ourselves
  if [ "$(id -u)" = 0 ]; then setpriv --reuid="$(id -u "$RUN_AS")" --regid="$(id -g "$RUN_AS")" --init-groups "$@"; else "$@"; fi
}
grep -v '^[[:space:]]*#' "$E/cases.txt" | grep -v '^[[:space:]]*$' | while IFS= read -r raw; do
  i=$((i+1)); d="$OUT/$i"; mkdir -p "$d/home"; cp "$E/seed.json" "$d/home/settings.json"
  line=${raw%%#expect:*}; expect=-; case "$raw" in *'#expect:'*) expect=${raw##*#expect:} ;; esac
  who=user; decoy=
  case "$line" in @root\ *) who=root; line=${line#@root } ;; @running\ *) who=running; line=${line#@running } ;; esac
  line=$(printf '%s' "$line" | sed 's/[[:space:]]*$//')
  printf '%s|%s|%s\n' "$expect" "$who" "$line" > "$d/case"
  [ "$(id -u)" = 0 ] && chown -R "$RUN_AS" "$d" 2>/dev/null
  if [ "$who" = root ] && [ "$(id -u)" != 0 ]; then echo skipped > "$d/code"; : > "$d/out"; : > "$d/err"; cp "$d/home/settings.json" "$d/settings"; continue; fi
  if [ "$who" = running ]; then
    # A live process whose command line names a worker (looksLikeWorker,
    # process.ts:131-136) + a pidfile describing it: inspect() → running.
    # (No shell function here: `$!` must be the pid that ends up exec'ing sleep.)
    fake='exec -a "node /opt/fake-agent/dist/worker.js" sleep 600'
    if [ "$(id -u)" = 0 ]; then
      setpriv --reuid="$(id -u "$RUN_AS")" --regid="$(id -g "$RUN_AS")" --init-groups bash -c "$fake" >/dev/null 2>&1 &
    else
      bash -c "$fake" >/dev/null 2>&1 &
    fi
    decoy=$!
    printf '{"pid":%s,"startedAt":%s000,"deviceId":"00000000-0000-4000-8000-0000000000c1","mode":"real","rateHz":1,"offline":false,"worker":"/opt/fake-agent/dist/worker.js","argv":[],"nodeVersion":"v20.20.2"}\n' \
      "$decoy" "$(date +%s)" > "$d/home/agent.pid"
    [ "$(id -u)" = 0 ] && chown "$RUN_AS" "$d/home/agent.pid"
    sleep 0.3
  fi
  eval "set -- $line"
  if [ "$who" = root ]; then
    env XORGATE_HOME="$d/home" LTE_TELEMETRY_FILE="$d/none.json" NO_COLOR=1 TZ=UTC $RUNNER set "$@" >"$d/out" 2>"$d/err" </dev/null
  else
    as_user env XORGATE_HOME="$d/home" LTE_TELEMETRY_FILE="$d/none.json" NO_COLOR=1 TZ=UTC $RUNNER set "$@" >"$d/out" 2>"$d/err" </dev/null
  fi
  echo $? > "$d/code"
  cp "$d/home/settings.json" "$d/settings" 2>/dev/null || echo "<absent>" > "$d/settings"
  if [ -n "$decoy" ]; then
    kill "$decoy" 2>/dev/null
    # A JS restart that succeeded spawned a real worker: stop it via its pidfile.
    p=$(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$d/home/agent.pid" 2>/dev/null)
    [ -n "$p" ] && [ "$p" != "$decoy" ] && kill "$p" 2>/dev/null
    rm -f "$d/home/agent.pid"
  fi
done
