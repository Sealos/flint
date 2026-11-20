# Workload lists show one Ready column with a status dot, and hide old ReplicaSet revisions

The server's Tables for replica workloads spread their health over several number columns: Desired, Current and
Ready for ReplicaSets, Ready, Up-to-date and Available for Deployments, five columns for DaemonSets. Reading health
means comparing numbers across columns on every row. The ReplicaSets list is also mostly old revisions: every rollout
leaves one behind with 0 replicas. Flint therefore computes one Ready column for these kinds, and hides old revisions
by default.

- **Ready column:** ReplicaSets, Deployments, StatefulSets, DaemonSets and ReplicationControllers replace the
  server's Desired, Current and Ready (and their kind's equivalents) with one Flint-computed **Ready** column:
  "ready/desired" and a status dot.
- **Fields:** ready is `readyReplicas` and desired is `spec.replicas`. For DaemonSets they are `numberReady` and
  `desiredNumberScheduled`. Current is the number of pods that exist (`status.replicas`; for DaemonSets,
  `currentNumberScheduled`), and shows only when it differs from desired ("1/2 · 3 current"). Kind-specific details
  show on hover (for Deployments: up-to-date and available).
- **Dot:** green when all are ready. Amber when some are not, and also when none are ready while the workload is
  rolling out, so a new Deployment that is starting is amber, not red. Red when none are ready, some are wanted, and
  it is not rolling out, or when its rollout is stuck. At 0/0 it is grey and the row is dimmed. During a surge, ready
  can exceed desired: the cell shows the real numbers ("4/3") and the dot is green.
- **Sort by health:** worst first is red, then amber, then green, then grey. Within each, rows sort by the share
  ready (ready divided by desired) from lowest, then by namespace and name. A second click reverses the order.
- **Rollout marker:** for Deployments, StatefulSets and DaemonSets, when fewer replicas are updated than desired, old
  pods still run, or the controller has not yet seen the latest spec, the cell shows a static ↻ glyph and "rolling
  out" ("2/3 ↻ rolling out"). There is no spinner and no animation. A stuck rollout (a Progressing condition with
  reason `ProgressDeadlineExceeded`) turns red, with the condition's message on hover.
- **Paused:** a Deployment with `spec.paused` shows "paused" in place of the rollout marker, and its dot follows the
  counts.
- **Wide** adds the server's raw columns (Desired, Current, Up-to-date, Available, Selector, Images, Containers, as
  each kind returns them) next to Ready, so nothing is lost.
- **Old revisions:** a ReplicaSet owned by a Deployment is hidden by default once both its desired and its observed
  replicas are 0, so one whose pods are still going away stays visible. The list header says how many are hidden
  (for example "84 old revisions hidden") with a **Show** toggle, remembered per cluster. Orphan 0/0 ReplicaSets
  still show. A text filter also matches hidden old revisions and shows them dimmed. This delivers part of UP-64.
- **Jobs and CronJobs** keep their own columns.

## Considered Options

- **Three columns with colour:** keeps the server's layout and colours each number, but the eye still has to compare
  three columns per row.
- **A progress bar:** easy to scan, but it takes width, hides the numbers, and reads as "loading".
- **ReplicaSets only:** the noisiest list gets fixed, but Deployments and DaemonSets would read differently from it.
- **Dim old revisions and sort them last:** nothing is hidden, but they still fill the list and the scroll bar.
- **Leave old revisions as they are:** what `kubectl get rs` shows, and the reason the list is hard to read.
- **Wide keeps only the merged column:** tidier, but anyone matching `kubectl get -o wide` would lose columns.
- **No rollout marker:** simpler, but "2/3" during a healthy rollout looks the same as a broken one.
- **An animated spinner as the rollout marker:** draws the eye, but every rolling-out cell would redraw the list
  constantly, which the tree's static markers avoid on purpose.

## Consequences

- This amends [ADR-0003](./0003-generic-resource-views.md): it is Flint's first change to the columns of a built-in
  kind's Table. Server Tables stay the source for every other column, and Wide shows them all.
- Flint's Ready column has its own identity, apart from the server's Ready column, so sorting, the remembered sort
  and a cluster set's union of columns never confuse the two, even when Wide shows both.
- ReplicaSets and ReplicationControllers need nothing beyond the Table's cells and the metadata: desired, current
  and ready are cells, and the controller owner uid is in the metadata. The resource diagram needs the owner uid
  too, so whichever lands first keeps it.
- Deployments, StatefulSets and DaemonSets need status and spec fields the Table does not carry (updated replicas,
  the Progressing condition, `spec.paused`, the update strategy). Only these three kinds are asked for with
  `includeObject=Object`, and only the typed fields are kept per row. Pods stay on `includeObject=Metadata`. The
  extra bandwidth and memory must fit the warm-cluster budget ([ADR-0012](./0012-keep-recently-used-clusters-warm.md));
  the kwok measurements that would check it are still planned, and #27 proposes deferring the nightly memory soak
  (#126).
- The MCP server keeps the server's columns and `includeObject=Metadata`: `list_resources` returns what it returns
  today, and a test checks this.
- A StatefulSet with a rolling-update partition, or a StatefulSet or DaemonSet with the `OnDelete` strategy, keeps
  fewer pods updated than desired on purpose. Its cell shows no rollout marker and says "partitioned" or "on delete"
  on hover.
- Of the three kinds, only Deployments report a Progressing condition, so only they can show a stuck rollout.
- `state.toml` gains the old-revisions toggle per cluster.
- A cluster set's combined list computes Ready per row from that row's cluster, so the column means the same
  everywhere.
