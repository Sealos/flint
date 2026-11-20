#!/usr/bin/env bash
# Runs the MCP end-to-end tests (flint-e2e's tests/mcp.rs, against the fake API server) on a
# packaged flint-mcp archive, in a bare `ubuntu:22.04` container:
#
#   scripts/test-mcp-linux.sh <arch> <archive>
#
# The test binary is built for <arch>-unknown-linux-musl too, so it is static like flint-mcp and
# runs on 22.04's glibc 2.35. The container gets the checkout read-only at the same path (the
# fixtures' path is compiled into the tests) and nothing else from the host: no packages, no
# kubeconfig, no network beyond the fake server on loopback. Needs Docker and what
# scripts/package-mcp-linux.sh needs.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <arch> <archive>" >&2
  exit 2
fi
arch=$1
archive=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
target=$arch-unknown-linux-musl
repo=$(cd "$(dirname "$0")/.." && pwd)
# The multi-arch index of ubuntu:22.04, so each runner gets its own architecture.
IMAGE=ubuntu:22.04@sha256:f64b2ffc2f10ab8f8505aa56b2664bd846cbb9df87c7e2532e30f7667021c0e6

export "CC_${target//-/_}=${CC_MUSL:-musl-gcc}"
# Cargo builds flint-e2e's binaries for its integration tests too; take the `mcp` test's path.
tests=$(cd "$repo" && cargo test -p flint-e2e --test mcp --locked --no-run --target "$target" \
  --message-format=json | grep '"reason":"compiler-artifact"' | grep '"kind":\["test"\]' |
  grep '"name":"mcp"' | grep -o '"executable":"[^"]*"' | cut -d'"' -f4)
if [ ! -x "$tests" ]; then
  echo "error: cargo did not report the mcp test binary" >&2
  exit 1
fi

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
tar -xzf "$archive" -C "$scratch"
cp "$tests" "$scratch/mcp-tests"

# HOME and the config directory are the container's own; the tests make scratch ones anyway.
docker run --rm --network none \
  -v "$repo:$repo:ro" -v "$scratch:/flint:ro" -w "$repo" \
  -e FLINT_E2E_MCP_BIN=/flint/flint-mcp/flint-mcp -e FLINT_CONFIG_DIR=/tmp/flint-config \
  "$IMAGE" /flint/mcp-tests
