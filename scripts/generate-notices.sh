#!/usr/bin/env bash
# Writes the third-party notices for every release asset, from Cargo.lock:
#
#   scripts/generate-notices.sh <output-file>
#
# Needs cargo and jq on PATH. Covers the Rust crates linked into any released binary, on the same
# package and target pairs as scripts/generate-sbom.sh. The license and notice files are copied
# verbatim from each crate's source, so files a license expression cannot name (ring's
# LICENSE-other-bits) are kept. Identical texts are printed once, under the crates that use them.
#
# Exits 1, writing nothing, when a linked crate declares no license or ships no license file.
# A crate whose package has no license file takes them from packaging/notices/<crate>/, else from
# packaging/notices/<owner>-<repo>/ for its GitHub repository; each directory's SOURCE names where
# the files came from.
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
mkdir "$work/texts" "$work/users"

# package target
while read -r package target; do
  cargo tree --locked -p "$package" --target "$target" -e normal,no-proc-macro --prefix none --format '{p}' |
    awk '{ print $1 "\t" substr($2, 2) }' >>"$work/linked.tsv"
done <<EOF
flint-app aarch64-apple-darwin
flint-app x86_64-apple-darwin
flint-app x86_64-unknown-linux-gnu
flint-app aarch64-unknown-linux-gnu
flint-mcp x86_64-unknown-linux-musl
flint-mcp aarch64-unknown-linux-musl
EOF
sort -u "$work/linked.tsv" -o "$work/linked.tsv"
[ -s "$work/linked.tsv" ] || { echo "error: cargo tree listed no crates" >&2; exit 1; }

# name, version, license, directory, repository slug; workspace crates (no source) are Flint's own.
# "-" stands for an empty field, because `read` collapses adjacent tabs.
cargo metadata --locked --format-version 1 |
  jq -r '.packages[] | select(.source != null)
    | [.name, .version, (.license // "-"), (.manifest_path | sub("/Cargo.toml$"; "")),
       (((.repository // "") | capture("github\\.com/(?<o>[^/]+)/(?<r>[^/#]+?)(\\.git)?/?$")
         | "\(.o)-\(.r)" | ascii_downcase) // "-")] | @tsv' |
  sort -u >"$work/packages.tsv"

# license_files <dir>: the crate's license and notice files, one per line.
license_files() {
  find -L "$1" -maxdepth 2 -type f 2>/dev/null |
    awk -v root="$1/" '{
      rel = substr($0, length(root) + 1); low = tolower(rel)
      if (low ~ /\.(rs|toml|json|lock|html|png|svg)$/) next
      if (low ~ /^(licen[cs]e|copying|notice|copyright|unlicense|authors-license)[^\/]*$/ ||
          low ~ /^licen[cs]es?\/[^\/]+$/) print rel
    }' | sort
}

problems=$work/problems.txt
: >"$problems"
: >"$work/crates.txt"
join -t "$(printf '\t')" -j 1 \
  <(awk -F'\t' '{ print $1 "@" $2 "\t" $3 "\t" $4 "\t" $5 }' "$work/packages.tsv" | sort -t "$(printf '\t')" -k1,1) \
  <(awk -F'\t' '{ print $1 "@" $2 }' "$work/linked.tsv" | sort) >"$work/selected.tsv"

while IFS=$'\t' read -r crate license dir slug; do
  name=${crate%@*}
  if [ "$license" = - ]; then
    echo "$crate declares no license" >>"$problems"
    continue
  fi
  source_dir=$dir
  files=$(license_files "$source_dir")
  for fallback in "$name" "$slug"; do
    if [ -z "$files" ] && [ -d "$repo/packaging/notices/$fallback" ]; then
      source_dir=$repo/packaging/notices/$fallback
      files=$(license_files "$source_dir")
    fi
  done
  if [ -z "$files" ]; then
    echo "$crate ($license) ships no license file" >>"$problems"
    continue
  fi
  printf '%s\t%s\n' "$crate" "$license" >>"$work/crates.txt"
  while IFS= read -r file; do
    sum=$(sha256sum <"$source_dir/$file" | cut -d' ' -f1)
    [ -f "$work/texts/$sum" ] || cp "$source_dir/$file" "$work/texts/$sum"
    printf '%s (%s)\n' "$crate" "$file" >>"$work/users/$sum"
  done <<<"$files"
done <"$work/selected.tsv"

# Every linked crate outside the workspace must have been selected from the metadata.
workspace=$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.packages[] | .name + "@" + .version')
awk -F'\t' '{ print $1 "@" $2 }' "$work/linked.tsv" | sort >"$work/linked-ids.txt"
cut -f1 "$work/selected.tsv" | sort >"$work/selected-ids.txt"
comm -23 "$work/linked-ids.txt" "$work/selected-ids.txt" | grep -vxF -f <(printf '%s\n' "$workspace") |
  sed 's/$/ is not in cargo metadata/' >>"$problems" || true

if [ -s "$problems" ]; then
  echo "error: cannot write complete third-party notices:" >&2
  sort "$problems" >&2
  exit 1
fi

{
  echo "THIRD-PARTY NOTICES FOR FLINT"
  echo
  echo "Flint's release binaries link the Rust crates listed here. Each crate's license and notice"
  echo "files follow, copied from its source. Identical texts appear once, under every crate and"
  echo "file that carries them."
  echo
  echo "CRATES"
  echo
  sort "$work/crates.txt" | awk -F'\t' '{ print $1 ": " $2 }'
  for users in $(cd "$work/users" && for f in *; do printf '%s\t%s\n' "$(sort "$f" | head -n 1)" "$f"; done |
    sort | cut -f2); do
    echo
    echo "================================================================================"
    sort "$work/users/$users"
    echo "--------------------------------------------------------------------------------"
    cat "$work/texts/$users"
  done
} >"$work/notices.txt"

mv "$work/notices.txt" "$output"
echo "notices: $(wc -l <"$work/crates.txt" | tr -d ' ') crates, $(wc -c <"$output" | tr -d ' ') bytes, $output"
