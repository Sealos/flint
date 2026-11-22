#!/usr/bin/env bash
# Which CI checks a change needs, from the paths it touches. Prints `name=true|false` lines, one
# per check group, and appends them to $GITHUB_OUTPUT when it is set.
#
#   scripts/ci-changes.sh <base-sha> <head-sha>   paths from `git diff base...head`
#   scripts/ci-changes.sh --all                   every group (pushes to master, schedules)
#   scripts/ci-changes.sh --stdin                 paths from stdin, one per line (self-test)
#   scripts/ci-changes.sh --suite <any of the above>
#                                                 which suite a pull request needs, instead of
#                                                 the groups: `full` and `packaging`
#
# Groups (the jobs each one gates are named in ci.yml and lint.yml):
#   rust      Rust sources, manifests, the lockfile, toolchain and lint config, the shared GPUI
#             action: build, test, clippy, docs
#   app       crates/flint-app: the app's UI lint scripts
#   appimage  rust, or the Linux release packaging and smoke scripts: the AppImage and the
#             static flint-mcp
#   macos     rust, or the release-binary automation check the macOS job runs
#   deps      manifests, the lockfile and deny.toml: cargo-deny
#   scripts   scripts/: shellcheck and the scripts' self-tests
#   workflows .github/: actionlint
# A change to CI itself (this script, ci.yml, lint.yml) turns every group on, so a broken filter
# cannot hide a broken check.
#
# Suites (--suite):
#   full      paths that always get the full suite, listed in needs_full below
#   packaging the Linux release packaging: a smoke run still builds the packages for these
set -euo pipefail

CHECKS="rust app appimage macos deps scripts workflows"
# The groups turned on so far, space separated (no associative arrays: macOS ships bash 3.2).
on=" "

turn_on() {
  local name
  for name in "$@"; do
    case "$on" in *" $name "*) ;; *) on="$on$name " ;; esac
  done
}

emit() {
  local name value
  for name in $CHECKS; do
    case "$on" in *" $name "*) value=true ;; *) value=false ;; esac
    echo "$name=$value"
    if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "$name=$value" >>"$GITHUB_OUTPUT"; fi
  done
}

all() {
  # shellcheck disable=SC2086 # word splitting is the point: one argument per group
  turn_on $CHECKS
}

# The paths that always get the full suite: all of flint-core except the modules named below, the
# app and test code that calls its write and connection path, MCP, the automation channel,
# manifests and CI.
needs_full() {
  case "$1" in
    # The only flint-core sources a smoke run covers: self-contained helpers. A new file is full
    # until it is added here.
    crates/flint-core/src/paging.rs | crates/flint-core/src/thresholds.rs | \
      crates/flint-core/src/thresholds_tests.rs)
      return 1
      ;;
    crates/flint-core/src/* | crates/flint-core/tests/* | crates/flint-core/clippy.toml)
      return 0
      ;;
    crates/flint-mcp/*) return 0 ;;
    crates/flint-app/src/main.rs | crates/flint-app/src/ui/write_confirm* | \
      crates/flint-app/src/ui/resource_actions* | crates/flint-app/src/ui/detail_edit* | \
      crates/flint-app/src/ui/protect_tests* | crates/flint-app/src/ui/automation/* | \
      crates/flint-app/src/ui/clusters* | crates/flint-app/src/ui/detail.rs | \
      crates/flint-app/src/ui/detail_access* | crates/flint-app/src/ui/detail_actions* | \
      crates/flint-app/src/ui/registry* | crates/flint-app/src/ui/row_menu* | \
      crates/flint-app/src/ui/terminal_model* | crates/flint-app/src/ui/terminal_view* | \
      crates/flint-app/src/ui/view_state* | crates/flint-app/src/ui/workspace* | \
      crates/flint-app/tests/mcp_subcommand.rs)
      return 0
      ;;
    crates/flint-e2e/src/mcp.rs | crates/flint-e2e/src/fake_api/server_writes* | \
      crates/flint-e2e/src/fake_api/kubeconfig.rs | crates/flint-e2e/tests/mcp* | \
      crates/flint-e2e/tests/write_* | crates/flint-e2e/tests/protected_cluster.rs | \
      crates/flint-e2e/tests/kubeconfig_reload.rs | crates/flint-e2e/tests/app/m3_writes.rs | \
      crates/flint-e2e/tests/app/exec_plugins.rs | crates/flint-e2e/tests/access_checks.rs | \
      crates/flint-e2e/tests/core.rs | crates/flint-e2e/tests/core_support/*)
      return 0
      ;;
    # Every manifest: a default-feature change reaches what the release binary contains.
    Cargo.toml | */Cargo.toml | Cargo.lock | deny.toml | .cargo/* | rust-toolchain | \
      rust-toolchain.toml)
      return 0
      ;;
    .github/workflows/* | .github/actions/*) return 0 ;;
    scripts/ci-changes.sh | scripts/ci-changes-test.sh | scripts/check-client-construction.sh | \
      scripts/check-no-automation.sh | scripts/check-no-automation-test.sh)
      return 0
      ;;
  esac
  return 1
}

is_packaging() {
  case "$1" in
    packaging/linux/* | scripts/package-appimage.sh | scripts/smoke-appimage.sh | \
      scripts/package-mcp-linux.sh | scripts/smoke-mcp-linux.sh | scripts/test-mcp-linux.sh)
      return 0
      ;;
  esac
  return 1
}

emit_suite() {
  echo "full=$1"
  echo "packaging=$2"
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf 'full=%s\npackaging=%s\n' "$1" "$2" >>"$GITHUB_OUTPUT"
  fi
}

suite=false
if [ "${1:-}" = --suite ]; then
  suite=true
  shift
fi

# NUL-separated in a file: `git diff -z` never quotes a path, and a failed diff stops the script
# here (a process substitution would hide the failure).
paths_file=$(mktemp)
trap 'rm -f "$paths_file"' EXIT

case "${1:-}" in
  --all)
    if [ "$suite" = true ]; then
      emit_suite true true
    else
      all
      emit
    fi
    exit 0
    ;;
  --stdin) { cat; echo; } | tr '\n' '\0' >"$paths_file" ;;
  "" | -*)
    echo "usage: $0 [--suite] <base-sha> <head-sha> | --all | --stdin" >&2
    exit 2
    ;;
  # --no-renames lists a moved file's old path too (moving a file out of crates/ still breaks the
  # build).
  *) git diff -z --name-only --no-renames "$1...${2:?head sha}" >"$paths_file" ;;
esac

if [ "$suite" = true ]; then
  full=false
  packaging=false
  while IFS= read -r -d '' path; do
    if needs_full "$path"; then full=true; fi
    if is_packaging "$path"; then packaging=true; fi
  done <"$paths_file"
  emit_suite "$full" "$packaging"
  exit 0
fi

while IFS= read -r -d '' path; do
  [ -n "$path" ] || continue
  case "$path" in
    scripts/ci-changes.sh | .github/workflows/ci.yml | .github/workflows/lint.yml)
      all
      ;;
  esac
  case "$path" in
    crates/* | Cargo.toml | Cargo.lock | .github/actions/* | .cargo/* | rust-toolchain | \
      rust-toolchain.toml | rustfmt.toml | .rustfmt.toml | clippy.toml | .clippy.toml)
      turn_on rust appimage macos
      ;;
  esac
  case "$path" in crates/flint-app/*) turn_on app ;; esac
  case "$path" in
    Cargo.toml | Cargo.lock | crates/*/Cargo.toml | deny.toml) turn_on deps ;;
  esac
  case "$path" in
    scripts/package-appimage.sh | scripts/smoke-appimage.sh | packaging/linux/* | \
      scripts/package-mcp-linux.sh | scripts/smoke-mcp-linux.sh | scripts/test-mcp-linux.sh)
      turn_on appimage
      ;;
  esac
  case "$path" in
    scripts/check-no-automation.sh) turn_on appimage macos ;;
  esac
  case "$path" in scripts/*) turn_on scripts ;; esac
  case "$path" in .github/*) turn_on workflows ;; esac
done <"$paths_file"

emit
