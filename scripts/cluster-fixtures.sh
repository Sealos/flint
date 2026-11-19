#!/usr/bin/env bash
# Install what Flint's cluster tests expect on a fresh kind cluster: metrics-server, the demo
# namespace (Deployment and Service `web`, Helm release `podinfo`), and a compacted etcd. Used by
# scripts/dev-cluster.sh and by the cluster-e2e CI job, so local runs and CI test the same cluster.
#
#   KUBECONFIG=<kind kubeconfig> scripts/cluster-fixtures.sh
#
# Guards, so this never touches ~/.kube/config or a real cluster: KUBECONFIG must name exactly one
# file, its current context must be a kind cluster (`kind-*`) served from this machine or the
# kind control-plane container, and every kubectl and helm call is pinned to that context.
#
# Versions are pinned for reproducible runs; docs/TESTING.md says how to bump them.
set -euo pipefail

METRICS_SERVER_VERSION=v0.9.1
PODINFO_CHART_VERSION=6.17.0

fail() {
  echo "cluster-fixtures: $*" >&2
  exit 2
}

if [ -z "${KUBECONFIG:-}" ]; then
  fail "set KUBECONFIG to the kind cluster's kubeconfig"
fi
case "$KUBECONFIG" in
  *:*) fail "KUBECONFIG must name a single file, not a list: $KUBECONFIG" ;;
esac
# helm reads these as defaults for its flags; none may point it elsewhere.
unset HELM_KUBECONTEXT HELM_KUBEAPISERVER HELM_KUBETOKEN HELM_KUBEASUSER

context="$(kubectl config current-context 2>/dev/null)" || fail "no current context in $KUBECONFIG"
case "$context" in
  kind-*) ;;
  *) fail "refusing to touch context \"$context\": not a kind cluster" ;;
esac
server="$(kubectl config view --minify --context "$context" -o jsonpath='{.clusters[0].cluster.server}')"
host="${server#https://}"
host="${host%%/*}"
case "$host" in
  127.0.0.1:* | localhost:* | '[::1]':* | "${context#kind-}-control-plane":*) ;;
  *) fail "refusing to touch context \"$context\": its server $server is not a local kind cluster" ;;
esac

k() { kubectl --context "$context" "$@"; }

k apply -f "https://github.com/kubernetes-sigs/metrics-server/releases/download/$METRICS_SERVER_VERSION/components.yaml"
# kind kubelets use self-signed certs. The apply above resets the args, so this adds the flag once.
k -n kube-system patch deployment metrics-server --type=json \
  -p='[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--kubelet-insecure-tls"}]'
k create namespace demo --dry-run=client -o yaml | k apply -f -
k -n demo create deployment web --image=nginx:1.27 --replicas=2 --dry-run=client -o yaml | k apply -f -
k -n demo expose deployment web --port=80 --dry-run=client -o yaml | k apply -f -
helm --kube-context "$context" upgrade --install podinfo oci://ghcr.io/stefanprodan/charts/podinfo \
  --version "$PODINFO_CHART_VERSION" -n demo --wait --timeout 3m

# The cluster tests read pod metrics; metrics-server needs a couple of scrapes before it has any.
k -n kube-system rollout status deployment/metrics-server --timeout=3m
k -n demo rollout status deployment/web --timeout=3m
echo "cluster-fixtures: waiting for pod metrics…"
metrics_ready=
for _ in $(seq 60); do
  metrics="$(k get --raw /apis/metrics.k8s.io/v1beta1/namespaces/demo/pods 2>/dev/null || true)"
  case "$metrics" in *'"containers"'*) metrics_ready=1 && break ;; esac
  sleep 5
done
if [ -z "$metrics_ready" ]; then
  echo "cluster-fixtures: metrics-server reported no pod metrics within 5 minutes" >&2
  k get apiservice v1beta1.metrics.k8s.io -o wide >&2 || true
  k -n kube-system logs deployment/metrics-server --tail=50 >&2 || true
  exit 1
fi

# A new cluster still holds its whole etcd history, so a watch from an old resourceVersion replays
# it instead of failing with 410 Expired. The API server compacts only every 5 minutes, so compact
# now: the cluster tests check the 410 path, and CI runs them minutes after creating the cluster.
etcd_pod="$(k -n kube-system get pod -l component=etcd -o jsonpath='{.items[0].metadata.name}')"
etcdctl() {
  k -n kube-system exec "$etcd_pod" -- etcdctl \
    --cacert=/etc/kubernetes/pki/etcd/ca.crt \
    --cert=/etc/kubernetes/pki/etcd/healthcheck-client.crt \
    --key=/etc/kubernetes/pki/etcd/healthcheck-client.key "$@"
}
revision="$(etcdctl endpoint status -w json | grep -oE '"revision":[0-9]+' | head -1 | cut -d: -f2 || true)"
if [ -z "$revision" ]; then
  echo "cluster-fixtures: could not read etcd revision from $etcd_pod" >&2
  exit 1
fi
# Fails only when nothing changed since the last compaction, which is fine on a re-run.
etcdctl compact "$revision" >/dev/null || true
# The 410 comes as an ERROR event inside a 200 response. A 410 as the HTTP status would mean the
# same, but kubectl reports it as a failure, so tell the cases apart.
if watch="$(k get --raw '/api/v1/pods?watch=1&resourceVersion=1&timeoutSeconds=1' 2>&1)"; then
  case "$watch" in
    *'"code":410'*) ;;
    *)
      echo "cluster-fixtures: a watch from resourceVersion 1 still replays history after compaction: ${watch:0:200}" >&2
      exit 1
      ;;
  esac
else
  case "$watch" in
    *410* | *Expired* | *Gone*) ;;
    *)
      echo "cluster-fixtures: the watch from resourceVersion 1 failed: ${watch:0:200}" >&2
      exit 1
      ;;
  esac
fi
echo "cluster-fixtures: done"
