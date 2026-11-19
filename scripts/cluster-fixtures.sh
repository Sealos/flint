#!/usr/bin/env bash
# Install what Flint's cluster tests expect on a fresh kind cluster: metrics-server, the demo
# namespace (Deployment and Service `web`, Helm release `podinfo`), and old resourceVersions
# expired, as on a long-lived cluster (etcd compacted, API server restarted). Used by
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

k -n kube-system rollout status deployment/metrics-server --timeout=3m
k -n demo rollout status deployment/web --timeout=3m

# The cluster tests check that a watch from an old resourceVersion fails with 410 Expired, as it
# does on any cluster that has run for a while. A new one is different twice over: etcd still holds
# its whole history (the API server compacts only every 5 minutes), and the API server's watch
# caches started at about resourceVersion 1, so they serve a watch from there out of memory. So
# compact etcd, then restart the API server: its caches start again at the current revision, and
# older versions go to etcd, which answers 410.
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

# The kind node container; the kubelet starts the stopped API server again.
node="${context#kind-}-control-plane"
echo "cluster-fixtures: restarting the API server in $node"
apiserver_id() { docker exec "$node" crictl ps -q --name kube-apiserver 2>/dev/null | head -1; }
old_id="$(apiserver_id)"
if [ -z "$old_id" ]; then
  echo "cluster-fixtures: no running kube-apiserver container in $node" >&2
  exit 1
fi
# `crictl stop` can time out on a busy runner (DeadlineExceeded) even though the container stops.
# So don't trust its exit status: retry it, and count the restart as done once the kubelet runs a
# new kube-apiserver container.
restarted=
for attempt in 1 2 3; do
  docker exec "$node" crictl --timeout 60s stop "$old_id" >/dev/null 2>&1 ||
    echo "cluster-fixtures: crictl stop attempt $attempt did not report success; checking the container"
  for _ in $(seq 30); do
    new_id="$(apiserver_id)"
    if [ -n "$new_id" ] && [ "$new_id" != "$old_id" ]; then restarted=1 && break 2; fi
    sleep 2
  done
done
if [ -z "$restarted" ]; then
  echo "cluster-fixtures: the API server in $node was not restarted after 3 attempts" >&2
  exit 1
fi
ready=
for _ in $(seq 60); do
  if k get --raw /readyz >/dev/null 2>&1; then ready=1 && break; fi
  sleep 2
done
if [ -z "$ready" ]; then
  echo "cluster-fixtures: the API server did not come back within 2 minutes" >&2
  exit 1
fi

# The 410 comes as an ERROR event inside a 200 response. A 410 as the HTTP status would mean the
# same, but kubectl reports it as a failure, so tell the cases apart.
expired=
for _ in $(seq 15); do
  if watch="$(k get --raw '/api/v1/pods?watch=1&resourceVersion=1&timeoutSeconds=1' 2>&1)"; then
    case "$watch" in
      *'"code":410'*) expired=1 && break ;;
      *) problem="a watch from resourceVersion 1 still replays history: ${watch:0:200}" ;;
    esac
  else
    case "$watch" in
      *410* | *Expired* | *Gone*) expired=1 && break ;;
      *) problem="the watch from resourceVersion 1 failed: ${watch:0:200}" ;;
    esac
  fi
  sleep 2
done
if [ -z "$expired" ]; then
  echo "cluster-fixtures: $problem" >&2
  exit 1
fi

# The cluster tests read pod metrics; metrics-server needs a couple of scrapes before it has any,
# and its API is briefly unavailable after the restart.
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
echo "cluster-fixtures: done"
