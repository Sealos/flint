# Each cluster opens on a dashboard, with an hour of in-memory trends

After connecting, Flint used to show "Pick a kind". Users want to know straight away whether anything is wrong and
how full the cluster is. Each cluster therefore opens on a **dashboard**, also reachable from an **Overview** entry at
the top of the resource tree, and a cluster set ([ADR-0014](./0014-cluster-sets-and-combined-views.md)) gets one
that compares its members.

**Per cluster** (one fixed layout, tiles with nothing to show collapse):

1. **Problems now**, each a count that opens the matching filtered list: failing or crash-looping pods, not-ready
   nodes, Warning events in the last hour, pending PersistentVolumeClaims, failed Jobs, and Deployments,
   StatefulSets or DaemonSets short of their desired replicas.
2. **Capacity and usage** from metrics-server: CPU and memory used against allocatable, and requests and limits
   against capacity, per node and in total, each with a **sparkline of the last hour**. Without metrics-server the
   tiles show a hint ([ADR-0005](./0005-metrics-server-only.md)).
3. **Recent activity**: a live feed of Warning events and rollouts, newest first.

**Per cluster set**: one compact card per member (colour tag, problem counts, CPU and memory gauges with sparklines),
sorted by most problems first, under a totals strip. A count opens the combined list filtered to it.

- **Trends are Flint's own samples, kept in memory.** While a cluster is connected (active, warm, or a set member),
  Flint keeps its 15-second metrics-server samples for the last hour in a bounded ring buffer per series. Nothing is
  written to disk and no Prometheus is queried. A cluster that goes cold drops its samples.
- **Data:** the dashboard counts as an open view of the kinds it needs, so it watches them while it is showing, in
  either watch mode ([ADR-0008](./0008-watch-everything-on-active-cluster.md)), and stops when the user leaves.
- **Thresholds are configurable:** typed defaults in `settings.toml` (for example CPU and memory warnings at 90% of
  allocatable, restarts in the last hour, Warning events in the last hour) with optional per-cluster overrides,
  edited from a Thresholds panel on the dashboard.
- **Agents:** no dashboard tool. The MCP server stays at its six read-only tools
  ([ADR-0009](./0009-read-only-mcp-server.md)).

## Considered Options

- **Keep "Pick a kind":** cheap, but the usability review found the first screen empty and unhelpful.
- **Current values only:** a spike two minutes ago would be invisible.
- **Prometheus history:** real history over days, but Prometheus discovery and PromQL per kind is a feature of its
  own (ADR-0005), and many clusters have no Prometheus.
- **Poll counts in "open views only" mode:** lighter on huge clusters, but up to 30 s stale and a second data path.
- **User-arranged tiles:** a layout model and editor to build and test for little gain over a well-ordered fixed
  layout.
- **Fixed thresholds:** simpler, but production and dev clusters need different warning levels.
- **Totals-only or a merged problem list for cluster sets:** both hide which cluster is unhealthy, or lose the
  capacity view.

## Consequences

- ARCHITECTURE.md's non-goal "Prometheus charts or any metric history" narrows to stored or Prometheus history;
  an hour of in-memory samples is in scope.
- Memory grows with the number of series (nodes × metrics × connected clusters); each ring buffer is bounded
  (240 samples), and the memory soak test covers a dashboard left open.
- The dashboard opens extra watches on large clusters while it is showing, which the warm-cluster memory budget
  (ADR-0012) and the kwok measurements must include.
