#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016,SC2013  # assertions `a && pass || failc`; literal $VARs of the JS golden; step labels are words
# Phase 8 WP-R1: scripts/provision-rs.sh's bodies and shared system steps.
#   1. the vendored backend goldens (tools/provision-e2e/golden) match SOURCE;
#   2. every provisioner body == fixtures/install_rs/bodies.json (which
#      crates/xorgate-core/tests/install_rs_bodies.rs ties to update_plan), and
#      the bodies it shares with install-rs.sh are byte-equal to that script's;
#   3. the JS-shared bodies == the heredocs of the JS install.sh golden
#      (10-user.conf, 020, 021, 023, logind, uplink, apt lock, the legacy
#      pipeline writer), and agent.env == the JS heredoc under the FIXED
#      Rust mapping (media root, CLI name; DEVICE_DIST_VERSION=latest literal);
#   4. the apt package set == the JS set minus nodejs; NEEDED providers are a
#      subset; no apt call overrides apt's configuration (a window-only
#      APT::Get::Upgrade "false" is honoured);
#   5. no JS-only step (node, npm, the JS unit/updater, 022, the `xorgate`
#      alias) and no modem restart in the provisioner;
#   6. every numbered step of the JS golden is named in docs/provision.md's
#      step table (the drift tripwire, D1.6);
#   7. follow_log advances the caller's offset (F-DA21-FOLLOW, v0.1.1);
#   8. P1b order + route-write limits (F-DA21-NM, v0.1.1; the kernel rows
#      are scripts/tests/lte-routes.sh).
# Needs bash 4 + sed/awk/grep:
#   docker run --rm -v "$PWD:/src:ro" -w /src debian:bookworm bash scripts/tests/provision-rs-bodies.sh
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
GOLDEN="$REPO/fixtures/install_rs/bodies.json"
GD="$REPO/tools/provision-e2e/golden"
JSG="$GD/js-install.golden.sh"
PROV="$REPO/scripts/provision-rs.sh"
DOC="$REPO/docs/provision.md"
fail=0
pass() { echo "PASS $*"; }
failc() { echo "FAIL $*"; fail=1; }

# The bodies.json reader of scripts/tests/install-rs-bodies.sh (\n and \" only).
golden() {
  local raw rest
  case $1 in
    *.*) raw=$(sed -n "/^  \"${1%%.*}\": {\$/,/^  },\{0,1\}\$/p" "$GOLDEN" \
           | sed -n "s/^    \"${1#*.}\": \"\(.*\)\",\{0,1\}\$/\1/p") ;;
    *) raw=$(sed -n "s/^  \"$1\": \"\(.*\)\",\{0,1\}\$/\1/p" "$GOLDEN") ;;
  esac
  [ "$(printf '%s\n' "$raw" | wc -l)" = 1 ] && [ -n "$raw" ] || { echo "golden: no single \"$1\" line" >&2; return 1; }
  rest=${raw//\\n/}; rest=${rest//\\\"/}
  case $rest in *\\*) printf 'golden: "%s" uses an escape other than \\n or \\"\n' "$1" >&2; return 1 ;; esac
  raw=${raw//\\\"/\"}
  printf '%s' "${raw//\\n/$'\n'}"
}
same() { # <label> <want> <got>
  if [ "$2" = "$3" ]; then pass "$1"; else
    failc "$1"; diff <(printf '%s' "$2") <(printf '%s' "$3") | sed 's/^/    /'
  fi
}
sha() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1 || shasum -a 256 "$1" | cut -d' ' -f1; }

echo "== 1. vendored goldens"
for f in rust-bootstrap.golden.sh js-install.golden.sh; do
  want=$(awk -v f="$f" '$1 == f { print $2 }' "$GD/SOURCE")
  [ -n "$want" ] && [ "$(sha "$GD/$f")" = "$want" ] && pass "$f == SOURCE sha ${want:0:12}" || failc "$f sha $(sha "$GD/$f") != SOURCE ${want:-none}"
done

echo "== 2. provision-rs.sh bodies == bodies.json (and == install-rs.sh where shared)"
# Both scripts guard main(): sourcing only defines the functions.
# shellcheck source=scripts/provision-rs.sh
source "$PROV"
set +e
for u in pi operator; do
  same "userDropin.$u" "$(golden "userDropin.$u"; echo X)" "$(user_dropin_body "$u" "$u"; echo X)"
  same "powerSudoers.$u (020)" "$(golden "powerSudoers.$u"; echo X)" "$(power_sudoers_body "$u"; echo X)"
  same "clockSudoers.$u (021 = update_plan)" "$(golden "clockSudoers.$u"; echo X)" "$(clock_sudoers_body "$u"; echo X)"
  same "camrotateSudoers.$u (023 = update_plan)" "$(golden "camrotateSudoers.$u"; echo X)" "$(camrotate_sudoers_body "$u"; echo X)"
  same "updaterSudoers.$u (024 = update_plan)" "$(golden "updaterSudoers.$u"; echo X)" "$(rs_update_sudoers_body "$u"; echo X)"
done
same "wrapper (update_plan::WRAPPER_BODY)" "$(golden wrapper; echo X)" "$(rs_wrapper_body; echo X)"
same "nativeDropin (20-native.conf)" "$(golden nativeDropin; echo X)" "$(native_dropin_body; echo X)"
same "nativeGenerator (the boot-time enablement)" "$(golden nativeGenerator; echo X)" "$(native_generator_body; echo X)"
same "nativeDistDropin (10-dist.conf)" "$(golden nativeDistDropin; echo X)" "$(native_dist_dropin_body; echo X)"
same "logindRemoveIpc" "$(golden logindRemoveIpc; echo X)" "$(logind_body; echo X)"
same "uplinkPriority" "$(golden uplinkPriority; echo X)" "$(uplink_body; echo X)"
same "aptLockTimeout" "$(golden aptLockTimeout; echo X)" "$(apt_lock_body; echo X)"
W=$(rs_wrapper_body; echo X); U=$(rs_update_sudoers_body pi; echo X)
# shellcheck source=scripts/install-rs.sh
source "$REPO/scripts/install-rs.sh"
set +e
same "wrapper == install-rs.sh rs_wrapper_body" "$(rs_wrapper_body; echo X)" "$W"
same "024 == install-rs.sh rs_update_sudoers_body" "$(rs_update_sudoers_body pi; echo X)" "$U"

echo "== 3. JS-shared bodies == the JS install.sh golden"
# heredoc <start line (fixed string)> <terminator> [nth]: the body after the
# nth line equal to <start>, up to the terminator, with the template's user
# variables rendered for pi.
heredoc() {
  awk -v s="$1" -v t="$2" -v n="${3:-1}" '
    on && $0 == t { exit }
    on { print }
    index($0, s) && !on { c++; if (c == n) on = 1 }' "$JSG" \
    | sed -e 's/\$SVC_USER/pi/g' -e 's/\$SVC_GROUP/pi/g'
}
same "10-user.conf == JS" "$(heredoc 'cat > "$UNIT_DEST.d/10-user.conf" <<EOF' EOF; echo X)" "$(user_dropin_body pi pi; echo X)"
same "020 == JS" "$(heredoc 'cat > "$SUDOERS_TMP" <<EOF' EOF 1; echo X)" "$(power_sudoers_body pi; echo X)"
same "021 == JS" "$(heredoc 'cat > "$SUDOERS_TMP" <<EOF' EOF 2; echo X)" "$(clock_sudoers_body pi; echo X)"
same "023 == JS" "$(heredoc 'cat > "$SUDOERS_TMP" <<EOF' EOF 3; echo X)" "$(camrotate_sudoers_body pi; echo X)"
heredoc 'cat > "$SUDOERS_TMP" <<EOF' EOF 4 | grep -q 'xorgate-updater.service' && pass "(the 4th JS sudoers heredoc is 022, which the native install replaces with 024)" || failc "JS sudoers heredoc order changed"
same "logind == JS" "$(heredoc "cat > /etc/systemd/logind.conf.d/10-xorgate-removeipc.conf <<'EOF'" EOF; echo X)" "$(logind_body; echo X)"
# v0.1.1 F-LTE-METRIC: the Rust sim7600-lte.sh defaults to metric 1000 (the JS
# copy keeps 100, user decision 2026-11-18), so the uplink comment names 1000.
# The mapping is that ONE comment line; every other byte stays the JS heredoc's.
JS_UPLINK=$(heredoc "cat > /etc/NetworkManager/conf.d/10-uplink-priority.conf <<'NMEOF'" NMEOF; echo X)
OLD_L='# sim7600-lte daemon installs its default route at metric 100, so WiFi wins'
NEW_L='# sim7600-lte daemon installs its default route at metric 1000, so WiFi wins'
[ "$(grep -cxF "$OLD_L" <<<"$JS_UPLINK")" = 1 ] && pass "JS uplink heredoc names metric 100 once (the mapped line)" || failc "JS uplink heredoc: the metric-100 line moved"
same "uplink == JS under the Rust LTE metric (100 -> 1000 in the comment only)" "${JS_UPLINK/"$OLD_L"/"$NEW_L"}" "$(uplink_body; echo X)"
grep -qE '^LTE_ROUTE_METRIC_DEFAULT=1000$' "$REPO/modem/sim7600-lte.sh" && pass "…and the shipped sim7600-lte.sh defaults to 1000" || failc "modem/sim7600-lte.sh: LTE_ROUTE_METRIC_DEFAULT is not 1000"
grep -qxF "    echo 'DPkg::Lock::Timeout \"600\";' > /etc/apt/apt.conf.d/99xorgate-lock-timeout" "$JSG" \
  && [ "$(apt_lock_body)" = 'DPkg::Lock::Timeout "600";' ] && pass "apt lock timeout == JS" || failc "apt lock timeout differs from JS"
fn() { sed -n '/^write_bridged_pipeline() {$/,/^}$/p' "$1"; }
[ -n "$(fn "$JSG")" ] && same "write_bridged_pipeline == JS, verbatim" "$(fn "$JSG"; echo X)" "$(fn "$PROV"; echo X)" || failc "write_bridged_pipeline not found in the JS golden"
# agent.env: the JS heredoc rendered with the same inputs, then the FIXED Rust
# mapping. Any other difference is a drift.
JS_ENV=$(heredoc 'install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/agent.env" <<EOF' EOF \
  | sed -e 's#\$DEVICE_ID#dev-1#; s#\$ENDPOINT#x.iot#; s#\$CONF_DIR#/etc/xorgate#g; s#\$INSTALL_PREFIX#/opt/xorgate#g' \
        -e 's#\$DIST_BUCKET_URL#https://bucket.test#; s#\$DIST_VERSION#latest#')
[ "$(printf '%s\n' "$JS_ENV" | grep -c '\$')" = 0 ] && [ -n "$JS_ENV" ] && pass "JS agent.env heredoc extracted + rendered" || failc "JS agent.env heredoc: $(printf '%s\n' "$JS_ENV" | grep '\$')"
WANT_ENV=$(printf '%s\n' "$JS_ENV" | sed -e 's#=/opt/xorgate/media#=/opt/xorgate-rs/media#' \
  -e "s#'xorgate set #'xorgate-rs set #; s#'xorgate pipeline #'xorgate-rs pipeline #; s#'xorgate update'#'xorgate-rs update'#")
same "agent.env == JS under the Rust mapping (media root, CLI name)" "$WANT_ENV" "$(agent_env_body dev-1 x.iot https://bucket.test)"
# review-wp-b S2 / the orchestrator's WP-R1 note: the LITERAL latest, never a
# version derived from the tarball (I-P8-1 passes none).
ENV_V=$(agent_env_body dev-1 x.iot https://bucket.test | grep '^DEVICE_DIST_VERSION=')
[ "$ENV_V" = DEVICE_DIST_VERSION=latest ] && pass "agent.env: DEVICE_DIST_VERSION=latest (literal)" || failc "agent.env: '$ENV_V'"
grep -qx 'DEVICE_DIST_VERSION=latest' "$PROV" && ! grep 'DEVICE_DIST_VERSION=' "$PROV" | grep -v 'DEVICE_DIST_VERSION=latest' >/dev/null \
  && pass "provision-rs.sh writes DEVICE_DIST_VERSION only as the literal latest" || failc "a non-literal DEVICE_DIST_VERSION in provision-rs.sh: $(grep -n 'DEVICE_DIST_VERSION=' "$PROV")"

echo "== 4. apt: the JS set minus nodejs; NEEDED providers fatal; no override of apt's config"
# Every package named on an `apt-get install -y -qq` line (continuations joined).
pkgs() {
  sed -e ':a' -e '/\\$/N; s/\\\n//; ta' "$1" | grep -E '^[[:space:]]*apt-get install -y -qq' \
    | sed -e 's/.*apt-get install -y -qq//' -e 's/[|&]\{2\}.*//' -e 's/2>\/dev\/null//' | tr ' ' '\n' | sed '/^$/d'
}
JSP=$(pkgs "$JSG" | sort -u)
RSP=$( { pkgs "$PROV" | grep -v '^"\$@"$' | grep -v '^"\${APT_'; printf '%s\n' curl ca-certificates jq build-essential python3 "${APT_NEEDED[@]}" "${APT_GST_PLUGINS[@]}"; } | sort -u)
same "Rust apt set == JS apt set minus nodejs" "$(printf '%s\n' "$JSP" | grep -vx nodejs)" "$RSP"
for p in "${APT_NEEDED[@]}"; do printf '%s\n' "$JSP" | grep -qx "$p" && pass "NEEDED provider $p is a JS package" || failc "NEEDED provider $p not in the JS set"; done
grep -E 'apt-get .*(-o |--only-upgrade|--upgrade|-u |--allow-downgrades|--reinstall)' "$PROV" >/dev/null && failc "an apt-get call overrides apt's configuration" || pass "no apt-get call overrides apt's configuration (APT::Get::Upgrade \"false\" is honoured)"

echo "== 5. no JS-only step, no modem restart"
for re in 'nodesource' 'npm ' '\bnode ' 'xorgate-updater\.service' '022_xorgate-update' '/usr/local/bin/xorgate[^-]' 'enable --now xorgate-agent\b' 'restart sim7600' 'try-restart sim7600' 'systemctl (reboot|poweroff)$'; do
  if grep -vE '^[[:space:]]*#' "$PROV" | grep -E "$re" | grep -v 'NOPASSWD' >/dev/null; then failc "provision-rs.sh matches /$re/"; else pass "provision-rs.sh has no /$re/"; fi
done

echo "== 6. every numbered JS step is named in docs/provision.md"
if [ -f "$DOC" ]; then
  for s in $(grep -oE '^# [0-9]+[a-z0-9]*\.' "$JSG" | sed 's/^# //; s/\.$//' | sort -u); do
    grep -qE "^\| (JS )?$s( |\|)" "$DOC" && pass "JS step $s in the D1.4 table" || failc "JS step $s missing from docs/provision.md's step table"
  done
else failc "docs/provision.md missing"; fi

echo "== 7. follow_log advances the CALLER's offset (F-DA21-FOLLOW, v0.1.1)"
# v0.1.0 declared `local n off=${!1}` and the hand-off loop passes `off`, so
# `printf -v off` set the function's own local and every 1 s poll re-printed
# the whole run (DA21: 61 791 client lines). Both caller names are pinned:
# `off` (the real caller, the one that collided) and another one.
FL_DIR=$(mktemp -d)
for var in off pos; do
  PROV_LOG=$FL_DIR/prov-$var.log
  printf 'line 1\nline 2\n' > "$PROV_LOG"
  printf -v "$var" '%s' 0
  got1=$(follow_log "$var"; echo "rc=$?")
  follow_log "$var" > /dev/null   # in THIS shell: the offset must move
  printf 'line 3\n' >> "$PROV_LOG"
  got2=$(follow_log "$var"; echo "rc=$?")
  follow_log "$var" > /dev/null
  got3=$(follow_log "$var"; echo "rc=$?")
  same "follow_log $var: the first poll prints the log so far" "$(printf 'line 1\nline 2\nrc=0')" "$got1"
  same "follow_log $var: the caller's offset = the log size" "$(stat -c %s "$PROV_LOG" 2>/dev/null || wc -c < "$PROV_LOG" | tr -d ' ')" "${!var}"
  same "follow_log $var: the next poll prints ONLY what the log gained" "$(printf 'line 3\nrc=0')" "$got2"
  same "follow_log $var: an idle poll prints nothing" "rc=0" "$got3"
done
rm -rf "$FL_DIR"

echo "== 8. P1b (F-DA21-NM, v0.1.1): ModemManager off, then the stale usb0 defaults, BEFORE the first apt call"
body=$(sed -n '/^provision_main() {$/,/^}$/p' "$PROV" | grep -vE '^[[:space:]]*#')
lnum() { grep -nE "$1" <<<"$body" | head -n1 | cut -d: -f1; }
l_mm=$(lnum '^[[:space:]]*if sim7600_present; then disable_modemmanager; fi$'); l_cl=$(lnum '^[[:space:]]*clear_stale_lte_defaults$')
l_apt=$(lnum 'apt-get |apt_hard '); l_pip=$(lnum 'pip install')
[ -n "$l_mm" ] && [ -n "$l_cl" ] && [ -n "$l_apt" ] && [ -n "$l_pip" ] && [ "$l_mm" -lt "$l_cl" ] && [ "$l_cl" -lt "$l_apt" ] && [ "$l_cl" -lt "$l_pip" ] \
  && pass "provision_main: (SIM7600 present) disable_modemmanager ($l_mm) < clear_stale_lte_defaults ($l_cl) < the first apt ($l_apt) / pip ($l_pip)" \
  || failc "P1b order: mm=$l_mm clear=$l_cl apt=$l_apt pip=$l_pip"
grep -E 'route (replace|change|flush)' "$PROV" | grep -vE '^[[:space:]]*#' >/dev/null && failc "provision-rs.sh replaces/flushes a route" || pass "provision-rs.sh never replaces, changes or flushes a route"
dels=$(grep -E 'ip -?[^ ]* *route del' "$PROV" | grep -vE '^[[:space:]]*#' | grep -v 'By hand')
[ -n "$dels" ] && ! grep -v 'route del default dev "\$LTE_IFACE"' <<<"$dels" >/dev/null && [ "$(grep -c '^LTE_IFACE=usb0$' "$PROV")" = 1 ] \
  && pass "every route delete is 'default dev \$LTE_IFACE' (usb0)" || failc "a route delete not limited to usb0's defaults: $dels"

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
