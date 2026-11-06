#!/bin/sh
# Self-test of rig_statvfs.c (Phase 5b WP5 item 4), in the rig image:
#   docker run --rm -v "$PWD/tools/replay/statvfs-shim:/c:ro" xorgate-replay-rig:node20-bookworm-gst sh /c/test.sh
# Builds the shim and checks: `df -kP /` (the TS reader) and statvfs("/") (the
# Rust reader, via python's os.statvfs = libc statvfs) both report the fixture;
# a path on the root fs (the recordings root) reports it to both too; a path on
# another fs (/dev/shm) and every call without the variable are REAL; growing
# the real disk mid-run moves the real numbers but never the fixture.
set -u
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
cc -shared -fPIC -O2 -Wall -Wextra -o /tmp/rig-statvfs.so /c/rig_statvfs.c -ldl || { echo "FAIL: build"; exit 1; }
FX=7500000:6060000:5685000:4096
P="env LD_PRELOAD=/tmp/rig-statvfs.so RIG_STATVFS_ROOT=$FX"
dfkp() { $1 df -kP "$2" | awk 'NR==2 {print $2, $3}'; }
pyv() { $1 python3 -c 'import os,sys; s=os.statvfs(sys.argv[1]); print(s.f_blocks, s.f_bfree, s.f_frsize)' "$2"; }
mkdir -p /tmp/rec
[ "$(dfkp "$P" /)" = "30000000 5760000" ] && ok "df -kP / = the fixture (19.2 %)" || bad "df -kP /: $(dfkp "$P" /)"
[ "$(dfkp "$P" /tmp/rec)" = "30000000 5760000" ] && ok "df -kP <path on /> = the fixture" || bad "df on /tmp/rec"
[ "$(pyv "$P" /)" = "7500000 6060000 4096" ] && ok "statvfs(\"/\") = the fixture" || bad "statvfs /: $(pyv "$P" /)"
[ "$(pyv "$P" /tmp/rec)" = "7500000 6060000 4096" ] && ok "statvfs(<path on />) = the fixture" || bad "statvfs /tmp/rec"
[ "$(pyv "$P" /dev/shm)" = "$(pyv env /dev/shm)" ] && [ "$(pyv env /dev/shm)" != "7500000 6060000 4096" ] && ok "another fs (/dev/shm) is real" || bad "/dev/shm"
real1=$(dfkp env /)
total() { echo "$1" | cut -d' ' -f1; }   # the real fs keeps moving (Used); its size does not
[ "$(total "$(dfkp "env LD_PRELOAD=/tmp/rig-statvfs.so" /)")" = "$(total "$real1")" ] && [ "$(total "$real1")" != 30000000 ] && ok "no RIG_STATVFS_ROOT = real" || bad "unset var"
[ "$(total "$(dfkp "env LD_PRELOAD=/tmp/rig-statvfs.so RIG_STATVFS_ROOT=1:2:3" /)")" = "$(total "$real1")" ] && ok "malformed = real" || bad "malformed"
# The host disk changes mid-run: the real numbers move, the fixture does not.
dd if=/dev/zero of=/tmp/rig-grow bs=1M count=512 status=none; sync
real2=$(dfkp env /)
[ "$real1" != "$real2" ] && ok "real df moved after writing 512 MiB ($real1 -> $real2)" || bad "real df did not move"
[ "$(dfkp "$P" /)" = "30000000 5760000" ] && [ "$(pyv "$P" /)" = "7500000 6060000 4096" ] && ok "fixture unchanged after the host disk grew" || bad "fixture moved"
rm -f /tmp/rig-grow
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
