#!/usr/bin/env bash
# Offline suite (Linux): the rig's xorgate-capture shim
# (tools/replay/shims/xorgate-capture). The fake "host" is a copy of the
# python3 ELF named xorgate-rs-py; exec'd as `xorgate-capture __capture-host
# <tokens>`, it runs the file `./__capture-host` (this suite writes it), which
# records sys.orig_argv (argv[0] included), its pid, and prints a few lines.
# The shim must: log its argv verbatim; swap ONLY `libcamerasrc
# camera-name=…`; tee every line with its own wall stamp; keep the pid; exec
# with argv[0] = xorgate-capture; take the host from RIG_REAL_HOST, else from
# /proc/<parent>/exe; refuse a parent that is not an xorgate-rs binary.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SHIM_SRC="$HERE/../shims/xorgate-capture"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/w/bin"; cp "$SHIM_SRC" "$T/w/bin/xorgate-capture"; chmod +x "$T/w/bin/xorgate-capture"
SHIM="$T/w/bin/xorgate-capture"
pass=0; fail=0
ok() { echo "PASS $1"; pass=$((pass + 1)); }
no() { echo "FAIL $1"; fail=$((fail + 1)); }
cp "$(readlink -f "$(command -v python3)")" "$T/xorgate-rs-py"
cat > "$T/__capture-host" <<'F'
import json, os, sys
open(os.environ["FAKE_OUT"], "w").write(json.dumps({"orig": sys.orig_argv, "argv": sys.argv[1:], "pid": os.getpid()}))
print("Setting pipeline to PLAYING ...", flush=True)
print("New clock: GstSystemClock", flush=True)
sys.stderr.write("ERROR: from element /x: boom\n"); sys.stderr.flush()
sys.stdout.write("tail-without-newline"); sys.stdout.flush()
F
cd "$T" || exit 1
wait_eof() { for _ in $(seq 50); do grep -q '"k":"eof"' "$T/w/shim.ndjson" 2>/dev/null && return; sleep 0.1; done; }
ARGV=(__capture-host -e libcamerasrc camera-name=/base/imx708@1a ! video/x-raw,framerate=15/1 ! "muxer=mp4mux fragment-duration=1000" "location=/h/%05d.mp4")
out=$(FAKE_OUT="$T/fake.json" RIG_REAL_HOST="$T/xorgate-rs-py" "$SHIM" "${ARGV[@]}" 2>"$T/err"); rc=$?
wait_eof
python3 -I - "$T" <<'P' && ok "log verbatim, swap, tee with own stamps, pid kept, argv[0]" || no "log verbatim, swap, tee with own stamps, pid kept, argv[0]"
import json, sys
t = sys.argv[1]
recs = [json.loads(l) for l in open(t + "/w/shim.ndjson")]
fake = json.load(open(t + "/fake.json"))
spawn = recs[0]
want = ["__capture-host", "-e", "libcamerasrc", "camera-name=/base/imx708@1a", "!", "video/x-raw,framerate=15/1", "!", "muxer=mp4mux fragment-duration=1000", "location=/h/%05d.mp4"]
assert spawn["k"] == "spawn" and spawn["tool"] == "xorgate-capture", spawn
assert spawn["argv"] == want, spawn["argv"]
swapped = ["__capture-host", "-e", "videotestsrc", "is-live=true", "pattern=ball", "!", "video/x-raw,framerate=15/1", "!", "muxer=mp4mux fragment-duration=1000", "location=/h/%05d.mp4"]
assert spawn["exec"] == swapped, spawn["exec"]
assert spawn["real"] == t + "/xorgate-rs-py", spawn
assert fake["orig"][0] == "xorgate-capture", fake["orig"]
assert fake["orig"][1:] == swapped, fake["orig"]
assert fake["pid"] == spawn["pid"], (fake, spawn)
lines = [(r["fd"], r["line"]) for r in recs if r["k"] == "line"]
assert (1, "New clock: GstSystemClock") in lines, lines
assert (2, "ERROR: from element /x: boom") in lines, lines
assert (1, "tail-without-newline") in lines, lines
assert all(isinstance(r["w"], int) and r["w"] > 1_600_000_000_000 for r in recs), recs
assert recs[-1]["k"] == "eof", recs[-1]
P
[ $rc -eq 0 ] && grep -q "New clock: GstSystemClock" <<<"$out" && ok "the agent still reads every stdout line" || no "the agent still reads every stdout line (rc=$rc)"
grep -q "ERROR: from element /x: boom" "$T/err" && ok "stderr passthrough" || no "stderr passthrough"
rm -f "$T/w/shim.ndjson"
FAKE_OUT="$T/fake2.json" RIG_REAL_HOST="$T/xorgate-rs-py" "$SHIM" __capture-host videotestsrc ! libcamerasrc ! fakesink >/dev/null 2>&1
wait_eof
python3 -I -c 'import json,sys; a=json.load(open(sys.argv[1]))["orig"]; assert a==["xorgate-capture","__capture-host","videotestsrc","!","libcamerasrc","!","fakesink"], a' "$T/fake2.json" \
  && ok "nothing but libcamerasrc camera-name=… is swapped" || no "nothing but libcamerasrc camera-name=… is swapped"
# No RIG_REAL_HOST: the host is the PARENT's exe. A parent that is not an
# xorgate-rs binary (bash) is refused: exit 127, nothing exec'd, logged.
rm -f "$T/w/shim.ndjson"
FAKE_OUT="$T/fake3.json" bash -c '"$0" __capture-host fakesrc ! fakesink; echo "rc=$?" > "$1"' "$SHIM" "$T/rc" >/dev/null 2>"$T/err3"
[ "$(cat "$T/rc")" = "rc=127" ] && [ ! -e "$T/fake3.json" ] && grep -q '"refused":true' "$T/w/shim.ndjson" && grep -q "not an xorgate-rs binary" "$T/err3" \
  && ok "a parent that is not xorgate-rs is refused" || no "a parent that is not xorgate-rs is refused ($(cat "$T/rc"))"
# ... and an xorgate-rs* parent IS the host (its /proc/<pid>/exe).
rm -f "$T/w/shim.ndjson"
FAKE_OUT="$T/fake4.json" "$T/xorgate-rs-py" -c "import subprocess,sys; sys.exit(subprocess.call(['$SHIM','__capture-host','fakesrc']))" >/dev/null 2>&1
wait_eof
python3 -I -c 'import json,sys; a=json.load(open(sys.argv[1]))["orig"]; assert a==["xorgate-capture","__capture-host","fakesrc"], a' "$T/fake4.json" \
  && grep -q "\"real\":\"$T/xorgate-rs-py\"" "$T/w/shim.ndjson" \
  && ok "the parent's own exe is the host (/proc/<ppid>/exe)" || no "the parent's own exe is the host"
echo "capture-shim: $pass passed, $fail failed"
[ $fail -eq 0 ]
