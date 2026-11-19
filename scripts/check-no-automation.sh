#!/usr/bin/env bash
# Fails if a release binary contains the test-only automation channel (docs/TESTING.md,
# "Automation channel"): its socket name, its log lines or its module path. Release binaries are
# stripped, so this looks at strings, not symbols.
#
# Run it on the executable itself (the ELF or Mach-O file in target/), not on an AppImage, DMG or
# archive: their contents are compressed, so a search would find nothing either way.
#
# Usage: scripts/check-no-automation.sh <binary>
# Exits 0 when clean, 1 when the channel is there, 2 for a missing file, 3 when the file does not
# look like a Flint binary (so an empty, compressed or wrong file never passes).
set -euo pipefail
binary=${1:?usage: $0 <binary>}
if [ ! -f "$binary" ]; then
  echo "error: $binary does not exist" >&2
  exit 2
fi
# Its own statement, so a missing or failing `strings` stops the script (set -e).
out=$(LC_ALL=C strings -a -- "$binary")
# Every Flint binary names flint-core's modules (tracing targets, panic locations).
if ! grep -q 'flint_core' <<<"$out"; then
  echo "error: $binary does not look like a Flint binary (no flint_core strings)" >&2
  exit 3
fi
if grep -qE 'automation\.sock|automation channel|flint::ui::automation' <<<"$out"; then
  echo "::error::$binary contains the automation channel"
  exit 1
fi
echo "$binary has no automation channel"
