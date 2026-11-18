#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016,SC2031,SC2329  # assertions `a && pass || failc`; literal $VARs in patterns; $! after the stub subshells; stubs called by the sourced scripts
# v0.1.1 F-LTE-METRIC / F-DA21-NET / F-DA21-NM: the LTE default route that
# modem/sim7600-lte.sh installs, and the stale-usb0 cleanup that
# scripts/provision-rs.sh runs at P1b.
#   1. static: the daemon writes a default route only in
#      install_lte_default_route, never with replace/change; the default metric
#      is 1000; modem/SHA256SUMS matches.
#   2. kernel: the REAL `ip` against the REAL kernel FIB, inside a PRIVATE
#      network namespace (`unshare -n`: the host's and the container's own
#      network are never touched; the rows refuse to run unless the namespace
#      holds nothing but lo):
#      K: the shipped daemon's bring_up_interface() (the dhclient bind, AT,
#         ping, pkill and sleep stubbed; every `ip` call real);
#      N: provision-rs.sh's clear_stale_lte_defaults() (the owners stubbed:
#         systemctl, nmcli, plus a fake `dhclient` process).
#      They need root, CAP_NET_ADMIN + CAP_SYS_ADMIN and iproute2. Without
#      them the rows are SKIPPED and the result line says so; with --kernel a
#      skip is a FAIL.
# Regression proof: LTE_DAEMON=<v0.1.0 sim7600-lte.sh> fails S1/S2 and the
# K rows (the eth1 default is overwritten); LTE_PROV=<v0.1.0 provision-rs.sh>
# fails the N rows (no clear_stale_lte_defaults).
# Runs:
#   - as a plain suite (hl.sh `suites`): static rows; the kernel rows SKIP;
#   - inside provision-rs-systemd.sh's privileged container, with --kernel;
#   - by hand:
#     docker run --rm --cap-add NET_ADMIN --cap-add SYS_ADMIN --label xorgate.agent=<tag> -v "$PWD:/src:ro" -w /src \
#       debian:bookworm bash -c 'apt-get update -qq && apt-get install -y -qq iproute2 >/dev/null && bash scripts/tests/lte-routes.sh --kernel'
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
SELF=$REPO/scripts/tests/lte-routes.sh
DAEMON=${LTE_DAEMON:-$REPO/modem/sim7600-lte.sh}
PROV=${LTE_PROV:-$REPO/scripts/provision-rs.sh}
fail=0
pass() { echo "PASS $*"; }
failc() { echo "FAIL $*"; fail=1; }

# ---------------------------------------------------------------- kernel rows
# (runs in the private namespace: `unshare -n env LTE_ROUTES_NETNS=1 bash $SELF`)
kernel_rows() {
  local links
  # Fresh = no routes, and no link but lo and the kernel's per-namespace
  # fallback tunnel devices (tunl0@NONE, sit0@NONE, …).
  links=$(ip -o link show | awk -F': ' '$2 != "lo" && $2 !~ /@NONE$/ {print $2}' | tr '\n' ' ')   # hl-clock-mocked: awk's $2, not a command
  [ -z "$links" ] && [ -z "$(ip -4 route show; ip -6 route show)" ] || { failc "kernel rows: not a fresh network namespace (links: $links); refusing"; return; }
  ip link set lo up
  T=$(mktemp -d)

  mk() { # <name> <ipv4/cidr> [<ipv6/64>]: a veth pair <name>/<name>p, both up
    ip link add "$1" type veth peer name "${1}p" && ip link set "${1}p" up && ip link set "$1" up \
      && ip addr add "$2" dev "$1" && { [ -z "${3:-}" ] || ip -6 addr add "$3" dev "$1" nodad; }
  }
  reset() { local l; for l in usb0 eth1 wlan0; do ip link del "$l" 2>/dev/null; done; : > "$T/daemon.log"; }
  defs() { ip -4 route show default | sed 's/ *$//' | sort; }   # the whole v4 default table
  dev_of() { ip -4 route get "$@" 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}'; }

  # One bring_up_interface() of the daemon under test, in a subshell (its
  # globals, the stubs and SIM7600_ROUTE_METRIC never leak). The dhclient bind
  # is what dhclient-script does on BOUND: the lease address plus a metric-0
  # default via the modem's NAT.
  daemon_up() { # [SIM7600_ROUTE_METRIC value]
    (
      if [ $# -gt 0 ]; then export SIM7600_ROUTE_METRIC="$1"; else unset SIM7600_ROUTE_METRIC; fi
      # shellcheck source=modem/sim7600-lte.sh
      source "$DAEMON"
      log() { echo "INFO $*" >> "$T/daemon.log"; }
      warn() { echo "WARN $*" >> "$T/daemon.log"; }
      err() { echo "ERR $*" >> "$T/daemon.log"; }
      wan_ip_up() { return 0; }
      wan_ping_ok() { return 0; }
      at_cmd() { :; }
      at_query() { :; }
      ipv6_off_iface() { :; }
      pkill() { :; }
      sleep() { :; }
      tee() { cat > /dev/null; }
      timeout() { ip addr add 10.66.0.2/24 dev usb0 && ip route add default via 10.66.0.1 dev usb0; }
      bring_up_interface
    ) >> "$T/daemon.log" 2>&1
  }

  echo "== K. the daemon's LTE default route on a real kernel FIB"
  reset; mk eth1 10.1.0.2/24 && ip route add default via 10.1.0.1 dev eth1 proto dhcp metric 100 && mk usb0 10.66.0.9/24
  daemon_up; rc=$?
  want=$(printf '%s\n' "default via 10.1.0.1 dev eth1 proto dhcp metric 100" "default via 10.66.0.1 dev usb0 metric 1000" | sort)
  [ "$rc" = 0 ] && [ "$(defs)" = "$want" ] && pass "K1 wired + LTE (DA21 shape): eth1's NM default (metric 100) kept, usb0 at 1000, nothing else" \
    || failc "K1 rc=$rc defaults: $(defs | tr '\n' '|')"
  [ "$(dev_of 1.1.1.1)" = eth1 ] && pass "K1 the uplink is eth1 (wired beats LTE)" || failc "K1 route get 1.1.1.1 -> $(dev_of 1.1.1.1)"
  [ "$(dev_of 8.8.8.8 oif usb0)" = usb0 ] && ip -4 route get 8.8.8.8 oif usb0 | grep -q 'via 10.66.0.1' \
    && pass "K1 a usb0-bound lookup (ping -I usb0, the health check) still leaves via the modem" || failc "K1 oif usb0 -> $(ip -4 route get 8.8.8.8 oif usb0 2>&1)"
  grep -q '^WARN' "$T/daemon.log" && failc "K1 the daemon warned: $(grep '^WARN' "$T/daemon.log")" || pass "K1 no warning"

  daemon_up; daemon_up
  [ "$(defs)" = "$want" ] && pass "K2 two more connect cycles: still exactly eth1:100 + usb0:1000 (no duplicates)" || failc "K2 defaults: $(defs | tr '\n' '|')"

  reset; mk wlan0 10.2.0.2/24 && ip route add default via 10.2.0.1 dev wlan0 proto dhcp metric 50 && mk usb0 10.66.0.9/24
  daemon_up
  [ "$(defs | tr '\n' '|')" = "default via 10.2.0.1 dev wlan0 proto dhcp metric 50|default via 10.66.0.1 dev usb0 metric 1000|" ] && [ "$(dev_of 1.1.1.1)" = wlan0 ] \
    && pass "K3 WiFi + LTE: wlan0 (50) kept and preferred, usb0 at 1000" || failc "K3 defaults: $(defs | tr '\n' '|') uplink $(dev_of 1.1.1.1)"

  reset; mk usb0 10.66.0.9/24
  daemon_up
  [ "$(defs | tr '\n' '|')" = "default via 10.66.0.1 dev usb0 metric 1000|" ] && [ "$(dev_of 1.1.1.1)" = usb0 ] \
    && pass "K4 LTE alone: usb0 (1000) carries the uplink" || failc "K4 defaults: $(defs | tr '\n' '|') uplink $(dev_of 1.1.1.1)"

  reset; mk eth1 10.1.0.2/24 && ip route add default via 10.1.0.1 dev eth1 proto dhcp metric 100 && mk usb0 10.66.0.9/24
  daemon_up 1000
  [ "$(defs)" = "$want" ] && ! grep -q '^WARN' "$T/daemon.log" && pass "K5 the DA21 drop-in (SIM7600_ROUTE_METRIC=1000) keeps working: same table, no warning" \
    || failc "K5 defaults: $(defs | tr '\n' '|')"

  reset; mk eth1 10.1.0.2/24 && ip route add default via 10.1.0.1 dev eth1 proto dhcp metric 100 && mk usb0 10.66.0.9/24
  daemon_up 100
  [ "$(defs | tr '\n' '|')" = "default via 10.1.0.1 dev eth1 proto dhcp metric 100|default via 10.66.0.1 dev usb0 metric 100|" ] && [ "$(dev_of 1.1.1.1)" = eth1 ] \
    && pass "K6 an override EQUAL to eth1's metric (100): appended beside it, eth1 kept and still first" || failc "K6 defaults: $(defs | tr '\n' '|') uplink $(dev_of 1.1.1.1)"

  reset; mk usb0 10.66.0.9/24
  daemon_up 1e3
  [ "$(defs | tr '\n' '|')" = "default via 10.66.0.1 dev usb0 metric 1000|" ] && grep -q "^WARN SIM7600_ROUTE_METRIC='1e3' is not a route metric; using 1000" "$T/daemon.log" \
    && pass "K7 a malformed override falls back to 1000, with a warning" || failc "K7 defaults: $(defs | tr '\n' '|') log: $(tr '\n' '|' < "$T/daemon.log")"

  echo "== N. provision-rs.sh P1b: clear_stale_lte_defaults on a real kernel FIB"
  # The owners are stubbed in the subshell: systemctl (sim7600-lte), nmcli.
  # FAKE_SIM / FAKE_NM select their answers.
  prov_clear() {
    (
      # shellcheck source=scripts/provision-rs.sh
      source "$PROV"
      set +e
      systemctl() { [ "${1:-}" = is-active ] && echo "${FAKE_SIM:-inactive}"; return 0; }
      nmcli() { [ -n "${FAKE_NM:-}" ] && printf '%s\n' "$FAKE_NM"; return 0; }
      clear_stale_lte_defaults
    ) > "$T/prov.out" 2>&1
  }
  stale() { # the DA21 leftovers: eth1 v4 default at 101 + v6; usb0's v4 AND v6 defaults at 100
    reset
    mk eth1 10.1.0.2/24 fd00:1::2/64 && ip route add default via 10.1.0.1 dev eth1 proto dhcp metric 101 && ip -6 route add default via fd00:1::1 dev eth1 metric 1024
    mk usb0 10.66.0.9/24 fd00:66::2/64 && ip route add default via 10.66.0.1 dev usb0 proto dhcp metric 100 && ip -6 route add default via fd00:66::1 dev usb0 proto ra metric 100
  }
  usb0_defs() { echo "$(ip -4 route show default dev usb0 | wc -l)/$(ip -6 route show default dev usb0 | wc -l)"; }
  eth1_defs() { echo "$(ip -4 route show default dev eth1)|$(ip -6 route show default dev eth1)" | sed 's/  */ /g'; }

  stale; E1=$(eth1_defs)
  [ "$(usb0_defs)" = 1/1 ] && [ -n "$E1" ] && pass "N0 setup: usb0 has a v4 and a v6 default, eth1 both families" || failc "N0 setup: usb0 $(usb0_defs) eth1 '$E1'"
  prov_clear
  [ "$(usb0_defs)" = 0/0 ] && [ "$(eth1_defs)" = "$E1" ] && pass "N1 no live owner: usb0's v4 AND v6 defaults removed; eth1's untouched" || failc "N1 usb0 $(usb0_defs) eth1 '$(eth1_defs)' out: $(tr '\n' '|' < "$T/prov.out")"
  grep -q 'removed a stale IPv4 default route on usb0 (no live owner; metric 100)' "$T/prov.out" && grep -q 'removed a stale IPv6 default route on usb0 (no live owner; metric 100)' "$T/prov.out" \
    && pass "N1 both removals logged" || failc "N1 log: $(tr '\n' '|' < "$T/prov.out")"
  [ "$(ip -4 addr show dev usb0 | grep -c 'inet ')" = 1 ] && pass "N1 usb0's address is left alone (routes only)" || failc "N1 usb0 address changed"
  [ "$(dev_of 1.1.1.1)" = eth1 ] && pass "N1 the uplink is eth1 again" || failc "N1 uplink $(dev_of 1.1.1.1)"

  # A fake dhclient: a script NAMED dhclient (its comm), args as the daemon's.
  stale; printf '#!/bin/bash\nsleep 300 & s=$!\ntrap '"'"'kill $s; exit 0'"'"' TERM\nwait $s\n' > "$T/dhclient"; chmod 0755 "$T/dhclient"
  "$T/dhclient" -1 -v usb0 & DH=$!   # hl-clock-mocked: the fake dhclient written above (sleep + wait)
  sleep 0.3
  if [ "$(cat "/proc/$DH/comm" 2>/dev/null)" = dhclient ]; then
    prov_clear
    [ "$(usb0_defs)" = 1/1 ] && grep -q "usb0 default routes kept (live owner: dhclient (pid $DH))" "$T/prov.out" \
      && pass "N2 a running 'dhclient … usb0' owns the routes: kept" || failc "N2 usb0 $(usb0_defs) out: $(tr '\n' '|' < "$T/prov.out")"
  else failc "N2 setup: the fake dhclient's comm is '$(cat "/proc/$DH/comm" 2>/dev/null)'"; fi
  kill "$DH" 2>/dev/null; wait "$DH" 2>/dev/null
  "$T/dhclient" -1 -v eth1 & DH=$!   # hl-clock-mocked: the same fake dhclient
  sleep 0.3
  prov_clear
  [ "$(usb0_defs)" = 0/0 ] && pass "N3 a dhclient for eth1 is not usb0's owner: removed" || failc "N3 usb0 $(usb0_defs) out: $(tr '\n' '|' < "$T/prov.out")"
  kill "$DH" 2>/dev/null; wait "$DH" 2>/dev/null

  stale; FAKE_SIM=active prov_clear
  [ "$(usb0_defs)" = 1/1 ] && grep -q 'live owner: sim7600-lte.service' "$T/prov.out" && pass "N4 an active sim7600-lte (e.g. --reprovision) owns them: kept" || failc "N4 usb0 $(usb0_defs) out: $(tr '\n' '|' < "$T/prov.out")"
  FAKE_NM=$'eth1:connected\nusb0:connected' prov_clear
  [ "$(usb0_defs)" = 1/1 ] && grep -q 'live owner: NetworkManager (connected)' "$T/prov.out" && pass "N5 NetworkManager manages usb0 (connected): kept" || failc "N5 usb0 $(usb0_defs) out: $(tr '\n' '|' < "$T/prov.out")"
  FAKE_NM=$'eth1:connected\nusb0:unmanaged' prov_clear
  [ "$(usb0_defs)" = 0/0 ] && pass "N6 usb0 listed as 'unmanaged' (after the bootstrap's 0a conf): nobody's, removed" || failc "N6 usb0 $(usb0_defs) out: $(tr '\n' '|' < "$T/prov.out")"

  reset; mk eth1 10.1.0.2/24 && ip route add default via 10.1.0.1 dev eth1 proto dhcp metric 100; B=$(defs)
  prov_clear
  [ "$(defs)" = "$B" ] && [ ! -s "$T/prov.out" ] && pass "N7 no usb0 at all: nothing done, nothing printed" || failc "N7 defaults $(defs | tr '\n' '|') out: $(tr '\n' '|' < "$T/prov.out")"
  reset; mk eth1 10.1.0.2/24 && ip route add default via 10.1.0.1 dev eth1 proto dhcp metric 100 && mk usb0 10.66.0.9/24; B=$(defs)
  prov_clear
  [ "$(defs)" = "$B" ] && [ ! -s "$T/prov.out" ] && pass "N8 usb0 with no default route: nothing done, nothing printed" || failc "N8 out: $(tr '\n' '|' < "$T/prov.out")"

  reset; rm -rf "$T"
}

if [ "${LTE_ROUTES_NETNS:-}" = 1 ]; then
  kernel_rows
  exit $fail
fi

# ---------------------------------------------------------------- static rows
KERNEL_REQUIRED=0
[ "${1:-}" = --kernel ] && KERNEL_REQUIRED=1
echo "== S. static (daemon: ${DAEMON#"$REPO"/})"
code=$(grep -vE '^[[:space:]]*#' "$DAEMON")
grep -E 'route (replace|change)' <<<"$code" >/dev/null && failc "S1 the daemon replaces/changes a route: $(grep -nE 'route (replace|change)' "$DAEMON" | grep -v ':[[:space:]]*#' | head -3 | tr '\n' '|')" \
  || pass "S1 the daemon never replaces or changes a route"
fn=$(sed -n '/^install_lte_default_route() {$/,/^}$/p' "$DAEMON")
writes=$(grep -cE '\bip( -[46])? +route +(add|append|del|replace|change|flush)\b' <<<"$code")
inside=$(grep -cE '\bip( -[46])? +route +(add|append|del|replace|change|flush)\b' <<<"$fn")
[ -n "$fn" ] && [ "$writes" = "$inside" ] && [ "$writes" -ge 2 ] \
  && pass "S2 every default-route write ($writes) is inside install_lte_default_route" || failc "S2 route writes: $writes in the script, ${inside:-0} in install_lte_default_route"
grep -qE '^LTE_ROUTE_METRIC_DEFAULT=1000$' "$DAEMON" && grep -qE '^LTE_ROUTE_METRIC="\$\{SIM7600_ROUTE_METRIC:-\$LTE_ROUTE_METRIC_DEFAULT\}"$' "$DAEMON" \
  && pass "S3 the LTE metric defaults to 1000; SIM7600_ROUTE_METRIC still overrides it" || failc "S3 metric default"
grep -nE 'route (del|append|add)' <<<"$fn" | grep -v 'dev "\$IFACE"' >/dev/null && failc "S4 a route write in install_lte_default_route without dev \$IFACE" \
  || pass "S4 every route write in install_lte_default_route names dev \$IFACE"
bash -n "$DAEMON" && pass "S5 the daemon parses" || failc "S5 bash -n"
if command -v sha256sum >/dev/null; then
  (cd "$REPO/modem" && sha256sum --quiet -c SHA256SUMS) >/dev/null 2>&1 && pass "S6 modem/SHA256SUMS matches modem/" || failc "S6 modem/SHA256SUMS is stale"
fi

# ---------------------------------------------------------------- the kernel, in a private netns
KNOTE=""
if [ "$(id -u)" = 0 ] && command -v ip >/dev/null 2>&1 && command -v unshare >/dev/null 2>&1 && unshare -n true 2>/dev/null; then
  out=$(unshare -n env LTE_ROUTES_NETNS=1 LTE_DAEMON="$DAEMON" LTE_PROV="$PROV" bash "$SELF" 2>&1); krc=$?
  printf '%s\n' "$out"
  [ "$krc" = 0 ] || fail=1
  grep -q '^PASS K1 ' <<<"$out" && grep -q '^PASS N1 ' <<<"$out" || { [ "$krc" != 0 ] || failc "kernel rows: K1/N1 did not run"; }
else
  if [ "$KERNEL_REQUIRED" = 1 ]; then
    failc "kernel rows required (--kernel) but unavailable: need root, iproute2 and unshare -n (CAP_NET_ADMIN + CAP_SYS_ADMIN)"
  else
    echo "SKIP: the kernel rows (K, N) need root, iproute2 and unshare -n (CAP_NET_ADMIN + CAP_SYS_ADMIN); provision-rs-systemd.sh runs them with --kernel"
    KNOTE=" (kernel rows SKIPPED)"
  fi
fi
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)$KNOTE"
exit $fail
