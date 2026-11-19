#!/usr/bin/env bash
# Install what Flint's cluster tests expect on a fresh kind cluster: metrics-server, the demo
# namespace (Deployment and Service `web`, Helm release `podinfo`), and a compacted etcd. Used by
# scripts/dev-cluster.sh and by the cluster-e2e CI job, so local runs and CI test the same cluster.
#
#   KUBECONFIG=<kind kubeconfig> scripts/cluster-fixtures.sh
#
# KUBECONFIG must be set and its current context must be a kind cluster (`kind-*`), so this never
# runs against ~/.kube/config or a real cluster. Safe to run again.
set -euo pipefail

if [ -z "${KUBECONFIG:-}" ]; then
  echo "cluster-fixtures: set KUBECONFIG to the kind cluster's kubeconfig" >&2
  exit 2
fi
context="$(kubectl config current-context)"
case "$context" in
  kind-*) ;;
  *)
    echo "cluster-fixtures: refusing to touch context \"$context\": not a kind cluster" >&2
    exit 2
    ;;
esac

kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
# kind kubelets use self-signed certs
kubectl -n kube-system patch deployment metrics-server --type=json \
  -p='[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--kubelet-insecure-tls"}]' || true
kubectl create namespace demo --dry-run=client -o yaml | kubectl apply -f -
kubectl -n demo create deployment web --image=nginx:1.27 --replicas=2 --dry-run=client -o yaml | kubectl apply -f -
kubectl -n demo expose deployment web --port=80 --dry-run=client -o yaml | kubectl apply -f -
helm upgrade --install podinfo oci://ghcr.io/stefanprodan/charts/podinfo -n demo --wait --timeout 3m

# The cluster tests read pod metrics; metrics-server needs a couple of scrapes before it has any.
kubectl -n kube-system rollout status deployment/metrics-server --timeout=3m
kubectl -n demo rollout status deployment/web --timeout=3m
metrics_ready=
for _ in $(seq 60); do
  metrics="$(kubectl get --raw /apis/metrics.k8s.io/v1beta1/namespaces/demo/pods 2>/dev/null || true)"
  case "$metrics" in *'"containers"'*) metrics_ready=1 && break ;; esac
  sleep 5
done
if [ -z "$metrics_ready" ]; then
  echo "cluster-fixtures: metrics-server reported no pod metrics within 5 minutes" >&2
  exit 1
fi

# A new cluster still holds its whole etcd history, so a watch from an old resourceVersion replays
# it instead of failing with 410 Expired. The API server compacts only every 5 minutes, so compact
# now: the cluster tests check the 410 path, and CI runs them minutes after creating the cluster.
etcd_pod="$(kubectl -n kube-system get pod -l component=etcd -o jsonpath='{.items[0].metadata.name}')"
etcdctl() {
  kubectl -n kube-system exec "$etcd_pod" -- etcdctl \
    --cacert=/etc/kubernetes/pki/etcd/ca.crt \
    --cert=/etc/kubernetes/pki/etcd/healthcheck-client.crt \
    --key=/etc/kubernetes/pki/etcd/healthcheck-client.key "$@"
}
revision="$(etcdctl endpoint status -w json | grep -oE '"revision":[0-9]+' | head -1 | cut -d: -f2)"
# Fails only when nothing changed since the last compaction, which is fine on a re-run.
etcdctl compact "$revision" >/dev/null || true
case "$(kubectl get --raw '/api/v1/pods?watch=1&resourceVersion=1&timeoutSeconds=1')" in
  *'"code":410'*) ;;
  *)
    echo "cluster-fixtures: a watch from resourceVersion 1 still replays history after compaction" >&2
    exit 1
    ;;
esac
echo "cluster-fixtures: done"
