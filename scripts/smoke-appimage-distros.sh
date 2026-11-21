#!/usr/bin/env bash
# Runs scripts/smoke-appimage.sh in a clean container of each supported distribution, with only
# the host packages the README documents (ADR-0016):
#
#   scripts/smoke-appimage-distros.sh <AppImage> <version>
#
# <AppImage> must be inside the repository, which the containers mount read-only. AlmaLinux 9
# carries the glibc floor (2.34). Images are pinned by the digest of their multi-architecture
# index, so the same line serves x86_64 and aarch64.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <AppImage> <version>" >&2
  exit 2
fi
repo=$(cd "$(dirname "$0")/.." && pwd)
appimage=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
case "$appimage" in
  "$repo"/*) appimage=${appimage#"$repo"/} ;;
  *)
    echo "error: $1 is not inside $repo" >&2
    exit 2
    ;;
esac
version=$2

APT='apt-get update -qq &&
  grep -v "^#" packaging/linux/ubuntu-runtime-packages.txt |
    xargs apt-get install -y -qq --no-install-recommends >/dev/null'
DNF='grep -v "^#" packaging/linux/rhel-runtime-packages.txt |
  xargs dnf install -y -q --setopt=install_weak_deps=False >/dev/null'

# smoke <label> <image> <install command>
smoke() {
  echo "== $1"
  docker run --rm -v "$repo:/w:ro" -w /w -e APPIMAGE="$appimage" -e VERSION="$version" \
    -e DEBIAN_FRONTEND=noninteractive "$2" \
    bash -c "set -euo pipefail; $3; ldd --version | sed -n 1p; scripts/smoke-appimage.sh \"\$APPIMAGE\" \"\$VERSION\""
}

smoke "AlmaLinux 9 (glibc 2.34)" \
  almalinux:9@sha256:02df05719ef287ebd0df0a1eedb9172a00bd70703aed7b25660e426b8e0d6047 "$DNF"
smoke "Ubuntu 22.04 (glibc 2.35)" \
  ubuntu:22.04@sha256:f64b2ffc2f10ab8f8505aa56b2664bd846cbb9df87c7e2532e30f7667021c0e6 "$APT"
smoke "Debian 12 (glibc 2.36)" \
  debian:12@sha256:268fb24c39bca92d326dd83ea3abebe4a3f7751c804ffef23bb6d857dd343ff8 "$APT"
smoke "Ubuntu 24.04 (glibc 2.39)" \
  ubuntu:24.04@sha256:73b999393d07f2bf65a40373778a44dc9a6d54ad7718d29247d8ff9413c4d6a8 "$APT"
