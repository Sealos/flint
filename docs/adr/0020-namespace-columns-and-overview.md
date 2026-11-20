# The Namespaces list shows health and usage, and a namespace opens on its own overview

The server's Namespaces Table has three columns: Name, Status and Age. To find the namespace that is broken or full,
the user has to open Pods, filter by each namespace in turn, and compare. Flint therefore adds columns it computes
itself, and a row opens a dashboard scoped to that namespace.

- **New columns on the Namespaces list**, computed by Flint:
  - **Problems:** failing or crash-looping pods, and Deployments, StatefulSets or DaemonSets short of their desired
    replicas that are not rolling out. These are the pod and workload rules of the dashboard's "Problems now"
    ([ADR-0017](./0017-cluster-dashboards.md)), shared in one place, so the two counts agree. The dashboard's other
    problems (not-ready nodes, Warning events, pending PVCs, failed Jobs) are not in this column, and its hover says
    "failing pods and stalled workloads".
  - **Pods:** a count.
  - **CPU** and **Memory:** each "used / requested", for example "120m / 500m". Usage comes from metrics-server
    ([ADR-0005](./0005-metrics-server-only.md)). Requests are each pod's effective request, as the scheduler computes
    it, summed over the pods that have not finished.
- **Empty cells say why:** with nothing requested the cell shows the usage alone, with "no requests" on hover. Without
  metrics-server the used part is blank, with "metrics-server is not installed" on hover; when RBAC forbids reading
  metrics, it is blank with "metrics not allowed" instead.
- **Unavailable, not zero:** a namespace whose pods or workloads Flint does not watch (namespace-limited access) shows
  "unavailable" in these columns, not 0.
- **Sort:** the default stays by name. A column click changes it, and the choice is remembered per cluster.
- **Namespace overview:** a row click opens the cluster dashboard scoped to the namespace, in the main view in place
  of the list, as the cluster dashboard opens; Back returns to the list. It shows problem counts, recent Warning
  events and rollouts, CPU and memory usage against requests with sparklines, and ResourceQuota use and LimitRange
  limits when the namespace has them. A **YAML** tab shows the namespace object, and the row's context menu also has
  **Show YAML**.
- **Counts open lists filtered to the namespace** with a filter of the opened list's own, shown in its header and
  dropped when the user leaves it. The cluster's namespace filter does not change.
- **Sparklines start when someone looks:** Flint records a namespace's usage samples while its overview, or the
  Namespaces list, is open, and the sparkline fills in from then, up to an hour. A namespace's samples are dropped
  when neither is open. Nothing is kept for namespaces nobody looks at.
- **Sequencing:** the columns ship first. They need only the workload watches, the Pods list, and the metrics-server
  poll. The overview lands right after the cluster dashboard, built from its components. Until then a row click keeps
  opening the detail pane.
- **Agents:** nothing now. Later, the dashboard's `get_cluster_summary` MCP tool takes an optional namespace, within
  the cluster's exposure scope and redaction ([ADR-0013](./0013-agent-trust-model.md)).
- **Cluster sets (deferred until cluster sets are scheduled)** ([ADR-0014](./0014-cluster-sets-and-combined-views.md)):
  namespaces with the same name merge into one row. Its columns are summed across members, and a member count shows
  ("3 clusters"). The status shown is the worst across members (Terminating beats Active), and the age is the oldest.
  When a member has no metrics, the merged usage is marked partial and compared only against the requests of the
  members that have usage. A chevron expands one child row per member cluster, each with its colour tag. Clicking a
  merged row opens a set-scoped namespace overview: one card per member for that namespace (problems, usage,
  sparklines) under a totals strip. A card opens that member's namespace overview.

## Considered Options

- **Live usage only:** shows what is busy now, but not whether a namespace is short of what it asked for.
- **Requests only:** works without metrics-server, but says nothing about real load.
- **Keep the detail pane on click:** no new view, but the pane has no room for problems, usage and activity.
- **Scope the app to the namespace on click:** sets the namespace filter and moves on, but changes a filter the user
  set elsewhere, and still gives no summary.
- **An overview with a workload list:** familiar, but repeats the Deployments and Pods lists one click away.
- **A diagram-first overview:** opens on the resource diagram, which shows structure well but hides counts and usage.
  The diagram stays one action away.
- **Sort by problems first:** puts the broken namespaces on top, but the list would reorder under the user as
  problems come and go. A click on Problems gives the same order on purpose.
- **Ship everything with the dashboards:** one release, but delays columns that need nothing from the dashboard.
- **A namespace MCP tool now:** agents can already list pods and events per namespace. A summary belongs with
  `get_cluster_summary` once it exists.
- **Always sample every namespace:** every overview would open with a full hour, but a cluster with hundreds of
  namespaces would keep hundreds of series nobody reads.
- **Keep the top N namespaces:** bounds the memory, but which namespaces have history would depend on a ranking the
  user cannot see.
- **One row per member namespace in a cluster set:** keeps each row's cluster, but hides that `payments` is the same
  namespace on three clusters, and triples the list.
- **Expand only, when a merged row is clicked:** no new view, but leaves no place to compare one namespace across
  members.

## Consequences

- The Namespaces list counts as an open view of Pods (the Metadata watch, for Problems and Pods), the replica
  workloads and pod metrics, so it watches and polls them while it shows, in either watch mode, as the dashboard
  does. The overview also needs Events, ResourceQuotas and
  LimitRanges.
- Pod problems come from the Pod Table's cells (Status, Ready, Restarts), which Flint already keeps. Pod requests
  are not in the Table, and Pods stay on `includeObject=Metadata` for every other view and for the MCP server. Flint
  reads pod specs through a second Pods watch with `includeObject=Object`, only while the Namespaces list or a
  namespace overview is open, and keeps only the summed requests. Each start re-lists every pod in full (tens of MB
  on a cluster with 10,000 pods), so the watch stays alive for 2 minutes after the last of them closes, which keeps
  moving between the list and an overview from paying that again.
  The kwok measurements that would size this are still planned, and #27 proposes deferring the nightly memory soak
  (#126).
- Workload problems use the typed replica status of [ADR-0021](./0021-replica-ready-column.md), so a healthy rollout
  is never a problem here. Proposed, pending the owner's decision: the dashboard's "short of replicas" count would
  leave out rolling-out workloads too (see #101).
- The effective request follows Kubernetes: pod-level requests when the pod sets them; otherwise the app containers
  plus the restartable init (sidecar) containers, or the largest init container plus the sidecars started before it
  when that is larger; plus pod overhead.
- Lists do not remember their sort today. This adds a remembered sort per cluster to `state.toml`, which every list
  can use.
- The dashboard's components take a namespace scope, so the cluster and namespace overviews share one
  implementation.
- Namespaces becomes the only list whose row click opens a view instead of the detail pane. The overview's YAML tab
  and the row's Show YAML cover what the pane showed.
- This amends [ADR-0014](./0014-cluster-sets-and-combined-views.md), once the deferred part is built: in the
  Namespaces list, a merged row stands for several clusters' objects. Its child rows, and every other list's rows,
  still keep their cluster.
