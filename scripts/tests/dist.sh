#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015  # test assertions: `[ … ]; expect $?` and `a && pass || failc`
# Offline tests of the dist plumbing (phase-5-distribution.md §6/§7, I5, gate
# row G4), run as ROOT in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/tests/dist.sh
# (with the b3dad3c extraction check: first `git show b3dad3c:.github/workflows/
#  device-rs-deploy-dev.yml > target/dist-ref/old-dev.yml` (+ prod), then add
#  `-e DIST_REF_DIR=target/dist-ref`).
#
#   1. scripts/dist-manifest.sh bytes == the deploy workflows' latest-rs.json
#      heredoc up to b3dad3c (copied below verbatim, de-indented as YAML `run: |`
#      does), for dev and prod, plus one literal golden; and, when the git object
#      store is reachable (a Mac run), against the heredoc extracted from
#      `git show b3dad3c:.github/workflows/device-rs-deploy-*.yml`.
#   2. dist-manifest.sh schema refusals (exit 2, nothing on stdout).
#   3. scripts/dist-guard.sh: the -rs allow-list; the JS keys refused.
#   4. the workflow YAML: triggers unchanged (no push/PR/schedule added; ci.yml
#      manual-only), the Q9 path filter, secrets = the AWS pair only, the prod
#      account guard, every `aws s3 cp` behind dist-guard, the §6 upload order,
#      cache headers, the post-upload dist-check step; and (Phase 8 WP-R2) the
#      gate parity: both deploy workflows run the same gate steps (fmt, clippy,
#      feature-gate.sh, test) in the same order, with negative controls.
#   5. the workflows' upload step itself, extracted from the YAML and run with a
#      stubbed `aws` into a local bucket (two scripts/pack.sh tarballs, v1 then
#      v2), served by python3 with ETags; refusals write nothing.
#   6. scripts/dist-check.sh against that bucket: pass, and FAIL on each broken
#      variant (sha mismatch, missing key, bad JSON, extra key, install-rs.sh !=
#      the tarball copy, alias sidecar name, wrong channel, stale version, JS
#      latest.json touched).
set -uo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root (in a container)"; exit 2; }
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
S=$REPO/scripts
need_pkgs=()
for c in curl:curl python3:python3 sha256sum:coreutils tar:tar; do command -v "${c%%:*}" >/dev/null || need_pkgs+=("${c#*:}"); done
python3 -c 'import yaml' 2>/dev/null || need_pkgs+=(python3-yaml)
if [ ${#need_pkgs[@]} -gt 0 ]; then
  apt-get update -qq >/dev/null && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "${need_pkgs[@]}" python3-yaml >/dev/null \
    || { echo "cannot install ${need_pkgs[*]}"; exit 2; }
fi
fail=0
pass() { echo "PASS: $*"; }
failc() { echo "FAIL: $*"; fail=1; }
expect() { if [ "$1" = 0 ]; then pass "$2"; else failc "$2"; fi; }
T=$(mktemp -d)
SRV_PID=""
# shellcheck disable=SC2317,SC2329  # invoked by the EXIT trap
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$T"; }
trap cleanup EXIT

echo "== 1. dist-manifest.sh bytes == the b3dad3c workflow heredoc"
# The heredoc of device-rs-deploy-{dev,prod}.yml "Upload to S3" @ b3dad3c,
# verbatim after YAML's block de-indent (10 spaces). DO NOT EDIT: this is the pin.
ref_heredoc() { # <channel> <version> <builtAt> <packageVersion> <tarball>
  local VERSION=$2 BUILT_AT=$3 PKG_VERSION=$4 TARBALL=$5
  if [ "$1" = dev ]; then
cat <<EOF
{ "version": "$VERSION", "channel": "dev", "builtAt": "$BUILT_AT",
  "packageVersion": "$PKG_VERSION", "tarball": "$TARBALL" }
EOF
  else
cat <<EOF
{ "version": "$VERSION", "channel": "prod", "builtAt": "$BUILT_AT",
  "packageVersion": "$PKG_VERSION", "tarball": "$TARBALL" }
EOF
  fi
}
for args in "dev abc1234 2026-10-21T00:00:00Z 0.1.0" "prod v0.1.0 2026-10-21T12:34:56Z 0.1.0" "dev 3f9e0d1 2026-01-02T03:04:05Z 1.22.333"; do
  read -r ch v t p <<<"$args"
  "$S/dist-manifest.sh" --version "$v" --channel "$ch" --built-at "$t" --package-version "$p" --tarball "xorgate-agent-rs-$v.tar.gz" > "$T/m.json"; rc=$?
  ref_heredoc "$ch" "$v" "$t" "$p" "xorgate-agent-rs-$v.tar.gz" > "$T/ref.json"
  [ "$rc" = 0 ] && cmp -s "$T/m.json" "$T/ref.json"; expect $? "bytes == heredoc ($ch $v)"
done
printf '{ "version": "abc1234", "channel": "dev", "builtAt": "2026-10-21T00:00:00Z",\n  "packageVersion": "0.1.0", "tarball": "xorgate-agent-rs-abc1234.tar.gz" }\n' > "$T/golden.json"
"$S/dist-manifest.sh" --version abc1234 --channel dev --built-at 2026-10-21T00:00:00Z --package-version 0.1.0 --tarball xorgate-agent-rs-abc1234.tar.gz | cmp -s - "$T/golden.json"
expect $? "bytes == the literal golden (2 lines, 2-space indent, trailing newline; $(wc -c < "$T/golden.json") bytes)"
python3 -I - "$T/golden.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert list(d) == ["version", "channel", "builtAt", "packageVersion", "tarball"], list(d)
assert all(isinstance(v, str) for v in d.values())
PY
expect $? "output parses as JSON: exactly the 5 string keys, in order"
# DIST_REF_DIR=<dir with old-dev.yml + old-prod.yml> (`git show b3dad3c:… >`)
# feeds this check inside a container, where the worktree's git store is absent.
if [ -n "${DIST_REF_DIR:-}" ] || { command -v git >/dev/null && git -C "$REPO" cat-file -e b3dad3c 2>/dev/null; }; then
  for ch in dev prod; do
    if [ -n "${DIST_REF_DIR:-}" ]; then cp "$DIST_REF_DIR/old-$ch.yml" "$T/old-$ch.yml"
    else git -C "$REPO" show "b3dad3c:.github/workflows/device-rs-deploy-$ch.yml" > "$T/old-$ch.yml"; fi
    python3 -I - "$T/old-$ch.yml" > "$T/old-$ch.sh" <<'PY'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
run = next(s["run"] for s in d["jobs"]["build-and-publish"]["steps"] if s.get("name", "").startswith("Upload to S3"))
lines = run.splitlines(True)
i = next(n for n, l in enumerate(lines) if l.startswith("cat > latest-rs.json <<EOF"))
j = next(n for n, l in enumerate(lines) if n > i and l.rstrip("\n") == "EOF")
sys.stdout.write("cat <<EOF\n" + "".join(lines[i + 1:j + 1]))
PY
    VERSION=v9 BUILT_AT=2026-10-21T00:00:00Z PKG_VERSION=0.1.0 TARBALL=xorgate-agent-rs-v9.tar.gz bash "$T/old-$ch.sh" > "$T/old-$ch.json"
    "$S/dist-manifest.sh" --version v9 --channel "$ch" --built-at 2026-10-21T00:00:00Z --package-version 0.1.0 --tarball xorgate-agent-rs-v9.tar.gz | cmp -s - "$T/old-$ch.json"
    expect $? "bytes == the heredoc EXTRACTED from b3dad3c's device-rs-deploy-$ch.yml"
  done
else
  echo "  SKIP: no git object store here and no DIST_REF_DIR (see above)"
fi

echo "== 2. dist-manifest.sh refusals (exit 2, empty stdout)"
OKA=(--version abc --channel dev --built-at 2026-10-21T00:00:00Z --package-version 0.1.0 --tarball xorgate-agent-rs-abc.tar.gz)
refuse() { # <label> <args…>
  local label=$1; shift
  "$S/dist-manifest.sh" "$@" > "$T/r.out" 2> "$T/r.err"; local rc=$?
  [ "$rc" = 2 ] && [ ! -s "$T/r.out" ]; expect $? "refuses $label (rc=$rc: $(head -n1 "$T/r.err"))"
}
refuse "a JS tarball name" --version abc --channel dev --built-at t --package-version p --tarball xorgate-agent-abc.tar.gz
refuse "latest.json as tarball" --version abc --channel dev --built-at t --package-version p --tarball latest.json
refuse "an empty -rs version" --version abc --channel dev --built-at t --package-version p --tarball xorgate-agent-rs-.tar.gz
refuse "a tarball that is not this version's key" --version abc --channel dev --built-at t --package-version p --tarball xorgate-agent-rs-latest.tar.gz
refuse "a .zip" --version abc --channel dev --built-at t --package-version p --tarball xorgate-agent-rs-abc.zip
refuse "channel ci" --version abc --channel ci --built-at t --package-version p --tarball xorgate-agent-rs-abc.tar.gz
refuse "a missing --built-at" --version abc --channel dev --package-version p --tarball xorgate-agent-rs-abc.tar.gz
refuse "an empty --package-version" --version abc --channel dev --built-at t --package-version "" --tarball xorgate-agent-rs-abc.tar.gz
refuse "a quote in a value (JSON injection)" --version abc --channel dev --built-at 't", "sha256": "x' --package-version p --tarball xorgate-agent-rs-abc.tar.gz
refuse "a backslash in a value" --version abc --channel dev --built-at 't"' --package-version p --tarball xorgate-agent-rs-abc.tar.gz
refuse "a newline in a value" --version abc --channel dev --built-at $'t\nx' --package-version p --tarball xorgate-agent-rs-abc.tar.gz
refuse "an unknown flag (--sha256: no 6th field, Q7)" "${OKA[@]}" --sha256 deadbeef
refuse "a flag without its value" "${OKA[@]}" --tarball
"$S/dist-manifest.sh" "${OKA[@]}" >/dev/null; expect $? "accepts the well-formed call"

echo "== 3. dist-guard.sh: the -rs allow-list"
for k in xorgate-agent-rs-abc1234.tar.gz xorgate-agent-rs-abc1234.tar.gz.sha256 xorgate-agent-rs-v0.1.0.tar.gz \
         xorgate-agent-rs-latest.tar.gz xorgate-agent-rs-latest.tar.gz.sha256 latest-rs.json install-rs.sh install-rs.sh.sha256; do
  "$S/dist-guard.sh" "$k" 2>/dev/null; expect $? "allows $k"
done
for k in latest.json xorgate-agent-v0.1.2.tar.gz xorgate-agent-v0.1.2.tar.gz.sha256 xorgate-agent-latest.tar.gz \
         xorgate-agent-latest.tar.gz.sha256 xorgate-agent-abc1234.tar.gz latest-rs.json.sha256 install-rs.sh.bak \
         dist/latest-rs.json ../latest-rs.json /latest-rs.json xorgate-agent-rs-.tar.gz xorgate-agent-rs-a/b.tar.gz \
         xorgate-agent-rs-..tar.gz "xorgate-agent-rs-a b.tar.gz" "" " latest-rs.json" "latest-rs.json " install.sh index.html; do
  "$S/dist-guard.sh" "$k" 2>/dev/null; rc=$?
  [ "$rc" = 1 ]; expect $? "refuses '$k' (rc=$rc)"
done
"$S/dist-guard.sh" 2>/dev/null; [ $? = 2 ]; expect $? "usage error without a key (rc 2)"

echo "== 4. the workflow YAML (static)"
python3 -I - "$REPO/.github/workflows" "$S/dist-guard.sh" > "$T/yaml.out" 2>&1 <<'PY'
import os, re, subprocess, sys, yaml
wf, guard = sys.argv[1], sys.argv[2]
bad = []
def chk(cond, msg):
    print(("PASS: " if cond else "FAIL: ") + msg)
    if not cond: bad.append(msg)
def load(name):
    return yaml.safe_load(open(os.path.join(wf, name))), open(os.path.join(wf, name)).read()
def on(d):
    return d.get("on", d.get(True))  # PyYAML reads the key `on` as True
ci, _ = load("ci.yml")
chk(set(on(ci)) == {"workflow_dispatch"}, "ci.yml stays manual-only (workflow_dispatch): %s" % sorted(on(ci)))
for ch, trig in (("dev", {"push", "workflow_dispatch"}), ("prod", {"release", "workflow_dispatch"})):
    d, text = load("device-rs-deploy-%s.yml" % ch)
    o = on(d)
    chk(set(o) == trig, "%s: triggers unchanged %s" % (ch, sorted(o)))
    chk(not ({"pull_request", "pull_request_target", "schedule", "workflow_run"} & set(o)), "%s: no PR/schedule/workflow_run trigger" % ch)
    if ch == "dev":
        chk(o["push"].get("branches") == ["main"], "dev: push only on main")
        paths = o["push"]["paths"]
        neg = [p for p in paths if p.startswith("!")]
        chk(neg == ["!scripts/bench/**", "!**/*.md"], "dev: Q9 negations %s" % neg)
        chk(paths[-2:] == neg, "dev: the negations come after every positive pattern")
        chk("scripts/**" in paths and "crates/**" in paths and ".github/workflows/device-rs-deploy-dev.yml" in paths, "dev: the positive filter is kept")
        chk(o.get("workflow_dispatch") in (None, {}), "dev: dispatch takes no inputs (unchanged)")
    else:
        chk(o["release"] == {"types": ["published"]}, "prod: release: published only")
        chk(list((o["workflow_dispatch"] or {}).get("inputs", {})) == ["tag"], "prod: dispatch input tag only")
    job = d["jobs"]["build-and-publish"]
    chk(list(d["jobs"]) == ["build-and-publish"], "%s: one job" % ch)
    chk(job["environment"] == ("development" if ch == "dev" else "production"), "%s: environment %s" % (ch, job["environment"]))
    secrets = set(re.findall(r"\$\{\{\s*secrets\.([A-Za-z0-9_]+)\s*\}\}", text))
    chk(secrets == {"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"}, "%s: secrets = the AWS pair only %s" % (ch, sorted(secrets)))
    steps = job["steps"]
    names = [s.get("name", s.get("uses", "")) for s in steps]
    runs = "\n".join(s.get("run", "") for s in steps)
    if ch == "prod":
        acct = next((s for s in steps if s.get("name") == "Verify target account"), None)
        chk(acct is not None and '"865609249890"' in acct["run"], "prod: the account guard (865609249890) is kept")
        chk(names.index("Verify target account") < names.index(next(n for n in names if n.startswith("Upload to S3"))), "prod: account guard before the upload")
    # every S3 write goes through put() -> dist-guard
    writes = re.findall(r"aws\s+s3(?:api)?\s+(\S+)", runs)
    chk(writes == ["cp"], "%s: exactly one aws s3 write verb occurrence (put's cp): %s" % (ch, writes))
    up = next(s for s in steps if s.get("name", "").startswith("Upload to S3"))
    r = up["run"]
    chk(re.search(r'put\(\) \{[^}]*scripts/dist-guard\.sh "\$2"\s*\n\s*aws s3 cp "\$1" "s3://\$BUCKET/\$2"', r) is not None, "%s: put() runs dist-guard on the key before aws s3 cp" % ch)
    chk(r.lstrip().startswith("set -euo pipefail"), "%s: upload step runs under set -euo pipefail" % ch)
    # In a `container:` job GitHub's default run shell is `sh -e` (dash): a
    # bash-only step must say `shell: bash` (c6b818b's deploy-dev died on
    # `set: Illegal option -o pipefail`).
    dflt = ((job.get("defaults") or {}).get("run") or {}).get("shell")
    for s_ in steps:
        body = s_.get("run", "")
        if re.search(r"pipefail|<<<|\[\[|\$'", body):
            sh = s_.get("shell", dflt)
            chk(sh == "bash", "%s: bash-only step %r declares shell: bash (got %r)" % (ch, s_.get("name"), sh))
    chk("scripts/dist-manifest.sh" in r and "<<EOF" not in r and "cat >" not in r, "%s: latest-rs.json from dist-manifest.sh (no heredoc)" % ch)
    chk("--channel %s " % ch in r, "%s: dist-manifest.sh --channel %s" % (ch, ch))
    puts = re.findall(r"^\s*put (\S+) (\S+) (\S+) (\S+)\s*$", r, re.M)
    keys = [k for _, k, _, _ in puts]
    want = ['"$TARBALL"', '"$TARBALL.sha256"', "xorgate-agent-rs-latest.tar.gz", "xorgate-agent-rs-latest.tar.gz.sha256",
            "install-rs.sh", "install-rs.sh.sha256", "latest-rs.json"]
    chk(keys == want, "%s: §6 upload order %s" % (ch, keys))
    cache = {k: c for _, k, c, _ in puts}
    chk(cache.get('"$TARBALL"') == '"$IMMUTABLE"' and cache.get('"$TARBALL.sha256"') == '"$IMMUTABLE"'
        and all(cache.get(k) == "no-cache" for k in want[2:]), "%s: cache headers (versioned immutable, the rest no-cache)" % ch)
    chk('IMMUTABLE="public,max-age=31536000,immutable"' in r, "%s: immutable = public,max-age=31536000,immutable (unchanged)" % ch)
    for k in want[2:]:
        rc = subprocess.run([guard, k], capture_output=True).returncode
        chk(rc == 0, "%s: literal key %s passes the guard" % (ch, k))
    chk("latest.json" not in re.sub(r"latest-rs\.json", "", r), "%s: the upload step never names latest.json" % ch)
    chk(re.search(r'scripts/dist-guard\.sh "\$TARBALL"', r) is not None, "%s: the -rs refusal of $TARBALL is kept (now dist-guard)" % ch)
    i_up = names.index(up["name"])
    post = [s for s in steps[i_up + 1:] if s.get("run", "").lstrip().startswith("scripts/dist-check.sh ")]
    chk(len(post) == 1 and '--channel %s --js-etag "$JS_ETAG"' % ch in post[0]["run"] and post[0].get("if") is None,
        "%s: post-upload dist-check.sh step (anonymous, --channel %s, --js-etag; not if: always)" % (ch, ch))
    pre = [s for s in steps[:i_up] if "scripts/dist-check.sh --js-etag-of" in s.get("run", "")]
    chk(len(pre) == 1, "%s: the JS latest.json ETag is captured BEFORE the upload" % ch)
    res = next(s for s in steps if s.get("id") == "bucket")
    chk("DeviceDistBucketUrl" in res["run"] and "url=$URL" in res["run"], "%s: device-facing URL from the stack output DeviceDistBucketUrl" % ch)
# Phase 8 WP-R2 (plan-notes X8): both deploy workflows run the SAME gate
# steps in the SAME order (deploy-prod lacked feature-gate.sh until WP-R2).
# The gate = every step from `cargo fmt` up to (not incl.) `cargo build
# --release`, each step compared whole (name, run, shell, env, if, ...); a gate
# command anywhere else in the job is refused.
import copy
GATE_WANT = ["cargo fmt --all -- --check", "cargo clippy --workspace --all-targets -- -D warnings",
             "bash scripts/tests/cargo/feature-gate.sh", "cargo test --workspace"]
GATE_CMD = re.compile(r"cargo (fmt|clippy|test)\b|scripts/tests/")
def gate_steps(steps):
    runs = [s_.get("run", "").strip() for s_ in steps]
    i = next((k for k, r in enumerate(runs) if r.startswith("cargo fmt")), None)
    j = next((k for k, r in enumerate(runs) if r.startswith("cargo build --release")), None)
    if i is None or j is None or j < i:
        return None, "no fmt..build --release span"
    rest = [r for k, r in enumerate(runs) if not i <= k < j and GATE_CMD.search(r)]
    if rest:
        return None, "a gate command outside the gate span: %r" % rest
    return [dict(s_) for s_ in steps[i:j]], None
def gate_parity(dsteps, psteps):
    """[] when both gates are equal and in the wanted order, else the problems."""
    (dg, de), (pg, pe) = gate_steps(dsteps), gate_steps(psteps)
    if de or pe:
        return [m for m in ("dev: %s" % de if de else None, "prod: %s" % pe if pe else None) if m]
    probs = []
    if [g.get("run", "").strip() for g in dg] != GATE_WANT:
        probs.append("dev gate order %r" % [g.get("run", "").strip() for g in dg])
    if dg != pg:
        probs.append("dev %r != prod %r" % ([g.get("name") for g in dg], [g.get("name") for g in pg]))
    return probs
dsteps = load("device-rs-deploy-dev.yml")[0]["jobs"]["build-and-publish"]["steps"]
psteps = load("device-rs-deploy-prod.yml")[0]["jobs"]["build-and-publish"]["steps"]
probs = gate_parity(dsteps, psteps)
chk(probs == [], "gate parity: deploy-dev and deploy-prod run the same gate steps in the same order (fmt, clippy, feature-gate, test) %s" % probs)
pg = gate_steps(psteps)[0] or []
chk([g.get("name") for g in pg][1:3] == ["cargo clippy", "Feature gate (imu-test-bus is test-only)"],
    "prod: feature-gate.sh runs right after clippy")
# Negative controls: the parity check must FAIL on each broken variant.
def without_fg(st):
    return [x for x in st if "feature-gate.sh" not in x.get("run", "")]
def swapped(st):
    st = copy.deepcopy(st)
    a = next(k for k, x in enumerate(st) if x.get("run", "").startswith("cargo clippy"))
    st[a], st[a + 1] = st[a + 1], st[a]
    return st
def edited(st):
    st = copy.deepcopy(st)
    next(x for x in st if "feature-gate.sh" in x.get("run", ""))["run"] += " || true"
    return st
def moved_late(st):
    st = copy.deepcopy(st)
    fg = next(x for x in st if "feature-gate.sh" in x.get("run", ""))
    st.remove(fg)
    st.append(fg)
    return st
for label, d_, p_ in (("prod without feature-gate (as on main 14413e0)", dsteps, without_fg(psteps)),
                      ("both without feature-gate", without_fg(dsteps), without_fg(psteps)),
                      ("prod clippy/feature-gate swapped", dsteps, swapped(psteps)),
                      ("both swapped", swapped(dsteps), swapped(psteps)),
                      ("prod feature-gate run edited", dsteps, edited(psteps)),
                      ("prod feature-gate moved after the build", dsteps, moved_late(psteps))):
    chk(gate_parity(d_, p_) != [], "gate parity control: %s is caught" % label)
print("YAML-FAILS=%d" % len(bad))
PY
cat "$T/yaml.out"
grep -q "^YAML-FAILS=0$" "$T/yaml.out"; expect $? "workflow YAML static checks"

echo "== 5. the upload step, run with a stubbed aws into a local bucket"
step_run() { # <dev|prod> <step-name-prefix> -> the step's run: text
  python3 -I - "$REPO/.github/workflows/device-rs-deploy-$1.yml" "$2" <<'PY'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
sys.stdout.write(next(s["run"] for s in d["jobs"]["build-and-publish"]["steps"] if s.get("name", "").startswith(sys.argv[2])))
PY
}
step_run dev "Upload to S3" > "$T/upload-dev.sh"; step_run prod "Upload to S3" > "$T/upload-prod.sh"
step_run dev "Capture the JS latest.json ETag" > "$T/etag-dev.sh"
diff <(sed 's/--channel prod/--channel dev/' "$T/upload-prod.sh") "$T/upload-dev.sh" > "$T/up.diff"; expect $? "dev and prod upload steps differ only in the channel"
mkdir -p "$T/stub" "$T/www/dev" "$T/ws"
cat > "$T/stub/aws" <<'A'
#!/usr/bin/env bash
echo "aws $*" >> "$AWS_LOG"
[ "${1:-} ${2:-}" = "s3 cp" ] || { echo "stub aws: unexpected: $*" >&2; exit 1; }
src=$3 dst=$4; shift 4; cc="" ct=""
while [ $# -gt 0 ]; do
  case "$1" in --cache-control) cc=$2; shift 2 ;; --content-type) ct=$2; shift 2 ;; *) echo "stub aws: unexpected arg $1" >&2; exit 1 ;; esac
done
key=${dst#"s3://$BUCKET/"}
[ "$key" != "$dst" ] && [ -n "$key" ] || { echo "stub aws: not under s3://$BUCKET/: $dst" >&2; exit 1; }
mkdir -p "$BUCKET_DIR/$(dirname "$key")"; cp "$src" "$BUCKET_DIR/$key"
echo "$key|$cc|$ct" >> "$PUT_LOG"
A
chmod +x "$T/stub/aws"
cat > "$T/srv.py" <<'PY'
import hashlib, http.server, os, sys
class H(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        p = self.translate_path(self.path)
        if os.path.isfile(p):
            with open(p, "rb") as f:
                self.send_header("ETag", '"%s"' % hashlib.md5(f.read()).hexdigest())
        super().end_headers()
    def log_message(self, *a):
        pass
os.chdir(sys.argv[2])
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
PORT=18731
python3 -I "$T/srv.py" "$PORT" "$T/www" & SRV_PID=$!
for _ in $(seq 1 50); do curl -q -s -o /dev/null "http://127.0.0.1:$PORT/" && break; sleep 0.1; done
URL="http://127.0.0.1:$PORT/dev"
printf '{ "version": "js-v0.1.2", "channel": "dev" }\n' > "$T/www/dev/latest.json"   # the JS manifest (never written)
printf 'JS tarball\n' > "$T/www/dev/xorgate-agent-latest.tar.gz"
# Two pack.sh tarballs (a stub binary: nothing here executes it).
printf '#!/bin/sh\necho "xorgate-rs 0.0.0-test"\n' > "$T/xorgate-rs"; chmod +x "$T/xorgate-rs"
for v in v1 v2; do
  "$S/pack.sh" --channel dev --version "$v" --bin "$T/xorgate-rs" --out-dir "$T/pack-$v" > "$T/pack-$v.out" 2> "$T/pack-$v.err"
  expect $? "pack.sh --version $v"
  TB="$T/pack-$v/xorgate-agent-rs-$v.tar.gz"
  LISTING=$(tar -tzf "$TB")   # into a variable: never `tar -tzf | grep -q` (SIGPIPE, F-E1-1)
  if ! grep -qx install-rs.sh <<<"$LISTING"; then
    # Pre-WP1 main: pack.sh does not ship install-rs.sh yet. Re-pack with the
    # repo's script (or a stand-in) so the upload step has its member.
    echo "  NOTE: $v: pack.sh did not ship install-rs.sh (WP1 not merged) — re-packed with $([ -f "$S/install-rs.sh" ] && echo scripts/install-rs.sh || echo 'a stand-in')"
    R=$(mktemp -d); tar -xzf "$TB" -C "$R"
    if [ -f "$S/install-rs.sh" ]; then install -m 0755 "$S/install-rs.sh" "$R/install-rs.sh"
    else printf '#!/usr/bin/env bash\n# stand-in install-rs.sh (%s)\necho "[install-rs] test"\n' "$v" > "$R/install-rs.sh"; chmod 0755 "$R/install-rs.sh"; fi
    (cd "$R" && tar -czf "$TB" --owner=0 --group=0 --numeric-owner -- *)
    rm -rf "$R"
    printf '%s  %s\n' "$(sha256sum "$TB" | cut -d' ' -f1)" "xorgate-agent-rs-$v.tar.gz" > "$TB.sha256"
  fi
done
ln -s "$S" "$T/ws/scripts"   # the step runs from the checkout root (scripts/…)
publish() { # <dev|prod> <version> [VAR=value overrides…] -> rc of the upload step
  local ch=$1 v=$2; shift 2
  local tb="$T/pack-$v/xorgate-agent-rs-$v.tar.gz"
  ( cd "$T/ws" && env PATH="$T/stub:$PATH" AWS_LOG="$T/aws.log" PUT_LOG="$T/put.log" BUCKET=fake-dist BUCKET_DIR="$T/www/dev" \
      TARBALL="xorgate-agent-rs-$v.tar.gz" TARBALL_PATH="$tb" VERSION="$v" BUILT_AT=2026-10-21T00:00:00Z PKG_VERSION=0.1.0 \
      SHA256="$(cut -d' ' -f1 "$tb.sha256")" "$@" bash -e "$T/upload-$ch.sh" ) > "$T/publish.out" 2>&1
}
( cd "$T/ws" && URL=$URL GITHUB_OUTPUT="$T/gh-etag.out" bash -e "$T/etag-dev.sh" ) > "$T/etag.out" 2>&1
JS_BEFORE=$(sed -n 's/^etag=//p' "$T/gh-etag.out" 2>/dev/null)
[ -n "$JS_BEFORE" ] && [ "$JS_BEFORE" != absent ]; expect $? "the ETag step reads the JS latest.json ETag ($JS_BEFORE)"
JS_MD5=$(md5sum "$T/www/dev/latest.json" | cut -d' ' -f1)
: > "$T/put.log"; publish dev v1; rc=$?
expect $rc "upload step (dev, v1) rc=0"
[ $rc = 0 ] || tail -5 "$T/publish.out"
: > "$T/put.log"; publish dev v2; rc=$?
expect $rc "upload step (dev, v2) rc=0"
cat > "$T/put.want" <<W
xorgate-agent-rs-v2.tar.gz|public,max-age=31536000,immutable|application/gzip
xorgate-agent-rs-v2.tar.gz.sha256|public,max-age=31536000,immutable|text/plain
xorgate-agent-rs-latest.tar.gz|no-cache|application/gzip
xorgate-agent-rs-latest.tar.gz.sha256|no-cache|text/plain
install-rs.sh|no-cache|text/x-shellscript
install-rs.sh.sha256|no-cache|text/plain
latest-rs.json|no-cache|application/json
W
diff "$T/put.want" "$T/put.log" > "$T/put.diff"; expect $? "puts: exactly the 7 -rs keys, §6 order, cache/content types"
[ -s "$T/put.diff" ] && cat "$T/put.diff"
[ "$(md5sum "$T/www/dev/latest.json" | cut -d' ' -f1)" = "$JS_MD5" ] && [ "$(cat "$T/www/dev/xorgate-agent-latest.tar.gz")" = "JS tarball" ]
expect $? "the JS keys are byte-unchanged"
ref_heredoc dev v2 2026-10-21T00:00:00Z 0.1.0 xorgate-agent-rs-v2.tar.gz | cmp -s - "$T/www/dev/latest-rs.json"; expect $? "published latest-rs.json == the frozen heredoc bytes"
(cd "$T/www/dev" && sha256sum -c --quiet install-rs.sh.sha256 && sha256sum -c --quiet xorgate-agent-rs-latest.tar.gz.sha256 && sha256sum -c --quiet xorgate-agent-rs-v2.tar.gz.sha256)
expect $? "every published sidecar passes sha256sum -c (the Q3 one-liner's check)"
for bad in "TARBALL=xorgate-agent-v2.tar.gz" "TARBALL=latest.json" "VERSION=v3" "BUILT_AT=x\"y"; do
  rm -rf "$T/www/dev-before"; cp -a "$T/www/dev" "$T/www/dev-before"
  : > "$T/put.log"; publish dev v2 "$bad"; rc=$?
  [ "$rc" != 0 ] && [ ! -s "$T/put.log" ] && diff -r "$T/www/dev-before" "$T/www/dev" >/dev/null
  expect $? "upload step with $bad: refused (rc=$rc), NOTHING written"
done
rm -rf "$T/www/dev-before"
: > "$T/put.log"
# shellcheck disable=SC2016  # the inner script expands its own $1/$2
( cd "$T/ws" && env PATH="$T/stub:$PATH" AWS_LOG="$T/aws.log" PUT_LOG="$T/put.log" BUCKET=fake-dist BUCKET_DIR="$T/www/dev" bash -c '
  set -e; eval "$(sed -n "/^ *put() {/,/^ *}/p" "$1")"; put "$2" latest.json no-cache application/json' _ "$T/upload-dev.sh" "$T/golden.json" ) > "$T/put-js.out" 2>&1; rc=$?
[ "$rc" != 0 ] && [ ! -s "$T/put.log" ] && grep -q "refusing to write key 'latest.json'" "$T/put-js.out"
expect $? "the workflow's own put() refuses latest.json (rc=$rc)"

echo "== 6. dist-check.sh against the local bucket"
"$S/dist-check.sh" "$URL/" v2 --channel dev --js-etag "$JS_BEFORE" > "$T/c-pass.out" 2>&1; rc=$?
sed 's/^/    /' "$T/c-pass.out"
[ "$rc" = 0 ] && ! grep -q '^FAIL' "$T/c-pass.out" && [ "$(grep -c '^ok ' "$T/c-pass.out")" = 14 ]
expect $? "published bucket passes: 14 ok, 0 FAIL (trailing / stripped; rc=$rc)"
"$S/dist-check.sh" "$URL" v2 > /dev/null 2>&1; expect $? "passes without the optional flags"
"$S/dist-check.sh" "$URL" v1 --channel dev > "$T/c-v1.out" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q "^FAIL alias" "$T/c-v1.out" && grep -q "^FAIL latest-rs.json schema: version 'v2' != 'v1'" "$T/c-v1.out" && grep -q "^ok sha256 xorgate-agent-rs-v1.tar.gz" "$T/c-v1.out"
expect $? "an older version (v1, no longer latest) fails: alias + manifest version, its own tarball still verifies"
variant() { rm -rf "$T/www/$1"; cp -a "$T/www/dev" "$T/www/$1"; }
check_fail() { # <variant> <grep -E of the FAIL line> <label> [extra dist-check args]
  local v=$1 re=$2 label=$3; shift 3
  "$S/dist-check.sh" "http://127.0.0.1:$PORT/$v" v2 "$@" > "$T/c-$v.out" 2>&1; local rc=$?
  [ "$rc" = 1 ] && grep -Eq "^FAIL $re" "$T/c-$v.out"; expect $? "$label → rc=1, 'FAIL $re' (rc=$rc)"
  [ "$rc" = 1 ] || sed 's/^/      /' "$T/c-$v.out"
}
variant shabad; sed -i -E 's/^[0-9a-fA-F]{64}/0000000000000000000000000000000000000000000000000000000000000000/' "$T/www/shabad/xorgate-agent-rs-v2.tar.gz.sha256"
check_fail shabad "sha256 xorgate-agent-rs-v2\.tar\.gz: sidecar" "sha mismatch (versioned sidecar)"
variant missing; rm "$T/www/missing/install-rs.sh.sha256"
check_fail missing "http 200 install-rs\.sh\.sha256: got 404" "missing key (install-rs.sh.sha256)"
variant missingtb; rm "$T/www/missingtb/xorgate-agent-rs-v2.tar.gz"
check_fail missingtb "http 200 xorgate-agent-rs-v2\.tar\.gz: got 404" "missing key (versioned tarball)"
variant badjson; head -c 40 "$T/www/dev/latest-rs.json" > "$T/www/badjson/latest-rs.json"
check_fail badjson "latest-rs\.json schema: not JSON" "bad JSON (truncated latest-rs.json)"
variant extra; printf '{ "version": "v2", "channel": "dev", "builtAt": "t", "packageVersion": "0.1.0", "tarball": "xorgate-agent-rs-v2.tar.gz", "sha256": "x" }\n' > "$T/www/extra/latest-rs.json"
check_fail extra "latest-rs\.json schema: keys" "a 6th key in latest-rs.json (Q7)"
variant nonstr; printf '{ "version": "v2", "channel": "dev", "builtAt": 1, "packageVersion": "0.1.0", "tarball": "xorgate-agent-rs-v2.tar.gz" }\n' > "$T/www/nonstr/latest-rs.json"
check_fail nonstr "latest-rs\.json schema: non-string" "a non-string field"
variant wrongtb; printf '{ "version": "v2", "channel": "dev", "builtAt": "t", "packageVersion": "0.1.0", "tarball": "xorgate-agent-rs-latest.tar.gz" }\n' > "$T/www/wrongtb/latest-rs.json"
check_fail wrongtb "latest-rs\.json schema: tarball" "tarball != xorgate-agent-rs-<ver>.tar.gz"
variant inst; printf '\n# tampered\n' >> "$T/www/inst/install-rs.sh"
printf '%s  install-rs.sh\n' "$(sha256sum "$T/www/inst/install-rs.sh" | cut -d' ' -f1)" > "$T/www/inst/install-rs.sh.sha256"
check_fail inst "install-rs\.sh == xorgate-agent-rs-v2\.tar\.gz:install-rs\.sh: the bucket copy differs" "install-rs.sh != the tarball's copy (sidecar consistent)"
grep -q "^ok sha256 install-rs.sh" "$T/c-inst.out"; expect $? "…while its own sidecar still verifies (the tarball check is what catches it)"
variant aliasname; sed -i 's/xorgate-agent-rs-latest.tar.gz/xorgate-agent-rs-v2.tar.gz/' "$T/www/aliasname/xorgate-agent-rs-latest.tar.gz.sha256"
check_fail aliasname "sha256 xorgate-agent-rs-latest\.tar\.gz: xorgate-agent-rs-latest\.tar\.gz\.sha256 has no" "alias sidecar naming the versioned basename"
variant aliasold; cp "$T/pack-v1/xorgate-agent-rs-v1.tar.gz" "$T/www/aliasold/xorgate-agent-rs-latest.tar.gz"
printf '%s  xorgate-agent-rs-latest.tar.gz\n' "$(sha256sum "$T/www/aliasold/xorgate-agent-rs-latest.tar.gz" | cut -d' ' -f1)" > "$T/www/aliasold/xorgate-agent-rs-latest.tar.gz.sha256"
check_fail aliasold "alias xorgate-agent-rs-latest\.tar\.gz == xorgate-agent-rs-v2\.tar\.gz: they differ" "alias pointing at another build"
check_fail dev "latest-rs\.json schema: channel 'dev' != 'prod'" "wrong channel (--channel prod on the dev bucket)" --channel prod
variant jstouch; printf '{ "version": "js-v0.1.3" }\n' > "$T/www/jstouch/latest.json"
check_fail jstouch "JS latest\.json untouched: ETag before" "JS latest.json changed during the run" --js-etag "$JS_BEFORE"
variant jsgone; rm "$T/www/jsgone/latest.json"
check_fail jsgone "JS latest\.json untouched" "JS latest.json deleted during the run" --js-etag "$JS_BEFORE"
"$S/dist-check.sh" --js-etag-of "http://127.0.0.1:$PORT/jsgone" | grep -qx absent; expect $? "--js-etag-of prints 'absent' for a missing latest.json"
"$S/dist-check.sh" "http://127.0.0.1:$PORT/jsgone" v2 --js-etag absent >/dev/null 2>&1; expect $? "--js-etag absent passes when it is still absent"
"$S/dist-check.sh" "http://127.0.0.1:1/x" v2 > "$T/c-down.out" 2>&1; rc=$?
[ "$rc" = 1 ] && [ "$(grep -c '^FAIL http 200' "$T/c-down.out")" = 7 ]; expect $? "bucket unreachable: all 7 http checks FAIL, rc=1"
"$S/dist-check.sh" "$URL" > /dev/null 2>&1; [ $? = 2 ]; expect $? "usage error without <ver> (rc 2)"
"$S/dist-check.sh" "$URL" v2 --channel ci > /dev/null 2>&1; [ $? = 2 ]; expect $? "usage error on --channel ci (rc 2)"

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
