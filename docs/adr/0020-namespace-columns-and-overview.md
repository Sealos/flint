# The Namespaces list shows health and usage, and a namespace opens on its own overview

The server's Namespaces Table has three columns: Name, Status and Age. To find the namespace that is broken or full,
the user has to open Pods, filter by each namespace in turn, and compare. Flint therefore adds columns it computes
itself, and a row opens a dashboard scoped to that namespace.

- **New columns on the Namespaces list**, computed by Flint:
  - **Problems:** failing or crash-looping pods, and Deployments, StatefulSets or DaemonSets short of their desired
    replicas, with the same definitions as the dashboard's "Problems now"
    ([ADR-0017](./0017-cluster-dashboards.md)).
  - **Pods:** a count.
  - **CPU** and **Memory:** each "used / requested". Usage comes from metrics-server, and the used part is blank
    without it ([ADR-0005](./0005-metrics-server-only.md)). Requests are summed from the pod specs.
- **Sort:** the default stays by name. A column click changes it, and the choice is remembered per cluster.
- **Namespace overview:** a row click opens the cluster dashboard scoped to the namespace. It shows problem counts
  that open filtered lists, CPU and memory usage against requests with sparklines of the last hour, recent Warning
  events and rollouts, and ResourceQuota and LimitRange use when the namespace has them. A **YAML** tab shows the
  namespace object, and the row's context menu also has **Show YAML**.
- **Sequencing:** the columns ship first. They need only the pod and workload watches and the metrics-server poll. The
  overview lands right after the cluster dashboard, built from its components. Until then a row click keeps opening
  the detail pane.
- **Agents:** nothing now. Later, the dashboard's `get_cluster_summary` MCP tool takes an optional namespace, within
  the cluster's exposure scope and redaction ([ADR-0013](./0013-agent-trust-model.md)).
- **Cluster sets** ([ADR-0014](./0014-cluster-sets-and-combined-views.md)): namespaces with the same name merge into
  one row. Its columns are summed across members, and a member count shows ("3 clusters"). The status shown is the
  worst across members (Terminating beats Active), and the age is the oldest. A chevron expands one child row per
  member cluster, each with its colour tag. Clicking a merged row opens a set-scoped namespace overview: one card per
  member for that namespace (problems, usage, sparklines) under a totals strip. A card opens that member's namespace
  overview.

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
- **One row per member namespace in a cluster set:** keeps each row's cluster, but hides that `payments` is the same
  namespace on three clusters, and triples the list.
- **Expand only, when a merged row is clicked:** no new view, but leaves no place to compare one namespace across
  members.

## Consequences

- The Namespaces list counts as an open view of Pods, the replica workloads and pod metrics, so it watches and polls
  them while it shows, in either watch mode, as the dashboard does.
- Rows keep only metadata and Table cells today. The problem rules and summed requests need a few typed fields from
  each pod and workload (phase, waiting reasons, readiness, resource requests, replica status), kept per row and not
  the whole object.
- Lists do not remember their sort today. This adds a remembered sort, per cluster and kind, to `state.toml`, which
  every list can use.
- The dashboard's components take a namespace scope, so the cluster and namespace overviews share one
  implementation.
- In a cluster set, the merged row and its set-scoped overview follow the set dashboard's card layout (UP-85).
