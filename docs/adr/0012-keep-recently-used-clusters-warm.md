# Keep recently used clusters warm

This amends [ADR-0008](./0008-watch-everything-on-active-cluster.md). It replaces the rule that a cluster drops its
watches as soon as it stops being active. Everything else in 0008 stays: the watch modes, and one active cluster in
view at a time.

The last N clusters the user selected stay **warm**: connected, with their watches running in the background. N is
a typed setting, `warm_clusters`, from 1 to 10, default 3. It counts the active cluster, so 1 gives the old behavior.
Switching to a warm cluster is instant, because its stores are already in sync. Views still show one cluster at a
time.

- **Eviction by least recent use.** Selecting a cluster moves it to the front. When the set holds more than N
  clusters, the one used least recently becomes **cold**: its watches stop, and it keeps only its client and
  discovery cache, as in 0008. The active cluster is never evicted.
- **Memory budget.** Each store keeps an estimate of its size (rows, labels and cell text). When the warm clusters
  other than the active one go over `warm_memory_mib` (default 128), the least recently used is made cold until the
  total fits. A very large cluster can therefore go cold as soon as the user leaves it.
- **Watch modes.** In mode All, a warm cluster keeps every watch. In mode On view, it keeps the watches of the views
  that were open when the user left it. Those views come back when the user returns. Kinds watched by no open view
  build no snapshots, so a warm cluster costs only the parsing of its watch events. Metrics are polled only for open
  views, so warm clusters are not polled.
- **No background reconnect storms.** If a warm cluster's watches fail with an auth error, or stay disconnected past
  the reconnect timeout, it goes cold with the error shown in the sidebar. Flint does not retry auth in the
  background, so an exec plugin never opens a login prompt for a cluster the user is not looking at.
- **Nothing on launch.** The warm set starts empty. A cluster connects only after the user selects it, as before.
  Kubeconfig changes that remove or re-point a warm context make it cold.
- **Sidebar.** Each row shows its state: active, warm, warm and reconnecting, cold, or error. Colors come from theme
  tokens. A Disconnect action makes a warm cluster cold.

## Memory

The M1 measurements ([m1-watch-measurements.md](../spikes/m1-watch-measurements.md)) put the core at 7 MiB RSS
before activation and 19 MiB with every kind synced in mode All. That cluster held 62 kinds and about 3,500 rows.
So a small warm cluster adds about 12 MiB, and the default of 3 costs about 24 MiB more than 0008 on small
clusters. The cost per row grows with labels and cell text, so large clusters cost more. The 128 MiB budget caps the
background clusters whatever their size.

## Considered Options

- **Keep 0008 as it is:** lowest memory, but every switch waits for a fresh list of every kind.
- **Keep every cluster that was ever selected warm:** memory grows with the number of clusters, which 0008 was
  written to prevent.
- **Views of several clusters side by side:** a different product. Every view, the namespace filter and the
  resource switcher would need a cluster dimension. Not in v1.
- **Keep warm clusters across restarts:** fast first switch, but Flint would connect to clusters at launch without
  the user selecting them.

## Consequences

- `WatchManager` keeps per-cluster state for several clusters at once. The ordered warm set lives in
  `ClusterManager`.
- Watch storms on a warm cluster cost CPU while the user works on another one. The UI is not affected, because
  no view subscribes to them.
- `state.toml` may remember the order clusters were used in, but not connect them.
