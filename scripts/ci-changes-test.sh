#!/usr/bin/env bash
# Self-test for scripts/ci-changes.sh: which check groups each kind of change turns on.
set -euo pipefail

fail=0
expect() {
  local want=$1
  shift
  local got
  got=$(printf '%s\n' "$@" | scripts/ci-changes.sh --stdin | { grep '=true' || true; } | cut -d= -f1 | paste -sd' ' -)
  if [ "$got" != "$want" ]; then
    echo "FAIL: $* -> '$got', want '$want'"
    fail=1
  fi
}

expect "" README.md docs/ARCHITECTURE.md site/index.html CHANGELOG.md
expect "rust appimage macos" crates/flint-core/src/logs/stream.rs
expect "rust app appimage macos" crates/flint-app/src/ui/sidebar.rs
expect "rust appimage macos deps" crates/flint-mcp/Cargo.toml
expect "rust appimage macos deps" Cargo.lock
expect "deps" deny.toml
expect "rust appimage macos workflows" .github/actions/gpui-linux-deps/action.yml
expect "appimage scripts" scripts/package-appimage.sh
expect "appimage" packaging/linux/ubuntu-runtime-packages.txt
expect "appimage macos scripts" scripts/check-no-automation.sh
expect "scripts" scripts/check-theme-colors.sh
expect "workflows" .github/workflows/pages.yml
expect "rust app appimage macos deps scripts workflows" .github/workflows/ci.yml
expect "rust app appimage macos deps scripts workflows" docs/TESTING.md scripts/ci-changes.sh
expect "" ""

all=$(scripts/ci-changes.sh --all | grep -c '=true')
[ "$all" = 7 ] || { echo "FAIL: --all turned on $all groups, want 7"; fail=1; }

if scripts/ci-changes.sh >/dev/null 2>&1; then
  echo "FAIL: no arguments must be a usage error"
  fail=1
fi

[ "$fail" = 0 ] && echo "ci-changes: all cases pass"
exit "$fail"
