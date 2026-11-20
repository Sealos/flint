#!/usr/bin/env bash
# Checks a packaged Flint DMG without a display or a cluster:
#
#   scripts/smoke-macos-app.sh <dmg> <version>
#
# Mounts the DMG read-only at a private mount point and checks that it holds Flint.app and an
# Applications link; that the bundle has the bundle identifier se.redstack.flint and <version>;
# that its signature verifies; that `Flint.app/Contents/MacOS/flint --version` prints
# "flint <version>" and `flint mcp` answers `initialize` (the README registers that path with MCP
# clients); and that the bundled binary has no automation channel. Detaches the DMG on exit.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <dmg> <version>" >&2
  exit 2
fi
dmg=$1
version=${2#v}
repo=$(cd "$(dirname "$0")/.." && pwd)
if [ ! -f "$dmg" ]; then
  echo "error: $dmg does not exist" >&2
  exit 2
fi

scratch=$(mktemp -d)
mnt=$scratch/mnt
mkdir -p "$mnt"
cleanup() {
  # Detach fails harmlessly when the DMG never attached. The scratch directory goes only once the
  # mount point is empty, so rm never walks into a DMG that is still attached.
  hdiutil detach "$mnt" -quiet 2>/dev/null || hdiutil detach "$mnt" -force -quiet 2>/dev/null || true
  rmdir "$mnt" 2>/dev/null && rm -rf "$scratch"
}
trap cleanup EXIT

hdiutil attach "$dmg" -readonly -nobrowse -noautoopen -mountpoint "$mnt" -quiet
app=$mnt/Flint.app
exe=$app/Contents/MacOS/flint

fail() {
  echo "error: $*" >&2
  exit 1
}

[ -d "$app" ] || fail "the DMG has no Flint.app"
[ "$(readlink "$mnt/Applications")" = /Applications ] || fail "the DMG has no Applications link"
[ -x "$exe" ] || fail "Flint.app has no executable Contents/MacOS/flint"
[ -f "$app/Contents/Resources/Flint.icns" ] || fail "Flint.app has no Contents/Resources/Flint.icns"

plist=$app/Contents/Info.plist
plutil -lint "$plist"
read_key() { /usr/libexec/PlistBuddy -c "Print :$1" "$plist"; }
[ "$(read_key CFBundleIdentifier)" = se.redstack.flint ] ||
  fail "CFBundleIdentifier is \"$(read_key CFBundleIdentifier)\", want se.redstack.flint"
[ "$(read_key CFBundleExecutable)" = flint ] || fail "CFBundleExecutable is not flint"
for key in CFBundleShortVersionString CFBundleVersion; do
  [ "$(read_key "$key")" = "$version" ] || fail "$key is \"$(read_key "$key")\", want $version"
done
echo "bundle: se.redstack.flint $version"

codesign --verify --deep --strict --verbose=2 "$app"
signature=$(codesign -dv "$app" 2>&1)
grep -qx 'Identifier=se.redstack.flint' <<<"$signature" || fail "the signature's identifier is not se.redstack.flint"
grep -qx 'Signature=adhoc' <<<"$signature" || fail "Flint.app is not ad-hoc signed"
echo "signature: ad-hoc, se.redstack.flint"

got=$("$exe" --version)
[ "$got" = "flint $version" ] || fail "--version printed \"$got\", want \"flint $version\""
echo "--version: $got"

# On the executable itself: the DMG is compressed, so its strings cannot be searched.
"$repo/scripts/check-no-automation.sh" "$exe"

mkdir -p "$scratch/config"
# A fixed PATH, so no login shell runs.
echo 'path = "/usr/bin:/bin"' >"$scratch/config/settings.toml"
cat >"$scratch/kubeconfig" <<'EOF'
apiVersion: v1
kind: Config
clusters:
- name: c
  cluster:
    server: https://127.0.0.1:1
users:
- name: u
  user: {}
contexts:
- name: dev
  context:
    cluster: c
    user: u
EOF
init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
# The server exits when stdin closes; the alarm guards against a hang (macOS has no `timeout`).
# `|| true`: a crash or the alarm must reach the message below, not end the script silently.
answer=$(echo "$init" | KUBECONFIG=$scratch/kubeconfig FLINT_CONFIG_DIR=$scratch/config FLINT_LOG=warn \
  perl -e 'alarm 60; exec @ARGV' "$exe" mcp | head -n 1) || true
grep -q '"serverInfo":{[^}]*"name":"flint"' <<<"$answer" || fail "mcp did not answer initialize: $answer"
echo "mcp: answered initialize"
