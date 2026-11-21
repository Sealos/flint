#!/usr/bin/env bash
# Every Kubernetes client is assembled in `cluster::build_client`, which puts the read-only rule on
# it. clippy.toml enforces that for what clippy can name. `Client::try_from` is a trait method, so
# this counts its uses instead. The counts below are exact: the proxy refusal in `cluster.rs`, and
# tests against a local listener. A new use fails here.
set -euo pipefail
cd "$(dirname "$0")/.."

expected='crates/flint-core/src/cluster.rs 1
crates/flint-core/src/logs/pods.rs 1
crates/flint-core/src/protection.rs 1
crates/flint-core/src/read.rs 2'

found=$(git ls-files 'crates/flint-core/*.rs' 'crates/flint-mcp/*.rs' |
  xargs grep -cE '^[[:space:]]*[^/[:space:]].*\bClient[[:space:]]*::[[:space:]]*try_from\b|^[[:space:]]*Client[[:space:]]*::[[:space:]]*try_from\b' |
  awk -F: '$2 > 0 { print $1, $2 }' | LC_ALL=C sort)

if [[ "$found" != "$expected" ]]; then
  echo "Client::try_from is used outside its known places. Build clients in cluster::build_client." >&2
  diff <(echo "$expected") <(echo "$found") >&2 || true
  exit 1
fi
