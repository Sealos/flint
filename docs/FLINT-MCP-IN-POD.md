# Run `flint-mcp` in a Kubernetes pod

The read-only `flint-mcp` server uses stdin and stdout for MCP. It builds its Kubernetes client from a kubeconfig
context and has no automatic in-cluster configuration. Set `KUBECONFIG` to a dedicated file that references the
pod's mounted ServiceAccount token and CA. Flint exposes the context only when `settings.toml` pins it to the API
server URL.

## Grant namespace-scoped reads

Use an existing namespace that contains the workloads you want to inspect. This example calls it `flint-example`.
Apply these resources to create a dedicated ServiceAccount with permission to list and get pods, get pod logs,
and list core API events in that namespace.

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: flint-mcp
  namespace: flint-example
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: flint-mcp-read
  namespace: flint-example
rules:
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["get", "list"]
  - apiGroups: [""]
    resources: ["pods/log"]
    verbs: ["get"]
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["list"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: flint-mcp-read
  namespace: flint-example
subjects:
  - kind: ServiceAccount
    name: flint-mcp
    namespace: flint-example
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: flint-mcp-read
```

Flint also uses `GET /version` for the reachability probe and `GET /api` and `GET /apis` for discovery, with
version-specific discovery paths as a fallback. The example assumes the API server's existing permissions allow
those requests for the ServiceAccount.

## Configure the API server and scope

Use an HTTPS API server URL reachable from the pod and covered by the server's TLS certificate. If you replace
`https://kubernetes.default.svc`, use the same URL in the kubeconfig and `settings.toml` below. Kubernetes supplies
the token and CA through the ServiceAccount volume. Apply this ConfigMap before creating the pod.

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: flint-mcp-config
  namespace: flint-example
data:
  kubeconfig: |
    apiVersion: v1
    kind: Config
    clusters:
      - name: pod-api
        cluster:
          server: https://kubernetes.default.svc
          certificate-authority: /var/run/secrets/kubernetes.io/serviceaccount/ca.crt
    users:
      - name: flint-mcp
        user:
          tokenFile: /var/run/secrets/kubernetes.io/serviceaccount/token
    contexts:
      - name: pod-api
        context:
          cluster: pod-api
          user: flint-mcp
          namespace: flint-example
    current-context: pod-api
  settings.toml: |
    [[ai_exposed_clusters]]
    context = "pod-api"
    server = "https://kubernetes.default.svc"
    namespaces = ["flint-example"]
    kinds = ["pods"]
```

Kubernetes RBAC limits what the ServiceAccount can read. Flint also limits MCP calls to the pinned context,
namespace, and kind. Keep both scope lists populated. An empty namespace list allows reads across namespaces.
An empty kind list allows all namespaced kinds. Flint returns pod events through `get_events` because it checks
the kind of the object each event concerns.

## Mount the files and connect over stdio

Replace the image placeholder with a Linux image that provides `/usr/local/bin/flint-mcp` and `/bin/sleep` for
UID `65532`, then apply the Pod manifest. The container stays running so an MCP client can start `flint-mcp` on
demand. The configuration files are mounted read-only, with writable space for Flint's activity and diagnostic logs.

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: flint-mcp
  namespace: flint-example
spec:
  serviceAccountName: flint-mcp
  automountServiceAccountToken: true
  securityContext:
    runAsUser: 65532
    runAsGroup: 65532
    fsGroup: 65532
  containers:
    - name: flint-mcp
      image: registry.example.invalid/flint-mcp:example
      command: ["/bin/sleep", "2147483647"]
      env:
        - name: KUBECONFIG
          value: /etc/flint/kubeconfig
        - name: FLINT_CONFIG_DIR
          value: /var/lib/flint
        - name: XDG_STATE_HOME
          value: /var/lib/flint
      volumeMounts:
        - name: config
          mountPath: /etc/flint
          readOnly: true
        - name: state
          mountPath: /var/lib/flint
        - name: config
          mountPath: /var/lib/flint/settings.toml
          subPath: settings.toml
          readOnly: true
  volumes:
    - name: config
      configMap:
        name: flint-mcp-config
    - name: state
      emptyDir: {}
```

Use this as the MCP client's stdio command once the pod is running. Keep stdin open with `-i` and leave TTY
allocation off. The identity running `kubectl` needs permission to exec into the pod separately from the pod's
ServiceAccount. Recreate the pod after changing the ConfigMap so both mounted files use the same configuration.

```sh
kubectl exec -i -n flint-example flint-mcp -- /usr/local/bin/flint-mcp
```

Call `list_clusters` with `{}` and look for `pod-api` with `reachable: true`. This Role omits cluster-scoped
namespace reads, so `scope.unchecked_namespaces` can appear even when the version probe succeeds. To check
namespaced access, call `list_resources` with these arguments.

```json
{"cluster":"pod-api","kind":"pods","namespace":"flint-example"}
```

Call `list_resource_kinds` with `cluster: "pod-api"` to see the scoped kinds. Use `get_resource` for a named
pod, `get_pod_logs` for its logs, and `get_events` with `object_kind: "Pod"` for pod events. A pod read in a
different namespace should return an out-of-scope error.

The kubeconfig loading, mounted-file authentication, read-only client, and scope checks were verified in source.
Deployment and authentication in a running pod remain unverified.
