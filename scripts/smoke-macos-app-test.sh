#!/usr/bin/env bash
# Self-test for scripts/smoke-macos-app.sh: given a good DMG (from scripts/package-macos-app.sh),
# it builds broken copies and checks that the smoke test fails on each, for the right reason.
#
#   scripts/smoke-macos-app-test.sh <good dmg> <version>
#
# The good DMG itself is checked by running smoke-macos-app.sh on it directly; this covers the
# failures: a wrong bundle identifier, a wrong version, a signature broken after signing, a
# signature with another identifier, and a missing LICENSE.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <good dmg> <version>" >&2
  exit 2
fi
good=$1
version=${2#v}
repo=$(cd "$(dirname "$0")/.." && pwd)

work=$(mktemp -d)
mnt=$work/mnt
mkdir -p "$mnt"
cleanup() {
  hdiutil detach "$mnt" -quiet 2>/dev/null || hdiutil detach "$mnt" -force -quiet 2>/dev/null || true
  rmdir "$mnt" 2>/dev/null && rm -rf "$work"
}
trap cleanup EXIT

hdiutil attach "$good" -readonly -nobrowse -noautoopen -mountpoint "$mnt" -quiet
ditto "$mnt/Flint.app" "$work/Flint.app"
hdiutil detach "$mnt" -quiet

# case_dmg <name> <want> <change...>: copies the good app, runs the change on the copy's path
# (appended as the last argument), packs it as a DMG and expects the smoke test to exit 1 with
# <want> in its error.
case_dmg() {
  local name=$1 want=$2 got=0
  shift 2
  local stage=$work/$name
  mkdir -p "$stage"
  ditto "$work/Flint.app" "$stage/Flint.app"
  ln -s /Applications "$stage/Applications"
  "$@" "$stage/Flint.app"
  hdiutil create -quiet -volname Flint -srcfolder "$stage" -fs HFS+ -format UDRO -ov "$work/$name.dmg"
  "$repo/scripts/smoke-macos-app.sh" "$work/$name.dmg" "$version" >"$work/$name.log" 2>&1 || got=$?
  if [ "$got" != 1 ] || ! grep -qF -- "$want" "$work/$name.log"; then
    echo "error: smoke-macos-app.sh on a DMG with $name exited $got, want 1 with \"$want\":" >&2
    cat "$work/$name.log" >&2
    exit 1
  fi
  echo "$name: fails as expected"
}

set_key() { /usr/libexec/PlistBuddy -c "Set :$1 $2" "$3/Contents/Info.plist"; }
resign() { codesign --force --deep -s - "$1" 2>/dev/null; }

wrong_bundle_id() { set_key CFBundleIdentifier com.example.notflint "$1" && resign "$1"; }
wrong_version() { set_key CFBundleShortVersionString 99.0.0 "$1" && resign "$1"; }
# Changed after signing: the sealed Info.plist no longer matches the signature.
tampered() { set_key CFBundleDisplayName Tampered "$1"; }
other_identifier() { codesign --force -s - --identifier com.example.notflint "$1" 2>/dev/null; }
no_license() { rm "$1/Contents/Resources/LICENSE" && resign "$1"; }

case_dmg wrong-bundle-id "CFBundleIdentifier is \"com.example.notflint\"" wrong_bundle_id
case_dmg wrong-version "CFBundleShortVersionString is \"99.0.0\"" wrong_version
case_dmg tampered-signature "the signature does not verify" tampered
case_dmg other-signature-identifier "the signature's identifier is not se.redstack.flint" other_identifier
case_dmg no-license "no Contents/Resources/LICENSE" no_license
echo "smoke-macos-app.sh: ok"
