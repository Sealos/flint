#!/usr/bin/env bash
# Self-test for scripts/check-no-automation.sh: it must pass a clean Flint-like binary and fail
# closed on everything else.
set -euo pipefail
cd "$(dirname "$0")/.."
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
check() {
  local want=$1 file=$2 got=0
  scripts/check-no-automation.sh "$file" >/dev/null 2>&1 || got=$?
  if [ "$got" != "$want" ]; then
    echo "error: check-no-automation.sh $file exited $got, want $want"
    exit 1
  fi
}
printf 'flint_core::manager\0some other text\0' >"$dir/clean"
printf 'flint_core::manager\0automation.sock\0' >"$dir/socket"
printf 'flint_core::manager\0flint::ui::automation\0' >"$dir/module"
: >"$dir/empty"
printf 'compressed bytes, no strings of ours\0' >"$dir/other"
check 0 "$dir/clean"
check 1 "$dir/socket"
check 1 "$dir/module"
check 3 "$dir/empty"
check 3 "$dir/other"
check 2 "$dir/missing"
echo "check-no-automation.sh: ok"
