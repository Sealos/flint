#!/usr/bin/env bash
# Create or delete the throwaway kind cluster used for Flint development and tests.
# Its kubeconfig is written to .dev/kubeconfig so ~/.kube/config is never touched.
#
#   scripts/dev-cluster.sh up      create cluster, install the test fixtures (scripts/cluster-fixtures.sh)
#   scripts/dev-cluster.sh down    delete the cluster
#   scripts/dev-cluster.sh env     print `export KUBECONFIG=...` for the dev cluster
#   scripts/dev-cluster.sh node-image   print the pinned kind node image (CI uses it too)
set -euo pipefail

NAME=flint-dev
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KCFG="$ROOT/.dev/kubeconfig"
# The Kubernetes version the tests run against: kind v0.33.0's default for 1.37. To bump it, see
# docs/TESTING.md.
NODE_IMAGE=kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5

case "${1:-up}" in
  up)
    mkdir -p "$ROOT/.dev"
    if ! kind get clusters | grep -qx "$NAME"; then
      kind create cluster --name "$NAME" --image "$NODE_IMAGE" --kubeconfig "$KCFG" --wait 120s
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
  node-image)
    echo "$NODE_IMAGE"
    ;;
  *)
    echo "usage: $0 up|down|env|node-image" >&2; exit 2
    ;;
esac
