#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2119  # test assertions: `a && ok || bad`; run_* take optional args
# Phase 8 WP-R1 DA6: the backend's Rust bootstrap (the vendored golden, run
# UNMODIFIED) -> scripts/provision-rs.sh from a scripts/pack.sh tarball, on a
# fresh bookworm "device" under a REAL systemd 252, against a fake dist bucket
# + claim API that records every claim (tools/provision-e2e: lib.sh says what
# is real and what is stubbed). Cases: happy, re-run refused, --reprovision,
# bad sha / missing member / bad manifest / 404 (no claim), NEEDED-lib apt
# failure + a binary that will not run (no claim), plugin + pip failures
# (WARN only), claim 409 / 404 (exit 5), a failed start (exit 4), a
# JS-provisioned host (exit 3, also with --reprovision), a root service user,
# a bench-shaped device (identical files never rewritten, NM not reloaded, an
# active modem daemon never restarted), the JS installer over a native device
# (JS alone from the next boot) + --uninstall, usage errors.
# Run (systemd must be PID 1; ~6 min), as hl.sh `systemd` does:
#   docker run -d --name xg-pe2e --label xorgate.agent=<tag> --privileged --cgroupns=private \
#     --tmpfs /run --tmpfs /run/lock -v "$PWD:/src:ro" debian:bookworm \
#     bash -c 'apt-get update -qq && apt-get install -y -qq systemd python3 sudo jq curl openssl ca-certificates >/dev/null && exec /lib/systemd/systemd'
#   docker exec xg-pe2e bash /src/scripts/tests/provision-e2e.sh
#   docker rm -f xg-pe2e
set -uo pipefail
# shellcheck source=tools/provision-e2e/lib.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/tools/provision-e2e/lib.sh"
for _ in $(seq 60); do s=$(systemctl is-system-running 2>/dev/null); case $s in running | degraded) break ;; esac; sleep 1; done
e2e_setup
grep -qx provision-rs.sh "$W/pack.list" && ok "pack.sh ships provision-rs.sh at the tarball root" || bad "pack.sh: no provision-rs.sh member"
[ "$(stat -c %a "$W/tree/provision-rs.sh")" = 755 ] && cmp -s "$W/tree/provision-rs.sh" "$PROV_SRC" && ok "…0755, byte-equal to scripts/provision-rs.sh" || bad "packed provision-rs.sh differs"

nothing_claimed() { # <label>
  [ "$(claims)" = 0 ] && ok "$1: no claim reached the API" || bad "$1: $(claims) claim(s)"
  for p in /opt/xorgate-rs /opt/xorgate-rs.new /etc/xorgate "$RS_UNIT_PATH"; do [ ! -e "$p" ] || bad "$1: $p exists"; done
  ls -d /tmp/tmp.* >/dev/null 2>&1 && bad "$1: the bootstrap temp dir is left behind" || ok "$1: temp dir removed"
}
RS_UNIT_PATH=/etc/systemd/system/xorgate-agent-rs.service

echo "== happy: fresh device, the bootstrap golden, Argus CM5 Rev1 (hardware block)"
reset_device
run_boot
expect_rc 0 "happy"
[ "$(claims)" = 1 ] && [ "$(jq -r .body.agent "$E/claims.log")" = rust ] && [ "$(jq -r .body.platform "$E/claims.log")" = aarch64 ] \
  && ok "one claim, body.agent=rust (+ the JS fields)" || bad "claims: $(cat "$E/claims.log")"
[ "$(jq -r '.body | keys | join(",")' "$E/claims.log")" = agent,cameras,dtModel,hostname,platform,serial ] && ok "claim body keys = JS's + agent" || bad "claim body keys"
first_claim=$(grep -n 'POST .*claim' "$E/requests.log" | head -1 | cut -d: -f1)
grep -q 'claiming registration code' "$OUT" && [ "$(grep -n 'staged xorgate-rs' "$OUT" | cut -d: -f1)" -lt "$(grep -n 'claiming registration code' "$OUT" | cut -d: -f1)" ] \
  && ok "the smoke-exec passed BEFORE the claim" || bad "order: smoke vs claim"
[ -n "$first_claim" ] && ok "claim seen by the fake API" || bad "no claim request"
grep -E 'apt-get install .*libgstreamer1.0-0' "$E/apt.log" >/dev/null && ok "NEEDED providers installed (hard call)" || bad "no NEEDED apt call"
grep -q 'E2E-KEY' "$OUT" && bad "the private key reached the output" || ok "the private key never printed"
for r in "/etc/xorgate d 700 pi:pi" "/etc/xorgate/agent.env f 600 pi:pi" "/etc/xorgate/device.cert.pem f 600 pi:pi" \
  "/etc/xorgate/device.key.pem f 600 pi:pi" "/etc/xorgate/root-ca.pem f 600 pi:pi" "/etc/xorgate/kvs.json f 600 pi:pi" \
  "/etc/xorgate/hardware.json f 600 pi:pi" "/etc/xorgate/pipelines d 755 pi:pi" "/opt/xorgate-rs d 755 root:root" \
  "/opt/xorgate-rs/bin/xorgate-rs f 755 root:root" "/opt/xorgate-rs/provision-rs.sh f 755 root:root" "/usr/local/bin/xorgate-rs f 755 root:root" \
  "$RS_UNIT_PATH f 644 root:root" "$RS_UNIT_PATH.d d 755 root:root" "$RS_UNIT_PATH.d/10-user.conf f 644 root:root" \
  "$RS_UNIT_PATH.d/20-native.conf f 644 root:root" "/etc/systemd/system/xorgate-updater-rs.service f 644 root:root" \
  "/etc/systemd/system/xorgate-updater-rs.service.d/10-dist.conf f 644 root:root" "/etc/sudoers.d/020_xorgate-power f 440 root:root" \
  "/etc/sudoers.d/021_xorgate-clock f 440 root:root" "/etc/sudoers.d/023_xorgate-camrotate f 440 root:root" \
  "/etc/sudoers.d/024_xorgate-update-rs f 440 root:root" "/usr/local/bin/xorgate-camrotate f 755 root:root" \
  "/usr/local/bin/sim7600-lte.sh f 755 root:root" "/etc/systemd/system/sim7600-lte.service f 644 root:root" \
  "/etc/udev/rules.d/99-sim7600-lte.rules f 644 root:root" "/etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf f 644 root:root" \
  "/etc/NetworkManager/conf.d/10-uplink-priority.conf f 644 root:root" "/etc/systemd/logind.conf.d/10-xorgate-removeipc.conf f 644 root:root" \
  "/etc/apt/apt.conf.d/99xorgate-lock-timeout f 644 root:root" "/etc/systemd/system-generators/xorgate-agent-rs-native f 755 root:root"; do
  # shellcheck disable=SC2086  # a row is four words
  row $r
done
for f in /usr/local/bin/xorgate-s*clock /etc/cron.d/xorgate-*; do row "$f" f "$([ -x "$f" ] && echo 755 || echo 644)" root:root; done
for p in /opt/xorgate /usr/local/bin/xorgate /etc/sudoers.d/022_xorgate-update /etc/systemd/system/xorgate-agent.service \
  /etc/systemd/system/xorgate-updater.service /opt/xorgate-rs.new /opt/xorgate-rs.old /etc/systemd/system/multi-user.target.wants/xorgate-agent-rs.service; do absent "$p"; done
[ -L /run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service ] && ok "wanted at boot through the generator (no /etc wants link)" || bad "no generated wants link"
command -v node >/dev/null && bad "node installed" || ok "no node"
grep -qx DEVICE_DIST_VERSION=latest /etc/xorgate/agent.env && ! grep -q "$VER" /etc/xorgate/agent.env \
  && ok "agent.env: DEVICE_DIST_VERSION=latest literally (the build is $VER, not pinned)" || bad "agent.env DEVICE_DIST_VERSION: $(grep DEVICE_DIST /etc/xorgate/agent.env)"
grep -qx "DEVICE_DIST_BUCKET_URL=https://$(golden_host DIST_BUCKET_URL)" /etc/xorgate/agent.env && grep -qx KVS_RUNTIME_BIN=/opt/xorgate-rs/media/xorgate-kvs-webrtc /etc/xorgate/agent.env \
  && grep -qx KVS_RUNTIME_LIB_DIR=/opt/xorgate-rs/media/lib /etc/xorgate/agent.env && ok "agent.env: bucket + KVS runtime under /opt/xorgate-rs/media" || bad "agent.env paths"
[ "$(jq -r .deviceId /etc/xorgate/kvs.json)" = dev-abcdefghijkmnpq2 ] && [ "$(jq -r .platform /etc/xorgate/hardware.json)" = rpi-cm5 ] \
  && ok "kvs.json + hardware.json from the claim" || bad "kvs/hardware json"
grep -qx 'dtoverlay=imx290,cam0' /boot/firmware/config.txt && grep -qx 'dtoverlay=imx290,cam1' /boot/firmware/config.txt && grep -qx camera_auto_detect=0 /boot/firmware/config.txt \
  && grep -q 'REBOOT required' "$OUT" && ok "overlays added + the REBOOT notice" || bad "overlays"
case "$(systemctl is-active xorgate-agent-rs)/$(systemctl is-enabled xorgate-agent-rs)" in active/generated | active/enabled-runtime) true ;; *) false ;; esac && ok "xorgate-agent-rs active + wanted by the generator ($(systemctl is-enabled xorgate-agent-rs))" || bad "agent $(systemctl is-active xorgate-agent-rs)"
grep -q "rs start .*uid=$(id -u pi) device=dev-abcdefghijkmnpq2 dist=latest kvs=/opt/xorgate-rs/media/xorgate-kvs-webrtc" "$E/agent.log" && ok "the agent runs as pi with the new identity" || bad "agent.log: $(tail -2 "$E/agent.log")"
[ "$(systemctl is-active sim7600-lte)/$(systemctl is-enabled sim7600-lte)" = active/enabled ] && ok "sim7600-lte enabled + started (the stub daemon)" || bad "sim7600-lte"
[ "$(wc -l < "$E/nm-reloads")" = 3 ] && ok "NM reloaded 3x on a fresh device (bootstrap 0a, step 7 full conf, step 7b uplink; as JS)" || bad "NM reloads: $(wc -l < "$E/nm-reloads")"
cmp -s /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf "$W/tree/modem/99-sim7600-unmanaged.conf" && ok "the bootstrap's minimal NM conf replaced by the tarball's" || bad "NM conf not the tarball copy"
visudo -c >/dev/null 2>&1 && ok "visudo -c" || bad "visudo -c"
in=" $(id -nG pi) "; case $in in *" i2c "*) case $in in *" dialout "*) ok "pi in i2c + dialout" ;; *) bad "pi groups:$in" ;; esac ;; *) bad "pi groups:$in" ;; esac
grep -q '^modprobe i2c-dev' "$E/calls.log" && ok "modprobe went to the shim (no raspi-config)" || bad "modprobe not called"
HAPPY_SNAP=$(snap)

echo "== re-run without --reprovision: refused (3), nothing changed, no claim"
: > "$E/claims.log"; rm -f "$E/used-codes"
run_boot
expect_rc 3 "re-run"
[ "$(claims)" = 0 ] && ok "re-run: no claim" || bad "re-run claimed"
[ "$(snap)" = "$HAPPY_SNAP" ] && ok "re-run: the surface is unchanged (bytes, modes, owners, inodes)" || { bad "re-run changed files"; diff <(echo "$HAPPY_SNAP") <(snap) | head; }
grep -q -- '--reprovision' "$OUT" && ok "re-run: the refusal names --reprovision" || bad "re-run message"

echo "== --reprovision: new identity, build swapped, agent restarted, config-sync.json deleted, a stale .new swept"
PID1=$(systemctl show -p MainPID --value xorgate-agent-rs)
SHARED_BEFORE=$(snap | grep -E ' /(usr/local/bin/(sim7600|xorgate-cam)|etc/(sudoers.d|udev|NetworkManager|systemd/logind|apt))')
echo '{"rev":81}' > /var/lib/xorgate/config-sync.json; mkdir -p /opt/xorgate-rs.new/junk
: > "$E/claims.log"; rm -f "$E/used-codes"; NMR=$(wc -l < "$E/nm-reloads")
# A new code: the golden carries one fixed code, so this run uses the
# provisioner's I-P8-1 interface directly with a second code.
CODE=e2ecode2 run_prov /opt/xorgate-rs/provision-rs.sh --reprovision
expect_rc 0 "--reprovision"
[ "$(claims)" = 1 ] && [ "$(jq -r .code "$E/claims.log")" = e2ecode2 ] && ok "reprovision: one claim with the new code" || bad "reprovision claims"
grep -qx DEVICE_ID=dev-e2ecode2 /etc/xorgate/agent.env && ok "reprovision: agent.env has the new identity" || bad "reprovision agent.env"
absent /var/lib/xorgate/config-sync.json; absent /opt/xorgate-rs.new; absent /opt/xorgate-rs.old
PID2=$(systemctl show -p MainPID --value xorgate-agent-rs)
[ "$(systemctl is-active xorgate-agent-rs)" = active ] && [ "$PID2" != "$PID1" ] && grep -q 'rs start .*device=dev-e2ecode2' "$E/agent.log" \
  && ok "reprovision: the agent was restarted on the new identity" || bad "reprovision restart ($PID1 -> $PID2)"
[ "$(snap | grep -E ' /(usr/local/bin/(sim7600|xorgate-cam)|etc/(sudoers.d|udev|NetworkManager|systemd/logind|apt))')" = "$SHARED_BEFORE" ] \
  && ok "reprovision: identical shared files never rewritten (same inodes)" || bad "reprovision rewrote shared files"
[ "$(wc -l < "$E/nm-reloads")" = "$NMR" ] && ok "reprovision: NM not reloaded" || bad "reprovision reloaded NM"

echo "== the bootstrap's pre-claim failures: no claim, nothing installed"
reset_device; bucket_bad_sha; run_boot; expect_rc 1 "bad sha"; nothing_claimed "bad sha"
grep -q 'checksum mismatch' "$OUT" && ok "bad sha: named" || bad "bad sha message"
reset_device; bucket_no_member; run_boot; expect_rc 1 "missing provision-rs.sh member"; nothing_claimed "missing member"
reset_device; bucket_manifest '../evil.tar.gz'; run_boot; expect_rc 1 "invalid latest-rs.json"; nothing_claimed "invalid manifest"
reset_device; bucket_manifest 'xorgate-agent-rs-nosuch.tar.gz'; run_boot; expect_rc 1 "404 tarball"; nothing_claimed "404 tarball"
reset_device; run_boot --bogus; expect_rc 2 "bootstrap: unknown argument"; nothing_claimed "bootstrap usage"

echo "== provision-rs.sh's own pre-claim failures"
reset_device; echo libgstreamer1.0-0 > "$E/apt-fail"; run_boot; expect_rc 1 "NEEDED-lib apt failure"; nothing_claimed "NEEDED apt failure"
grep -q 'nothing was claimed' "$OUT" && ok "NEEDED apt failure: says the code is still usable" || bad "NEEDED apt message"
reset_device; echo lie:libgstreamer1.0-0 > "$E/apt-fail"; run_boot; expect_rc 1 "GStreamer absent: the staged binary does not run"; nothing_claimed "smoke failure"
grep -q 'libgstreamer-1.0.so.0' "$OUT" && ok "smoke failure: the loader error is shown" || bad "smoke message"
reset_device; echo update > "$E/apt-fail"; run_boot; expect_rc 1 "apt-get update failure"; nothing_claimed "apt update failure"
reset_device; d=$(mktemp -d); cp "$BUCKET/$TGZ" "$d/"; printf '%064d  %s\n' 0 "$TGZ" > "$d/$TGZ.sha256"; tar -xzf "$d/$TGZ" -C "$d" provision-rs.sh
env PATH="$P" SUDO_USER=pi bash "$d/provision-rs.sh" --reg-code c --api-base "$API_TEST" --from "$DIST_TEST" --tarball "$d/$TGZ" > "$OUT" 2>&1; RC=$?
rm -rf "$d"; expect_rc 1 "provision-rs.sh re-checks the sidecar"; nothing_claimed "provisioner sha"

echo "== WARN-only steps still provision (as JS)"
reset_device; echo gstreamer1.0-plugins-bad > "$E/apt-fail"; touch "$E/pip-fail"; run_boot; expect_rc 0 "plugin + pip failures"
grep -q 'WARN: GStreamer runtime install failed' "$OUT" && grep -q 'WARN: adafruit python libs failed' "$OUT" && ok "both WARNed" || bad "WARN lines"
[ "$(claims)" = 1 ] && ok "…and claimed once" || bad "claims $(claims)"

echo "== claim refused: exit 5, only the dependencies installed"
for m in 409 404; do
  reset_device; echo "$m" > "$E/claim-mode"; run_boot; expect_rc 5 "claim $m"
  [ "$(claims)" = 1 ] && ok "claim $m: one claim attempt" || bad "claim $m: $(claims)"
  for p in /opt/xorgate-rs /opt/xorgate-rs.new /etc/xorgate "$RS_UNIT_PATH"; do [ ! -e "$p" ] || bad "claim $m: $p exists"; done
  [ -e "$E/apt/installed/libgstreamer1.0-0" ] && ok "claim $m: dependencies installed, nothing else" || bad "claim $m: deps"
done

echo "== claimed, but the agent never becomes active: exit 4 + recovery (~90 s)"
reset_device; echo fail > "$E/agent-mode"; run_boot; expect_rc 4 "failed start"
grep -q 'RECOVERY: the registration code is USED' "$OUT" && grep -q 'install.sh --reprovision' "$OUT" && ok "failed start: recovery printed" || bad "failed start: no recovery"

echo "== a JS-provisioned device is refused (3), also with --reprovision"
for a in "" --reprovision; do
  reset_device; mkdir -p /opt/xorgate/dist; printf '[Service]\nExecStart=/bin/true\n' > /etc/systemd/system/xorgate-agent.service
  install -m 0644 "$W/tree/modem/99-sim7600-unmanaged.conf" /etc/NetworkManager/conf.d/   # a JS device has it: the bootstrap skips 0a
  B=$(snap); run_boot $a; expect_rc 3 "JS device ${a:-(plain)}"
  [ "$(claims)" = 0 ] && [ "$(snap)" = "$B" ] && [ ! -s "$E/apt.log" ] && ok "JS device ${a:-(plain)}: nothing changed, nothing installed, no claim" || bad "JS device ${a:-(plain)} changed something"
  grep -q 'install-rs.sh' "$OUT" && ok "JS device: points at install-rs.sh" || bad "JS device message"
done

echo "== root service user (no SUDO_USER)"
reset_device; run_boot_root; expect_rc 0 "root service user"
grep -qx 'User=root' "$RS_UNIT_PATH.d/10-user.conf" && absent /etc/sudoers.d/024_xorgate-update-rs && grep -q '^root ALL=(root) NOPASSWD: /usr/bin/systemctl reboot' /etc/sudoers.d/020_xorgate-power \
  && ok "root: User=root, 020 for root (as JS), no 024 (update_plan::grant_user)" || bad "root svc surface"
[ "$(stat -c %U /etc/xorgate/agent.env)" = root ] && ok "root: /etc/xorgate owned by root" || bad "root: owner"

echo "== bench-shaped device: shared files already installed and identical, the modem daemon running"
reset_device
mkdir -p /etc/udev/rules.d
install -m 0755 "$W/tree/modem/sim7600-lte.sh" /usr/local/bin/sim7600-lte.sh
install -m 0644 "$W/tree/modem/sim7600-lte.service" /etc/systemd/system/sim7600-lte.service
install -m 0644 "$W/tree/modem/99-sim7600-lte.rules" /etc/udev/rules.d/99-sim7600-lte.rules
install -m 0644 "$W/tree/modem/99-sim7600-unmanaged.conf" /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf
for f in "$W"/tree/system/xorgate-c*; do install -m 0755 "$f" "/usr/local/bin/$(basename "$f")"; done
mkdir -p /etc/systemd/logind.conf.d
printf 'dtparam=i2c_arm=on\ncamera_auto_detect=0\ndtoverlay=imx290,cam0\ndtoverlay=imx290,cam1\n' > /boot/firmware/config.txt
cp /boot/firmware/config.txt /boot/firmware/config.txt.bak-xorgate
systemctl daemon-reload; systemctl enable --now sim7600-lte >/dev/null 2>&1
MPID=$(systemctl show -p MainPID --value sim7600-lte); MTS=$(systemctl show -p ActiveEnterTimestampMonotonic --value sim7600-lte)
bench_files() { snap | grep '^f ' | grep -E ' /(usr/local/bin/(sim7600|xorgate-c)|etc/udev/|etc/NetworkManager/conf.d/99|boot/)'; }
BENCH=$(bench_files)
CFG_INODE=$(stat -c %i /boot/firmware/config.txt)
run_boot; expect_rc 0 "bench-shaped"
[ "$(bench_files)" = "$BENCH" ] && [ -n "$BENCH" ] \
  && ok "bench: identical modem/system/NM/config.txt files never rewritten (same inodes)" || { bad "bench: files rewritten"; diff <(echo "$BENCH") <(bench_files) | head; }
[ "$(stat -c %i /boot/firmware/config.txt)" = "$CFG_INODE" ] && grep -q 'overlays already present' "$OUT" && ok "bench: config.txt untouched, no REBOOT notice" || bad "bench: config.txt"
[ "$(systemctl show -p MainPID --value sim7600-lte)" = "$MPID" ] && [ "$(systemctl show -p ActiveEnterTimestampMonotonic --value sim7600-lte)" = "$MTS" ] \
  && ok "bench: the active modem daemon was never restarted" || bad "bench: modem daemon restarted"
[ "$(wc -l < "$E/nm-reloads")" = 1 ] && ok "bench: NM reloaded once (only for the new uplink conf; the unmanaged conf skipped by the bootstrap and step 7)" || bad "bench: NM reloads $(wc -l < "$E/nm-reloads")"
! grep -q 'udevadm' "$E/calls.log" && ok "bench: udev rules unchanged, no udevadm reload" || bad "bench: udevadm called"

echo "== --uninstall refusals + usage"
reset_device; run_boot; expect_rc 0 "native device (setup)"
B=$(snap --no-inode)
env PATH="$P" bash /opt/xorgate-rs/provision-rs.sh --uninstall > "$OUT" 2>&1; RC=$?; expect_rc 3 "--uninstall with no JS agent installed"
env PATH="$P" bash /opt/xorgate-rs/provision-rs.sh --uninstall --reprovision > "$OUT" 2>&1; RC=$?; expect_rc 2 "--uninstall + another argument"
env PATH="$P" bash /opt/xorgate-rs/provision-rs.sh --reg-code c --api-base http://x/v1 --from https://d --tarball /x > "$OUT" 2>&1; RC=$?; expect_rc 2 "http:// api base"
env PATH="$P" bash /opt/xorgate-rs/provision-rs.sh --reg-code c > "$OUT" 2>&1; RC=$?; expect_rc 2 "missing arguments"
[ "$(snap --no-inode)" = "$B" ] && [ "$(systemctl is-active xorgate-agent-rs)" = active ] && ok "refusals and usage errors changed nothing" || bad "a refusal changed something"

echo "== the JS installer over a native device: JS alone from the next boot (20-native.conf), then --uninstall"
# What the JS install.sh does to the unit surface (its step 6): /opt/xorgate,
# its unit + 10-user.conf, enable --now (Conflicts= stops the Rust agent).
mkdir -p /opt/xorgate/dist /etc/systemd/system/xorgate-agent.service.d
printf '[Unit]\nDescription=JS agent stand-in\n[Service]\nType=simple\nExecStart=/bin/sleep infinity\n[Install]\nWantedBy=multi-user.target\n' > /etc/systemd/system/xorgate-agent.service
printf '[Service]\nUser=pi\nGroup=pi\n' > /etc/systemd/system/xorgate-agent.service.d/10-user.conf
systemctl daemon-reload; systemctl enable --now xorgate-agent >/dev/null 2>&1
active_wait xorgate-agent active 10
[ "$(systemctl is-active xorgate-agent)/$(systemctl is-active xorgate-agent-rs)" = active/inactive ] && ok "JS enable --now stopped Rust (Conflicts=)" || bad "after the JS install: $(systemctl is-active xorgate-agent)/$(systemctl is-active xorgate-agent-rs)"
[ "$(systemctl is-enabled xorgate-agent)" = enabled ] && [ -e /etc/systemd/system-generators/xorgate-agent-rs-native ] && ok "(JS enabled; the Rust generator still installed, as the JS script leaves it)" || bad "JS enablement / generator"
# The next boot: both start jobs in ONE transaction, as at boot.
systemctl stop xorgate-agent; systemctl start multi-user.target; sleep 3
[ "$(systemctl is-active xorgate-agent)/$(systemctl is-active xorgate-agent-rs)" = active/inactive ] && ok "next 'boot': JS active, Rust inactive" \
  || { bad "next 'boot': JS $(systemctl is-active xorgate-agent), Rust $(systemctl is-active xorgate-agent-rs)"; journalctl -n 15 --no-pager | sed 's/^/    | /'; }
systemctl start xorgate-agent-rs 2>/dev/null; sleep 1
[ "$(systemctl show -p ConditionResult --value xorgate-agent-rs)" = no ] && [ "$(systemctl is-active xorgate-agent)" = active ] \
  && ok "a direct Rust start is skipped by ConditionPathExists=!/opt/xorgate, JS keeps running" || bad "Rust condition: $(systemctl show -p ConditionResult --value xorgate-agent-rs), JS $(systemctl is-active xorgate-agent)"
KEEP=$(snap --no-inode | grep -E ' /(etc/xorgate|usr/local/bin/sim7600|etc/sudoers.d/02[0-3]|etc/udev|etc/NetworkManager|etc/systemd/system/xorgate-agent\.service)')
env PATH="$P" bash /opt/xorgate-rs/provision-rs.sh --uninstall > "$OUT" 2>&1; RC=$?; expect_rc 0 "--uninstall"
for p in /opt/xorgate-rs /usr/local/bin/xorgate-rs "$RS_UNIT_PATH" "$RS_UNIT_PATH.d" /etc/systemd/system/xorgate-updater-rs.service \
  /etc/systemd/system/xorgate-updater-rs.service.d /etc/sudoers.d/024_xorgate-update-rs /etc/systemd/system/multi-user.target.wants/xorgate-agent-rs.service \
  /etc/systemd/system-generators/xorgate-agent-rs-native /run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service; do absent "$p"; done
[ "$(snap --no-inode | grep -E ' /(etc/xorgate|usr/local/bin/sim7600|etc/sudoers.d/02[0-3]|etc/udev|etc/NetworkManager|etc/systemd/system/xorgate-agent\.service)')" = "$KEEP" ] \
  && ok "--uninstall kept /etc/xorgate, the modem/NM/udev assets, 020-023 and the JS unit, byte for byte" || bad "--uninstall touched a kept path"
[ "$(systemctl is-active xorgate-agent)" = active ] && ok "the JS agent still runs" || bad "JS not active after --uninstall"
ls /tmp/provision-rs.* >/dev/null 2>&1 && bad "--uninstall left its private copy" || ok "--uninstall removed its private copy"
reset_device >/dev/null 2>&1; run_boot >/dev/null; mkdir -p /opt/xorgate /var/lib/xorgate-rs-install; : > /var/lib/xorgate-rs-install/record; cp /etc/systemd/system/xorgate-agent-rs.service /etc/systemd/system/xorgate-agent.service
env PATH="$P" bash /opt/xorgate-rs/provision-rs.sh --uninstall > "$OUT" 2>&1; RC=$?; expect_rc 3 "--uninstall on a CONVERTED device (install-rs.sh record)"

reset_device
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
