#!/usr/bin/env bash
# Create or delete the throwaway kind cluster used for Flint development and tests.
# Its kubeconfig is written to .dev/kubeconfig so ~/.kube/config is never touched.
#
#   scripts/dev-cluster.sh up      create cluster, install the test fixtures (scripts/cluster-fixtures.sh)
#   scripts/dev-cluster.sh down    delete the cluster
#   scripts/dev-cluster.sh env     print `export KUBECONFIG=...` for the dev cluster
set -euo pipefail

NAME=flint-dev
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KCFG="$ROOT/.dev/kubeconfig"

case "${1:-up}" in
  up)
    mkdir -p "$ROOT/.dev"
    if ! kind get clusters | grep -qx "$NAME"; then
      kind create cluster --name "$NAME" --kubeconfig "$KCFG" --wait 120s
    fi
    KUBECONFIG="$KCFG" "$ROOT/scripts/cluster-fixtures.sh"
    echo "Dev cluster ready. Use: export KUBECONFIG=$KCFG"
    ;;
  down)
    kind delete cluster --name "$NAME"
    rm -f "$KCFG"
    ;;
  env)
    echo "export KUBECONFIG=$KCFG"
    ;;
  *)
    echo "usage: $0 up|down|env" >&2; exit 2
    ;;
esac
