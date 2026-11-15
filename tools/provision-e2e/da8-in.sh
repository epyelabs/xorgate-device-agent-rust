#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2086,SC2016  # assertions `a && ok || bad`; $pkgs is a word list; dpkg-query formats
# DA8, inside a fresh debian:bookworm arm64 container (see da8-arm64.sh).
# The baseline is what Raspberry Pi OS Lite already has (curl, ca-certificates,
# sudo, python3) plus the Raspberry Pi apt archive, both installed with REAL apt.
set -uo pipefail
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
TGZ=$(cd /in && ls xorgate-agent-rs-*.tar.gz)
export DEBIAN_FRONTEND=noninteractive
echo "== baseline (Pi OS Lite subset) + the Raspberry Pi archive"
apt-get update -qq && apt-get install -y -qq curl ca-certificates sudo python3 >/dev/null || { bad "baseline apt"; exit 1; }
install -d /etc/apt/keyrings
curl -fsSL http://archive.raspberrypi.com/debian/raspberrypi.gpg.key -o /etc/apt/keyrings/raspberrypi.asc || { bad "RPi archive key"; exit 1; }
echo 'deb [signed-by=/etc/apt/keyrings/raspberrypi.asc] http://archive.raspberrypi.com/debian/ bookworm main' > /etc/apt/sources.list.d/raspi.list
apt-get update -qq || { bad "apt-get update with the RPi archive"; exit 1; }
useradd -m pi
mkdir -p /t && tar -xzf "/in/$TGZ" -C /t
# shellcheck source=scripts/provision-rs.sh
. /t/provision-rs.sh   # defines APT_NEEDED etc. (main is guarded)
set +e +u
case $1 in
A)
  echo "== A: the binary's NEEDED vs provision-rs.sh's fatal apt set (${APT_NEEDED[*]})"
  out=$(/t/bin/xorgate-rs -v 2>&1); rc=$?
  [ $rc != 0 ] && grep -q 'error while loading shared libraries' <<<"$out" && ok "before apt: the binary does not load ($(grep -o 'lib[^:]*\.so[^:]*' <<<"$out" | head -1))" || bad "before apt: rc $rc: $out"
  apt-get install -y -qq "${APT_NEEDED[@]}" >/dev/null && ok "apt-get install ${APT_NEEDED[*]}" || bad "apt NEEDED"
  echo "  NEEDED resolution:"; ldd /t/bin/xorgate-rs | sed 's/^/    /'
  ldd /t/bin/xorgate-rs | grep -q 'not found' && bad "ldd: a NEEDED is not found" || ok "ldd resolves every NEEDED after ONLY the fatal set"
  out=$(/t/bin/xorgate-rs -v 2>&1); rc=$?
  [ $rc = 0 ] && [ "${out#xorgate-rs }" != "$out" ] && ok "smoke: $out" || bad "smoke after the fatal set: rc $rc $out"
  ;;
B)
  echo "== B: provision-rs.sh with real apt + pip, up to the claim (fake API: 409)"
  S=/var/lib/da8; mkdir -p "$S/bucket" && cd "$S" || exit 1
  openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=da8-ca -keyout ca.key -out ca.pem >/dev/null 2>&1
  openssl req -newkey rsa:2048 -nodes -subj /CN=da8 -keyout srv.key -out srv.csr >/dev/null 2>&1
  printf 'subjectAltName=DNS:api.e2e.invalid,DNS:dist.e2e.invalid\n' > ext
  openssl x509 -req -in srv.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 -extfile ext -out srv.pem >/dev/null 2>&1
  cp ca.pem /usr/local/share/ca-certificates/da8.crt && update-ca-certificates >/dev/null 2>&1
  echo '127.0.0.1 api.e2e.invalid dist.e2e.invalid' >> /etc/hosts
  echo api.e2e.invalid > api-hosts; echo dist.e2e.invalid > dist-hosts; echo 409 > claim-mode; echo ca > root-ca.pem
  python3 -I /in/fake-dist-api.py "$S" "$S/bucket" srv.pem srv.key 443 & sleep 2
  mkdir -p /tmp/d && cp "/in/$TGZ" "/in/$TGZ.sha256" /tmp/d/
  t0=$(date +%s)
  SUDO_USER=pi bash /t/provision-rs.sh --reg-code da8code --api-base https://api.e2e.invalid/v1 --from https://dist.e2e.invalid \
    --tarball "/tmp/d/$TGZ" > /tmp/run1.log 2>&1; rc=$?
  echo "  run 1: exit $rc after $(( $(date +%s) - t0 )) s"; grep -E 'WARN|ERROR|staged|claiming' /tmp/run1.log | sed 's/^/    | /'
  [ $rc = 5 ] && ok "run 1: exit 5 (claim refused by the fake API; nothing past the claim ran)" || { bad "run 1: exit $rc"; tail -30 /tmp/run1.log; }
  grep -q 'staged xorgate-rs ' /tmp/run1.log && ok "the smoke-exec passed after the real apt steps" || bad "no smoke banner"
  [ "$(wc -l < claims.log)" = 1 ] && grep -q '"agent": "rust"' claims.log && grep -q '"platform": "aarch64"' claims.log && ok "the claim reached the API (agent rust, platform aarch64)" || bad "claims: $(cat claims.log)"
  grep -q 'WARN' /tmp/run1.log && bad "a WARN step failed for real (see above)" || ok "no WARN: every apt/pip step succeeded"
  python3 -c 'import adafruit_bno08x, adafruit_blinka' 2>/tmp/imp.err && ok "import adafruit_bno08x (+ adafruit_blinka)" || bad "IMU import: $(tail -1 /tmp/imp.err)"
  pkgs="curl ca-certificates jq build-essential python3 i2c-tools python3-pip python3-dev ${APT_NEEDED[*]} ${APT_GST_PLUGINS[*]} gstreamer1.0-libcamera"
  missing=$(for p in $pkgs; do dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'install ok installed' || echo "$p"; done)
  [ -z "$missing" ] && ok "every package of the set is installed ($(echo $pkgs | wc -w))" || bad "not installed: $missing"
  ldd /opt/xorgate-rs.new/bin/xorgate-rs >/dev/null 2>&1 && bad "the staging dir survived the exit-5 path" || ok "/opt/xorgate-rs.new removed on exit 5"
  for p in /opt/xorgate-rs /etc/xorgate /etc/systemd/system/xorgate-agent-rs.service; do [ ! -e $p ] || bad "$p exists after exit 5"; done
  pip_list=$(python3 -m pip list 2>/dev/null | grep -iE '^(adafruit-circuitpython-bno08x|adafruit-blinka) ' | tr -s ' ' | tr '\n' ';'); echo "  pip: $pip_list"
  echo "== B2: a re-run under a window-only APT::Get::Upgrade \"false\" (the bench mechanism)"
  dpkg-query -W > /tmp/dpkg.before
  echo 'APT::Get::Upgrade "false";' > /etc/apt/apt.conf.d/00p8-no-upgrade
  rm -f claims.log; SUDO_USER=pi bash /t/provision-rs.sh --reg-code da8code --api-base https://api.e2e.invalid/v1 --from https://dist.e2e.invalid \
    --tarball "/tmp/d/$TGZ" > /tmp/run2.log 2>&1; rc=$?
  [ $rc = 5 ] && ok "run 2: exit 5 again" || bad "run 2: exit $rc"
  dpkg-query -W > /tmp/dpkg.after
  cmp -s /tmp/dpkg.before /tmp/dpkg.after && ok "run 2 changed no package version ($(wc -l < /tmp/dpkg.after) packages)" || { bad "run 2 changed packages"; diff /tmp/dpkg.before /tmp/dpkg.after | head; }
  ;;
esac
echo "== result $1: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
