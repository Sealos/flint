#!/usr/bin/env bash
# Record the fake API server's fixtures from the kind dev cluster into crates/flint-e2e/fixtures/:
# version, aggregated discovery, OpenAPI v3, Table lists (includeObject=Metadata), objects and
# events. Secrets are scrubbed and uids, timestamps and resource versions normalized, so a
# re-recording of an unchanged cluster gives a small diff.
#
#   KUBECONFIG=.dev/kubeconfig scripts/record-fixtures.sh [out-dir]
#
# With out-dir, the recording goes there instead (the nightly drift check records into a scratch
# directory and compares it with the committed set: fixture-drift).
#
# Refuses to run unless KUBECONFIG is set explicitly to one file whose current context is
# kind-flint-dev (scripts/dev-cluster.sh up). Never reads ~/.kube/config.
set -euo pipefail

CONTEXT=kind-flint-dev
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if ! command -v kubectl >/dev/null; then
  echo "error: kubectl is not on PATH" >&2
  exit 2
fi
if [ -z "${KUBECONFIG:-}" ]; then
  echo "error: set KUBECONFIG to the dev cluster's kubeconfig (KUBECONFIG=.dev/kubeconfig $0)" >&2
  exit 2
fi
case "$KUBECONFIG" in
  *:*) echo "error: KUBECONFIG must name exactly one file" >&2; exit 2 ;;
esac
if [ ! -f "$KUBECONFIG" ]; then
  echo "error: KUBECONFIG $KUBECONFIG does not exist" >&2
  exit 2
fi
# Absolute, so it still names the same file after the cd below.
KUBECONFIG="$(cd "$(dirname "$KUBECONFIG")" && pwd)/$(basename "$KUBECONFIG")"
export KUBECONFIG
current="$(kubectl --kubeconfig "$KUBECONFIG" config current-context 2>/dev/null || true)"
if [ "$current" != "$CONTEXT" ]; then
  echo "error: refusing to record: current context is '$current', not '$CONTEXT'" >&2
  exit 2
fi
# Neither kubectl nor the recorder reads Flint config; a scratch dir keeps the rule anyway.
export FLINT_CONFIG_DIR="${FLINT_CONFIG_DIR:-$(mktemp -d)}"

OUT="${1:-$ROOT/crates/flint-e2e/fixtures}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

cd "$ROOT"
# The recorder checks the same things again before it connects.
cargo run --locked -p flint-e2e --bin record-fixtures -- "$OUT"
