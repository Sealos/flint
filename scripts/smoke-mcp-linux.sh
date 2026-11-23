#!/usr/bin/env bash
# Checks a packaged flint-mcp archive without a cluster:
#
#   scripts/smoke-mcp-linux.sh <archive> <version>
#
# The archive must hold flint-mcp/flint-mcp with LICENSE, LICENSE-MUSL and README.md, `--version` must print
# "flint-mcp <version>", and the server must answer `initialize`; the kubeconfig points at a port
# that refuses connections. It needs only bash, coreutils, diffutils, grep and tar, so it runs in a bare
# `ubuntu:22.04` container, which is how CI checks that the binary needs nothing from the host.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <archive> <version>" >&2
  exit 2
fi
archive=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
version=${2#v}

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
tar -xzf "$archive" -C "$scratch"
for file in flint-mcp LICENSE LICENSE-MUSL README.md; do
  if [ ! -s "$scratch/flint-mcp/$file" ]; then
    echo "error: the archive has no flint-mcp/$file" >&2
    exit 1
  fi
done
binary=$scratch/flint-mcp/flint-mcp
echo "archive: flint-mcp, LICENSE, LICENSE-MUSL, README.md"
# Release builds set THIRD_PARTY_NOTICES to the file that must be inside.
if [ -n "${THIRD_PARTY_NOTICES:-}" ]; then
  if [ ! -s "$THIRD_PARTY_NOTICES" ] ||
    ! cmp -s "$scratch/flint-mcp/THIRD-PARTY-NOTICES.txt" "$THIRD_PARTY_NOTICES"; then
    echo "error: the archive has no matching flint-mcp/THIRD-PARTY-NOTICES.txt" >&2
    exit 1
  fi
  if [ ! -s "${RUST_LIBRARY_NOTICES:-}" ] ||
    ! cmp -s "$scratch/flint-mcp/COPYRIGHT-library.html" "$RUST_LIBRARY_NOTICES"; then
    echo "error: the archive has no matching flint-mcp/COPYRIGHT-library.html" >&2
    exit 1
  fi
  echo "archive: THIRD-PARTY-NOTICES.txt, COPYRIGHT-library.html"
fi

got=$("$binary" --version)
if [ "$got" != "flint-mcp $version" ]; then
  echo "error: --version printed \"$got\", want \"flint-mcp $version\"" >&2
  exit 1
fi
echo "--version: $got"

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
# HOME and XDG_STATE_HOME place the log file: never the real one when run locally.
mkdir -p "$scratch/home"
answer=$(echo "$init" | HOME=$scratch/home XDG_STATE_HOME=$scratch/state \
  KUBECONFIG=$scratch/kubeconfig FLINT_CONFIG_DIR=$scratch/config FLINT_LOG=warn \
  timeout 60 "$binary" | head -n 1)
if ! grep -q '"serverInfo":{[^}]*"name":"flint"' <<<"$answer"; then
  echo "error: flint-mcp did not answer initialize: $answer" >&2
  exit 1
fi
echo "mcp: answered initialize"
