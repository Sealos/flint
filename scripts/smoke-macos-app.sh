#!/usr/bin/env bash
# Checks a packaged Flint DMG without a display or a cluster:
#
#   scripts/smoke-macos-app.sh <dmg> <version>
#
# Mounts the DMG read-only at a private mount point and checks that it holds Flint.app and an
# Applications link; that the bundle has the bundle identifier se.redstack.flint, <version>, its
# icon, LICENSE and LICENSE-LUCIDE; that its signature verifies and is the ad-hoc one for se.redstack.flint; that
# `Flint.app/Contents/MacOS/flint --version` prints "flint <version>" and `flint mcp` answers
# `initialize` and exits cleanly; and that the bundled binary has no automation channel. Detaches
# the DMG on exit. Exits 1 when a check fails, 2 for bad arguments.
#
# scripts/smoke-macos-app-test.sh checks that it fails on a wrong bundle.
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

fail() {
  echo "error: $*" >&2
  exit 1
}
# guard <seconds> <command...>: runs the command, killed after <seconds> (macOS has no `timeout`),
# so a hung tool fails the check instead of holding the runner. Killed means exit status 142.
guard() {
  local seconds=$1
  shift
  perl -e 'alarm shift; exec @ARGV or die "exec $ARGV[0]: $!\n"' "$seconds" "$@"
}

scratch=$(mktemp -d)
mnt=$scratch/mnt
mkdir -p "$mnt"
cleanup() {
  # Detach fails harmlessly when the DMG never attached. The scratch directory goes only once the
  # mount point is empty, so rm never walks into a DMG that is still attached.
  guard 60 hdiutil detach "$mnt" -quiet 2>/dev/null ||
    guard 60 hdiutil detach "$mnt" -force -quiet 2>/dev/null || true
  rmdir "$mnt" 2>/dev/null && rm -rf "$scratch"
}
trap cleanup EXIT

# hdiutil sometimes fails with "Resource busy" on CI runners; a retry gets through.
for attempt in 1 2 3; do
  if guard 120 hdiutil attach "$dmg" -readonly -nobrowse -noautoopen -mountpoint "$mnt" -quiet; then
    break
  fi
  [ "$attempt" != 3 ] || fail "hdiutil attach failed three times"
  sleep 5
done
app=$mnt/Flint.app
exe=$app/Contents/MacOS/flint

[ -d "$app" ] || fail "the DMG has no Flint.app"
[ "$(readlink "$mnt/Applications")" = /Applications ] || fail "the DMG has no Applications link"
[ -x "$exe" ] || fail "Flint.app has no executable Contents/MacOS/flint"
[ -f "$app/Contents/Resources/Flint.icns" ] || fail "Flint.app has no Contents/Resources/Flint.icns"
cmp -s "$app/Contents/Resources/LICENSE" "$repo/LICENSE" || fail "Flint.app has no Contents/Resources/LICENSE"
cmp -s "$app/Contents/Resources/LICENSE-LUCIDE" "$repo/crates/flint-app/assets/icons/LICENSE-LUCIDE" ||
  fail "Flint.app has no Contents/Resources/LICENSE-LUCIDE"

plist=$app/Contents/Info.plist
plutil -lint "$plist" || fail "Info.plist does not parse"
read_key() { /usr/libexec/PlistBuddy -c "Print :$1" "$plist" 2>/dev/null || true; }
[ "$(read_key CFBundleIdentifier)" = se.redstack.flint ] ||
  fail "CFBundleIdentifier is \"$(read_key CFBundleIdentifier)\", want se.redstack.flint"
[ "$(read_key CFBundleExecutable)" = flint ] || fail "CFBundleExecutable is not flint"
for key in CFBundleShortVersionString CFBundleVersion; do
  [ "$(read_key "$key")" = "$version" ] || fail "$key is \"$(read_key "$key")\", want $version"
done
echo "bundle: se.redstack.flint $version"

guard 120 codesign --verify --deep --strict --verbose=2 "$app" || fail "the signature does not verify"
signature=$(guard 60 codesign -dv "$app" 2>&1) || fail "codesign -dv failed: $signature"
grep -qx 'Identifier=se.redstack.flint' <<<"$signature" || fail "the signature's identifier is not se.redstack.flint"
grep -qx 'Signature=adhoc' <<<"$signature" || fail "Flint.app is not ad-hoc signed"
echo "signature: ad-hoc, se.redstack.flint"

got=$(guard 30 "$exe" --version) || fail "--version failed or hung (exit $?)"
[ "$got" = "flint $version" ] || fail "--version printed \"$got\", want \"flint $version\""
echo "--version: $got"

# On the executable itself: the DMG is compressed, so its strings cannot be searched.
"$repo/scripts/check-no-automation.sh" "$exe" || fail "check-no-automation.sh failed on $exe"

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
# The server answers, then exits when stdin closes: both the answer and a clean exit are required,
# so a crash or hang after a good answer still fails. All of stdout is kept (no `head` closing
# the pipe early).
status=0
# HOME places the log file: never the real one when run locally.
mkdir -p "$scratch/home"
HOME=$scratch/home KUBECONFIG=$scratch/kubeconfig FLINT_CONFIG_DIR=$scratch/config FLINT_LOG=warn \
  guard 60 "$exe" mcp <<<"$init" >"$scratch/mcp.out" 2>"$scratch/mcp.err" || status=$?
answer=$(head -n 1 "$scratch/mcp.out")
if [ "$status" != 0 ]; then
  cat "$scratch/mcp.err" >&2
  fail "flint mcp exited $status (142: killed after 60 s); first line of output: $answer"
fi
grep -q '"serverInfo":{[^}]*"name":"flint"' <<<"$answer" || fail "mcp did not answer initialize: $answer"
echo "mcp: answered initialize and exited 0"
