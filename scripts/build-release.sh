#!/bin/sh
# build-release.sh: build one exnode release binary, reproducibly.
#
#   sh scripts/build-release.sh <target> <out-dir>
#
# The release workflow and ex_release_rebuild (scripts/release-check.fish) both
# call this, so a host who rebuilds a tag gets the same bytes the workflow
# signed.  It needs the toolchain in rust-toolchain.toml, the target installed,
# and cargo-zigbuild + zig on PATH at the versions in scripts/zig-requirements.txt.
#
# Reproducibility: the binary must not depend on where the source or the cargo
# registry sits.  rustc embeds both paths (panic locations), so remap them to
# fixed prefixes.  Everything else (toolchain, zig, Cargo.lock, codegen) is
# pinned by the files above, and strip = true drops the build id.
set -eu

target=${1:?usage: build-release.sh <target> <out-dir>}
out=${2:?usage: build-release.sh <target> <out-dir>}
case $target in
    x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) ;;
    *) echo "build-release.sh: unsupported target $target" >&2; exit 1 ;;
esac

src=$(cd "$(dirname "$0")/.." && pwd -P)
cargo_home=$(cd "${CARGO_HOME:-$HOME/.cargo}" && pwd -P)

RUSTFLAGS="--remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$src=/exnode"
export RUSTFLAGS
unset CARGO_ENCODED_RUSTFLAGS RUSTC_WRAPPER

cd "$src"
cargo zigbuild --release --locked -p exnode --target "$target"

bin="target/$target/release/exnode"
if strings "$bin" | grep -qF -e "$cargo_home" -e "$src/"; then
    echo "build-release.sh: $bin still embeds a local path; not reproducible" >&2
    exit 1
fi
file "$bin" | grep -Eq 'statically linked|static-pie linked' \
    || { echo "build-release.sh: $bin is not statically linked" >&2; exit 1; }
mkdir -p "$out"
cp "$bin" "$out/exnode-$target"
chmod 0755 "$out/exnode-$target"
