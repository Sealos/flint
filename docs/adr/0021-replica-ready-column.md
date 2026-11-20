# Workload lists show one Ready column with a status dot, and hide old ReplicaSet revisions

The server's Tables for replica workloads spread their health over several number columns: Desired, Current and
Ready for ReplicaSets, Ready, Up-to-date and Available for Deployments, five columns for DaemonSets. Reading health
means comparing numbers across columns on every row. The ReplicaSets list is also mostly old revisions: every rollout
leaves one behind with 0 replicas. Flint therefore computes one Ready column for these kinds, and hides old revisions
by default.

- **Ready column:** ReplicaSets, Deployments, StatefulSets, DaemonSets and ReplicationControllers replace the
  server's Desired, Current and Ready (and their kind's equivalents) with one Flint-computed **Ready** column:
  "ready/desired" and a status dot. The dot is green when all are ready, amber when some are not, and red when none
  are but some are wanted. At 0/0 it is grey and the row is dimmed.
- Each kind uses its own status fields. For DaemonSets, desired is the desired number scheduled. Current shows only
  when it differs from desired ("1/2 · 3 current"). Kind-specific details show on hover (for Deployments: up-to-date
  and available). The column sorts by health.
- **Rollout marker:** for Deployments, StatefulSets and DaemonSets, when fewer replicas are updated than desired, or
  old pods still run, the cell shows a small spinner and "2/3 · rolling out". A stuck rollout (a Progressing
  condition with reason `ProgressDeadlineExceeded`) turns red, with the condition's message on hover.
- **Wide** adds the server's raw columns (Desired, Current, Up-to-date, Available, Selector, Images, Containers, as
  each kind returns them) next to Ready, so nothing is lost.
- **Old revisions:** ReplicaSets with 0 desired that are owned by a Deployment are hidden by default. The list header
  says "84 old revisions hidden" with a **Show** toggle, remembered per cluster. Orphan 0/0 ReplicaSets still show.
  A text filter also matches hidden old revisions and shows them dimmed. This delivers part of UP-64.
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

## Consequences

- This amends [ADR-0003](./0003-generic-resource-views.md): it is Flint's first change to the columns of a built-in
  kind's Table. Server Tables stay the source for every other column, and Wide shows them all.
- The Ready column needs status fields the Table does not carry (updated replicas for StatefulSets, the Progressing
  condition). Lists ask for `includeObject=Metadata` today, which carries no status, so these kinds are asked for
  with `includeObject=Object`, and only the typed fields are kept per row, not the whole object. That costs bandwidth
  and some memory, which the memory budgets must include. The controller owner uid is in the metadata already; the
  resource diagram needs it too, so whichever lands first keeps it.
- "Current" is the number of pods that exist (`status.replicas`; for DaemonSets, the current number scheduled), not
  a StatefulSet's `currentReplicas`, which counts pods at the old revision.
- A StatefulSet with a rolling-update partition, or a StatefulSet or DaemonSet with the `OnDelete` strategy, keeps
  fewer pods updated than desired on purpose. Its cell shows no spinner and says "partitioned" or "on delete" on
  hover.
- Of the three kinds, only Deployments report a Progressing condition, so only they can show a stuck rollout.
  StatefulSets and DaemonSets show the rolling-out marker without a deadline.
- `state.toml` gains the old-revisions toggle per cluster.
- A cluster set's combined list computes Ready per row from that row's cluster, so the column means the same
  everywhere.
