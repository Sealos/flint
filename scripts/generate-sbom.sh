#!/usr/bin/env bash
# Writes one CycloneDX SBOM, with a checksum, per released binary, from Cargo.lock.
#
#   scripts/generate-sbom.sh <tag> <output-dir>
#
# Needs `cargo cyclonedx` and jq on PATH. The macOS tarball and DMG hold the same universal binary,
# so they share two SBOMs, one per slice. SBOMs cover Rust crates only, not the non-Rust parts
# bundled into an asset (the AppImage runtime, musl).
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <tag> <output-dir>" >&2
  exit 2
fi
tag=$1
version=${tag#v}
mkdir -p "$2"
out=$(cd "$2" && pwd)
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"

# The name is fixed so the file can be found in the package's directory; the tool writes one for
# every workspace member.
generated=flint-sbom
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}

# cargo-cyclonedx has no --locked and `cargo metadata` may rewrite the lockfile, which would
# describe different crates from the ones built.
lock_before=$(git hash-object Cargo.lock)

# asset-name package target
while read -r name package target; do
  rm -f crates/*/"$generated.json"
  cargo cyclonedx \
    --manifest-path Cargo.toml \
    --format json \
    --spec-version 1.5 \
    --all \
    --no-build-deps \
    --target "$target" \
    --override-filename "$generated"

  file="crates/$package/$generated.json"
  [ -s "$file" ] || { echo "error: no SBOM written for $package on $target" >&2; exit 1; }
  jq -e --arg package "$package" --arg version "$version" --arg target "$target" '
    .bomFormat == "CycloneDX"
    and .metadata.component.name == $package
    and .metadata.component.version == $version
    and any(.metadata.properties[]?; .name == "cdx:rustc:sbom:target:triple" and .value == $target)
    and (.components | length) > 0
  ' "$file" >/dev/null || { echo "error: bad SBOM for $package on $target" >&2; exit 1; }

  mv "$file" "$out/$name.cdx.json"
  (cd "$out" && sha256sum "$name.cdx.json" > "$name.cdx.json.sha256")
done <<EOF
flint-$tag-macos-aarch64 flint-app aarch64-apple-darwin
flint-$tag-macos-x86_64 flint-app x86_64-apple-darwin
flint-$tag-linux-x86_64 flint-app x86_64-unknown-linux-gnu
flint-$tag-linux-aarch64 flint-app aarch64-unknown-linux-gnu
flint-mcp-$tag-x86_64-unknown-linux-musl flint-mcp x86_64-unknown-linux-musl
flint-mcp-$tag-aarch64-unknown-linux-musl flint-mcp aarch64-unknown-linux-musl
EOF
rm -f crates/*/"$generated.json"

if [ "$(git hash-object Cargo.lock)" != "$lock_before" ]; then
  echo "error: Cargo.lock changed while generating SBOMs" >&2
  exit 1
fi
