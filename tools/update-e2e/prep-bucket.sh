#!/bin/bash
# Build the fake dist bucket (runs INSIDE the e2e image, called by run.sh).
#   in:  /in/xorgate-agent-rs-v1.tar.gz, /in/xorgate-agent-rs-v2.tar.gz (+ .sha256),
#        both made by scripts/pack.sh; /in/jsys/system/* (the JS repo's
#        system/ @ 89b5c64, git archive); /in/x86_64-true (an x86_64 ELF)
#   out: /bucket/<key> — what `python3 -m http.server` serves in every case
# Variants are derived from the v2 tree with GNU tar (root-owned entries, as
# pack.sh writes them) and get a `sha256sum`-format sidecar naming their own
# basename, exactly like device-rs-deploy-*.yml does for `-latest`.
set -euo pipefail
B=/bucket
mkdir -p "$B"
sidecar() { (cd "$B" && sha256sum "$1" > "$1.sha256"); }

# v1 + v2: pack.sh's tarballs. On a Mac pack.sh runs bsdtar, which records the
# packing user as owner and Apple provenance xattrs; CI packs on Linux with
# GNU tar --owner=0 --group=0 (pack.sh:95-98). Re-pack the SAME members with
# GNU tar so the bucket holds what CI publishes (content byte-identical,
# checked below), then write fresh sidecars.
R=$(mktemp -d)
for v in v1 v2; do
  (cd /in && sha256sum -c "xorgate-agent-rs-$v.tar.gz.sha256" >/dev/null) || { echo "pack sidecar mismatch for $v" >&2; exit 1; }
  mkdir "$R/$v"
  tar -xzf "/in/xorgate-agent-rs-$v.tar.gz" -C "$R/$v" --warning=no-unknown-keyword --no-same-owner
  (cd "$R/$v" && tar -czf "$B/xorgate-agent-rs-$v.tar.gz" --owner=0 --group=0 --numeric-owner $(ls -A))
  mkdir "$R/$v.check"
  tar -xzf "$B/xorgate-agent-rs-$v.tar.gz" -C "$R/$v.check"
  diff -r "$R/$v" "$R/$v.check" >/dev/null || { echo "repack of $v changed content" >&2; exit 1; }
  sidecar "xorgate-agent-rs-$v.tar.gz"
done
rm -rf "$R"

# `latest` alias = v2 bytes, sidecar re-written for the alias name (CI does this).
cp "$B/xorgate-agent-rs-v2.tar.gz" "$B/xorgate-agent-rs-latest.tar.gz"
sidecar xorgate-agent-rs-latest.tar.gz

T=$(mktemp -d)
mkdir "$T/v2"
tar -xzf "$B/xorgate-agent-rs-v2.tar.gz" -C "$T/v2"

# variant <name> <edit-fn>: copy the v2 tree, apply the edit, pack, sidecar.
variant() {
  local name=$1 fn=$2 d="$T/$1"
  cp -a "$T/v2" "$d"
  "$fn" "$d"
  sed -i "s/\"version\": *\"[^\"]*\"/\"version\": \"$name\"/" "$d/build-info.json"
  (cd "$d" && tar -czf "$B/xorgate-agent-rs-$name.tar.gz" --owner=0 --group=0 --numeric-owner $(ls -A))
}
add_system() { rm -rf "$1/system"; cp -a /in/jsys/system "$1/system"; }
wrong_arch() { install -m 0755 /in/x86_64-true "$1/bin/xorgate-rs"; }
truncate_bin() { head -c 1048576 "$1/bin/xorgate-rs" > "$1/bin/xorgate-rs.t"; mv "$1/bin/xorgate-rs.t" "$1/bin/xorgate-rs"; chmod 0755 "$1/bin/xorgate-rs"; }
same() { :; }

variant v2sys add_system;      sidecar xorgate-agent-rs-v2sys.tar.gz
variant wrongarch wrong_arch;  sidecar xorgate-agent-rs-wrongarch.tar.gz
variant truncated truncate_bin; sidecar xorgate-agent-rs-truncated.tar.gz
variant nosum same             # no sidecar published
variant badsha same
# a well-formed sidecar whose digest is v2's, not badsha's
printf '%s  %s\n' "$(cut -d' ' -f1 "$B/xorgate-agent-rs-v2.tar.gz.sha256")" xorgate-agent-rs-badsha.tar.gz > "$B/xorgate-agent-rs-badsha.tar.gz.sha256"
variant wrongname same
# right digest, but the sidecar names another file
printf '%s  %s\n' "$(sha256sum "$B/xorgate-agent-rs-wrongname.tar.gz" | cut -d' ' -f1)" xorgate-agent-rs-v2.tar.gz > "$B/xorgate-agent-rs-wrongname.tar.gz.sha256"
# missing member: build-info.json removed AFTER the version stamp
variant nomember same
mkdir "$T/nm" && tar -xzf "$B/xorgate-agent-rs-nomember.tar.gz" -C "$T/nm" && rm "$T/nm/build-info.json"
(cd "$T/nm" && tar -czf "$B/xorgate-agent-rs-nomember.tar.gz" --owner=0 --group=0 --numeric-owner $(ls -A))
sidecar xorgate-agent-rs-nomember.tar.gz
# not a tarball at all
echo "this is not gzip" > "$B/xorgate-agent-rs-garbage.tar.gz"; sidecar xorgate-agent-rs-garbage.tar.gz
rm -rf "$T"
(cd "$B" && ls -l)
