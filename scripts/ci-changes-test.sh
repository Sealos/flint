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
expect "appimage scripts" scripts/package-mcp-linux.sh
expect "appimage scripts" scripts/smoke-mcp-linux.sh scripts/test-mcp-linux.sh
expect "appimage macos scripts" scripts/check-no-automation.sh
expect "scripts" scripts/check-theme-colors.sh
expect "workflows" .github/workflows/pages.yml
expect "rust app appimage macos deps scripts workflows" .github/workflows/ci.yml
expect "rust app appimage macos deps scripts workflows" docs/TESTING.md scripts/ci-changes.sh
expect "" ""

expect "rust appimage macos" rust-toolchain.toml
expect "rust appimage macos" .cargo/config.toml

# Which suite a path needs: flint-core is full unless a module is named in needs_full.
expect_suite() {
  local want=$1 path=$2 got
  got=$(printf '%s\n' "$path" | scripts/ci-changes.sh --suite --stdin | grep '^full=')
  if [ "$got" != "full=$want" ]; then
    echo "FAIL: suite of $path -> '$got', want 'full=$want'"
    fail=1
  fi
}

for path in \
  crates/flint-core/src/handle.rs \
  crates/flint-core/src/recheck.rs \
  crates/flint-core/src/lib.rs \
  crates/flint-core/src/logs/session.rs \
  crates/flint-core/src/watch/engine.rs \
  crates/flint-core/src/new_module.rs \
  crates/flint-core/src/new_dir/new_module.rs \
  crates/flint-core/src/paging_extra.rs \
  crates/flint-core/tests/core_handle.rs \
  crates/flint-app/src/ui/clusters.rs \
  crates/flint-app/src/ui/workspace_actions.rs \
  crates/flint-e2e/tests/access_checks.rs \
  crates/flint-mcp/src/main.rs \
  .github/workflows/ci.yml; do
  expect_suite true "$path"
done
for path in \
  crates/flint-core/src/paging.rs \
  crates/flint-core/src/thresholds.rs \
  crates/flint-core/src/thresholds_tests.rs \
  crates/flint-app/src/ui/sidebar.rs \
  docs/TESTING.md \
  README.md; do
  expect_suite false "$path"
done

# The git path: a file moved out of crates/ and a non-ASCII name must still count as Rust changes.
script="$PWD/scripts/ci-changes.sh"
repo=$(mktemp -d)
trap 'rm -rf "$repo"' EXIT
(
  cd "$repo"
  git init -q
  git config user.email ci@example.invalid
  git config user.name ci
  mkdir -p crates/a docs
  echo 'pub fn x() {}' >crates/a/x.rs
  git add . && git commit -qm base
  git mv crates/a/x.rs docs/x.rs
  echo '// é' >"crates/a/café.rs"
  git add . && git commit -qm change
)
got=$(cd "$repo" && "$script" HEAD~1 HEAD | grep -E '^rust=' || true)
[ "$got" = rust=true ] || { echo "FAIL: rename out of crates/ + non-ASCII path -> '$got', want rust=true"; fail=1; }
moved=$(cd "$repo" && git -c core.quotePath=false diff --name-only --no-renames HEAD~1 HEAD | grep -c '^crates/' || true)
[ "$moved" = 2 ] || { echo "FAIL: expected the old path and the non-ASCII path under crates/, got $moved"; fail=1; }

all=$(scripts/ci-changes.sh --all | grep -c '=true')
[ "$all" = 7 ] || { echo "FAIL: --all turned on $all groups, want 7"; fail=1; }

if scripts/ci-changes.sh >/dev/null 2>&1; then
  echo "FAIL: no arguments must be a usage error"
  fail=1
fi

[ "$fail" = 0 ] && echo "ci-changes: all cases pass"
exit "$fail"
