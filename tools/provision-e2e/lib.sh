#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2034  # assertions `a && ok || bad`; the variables are the sourcing suite's
# tools/provision-e2e/lib.sh: the shared harness of the two real-systemd suites
# scripts/tests/provision-e2e.sh (DA6) and scripts/tests/provision-rs-systemd.sh
# (DA7), Phase 8 WP-R1. Sourced, never run. It runs ONLY inside a throwaway
# privileged debian:bookworm container with systemd 252 as PID 1 (hl.sh
# `systemd`; recipe in each suite's header) and refuses anywhere else.
#
# What is real: systemd (units, Type=notify, Conflicts=, conditions, drop-ins),
# bash, coreutils, tar, curl + TLS, jq, visudo/sudo, usermod, the vendored
# bootstrap golden (run UNMODIFIED), scripts/pack.sh, scripts/provision-rs.sh.
# What is stubbed (on a PATH dir the provisioner sees first): apt-get (a fake
# dpkg state under $E/apt, with per-case failures), `python3 -m pip`, uname -m
# (aarch64: the host is x86_64), modprobe and udevadm (never touch the HOST
# kernel from a privileged container), nmcli, ip and ping (a model of the
# default routes and of a usb0-bound probe for P1b: the provisioner never
# touches the container's network). The agent binary is a python
# Type=notify stand-in; the modem daemon and the system/ helpers in the test
# tarball are inert stubs, so no real modem, clock or boot-config tool ever
# runs (handbook §3a: no clock/RTC tool, never the real agent).
# The fake bucket + claim API (fake-dist-api.py) answers on 127.0.0.1:443 for
# the golden's two PROD hostnames and two .invalid ones via /etc/hosts and a
# test CA; setup refuses to go on unless every name resolves to 127.0.0.1.
# raspi-config is absent here, so the modprobe path runs. On a real Pi
# `raspi-config nonint do_i2c 0` rewrites config.txt through .bak + mv (as the
# JS installer does): the same bytes, a NEW inode. The bench-shaped case
# therefore compares config.txt by sha only, and so must W1/W2 (review N4).
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SRC=$(cd "$HERE/../.." && pwd)
E=/var/lib/e2e
W=/var/tmp/e2e
SHIM=/usr/local/e2e/bin
BUCKET=$W/bucket
BOOT=$HERE/golden/rust-bootstrap.golden.sh
PROV_SRC=$SRC/scripts/provision-rs.sh
VER=v-e2e1
TGZ=xorgate-agent-rs-$VER.tar.gz
DIST_TEST=https://dist.e2e.invalid
API_TEST=https://api.e2e.invalid/v1
OUT=$W/out
RC=0
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
check() { if [ "$1" = 0 ]; then ok "$2"; else bad "$2"; fi; }   # <status> <label>

# The golden's hostnames (its API_BASE and DIST_BUCKET_URL lines).
golden_host() { sed -n "s#^$1='https://\([^/']*\).*#\1#p" "$BOOT"; }

e2e_guard() {
  [ "$(cat /proc/1/comm 2>/dev/null)" = systemd ] || { echo "FAIL: systemd is not PID 1 (run inside the suite's container)"; exit 2; }
  [ -f /.dockerenv ] || { echo "FAIL: not a docker container: refusing to provision this host"; exit 2; }
  [ "$(id -u)" = 0 ] || { echo "FAIL: needs root"; exit 2; }
  local t
  for t in python3 jq curl openssl visudo flock; do type -P "$t" >/dev/null || { echo "FAIL: $t missing (see the suite header)"; exit 2; }; done
  for t in /opt/xorgate /opt/xorgate-rs /etc/xorgate; do [ ! -e "$t" ] || { echo "FAIL: $t exists before setup: not a fresh container"; exit 2; }; done
}

write_shims() {
  mkdir -p "$SHIM" "$E/apt/installed"
  cat > "$SHIM/apt-get" <<'EOF'
#!/bin/bash
# e2e apt-get: a fake dpkg state. $E/apt-fail lines: `update`, `<pkg>` (that
# call fails, exit 100) or `lie:<pkg>` (exit 0, but the package is not there).
E=/var/lib/e2e
echo "apt-get $*" >> $E/apt.log
case ${1:-} in
  update) grep -qx update $E/apt-fail 2>/dev/null && { echo "E: e2e: update failed" >&2; exit 100; }; exit 0 ;;
  install)
    shift; pkgs=()
    for a in "$@"; do case $a in -*) ;; *) pkgs+=("$a") ;; esac; done
    for p in "${pkgs[@]}"; do grep -qx "$p" $E/apt-fail 2>/dev/null && { echo "E: Unable to locate package $p" >&2; exit 100; }; done
    for p in "${pkgs[@]}"; do grep -qx "lie:$p" $E/apt-fail 2>/dev/null || : > "$E/apt/installed/$p"; done
    exit 0 ;;
  *) echo "e2e apt-get: unmodelled: $*" >&2; exit 100 ;;
esac
EOF
  cat > "$SHIM/python3" <<'EOF'
#!/bin/bash
# e2e: `python3 -m pip …` is recorded (and fails while $E/pip-fail exists); everything else is the real python3.
if [ "${1:-}" = -m ] && [ "${2:-}" = pip ]; then
  echo "pip $*" >> /var/lib/e2e/apt.log
  [ ! -e /var/lib/e2e/pip-fail ] || { echo "ERROR: e2e pip failure" >&2; exit 1; }
  exit 0
fi
exec /usr/bin/python3 "$@"
EOF
  cat > "$SHIM/uname" <<'EOF'
#!/bin/bash
# e2e: the host is x86_64; the installers check for aarch64.
if [ "${1:-}" = -m ]; then echo "${E2E_ARCH:-aarch64}"; exit 0; fi
exec /usr/bin/uname "$@"
EOF
  for t in modprobe udevadm; do
    printf '#!/bin/bash\n# e2e: recorded, never run for real (the host kernel/udev).\necho "%s $*" >> /var/lib/e2e/calls.log\nexit 0\n' "$t" > "$SHIM/$t"
  done
  # nmcli: recorded; `-t -f DEVICE,STATE device` answers $E/nmcli.out when present.
  cat > "$SHIM/nmcli" <<'EOF'
#!/bin/bash
# e2e: recorded, never run for real.
echo "nmcli $*" >> /var/lib/e2e/calls.log
if [ "$*" = "-t -f DEVICE,STATE device" ] && [ -f /var/lib/e2e/nmcli.out ]; then cat /var/lib/e2e/nmcli.out; fi
exit 0
EOF
  # v0.1.1 P1b (F-DA21-NM): ip is a MODEL of the default routes only, so the
  # provisioner never touches this container's network (the kernel semantics
  # are scripts/tests/lte-routes.sh's, in a private netns). $E/net/usb0 = the
  # link exists; $E/net/route4 / route6 = one usb0 default per line;
  # $E/net/other4 = the other interfaces' IPv4 defaults (full lines). Every
  # call goes to apt.log, so its order against apt-get is on record.
  cat > "$SHIM/ip" <<'EOF'
#!/bin/bash
N=/var/lib/e2e/net
echo "ip $*" >> /var/lib/e2e/apt.log
case "$*" in
  "link show usb0") [ -e "$N/usb0" ] ;;
  "-4 route show default")
    cat "$N/other4" 2>/dev/null
    sed 's/^default via \([^ ]*\)/default via \1 dev usb0/' "$N/route4" 2>/dev/null; exit 0 ;;
  "-4 route show default dev usb0") cat "$N/route4" 2>/dev/null; exit 0 ;;
  "-6 route show default dev usb0") cat "$N/route6" 2>/dev/null; exit 0 ;;
  "-4 route del default dev usb0" | "-6 route del default dev usb0")
    f=$N/route${1#-}
    [ -s "$f" ] || { echo "RTNETLINK answers: No such process" >&2; exit 2; }
    sed -i 1d "$f" ;;
  *) echo "e2e ip: unmodelled: $*" >&2; exit 1 ;;
esac
EOF
  # ping: recorded in apt.log; a usb0-bound probe answers while $E/net/usb0-live exists.
  cat > "$SHIM/ping" <<'EOF'
#!/bin/bash
echo "ping $*" >> /var/lib/e2e/apt.log
case " $* " in *" -I usb0 "*) [ -e /var/lib/e2e/net/usb0-live ] ;; *) exit 1 ;; esac
EOF
  printf '#!/bin/bash\necho reload >> /var/lib/e2e/nm-reloads\n' > "$SHIM/nm-reload"
  chmod 0755 "$SHIM"/*
}

# The agent stand-in: a real Type=notify main process (READY=1 from its own
# pid, WATCHDOG=1 every second), a GStreamer-linked binary's -v (fails like
# the loader while the fake dpkg lacks the NEEDED packages), and `update`,
# which execs the REAL x86_64 xorgate-rs at $E/real-xorgate-rs when present.
write_stub_bin() { # <dest>
  cat > "$1" <<'PY'
#!/usr/bin/python3
import os, signal, socket, sys, time
E = "/var/lib/e2e"
def log(m):
    try:
        with open(E + "/agent.log", "a") as f:
            f.write(m + "\n")
    except OSError:
        pass
def notify(m):
    p = os.environ.get("NOTIFY_SOCKET")
    if p:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        s.sendto(m.encode(), "\0" + p[1:] if p.startswith("@") else p)
        s.close()
def setting(name, default=""):
    try:
        return open(E + "/" + name).read().strip()
    except OSError:
        return default
a = sys.argv[1:]
if a[:1] in (["-v"], ["--version"]):
    for lib, pkg in (("libgstreamer-1.0.so.0", "libgstreamer1.0-0"), ("libglib-2.0.so.0", "libglib2.0-0")):
        if not os.path.exists(E + "/apt/installed/" + pkg):
            sys.stderr.write(sys.argv[0] + ": error while loading shared libraries: " + lib + ": cannot open shared object file: No such file or directory\n")
            sys.exit(127)
    print("xorgate-rs 0.0.1 (e2e stub)")
elif a[:1] == ["run"]:
    if setting("agent-mode") == "fail":
        log("rs run fail")
        sys.exit(1)
    log("rs start pid=%d uid=%d device=%s dist=%s kvs=%s" % (os.getpid(), os.getuid(), os.environ.get("DEVICE_ID", ""),
        os.environ.get("DEVICE_DIST_VERSION", ""), os.environ.get("KVS_RUNTIME_BIN", "")))
    signal.signal(signal.SIGTERM, lambda *_: (log("rs stop pid=%d" % os.getpid()), sys.exit(0)))
    time.sleep(float(setting("ready-delay", "0") or 0))
    notify("READY=1")
    while True:
        time.sleep(1)
        notify("WATCHDOG=1")
elif a[:1] == ["update"]:
    real = E + "/real-xorgate-rs"
    if os.access(real, os.X_OK):
        os.execv(real, [real] + a)
    sys.stderr.write("e2e stub: no real binary for update\n")
    sys.exit(3)
elif a[:1] == ["led"]:
    log("rs led " + " ".join(a[1:]))
elif a[:3] == ["camera", "apply", "--from-claim"] and len(a) == 4:
    # The stand-in for `xorgate-rs camera apply` (step 8b). The REAL command
    # is tested against the REAL helper on fixture files by
    # crates/xorgate-agent/tests/camera_apply.rs; this models what the
    # provisioner sees of it: the claim file it is handed, the lines it
    # writes (always an explicit rotation=, one line per port, a line of the
    # same sensor left alone), its per-port output and its last line.
    import json, re
    log("rs camera apply --from-claim " + a[3])
    DEFAULT = {"imx290": 0, "imx219": 180, "imx708": 180, "imx335": 180}
    claim = json.load(open(a[3]))
    video = (claim.get("hardware") or {}).get("video") or []
    setting = (claim.get("cameraSensors") or {}).get("cameras")
    if setting is not None:
        ports = {k: e.get("sensor") for k, e in setting.items()}
    elif video:
        ports = {v["connector"].replace("csi", "cam"): v["sensor"] for v in video}
    else:
        ports = {"cam0": "imx290", "cam1": "imx290"}
    path = "/boot/firmware/config.txt"
    lines = open(path).read().split("\n")
    changed = False
    for cam, sensor in ports.items():
        mine = [l for l in lines if re.match(r"dtoverlay=imx[0-9]+(,|$)", l) and (("cam0" in l.split(",")) == (cam == "cam0"))]
        if sensor == "none":
            if mine:
                lines = [l for l in lines if l not in mine]
                changed = True
                print(cam + ": camera line removed (was: " + mine[-1] + ")")
            else:
                print(cam + ": no change (no camera line)")
            continue
        if sensor not in DEFAULT:
            print(cam + ": FAILED: this agent build does not know the camera sensor \"" + str(sensor) + "\"")
            sys.exit(1)
        if len(mine) == 1 and mine[0].split(",")[0] == "dtoverlay=" + sensor:
            print(cam + ": no change (" + mine[0] + ")")
            continue
        want = "dtoverlay=%s,%s,rotation=%d" % (sensor, cam, DEFAULT[sensor])
        lines = [l for l in lines if l not in mine]
        while lines and lines[-1] == "":
            lines.pop()
        lines += [want, ""]
        changed = True
        print(cam + ": " + want + " written")
    if changed:
        with open(path, "w") as f:
            f.write("\n".join(lines))
    print("config.txt changed: REBOOT required before the cameras stream" if changed else "config.txt unchanged: the camera lines were already as declared")
PY
  chmod 0755 "$1"
}

# TLS for the fake server: a test CA the container trusts + one server cert
# naming every fake host; /etc/hosts points them at 127.0.0.1.
start_fake_server() {
  local hosts=() h san="" i=0
  hosts=("$(golden_host API_BASE)" "$(golden_host DIST_BUCKET_URL)" dist.e2e.invalid api.e2e.invalid)
  [ -n "${hosts[0]}" ] && [ -n "${hosts[1]}" ] || { echo "FAIL: cannot read the golden's hostnames"; exit 2; }
  mkdir -p "$E/tls"
  printf '%s %s\n' "${hosts[1]}" dist.e2e.invalid > "$E/dist-hosts"
  printf '%s %s\n' "${hosts[0]}" api.e2e.invalid > "$E/api-hosts"
  for h in "${hosts[@]}"; do i=$((i + 1)); san="${san:+$san,}DNS:$h"; done
  ( cd "$E/tls" && openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=e2e-ca -keyout ca.key -out ca.pem >/dev/null 2>&1 \
    && openssl req -newkey rsa:2048 -nodes -subj /CN=e2e -keyout srv.key -out srv.csr >/dev/null 2>&1 \
    && printf 'subjectAltName=%s\n' "$san" > ext && openssl x509 -req -in srv.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 -extfile ext -out srv.pem >/dev/null 2>&1 ) \
    || { echo "FAIL: openssl"; exit 2; }
  cp "$E/tls/ca.pem" /usr/local/share/ca-certificates/e2e-test-ca.crt && update-ca-certificates >/dev/null 2>&1
  printf 'E2E ROOT CA (stand-in for AmazonRootCA1)\n' > "$E/root-ca.pem"
  for h in "${hosts[@]}"; do echo "127.0.0.1 $h" >> /etc/hosts; done
  for h in "${hosts[@]}"; do
    [ "$(getent hosts "$h" | awk '{print $1}' | head -n1)" = 127.0.0.1 ] || { echo "FAIL: $h does not resolve to 127.0.0.1: refusing to run (it would reach the real service)"; exit 2; }
  done
  systemd-run --quiet --unit=e2e-fake-api /usr/bin/python3 -I "$HERE/fake-dist-api.py" "$E" "$BUCKET" "$E/tls/srv.pem" "$E/tls/srv.key" 443
  for _ in $(seq 50); do [ -f "$E/server.ready" ] && curl -s -o /dev/null "https://${hosts[1]}/" && break; sleep 0.2; done
  [ -f "$E/server.ready" ] || { echo "FAIL: the fake server did not start"; journalctl -u e2e-fake-api --no-pager | tail -5; exit 2; }
}

# The test tarballs: scripts/pack.sh for real (so the member list is the
# shipped one), then the binary, the modem daemon and the system/ helpers
# replaced by inert stubs, re-packed root-owned. Bucket: the versioned tarball,
# the -latest alias (same bytes; what `xorgate-rs update` fetches), both
# sidecars, latest-rs.json. A pristine copy is kept for reset_bucket.
build_bucket() {
  local R=$W/repo T=$W/tree f
  rm -rf "$R" "$T" "$BUCKET" && mkdir -p "$R/crates/xorgate-agent" "$T" "$BUCKET" "$W/pack"
  cp -R "$SRC/scripts" "$SRC/systemd" "$SRC/media" "$SRC/modem" "$SRC/system" "$SRC/python" "$R/"
  cp "$SRC/Cargo.toml" "$R/"; cp "$SRC/crates/xorgate-agent/Cargo.toml" "$R/crates/xorgate-agent/"
  write_stub_bin "$W/stub-bin"
  bash "$R/scripts/pack.sh" --channel dev --version "$VER" --bin "$W/stub-bin" --out-dir "$W/pack" > "$W/pack.out" 2> "$W/pack.err" \
    || { echo "FAIL: pack.sh"; cat "$W/pack.err"; exit 2; }
  tar -tzf "$W/pack/$TGZ" > "$W/pack.list"
  tar -xzf "$W/pack/$TGZ" -C "$T"
  printf '#!/bin/bash\n# e2e stub modem daemon (never the real one)\necho "start $$" >> /var/lib/e2e/modem.log\nwhile :; do sleep 30; done\n' > "$T/modem/sim7600-lte.sh"
  for f in "$T"/system/*; do [ -x "$f" ] && printf '#!/bin/sh\n# e2e stub helper: never run\nexit 0\n' > "$f"; done
  (cd "$T" && tar --owner=0 --group=0 --numeric-owner -czf "$BUCKET/$TGZ" -- *)
  cp "$BUCKET/$TGZ" "$BUCKET/xorgate-agent-rs-latest.tar.gz"
  (cd "$BUCKET" && sha256sum "$TGZ" > "$TGZ.sha256" && sha256sum xorgate-agent-rs-latest.tar.gz > xorgate-agent-rs-latest.tar.gz.sha256)
  printf '{ "version": "%s", "channel": "dev", "builtAt": "2026-11-15T00:00:00Z",\n  "packageVersion": "0.0.1", "tarball": "%s" }\n' "$VER" "$TGZ" > "$BUCKET/latest-rs.json"
  rm -rf "$W/bucket.base" && cp -a "$BUCKET" "$W/bucket.base"
}
reset_bucket() { rm -rf "$BUCKET" && cp -a "$W/bucket.base" "$BUCKET"; }
# Bucket variants (the bootstrap's failure modes).
bucket_bad_sha() { printf 'x' >> "$BUCKET/$TGZ"; }
bucket_no_member() { # a build without provision-rs.sh, its sidecar consistent
  local T=$W/tree2; rm -rf "$T" && mkdir -p "$T" && tar -xzf "$BUCKET/$TGZ" -C "$T" && rm -f "$T/provision-rs.sh"
  (cd "$T" && tar --owner=0 --group=0 -czf "$BUCKET/$TGZ" -- *) && (cd "$BUCKET" && sha256sum "$TGZ" > "$TGZ.sha256")
}
bucket_manifest() { printf '{ "version": "x", "tarball": "%s" }\n' "$1" > "$BUCKET/latest-rs.json"; }

e2e_setup() {
  e2e_guard
  mkdir -p "$E" "$W"
  write_shims
  id pi >/dev/null 2>&1 || useradd -m -s /bin/bash pi
  groupadd -f i2c; groupadd -f dialout
  mkdir -p /etc/NetworkManager/conf.d /boot/firmware
  # A NetworkManager stand-in, so the reloads the installers do are counted.
  printf '[Service]\nType=simple\nExecStart=/bin/sleep infinity\nExecReload=%s/nm-reload\n' "$SHIM" > /etc/systemd/system/NetworkManager.service
  systemctl daemon-reload && systemctl start NetworkManager.service
  cp /etc/group "$W/group.base"; cp /etc/gshadow "$W/gshadow.base"
  start_fake_server
  build_bucket
  : > "$E/agent.log"; chmod 0666 "$E/agent.log"
  echo "  setup: $(systemctl --version | head -1); golden $(sha256sum "$BOOT" | cut -c1-12); tarball $(sha256sum "$BUCKET/$TGZ" | cut -c1-12)"
}

# Back to a fresh device between cases (the container is the device).
reset_device() {
  local u
  for u in e2e-install xorgate-provision-rs xorgate-agent-rs xorgate-agent xorgate-updater-rs sim7600-lte; do systemctl stop "$u" >/dev/null 2>&1; systemctl disable "$u" >/dev/null 2>&1; done
  rm -rf /etc/xorgate /opt/xorgate /opt/xorgate-rs /opt/xorgate-rs.new /opt/xorgate-rs.old /var/lib/xorgate /var/lib/xorgate-rs-install \
    /usr/local/bin/xorgate /usr/local/bin/xorgate-* /usr/local/bin/sim7600-lte.sh \
    /etc/systemd/system/xorgate-* /etc/systemd/system/sim7600-lte.service /etc/systemd/system/multi-user.target.wants/xorgate-* \
    /etc/systemd/system/multi-user.target.wants/sim7600-lte.service /etc/sudoers.d/02*_xorgate-* /etc/cron.d/xorgate-* \
    /etc/udev/rules.d/99-sim7600-lte.rules /etc/systemd/system-generators/xorgate-* /etc/NetworkManager/conf.d/* /etc/systemd/logind.conf.d \
    /etc/apt/apt.conf.d/99xorgate-* /etc/apt/apt.conf.d/00p8-* /run/xorgate-provision-rs.lock /tmp/tmp.* /tmp/provision-rs.* \
    /var/log/xorgate-provision-rs.log /run/xorgate-provision-rs.rc* "$E/ca-delay"
  printf 'dtparam=i2c_arm=on\n' > /boot/firmware/config.txt; rm -f /boot/firmware/config.txt.bak-xorgate
  cp "$W/group.base" /etc/group; cp "$W/gshadow.base" /etc/gshadow
  rm -rf "$E/apt/installed" && mkdir -p "$E/apt/installed"
  rm -f "$E/apt-fail" "$E/pip-fail" "$E/agent-mode" "$E/claim-mode" "$E/used-codes" "$E/ready-delay" "$E/nmcli.out"
  rm -rf "$E/net"
  : > "$E/claims.log"; : > "$E/requests.log"; : > "$E/apt.log"; : > "$E/calls.log"; : > "$E/nm-reloads"; : > "$E/agent.log"; : > "$E/modem.log"
  reset_bucket
  systemctl daemon-reload; systemctl reset-failed >/dev/null 2>&1
}

# Runs (output in $OUT, status in $RC). The bootstrap runs as `sudo bash
# install.sh` would: root, SUDO_USER=pi, the shims first on PATH.
P=$SHIM:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
run_boot() { # [bootstrap args…]
  (cd /home/pi && env PATH="$P" SUDO_USER=pi bash "$BOOT" "$@") > "$OUT" 2>&1 < /dev/null; RC=$?
  echo "    bootstrap $* -> exit $RC: $(tail -n1 "$OUT")"
}
run_boot_root() { (cd /root && env -u SUDO_USER PATH="$P" bash "$BOOT" "$@") > "$OUT" 2>&1 < /dev/null; RC=$?; echo "    bootstrap (root, no SUDO_USER) -> exit $RC: $(tail -n1 "$OUT")"; }
# The provisioner directly, through I-P8-1, on the test hostnames.
run_prov() { # <provision-rs.sh> [extra args…]
  local p=$1 d; shift
  d=$(mktemp -d); cp "$BUCKET/$TGZ" "$BUCKET/$TGZ.sha256" "$d/"
  env PATH="$P" SUDO_USER=pi bash "$p" --reg-code "${CODE:-e2ecode1}" --api-base "$API_TEST" --from "$DIST_TEST" \
    --tarball "$d/$TGZ" --cleanup "$d" "$@" > "$OUT" 2>&1 < /dev/null; RC=$?
  echo "    provision-rs.sh $* -> exit $RC: $(tail -n1 "$OUT")"
}
claims() { wc -l < "$E/claims.log" | tr -d ' '; }
# The bootstrap as a background "ssh session" whose SIGINT is NOT ignored
# (bash starts async children with SIGINT ignored, and an ignored signal
# cannot be trapped): python resets it to the default, then execs bash.
boot_bg() { # <out file>: sets BG_PID
  env -C /home/pi PATH="$P" SUDO_USER=pi python3 -I "$HERE/sigdfl.py" bash "$BOOT" > "$1" 2>&1 < /dev/null &
  BG_PID=$!   # env -> python -> bash -> provision-rs.sh: one pid (each execs)
}
wait_log() { # <ERE> <secs>: the provisioning unit's log shows it
  local i; for i in $(seq "$2"); do grep -qE "$1" /var/log/xorgate-provision-rs.log 2>/dev/null && return 0; sleep 1; done; return 1
}
wait_rc() { # <secs>: the provisioning unit's exit code (its last log line) -> $UNIT_RC
  local i; UNIT_RC=""
  for i in $(seq "$1"); do
    UNIT_RC=$(sed -n 's/^\[xorgate\] (provisioning unit) exit code //p' /var/log/xorgate-provision-rs.log 2>/dev/null | tail -n1)
    [ -z "$UNIT_RC" ] || return 0; sleep 1
  done; return 1
}
show_out() { sed 's/^/    > /' "$OUT"; }
expect_rc() { # <want> <label>
  if [ "$RC" = "$1" ]; then ok "$2 (exit $RC)"; else bad "$2: exit $RC, want $1"; show_out; fi
}
# The provisioner's surface: one line per path (type mode owner sha inode), for
# "nothing changed" / "never rewritten" comparisons.
SURFACE=(/etc/xorgate /opt /usr/local/bin /etc/systemd/system /etc/systemd/system-generators /etc/sudoers.d /etc/cron.d /etc/udev/rules.d
  /etc/NetworkManager/conf.d /etc/systemd/logind.conf.d /etc/apt/apt.conf.d /boot/firmware /var/lib/xorgate)
snap() { # [--no-inode] -> stdout
  local r
  for r in "${SURFACE[@]}"; do
    [ -e "$r" ] || continue
    find "$r" -printf '%y %m %u:%g %i %p\n' | while read -r y m o i p; do
      s=-; [ "$y" = f ] && s=$(sha256sum "$p" | cut -c1-16)
      [ "$y" = l ] && s=$(readlink "$p")
      if [ "${1:-}" = --no-inode ]; then i=-; fi
      echo "$y $m $o $s $i $p"
    done
  done | sort -k6
}
row() { # <path> <type f|d|l> <mode> <owner:group>
  local got; got=$(stat -c '%F %a %U:%G' "$1" 2>/dev/null) || { bad "missing $1"; return; }
  local want; case $2 in f) want="regular file";; d) want=directory;; l) want="symbolic link";; esac
  [ "$got" = "$want $3 $4" ] || [ "$got" = "regular empty file $3 $4" ] && ok "$1 $2 $3 $4" || bad "$1: $got (want $want $3 $4)"
}
absent() { [ ! -e "$1" ] && [ ! -L "$1" ] && ok "absent: $1" || bad "present: $1"; }
active_wait() { # <unit> <want> <secs>
  local i; for i in $(seq "$3"); do [ "$(systemctl is-active "$1")" = "$2" ] && return 0; sleep 1; done; return 1
}
