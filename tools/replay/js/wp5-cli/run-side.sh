#!/bin/sh
# Inside a container: run every case of /tools/cases.txt with $RUNNER, under
# /w/<id> (cwd = the case root, XORGATE_HOME=/w/<id>/home), fake nmcli/sudo/ping
# first on PATH, NO_COLOR. Writes /out/<id>/{out,err,code,tree,sudo.log}.
set -u
mkdir -p /fake/bin /fake/nosudo /fake/nonmcli
cat > /fake/bin/nmcli <<'X'
#!/bin/sh
case "$*" in
  *DEVICE,TYPE*) printf '%b\n' "${NMCLI_TYPES:-}";;
  *DEVICE,STATE*) printf '%b\n' "${NMCLI_STATES:-}";;
esac
X
cat > /fake/bin/sudo <<'X'
#!/bin/sh
echo "$*" >> "$CASE_ROOT/sudo.log"
[ -n "${SUDO_ERR:-}" ] && echo "$SUDO_ERR" | tr ':' ' ' >&2
exit "${SUDO_CODE:-0}"
X
cat > /fake/bin/ping <<'X'
#!/bin/sh
exit "${PING_CODE:-0}"
X
chmod +x /fake/bin/*
cp /fake/bin/nmcli /fake/bin/ping /fake/nosudo/; cp /fake/bin/sudo /fake/bin/ping /fake/nonmcli/
SELF_IP=$(hostname -i | awk '{print $1}')
grep -v '^#' /tools/cases.txt | while IFS='|' read -r id envs argv; do
  [ -n "$id" ] || continue
  R=/w/$id; rm -rf "$R"; mkdir -p "$R/home" /out/$id
  (
    cd "$R" || exit 1
    export CASE_ROOT=$R XORGATE_HOME=$R/home HARDWARE_MANIFEST=$R/hardware.json NO_COLOR=1 LTE_TELEMETRY_FILE=$R/none.json
    unset SSH_CONNECTION KVS_PIPELINE_DIR KVS_MANIFEST
    P=/fake/bin
    for kv in $envs; do
      k=${kv%%=*}; v=${kv#*=}
      case $k in
        SEED) case $v in
                src) printf 'videotestsrc ! appsink name=appsink-video\n' > src.gst;;
                direct) printf 'libcamerasrc camera-name=/base/cam0@1a ! video/x-raw ! appsink name=appsink-video\n' > src.gst;;
                counters) mkdir -p home/recordings; printf '{"v":1,"droppedSegments":17}' > home/recordings/counters.json;;
              esac;;
        SSH) export SSH_CONNECTION="10.9.8.7 51234 $SELF_IP 22";;
        NOSUDO) P=/fake/nosudo;;
        NONMCLI) P=/fake/nonmcli;;
        *) export "$k=$v";;
      esac
    done
    export PATH=$P:/usr/bin:/bin
    : > /out/$id/out; : > /out/$id/err; : > /out/$id/code
    echo "$argv" | sed 's/ ;; /\n/g' | while read -r cmd; do
      # shellcheck disable=SC2086
      $RUNNER $cmd >> /out/$id/out 2>> /out/$id/err; echo $? >> /out/$id/code
    done
    [ -f sudo.log ] && mv sudo.log /out/$id/sudo.log
    find . -type f | LC_ALL=C sort | while read -r f; do printf '%s %s\n' "$f" "$(od -An -tx1 -v "$f" | tr -d ' \n')"; done > /out/$id/tree
  )
done
