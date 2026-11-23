#!/usr/bin/env bash
# Packages a macOS release binary of Flint as Flint.app in a drag-to-Applications DMG (#175):
#
#   scripts/package-macos-app.sh <binary> <version> <out-dir>
#
# writes <out-dir>/Flint.app and <out-dir>/flint-v<version>-macos-<arch>.dmg, where <arch> is
# "universal" for a binary with both arm64 and x86_64 slices (what releases ship) and the single
# architecture otherwise. <version> is X.Y.Z (a leading "v" is accepted). Prints the DMG's path
# on the last line of its output.
#
# The bundle is ad-hoc signed (`codesign -s -`), not Developer ID signed or notarized: that waits
# on the Apple Developer account (#25). Signing later replaces the ad-hoc signature at the same
# bundle identifier. The Info.plist comes from packaging/macos/Info.plist, the icon from
# assets/brand/Flint.icns, and LICENSE and LICENSE-LUCIDE (for the Lucide icons compiled into the binary) go
# into Contents/Resources. scripts/smoke-macos-app.sh checks the DMG.
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "usage: $0 <binary> <version> <out-dir>" >&2
  exit 2
fi
binary=$1
version=${2#v}
out_dir=$3

if [ "$(uname -s)" != Darwin ]; then
  echo "error: run this on macOS (this host is $(uname -s))" >&2
  exit 2
fi
if [ ! -x "$binary" ] || [ -d "$binary" ]; then
  echo "error: $binary is not an executable file" >&2
  exit 2
fi
# CFBundleVersion must be numbers separated by dots.
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: version \"$version\" is not X.Y.Z" >&2
  exit 2
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)

archs=$(lipo -archs "$binary")
if [ "$(tr ' ' '\n' <<<"$archs" | sort | tr '\n' ' ')" = "arm64 x86_64 " ]; then
  arch=universal
else
  arch=${archs// /-}
fi
app=$out_dir/Flint.app
dmg=$out_dir/flint-v$version-macos-$arch.dmg

# The bundle.
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$binary" "$app/Contents/MacOS/flint"
install -m 644 "$repo/assets/brand/Flint.icns" "$app/Contents/Resources/Flint.icns"
install -m 644 "$repo/LICENSE" "$app/Contents/Resources/LICENSE"
# The Lucide icons compiled into the binary (crates/flint-app/src/ui/icons.rs) are ISC: their
# notice travels with the app.
install -m 644 "$repo/crates/flint-app/assets/icons/LICENSE-LUCIDE" "$app/Contents/Resources/LICENSE-LUCIDE"
# Release builds set THIRD_PARTY_NOTICES to the output of scripts/generate-notices.sh.
if [ -n "${THIRD_PARTY_NOTICES:-}" ]; then
  [ -s "$THIRD_PARTY_NOTICES" ] || { echo "error: no notices at $THIRD_PARTY_NOTICES" >&2; exit 1; }
  install -m 644 "$THIRD_PARTY_NOTICES" "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt"
  [ -s "${RUST_LIBRARY_NOTICES:-}" ] || { echo "error: RUST_LIBRARY_NOTICES names no file" >&2; exit 1; }
  install -m 644 "$RUST_LIBRARY_NOTICES" "$app/Contents/Resources/COPYRIGHT-library.html"
fi
sed "s/@VERSION@/$version/g" "$repo/packaging/macos/Info.plist" >"$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"
plutil -lint "$app/Contents/Info.plist"
if grep -q '@VERSION@' "$app/Contents/Info.plist"; then
  echo "error: Info.plist still has a @VERSION@ placeholder" >&2
  exit 1
fi

# Ad-hoc signature: Apple Silicon refuses to run unsigned code, and the bundle's signature seals
# Info.plist and the resources with the executable.
codesign --force --deep -s - "$app"
codesign --verify --deep --strict --verbose=2 "$app"

# The DMG: Flint.app beside an Applications link, so it installs by dragging one onto the other.
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
# ditto keeps the signature's extended attributes and symlinks intact.
ditto "$app" "$stage/Flint.app"
ln -s /Applications "$stage/Applications"
rm -f "$dmg"
# hdiutil sometimes fails with "Resource busy" on CI runners; a retry gets through.
for attempt in 1 2 3; do
  if hdiutil create -volname Flint -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"; then
    break
  fi
  if [ "$attempt" = 3 ]; then
    echo "error: hdiutil create failed three times" >&2
    exit 1
  fi
  sleep 5
done
hdiutil verify "$dmg"
ls -l "$dmg"
echo "$dmg"
