#!/usr/bin/env bash
# Builds and packages the Linux release AppImage on the glibc floor (ADR-0016):
#
#   scripts/build-linux-appimage.sh <arch>
#
# writes flint-v<version>-linux-<arch>.AppImage to the repository root. <arch> is x86_64 or
# aarch64 and must match the host. The binary links the glibc it is built against and linuxdeploy
# bundles libraries from the system it runs on, so both run inside AlmaLinux 9 (glibc 2.34),
# pinned by digest below. The Rust toolchain and the cargo cache are the host's, mounted at the
# same paths; the build runs as the host's user, so they stay owned by it. Its objects go to their
# own target/glibc-<floor>, because cargo would otherwise reuse those of a native build.
set -euo pipefail

# A multi-architecture index: the same digest serves x86_64 and aarch64.
BUILD_IMAGE=almalinux:9@sha256:02df05719ef287ebd0df0a1eedb9172a00bd70703aed7b25660e426b8e0d6047
GLIBC_FLOOR=2.34

repo=$(cd "$(dirname "$0")/.." && pwd)

if [ "${1:-}" = --in-container ]; then
  arch=$2
  uid=$3
  gid=$4
  grep -v '^#' "$repo/packaging/linux/almalinux-build-packages.txt" |
    xargs dnf install -y -q --enablerepo=crb --setopt=install_weak_deps=False
  # shellcheck disable=SC2016 # expanded by the inner shell
  exec setpriv --reuid "$uid" --regid "$gid" --clear-groups env \
    ARCH="$arch" FLINT_GLIBC_FLOOR="$GLIBC_FLOOR" PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH" \
    CARGO_TARGET_DIR="target/glibc-$GLIBC_FLOOR" \
    bash -euo pipefail -c '
      cargo build --release --locked -p flint-app
      # On the ELF binary, before the AppImage compresses it (docs/TESTING.md).
      scripts/check-no-automation.sh "$CARGO_TARGET_DIR/release/flint"
      version=$("$CARGO_TARGET_DIR/release/flint" --version | cut -d" " -f2)
      scripts/package-appimage.sh "$CARGO_TARGET_DIR/release/flint" "$version" "$ARCH"
    '
fi

if [ $# -ne 1 ]; then
  echo "usage: $0 <arch>" >&2
  exit 2
fi
arch=$1
case "$arch" in
  x86_64 | aarch64) ;;
  *)
    echo "error: unsupported arch \"$arch\" (x86_64 or aarch64)" >&2
    exit 2
    ;;
esac

cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
mkdir -p "$repo/target"
docker run --rm \
  -v "$repo:$repo" -w "$repo" \
  -v "$cargo_home:$cargo_home" -v "$rustup_home:$rustup_home" \
  -e HOME="$HOME" -e CARGO_HOME="$cargo_home" -e RUSTUP_HOME="$rustup_home" \
  -e CARGO_INCREMENTAL -e CARGO_TERM_COLOR -e CARGO_BUILD_JOBS \
  "$BUILD_IMAGE" \
  bash scripts/build-linux-appimage.sh --in-container "$arch" "$(id -u)" "$(id -g)"
