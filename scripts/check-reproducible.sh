#!/usr/bin/env bash
# Build the driver twice, from two copies of the checkout in different
# directories with different CARGO_HOMEs, and check the two images are
# byte-identical (#12). On a mismatch, print what differs: the PE header
# fields and the strings found in only one image.
#
#   sc-build scripts/check-reproducible.sh
set -euo pipefail
src="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/repro.XXXXXX")"
trap 'rm -rf "$work"' EXIT
rustc --version; cargo --version

build() { # $1: a name; prints the image's path
    local dir="$work/$1/stormnic-ixgbe"
    mkdir -p "$dir"
    git -C "$src" archive HEAD | tar -x -C "$dir"
    # A fresh CARGO_HOME at a different path, seeded from the normal one so
    # no download is needed; the registry sources then sit at another path.
    local home="$work/$1/cargo-home"
    mkdir -p "$home"
    [[ -d "${CARGO_HOME:-$HOME/.cargo}/registry" ]] && cp -r "${CARGO_HOME:-$HOME/.cargo}/registry" "$home/"
    ( cd "$dir" && CARGO_HOME="$home" CARGO_TARGET_DIR="$dir/target" \
        cargo build -q --locked --release --target x86_64-unknown-uefi ) >&2
    echo "$dir/target/x86_64-unknown-uefi/release/stormnic-ixgbe.efi"
}

a="$(build one)"
b="$(build second-build-dir)"
python3 - "$a" "$b" <<'PY'
import hashlib, re, struct, sys
imgs = [open(p, "rb").read() for p in sys.argv[1:3]]
for p, d in zip(sys.argv[1:3], imgs):
    print(f"{p}: {len(d)} bytes, sha256 {hashlib.sha256(d).hexdigest()}")
if imgs[0] == imgs[1]:
    print("OK: byte-identical across build directories and CARGO_HOMEs")
    sys.exit(0)
def header(d):
    pe = struct.unpack_from("<I", d, 0x3c)[0]
    return {"TimeDateStamp": hex(struct.unpack_from("<I", d, pe + 8)[0]),
            "CheckSum": hex(struct.unpack_from("<I", d, pe + 24 + 64)[0])}
for d in imgs: print(" ", header(d))
if len(imgs[0]) == len(imgs[1]):
    diff = [i for i in range(len(imgs[0])) if imgs[0][i] != imgs[1][i]]
    print(f"  {len(diff)} bytes differ, first at {diff[:16]}")
strs = [set(re.findall(rb"[\x20-\x7e]{6,}", d)) for d in imgs]
for n, (mine, other) in enumerate([(strs[0], strs[1]), (strs[1], strs[0])]):
    only = sorted(mine - other)
    print(f"  strings only in image {n + 1} ({len(only)}):")
    for s in only[:40]: print("   ", s.decode())
sys.exit("FAIL: the images differ")
PY
