#!/usr/bin/env bash
# Checks a packaged AppImage without a display or a cluster:
#
#   scripts/smoke-appimage.sh <AppImage> <version>
#
# `--version` must print "flint <version>", `mcp --help` must name the AppImage and no temporary
# path, LICENSE and LICENSE-LUCIDE must be inside, and `mcp` must reach the MCP server (arguments pass through the AppImage) and answer `initialize`. Runs
# with APPIMAGE_EXTRACT_AND_RUN=1, since CI runners have no FUSE; the kubeconfig points at a port
# that refuses connections.
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

got=$("$appimage" --version)
if [ "$got" != "flint $version" ]; then
  echo "error: --version printed \"$got\", want \"flint $version\"" >&2
  exit 1
fi
echo "--version: $got"

# The registration example must name the AppImage: the path a launch runs from is deleted when
# that launch exits.
help=$("$appimage" mcp --help)
if grep -qE 'appimage_extracted|\.mount_' <<<"$help"; then
  echo "error: mcp --help names a temporary path:" >&2
  echo "$help" >&2
  exit 1
fi
if ! grep -qF "$(basename "$appimage")" <<<"$help"; then
  echo "error: mcp --help does not name the AppImage:" >&2
  echo "$help" >&2
  exit 1
fi
echo "mcp --help: names the AppImage"

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
