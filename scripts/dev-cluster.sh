#!/usr/bin/env bash
# Create or delete the throwaway kind cluster used for Flint development and tests.
# Its kubeconfig is written to .dev/kubeconfig so ~/.kube/config is never touched.
#
#   scripts/dev-cluster.sh up      create cluster, install metrics-server and a sample Helm release
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
    export KUBECONFIG="$KCFG"
    kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
    # kind kubelets use self-signed certs
    kubectl -n kube-system patch deployment metrics-server --type=json \
      -p='[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--kubelet-insecure-tls"}]' || true
    kubectl create namespace demo --dry-run=client -o yaml | kubectl apply -f -
    kubectl -n demo create deployment web --image=nginx:1.27 --replicas=2 --dry-run=client -o yaml | kubectl apply -f -
    kubectl -n demo expose deployment web --port=80 --dry-run=client -o yaml | kubectl apply -f -
    helm upgrade --install podinfo oci://ghcr.io/stefanprodan/charts/podinfo -n demo --wait --timeout 3m
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
