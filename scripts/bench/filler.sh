#!/usr/bin/env bash
# D9 disk-quota filler for the Rust agent's recordings (plan
# phase-3-recording-media.md §Bench recipe step 4, check D9). Rust home ONLY.
#
#   scripts/bench/filler.sh seed  [--mib N] [--dry-run]     # N=600
#   scripts/bench/filler.sh clear [--dry-run]
#   scripts/bench/filler.sh status
#
# seed: one session dir /var/lib/xorgate-rs/recordings/video/cam0/<uuidv7>/
#   whose uuidv7 timestamp is 7 days old, holding ceil(N/10) CLOSED filler
#   fragments 00000.mp4 … of 10 MiB each (fallocate; zero bytes, so they must
#   never be uploaded: D9 runs with upload.mode "off" and clears the filler
#   before uploads resume), a sidecar 0000N.json per fragment (the
#   build_sidecar field set of crates/xorgate-core/src/sidecar.rs, finalized
#   true, preset 720p15-1mbps, segmentSeconds 60), a session.json
#   (VideoSessionManifest fields), and the marker .xorgate-filler; everything
#   pi-owned with mtimes 7 days in the past (fragment i at +i x 60 s).
#   Idempotent: an existing filler session is reported, not doubled.
#   Refuses unless the target resolves under /var/lib/xorgate-rs/ and the
#   filesystem keeps >= 1 GiB free after the seed.
# clear: removes ONLY session dirs that contain the marker (exclusive.sh end
#   runs the same logic). Never writes under /var/lib/xorgate.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
VERB="${1:-}"; shift || true
DRY=0; MIB=600
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --mib) MIB="$2"; shift 2 ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
case "$VERB" in seed|clear|status) ;; *) sed -n '2,23p' "$0"; exit 2 ;; esac
case "$MIB" in ''|*[!0-9]*) echo "--mib takes a whole number" >&2; exit 2 ;; esac
[ "$MIB" -ge 1 ] || { echo "--mib must be >= 1" >&2; exit 2; }
log "filler $VERB on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY VERB=$VERB MIB=$MIB"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
STREAM_DIR=$FILLER_ROOT/cam0
case "$VERB" in
status)
  n=0
  while IFS= read -r d; do
    [ -n "$d" ] || continue; n=$((n + 1))
    echo "  filler: $d ($(du -sh -- "$d" | cut -f1), $(find "$d" -maxdepth 1 -name '*.mp4' | wc -l) fragments)"
  done < <(filler_sessions)
  [ "$n" -gt 0 ] || echo "  no filler sessions under $FILLER_ROOT"
  echo "  recordings total: $(du -sb "$RS_HOME/recordings" 2>/dev/null | cut -f1 || echo 0) bytes"
  ;;
clear) filler_clear ;;
seed)
  under_rs_home "$STREAM_DIR" || { echo "REFUSING: $STREAM_DIR resolves to $(realpath -m -- "$STREAM_DIR") (outside /var/lib/xorgate-rs/)"; exit 1; }
  EXISTING=$(filler_sessions | head -n1)
  if [ -n "$EXISTING" ]; then
    echo "  = already seeded: $EXISTING ($(du -sh -- "$EXISTING" | cut -f1)) — filler.sh clear first to reseed"
  else
    FREE_MIB=$(( $(df -Pk "$(realpath -m -- "$RS_HOME")" | awk 'NR==2{print $4}') / 1024 ))
    [ "$FREE_MIB" -ge $((MIB + 1024)) ] || { echo "REFUSING: only $FREE_MIB MiB free on $RS_HOME (need $MIB + 1024)"; exit 1; }
    DEVICE_ID=$(sed -n 's/^DEVICE_ID=//p' "$RS_ENV" 2>/dev/null | head -n1 || true)
    [ -n "$DEVICE_ID" ] || DEVICE_ID=bench-filler
    python3 - "$STREAM_DIR" "$MIB" "$DEVICE_ID" "$DRY" "$FILLER_MARK" <<'PY'
import collections, json, math, os, secrets, sys, time
root, mib, device_id, dry, mark = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4] == "1", sys.argv[5]
FRAG = 10 * 1024 * 1024
SEG_S = 60
n = max(1, math.ceil(mib / 10))
now = time.time()
base_ms = int((now - 7 * 86400) * 1000)
def uuidv7(ms):
    b = bytearray(ms.to_bytes(6, "big") + secrets.token_bytes(10))
    b[6] = (b[6] & 0x0F) | 0x70
    b[8] = (b[8] & 0x3F) | 0x80
    h = b.hex()
    return f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:]}"
sid = uuidv7(base_ms)
d = os.path.join(root, sid)
O = collections.OrderedDict
session = O(v=1, deviceId=device_id, streamKey="cam0", sessionId=sid, bootId="00000000-0000-4000-8000-00000000f111",
            timeSource="ntp", preset="720p15-1mbps", width=1280, height=720, fps=15, bitrateKbps=1000,
            segmentSeconds=SEG_S, anchorTs=base_ms, anchorMono=str(1_000_000_000))
print(f"  {'[dry-run] would seed' if dry else 'seeding'} {d}: {n} x 10 MiB fragments + sidecars + session.json + {mark} (uuidv7 ts {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(base_ms / 1000))})")
if dry:
    sys.exit(0)
os.makedirs(d, mode=0o755)
def put_json(path, obj, mtime):
    with open(path, "w") as f:
        json.dump(obj, f, indent=2)
        f.write("\n")
    os.utime(path, (mtime, mtime))
put_json(os.path.join(d, "session.json"), session, base_ms / 1000)
for seq in range(n):
    start_ms = base_ms + seq * SEG_S * 1000
    mp4 = os.path.join(d, f"{seq:05d}.mp4")
    fd = os.open(mp4, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    try:
        os.posix_fallocate(fd, 0, FRAG)
    except OSError:
        os.ftruncate(fd, 0)
        zero = bytes(1024 * 1024)
        for _ in range(FRAG // len(zero)):
            os.write(fd, zero)
    os.close(fd)
    end_s = (start_ms + SEG_S * 1000) / 1000
    os.utime(mp4, (end_s, end_s))
    sidecar = O(v=1, deviceId=device_id, streamKey="cam0", sessionId=sid, seq=seq, startTs=start_ms,
                startMono=1_000 + seq * SEG_S * 1000, durationMs=SEG_S * 1000, sizeBytes=FRAG, timeSource="ntp",
                codec="h264", width=1280, height=720, fps=15, bitrateKbps=1000,
                anchor="boundary" if seq == 0 else "fstat", finalized=True)
    put_json(os.path.join(d, f"{seq:05d}.json"), sidecar, end_s)
with open(os.path.join(d, mark), "w") as f:
    f.write(f"xorgate-rs bench D9 filler (scripts/bench/filler.sh seed --mib {mib}); filler.sh clear removes this dir\n")
os.utime(d, (base_ms / 1000 + n * SEG_S, base_ms / 1000 + n * SEG_S))
print(f"  seeded {d}: {n * FRAG} bytes in {n} fragments")
PY
    if [ "$DRY" != 1 ]; then
      NEW=$(filler_sessions | head -n1)
      [ -n "$NEW" ] && under_rs_home "$NEW" || { echo "seed did not produce a marked session under $STREAM_DIR"; exit 1; }
      chown -R pi:pi "$NEW"
      # The parents the seed may have created (recordings/, video/, cam0/) are pi-owned too.
      for p in "$RS_HOME/recordings" "$FILLER_ROOT" "$STREAM_DIR"; do chown pi:pi "$p"; done
      note "$NEW seeded ($MIB MiB requested)"
      echo "  recordings total now: $(du -sb "$RS_HOME/recordings" | cut -f1) bytes"
    fi
  fi
  ;;
esac
echo "== done: ${#CHANGED[@]} change(s)$([ "$DRY" = 1 ] && echo ' (dry run: nothing written)')"
REMOTE
} | bench_sudo_script
