#!/usr/bin/env bash
# Writes the third-party notices for every release asset, from Cargo.lock:
#
#   scripts/generate-notices.sh <output-file>
#
# Needs cargo and python3 (3.11 or newer) on PATH, and no network beyond cargo's registry.
# Covers the Rust crates linked into any released binary, on the same package and target pairs as
# scripts/generate-sbom.sh. scripts/notices.py checks each crate and writes the file; it exits 1,
# writing nothing, when a crate's license or notice text cannot be accounted for.
#
# packaging/notices/overrides.tsv holds texts for crates whose package has none, and
# packaging/notices/embedded.tsv the audit of license files below a crate's root. Both are bound to
# the exact name, version and Cargo.lock checksum, so updating such a crate needs a new audit.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <output-file>" >&2
  exit 2
fi
mkdir -p "$(dirname "$1")"
output=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
export LC_ALL=C

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# package target
while read -r package target; do
  cargo tree --locked -p "$package" --target "$target" -e normal,no-proc-macro --prefix none --format '{p}' |
    awk '{ print $1 "@" substr($2, 2) }' >>"$work/linked.txt"
done <<EOF
flint-app aarch64-apple-darwin
flint-app x86_64-apple-darwin
flint-app x86_64-unknown-linux-gnu
flint-app aarch64-unknown-linux-gnu
flint-mcp x86_64-unknown-linux-musl
flint-mcp aarch64-unknown-linux-musl
EOF
[ -s "$work/linked.txt" ] || { echo "error: cargo tree listed no crates" >&2; exit 1; }

cargo metadata --locked --format-version 1 >"$work/metadata.json"
python3 -I "$repo/scripts/notices.py" "$repo" "$work/linked.txt" "$work/metadata.json" "$output"
