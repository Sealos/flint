#!/usr/bin/env bash
# Builds and packages the GPUI-free MCP server, `flint-mcp`, as a static musl binary:
#
#   scripts/package-mcp-linux.sh <arch> <version>
#
# writes flint-mcp-v<version>-<arch>-unknown-linux-musl.tar.gz and its .sha256 to the current
# directory, and prints the archive's name last. <version> is X.Y.Z (a leading "v" is accepted)
# and must be the workspace version; <arch> is x86_64 or aarch64 and must be the host's: the
# build is native, with the host's musl-gcc (Debian and Ubuntu: musl-tools) and the Rust target
# <arch>-unknown-linux-musl installed. The archive holds flint-mcp/flint-mcp, LICENSE and
# README.md.
#
# Static means no system library at run time, not even libc: the binary runs on any Linux of its
# architecture, however old its glibc. rustls with ring is the only C code in its tree; musl-gcc
# compiles it.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <arch> <version>" >&2
  exit 2
fi
arch=$1
version=${2#v}
case "$arch" in
  x86_64 | aarch64) ;;
  *)
    echo "error: arch must be x86_64 or aarch64, not \"$arch\"" >&2
    exit 2
    ;;
esac
if [ "$(uname -m)" != "$arch" ]; then
  echo "error: build $arch on an $arch host, not $(uname -m)" >&2
  exit 2
fi
target=$arch-unknown-linux-musl
repo=$(cd "$(dirname "$0")/.." && pwd)
out=$PWD

# cc-rs looks for a cross compiler named <arch>-linux-musl-gcc for aarch64; the native musl-gcc
# is the one to use on both architectures.
export "CC_${target//-/_}=${CC_MUSL:-musl-gcc}"
(cd "$repo" && cargo build --release --locked -p flint-mcp --bin flint-mcp --target "$target")
binary=${CARGO_TARGET_DIR:-$repo/target}/$target/release/flint-mcp

# No program interpreter and no shared libraries: what makes it run without the host's libc.
headers=$(readelf -lW "$binary")
if grep -q 'Requesting program interpreter' <<<"$headers"; then
  echo "error: $binary asks for a program interpreter; it is not static" >&2
  exit 1
fi
if readelf -dW "$binary" | grep -q '(NEEDED)'; then
  echo "error: $binary links shared libraries:" >&2
  readelf -dW "$binary" | grep '(NEEDED)' >&2
  exit 1
fi
echo "static: no interpreter, no shared libraries"

got=$("$binary" --version)
if [ "$got" != "flint-mcp $version" ]; then
  echo "error: --version printed \"$got\", want \"flint-mcp $version\"" >&2
  exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/flint-mcp"
cp "$binary" "$repo/LICENSE" "$repo/README.md" "$stage/flint-mcp/"
archive=flint-mcp-v$version-$target.tar.gz
tar -czf "$out/$archive" -C "$stage" flint-mcp
(cd "$out" && sha256sum "$archive" >"$archive.sha256")
echo "$out/$archive"
