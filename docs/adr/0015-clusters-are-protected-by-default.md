# Clusters are protected (read-only) by default

A kubeconfig usually holds production clusters next to dev ones, and one wrong click in a GUI can delete or scale
the wrong thing. Flint therefore treats every cluster as **protected** until the user says otherwise. A protected
cluster is read-only in Flint: browsing, logs and the detail view work, but every action that changes the cluster
(edit, apply, create, delete, scale, restart, cordon, drain, Helm changes, exec, port-forward, debug containers) is
disabled, and says why.

- **Unlocked by default:** known local clusters: a context named exactly `kind`, `minikube`, `docker-desktop` or
  `orbstack`, or starting with exactly `kind-`, whose API server is on the user's own machine (`localhost`,
  `127.0.0.1`, `::1`) and whose cluster has no `proxy-url`. Docker Desktop's `kubernetes.docker.internal`, names under
  `.localhost` and the rest of `127.0.0.0/8` count as this machine too, as the kubeconfig names them, never resolved.
  Every other context on a local address takes Unlock or Trust like any other cluster, and the
  `protect_local_clusters` setting protects the known local ones as well.
- **Trusted clusters:** the user can mark any cluster trusted, which keeps it unlocked across restarts. Trust is a
  typed `trusted_clusters` setting, pinned to the context name and API server URL like AI exposure, so a re-pointed
  context is protected again.
- **Session unlock:** a dialog names the cluster and its server, and one click unlocks it until Flint quits. The
  sidebar shows a lock icon for each state (protected, unlocked for this session, trusted). **Lock Again** is in the
  cluster's menu and the palette.
- **Fleet actions** (v0.7) on a set: the user picks an explicit target list, sees a server-side dry-run diff
  per cluster with checkboxes to drop clusters, and the action runs **one cluster at a time, stopping at the first
  failure**. Any locked cluster in the target list must have its name typed to proceed, and is unlocked for that
  action only. Fleet actions are rollout restart, scale, apply YAML and delete.

## Considered Options

- **Unprotected by default with an opt-in protect flag** (k9s `readOnly`, Aptakube's change protection): relies on
  the user remembering to protect production before the first mistake.
- **Protect only contexts whose names contain "prod":** names are unreliable (`eks-main`, `customer-a`).
- **Type the cluster's name for every unlock:** safer against a stray click, but slow for the common case; it is
  kept for fleet actions, where one confirmation covers several clusters.
- **Unlocks that expire after idle time:** considered; Flint relocks on quit instead, and Lock Again is one action.
- **Fleet actions all at once:** faster, but a bad change reaches every cluster before the user can react.

## Consequences

- Every write path in `flint-core` takes the cluster's protection state and refuses when protected, so the UI
  cannot bypass it by mistake. `flint mcp` stays read-only regardless ([ADR-0009](./0009-read-only-mcp-server.md)).
- Protection lands with M3, the first milestone with write actions. Exec and port-forward (M2, M5) count as
  writes, because they run code in, or open a path into, the cluster; on a protected cluster they ask for an
  unlock. Until M3 ships protection, they work as today.
- **Kubeconfig changes and resource writes:** before each request of a reviewed resource write, Flint reads the
  kubeconfig files from disk and compares them to the exact connection definition the write is pinned to. A read that
  fails, times out, or is blank, provisional or changed refuses the write. A change made after that read and before
  the request is sent is not detected. Opening a shell uses a separate launch check against the loaded kubeconfig.
- **Reviewed resource writes with exec and auth-provider credentials need Kubernetes 1.28 or newer.** These tokens can
  rotate at any time, so a review is bound to the identity the server reports for the credential, not to the token.
  Flint asks for it with a `SelfSubjectReview` (`authentication.k8s.io/v1`, served from Kubernetes 1.28) before each
  request of the write. On an older server that API is missing, so Flint refuses the write before sending it. Token
  files and client certificates are compared directly and have no such requirement. Shells and reading a cluster do
  not use this API, so they keep working on older servers.
- New users with only remote clusters see actions disabled until they unlock or trust a cluster. The disabled
  state explains this and offers the unlock in place.
