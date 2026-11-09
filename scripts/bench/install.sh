#!/usr/bin/env bash
# Install (or update) the Rust agent on the bench CM5 next to the running JS
# prod agent. Idempotent; run from the Mac.
#
#   scripts/bench/install.sh <tarball>              # a pack.sh / CI tarball on the Mac
#   scripts/bench/install.sh --from-bench-build     # RETIRED: refuses with exit 2. Never build on
#                                                   # the bench: build in Docker (scripts/docker-build.sh
#                                                   # pack) and pass the tarball (guide/01-setup.md)
#   BENCH=pi@10.0.0.111 scripts/bench/install.sh …  # other address
#
# What it does on the bench (all as root, all re-runnable):
#   1. extract the tarball to /opt/xorgate-rs.new, verify bin/xorgate-rs +
#      build-info.json + systemd/xorgate-agent-rs.service, swap it to
#      /opt/xorgate-rs (previous install kept at /opt/xorgate-rs.prev)
#   2. /usr/local/bin/xorgate-rs wrapper (XORGATE_HOME defaults to
#      /var/lib/xorgate-rs on the bench; the SHIPPED default is /var/lib/xorgate)
#   3. /etc/systemd/system/xorgate-agent-rs.service — the shipped unit from
#      the tarball with its ONE `Conflicts=xorgate-agent.service` line
#      commented out (systemd cannot reset a dependency list from a drop-in;
#      an empty `Conflicts=` is ignored on 252) — plus the bench-only drop-in
#      xorgate-agent-rs.service.d/90-bench-dev.conf (plan D0.1)
#   4. /etc/xorgate/agent-rs-dev.env, created ONCE from the parked dev bundle
#      /etc/xorgate/dev-backup-20260823/agent.env with CERT_PATH/KEY_PATH
#      rewritten into the backup dir (the backup's own values point at the
#      PROD cert in /etc/xorgate), CA_PATH=/etc/xorgate/root-ca.pem unless the
#      backup carries its own, and XORGATE_HOME=/var/lib/xorgate-rs. An
#      existing env file is never rewritten (edit it by hand for A6 faults).
#   3b. Phase 4: /etc/systemd/system/xorgate-updater-rs.service from the
#      tarball (0644, no [Install], never started here) + the exact-args grant
#      /etc/sudoers.d/024_xorgate-update-rs for pi (visudo -cf before it
#      lands, 0440; a refusal warns) — the bootstrap of the remote update rail
#   5. /var/lib/xorgate-rs{,/evidence} owned by pi; systemctl daemon-reload
# Precondition (step 0): while the unit is enabled/active the shared input
# profile must be complete — Phase 4 adds the LED/SMS knobs + the pinctrl and
# sudo shims, so on a bench still on a Phase 3 profile run
# `scripts/bench/profile.sh shared` FIRST, then this script.
# It never starts the service (scripts/bench/start.sh does) and NEVER touches
# xorgate-agent.service, /etc/xorgate/agent.env, /var/lib/xorgate or
# sim7600-lte.service.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

usage() { sed -n '2,30p' "$0"; exit 2; }

# On-bench builds are retired: a cargo build on the bench browned out its
# power supply (the Pi and the modem share one). The option still parses so an
# old command line gets a clear refusal instead of "unknown option".
refuse_bench_build() {
  echo "install.sh: --from-bench-build is retired: never build on the bench (a cargo build there browned out its shared power supply)." >&2
  echo "  build in Docker instead:  scripts/docker-build.sh pack" >&2
  echo "  then install the tarball: scripts/bench/install.sh target/dist/xorgate-agent-rs-<version>.tar.gz" >&2
  exit 2
}

TARBALL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --from-bench-build|--bench-src) refuse_bench_build ;;
    -h|--help) usage ;;
    -*) echo "unknown option: $1" >&2; usage ;;
    *) TARBALL="$1"; shift ;;
  esac
done
if [ -z "$TARBALL" ]; then usage; fi

[ -f "$TARBALL" ] || { echo "no such tarball: $TARBALL" >&2; exit 1; }
# List once: `tar -tzf | grep -q` under pipefail fails spuriously (SIGPIPE).
LISTING=$(tar -tzf "$TARBALL")
for required in bin/xorgate-rs build-info.json systemd/xorgate-agent-rs.service systemd/bench/90-bench-dev.conf; do
  grep -qx "$required" <<<"$LISTING" || { echo "$TARBALL lacks $required" >&2; exit 1; }
done
REMOTE_TARBALL="/home/pi/xorgate-rs-upload/$(basename "$TARBALL")"
log "uploading $(basename "$TARBALL") ($(wc -c < "$TARBALL" | tr -d ' ') bytes) to $BENCH:$REMOTE_TARBALL"
bench_ssh 'mkdir -p /home/pi/xorgate-rs-upload'
bench_scp_to "$TARBALL" "$REMOTE_TARBALL"

log "installing on $BENCH from $REMOTE_TARBALL"
# Everything below runs as root ON THE BENCH. Quoted heredoc: nothing expands
# locally; the only input is $1 (the tarball path).
{ bench_remote_lib; cat <<'REMOTE'
TARBALL="$1"
# Phase 2 bench-safety precondition (orchestrator, review-wp2): a build with
# the WP2 collectors must never run against the JS agent's inputs. While the
# unit is enabled or active the swap below would put the new build into it
# (Restart=always), so refuse unless the SHARED profile is fully in force —
# run scripts/bench/dev-manifests.sh and scripts/bench/profile.sh shared first.
echo "== 0. input-profile precondition"
if systemctl is-enabled --quiet xorgate-agent-rs 2>/dev/null || systemctl is-active --quiet xorgate-agent-rs 2>/dev/null; then
  if ! inputs_safe; then
    echo "REFUSING to install: xorgate-agent-rs is enabled/active and the shared input profile is incomplete (above)."
    echo "  run: scripts/bench/dev-manifests.sh && scripts/bench/profile.sh shared   then retry"
    exit 1
  fi
  echo "  ok: shared input profile in force"
else
  echo "  unit not enabled/active: installing; start.sh refuses until the shared profile is in force"
fi
RS_ROOT=/opt/xorgate-rs
RS_HOME=/var/lib/xorgate-rs
RS_ENV=/etc/xorgate/agent-rs-dev.env
DEV_BACKUP=/etc/xorgate/dev-backup-20260823
UNIT=xorgate-agent-rs.service
SVC_USER=pi
CHANGES=()
note() { CHANGES+=("$1"); echo "  - $1"; }

[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
[ -f "$TARBALL" ] || { echo "tarball not found on bench: $TARBALL"; exit 1; }
# Hard guard: this script must never be pointed at the JS agent's tree.
case "$RS_ROOT" in /opt/xorgate-rs) ;; *) echo "refusing RS_ROOT=$RS_ROOT"; exit 1 ;; esac

echo "== 1. stage + swap $RS_ROOT"
LISTING=$(tar -tzf "$TARBALL")  # list once (pipefail + grep -q = spurious SIGPIPE failure)
for required in bin/xorgate-rs build-info.json systemd/xorgate-agent-rs.service; do
  grep -qx "$required" <<<"$LISTING" || { echo "tarball lacks $required"; exit 1; }
done
rm -rf "$RS_ROOT.new"
mkdir -p "$RS_ROOT.new"
tar -xzf "$TARBALL" -C "$RS_ROOT.new" --no-same-owner
chmod 0755 "$RS_ROOT.new/bin/xorgate-rs"
chmod -R a+rX "$RS_ROOT.new"
NEW_INFO=$(tr -d '\n' < "$RS_ROOT.new/build-info.json" | tr -s ' ')
if [ -d "$RS_ROOT" ]; then
  OLD_INFO=$(tr -d '\n' < "$RS_ROOT/build-info.json" 2>/dev/null | tr -s ' ' || echo "?")
  rm -rf "$RS_ROOT.prev"
  mv "$RS_ROOT" "$RS_ROOT.prev"
  mv "$RS_ROOT.new" "$RS_ROOT"
  note "$RS_ROOT replaced (prev kept at $RS_ROOT.prev): $OLD_INFO -> $NEW_INFO"
else
  mv "$RS_ROOT.new" "$RS_ROOT"
  note "$RS_ROOT installed: $NEW_INFO"
fi

echo "== 2. wrapper /usr/local/bin/xorgate-rs"
WRAPPER=$(mktemp)
cat > "$WRAPPER" <<'EOF'
#!/usr/bin/env bash
# xorgate-rs CLI wrapper (BENCH flavour, written by scripts/bench/install.sh).
# The shipped wrapper (Phase 5 install-rs.sh) defaults XORGATE_HOME to
# /var/lib/xorgate like the JS one; on the bench the Rust agent runs under the
# bench-only drop-in with StateDirectory=xorgate-rs, so the CLI must look there
# to reflect THIS service (plan D0.1).
exec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate-rs}" /opt/xorgate-rs/bin/xorgate-rs "$@"
EOF
if [ -f /usr/local/bin/xorgate-rs ] && cmp -s "$WRAPPER" /usr/local/bin/xorgate-rs; then
  echo "  - wrapper unchanged"
else
  install -m 0755 "$WRAPPER" /usr/local/bin/xorgate-rs
  note "/usr/local/bin/xorgate-rs written"
fi
rm -f "$WRAPPER"

echo "== 3. unit + bench drop-in"
# BENCH DEVIATION from "shipped unit, verbatim": systemd (252 on Bookworm)
# cannot reset a dependency list such as Conflicts= from a drop-in — an empty
# "Conflicts=" is silently ignored (verified on the bench 2026-10-08), so the
# shipped unit's Conflicts=xorgate-agent.service would stop the JS prod agent
# the moment this unit starts. The bench copy therefore has that ONE line
# commented out; everything else is the shipped unit byte for byte.
UNIT_TMP=$(mktemp)
sed -e 's|^Conflicts=xorgate-agent.service$|# BENCH (scripts/bench/install.sh): Conflicts=xorgate-agent.service stripped — both agents coexist on the bench|' \
  "$RS_ROOT/systemd/xorgate-agent-rs.service" > "$UNIT_TMP"
grep -q '^Conflicts=' "$UNIT_TMP" && { echo "unit still carries a Conflicts= line after the bench strip — refusing"; rm -f "$UNIT_TMP"; exit 1; }
if [ -f "/etc/systemd/system/$UNIT" ] && cmp -s "$UNIT_TMP" "/etc/systemd/system/$UNIT"; then
  echo "  - $UNIT unchanged"
else
  install -m 0644 "$UNIT_TMP" "/etc/systemd/system/$UNIT"
  note "/etc/systemd/system/$UNIT installed (shipped unit with Conflicts= line stripped, bench-only)"
fi
rm -f "$UNIT_TMP"
DROPIN_SRC="$RS_ROOT/systemd/bench/90-bench-dev.conf"
[ -f "$DROPIN_SRC" ] || { echo "tarball lacks systemd/bench/90-bench-dev.conf (bench drop-in)"; exit 1; }
install -d -m 0755 "/etc/systemd/system/$UNIT.d"
if [ -f "/etc/systemd/system/$UNIT.d/90-bench-dev.conf" ] && cmp -s "$DROPIN_SRC" "/etc/systemd/system/$UNIT.d/90-bench-dev.conf"; then
  echo "  - 90-bench-dev.conf unchanged"
else
  install -m 0644 "$DROPIN_SRC" "/etc/systemd/system/$UNIT.d/90-bench-dev.conf"
  note "/etc/systemd/system/$UNIT.d/90-bench-dev.conf installed (bench-only)"
fi

echo "== 3b. remote update rail bootstrap (Phase 4: $UPDATER_UNIT + $UPDATE_SUDOERS)"
# Chicken-and-egg (phase-4-operations.md §5 "Bootstrap"): the first updater
# unit + exact-args grant come from here; every later `xorgate-rs update`
# reinstalls them from its own tarball. Never enabled, never started here.
p4_updater_bootstrap "$RS_ROOT" "$SVC_USER"
echo "== 4. dev identity env file $RS_ENV"
if [ -f "$RS_ENV" ]; then
  echo "  - $RS_ENV exists — kept as is"
else
  [ -f "$DEV_BACKUP/agent.env" ] || { echo "no parked dev bundle at $DEV_BACKUP/agent.env"; exit 1; }
  [ -f "$DEV_BACKUP/device.cert.pem" ] && [ -f "$DEV_BACKUP/device.key.pem" ] || { echo "$DEV_BACKUP lacks device.cert.pem/device.key.pem"; exit 1; }
  CA=/etc/xorgate/root-ca.pem
  [ -f "$DEV_BACKUP/root-ca.pem" ] && CA="$DEV_BACKUP/root-ca.pem"
  [ -f "$CA" ] || { echo "no CA at $CA"; exit 1; }
  TMP=$(mktemp)
  {
    echo "# Bench-only DEV identity for xorgate-agent-rs (Phase 0-5). Derived by"
    echo "# scripts/bench/install.sh from $DEV_BACKUP/agent.env with the cert/key"
    echo "# paths rewritten into the backup dir (the backup's own values point at"
    echo "# the PROD cert in /etc/xorgate). Edit by hand; install.sh never rewrites it."
    sed -e "s|^CERT_PATH=.*|CERT_PATH=$DEV_BACKUP/device.cert.pem|" \
        -e "s|^KEY_PATH=.*|KEY_PATH=$DEV_BACKUP/device.key.pem|" \
        -e "s|^CA_PATH=.*|CA_PATH=$CA|" \
        -e "/^XORGATE_HOME=/d" \
        "$DEV_BACKUP/agent.env"
    echo "XORGATE_HOME=$RS_HOME"
  } > "$TMP"
  grep -q '^CA_PATH=' "$TMP" || echo "CA_PATH=$CA" >> "$TMP"
  DEV_ID=$(sed -n 's/^DEVICE_ID=//p' "$TMP" | head -n1)
  PROD_ID=$(sed -n 's/^DEVICE_ID=//p' /etc/xorgate/agent.env 2>/dev/null | head -n1 || true)
  [ -n "$DEV_ID" ] || { echo "derived env has no DEVICE_ID"; rm -f "$TMP"; exit 1; }
  if [ -n "$PROD_ID" ] && [ "$DEV_ID" = "$PROD_ID" ]; then
    echo "derived DEVICE_ID equals the PROD identity in /etc/xorgate/agent.env — refusing"; rm -f "$TMP"; exit 1
  fi
  install -o "$SVC_USER" -g "$SVC_USER" -m 0600 "$TMP" "$RS_ENV"
  rm -f "$TMP"
  note "$RS_ENV created (DEVICE_ID=${DEV_ID:0:8}…, CERT/KEY in $DEV_BACKUP, CA=$CA)"
fi
# Every run: the cert/key/CA the env points at must be readable by the service user.
for key in CERT_PATH KEY_PATH CA_PATH; do
  P=$(sed -n "s/^$key=//p" "$RS_ENV" | head -n1)
  [ -n "$P" ] || { echo "$RS_ENV lacks $key"; exit 1; }
  sudo -u "$SVC_USER" test -r "$P" || { echo "$key=$P not readable by $SVC_USER"; exit 1; }
done
DEV_ID=$(sed -n 's/^DEVICE_ID=//p' "$RS_ENV" | head -n1)
PROD_ID=$(sed -n 's/^DEVICE_ID=//p' /etc/xorgate/agent.env 2>/dev/null | head -n1 || true)
echo "  - identities: rs=${DEV_ID:0:8}… (dev)  js=${PROD_ID:0:8}… (prod)"

echo "== 5. state dir + daemon-reload"
if [ ! -d "$RS_HOME" ]; then
  install -d -o "$SVC_USER" -g "$SVC_USER" -m 0700 "$RS_HOME"
  note "$RS_HOME created"
fi
if [ ! -d "$RS_HOME/evidence" ]; then
  install -d -o "$SVC_USER" -g "$SVC_USER" -m 0755 "$RS_HOME/evidence"
  note "$RS_HOME/evidence created"
fi
systemctl daemon-reload
note "systemctl daemon-reload"

echo "== effective unit settings (drop-in applied?)"
systemctl show "$UNIT" -p Conflicts -p User -p Group -p EnvironmentFiles -p StateDirectory -p WorkingDirectory -p Type -p WatchdogUSec -p NotifyAccess | sed 's/^/  /'
systemd-analyze verify "/etc/systemd/system/$UNIT" 2>&1 | sed 's/^/  verify: /' || true
echo "== build"
"$RS_ROOT/bin/xorgate-rs" --version 2>&1 | sed 's/^/  /' || echo "  (binary did not answer --version; fine for a stub)"
echo "== units"
printf '  %-18s %s\n' xorgate-agent "$(systemctl is-active xorgate-agent || true)" \
                      "$UNIT" "$(systemctl is-active "$UNIT" || true)" \
                      sim7600-lte "$(systemctl is-active sim7600-lte || true)"
echo "== changed (${#CHANGES[@]})"
if [ "${#CHANGES[@]}" -eq 0 ]; then echo "  nothing"; else printf '  %s\n' "${CHANGES[@]}"; fi
echo "next: scripts/bench/start.sh"
REMOTE
} | bench_sudo_script "$REMOTE_TARBALL"
