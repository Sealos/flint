#!/usr/bin/env bash
# Every Kubernetes client is assembled in `cluster::build_client`, which puts the read-only rule on
# it. crates/flint-core/clippy.toml (linked from flint-mcp) enforces that for what clippy can name.
# `Client::try_from` is one type's trait method, so this counts its uses instead. The counts below
# are exact: tests against a local listener. A new use, or a conversion that does not name its
# target type, fails here.
set -euo pipefail
cd "$(dirname "$0")/.."

expected='crates/flint-core/src/logs/pods.rs 1
crates/flint-core/src/protection.rs 1
crates/flint-core/src/read.rs 2'

found=$(git ls-files 'crates/flint-core/*.rs' 'crates/flint-mcp/*.rs' |
  xargs grep -cE '^[[:space:]]*[^/[:space:]].*\bClient[[:space:]]*::[[:space:]]*try_from\b|^[[:space:]]*Client[[:space:]]*::[[:space:]]*try_from\b' |
  awk -F: '$2 > 0 { print $1, $2 }' | LC_ALL=C sort)

# shellcheck disable=SC2016
unnamed=$(git ls-files 'crates/flint-core/*.rs' 'crates/flint-mcp/*.rs' |
  xargs grep -nE '^[[:space:]]*[^/[:space:]].*(TryFrom::try_from|TryInto::try_into|\bClient as |<Client as )' || true)
if [[ -n "$unnamed" ]]; then
  echo "Name the target type of a conversion, and do not rename kube's Client:" >&2
  echo "$unnamed" >&2
  exit 1
fi

if [[ "$found" != "$expected" ]]; then
  echo "Client::try_from is used outside its known places. Build clients in cluster::build_client." >&2
  diff <(echo "$expected") <(echo "$found") >&2 || true
  exit 1
fi
