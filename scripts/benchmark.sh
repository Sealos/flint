#!/usr/bin/env bash
# Runs the UP-26 load test (10,000 pods on the fake API server, then a burst of 200 deletes) and
# writes its numbers with the machine and commit they came from:
#
#   scripts/benchmark.sh [--profile debug|optimized] [output.json]
#
# The method is in docs/BENCHMARK.md. This reuses the nightly test (`macos-load` in
# .github/workflows/memory.yml) rather than a second harness.
#
#   debug       the build CI uses (target/debug).
#   optimized   the same code with opt-level 3 and no debug info, in its own target directory
#               (target/benchmark). The automation channel the test needs refuses --release, so
#               this is the closest build to a shipped one; debug assertions stay on.
#
# Only the fake API server is used. The script sets KUBECONFIG itself and refuses to run when the
# caller's points anywhere else, so a real cluster's kubeconfig cannot be picked up. macOS only
# (the test opens a window).
set -euo pipefail

profile=optimized
out=
while [ $# -gt 0 ]; do
  case $1 in
    --profile) profile=${2:-}; shift 2 ;;
    -h | --help) sed -n '2,15p' "$0"; exit 0 ;;
    *) out=$1; shift ;;
  esac
done
case $profile in debug | optimized) ;; *) echo "error: --profile is debug or optimized" >&2; exit 2 ;; esac

if [ -n "${KUBECONFIG:-}" ] && [ "$KUBECONFIG" != /dev/null ]; then
  echo "error: KUBECONFIG is set to $KUBECONFIG; this benchmark runs on the fake API only. Unset it." >&2
  exit 1
fi
export KUBECONFIG=/dev/null
if [ "$(uname -s)" != Darwin ]; then
  echo "error: the app test needs macOS and a GUI session" >&2
  exit 1
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
commit=$(git rev-parse HEAD)
dirty=false
[ -n "$(git status --porcelain --untracked-files=no)" ] && dirty=true

if [ "$profile" = optimized ]; then
  export CARGO_TARGET_DIR="$repo/target/benchmark"
  export CARGO_PROFILE_DEV_OPT_LEVEL=3
  export CARGO_PROFILE_DEV_DEBUG=0
fi
target=${CARGO_TARGET_DIR:-$repo/target}

cargo build -p flint-app --features automation --bin flint --locked
raw=$(mktemp)
trap 'rm -f "$raw"' EXIT
FLINT_E2E_APP_BIN="$target/debug/flint" FLINT_E2E_LOAD_REPORT="$raw" \
  cargo test -p flint-e2e --test app --locked -- \
  --ignored --exact load::up_26_large_list_stays_responsive --nocapture

result=$(jq -n --slurpfile load "$raw" \
  --arg commit "$commit" --argjson dirty "$dirty" --arg profile "$profile" \
  --arg date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg cpu "$(sysctl -n machdep.cpu.brand_string)" \
  --argjson ram_bytes "$(sysctl -n hw.memsize)" \
  --arg os "$(sw_vers -productName) $(sw_vers -productVersion)" \
  --arg rustc "$(rustc --version)" \
  '{commit: $commit, dirty: $dirty, profile: $profile, date: $date, cpu: $cpu,
    ram_bytes: $ram_bytes, os: $os, rustc: $rustc, load: $load[0]}')
if [ -n "$out" ]; then
  printf '%s\n' "$result" >"$out"
fi
printf '%s\n' "$result"
