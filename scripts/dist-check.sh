#!/usr/bin/env bash
# Anonymous post-publish check of a dist bucket (phase-5-distribution.md §6/§7,
# I5; gate rows G4/G5/G12). Runs on the Mac, in CI right after the upload, and
# against the e2e fake bucket.
#
#   scripts/dist-check.sh <bucket-base-url> <ver> [--channel dev|prod] [--js-etag <etag>|absent]
#   scripts/dist-check.sh --js-etag-of <bucket-base-url>   # print the JS latest.json ETag (or absent)
#
#   <bucket-base-url>  the form devices get (agent.env DEVICE_DIST_BUCKET_URL,
#                      the CDK output DeviceDistBucketUrl):
#                      https://<bucket>.s3.<region>.amazonaws.com — one trailing
#                      `/` is stripped (the update_plan rule).
#   <ver>              the version just published (short sha on dev, tag on prod).
#   --channel          also require latest-rs.json "channel" == this value.
#   --js-etag          the JS latest.json ETag captured BEFORE the publish
#                      (`absent` = it did not exist): require it unchanged, i.e.
#                      this run wrote no JS key. Optional; G5/G12 evidence.
#
# Plain `curl` with NO credentials (`-q` ignores ~/.curlrc; nothing signs the
# request), so it sees exactly what a device sees. Prints one `ok <check>` or
# `FAIL <check>: <why>` line per check, then a summary; exit 0 only if every
# check is ok, 1 otherwise, 2 on usage.
#
# Checks: HTTP 200 for xorgate-agent-rs-<ver>.tar.gz, xorgate-agent-rs-latest.tar.gz,
# both sidecars, install-rs.sh + sidecar, latest-rs.json; every sidecar names its
# object's basename and its digest matches the object (the update_plan
# parse_sha256_sidecar rules: first line whose name — optional `*` — equals the
# basename, 64 hex digits); the alias is byte-identical to the versioned tarball;
# latest-rs.json is JSON with exactly the 5 string keys version/channel/builtAt/
# packageVersion/tarball, version == <ver>, channel dev|prod (== --channel when
# given), tarball == xorgate-agent-rs-<ver>.tar.gz; the install-rs.sh object is
# byte-identical to the copy inside the versioned tarball.
# Needs: bash, curl, sha256sum (or shasum), GNU or bsd tar, cmp, python3 (JSON).
set -uo pipefail

usage() { sed -n '2,33p' "$0" >&2; exit 2; }
# The JS latest.json ETag as an anonymous HEAD sees it ("absent" on non-200).
js_etag_of() { # <base-url>
  local r code etag
  r=$(curl -q -sS -I --retry 3 --retry-delay 2 -o /dev/null -w '%{http_code} %header{etag}' "${1%/}/latest.json" 2>/dev/null || true)
  code=${r%% *}; etag=${r#* }
  if [ "$code" != 200 ] || [ -z "$etag" ] || [ "$etag" = "$r" ]; then etag=absent; fi
  printf '%s\n' "$etag"
}
if [ "${1:-}" = --js-etag-of ]; then [ $# = 2 ] || usage; js_etag_of "$2"; exit 0; fi
[ $# -ge 2 ] || usage
BASE=${1%/}; VER=$2; shift 2
WANT_CHANNEL="" JS_ETAG=""; JS_ETAG_SET=0
while [ $# -gt 0 ]; do
  case "$1" in
    --channel) [ $# -ge 2 ] || usage; WANT_CHANNEL=$2; shift 2
               case "$WANT_CHANNEL" in dev|prod) ;; *) echo "dist-check: --channel dev|prod" >&2; exit 2 ;; esac ;;
    --js-etag) [ $# -ge 2 ] || usage; JS_ETAG=$2; JS_ETAG_SET=1; shift 2 ;;
    -h|--help) usage ;;
    *) echo "dist-check: unknown argument $1" >&2; exit 2 ;;
  esac
done
if [ -z "$BASE" ] || [ -z "$VER" ]; then usage; fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
FAILS=0 OKS=0
ok() { echo "ok $*"; OKS=$((OKS + 1)); }
bad() { echo "FAIL $*"; FAILS=$((FAILS + 1)); }
sha_of() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

VERSIONED="xorgate-agent-rs-$VER.tar.gz"
ALIAS="xorgate-agent-rs-latest.tar.gz"
KEYS=("$VERSIONED" "$VERSIONED.sha256" "$ALIAS" "$ALIAS.sha256" install-rs.sh install-rs.sh.sha256 latest-rs.json)

# 1. every key: HTTP 200, anonymously.
# (bash 3.2-safe: no associative arrays — a fetched object exists as $TMP/o/<key>)
mkdir -p "$TMP/o"
got() { [ -f "$TMP/o/$1" ]; }
for k in "${KEYS[@]}"; do
  code=$(curl -q -sS --retry 3 --retry-delay 2 -o "$TMP/o/$k" -w '%{http_code}' "$BASE/$k" 2>"$TMP/$k.err")
  if [ "$code" = 200 ]; then ok "http 200 $k"
  else bad "http 200 $k: got ${code:-none}$( [ -s "$TMP/$k.err" ] && printf ' (%s)' "$(head -c 200 "$TMP/$k.err" | tr '\n' ' ')")"; rm -f "$TMP/o/$k"; fi
done

# 2. sidecars (update_plan::parse_sha256_sidecar rules).
sidecar_hex() { # <sidecar file> <basename> -> lower-case hex or nothing
  awk -v want="$2" '{
      line = $0; sub(/\r$/, "", line); sub(/^[ \t]+/, "", line); sub(/[ \t]+$/, "", line)
      if (!match(line, /[ \t]/)) next
      hex = substr(line, 1, RSTART - 1); name = substr(line, RSTART + 1)
      sub(/^[ \t]+/, "", name); sub(/^\*/, "", name)
      if (name != want) next
      if (hex !~ /^[0-9a-fA-F]+$/ || length(hex) != 64) next
      print tolower(hex); exit
    }' "$1"
}
for obj in "$VERSIONED" "$ALIAS" install-rs.sh; do
  if ! got "$obj" || ! got "$obj.sha256"; then bad "sha256 $obj: object or sidecar missing"; continue; fi
  want=$(sidecar_hex "$TMP/o/$obj.sha256" "$obj")
  have=$(sha_of "$TMP/o/$obj")
  if [ -z "$want" ]; then bad "sha256 $obj: $obj.sha256 has no '<64 hex>  $obj' line"
  elif [ "$want" = "$have" ]; then ok "sha256 $obj ($have)"
  else bad "sha256 $obj: sidecar $want != object $have"; fi
done

# 3. the alias is the versioned build.
if got "$VERSIONED" && got "$ALIAS"; then
  if cmp -s "$TMP/o/$VERSIONED" "$TMP/o/$ALIAS"; then ok "alias $ALIAS == $VERSIONED"
  else bad "alias $ALIAS == $VERSIONED: they differ (the alias points at another build)"; fi
else bad "alias $ALIAS == $VERSIONED: object missing"; fi

# 4. latest-rs.json: frozen 5-field schema (Q7).
if got latest-rs.json; then
  if ! command -v python3 >/dev/null 2>&1; then bad "latest-rs.json schema: python3 not available to parse JSON"
  else
    if why=$(python3 -I - "$TMP/o/latest-rs.json" "$VER" "$WANT_CHANNEL" <<'PY'
import json, sys
path, ver, want_channel = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    d = json.load(open(path, encoding="utf-8"))
except Exception as e:  # noqa: BLE001
    print("not JSON: %s" % e); sys.exit(1)
keys = ["version", "channel", "builtAt", "packageVersion", "tarball"]
if not isinstance(d, dict):
    print("not an object"); sys.exit(1)
if sorted(d) != sorted(keys):
    print("keys %s != %s" % (sorted(d), sorted(keys))); sys.exit(1)
bad = [k for k in keys if not isinstance(d[k], str) or not d[k]]
if bad:
    print("non-string or empty: %s" % bad); sys.exit(1)
if d["version"] != ver:
    print("version %r != %r" % (d["version"], ver)); sys.exit(1)
if d["channel"] not in ("dev", "prod"):
    print("channel %r not dev|prod" % d["channel"]); sys.exit(1)
if want_channel and d["channel"] != want_channel:
    print("channel %r != %r" % (d["channel"], want_channel)); sys.exit(1)
if d["tarball"] != "xorgate-agent-rs-%s.tar.gz" % ver:
    print("tarball %r != %r" % (d["tarball"], "xorgate-agent-rs-%s.tar.gz" % ver)); sys.exit(1)
print("version=%s channel=%s builtAt=%s packageVersion=%s" % (d["version"], d["channel"], d["builtAt"], d["packageVersion"]))
PY
); then
      ok "latest-rs.json schema ($why)"
    else bad "latest-rs.json schema: $why"; fi
  fi
else bad "latest-rs.json schema: object missing"; fi

# 5. the bucket install-rs.sh == the immutable copy inside the versioned tarball.
if got "$VERSIONED" && got install-rs.sh; then
  LISTING=$(tar -tzf "$TMP/o/$VERSIONED" 2>/dev/null) || LISTING=""
  member=""
  for m in install-rs.sh ./install-rs.sh; do grep -qxF "$m" <<<"$LISTING" && { member=$m; break; }; done
  if [ -z "$member" ]; then bad "install-rs.sh == $VERSIONED:install-rs.sh: no install-rs.sh member in the tarball"
  elif tar -xzOf "$TMP/o/$VERSIONED" "$member" > "$TMP/tar-install-rs.sh" 2>/dev/null && cmp -s "$TMP/tar-install-rs.sh" "$TMP/o/install-rs.sh"; then
    ok "install-rs.sh == $VERSIONED:install-rs.sh"
  else bad "install-rs.sh == $VERSIONED:install-rs.sh: the bucket copy differs from the tarball's"; fi
else bad "install-rs.sh == $VERSIONED:install-rs.sh: object missing"; fi

# 6. optional: the JS manifest was not written by this run.
if [ "$JS_ETAG_SET" = 1 ]; then
  etag=$(js_etag_of "$BASE")
  if [ "$etag" = "$JS_ETAG" ]; then ok "JS latest.json untouched (ETag $etag)"
  else bad "JS latest.json untouched: ETag before '$JS_ETAG', now '$etag'"; fi
fi

echo "== dist-check $BASE $VER: $OKS ok, $FAILS FAIL"
[ "$FAILS" = 0 ]
