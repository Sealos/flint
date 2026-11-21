#!/usr/bin/env bash
# Packages a Linux release binary of Flint as an AppImage (ADR-0016):
#
#   scripts/package-appimage.sh <binary> <version> <arch>
#
# writes flint-v<version>-linux-<arch>.AppImage to the current directory. <version> is X.Y.Z
# (a leading "v" is accepted), <arch> is x86_64 or aarch64 and must match the binary and the
# host: linuxdeploy inspects the binary with the host's loader.
#
# linuxdeploy copies the shared libraries the binary links that a desktop may lack (its
# excludelist keeps glibc, GL, fontconfig and the like out; Vulkan is loaded at run time and never
# bundled), then appimagetool wraps the AppDir with the AppImage runtime. All three tools are
# official releases from the linuxdeploy and AppImage GitHub organizations, pinned by version and
# SHA-256 below, downloaded once into $FLINT_APPIMAGE_TOOLS (default target/appimage-tools).
#
# The tools are AppImages themselves; APPIMAGE_EXTRACT_AND_RUN=1 runs them without FUSE (CI
# runners and containers have none).
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "usage: $0 <binary> <version> <arch>" >&2
  exit 2
fi
binary=$1
version=${2#v}
arch=$3

# Pinned tools. Bump a version together with its checksums (from the release's asset digests).
LINUXDEPLOY_VERSION=1-alpha-20251107-1
APPIMAGETOOL_VERSION=1.9.1
RUNTIME_VERSION=20251108
# packaging/linux/appimage-runtime-LICENSE is the LICENSE file at that runtime tag. Refresh it
# with the runtime: gh api "repos/AppImage/type2-runtime/contents/LICENSE?ref=<tag>" --jq .content | base64 -d
RUNTIME_LICENSE_SHA256=aa154fc9070614bbe7921f89db11efd1dba7a1f3a41685958110e2230f9c0ca1
case "$arch" in
  x86_64)
    LINUXDEPLOY_SHA256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
    APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    ;;
  aarch64)
    LINUXDEPLOY_SHA256=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff
    APPIMAGETOOL_SHA256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
    ;;
  *)
    echo "error: unsupported arch \"$arch\" (x86_64 or aarch64)" >&2
    exit 2
    ;;
esac

if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != "$arch" ]; then
  echo "error: run this on Linux $arch (this host is $(uname -s) $(uname -m))" >&2
  exit 2
fi
if [ ! -x "$binary" ]; then
  echo "error: $binary is not an executable file" >&2
  exit 2
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
binary=$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")
tools=${FLINT_APPIMAGE_TOOLS:-$repo/target/appimage-tools}
out=$PWD/flint-v$version-linux-$arch.AppImage
mkdir -p "$tools"

# fetch <url> <file name> <sha256>: downloads into $tools unless a verified copy is there.
fetch() {
  local url=$1 dest=$tools/$2 sum=$3
  if [ -f "$dest" ] && echo "$sum  $dest" | sha256sum --check --status; then
    return
  fi
  rm -f "$dest"
  curl --fail --location --silent --show-error --retry 3 --proto '=https' --tlsv1.2 \
    --output "$dest.part" "$url"
  if ! echo "$sum  $dest.part" | sha256sum --check --status; then
    echo "error: $url does not match its pinned SHA-256" >&2
    rm -f "$dest.part"
    exit 1
  fi
  mv "$dest.part" "$dest"
  chmod +x "$dest"
}

fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_VERSION/linuxdeploy-$arch.AppImage" \
  "linuxdeploy-$LINUXDEPLOY_VERSION-$arch.AppImage" "$LINUXDEPLOY_SHA256"
fetch "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-$arch.AppImage" \
  "appimagetool-$APPIMAGETOOL_VERSION-$arch.AppImage" "$APPIMAGETOOL_SHA256"
fetch "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$arch" \
  "runtime-$RUNTIME_VERSION-$arch" "$RUNTIME_SHA256"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
appdir=$work/Flint.AppDir

export APPIMAGE_EXTRACT_AND_RUN=1
# Never let a tool phone home for updates or embed update information we do not serve.
unset UPDATE_INFORMATION LDAI_UPDATE_INFORMATION

# linuxdeploy names the AppRun after the desktop file's Exec (`flint`) and links it to the
# binary, so `./Flint.AppImage mcp` reaches `flint mcp` with its arguments unchanged.
"$tools/linuxdeploy-$LINUXDEPLOY_VERSION-$arch.AppImage" \
  --appdir "$appdir" \
  --executable "$binary" \
  --desktop-file "$repo/packaging/linux/flint.desktop" \
  --icon-file "$repo/packaging/linux/flint.png" \
  --icon-filename flint
install -Dm644 "$repo/LICENSE" "$appdir/usr/share/doc/flint/LICENSE"
# The Lucide icons compiled into the binary (crates/flint-app/src/ui/icons.rs) are ISC: their
# notice travels with every AppImage.
install -Dm644 "$repo/crates/flint-app/assets/icons/LICENSE-LUCIDE" "$appdir/usr/share/doc/flint/LICENSE-LUCIDE"
# The runtime is MIT: its notice must travel with every AppImage. linuxdeploy already copies the
# bundled libraries' copyright files into usr/share/doc.
runtime_license=$repo/packaging/linux/appimage-runtime-LICENSE
if ! echo "$RUNTIME_LICENSE_SHA256  $runtime_license" | sha256sum --check --status; then
  echo "error: $runtime_license is not the LICENSE of type2-runtime $RUNTIME_VERSION" >&2
  exit 1
fi
install -Dm644 "$runtime_license" "$appdir/usr/share/doc/appimage-runtime/LICENSE"

# The binary must be called flint: the desktop file's Exec and the AppRun link point at it.
test -x "$appdir/usr/bin/flint"

# With FLINT_GLIBC_FLOOR set (scripts/build-linux-appimage.sh), no ELF file in the AppDir may ask
# for a newer glibc symbol version: a single one would stop the AppImage on the oldest systems.
if [ -n "${FLINT_GLIBC_FLOOR:-}" ]; then
  newest=$(find "$appdir" -type f -exec sh -c 'head -c4 "$1" | grep -q "^.ELF" && readelf -W --version-info "$1"' _ {} \; |
    grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -Vu | tail -n 1)
  if [ -z "$newest" ]; then
    echo "error: found no glibc symbol versions in $appdir" >&2
    exit 1
  fi
  if [ "$(printf '%s\n%s\n' "$newest" "$FLINT_GLIBC_FLOOR" | sort -V | tail -n 1)" != "$FLINT_GLIBC_FLOOR" ]; then
    echo "error: the AppDir needs glibc $newest, above the floor $FLINT_GLIBC_FLOOR" >&2
    exit 1
  fi
  echo "glibc: newest symbol version $newest (floor $FLINT_GLIBC_FLOOR)"
fi

rm -f "$out"
ARCH=$arch VERSION=$version "$tools/appimagetool-$APPIMAGETOOL_VERSION-$arch.AppImage" \
  --no-appstream \
  --runtime-file "$tools/runtime-$RUNTIME_VERSION-$arch" \
  "$appdir" "$out"

echo "bundled libraries:"
find "$appdir/usr/lib" -maxdepth 1 -name '*.so*' -printf '  %f\n' 2>/dev/null | sort || true
# What the host must provide: the binary's direct needs that were not bundled (README lists the
# packages; CI checks them in a clean container).
echo "libraries left to the host:"
readelf -d "$appdir/usr/bin/flint" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | sort | while read -r lib; do
  [ -e "$appdir/usr/lib/$lib" ] || echo "  $lib"
done
ls -l "$out"
