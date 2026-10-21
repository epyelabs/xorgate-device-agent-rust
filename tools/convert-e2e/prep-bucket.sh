#!/bin/bash
# Build the fake dist bucket (INSIDE the convert-e2e image, called by run.sh).
#   in:  /in/xorgate-agent-rs-v{1,2}.tar.gz (+ .sha256) from scripts/pack.sh,
#        /in/install-rs.sh (INSTALL_RS), /in/x86_64-true (an x86_64 ELF)
#   out: /bucket/<key>; every case serves a writable COPY of it on
#        127.0.0.1:8000 (case.sh), so a case can flip `-latest` or stop it.
# Keys (phase-5-distribution.md §6): xorgate-agent-rs-<v>.tar.gz + .sha256,
# the -latest alias (= v1; case.sh flips it to v2) with a sidecar naming the
# alias basename, install-rs.sh + install-rs.sh.sha256. Variants (from the v1
# tree, version-stamped with their own name): badsha, nosum (no sidecar),
# nomember-{bin,unit,buildinfo,installrs,updater} (each of the 5 required
# members, I1 step 3), wrongname (right digest, the sidecar names ANOTHER
# file), wrongarch (x86_64 binary), runfails
# (a shell stub that answers -v like the real binary and exits 1 on `run`).
set -euo pipefail
B=/bucket
mkdir -p "$B"
sidecar() { (cd "$B" && sha256sum "$1" >"$1.sha256"); }
pack_dir() { # <dir> <tarball>: the dir's top-level members, root-owned (as CI packs)
  local m
  mapfile -t m < <(cd "$1" && find . -mindepth 1 -maxdepth 1 -printf '%P\n' | LC_ALL=C sort)
  (cd "$1" && tar -czf "$B/$2" --owner=0 --group=0 --numeric-owner "${m[@]}")
}

# v1 + v2: pack.sh's tarballs, re-packed with GNU tar --owner=0 (what CI
# publishes; pack.sh on a Mac runs bsdtar). INSTALL_RS is the install-rs.sh
# under test: a tarball whose root lacks it (pack.sh before WP1) gets it
# injected, one whose copy differs gets it replaced — both reported, so a run
# never tests a different converter than the bucket's.
R=$(mktemp -d)
for v in v1 v2; do
  (cd /in && sha256sum -c "xorgate-agent-rs-$v.tar.gz.sha256" >/dev/null) || { echo "pack sidecar mismatch for $v" >&2; exit 1; }
  mkdir "$R/$v"
  tar -xzf "/in/xorgate-agent-rs-$v.tar.gz" -C "$R/$v" --warning=no-unknown-keyword --no-same-owner
  if [ ! -e "$R/$v/install-rs.sh" ]; then
    echo "NOTE: $v tarball has no install-rs.sh (pack.sh predates WP1): INJECTED INSTALL_RS"
    install -m 0755 /in/install-rs.sh "$R/$v/install-rs.sh"
  elif ! cmp -s "$R/$v/install-rs.sh" /in/install-rs.sh; then
    echo "NOTE: $v tarball's install-rs.sh differs from INSTALL_RS: REPLACED"
    install -m 0755 /in/install-rs.sh "$R/$v/install-rs.sh"
  else
    [ "$(stat -c %a "$R/$v/install-rs.sh")" = 755 ] || { echo "packed install-rs.sh is not 0755" >&2; exit 1; }
    echo "ok: $v tarball ships install-rs.sh == INSTALL_RS (0755)"
  fi
  pack_dir "$R/$v" "xorgate-agent-rs-$v.tar.gz"
  sidecar "xorgate-agent-rs-$v.tar.gz"
done

# -latest = v1 at convert time.
cp "$B/xorgate-agent-rs-v1.tar.gz" "$B/xorgate-agent-rs-latest.tar.gz"
sidecar xorgate-agent-rs-latest.tar.gz

# The bootstrap key (§6): install-rs.sh + sidecar.
install -m 0644 /in/install-rs.sh "$B/install-rs.sh"
sidecar install-rs.sh

variant() { # <name> <edit-fn>: v1 tree + edit + version stamp, packed (no sidecar)
  local d="$R/$1"
  cp -a "$R/v1" "$d"
  "$2" "$d"
  [ -f "$d/build-info.json" ] && sed -i "s/\"version\": *\"[^\"]*\"/\"version\": \"$1\"/" "$d/build-info.json"
  pack_dir "$d" "xorgate-agent-rs-$1.tar.gz"
}
same() { :; }
wrong_arch() { install -m 0755 /in/x86_64-true "$1/bin/xorgate-rs"; }
no_buildinfo() { rm "$1/build-info.json"; }
no_bin() { rm "$1/bin/xorgate-rs"; }
no_unit() { rm "$1/systemd/xorgate-agent-rs.service"; }
no_installrs() { rm "$1/install-rs.sh"; }
no_updater() { rm "$1/systemd/xorgate-updater-rs.service"; }
run_fails() {
  cat >"$1/bin/xorgate-rs" <<'EOF'
#!/bin/sh
# convert-e2e `runfails`: passes the smoke-exec, dies on `run`.
case "${1:-}" in
  -v|--version) echo "xorgate-rs 0.0.1 (runfails, dev)"; exit 0 ;;
  run) echo "runfails stub: exiting 1 on run" >&2; exit 1 ;;
  *) exit 0 ;;
esac
EOF
  chmod 0755 "$1/bin/xorgate-rs"
}

variant badsha same
# a well-formed sidecar whose digest is v1's, not badsha's
printf '%s  %s\n' "$(cut -d' ' -f1 "$B/xorgate-agent-rs-v1.tar.gz.sha256")" xorgate-agent-rs-badsha.tar.gz >"$B/xorgate-agent-rs-badsha.tar.gz.sha256"
variant nosum same
variant nomember-bin no_bin;               sidecar xorgate-agent-rs-nomember-bin.tar.gz
variant nomember-unit no_unit;             sidecar xorgate-agent-rs-nomember-unit.tar.gz
variant nomember-buildinfo no_buildinfo;  sidecar xorgate-agent-rs-nomember-buildinfo.tar.gz
variant wrongname same
# the right digest, but the sidecar names another file (sha256sum -c would
# check xorgate-agent-rs-v1.tar.gz instead)
printf '%s  %s\n' "$(sha256sum "$B/xorgate-agent-rs-wrongname.tar.gz" | cut -d' ' -f1)" xorgate-agent-rs-v1.tar.gz >"$B/xorgate-agent-rs-wrongname.tar.gz.sha256"
variant nomember-installrs no_installrs;  sidecar xorgate-agent-rs-nomember-installrs.tar.gz
variant nomember-updater no_updater;      sidecar xorgate-agent-rs-nomember-updater.tar.gz
variant wrongarch wrong_arch;             sidecar xorgate-agent-rs-wrongarch.tar.gz
variant runfails run_fails;               sidecar xorgate-agent-rs-runfails.tar.gz
rm -rf "$R"
(cd "$B" && ls -l)
