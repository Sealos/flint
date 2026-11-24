# Keep recently used clusters warm

**Not scheduled before 1.0** (#247, after 1.0). Cluster sets ([ADR-0014](./0014-cluster-sets-and-combined-views.md))
come first and work without warm clusters.

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
- **Memory budget.** Each store keeps an estimate of its size (rows, labels and cell text), calibrated against
  measured RSS by a multiplier. `warm_memory_mib` (default 128) covers only the background clusters, not the active
  one. When they go over it, the least recently used is made cold until the total fits. A very large cluster can
  therefore go cold as soon as the user leaves it.
- **Idle limit.** A warm cluster goes cold after 30 minutes without being selected, and when the Mac sleeps. When
  several warm clusters reconnect at once, after a network change for example, their reconnects are staggered.
- **Watch modes.** In mode All, a warm cluster keeps its watches but drops the high-churn kinds that no open view
  needs: Events (`v1` and `events.k8s.io`), Leases and EndpointSlices. They start again when the cluster becomes
  active. In mode On view, it keeps the watches of the views that were open when the user left it. Those views come
  back when the user returns. Kinds watched by no open view build no snapshots, so a warm cluster costs only the
  parsing of its watch events. Metrics are polled only for open views, so warm clusters are not polled.
- **No exec plugins in the background.** Flint never runs an exec plugin for a cluster that is not active. When a
  warm cluster's credential expires, or a plugin would need to run, the cluster goes cold with a "sign-in needed"
  badge. Any plugin Flint does run gets `KUBERNETES_EXEC_INFO` with `interactive: false` and a null stdin.
  Clicking the cluster makes it active and runs the plugin in the foreground, where its prompt is expected.
- **Background errors stay quiet.** If a warm cluster's watches fail, or stay disconnected for longer than the
  reconnect timeout of 2 minutes, it goes cold with an error badge. Errors of background clusters appear only in the
  sidebar, through `safe_error_summary`, never as toasts.
- **Nothing on launch.** The warm set starts empty. A cluster connects only after the user selects it, as before.
  Kubeconfig changes that remove or re-point a warm context make it cold.
- **MCP is not affected.** `flint mcp` is a separate process with its own connections. Warm stores in the app do not
  widen what an agent can read.

## Amendment: keep watching a cluster (v0.6, #101)

A cluster can be marked **keep watching**, from its sidebar menu or its overview. This is separate from the warm set
above and replaces two of its rules for that cluster only: **No exec plugins in the background** and **Nothing on
launch**.

- **What it does.** A watched cluster runs one telemetry feed while it is not selected, so its overview data and
  alerts stay current. Watched clusters start at launch; all others still load when opened. The toggle overrides
  On view for that cluster. Alerts go through the existing alert center.
- **Limit.** At most 32 clusters, the capacity of `TrendStore`. The 33rd is refused with a message.
- **Read only.** The feed has its own connection with a read client and no write client. The slot's connection,
  write credential and write gate are unchanged, so a background cluster gains no write path. `flint mcp` does not
  read this setting.
- **Switching.** Selecting a watched cluster hands its running feed over: the overview reads it, and the slot
  starts no feed of its own, so nothing is listed again. Leaving the cluster keeps the same feed running. Only a
  feed that is up, on the slot and whole definition it was started for, is handed over; a changed definition or
  ended consent drops it, and the slot's own connection starts a feed. Writes still go through the slot and its
  gate. A feed still connecting is dropped on selection, as before.
- **Warm lists.** While a watched cluster is selected and its feed's connection is up, its resource watches read
  through that connection's read client too. Leaving the cluster keeps up to 8 kinds running: those its views
  showed last, then Pods, Deployments, Services and Nodes, never Events, Leases or EndpointSlices. Selecting it
  again lists none of those anew, even when the slot signs in again. At most 3 clusters keep warm lists in the
  background, the most recently left. Whatever drops the feed's connection (a changed
  definition or credential, a pause, ended consent, a failure) drops the warm lists with it: the selected cluster
  lists again on the slot's own connection, and a background one stops its watches.
- **A handed-over feed that fails.** If the feed's credential fails while its cluster is selected, the feed ends
  without stopping keep watching: the slot's working connection starts a feed in its place, so alerts go on, and
  leaving the cluster starts a background feed again. A failure in the background still waits for a sign-in.
- **Members of a cluster set** ([ADR-0014](./0014-cluster-sets-and-combined-views.md)). The consent below also lets a
  set member that is not selected connect its own slot under the same rules: no prompt, a null stdin, bound to the
  whole definition, never saved, and no auth providers. That connection stays when the member is selected and when
  it is left, so a switch does not sign it out. Its write client presents the token the plugin got in the
  background, behind the unchanged write gate: the member stays read-only unless it is unlocked for the session or
  trusted ([ADR-0015](./0015-clusters-are-protected-by-default.md)). Ending the consent ends that connection too.
- **Consent for exec plugins.** A cluster whose credential is an exec plugin has a second, per-cluster toggle next to
  keep watching, which says it lets Flint run this cluster's sign-in command in the background and shows the command.
  It is off by default, and only a click on it turns it on. It is bound to the context's whole definition: the
  context, its user entry and its cluster entry. It is also bound to the program the command runs: the absolute path
  that resolving the command gives when the toggle is shown (through the `PATH` in the plugin's `env`, else the app's
  own), and a SHA-256 digest of that file. Before each unattended start the command is resolved and the file checked
  again; another path, changed contents or a missing file turns it off, and it must be confirmed again. The digest is
  read again only when the file's size, modification time or inode changes, and never on the app's event loop: the
  toggle looks at the file at most every two seconds and may show a change late, while a start always checks it in
  full. A command searched for in `PATH` is not bound when the plugin's `dropEnv` removes `PATH`. Any confirmed change turns it off, and
  undoing the change leaves it off. Turning it off stops the cluster's background watch at once. It lasts until the app quits and is never saved,
  so after a restart it reads off and runs nothing. Auth-provider clusters show it disabled. The keep-watching choice
  itself is saved.
- **How a plugin runs.** With `interactive: false` and a null stdin, never in a terminal. A provisional or paused
  context runs none, and neither does one whose files changed until they are read again. Turning the toggle off,
  removing the context or quitting the app kills a plugin that is running. Auth providers are not eligible, because
  their command cannot be stopped once it starts.
- **Failure.** The first failure of a plugin, or a 401, stops the feed with no retry and shows **sign-in needed**
  until the person selects the cluster or changes the toggle. A background request is sent once, without the
  client's retries. A credential that runs nothing retries a failed connection after a minute.
- **Last values.** A cluster that is unreachable or signed out keeps its last values in memory, marked with their
  age. Nothing is written to disk ([ADR-0017](./0017-cluster-dashboards.md)).
- **Sidebar.** Only "watching" or "sign-in needed". Health badges are deferred.

## Sidebar

Each cluster is **active**, **warm** or **cold**. A badge adds **reconnecting**, **sign-in needed** or **error**
when one applies. Colors come from theme tokens. Clicking a row with sign-in needed or an error makes it active and
retries auth in the foreground. A Disconnect action, in the row's context menu and the command palette, makes a warm
cluster cold.

## Memory and CPU

The M1 measurements ([m1-watch-measurements.md](../spikes/m1-watch-measurements.md)) put the core at 7 MiB RSS
before activation and 19 MiB with every kind synced in mode All. That cluster held 62 kinds and about 3,500 rows.
So a small warm cluster adds about 12 MiB, and the default of 3 costs about 24 MiB more than 0008 on small
clusters. The cost per row grows with labels and cell text, so large clusters cost more. The 128 MiB budget caps the
background clusters whatever their size.

The target for CPU is under 1% of one core with 3 idle warm clusters. Before warm clusters roll out, the estimate is checked
against RSS with 1, 3 and 5 warm clusters at 10,000 and 50,000 pods (simulated with kwok), and the multiplier is
set from those numbers.

## Considered Options

- **Keep 0008 as it is:** lowest memory, but every switch waits for a fresh list of every kind.
- **Keep every cluster that was ever selected warm:** memory grows with the number of clusters, which 0008 was
  written to prevent.
- **Views of several clusters side by side:** a different product. Every view, the namespace filter and the
  resource switcher would need a cluster dimension. Not in v1.
- **Keep warm clusters across restarts:** fast first switch, but Flint would connect to clusters at launch without
  the user selecting them.
- **Refresh credentials in the background:** keeps more clusters warm, but an exec plugin can open a browser or a
  prompt for a cluster the user is not looking at.

## Consequences

- `WatchManager` keeps per-cluster state for several clusters at once. The ordered warm set lives in
  `ClusterManager`.
- Watch storms on a warm cluster cost CPU while the user works on another one. The UI is not affected, because
  no view subscribes to them.
- `state.toml` may remember the order clusters were used in, but not connect them.
- Whether `kube` lets Flint set `interactive: false` in `KUBERNETES_EXEC_INFO` must be checked during
  implementation. If it does not, Flint keeps the rule by never starting a plugin for a background cluster.
