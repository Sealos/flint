#!/usr/bin/env bash
# Checks a packaged AppImage without a display or a cluster:
#
#   scripts/smoke-appimage.sh <AppImage> <version>
#
# `--version` must print "flint <version>", LICENSE and LICENSE-LUCIDE must be inside, and `mcp`
# must reach the MCP server (arguments pass through the AppImage) and answer `initialize`, as must
# the registration command `mcp --help` prints. Runs with APPIMAGE_EXTRACT_AND_RUN=1, since CI
# runners have no FUSE; the kubeconfig points at a port that refuses connections.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <AppImage> <version>" >&2
  exit 2
fi
appimage=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
version=${2#v}
export APPIMAGE_EXTRACT_AND_RUN=1

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
# Each extract-and-run launch extracts to $TMPDIR/appimage_extracted_<md5> and deletes it on exit,
# so concurrent launches sharing a TMPDIR could delete each other's files. Keep ours private.
mkdir -p "$scratch/tmp"
export TMPDIR=$scratch/tmp
# The log file's location: never the real one when run locally.
mkdir -p "$scratch/home"
export HOME=$scratch/home XDG_STATE_HOME=$scratch/state

got=$("$appimage" --version)
if [ "$got" != "flint $version" ]; then
  echo "error: --version printed \"$got\", want \"flint $version\"" >&2
  exit 1
fi
echo "--version: $got"

# The notices travel with the AppImage: Flint's LICENSE, and LICENSE-LUCIDE for the Lucide icons
# compiled into the binary.
repo=$(cd "$(dirname "$0")/.." && pwd)
(cd "$scratch" && "$appimage" --appimage-extract 'usr/share/doc/flint/*' >/dev/null)
for notice in LICENSE crates/flint-app/assets/icons/LICENSE-LUCIDE; do
  name=$(basename "$notice")
  if ! cmp -s "$scratch/squashfs-root/usr/share/doc/flint/$name" "$repo/$notice"; then
    echo "error: the AppImage has no usr/share/doc/flint/$name" >&2
    exit 1
  fi
done
echo "notices: LICENSE, LICENSE-LUCIDE"
# Release builds set THIRD_PARTY_NOTICES to the file that must be inside.
if [ -n "${THIRD_PARTY_NOTICES:-}" ]; then
  if [ ! -s "$THIRD_PARTY_NOTICES" ] ||
    ! cmp -s "$scratch/squashfs-root/usr/share/doc/flint/THIRD-PARTY-NOTICES.txt" "$THIRD_PARTY_NOTICES"; then
    echo "error: the AppImage has no matching usr/share/doc/flint/THIRD-PARTY-NOTICES.txt" >&2
    exit 1
  fi
  if [ ! -s "${RUST_LIBRARY_NOTICES:-}" ] ||
    ! cmp -s "$scratch/squashfs-root/usr/share/doc/flint/COPYRIGHT-library.html" "$RUST_LIBRARY_NOTICES"; then
    echo "error: the AppImage has no matching usr/share/doc/flint/COPYRIGHT-library.html" >&2
    exit 1
  fi
  echo "notices: THIRD-PARTY-NOTICES.txt, COPYRIGHT-library.html"
fi

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
# The server exits when stdin closes; timeout guards against a hang.
answer=$(echo "$init" | KUBECONFIG=$scratch/kubeconfig FLINT_CONFIG_DIR=$scratch/config FLINT_LOG=warn \
  timeout 60 "$appimage" mcp | head -n 1)
if ! grep -q '"serverInfo":{[^}]*"name":"flint"' <<<"$answer"; then
  echo "error: mcp did not answer initialize: $answer" >&2
  exit 1
fi
echo "mcp: answered initialize"

# The command `mcp --help` suggests must work in a later launch, as an AI client would run it:
# the path this launch runs from is gone by then, and APPIMAGE_EXTRACT_AND_RUN is not inherited.
help=$("$appimage" mcp --help)
command=$(sed -n 's/^ *claude mcp add flint -- //p' <<<"$help")
if [ -z "$command" ]; then
  echo "error: mcp --help has no registration command:" >&2
  echo "$help" >&2
  exit 1
fi
eval "set -- $command"
answer=$(echo "$init" | KUBECONFIG=$scratch/kubeconfig FLINT_CONFIG_DIR=$scratch/config FLINT_LOG=warn \
  timeout 60 env -u APPIMAGE_EXTRACT_AND_RUN "$@" | head -n 1)
if ! grep -q '"serverInfo":{[^}]*"name":"flint"' <<<"$answer"; then
  echo "error: the command from mcp --help did not answer initialize: $command" >&2
  echo "$answer" >&2
  exit 1
fi
echo "mcp --help: its command answers initialize"
