#!/usr/bin/env bash
# Which CI checks a change needs, from the paths it touches. Prints `name=true|false` lines, one
# per check group, and appends them to $GITHUB_OUTPUT when it is set.
#
#   scripts/ci-changes.sh <base-sha> <head-sha>   paths from `git diff base...head`
#   scripts/ci-changes.sh --all                   every group (pushes to master, schedules)
#   scripts/ci-changes.sh --stdin                 paths from stdin, one per line (self-test)
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

case "${1:-}" in
  --all)
    all
    emit
    exit 0
    ;;
  --stdin) paths=$(cat) ;;
  "" | -*)
    echo "usage: $0 <base-sha> <head-sha> | --all | --stdin" >&2
    exit 2
    ;;
  # --no-renames lists a moved file's old path too (moving a file out of crates/ still breaks the
  # build); quotePath=false keeps non-ASCII paths unquoted so the patterns below match them.
  *) paths=$(git -c core.quotePath=false diff --name-only --no-renames "$1...${2:?head sha}") ;;
esac

while IFS= read -r path; do
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
done <<<"$paths"

emit
