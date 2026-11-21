# Flint architecture

Flint is a native desktop app for managing Kubernetes clusters, in the space of Lens and Freelens. It exists because
Electron-based tools are slow to start, heavy on memory, and sluggish on large clusters. Flint's main goal is **speed
and low memory**. Native look and feel comes second.

This document describes the shape of the system. The reasons behind the hard-to-reverse choices are recorded as ADRs
in [`docs/adr/`](./adr/). The journeys every release must keep working, with stable IDs for tests, are in
[`docs/USER-PATHS.md`](./USER-PATHS.md). How they are tested is in [`docs/TESTING.md`](./TESTING.md).

## Scope

### v1 goals

- macOS first ([ADR-0001](./adr/0001-rust-and-gpui.md)), plus a best-effort Linux AppImage for x86_64 and aarch64
  ([ADR-0016](./adr/0016-linux-appimage.md)). The core stays portable so Windows can follow.
- Multiple clusters, loaded from the user's kubeconfig files, shown in one window with a cluster sidebar. Cluster
  sets combine 2 to 5 of them in one view ([ADR-0014](./adr/0014-cluster-sets-and-combined-views.md)). Keeping
  recently used clusters warm for instant switching is planned after 1.0
  ([ADR-0012](./adr/0012-keep-recently-used-clusters-warm.md), #247).
- Browse every resource kind, including CRDs, with live updates ([ADR-0003](./adr/0003-generic-resource-views.md)).
- Pod logs: follow, search, pick a container, show previous logs, toggle timestamps, and highlight errors and
  warnings.
- AI log analysis (not scheduled): send selected log lines, redacted, to the user's own Claude Code CLI (`claude`) and read the
  answer in a side panel ([ADR-0011](./adr/0011-ai-log-analysis-through-local-cli.md)).
- Pod exec terminal.
- Edit YAML in the app. Syntax and schema errors show inline as you type, and a server-side dry-run with a diff runs
  before apply ([ADR-0006](./adr/0006-yaml-validation-by-dry-run.md)).
- Common actions: delete, scale, rollout restart, cordon and drain.
- Current CPU and memory from metrics-server ([ADR-0005](./adr/0005-metrics-server-only.md)).
- Port-forwarding to pods and services, for the current session only, with open in browser for HTTP ports.
- A resource diagram: the ownership tree of one namespace's workloads.
- Helm releases: list, history, values, upgrade, rollback, uninstall ([ADR-0004](./adr/0004-helm-via-cli.md)).
- A read-only MCP server (`flint mcp`) so AI agents can inspect clusters the user has opted in
  ([ADR-0009](./adr/0009-read-only-mcp-server.md)).
- A dense, keyboard-first look in the style of Zed, with orange accents, light and dark themes, a command palette and
  a resource switcher.
- Open source under Apache-2.0.

### v1 non-goals

- Windows builds, and Linux packages other than the AppImage (deb, rpm, Flatpak, Snap).
- Prometheus charts or stored metric history. Dashboards keep an hour of their own samples in memory
  ([ADR-0017](./adr/0017-cluster-dashboards.md)).
- Local terminal tabs and node shells (shells into a node through a privileged pod).
- An extension or plugin API ([ADR-0007](./adr/0007-no-extension-api-in-v1.md)).
- YAML schema autocomplete.
- Saved port-forwards that come back after a restart.
- An in-app auto-updater.
- MCP write tools, an MCP server over HTTP, and a built-in AI chat panel. The log analysis panel shows one answer
  per request and takes no follow-up prompts.
- LLM API keys or provider clients in Flint. AI features go through the user's own tools.
- Fleets of dozens of clusters. Combined views cover sets of 2 to 5 clusters
  ([ADR-0014](./adr/0014-cluster-sets-and-combined-views.md)).
- Network edges in the resource diagram (Service to Pod, Ingress to Service).

## Stack

| Concern | Choice |
|---|---|
| Language | Rust |
| UI toolkit | [GPUI](https://www.gpui.rs/) with `gpui-component` (virtualized tables, docks, code editor), pinned to a specific git version |
| Kubernetes client | `kube` (`client`, `runtime`, `ws` features) and `k8s-openapi` |
| Async runtime (core) | `tokio`, on its own threads ([ADR-0002](./adr/0002-tokio-core-gpui-ui-bridge.md)) |
| Terminal emulation | `alacritty_terminal` |
| YAML and diff | `serde-saphyr`, `tree-sitter-yaml` (syntax errors, path-to-position mapping), `similar` |
| Schema validation | The cluster's OpenAPI v3 schema, cached per cluster, checked by a JSON Schema validator in Rust |
| Settings | TOML in `~/Library/Application Support/Flint/` on macOS, `$XDG_CONFIG_HOME/flint/` (default `~/.config/flint/`) on Linux |
| Helm | The user's `helm` CLI, called as a subprocess |
| Packaging | macOS: `cargo-packager` (or `cargo-bundle`), universal binary via `lipo`, `codesign` with hardened runtime for both app and DMG, `notarytool` and `stapler`, GitHub Actions on macOS. Linux: an AppImage per architecture built by `scripts/package-appimage.sh` (linuxdeploy and appimagetool), GitHub Actions on Ubuntu ([ADR-0016](./adr/0016-linux-appimage.md)) |
| Distribution | Signed, notarized DMG on GitHub Releases, plus a Homebrew cask; unsigned Linux AppImages with SHA-256 files on the same releases |

## Workspace layout

```
crates/
  flint-core/   Kubernetes logic: kubeconfig, connections, discovery, watches, stores, actions,
                logs, exec, port-forward, metrics, Helm. Depends on tokio and kube. Never on GPUI.
  flint-mcp/    The MCP server (stdio): tool definitions, cluster opt-in checks, output limits. Depends on flint-core.
                Redaction rules live in `flint_core::redact`, shared with the app.
                Its `flint-mcp` binary is the same server without GPUI: CI's MCP e2e tests run it, and releases
                ship it for Linux as static musl binaries (ADR-0016).
  flint-app/    The `flint` binary: windows, views, the terminal widget, settings UI, the bridge to core.
                `flint mcp` runs flint-mcp instead of starting the UI.
  flint-e2e/    Tests only (not published): the fake API server replaying fixtures recorded from kind, and
                the e2e harness. See TESTING.md.
```

Keeping `flint-core` free of GPUI has three benefits. The core can be tested without a window. It can be reused by a
CLI or a different UI later. And if GPUI ever has to be replaced, the work is limited to `flint-app`.

## Runtime model

```
┌──────────────────────── flint-app (GPUI main thread) ────────────────────────┐
│  Views (entities) ──read──▶ Snapshots          Commands ──▶ CoreHandle       │
│        ▲                                                       │             │
│        └──── coalesced change events (≤ ~10/s per view) ◀──────┼──────┐      │
└────────────────────────────────────────────────────────────────┼──────┼──────┘
                                                                 ▼      │
┌──────────────────────── flint-core (tokio runtime) ────────────────────────┐
│  ClusterManager ─▶ Cluster (one per kubeconfig context)                     │
│                      ├─ kube::Client + discovery cache                      │
│                      ├─ WatchManager ─▶ reflector stores (one per kind)     │
│                      ├─ Sessions: log streams, exec, port-forwards          │
│                      └─ Metrics poller (metrics.k8s.io)                     │
│  HelmRunner (spawns `helm` subprocesses)                                    │
│  LogAnalysisRunner (spawns `claude`, on a user action only)                 │
└─────────────────────────────────────────────────────────────────────────────┘
```

- `flint-core` starts a multi-threaded tokio runtime on background threads when the app launches. GPUI keeps its own
  executor on the main thread ([ADR-0002](./adr/0002-tokio-core-gpui-ui-bridge.md)).
- The UI talks to the core only through `CoreHandle`. Commands go in over a channel that works with any runtime.
  Events come back through a queue (`flint_core::events`, #255) that a GPUI task drains to update entities. It is
  bounded however slowly it is read: state events (the cluster list, the active cluster, settings, a cluster's
  discovery, the activity log) keep only their latest form per kind and cluster, a connection its latest attempt
  starting and where it settled, and errors the last 64 (older ones are dropped, counted and logged). The order of
  what is left is the order of the last changes, so reading it ends where reading every event would. A reader
  that skips lists cannot tell from them that a context started over (re-pointed, even and back; removed and added
  back), so the core numbers its slots: each `ClusterSummary` and each `DiscoveryChanged` carries the slot's
  generation, and a new slot gets a newer one. The app keeps (and uses) only discovery of a cluster's current
  generation, holding back one that came ahead of its list; the workspace closes its views when the active cluster's
  generation moves, as on a switch, and an open resource switcher lists anew. The dock still compares servers: a
  context that started over at the same server (a namespace edit, a rotated token) keeps its shells, and one
  re-pointed and put back between reads is ended by the core's `ContextHistory` pin recheck. A list also takes the queued connection and discovery events of contexts it no longer has, so contexts that
  come and go under new names do not pile up. The reader owns
  the queue: `CoreHandle::events` hands out readers, and once every reader is dropped the core stops queueing.
- Watches can change faster than the UI should redraw. The core marks the store as changed and sends at most one
  change event per view per frame budget (about 100 ms). The view then reads the latest snapshot. Individual watch
  events are never forwarded to the UI one by one.
- Snapshots are shared, read-only data (`Arc`). Views never hold locks on core state.
- A watch nobody subscribes to builds no snapshots. `CoreHandle::stored_snapshots` reads the rows every watch of a
  cluster holds without subscribing: a kind someone subscribes to gives what it last published, and any other is
  asked for one snapshot (`Hub::peek`, a one-shot reply, answered within 300 ms or left out), which its watch task
  builds from its store and keeps nowhere; a namespace-limited list asks each of its namespaces' watches the same
  way and merges their answers, without touching its own state. A kind whose watch does not answer in time (busy
  relisting) shows its kind row as "not loaded" rather than older rows. A read is not a subscriber: the hub still holds no rows afterwards, the
  task publishes nothing, the kind keeps its place behind the per-cluster list limit, and the hub can be released
  meanwhile. It never tells the manager, creates no hub and makes no request, so no watch starts in either mode.
  The resource switcher uses it.

## Clusters

- A **cluster** in Flint is one kubeconfig context. Kubeconfig files are read from `KUBECONFIG`, `~/.kube/config`,
  and any extra paths the user adds in settings. Flint watches those files and reloads when they change.
- Connecting runs the context's auth: client certificates, tokens, and exec plugins (`aws`,
  `gke-gcloud-auth-plugin`, `kubelogin`, …). `kube` handles all of these.
- **PATH:** apps started from Finder (or from some Linux desktop launchers) do not get the user's shell `PATH`, so
  exec plugins and `helm` would not be found. When Flint starts, it runs the user's shell once as an interactive login shell (`$SHELL -ilc env`), so that
  both `.zprofile` and `.zshrc` are read. The call has a timeout. If it fails, Flint falls back to the current `PATH`
  plus `/opt/homebrew/bin:/usr/local/bin`. A `path` value in `settings.toml` overrides all of this. Flint sets the
  result as its own process `PATH` before starting the tokio runtime or any threads, because `kube` starts exec
  plugins with Flint's own environment. VS Code does the same.
- **Dead connections:** API server connections use TCP keepalive as client-go does (first probe after 30 s idle,
  then every 30 s, 9 probes), so a connection that died silently (the Mac slept, the network or VPN changed) fails
  and its log stream, watch or shell reconnects or ends instead of waiting forever. There is no read timeout: it would
  end healthy streams that are only quiet. kube has no hook for this, so `cluster::build_client` builds kube's client
  stack itself with a keepalive connector.
- **Detail reads** (the object and the events about it, for the detail pane) have a deadline of their own
  (`read::DETAIL_TIMEOUT`, 30 s): events that do not arrive in time leave the object shown with why they are missing.
  The read stops when the pane that asked for it closes or moves to another row (its reply is dropped), so stepping
  through rows on a stalled server does not leave reads, their connections and the objects they hold behind (#258).
- One window shows a cluster sidebar. Exactly one cluster is **active**, meaning the one in view.
- Planned after 1.0 (#247). The last N clusters the user selected (`warm_clusters`, default 3, counting the active one) stay **warm**: connected,
  with their watches running in the background, so switching to them is instant. The least recently used is made
  **cold** when the set is full, when the stores of the background clusters (not the active one) go over
  `warm_memory_mib` (default 128), after 30 minutes unused, or when the Mac sleeps. A cold cluster keeps its client
  and discovery cache but drops its watches ([ADR-0008](./adr/0008-watch-everything-on-active-cluster.md), amended
  by [ADR-0012](./adr/0012-keep-recently-used-clusters-warm.md)).
- The warm set starts empty on launch. Flint never runs an exec plugin for a cluster that is not active. A warm
  cluster whose credential expires goes cold with a "sign-in needed" badge. One whose watches fail, or stay
  disconnected for over 2 minutes, goes cold with an error badge. Background errors show only in the sidebar, never
  as toasts.
- The sidebar shows each cluster as active, warm or cold, with a badge for reconnecting, sign-in needed or error.
  Clicking a row with a badge makes it active and retries auth in the foreground. Disconnect makes a warm cluster
  cold.
- **Cluster sets (super mode):** Cmd-click clusters, or open a saved named set, to combine 2 to 5 clusters in one
  view ([ADR-0014](./adr/0014-cluster-sets-and-combined-views.md)). Lists interleave the members' rows with a
  sortable, filterable Cluster column, or group them by cluster with a toggle. Columns are the union by name (by column key once Flint computes columns of its own, #196); the
  resource tree lists every kind any member serves, with a `2/3` badge when only some do. Every member of an open set
  is connected and watched like the active cluster. Rows keep their cluster: details and actions act on the row's
  own cluster. Each cluster may have an optional colour tag, shown in the sidebar, the Cluster column and the detail
  header.
- **Merged rows (planned, deferred until cluster sets are scheduled, #199):** the Namespaces list will be the one
  combined list that merges rows. Namespaces with the same name become one row whose columns are summed across
  members, with a member count, the worst status and the oldest age; usage from members without metrics is marked
  partial. A chevron shows one child row per member, and clicking the row opens a set-scoped namespace overview with
  a card per member ([ADR-0020](./adr/0020-namespace-columns-and-overview.md)). The merged row is a view over the
  members' per-namespace aggregates, keyed by the namespace name; every other list keeps one row per object.
- **Protection (#81):** every cluster is protected (read-only in Flint) by default
  ([ADR-0015](./adr/0015-clusters-are-protected-by-default.md)), except:
  - known local clusters: a context named exactly `kind`, `minikube`, `docker-desktop` or `orbstack`, or
    `kind-<name>` (`protection::is_known_local_context`, case-sensitive), whose API server is at `localhost` (or a
    name under `.localhost`), a loopback address (`127.0.0.0/8`, `::1`) or Docker Desktop's
    `kubernetes.docker.internal`, as the kubeconfig says it, never resolved (`protection::is_local_server`), and
    only when the cluster entry has no `proxy-url`. Any other context on those addresses takes Unlock or Trust like
    any other cluster. So does a local cluster served on another address, such as minikube's Docker driver on Linux
    (`192.168.49.2`, a bridge address). `protect_local_clusters = true` in `settings.toml` protects the known local
    clusters too;
  - clusters in `trusted_clusters` in `settings.toml`, pinned to the context and API server URL like AI exposure
    (`settings::ClusterPin`), so a re-pointed context is protected again;
  - clusters unlocked for this session (`protection::SessionUnlocks`): one confirmation naming the context and its
    server, pinned to that server, until Flint quits or Lock Again. Nothing is written to disk.
- **A `settings.toml` that does not parse** (or a symlink to one that is gone, which is not a fresh install, and
  whose target is watched so restoring it reloads) opens nothing up (`Settings::unreadable_fallback`), at launch and on a
  reload alike: nothing is exposed or trusted, and known local clusters are protected too, since the file may
  have asked for that. The sidebar's banner says so. Unknown keys are ignored so newer files load; one within two
  edits of a known key (`protect_local_cluster`) is named in the log, since it would otherwise fail open silently.
- **One gate in the core.** Every action that changes a cluster or runs code in it gets its client from the cluster
  manager through `Request::WriteClient`, answered by `ClusterManager::write_client`, which refuses a protected
  cluster with `WriteRefusal::Protected` before anything is sent. Exec is the only such action today
  (`CoreHandle::exec`, failing with `ExecFailureKind::Protected`); edit, apply, delete, scale, restart, cordon, drain,
  Helm changes, port-forward and debug containers take the same gate when they are built (M3, M5). The session's
  unlocks are shared between the front end and the core (`CoreHandle::unlock`, `lock`, `unlocks`), so an unlock
  takes effect before the next action is asked for. Stop Trusting (`CoreHandle::untrust`) revokes the trust in that
  shared state at once, before `Command::SetTrusted` writes the file, since commands and the gate's requests take
  separate queues. Every Trust and Stop Trusting request is numbered when it is made, in that shared state: a Trust's
  write lifts a revocation only if its pin was saved and it is still the newest request, and a revocation ends
  otherwise only when the core's own write of that Stop Trusting removed the pin and no older Trust can still write
  one (a reload that finds no pin, such as a file caught half-written, proves nothing). Lock Again also counts as a
  newer request, and revokes a Trust still being written. A failed write leaves the cluster protected, and the lock
  menu offers the removal again while a pin is on disk. Trust goes through
  `Command::SetTrusted`, which, like exposure, writes nothing if the context moved since the user confirmed. `flint mcp` stays read-only either way.
- **In the app.** The sidebar shows a lock button per cluster (`lock-<context>`): a closed lock while protected, an
  open one (in the warning tone) while unlocked, a shield while trusted, nothing for a known local cluster. Its
  menu offers Unlock for This Session…, Lock Again, Trust This Cluster… and Stop Trusting (also Remove the Old Trust
  for a trust left at another server), each a registered action (`workspace::UnlockCluster`, `LockCluster`,
  `TrustCluster`, `UntrustCluster`) the palette lists too. On a protected cluster the detail pane's Shell is
  disabled with the reason ("alpha is protected (read-only in Flint) until you unlock it") in its tooltip and
  caption and an Unlock… button beside it; the row menu's Shell is disabled the same way, followed by Unlock alpha….
  An ended or failed shell tab's Restart shell is disabled the same way, with the refusal under it and Unlock…
  beside it (no Unlock… when the tab's context left kubeconfig or lost its server: that is the reason shown then),
  and Restart Shell is unavailable from the keys and the palette (the workspace refuses it too, and the
  core is the backstop). The unlock and trust confirmations take the button: Enter cancels, as closing and quitting
  do. Lock Again leaves running shells running; it stops new ones, and its menu entry, palette entry and dialog say
  so. Stop Trusting also locks the cluster, even if it was unlocked for the session. A trusted context whose
  cluster entry is gone (no server) cannot be unlocked or trusted, but its lock menu still removes the old trust.
- Sessions the user started (log streams, exec terminals, port-forwards) keep running when the user switches to
  another cluster, warm or cold. They end only when the user closes them or quits Flint.

## Resources

### Discovery and lists

- When a cluster connects, Flint runs API discovery to find every group, version, and kind, CRDs included. The left
  navigation is built from those results: built-in kinds in fixed sections, CRDs grouped by API group.
- Lists use the server's **Table** format (`Accept: application/json;as=Table;v=v1;g=meta.k8s.io`), the same columns
  `kubectl get` shows. This gives correct columns for every kind, including CRDs with `additionalPrinterColumns`,
  without writing per-kind code ([ADR-0003](./adr/0003-generic-resource-views.md)).
- `kube` has no typed support for Table responses, so the list code makes raw requests through `kube::Client`. The M1
  spike must confirm how Table rows and the objects behind them (`includeObject`) work together with the watch
  stream. If an API (some aggregated APIs, for example) cannot return Tables, the list falls back to name, namespace,
  and age columns.
- **Namespace-limited access.** Many users may list a kind only in some namespaces (Roles, not
  ClusterRoles). When the all-namespaces list of a namespaced kind is refused (403, after a few quick
  retries, since a restarting API server can refuse briefly), Flint lists and watches it namespace by
  namespace and merges the results into one list, which says it is limited and to which namespaces.
  The namespaces are the user's namespace filter, else the context's default namespace, else every
  namespace the user can list. This is Lens's "accessible namespaces", without a setting: the
  namespace filter (which accepts typed names when namespaces cannot be listed) is the setting.
- The table widget is virtualized. Only visible rows are laid out, so a list of 10,000 pods costs about the same to
  draw as a list of 50.
- **Kinds in use (planned, #194):** in watch mode All, the tree shows only the kinds with at least one object anywhere
  in the cluster, and folds the rest into a dimmed "… N more" row per section; CRD groups with no objects fold into
  one "… N empty groups" row ([ADR-0019](./adr/0019-sidebar-shows-kinds-in-use.md)). Each navigation entry gets a
  state taken from its watch cache at no extra cost: unknown until its first list completes (and shown), in use once
  its store has an object, and not in use when the first list finds nothing, the list is forbidden, or the kind has
  no watch. A kind becomes in use as soon as its store gains an object, and leaves that state only when the cluster
  connects again or becomes active again; a watch that restarts keeps the last state. Forbidden kinds keep today's
  "cannot be listed" text and red dot. `Navigation` still holds every listable kind, and the tree folds by the state;
  the open kind and Overview are never folded. A collapsed section's count is its kinds in use. Kinds listed
  namespace by namespace only know about the namespaces they list. In a cluster set, the state is the union of the
  members', and the tree folds only when every member is in mode All. In mode On view the tree is not folded and
  nothing is probed. The resource switcher finds every kind.
- **Flint-computed columns (planned, #195, #196):** a few columns are computed by Flint and layered on the server's
  Table. They are synthetic columns, built in `table.rs` beside the synthetic Namespace column (`Column::synthetic`,
  with their own `ColumnRole`). Each has an identity of its own, apart from its header text (`Column::key`: `flint:`
  and the role for Flint's columns, `server:` and the header for the server's), so a Flint column and a server column
  with the same name (Flint's Ready and the server's Ready with Wide on) stay distinct for sorting and the
  remembered sort, which match columns by key (`list_model::resolve_sort`). A cluster set's union of columns
  should match by key too.
  - **Row data:** Deployments, StatefulSets and DaemonSets are asked for with `includeObject=Object`, and only a few
    typed fields are kept per row (replica counts, the Progressing condition, `spec.paused`, the update strategy).
    ReplicaSets and ReplicationControllers need only their Table cells and metadata. Pods stay on
    `includeObject=Metadata`; their requests are read only while the Namespaces list or a namespace overview is open
    (see [Namespaces](#namespaces)).
  - **Wide:** replica workloads get one **Ready** column ("ready/desired", a status dot, a static rollout marker) in
    place of the server's replica count columns (Desired, Current and Ready, or each kind's equivalents)
    ([ADR-0021](./adr/0021-replica-ready-column.md)). The replaced server columns are kept in the layout with a
    priority above 0, so the **Wide** toggle (which shows `priority > 0` columns) puts them back beside Ready.
  - **MCP** reads lists through the same `table` code but keeps the server's columns and `includeObject=Metadata`, so
    `list_resources` does not change.
- **Old ReplicaSet revisions (planned, #197):** ReplicaSets with 0 desired and 0 observed replicas and a Deployment as
  controller owner are left out of the list by default, with a count and a Show toggle in the header, remembered per
  cluster in `state.toml`. The text filter still matches them and shows them dimmed.

### Curated views

About 10 core kinds get hand-built detail views: Pod, Deployment, StatefulSet, DaemonSet, Job, CronJob, Service,
Ingress, Node, and Namespace. Every other kind uses the generic detail view, which shows metadata, conditions, events,
and YAML. A curated view is an addition to the generic view, never a replacement: each kind can always show its YAML.

### Namespaces

The Namespaces list's Problems, Pods, CPU and Memory columns, and the namespace overview, are per-namespace
aggregates ([ADR-0020](./adr/0020-namespace-columns-and-overview.md), #195, #198). The core keeps them per cluster:

- **Problems and Pods** are updated from the Pods store (its Table cells) and the replica workloads' typed status as
  their snapshots change. The pod and workload problem rules live in one place, shared with the dashboard.
- **Usage** is summed by namespace from each metrics-server poll.
- **Requests** come from a second Pods watch with `includeObject=Object`, beside the Metadata one, that runs while
  the Namespaces list or a namespace overview is open. Each pod's effective request (as the scheduler computes it) is
  summed by namespace and the object is dropped. Starting it re-lists every pod in full: tens of MB on a cluster with
  10,000 pods, on each start. It therefore stays alive for 2 minutes after the last of the list and the overviews
  closes, so moving between the list, an overview and back, or a short detour, does not pay that again. After the
  grace period it stops and its sums are dropped.
- **Coverage:** with namespace-limited access, Pods and workloads are watched only in some namespaces. The aggregate
  records which namespaces its watches cover, and a namespace outside them shows "unavailable", not 0. Metrics that
  RBAC forbids and a missing metrics-server are told apart.
- **Sparklines:** a namespace's usage series is a bounded ring buffer like the dashboard's, started when its overview
  or the Namespaces list opens and dropped when neither is open.

While the list or an overview is open, it counts as an open view of those kinds and of pod metrics, in both watch
modes. The overview (planned, #198) is the dashboard's components with a namespace scope, plus ResourceQuota use,
LimitRange limits and a YAML tab. It opens in the main view in place of the list; until it lands, a row opens the
detail pane.

### Watch modes

Two modes, set globally with an optional per-cluster override ([ADR-0008](./adr/0008-watch-everything-on-active-cluster.md)):

- **All (default):** while a cluster is active, Flint watches every kind it discovered that can be listed and
  watched. Moving between lists is instant.
- **On view:** Flint watches a kind only while a list or detail view for it is open. Uses less memory on very large
  clusters, but a list may need a moment to load when you open it.

When the user switches to another cluster, the old one stays warm. In mode All it keeps its watches, except the
high-churn kinds no open view needs (Events, Leases, EndpointSlices), which start again when it becomes active. In
mode On view it keeps the watches of the views that were open when the user left. When a cluster goes cold, its
watches stop in both modes, and switching back starts them again with a fresh list.

### Resource diagram

A read-only view of one namespace's ownership tree: Deployment, ReplicaSet and Pods, StatefulSets and DaemonSets
and their Pods, and CronJob, Job and Pods on three levels. Each owner is drawn as a group around what it owns.
Status colors come from theme tokens. Clicking a node opens its detail view.

- **Size:** empty ReplicaSets and finished Jobs are collapsed by default. Each group can be collapsed, and a group of
  more than about 50 pods shows a count instead of every pod. Past a node limit, the rest is shown as one summary
  node.
- **Data:** the tree is built from the stores, with no API requests beyond the watches the diagram needs. In mode All
  those are already running. In On view mode, opening the diagram starts them, like any view. Kinds the user may not
  list are left out, and the diagram says so.
- **Owners:** each row keeps the uid of its controller owner reference, stored compactly (as a `u128` or interned).
  Rows did not keep owner references before. The comment in `table.rs` that says so changes with this.
- Network edges (Service to Pod, Ingress to Service) are not drawn in the first version.

## Features

### Logs

`Api::<Pod>::log_stream` with `LogParams` (`follow`, `tail_lines`, `since_seconds`/`since_time`, `previous`,
`container`). Lines go into a
ring buffer with two limits, both settings: a line count (`log_buffer_lines`, default 100k) and a memory budget
(`log_buffer_mib`, default 64 MiB). Whichever is hit first evicts the oldest lines, so neither long-running streams
nor very long lines (up to 64 KiB each) can grow memory without bound. Search runs over the buffer.

All sessions also share one budget (`log_total_mib`, default 512 MiB, never below `log_buffer_mib`;
`flint_core::logs::LogBudget`), since nothing limits how many log tabs are open. Sessions report the bytes their buffers hold when they publish; when the total is
over the budget, the tabs viewed least recently drop their oldest blocks first (each only what its sealed blocks can
give; the open tail block stays, until the stream has ended and it is sealed), then paused tabs, and never the tab shown (beyond its own `log_buffer_mib`). The dock tells the core which log tab is shown (`LogSession::set_shown`). A session detaches one block at a time under the budget's lock, after checking that its tab is still hidden and the plan still stands, and frees the lines outside it: no block goes once `set_shown` returned, and the UI thread waits for one constant-time detach at most.
Streams keep running; only history goes, and a trimmed tab says "Older lines dropped to save memory" so a short
history is not taken for a quiet pod (#254).

- **Sessions** (`flint_core::logs`): `CoreHandle::open_logs` returns a `LogSession`; dropping it stops the stream.
  Sessions belong to no cluster slot, so they keep running across cluster switches. `timestamps=true` is always
  requested, so showing timestamps is a view toggle, and a dropped stream resumes from the last timestamp (with
  backoff, skipping lines already shown). A followed stream that ends is reopened only while the container can log
  again (it restarts, or the pod still runs). When the pod's status says the container waits in `CrashLoopBackOff`,
  `LogState::Reconnecting` carries `back_off`, and the tab says it waits for the restart rather than counting
  reconnect attempts. The kubelet answers a log it cannot read with one line without a timestamp (`unable to
  retrieve container logs for …`, or `failed to try resolving symlinks in path …`): as the first line of a read it
  is never shown as a log line. A previous-instance session ends with `PREVIOUS_NOT_AVAILABLE`
  (`LogState::previous_not_available`), as it does when the API server says the previous terminated container was
  not found; a current-instance read treats it as an end of stream, with the kubelet's words as the error: it ends
  when not following, and otherwise the pod's status decides (a finished container whose log is gone ends, one that
  restarts is waited for).
- **Delivery** follows [ADR-0002](./adr/0002-tokio-core-gpui-ui-bridge.md): at most one `Arc<LogSnapshot>` per
  ~100 ms. The buffer is stored in shared blocks (at most 1024 lines, or 1/32 of the memory budget), so a snapshot
  of 100k lines costs a few hundred pointer copies. Every line has a `LineId` that never changes, so a view diffs
  two snapshots by id.
- **Memory:** the line limit is exact. The budget evicts whole blocks from the oldest end, so a session stays
  within the budget plus one block. Pausing freezes the lines shown and counts the lines held back while the buffer
  keeps filling. The frozen view is trimmed whenever the buffer drops a block, so a pause never keeps evicted blocks
  alive and counts against the same budget. The cost: after a long pause, the oldest frozen lines disappear just as
  they would while following.
- **Log tab** (`flint-app/src/ui/log_view.rs`): a virtualized list (GPUI `list` in tail-follow mode) spliced by line
  id from each snapshot, so following costs only the new lines. Timestamps, wrapping and a workload's pod prefix
  are view toggles, and a workload tab counts its streams in the toolbar. Copy and Save (after a confirmation, then
  the native save dialog) write lines as shown, not redacted. Its pure parts (snapshot diff, line text and
  highlights, state and error wording, toolbar fit, save text) are in `log_model.rs`.
  - **Toolbar:** one row at any width (`toolbar_fit`, from the tab's measured width): the status (state, pause and
    held-back count) has a slot that truncates with an ellipsis, with everything in its tooltip; the toggles, Pause,
    Jump and Save are one group that never wraps and is never cut (the rest of the toolbar gives way first), and
    below about 980 points the toggles and Save fold into a "⋯" menu, then the counts and the match count go. Away
    from the newest line of a running, followed stream it says "Not following" beside Jump. Pause and
    "Paused" go once the stream has ended (held-back lines then show).
  - **Notices:** one strip under the toolbar, the latest transient thing: a copy or save note (a muted one times
    out after 4 s; any goes when the selection changes), else the selection hint. An ended session's error has its
    own banner: plain words (`explain`: a 403 is "You don't have permission to read this pod's logs.", a transport
    failure "Can't reach the API server.", and an API server error proxying to an unreachable kubelet (an
    `InternalError`, or a `:10250` address) "Can't reach the node running this pod.") over the server's text,
    which wraps; the status says only a few words, or a short form of an error it has no words for. A
    previous-instance tab whose log could not be read says the container may have been removed. Reading the logs
    again is always Reconnect (`dock::ReconnectLogs`, ⌘R): a new tab beside the ended one, which keeps what it
    showed; never for a deleted pod (`DockItem::can_restart`).
  - **Lines:** a shown timestamp is `HH:MM:SS.mmm` in the system's time zone (UTC, said as such in the toggle's
    tooltip, when it cannot be read), muted and fixed-width. A dated separator row (inside the line's list item)
    starts the first timestamped line shown and each line that moves the day or UTC offset on from the latest of the
    recent timestamped lines (untimestamped lines are skipped; a workload pod's line slightly out of order never
    starts a day), so a tail spanning days, or a fall-back hour, is unambiguous; a workload line's prefix is the
    pod name after the prefix all its pods share (the container only when the pod has several), padded to the
    widest, in a colour per pod (`theme::log_source_colors`, by first sight). Prefix, timestamp and text are
    separate elements, so wrapped text hangs past both; hovering over the timestamp or prefix shows the full value.
    The current search match is drawn over a stronger tint, bold and underlined, on a tinted line.
  - **Keys:** Esc in the search field clears the search, then (empty) returns focus to the lines (`LeaveSearch`,
    bound in `LogView > Input`, which the field passes Esc to). Esc in the lines clears the selection and otherwise
    does nothing: it never falls through to the detail pane's Close.
- **Colors and control characters** are stripped from each line once, on arrival in the core
  (`logs::strip_controls`), before it is classified, searched or stored: ANSI escape sequences (SGR colors, cursor
  codes, OSC titles and links, cut-off sequences at the end of a line) and control characters other than tab.
  Only the text after a line's last carriage return is kept, as a terminal shows a redrawn progress bar. Colors
  are not interpreted, so lines show, copy and save as plain text, and `\x1b[33mWARN` still reads as a warning.
  Only a line with something to strip costs an extra allocation.
- **Severity** is classified once per line on arrival. **Search** (`LogSearch`) remembers how far it scanned, so
  following with a search open scans only new lines; there is no index. A search is plain text, matched in the
  rows drawn as they are drawn. **Filtering** (`LogFilter`, #264) works the same way: "Matching lines only" (with 0
  to 100 lines of context, as `grep -C`) and "Errors" (warnings and errors, by the severity rules below) keep the
  ids of the lines shown, scan only new lines when the tab is drawn, and run their first scan, and any catch-up of
  more than 20,000 lines, on the core's threads (`CoreHandle::filter_logs`), so a 100k-line buffer filters without
  holding up a frame. The list then has one item per row shown; copies and saves of a filtered tab write the rows
  it shows. **Selection** (`LineSelection`) is a range of line ids that shrinks when its start is evicted, and is
  what copy and AI log analysis read.
- **Highlighting:** the view colors each line by its severity with the theme's error and warning tokens. The rules
  live in `logs::classify`: case-insensitive whole words; `ERROR`, `FATAL`, `panic`, and any word ending in
  `Exception` or `Error` (such as `NullPointerException`) mark an error; `WARN` and `WARNING` mark a warning; an
  error wins over a warning.
- **Copy** puts the selected lines on the clipboard as shown (with timestamps only when they are shown) and without
  redaction. AI log analysis redacts what it sends (see below). In a workload session copy keeps the
  `[pod/container]` prefix unless the view hides it (`LineSelection::text_as`). "As shown" means the same parts:
  copies and saves write each in full, the server's RFC 3339 UTC timestamp and the whole `[pod/container]`, so a
  file stays unambiguous across days, time zones and pods, though the tab shows them short.
- **Workload logs** (UP-45): `open_logs` also takes a `WorkloadTarget`: a Deployment, StatefulSet, DaemonSet,
  ReplicaSet or Job (its `spec.selector`, read once), or a label selector. One session watches the matching pods
  (a pod watch, not polling) and runs one stream per pod container: every regular container and native sidecar, or
  the one named container. Streams start when a container has started, reopen at once when the watch sees it
  restart (a stream that had ended reads on from where it stopped), and stop when the pod is deleted or stops
  matching (after reading what the server still sends, for at most 10 s; a pod that matches again while its stream
  still reads keeps that stream). Pods are told apart by uid, so a pod recreated under the same
  name gets a new stream and the old one never reads it: every open checks the pod by that name is still the
  stream's uid. Restarted streams queue for a slot like new ones. To resume without repeats, a stream remembers
  fingerprints of its newest lines, not their text, so ended streams hold no lines outside the budget. Each stream is a state machine polled by the session's one
  task, with the same reconnect, backoff and dedup as a pod session.
  - Lines carry their source (`pod/container`, one shared `Arc` per stream). `display`, copy (`text_as`) and
    search (`SearchQuery::sources`) can include or leave out the `[pod/container]` prefix.
  - Lines interleave in **arrival order**; there is no global timestamp sort. A reorder window would delay every
    line and still misplace any line later than the window (a slow node, a replay after a reconnect), while line
    ids must never change once assigned. Initial tails therefore show as one block per stream.
  - **One budget** for the session: all streams fill the same buffer. Without `tail_lines`, the line limit is
    shared out between the containers found at start (those over the cap included); an explicit `tail_lines`
    applies per container (like `kubectl logs -l`). At most 32 streams read at once (`MAX_STREAMS`); further
    containers queue and start as others end, counted as `capped`.
  - A failing stream does not end the session. `LogSnapshot::pods` (`PodSetStatus`) lists each stream's state and
    counts live, connecting, failed, waiting and capped ones. The session's own state covers the pod watch: a
    broken watch reconnects and re-lists while streams keep running. An error retrying cannot fix (the workload is
    missing, access is denied) or a context that now points at another server ends the whole session. kube's
    watcher keeps its client across re-watches, so a following session also rechecks its context every 30 s.

### AI log analysis

Not scheduled: no release on the roadmap includes it. This is the design for when it is.

The log viewer has an **Analyze with Claude** button. A palette action with a shortcut does the same. `claude` is
found on the resolved `PATH` (relative and empty entries ignored) or set in `settings.toml`. If it is not found, the
button and the action are hidden. Codex is not supported in v1, because it has no way to turn its tools off. The full
design is in [ADR-0011](./adr/0011-ai-log-analysis-through-local-cli.md).

1. The user selects lines and clicks Analyze with Claude. The opt-in is checked against the log session's own
   cluster (context name plus server URL), not the active one.
2. Flint redacts the lines with the log redactor in `flint_core::redact`: credential shapes, vendor tokens, private
   keys, and the values of Secrets, env and ConfigMaps the pod references, read at that moment. It sends at most
   1,000 lines, 4 KiB per line and 256 KiB in total.
3. A sheet shows the exact prompt and lines and names the CLI and its path. The first time on a cluster it has a
   checkbox "Allow log analysis on `<context>` (`<server>`)", and on every cluster an "Always for this cluster"
   option. The opt-in stores the CLI and its binary path, and "Always" resets when either changes. The opt-in is
   revoked from the sidebar context menu or the palette.
4. Flint starts the CLI in its own process group, with a cleared environment plus an allowlist, `KUBECONFIG` set to
   `/dev/null`, and a private temporary directory. Claude Code runs with its tools, MCP servers, settings and hooks
   off, and Flint checks its `system/init` event before anything is sent. The prompt goes in on stdin. It says the
   lines are untrusted data and must not be followed.
5. The answer streams into a side panel as text, below what was sent. Stop kills the process group. Flint never acts
   on the answer.

### Bottom dock

`flint-app/src/ui/dock.rs` (pure parts in `dock_model.rs`): a resizable panel under the main area, with one tab per
session that outlives navigation (log tabs and shell tabs now; AI log analysis later). Tabs keep running across
cluster switches. Cmd-J shows and hides the dock; Cmd-W closes the active tab wherever focus is in the window, and
closes the window only when the dock shows no tab; Shift-Cmd-W (Close Window) always closes the window. Closing the
window or quitting while a shell runs or a log tab follows asks first, naming how many sessions end (`ui/closing.rs`,
#177). That confirmation sets `DialogText::default_cancel`: the dialog's own keys are off, and its description (which
takes focus) and its buttons sit in the `DefaultCancel` key context, where Enter and Escape are bound to Cancel; with
focus on the dialog's own node (a click on the title-bar strip), a keystroke interceptor (`confirm::init`) cancels. Enter and Escape cancel; confirming
needs the Quit or Close Window button (a click, or Space when it has focus). A new tab kind implements `DockItem` and
adds a `DockTab` and `TabKey` variant; the module documentation lists the steps.

- **Titles.** One pattern for every kind: `chatty · app`, `chatty · app (previous)`, `api (Deployment)`,
  `-l app=web`, `Shell: web-1 · app`, cut in the middle past 26 characters. Every tab shows its cluster in a chip
  before the title, always, so a shell on production never passes for one on a dev cluster after a switch; the tab's
  tooltip adds the API server. Showing it only when several clusters are in play was rejected: tabs would change
  shape when a second cluster connects, and one cluster is no guarantee the tab's cluster is the one shown.
- **Chips** (`dock_model::chip_labels`) name the cluster briefly: an EKS ARN's cluster name (no `cluster/`), a GKE
  context's cluster, else the context, cut in the middle past 16 characters. No two contexts in kubeconfig share a
  chip: where two would read alike, the chip adds what tells them apart (the differing part of the account or
  region for EKS, of the project or location for GKE, else of the context names), and anything still alike is
  numbered. Contexts with an open tab count too, so a tab whose context left kubeconfig keeps a distinct chip,
  marked, with a tooltip saying so. A shell's toolbar shows the same chip, with the full context and server in its tooltip; both mark a
  re-pointed context alike. With the longest chip and title, about two and a half tabs fit in 860 points.
- **Identity.** A tab is its `TabKey` (what it reads or runs) on a `TabCluster`: the context name and the API server
  URL it pointed at when the tab opened, as AI exposure (ADR-0009) and trusted clusters (ADR-0015) pin a cluster.
  Opening a request on the same cluster shows its tab while the tab answers for it (`dock_model::reopens`): while its
  session starts, runs or retries, and for previous logs read to their end. A tab that failed, or a stream or shell
  that ended, keeps its output, and a fresh tab opens beside it. When a context is re-pointed at another server, a
  shell on it ends and says why (the core's log and exec sessions also recheck their context every 30 s, and end on
  their own), and its tab stops answering for its request, so Logs or Shell opens on the new server.
- **Restarting.** Reconnect Logs and Restart Shell (`dock::ReconnectLogs`, Cmd-R in the dock;
  `dock::RestartShell`, Cmd-R, or Ctrl+Shift+R on Linux, where a shell tab hands Ctrl-R to the shell) start the
  active tab's ended or failed session again, on the server its context points at now, in a new tab right after it
  (`Dock::open_beside`), which is shown. The ended tab keeps its lines or scrollback until it is closed, so starting
  again never discards output. A deleted pod's log tab cannot be reconnected (`DockItem::can_restart`). The dock
  emits `DockEvent::Restart` and the workspace builds the session, refusing while the cluster is not connected with a
  refusal's sentence ("Can't restart the shell: alpha is not connected.", "Can't reconnect the logs: …";
  `detail::refused`). On a tab whose session runs, Cmd-R falls through to Refresh. The ended and failed banners of
  log and shell tabs carry Reconnect and Restart shell buttons.
- **Many tabs.** The strip scrolls the active tab into view whenever a tab opens, is shown or closes, and on the
  frame after the tab bar changes width (the window or a pane was resized) or a kubeconfig reload changes chips. Show Dock Tabs (`dock::ShowTabList`,
  Shift-Cmd-J, or the button at the strip's end) lists every tab with its cluster and status and shows the one
  picked (`dock::ActivateTab`). With no tab, the strip is hidden and the dock says how to open logs or a shell.

**From rows** (UP-47): Logs (`detail::OpenLogs`, ⌘L), Previous Logs (`detail::OpenPreviousLogs`, ⇧⌘L) and Shell
(`detail::OpenShell`, ⇧⌘E) act on the selected row, which the detail pane shows, so the keys, the palette, the pane's
buttons and a row's context menu (`ui/row_menu.rs`) are one set of actions. `row_menu::row_actions` says what each can
do now and why not, with the same checks as the guards; the menu, the pane's buttons and the automation `state` show
it (a disabled entry carries its reason). The pane says it without hovering too: a muted caption under its header
(`DetailView::action_captions`) gives the refusal's sentence for Logs and for Shell, once when they share it; Logs and
Shell stay, disabled, when the pod could not be fetched. The caption never repeats what the pane already says: a pod
that could not be fetched reads "Can't open the logs or a shell until the pod is fetched." (the body shows the
server's error), and a deleted object gets no caption (its notice says so). While the object's cluster is not
connected the workspace tells the pane (`DetailView::set_sessions_blocked`, from `Workspace::sessions_blocked`), and
both buttons are disabled with that reason in their tooltips and the caption. Kinds other than pods and workloads have
no Logs or Shell button. A right-click selects the row first. On a pod with several containers the menu's Logs,
Previous Logs (the containers that ran before) and Shell, and the pane's Logs and Shell buttons, are container pickers
(`detail::OpenContainerLogs`, `detail::OpenContainerShell`) that mark the default container, whose entry also carries
the plain action so the menu shows its key beside it; a container whose status says it is waiting or terminated
(crash-looping, say) is listed disabled in the Shell picker as "not running", and a native sidecar (an init container
with `restartPolicy: Always`) counts as an app container, for a pod's pickers and a workload's pods alike
(`WorkloadPod::from_pod`), and the default-container annotation may name one; when the default container is not
running, its key refuses with the same "not running" reason while the picker offers the others; the keys always open
the default container at once, never a picker. The default is the one `kubectl` picks (the
`kubectl.kubernetes.io/default-container` annotation, else the first app container), so ⌘L and ⇧⌘E do in one step what
`kubectl logs` and `kubectl exec` do without `-c`, the same way every time and from the palette too, where no menu is
open; the picker is one right-click or button away, and the buttons' tooltips say which container the key opens. For a
Deployment, StatefulSet, DaemonSet, ReplicaSet or Job, Logs opens the workload session (all pods, prefixed) and Shell
opens in its first ready pod by name.

- **Workload pods** (`ui/detail_pods.rs`, `CoreHandle::workload_pods`): never listed on each update of the workload.
  A cached list (`resourceVersion=0`) shows which pod a shell would open in: a moment after the row is selected (so
  key repeat through a list starts no request per row), when its menu opens if the list is missing, failed or
  older than the workload, and 2 seconds after the workload changes (once however often it changes; a list that ran
  across a change counts as older). A failed list is not retried on a timer, so a lasting error does not poll. A list
  that is stale or failed decides nothing: Shell stays enabled, as it is when nothing is listed yet. Shell itself
  always runs a quorum list and opens in a pod of that list; a list that fails then refuses Shell with its error,
  never falling back on an older list. One list runs at a time per pane (asking again joins it). The selector comes
  from the fetched workload when it has one. Dropping the pane drops the request, and dropping the core's future
  aborts its task.
- **Waiting.** Asked for before the pod (or the pods) has loaded, an action waits, shown in the pane, for 30 seconds
  at most, then gives up saying so; a Shell that gives up drops its pod list, so the next one starts its own. A tab
  opened late takes focus only if focus has not moved since the ask (one asked for at startup, `FLINT_VIEW=…#logs`,
  always does); if focus is in another dock tab, the new tab opens behind it (`Dock::add`). The open menu is rebuilt
  whenever what it shows changes, until it is dismissed.
- **Refusals.** An action refused, at once or after waiting, says why in a strip between the main area and the dock
  (`action-error`): a row of its own, so it covers neither the list nor the pane, one line high with a warning icon
  (the whole sentence in its tooltip). It is cleared by the next tab opened, a change of selection, list or cluster,
  or after 10 seconds. Every refusal is one sentence worded by `detail::refused`, naming what was refused, then why
  ("Can't open a shell: no pod of Deployment web is ready."). A menu entry refused as not available says why from
  the row's actions, never by the action's internal name. Shell is refused for a deleted pod or workload and for a pod
  that finished (`Succeeded`, `Failed`) or is being deleted. While the row's cluster is not connected every entry is
  disabled and the menu says why with core's summary of the error (`ClusterError::summary`); an action's
  own reason (a refused pod list, say) is cut to one short line.
- The workspace forwards these actions to the detail pane showing when they arrive (`registry::ForwardCurrent`), not
  the one last drawn, so a key right after moving to another row acts on that row.

`logs::OpenSelectorLogs` (palette: Open Logs by Label Selector…, which asks for the namespace and then the selector)
opens a label selector's pods on the active cluster; core refuses an empty selector.

### Exec terminal

`Api::<Pod>::exec` with a TTY, over the `ws` feature. Bytes go to `alacritty_terminal`, and a GPUI terminal view
draws the grid, as Zed does. Window size changes are sent to the pod as terminal resize messages. Each exec session
opens in a tab in the bottom dock.

- **Core side (`flint_core::exec`, `flint_core::terminal`).** `CoreHandle::exec` starts a session on its own task,
  apart from the cluster manager, so it survives cluster switches and ends only when closed or dropped. The
  `alacritty_terminal` grid lives in the core: output is parsed on the core runtime, terminal replies (cursor
  position reports) go straight back to the pod, and the UI reads immutable screen snapshots, at most one per frame
  (16 ms; the first after a quiet spell at once). Keystrokes, pastes (bracketed when asked for), resizes, scrolling and
  selections go back through the session. The screen stays scrollable and selectable after the command exits.
- **Pinned to its context.** A session is pinned to its context as it was when the cluster manager handed out the
  client, read in the same step: the context's generation in the manager's `recheck::ContextHistory`, and the
  server the client was built for. The generation changes with each confirmed change: a load gives the context
  another server, or lacks it. A change undone by a later reload still counts.
  - **What a load confirms.** The loader reads the kubeconfig files in order, and the first file that defines a name
    wins. A file that fails to load (caught half-written) is skipped, and what it defines is unknown. A context
    missing from a load is removed only if no failed file ranks at or above the file that defined it, and no
    failed file defined it too (a shadowed definition) when it last loaded. A failure in a file never known to
    define the context masks nothing. A context with another server is a confirmed change only if no failed file
    ranks above the file it now comes from, and above the file that won its cluster entry; otherwise it may be a
    later file's fallback, showing while the first file is unreadable, and it stays provisional. A running session
    survives a fallback of the cluster entry, but the write gate lets nothing new start on a pin from before it,
    even once the file is back.
  - **Provisional entries.** What a load cannot confirm never replaces confirmed history, its file included, even
    on the same server. A context first seen that way (a file before it already unreadable at startup) is recorded
    as unconfirmed; a load that confirms it on the same server keeps its sessions. Sessions on the confirmed
    definition go on while a fallback shows. A session opened on the fallback (a shell, or a log tab, labelled with
    the fallback's server) is pinned to that server, stale: its first check ends it with why, so it never moves to
    the confirmed server once the first file loads again. A log tab passes the server it is labelled with
    (`CoreHandle::open_logs_at`), so a request the core takes after the fallback went is still pinned to it. A log
    session is never handed a client for another server than its pin. A shell does the same (`ExecRequest::at`): it is
    refused before it starts if the context shows another server than its tab when the core takes the request. An unconfirmed entry follows the file it comes from on the same
    server, so a failed file between masks its removal.
  - **Checks.** While the session connects or runs, it asks the manager every 30 s (`recheck::RECHECK_EVERY`,
    shared with log sessions, which are pinned the same way) whether the context changed since, never the server.
    A change ends it with `ExecFailureKind::ContextChanged` and the reason (`recheck::reason`): removed, now on
    another server, or, when it points where it did (removed and added back, say), changed since the session
    opened. A shell refused before it starts says it did not start (`NotStartedMoved`, `NotStartedRemoved`). A pin
    refused while the context's recorded server is still its exact URL gets `WriteRefusal::Stale`, which does not
    claim the server moved (`NotStartedStale`, or `EndedStale` once its connection is open). A log
    session opened on a removed context ends at once. Ending drops the exec connection, as closing
    the session does, so no stream stays open on the old server.
- **Input budget (#256).** A session holds at most 1 MiB of input (`exec::INPUT_BUDGET`) that the pod has not read:
  what waits for the session task, what is kept for the next shell while the first may turn out not to exist, and
  what waits for the pod's stdin. `ExecSession::write` and `paste` reserve their bytes with an atomic update (never
  blocking the UI) and are refused whole, with `InputRefused`, when they do not fit; the reservation travels with
  the bytes and is given back once kube's stdin pipe has them, or when they are dropped on any path. The shell tab
  then says the shell is not reading its input, until input is taken again. The terminal's replies to the program
  may go 16 KiB past the budget. A copy kept for the next shell counts too; when it does not fit, nothing is kept,
  input sent for the first shell and still queued is dropped (each input carries the shell attempt it was sent
  during), and if the next shell starts, its snapshot says so (`input_not_replayed`). Resizes and closing never
  wait behind input: a resize replaces the size not yet taken in and has its own task to the pod's TTY, and
  closing is a signal of its own; the session then empties its input queue, giving the bytes back.
- **Shell choice.** Without an explicit command, `bash` is tried, then `/bin/sh`. A missing executable is reported on
  the exec status channel before the command prints anything (Docker prints it on the TTY and exits with 126 or
  127 instead), which tells it apart from a shell that ran and failed. Keystrokes typed meanwhile go to the shell that starts.
- **Shell tab** (`flint-app/src/ui/terminal_view.rs`): opened by Shell on a pod or workload, in its detail pane or
  its row's menu (`detail::OpenShell`, ⇧⌘E; a container picker when the pod has several app containers,
  `detail::OpenContainerShell`; see "From rows" above). While its command
  runs, Shell on the same container of the same cluster (context and server) shows the tab; once it ended, Shell
  opens a new one beside it, and Restart Shell starts a new shell in a tab beside it (see "Bottom dock"). Its
  toolbar shows the cluster's chip beside the state. The grid is painted
  from each snapshot with the theme's mono font: one quad per run of equal background, then each row's text shaped
  with every glyph forced to the cell width (wide characters and combining marks shaped on their own), so columns
  stay aligned. Colors come from the theme: the default foreground, background, cursor (the accent) and selection are
  theme tokens, the 16 ANSI colors are a `terminal` palette per theme in `themes/flint.json` (checked for contrast in
  both themes), 16 to 255 are xterm's table (`terminal_palette.rs`). Bold, italic, underline, strikethrough, dim,
  hidden and inverse are drawn; the cursor is a block, beam or underline (a hollow block, or a faded beam or underline,
  without focus; none once the session ended); the bell flashes the
  screen. The grid size comes from the element's size and the font's advance; a change is sent after 80 ms without
  further change (the first at once). The header says Connecting, Running (command), Exited (code) or Failed;
  why it failed is said once, in the banner under it: plain words, then "Details:" and what was said, in full and
  wrapping, so a long RBAC message stays readable in a narrow dock. Core types where a failure came from
  (`ExecFailureKind`), from its real cause, in `LaunchError::kube` before kube's error is collapsed: an auth error
  anywhere in kube's chain, a missing exec plugin, a bad kubeconfig or a 401 is signing in ("Flint couldn't sign in
  to the cluster."); a failed WebSocket upgrade (a proxy answering 200, missing upgrade headers) has words of its own;
  a refused or timed-out connection is reaching the cluster; only the API server's own `Status` is the server's, and
  only those are told apart by their words (no permission, pod or exec not found, container not running). A plain-text
  body is never shown, except the kubelet's refusal of a stopped container before the upgrade, `container not found
  ("<name>")`, matched exactly (a valid container name, at most 63 characters), which is the server's too. That phrase
  is read only when launching an exec, and only from a plain 500; any other request or status keeps it a bare HTTP
  code, whose diagnostic says more (a proxy's 502, `/version`). A bare HTTP
  code (a proxy's page, a wrong server path) and anything else read "The shell could not start." with the full text.
  After the end the screen stays scrollable and copyable. Closing the tab drops the session.
- **Keys in a shell tab.** The tab's key context (`Terminal`) binds Tab, Shift-Tab, Esc, Return, Backspace, Delete,
  Space, the arrows, Home/End, Page Up/Down and Ctrl with a letter or symbol to nothing
  (`registry::TERMINAL_PASSTHROUGH`); it is the deepest context, so those keys skip the app's bindings (focus
  traversal, list keys, and the app's Ctrl shortcuts on Linux) and reach the tab's key handler, which encodes them
  with the program's modes (`terminal_keys.rs`: application cursor keys, xterm modifier parameters, Ctrl codes,
  function keys; Alt as Meta except on macOS, where Option composes text). Text, including what an input method
  composes, arrives through the platform text input. What stays the app's: Cmd shortcuts on macOS; on Linux,
  Ctrl-Shift ones (copy is Ctrl-Shift-C, paste Ctrl-Shift-V) and Ctrl-Tab for the dock. Known conflicts: on Linux an app
  shortcut on Ctrl with a letter (without Shift) would not fire while a shell has focus (today's are Cmd/Super or
  Ctrl-Shift); Shift-Page Up/Down and Shift-Home/End
  scroll the tab instead of reaching the program; mouse reporting (programs that ask for clicks) is not sent, the
  mouse always selects.
- **Pasting.** A paste of more than one line into a program that did not ask for bracketed paste (BusyBox `sh`
  never does) would run each line as it arrives, so the tab asks first ("Paste 3 lines?"). Whether the program has
  bracketed paste on is the core's call when the paste arrives (`ExecSession::paste_if_bracketed`), not the tab's
  last snapshot: bash turns it off while it runs a command. A clipboard over 256 KB is refused with a note rather
  than queued: the session's input channel is unbounded (#152).
- **Mouse in a shell tab.** Dragging selects (double click: words, triple click: lines, Option: a block, Shift-click
  extends); the wheel scrolls the history, or sends arrow keys to a full-screen program on the alternate screen.
- **OSC 52** (clipboard access by the remote program) is disabled.
- **Synchronized updates** (DECSET 2026) that a program never ends are applied after alacritty's 150 ms timeout,
  and at the end of the output.

### Editing and actions

1. The editor opens the live object without `metadata.managedFields` and `status`. It keeps `resourceVersion`.
   While the user types, tree-sitter marks syntax errors and the object is checked against the cluster's OpenAPI v3
   schema (fetched from `/openapi/v3` and cached per cluster), so unknown fields, wrong types and missing required
   fields are underlined inline ([ADR-0006](./adr/0006-yaml-validation-by-dry-run.md)).
2. **Review** sends a replace (`PUT`) with `dryRun=All` and shows a diff between the live object and the result.
3. **Apply** sends the same `PUT` for real, with `fieldManager=flint`. If the object changed since it was opened, the
   `resourceVersion` makes the request fail with a conflict. The user is then offered a reload and a re-diff, so
   nobody else's change is overwritten without them seeing it. This is how `kubectl edit` behaves. Server-side apply
   is not used for editing, because applying a full object would claim ownership of every field and conflict with
   Helm and controllers.
4. A reload keeps the text being edited and moves it to the current `resourceVersion`, so the new diff shows any
   change the edit would undo. An edit that leaves `status` out keeps the live value, since a `PUT` to a kind without
   a status subresource would erase it.
5. A Secret is not editable yet: its values are masked in the YAML, and a mask must never be written back.

Delete, scale, rollout restart, cordon, and drain are typed actions in `flint-core`, each with a confirmation in the
UI. Every write refuses on a protected cluster ([ADR-0015](./adr/0015-clusters-are-protected-by-default.md)): it
asks for its client through the core's one gate (see "Protection" above), and the UI offers a session unlock in its
place. Destructive actions show the cluster and namespace in the confirmation. Kubernetes has no drain API, so drain
works like `kubectl drain`: cordon the node, then evict its pods through the Eviction API. Pods blocked by a
PodDisruptionBudget are reported back to the user instead of being retried forever.

### Dashboards

Each cluster opens on a dashboard, also reachable from **Overview** at the top of the resource tree
([ADR-0017](./adr/0017-cluster-dashboards.md)). Its fixed layout has three parts: **problems now** (failing pods,
not-ready nodes, Warning events in the last hour, pending PVCs, failed Jobs, workloads short of replicas), each count
opening the matching filtered list; **capacity and usage** (CPU and memory used against allocatable, requests and
limits against capacity, per node and in total, with a sparkline of the last hour); and **recent activity** (Warning
events and rollouts). The sparklines come from Flint's own 15-second metrics-server samples, kept in a bounded
in-memory buffer while the cluster is connected. Thresholds are typed settings with per-cluster overrides, edited from
the dashboard. The dashboard counts as an open view of the kinds it needs. A cluster set's dashboard shows one card per
member, worst first, under a totals strip. `get_cluster_summary` gives agents the same summary. The same components,
scoped to one namespace, make the namespace overview (see [Namespaces](#namespaces)).

### Metrics

Flint polls `metrics.k8s.io/v1beta1` `PodMetrics` and `NodeMetrics` about every 15 s, and only for views that are
open. If metrics-server is not installed, the metric columns are hidden and a hint is shown
([ADR-0005](./adr/0005-metrics-server-only.md)).

### Recordings (after 1.0)

A recording keeps capturing a workload's, pod's or label selector's logs, its Kubernetes events and its pods' CPU and
memory samples while Flint runs, with no tab open, across restarts and replacements
([ADR-0018](./adr/0018-recordings.md)). Recordings start by hand, or automatically on trouble on clusters where the
user enabled it (CrashLoopBackOff, OOMKilled, failed Jobs; up to 10 pods; stopping 30 minutes after recovery). They
are stored in `recordings.sqlite` in Flint's data directory (typed tables, owner-only, WAL), as written, with
configurable retention (7 days, 500 MB per recording and 2 GB in total by default; starred ones kept). Expired
credentials pause a recording with a "sign-in needed" badge. A recording opens as a timeline of logs, events and
metric tracks. Agents can query recordings read-only through MCP, redacted and scoped; a time range can be exported.

### Port-forward

`Api::<Pod>::portforward` behind a local `TcpListener`. When the target is a Service, Flint picks a ready pod behind
it, and picks another one if that pod goes away. The user picks the local port, or lets Flint choose a free one, and
can forward several ports of one target at once.

A panel lists the active forwards: target, local and remote port, and state, with stop buttons. Ports named `http`
or `web`, or with `appProtocol` `http`, get an **Open in browser** action for `http://localhost:<port>`. Ports named
`https`, or with `appProtocol` `https`, open `https://localhost:<port>`. Unnamed ports offer Open in browser from the
row's context menu. Forwards bind to `127.0.0.1` only and end when the app quits.

### Helm

`HelmRunner` calls the `helm` CLI with `--kube-context`. It sets `KUBECONFIG` on the subprocess to the same list of
files Flint loaded, because `--kubeconfig` takes only one file.

- JSON output (`-o json`) is used for `list -A`, `history`, `status`, and `get values`.
- `get manifest` returns plain YAML.
- `rollback` and `uninstall` return only an exit code and text.
- **Upgrade** in v1 means editing values and running `upgrade` again with the same chart. Helm cannot upgrade from
  release data alone, so the user picks the chart reference (a `repo/chart` known to their local Helm, or an OCI
  reference). Flint pre-fills it from the release's chart name and version, and passes the edited values with `-f`.

If `helm` is not on the resolved `PATH`, the Helm section shows an install hint ([ADR-0004](./adr/0004-helm-via-cli.md)).

## MCP server

`flint mcp` runs a Model Context Protocol server over stdio for AI agents such as Claude Code and Claude Desktop
([ADR-0009](./adr/0009-read-only-mcp-server.md)). It does not start GPUI. It resolves `PATH`, loads kubeconfig and
settings the same way the app does, and serves these read-only tools:

| Tool | Returns |
|---|---|
| `list_clusters` | Clusters marked Expose to AI: name, server, whether a connection works, and the scope when one is set |
| `list_resource_kinds` | Kinds found by discovery for a cluster, CRDs included |
| `list_resources` | Objects of a kind as Table rows (the `kubectl get -o wide` columns, CRD printer columns included), with namespace and label selector filters |
| `get_resource` | One object as YAML |
| `get_events` | Events for a namespace or one object |
| `get_pod_logs` | The last lines of a container's logs |

Rules:

- **Opt-in:** a cluster is visible only if `ai_exposed_clusters` in `settings.toml` has an entry with its context
  name **and** the API server URL that context pointed to when the user exposed it. If the context now points to a
  different server, for example because `flint mcp` was started with a different `KUBECONFIG` or the context was
  re-pointed, it is treated as not exposed, and the app shows the toggle as off with a "cluster changed" note. The
  user toggles exposure from the cluster sidebar. The list is read again on every call, and calls naming any other
  cluster fail as if it did not exist.
- **Read-only:** there are no tools that create, change, or delete anything, and no exec or port-forward.
- **Names:** object, pod, namespace and container names are validated as single path segments before any request
  (DNS-1123 names, or for other objects letters, digits and `-._:@`; never `.`, `..`, `/`, `%`, `?` or `#`), so a
  name cannot reach another API path. The check runs before discovery, so a name that fails sends no request at
  all; it gets a not-found error (a bad container name, the server's "not valid for pod") shaped like the API
  server's, naming only what the caller sent. Discovery likewise leaves out any resource whose group, version or
  plural is not a valid single path segment. RBAC names that are not DNS-1123 and use other characters (spaces, `+`, `~`, non-ASCII)
  cannot be read this way.
- **Scope (#67):** an exposed cluster's entry may list the namespaces and kinds an agent may read
  ([ADR-0013](./adr/0013-agent-trust-model.md)): `namespaces` and `kinds` (kind ids, `pods`, `deployments.apps`) on
  its `ai_exposed_clusters` entry (`settings::AiExposure`, `settings::AiScope`). Missing or empty means everything,
  so entries written before scopes load unchanged. The file is refused (nothing is exposed) rather than read as
  everything when an entry has a key it does not know (`namespace` for `namespaces`; the error names it), when a list
  names only blank entries, or when two entries name the same context and server (the first would decide). A Flint
  older than this reads a scoped entry as the whole cluster: an agent configured with an old `flint mcp` binary is
  not limited. The policy (`flint-mcp`'s `policy.rs`) applies it to every tool, after the names are checked (above),
  so a name that is not valid gets the same not-found scoped or not:
  - `list_clusters` reports a cluster's scope, so the agent asks within it, and its default namespace only when
    the scope allows it;
  - `list_resource_kinds` lists only kinds in scope; with a namespace scope, cluster-scoped kinds (nodes, namespaces,
    CRDs) only when the kinds scope names them;
  - `list_resources` and `get_resource` resolve kinds among those in scope only, and check the namespace; a list
    across all namespaces on a namespace-scoped cluster lists each namespace of the scope in turn, with no continue
    token (the agent pages one namespace at a time; a server token could name objects outside the scope); when the
    namespaces' pages come with different columns, every row falls back to the plain layout (name, namespace, age);
  - `get_events` follows the namespace scope the same way, and drops events about objects of kinds out of scope
    (looked up in discovery by Kind and group; a kind discovery does not list is dropped on a scoped cluster); an
    `object_kind` out of scope fails before anything is fetched, and `truncated` counts only events in scope. On a
    scoped cluster, events are read only this way: the event kinds (`events`, `events.events.k8s.io`) are out of
    scope for `list_resources` and `get_resource`, even when named, since an event in an allowed namespace can be
    about a node or a Secret the scope hides;
  - `get_pod_logs` needs pods and the pod's namespace in scope.

  Anything outside the scope, including a kind that does not exist on a scoped cluster, fails with one
  "outside the scope" error that names only the cluster (`policy::out_of_scope`), so an agent cannot tell what exists
  beyond its scope. The
  app edits the scope in the Expose to AI dialog and in Edit AI Scope (`workspace::EditAiScope`, from the sidebar's
  "AI" badge, which reads "AI·" when the cluster is scoped, or the palette). Both show pickers
  (`ui/scope_picker.rs`): the cluster's listable kinds from discovery and, for the active cluster, its namespaces,
  plus whatever the scope already names, and a field to add a namespace by name (for another cluster, or one whose
  namespaces cannot be listed). Exposing writes the entry and its scope in one rewrite of `settings.toml`
  (`Command::SetAiExposed`, which, as before, writes nothing if the context moved since the dialog opened); exposing
  a re-pointed context again starts from the scope its old entry had. Edit AI Scope saves through
  `Command::SetAiScope` (`settings::set_ai_scope`), which changes the lists of an entry the file still holds for that
  context and server, with the scope the dialog opened with: it never exposes a cluster hidden while the dialog was
  open, nor overwrites a scope changed on disk meanwhile (it says to open the dialog again). A namespace still typed
  in the field is added on confirming; text that is not a namespace name keeps the dialog open, saying why. The
  kind picker does not offer the event kinds: events follow the namespaces and kinds chosen.
  - What a scope cannot hide: text in objects the agent may read can still name things outside it, such as the node
    a pod runs on (`spec.nodeName`, a scheduling event's message). Naming `namespaces` in the kinds scope shows every
    namespace's name.
- **Redaction:** Secret `data` and `stringData` values are replaced with a marker, and only the keys are kept. The
  items of a `SecretList`, which carry no `kind`, and whatever is read as a Secret are redacted the same way.
  `metadata.managedFields` is removed from all output.
  - **Credentials in text** ([ADR-0013](./adr/0013-agent-trust-model.md)): `flint_core::redact::redact_credentials`
    replaces each value that looks like a credential with `[REDACTED:<kind>]`, keeping the key, the scheme, the user
    and host of a URL, and the rest of the line. It covers:
    - `Authorization`/`Proxy-Authorization` header values (Basic, Bearer and other schemes, also in JSON and Go
      header maps; a Digest value is hidden to the end of the header, its closing quote when quoted or else the end
      of the line, so its `response` never shows however the parameters are separated or quoted; an AWS SigV4
      value is hidden through its last parameter, so its `Signature` never shows) and bearer tokens (8 characters
      or more, or shorter with a digit; a token that looks like a key with a quoted value, `Bearer password="x"`,
      is hidden with that value; after a credential key, `token: Bearer abc`, the scheme and the word after it
      are hidden together whatever its length, unless that word is the next field, a flag or `key=`);
    - `Cookie` and `Set-Cookie` header values, every cookie with its name: to the closing quote when quoted, to the
      closing bracket of a list (a JSON array, a Go header map, `[]string{...}`; a bracket inside a quoted cookie
      does not end it; quoted items become one quoted marker, `["[REDACTED:cookie]"]`, so JSON still parses), to
      the closing apostrophe of a single-quoted header or value (`curl -H 'Cookie: ...'`, `Cookie='sid=...'`), or
      else to the end of the line; a quoted cookie value (`session="v"`) does not end it. A JSON array that opens
      at the end of its line (pretty-printed) after a `"Cookie"` or `"Set-Cookie"` key is found over the whole
      text, and each item becomes `"[REDACTED:cookie]"`; a list is skipped only when every item is already a
      marker. This header rule and the `Authorization` one also match right after an escaped `\n`,
      as in a request dumped into a JSON string, where the value (a Digest one too) ends at the next escaped `\n`;
    - passwords in URLs (`scheme://user:pass@host`), up to the authority's last `@`; a password with an unencoded
      `/`, `?` or `#` is caught too, unless the part after the colon is a port (`:8443/`); and in Go MySQL DSNs,
      which have no `://` (`user:pass@tcp(host:3306)/db`, also `unix(` and `udp(`), from the DSN's first `:` to
      the `@` before the protocol; what ends the DSN tells what opened it: a quote (with as many backslashes, in
      escaped JSON) opens it at the last such quote before the `@`, so its password can hold spaces, brackets,
      apostrophes and escaped quotes (a closed address, such as a socket path, can hold spaces too); a bare DSN
      starts after the last space, comma or opening brace and a field name (`"dsn":`, `dsn=`), or after the
      last quote before it when that gives a longer password; DSNs in a comma list are each found;
    - registry `auth` values in docker config JSON (`"auth": "<base64 user:pass>"`, at any escape level, and the
      same key in YAML, also `auth :`, or a Python dict), when the value decodes to `user:password`;
    - AWS access key ids (`AKIA`/`ASIA`) and secret access keys (by key name, or a 40-character mixed-case value on
      the same line as a key id), GCP API keys, GitHub (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`),
      GitLab (`glpat-`, `glrt-`, `gldt-`) and Slack (`xox?-`, `xapp-`) tokens, Slack webhook paths, Stripe live keys
      (`sk_live_`, `rk_live_`), OpenAI (`sk-proj-`) and Anthropic (`sk-ant-`) keys, Azure SAS `sig=` values, JWTs;
    - private key blocks (PEM and PGP). The BEGIN and END lines are kept. A BEGIN with no END hides the rest of the
      text; an END with no BEGIN (a block whose start the log tail cut off) hides only the key lines right before
      it, so a message that merely names the END line is kept;
    - the values of keys ending in `password`, `passwd`, `secret`, `token`, `api_key`/`apikey`/`api-key`,
      `access_key`, `secret_key`, `secret_key_base`, `private_key`, `account_key`, `encryption_key`, `signing_key`,
      `master_key`, `ssh_key` or `credentials`, and of keys that are or end in `_pass`/`_pwd` (`DB_PASS`; not
      `bypass`), with `=`, `:`, `=>`, `:=` or a URL-encoded `=`, bare or quoted, as in logfmt,
      JSON (escaped JSON at any level, with escaped quotes inside the value), YAML, Ruby and Go, plus command-line
      flags such as `--password hunter2`. A quoted value runs to its closing quote. A bare password after `: ` (an
      unquoted password key, a colon and a space, as in YAML, headers and prose) runs to the end of the line, or to
      an unescaped double quote (other than one opening a value after `=`) or escaped `\n` before it, so
      `password: correct horse battery` is hidden whole, also when an earlier rule already hid its first word; a
      single-quoted header ends at its closing apostrophe. Any other bare value runs to the next space or quote,
      so `oauth token: unexpected status: 401` keeps the error. When a value is left alone, the search goes on
      right after its key, so `pass: 3 fail: 0 token=x` still hides the token. Masks a workload already wrote (`****`, `[REDACTED]`) are left alone, and so are a few values that
      are not credentials: a path after `PWD`/`OLDPWD` or a `credentials` key (`PWD=/app`,
      `GOOGLE_APPLICATION_CREDENTIALS=/var/run/key.json`), and a bare number after a bare `pass` key
      (`pass=1 failed=0`, `pass: 3 fail: 0`; `DB_PASS=123456` and `pass="123 example phrase"` are still hidden).

    The patterns err toward redacting. A test keeps the redactor as it was on master after #235 and checks, over
    every unit-test input and each credential shape in each wrapper a log puts it in, that every byte it hid is
    still hidden, apart from two intended narrowings: the quotes a quoted list keeps around its marker, and the
    text after a single-quoted cookie value's closing apostrophe.
  - **Where it applies:** every line `get_pod_logs` returns; every event message `get_events` returns; every
    `list_resources` cell (printer columns show event, status and condition messages); every string, at any depth,
    under a `note` key or a key ending in `message` or `messages` (also each string of a list there) in
    `get_resource` (event messages and notes, status and condition messages, container termination messages, which
    can be the last log lines, operators' `failureMessage` or `errorMessage`, and lists such as `messages`), every
    annotation value, at any depth (tools keep whole objects there; kubectl's last-applied configuration is
    parsed and redacted like the live object, so an argument after `--password` is hidden there too), and
    container
    `args` and `command` lists, where the element after a
    flag such as `--password` is hidden whole; and tool error text. ConfigMaps are left as they are. Logs are
    redacted before the 256 KiB cap, so each value is seen next to its label, and the cap drops a line it would cut
    in the middle; when that leaves nothing, because the last line alone is longer than the cap, `get_pod_logs`
    returns an empty `log` with a `note` that says so. Each result says how many values it returns replaced, not
    counting markers the workload wrote itself (`"redacted"` in JSON, a `# redacted=N`
    first line on YAML). The server's instructions and tool descriptions tell agents about the markers.
  - **Speed:** one Aho-Corasick pass over the whole text finds trigger words; only lines holding one are matched,
    and only against the rules those triggers start. Private key blocks are found first, over the whole text. The
    `regex` crate runs in linear time, so no input backtracks catastrophically. No pattern captures a value of
    open-ended length: a quoted value's or a rest-of-line value's end is found by one forward scan and the next
    match starts after it, so a line costs time linear in its length. A test redacts 1 MiB lines packed with quoted pairs in well under the
    2 s it allows.
  - **Not redacted:** ConfigMap data, literal `env` values in pod specs, and custom resource fields other than
    messages, annotations and argument lists reach the agent as the cluster returns them.
  - **The app is unchanged:** its log viewer, events and object views show the cluster's text as written, since
    people look at their own cluster there.
- **Activity log (UP-16, `flint_core::activity`):** every call is recorded as one JSON line in
  `agent-activity.jsonl` in the config directory: time, tool, cluster and the server it is exposed with, what the call
  named (kind, namespace, object, container), how many items and bytes came back, whether it was cut short, and
  whether it ended, failed or was denied (a cluster not exposed, where the agent reads "not found", or a request
  outside its scope, where it reads one "outside the scope" error whatever it named). The count and the cut-short flag are read from the reply's own fields, so they are what the agent saw.
  The returned content is never recorded. At 1 MiB the file is renamed to `agent-activity.1.jsonl` (replacing the one
  before) and a new one starts. Writers hold an exclusive `flock` on `agent-activity.lock` while they check, rotate
  and append, so several `flint mcp` processes (and concurrent calls in one) never rotate a file twice. Logging is
  best-effort: a writer waits at most 1 s for the lock, then skips its record, so a stalled writer cannot hold up
  other processes' replies; a write that fails is logged and does not fail the call. Readers take no lock: if the
  log rotated while they read the two files (the previous file is then another one), they read both again. The
  files, and the lock file, are the owner's only (0600: they name clusters and objects),
  and a line cut off before its newline is ended before the next record, so it costs only itself. Text fields hold
  what the agent wrote, so each is cut at 253 bytes: an agent cannot flush the log with a few huge names. Each record
  also has keys of the full context and server, which the app matches by. The server is the one the call's own
  settings and kubeconfig resolved; it is stored as shown (without credentials), and its key is of the endpoint the
  cluster pin compares (the kubeconfig's URL without a trailing slash), so two servers that show alike are not taken
  for one. That URL can hold a password or token, so the server key is an HMAC-SHA256 digest (ring, cut to 128 bits)
  under a random 32-byte key in `activity.key` beside the log: owner-only, made on first use by whichever process
  comes first (written to a file of its own, then linked into place without replacing one already there), and read
  by the core with the log, off the UI thread, for the app to key the tab's own server. Without a usable key file, a record has no server key and its tab matches it
  by context alone. The context key stays an unkeyed FNV-1a hash: context names are in the record anyway. Server
  keys from before the keyed digest no longer match, so those calls no longer show in their cluster's tab. A server
  key is never guessed from the shown server, which can drop a query or be a placeholder.
  `list_clusters` names no cluster, so its record holds the keys of each cluster it probed (the first 256, and a
  count of the rest, so one record stays far below the 1 MiB cap), and it shows in each of their tabs. When it
  probed more than it lists, it also shows in the tab of every cluster it does not list, marked as one that may have
  probed it, so no tab claims a call never happened. A call the router answers without running its tool (an unknown tool, arguments that do not fit) is
  recorded as failed, with what its arguments name. A context re-pointed at another
  server is another cluster: its activity tab shows the calls on the server it was opened for, newest first, and
  offers Edit AI Scope only while the context still points there. The core watches both files beside `settings.toml` and sends
  the log to the app (`Event::AgentActivity`) at start and whenever it changes. The cluster's "AI" badge, or Show
  Agent Activity, opens a dock tab with the cluster's calls, newest first, and Edit AI Scope.
- **Bounded output:** lists, events and logs have default and maximum sizes (for example 200 objects, 200 events
  and 500 log lines), and lists, objects and logs are also capped at 256 KiB. When
  output is cut short, the result says so, so a large cluster cannot flood an agent's context.
- **Bounded work (`flint_mcp::Limits`, #252):** rmcp starts a task per request and per reply and limits neither, so
  Flint does. At most 16 tool calls run at once and 48 more wait for a turn; past those a call is refused at once with
  a tool error ("too many tool calls in flight"; retry). A running call holds its turn until it ends: when the client
  cancels it (`notifications/cancelled`, which drops its Kubernetes requests, a connecting exec plugin included) or
  after 60 s, with a timeout naming the cluster's server (redacted like every reply). A wrapping transport counts the
  bytes of replies handed to it and not written yet, and the requests read and not answered: while the replies pass
  8 MiB it reads nothing more, so a client that stops reading stdout gets nothing more run or answered (refusals and
  replies to other requests included); a request read while 128 are unanswered is refused there at once, as is one
  whose id is still in flight; notifications, a cancellation in particular, always pass. A request counts until its
  reply reaches the transport, or, once cancelled, until nothing works on it (a tool call's handler has ended). When
  the client closes its end, the stream ends once every request read is answered and every refusal written, or once
  no reply has been written for the call deadline plus 5 s, so a client that stopped reading stdout cannot keep the
  process alive. While replies pass the budget, input is read ahead, unacted on, to see stdin close. Reading stops at
  128 messages or once 8 MiB is queued; the message that crosses 8 MiB is kept whole, and a line is at most 16 MiB, so
  the queue holds under 24 MiB of JSON. With that queue full stdin's end cannot be seen, so the same wait with no reply written counts the
  client as gone and ends the connection. Clusters connect through the
  per-cluster client cache, each connection bounded by its own 30 s timeout; the admission limit bounds how many
  connect at once. Every call is recorded in the activity log on its own path (refused, cancelled and timed-out calls
  as failed, tool calls refused at the transport too; a call refused or cancelled before it is admitted is recorded by
  its tool alone, since its arguments are unvalidated, and shows on no cluster's activity), waiting at most 2 s for the file; a record that cannot be
  written in time is skipped and logged.
- **stdout is the protocol channel.** Diagnostics go to stderr and Flint's own events also go to the rotating log
  shared with the app. Both destinations use `FLINT_LOG` (else `RUST_LOG`) if set. Otherwise stderr shows warnings
  and errors, adding info with `--verbose`, and the file records info (`flint_core::bootstrap::log_filter`). A
  writer thread owns the file, so logging never blocks the thread that emits an event; when it falls behind, events
  are dropped from the file but still reach stderr.

The read functions (dynamic list, get, events, logs) live in `flint-core`, where the UI uses them as well. Only the
tool layer and the policy (opt-in, limits) live in `flint-mcp`. The redaction rules live in `flint_core::redact`, so
the app and the MCP server hide the same Secret values. The MCP server applies the credential redactor to text in
tool results. The shared tracing subscriber also applies it to process diagnostics.

## Look and keyboard

Flint looks and behaves like Zed: dense and minimal, with thin borders, tight spacing, and monospace text where it
helps (names, YAML, logs). The keyboard is a primary way to use the app.

- **Color:** neutral gray surfaces, in the style of Zed. Orange is only an accent: selection, focus rings, primary
  buttons, the active cluster, and links. Status colors (red, yellow, green) keep their usual meaning and must stay
  readable against both themes.
- **Themes:** two built-in themes, Flint Light and Flint Dark, defined as gpui-component theme data. They follow the
  macOS appearance and change with it live. An `appearance` setting (`system` | `light` | `dark`) overrides this.
  User theme files are not supported in v1, but because themes are data, they could be added later.
- **Theme tokens only:** views take every color from theme tokens and never hard-code one. This applies from M0
  onward, so the M1.5 theme work does not mean restyling every view.
- **Keys:** default keys use GPUI's `secondary-` modifier, which is Cmd on macOS and Ctrl on Linux, so the shortcuts
  below read Ctrl-… on Linux. Linux has no menu bar: Quit and Close Window work by their keys, and the palette
  (Ctrl-Shift-P) reaches every menu item, View > Appearance included.
- **Menus (macOS):** the standard ones, every item a registered action in category Flint. The app menu has About
  Flint, Hide Flint (Cmd-H), Hide Others (Option-Cmd-H), Show All and Quit; Edit has Cut, Copy, Paste and Select All,
  which the focused text field handles (the list filter, the namespace field, the log search, the palette), and
  Copy and Paste also a shell tab; View has
  Show Command Palette, Go to Resource, Go Back and Appearance; Window has Close Dock Tab and Close Window. The Edit actions are bound only in
  a text field's key context (`Input`), so a shell tab keeps Cmd-C and Cmd-V for itself (and Linux shells keep Ctrl-C,
  Ctrl-V, Ctrl-X and Ctrl-A, which are in `TERMINAL_PASSTHROUGH`). Hide has no key on Linux, where Ctrl-H is a
  shell's backspace.
- **Command palette (Cmd-Shift-P, `ui/palette.rs`):** a fuzzy search over every action that is available where focus
  was when it opened: `registry::available_in(previous_focus)`, palette-visible actions only, through the registry's
  guards, so it never offers what would do nothing. Each row shows the label, the category and the shortcut that fires
  there (`ActionSpec::shortcut_in`: of the bindings for its keys at that focus's key contexts, in precedence order,
  the first whose action would run; Cmd-R in a log tab is Refresh Object while the tab has nothing to reconnect), in a
  column of its own so the categories line up. Typing filters (a subsequence match, ignoring case as Unicode lower
  case does, that prefers word starts and runs, `palette_model.rs`); the arrows or Ctrl-N and Ctrl-P move the
  selection; Enter or a click runs it; Esc or a click outside closes it and gives focus back. The panel is as tall as
  its content and takes the pointer events over it, so nothing behind it gets them. With no query, recently run
  actions come first (for the app session); with one, the closer match wins and recency only breaks ties.
  - Where from: the focus before the palette opened, while it is still drawn in the workspace; once it is not (a
    menu that closed behind the palette), the workspace's focus target, as `registry::perform` uses. GPUI would
    otherwise treat the stale focus as the window's root, where Flint's actions are refused and App ones lost.
  - Running: the palette checks the action from there (`registry::check_in`), gives focus back, closes, and
    dispatches it there (`registry::dispatch_in`), exactly as its key would. A refusal (the guard's reason) stays
    in the palette's footer instead of closing it.
  - Arguments: an action that takes them (Select Cluster…, Open Kind…, Sort by Column…, Open Logs by Label Selector…,
    the namespace and container ones) asks for each required one in turn: a list of the clusters, namespaces, kinds
    the active cluster serves, columns drawn or the shown pod's containers, or typed text (a label selector). Values
    equal to the text typed come first: a stored value as typed, then ignoring case, then a shown label (so contexts
    `prod` and an EKS ARN shown as `prod` do not collide, nor `ABC` and `abc`), and the action always
    gets the stored value; else, where a typed value is allowed (a namespace not listed, a selector), the text typed
    is the first row and selected. Typed text is trimmed, a picked value never is. Open Container Shell… offers only
    the running app containers a shell can open in (a workload's first ready pod's), Open Container Logs… every
    container. An empty pick list says so ("No clusters", "No containers"). Optional arguments keep their defaults.
    Each step starts with focus in the field, also after a click on a row. The heading shows the action and the picks
    so far; Esc, or Backspace in the empty field, goes back a step keeping the earlier picks, and back to the actions
    with the query that found it; a click outside closes the palette. A refusal at the last step keeps the earlier
    picks, so Enter retries with them. An open step's choices follow the data it lists (the clusters, discovery, the
    namespaces, the shown pod), keeping the query, and the selection when it was moved with the arrows (else the best
    row, so an exact match that arrives is selected): the palette observes the models already in memory, and fetches
    nothing.
  - The Edit actions are listed only where they act: in a text field, or in a shell tab, which handles Edit > Copy and
    Edit > Paste with its own copy and paste (the paste checks included). gpui-component's root handles Copy for text
    selected anywhere, so availability alone would list it everywhere.
  - The window going to the background (Cmd-Tab, Hide) keeps the palette open, as a dialog stays; focus moving
    elsewhere in the window closes it.
  - Opening it dismisses an open context menu, drop-down or popover (the namespace filter), as Esc would, so none
    stays drawn over it. The namespace filter's popover is closed by its open state, which the workspace holds, so it
    closes also when focus has moved off it.
  - A click carries the version of the rows it was drawn on (`palette::PalettePick { row, rows }`); when a live
    refresh has replaced the rows since, it is dropped rather than pick the entry now in that row. When the menu
    gives focus back, the palette runs from there; else from the workspace.
  - Speed: matching runs when the query changes, never while drawing, over labels prepared when the palette opens,
    with a cheap subsequence pass before the scoring; the rows are a virtual list, so long argument lists (thousands
    of namespaces) stay within a frame per keystroke.
  - Later features register their actions and the palette lists them: change namespace, toggle watch mode, toggle
    Expose to AI, disconnect a cluster, open the diagram, analyze the selected log lines.
- **Action registry (`flint-app/src/ui/registry.rs`):** every interactive control is a named GPUI action with a Title
  Case label, a description, a category (Flint, Cluster, Namespace, View, List, Detail, Logs, Terminal), typed
  arguments (so a palette can offer pickers), default keys, and whether the palette shows it (automation-only actions
  such as a pane width, a row or a log line by id are hidden).
  - Views own the handlers and list them once (a `bind_actions` function per view). The workspace also forwards the
    list's, the detail pane's and the navigation's actions, so they and their keys work wherever focus is.
  - The workspace installs guards: when each action is enabled, and whether given arguments would do anything.
    `registry::dispatch`, `perform` and `dispatch_in` check them first and refuse instead of dispatching a no-op; the
    handlers call the same checks.
  - Every click is `registry::on_click(action)`, so clicks, keys, the palette and the test automation channel take
    one path. `scripts/check-click-actions.sh` (CI) fails on a click handler that is not.
  - Plain keys (arrows, Esc) are bound in `Workspace && !Input`, so they never fire while typing in a field.
  - The palette lists `registry::available_in(previous_focus)` with `ActionSpec::shortcut` (the live binding), and
    dispatches with `registry::dispatch_in(previous_focus, …)`. Its own keys (`palette::*`) are registry actions too,
    bound in `CommandPalette > Input` so they win over the search field's arrows, Enter and Esc, and hidden from it.
  - A new control is not done until it is in the registry; a unit test per view checks that it binds every action
    of its scope.
- **Resource switcher (Cmd-P, Ctrl-P on Linux; a mode of the palette view, `switcher_model.rs`):** a fuzzy search
  over the active cluster's listable kinds and the objects its watches hold in memory, jumping straight to a list or
  a detail view. Rows show the namespace/name (or the kind's label), the kind and the cluster.
  - Never the network: the objects come from `CoreHandle::stored_snapshots` (above), so a kind not watched now (On
    view mode, or still loading) has no objects listed. Its kind is still listed, its row saying how many of its
    objects are in memory or that they are not loaded; in On view mode a note under the footer says that only the
    kinds being watched list their objects. A failed discovery is said as such.
  - Off the render path: the kinds are listed at once, and the objects arrive a moment later, once the stores are
    read and the index is built on a background thread. Each keystroke runs the search once: when the query only
    grew, over the last query's matches; a subsequence pass rules out most objects, a linear-time score with no
    allocation (`Matcher::quick_points`) ranks the rest, and only the best 200 get the palette's full scoring for
    their order and highlights. The rows are a virtual list. A unit test types a name over 50,000 objects within
    100 ms a keystroke in an unoptimized build (about 16-23 ms here).
  - "pods web" finds the pod `web`: an object also matches its kind's label followed by its text, with less weight.
  - The name counts most: the query being the whole name (the object's without its namespace, or the kind's label)
    earns the most extra points, the name starting with it fewer, and the query found in the name alone a few (always
    for a kind or a cluster-scoped object, whose text is the name; while ranking every object it is judged from
    where the quick match starts, and the best 200 check the name itself), so
    Deployment `demo/web` comes before ConfigMap `demo/web-cfg` for "web". Equal scores list the shorter text
    first. While the objects arrive, and when the switcher lists anew on the same cluster (a kind renamed),
    the selection stays on the item the user moved to once it is listed again, and a click drawn on the
    rows before (`PalettePick`'s rows version, as in the palette) is dropped, so it cannot open the object now in
    its row.
  - Enter on a kind runs Open Kind… (`workspace::OpenView`); on an object, Open Object… (`workspace::OpenObject`,
    hidden from the palette), which opens the kind's list with the object selected, so its details show. The
    namespace filter is never changed for it: when the filter hides the object, its row is not listed, its details
    still show, and the list says so (`list-filtered-out`). The selection shown before is cleared at once; when the
    first current list lacks the object, the list says it no longer exists (`list-missing`) and stops waiting, so
    one made later with the name is not selected. Only a list published after the list subscribed counts: the
    first snapshot a subscription gets may be a watch's last publish from long before (a kind viewed earlier). Clearing the selection drops a pick still waited for. Both go through the registry from where focus
    was, as the palette runs any action, and are not recorded as recent actions.
  - An open switcher follows what is active: when another cluster (or API server) becomes active, or the
    active cluster's discovery arrives or fails, or its watch mode changes, it lists anew with what was typed
    kept; objects read for an earlier scope are dropped when they arrive late. An Enter names the cluster and
    server its row was listed on, and Open Kind… and Open Object… refuse it
    once another is active, so a row of one cluster never opens a same-named object on another.
  - On a namespace-limited list, an object opened is judged gone only once its own namespace was listed anew:
    the merged list says which rows of each namespace it holds (`NamespaceLimit::revisions`, each the
    `KindSnapshot::revision` of that namespace's list), since it can be published again from a namespace's old
    rows. A revision moves only when a list completes or a row changes, never on a republish of the same rows; a
    quiet namespace is not proven gone until it is listed again, and the pick waits (clearing the selection drops
    it). Go Back knows more: each remembered object carries the revision of the rows it was last seen in, so rows
    newer than that without it prove it gone at once, even the rows the list opened again starts from (an object
    deleted while another kind was shown).
  - Cmd-Shift-P in the switcher swaps it for the palette, and Cmd-P back, keeping where focus was. On Linux, Ctrl-P
    in the palette's field moves the selection up (its own key wins there), and a shell tab passes it to the shell.
- **Panes and Go Back (UP-33, `workspace_keys.rs`):** the cluster sidebar (key context `ClusterSidebar`) and the
  navigation (`Navigation`) take focus, by Tab or a click. Tab and Shift-Tab (`workspace::FocusNextPane`,
  `FocusPreviousPane`, bound in `Workspace && !Input`) move focus through the panes shown: sidebar, navigation, list,
  detail pane, the dock's shown tab, and around. A text field keeps Tab for moving between controls, and a shell tab
  passes it to the shell.
  - In the sidebar, the arrows move a cursor over the clusters (it starts on the active one) and Enter connects to the
    one under it (`sidebar::*`). In the navigation, the arrows move over the section headers and the listable kinds
    of open sections (starting on the open kind), Enter opens a kind (its list takes focus) or opens and closes a
    section, Right opens a section and Left closes it, from a kind its own (`navigation::*`). These are bound on the
    pane's own element, so they are available, and listed by the palette, only while it has focus; their keys are in
    `ClusterSidebar && !Input` and `Navigation && !Input`, deeper than the list's arrows in `Workspace && !Input`.
  - The list and the detail pane, which have no cursor of their own, show focus with a 1 px ring in the same
    colour drawn over their edge while the pane itself has focus, so the layout does not move and clicks reach
    the rows. The sidebar's cursor brings its cluster into view as it moves. Enter on the kind already open puts
    focus in its list.
  - The cursor shows as a 1 px border in the theme's `ring` color while its pane has focus; every row has the
    border, transparent off the cursor, so the ring moving shifts nothing (nor a row's lock button). The lock
    button is not a tab stop; from the sidebar, the palette's Unlock Cluster…, Lock Cluster Again…, Trust
    Cluster… and Stop Trusting Cluster… reach it from the keyboard. A click, or a cluster or kind picked any
    other way, moves it there.
  - Go Back (Cmd-[, `workspace::GoBack`) returns to the kind and object shown before, with focus in the list:
    opening another kind, or jumping from the switcher, remembers the list and its selected object (at most 50
    places). It is available only while a remembered kind is still served; places of kinds no longer served are
    skipped. It is per active cluster: switching clusters forgets them. In a text field Cmd-[ is the field's
    (outdent).

## Settings and state

- Settings and UI state live in Flint's config directory: `FLINT_CONFIG_DIR` if set, else `~/Library/Application Support/Flint/`
  on macOS and `$XDG_CONFIG_HOME/flint/` (default `~/.config/flint/`) on Linux. `state.toml` stays beside
  `settings.toml` on Linux too, so the same directory override covers both files
  ([ADR-0016](./adr/0016-linux-appimage.md)).
- `settings.toml`: `appearance`, `ai_exposed_clusters`, `trusted_clusters`, `protect_local_clusters`, extra kubeconfig paths, watch mode (global plus per-cluster), log buffer size (`log_buffer_lines`, `log_buffer_mib`, and `log_total_mib` for all tabs together), a `path`
  override, and a `helm` binary override. From M5.5 also `warm_clusters`, `warm_memory_mib`,
  `log_analysis_clusters` (context, server URL, CLI, binary path, and whether to confirm each send), the log
  analysis line limit, and a `claude` binary override.
- `state.toml`: window layout (the widths of the cluster column, navigation and detail pane, and the dock's height,
  which the user drags), cluster order and pins in the sidebar, the last view open in each cluster, and the namespace
  filter of each cluster, and each list's sort per cluster and kind (`[list_sorts.<cluster>]`, the column's key and
  direction; a column that is gone falls back to the kind's default order). Planned: per cluster, the expanded "… N
  more" rows of the tree and the old-revisions toggle.
- `agent-activity.jsonl` and `agent-activity.1.jsonl`: the agent activity log, written by `flint mcp` and rotated at
  1 MiB ([MCP server](#mcp-server)).
- `flint.log`: process diagnostics from the app and MCP, in `~/Library/Logs/Flint/` on macOS and
  `$XDG_STATE_HOME/flint/` on Linux (default `~/.local/state/flint/`). The file is owner-only and keeps three
  backups, rotating by size at 4 MiB. Its events come from Flint targets. If the file is unavailable, diagnostics
  still go to stderr.
- `recordings.sqlite` (after 1.0): recordings, their log lines, events and metric samples, in typed tables
  ([ADR-0018](./adr/0018-recordings.md)). Retention settings and per-cluster auto-record live in `settings.toml`.
- Flint never writes to kubeconfig files.

## Milestones

| Milestone | Delivers |
|---|---|
| M0 | Cargo workspace, GPUI window, kubeconfig loading, cluster sidebar, connect and health check, PATH resolution, read-only `flint mcp` server with Expose to AI toggle |
| M1 | Discovery-driven navigation, Table lists (spike first), watch manager with both modes, generic detail and YAML view, namespace filter |
| T | Test foundation: unit tests for every module, headless view tests, automation channel (test builds only), `flint-e2e` harness, recorded fake API server, Linux kind e2e, e2e screenshots ([TESTING.md](./TESTING.md)) |
| M1.5 | Look and keyboard: Flint Light and Flint Dark themes with orange accents that follow macOS, command palette, resource switcher |
| M2 | Log viewer with streaming, search, timestamps and error highlighting; exec terminal |
| M3 | YAML editing with inline syntax and schema validation, dry-run and diff, delete/scale/restart/cordon/drain, clusters protected by default |
| M4 | Curated views for core kinds, metrics-server columns and node/pod usage, resource diagram, cluster dashboard with thresholds and `get_cluster_summary` |
| M5 | Port-forward panel with local ports and open in browser, Helm releases |
| M5.5 | Multi-cluster and AI (history). Cluster sets (super mode), named sets, colour tags and a set dashboard are now scheduled across v0.5 and v0.6. Warm clusters are after 1.0 (#247). AI log analysis is not scheduled |
| M6 | Universal (arm64 + x86_64) build, signing with hardened runtime, signed DMG, notarization and stapling, Homebrew cask, first public release |
| M7 | Fleet actions on a cluster set (v0.7): rollout restart, scale, apply YAML and delete, with a per-cluster dry-run preview, one cluster at a time, stopping at the first failure |
| M8 | Recordings: background capture of a workload's logs, events and metrics into SQLite, automatic recording on trouble, a timeline view, MCP access and export |

## Risks

- **GPUI churn.** GPUI is young and its API breaks often. Pin a git version, upgrade on purpose, and keep view code
  thin so breaking changes stay small.
- **tokio and GPUI bridge.** Mistakes here cause deadlocks or a frozen UI. Keep the bridge narrow and
  message-based, and never block the main thread on core work.
- **Memory with watch mode All.** Clusters with tens of thousands of objects can use a lot of memory. On view mode is
  the escape hatch. If more is needed, metadata-only watches (`kube::runtime::metadata_watcher`) can serve kinds whose
  list does not need the full object.
- **Table plus watch.** If watch streams in Table format turn out to be unreliable, lists can be built from full
  objects with columns computed on the client (built-in definitions plus CRD printer columns). This costs more code
  but stays within ADR-0003.
- **Exec plugin latency.** Some auth plugins take seconds to run, and some hang (a browser login that never
  completes). Flint runs the plugin itself (`flint_core::auth_plugin`) instead of letting kube run it on a blocking
  thread: the plugin is a child in its own process group (in a session of its own, with no terminal, when it may not
  prompt, so it cannot write into another plugin's prompt), killed with what it started when its connection attempt
  times out or is dropped. The group is killed only while the plugin is not reaped yet, so its id cannot name another
  group; stdout is capped at 1 MiB (past it the plugin is killed), stderr keeps its first 64 KiB, and a helper that
  keeps the output open after the plugin exited gets a second before what the plugin printed is used. A plugin that
  may prompt, with Flint in the foreground of a terminal, is given the terminal once it started (one such plugin at
  a time; the next waits 1.5 s for it, then runs without prompting, as `interactive: false`; one set to
  `interactiveMode: Always` waits as long as its caller) and Flint takes it back after, with the terminal's settings as they were (echo back on after a killed password prompt). Meanwhile Flint ignores
  SIGTTOU, so a log line it writes to the terminal with `stty tostop` set does not stop it. When Flint is a
  background job of its terminal, a plugin that may prompt runs without prompting, and an `Always` one waits until
  Flint is brought to the foreground. Errors name the plugin by its file name when its command has no whitespace; otherwise they say
  "the exec auth plugin" and show nothing of the command (it may hold arguments, and telling a spaced path apart would
  mean reading the disk), so no argument text ever shows. Tokens are cached until a minute before they expire. A
  refresh runs in a task of its own per client, bounded by the connect timeout: requests wait for it, and one that
  gives up (a short request deadline) does not restart a slow login; dropping the client ends it, plugin included. Requests use what the refresh got, even a
  token that expires within the margin, as kube does. A refresh that hangs is a sign-in failure naming the plugin
  (`ClusterError::ExecPlugin`, which keeps the plugin's name in what the app shows); a prompting plugin then rests for five minutes (requests say until when) unless someone selects or
  retries that cluster, or opens or restarts a shell on it. A proxy is refused before the plugin runs, as kube's builder did. Connection progress shows in
  the UI.
- **No `helm` installed.** Covered by an install hint. Bundling the binary is a possible follow-up.
- **AI CLI flags change.** `claude` is young, and its flags can change. Keep the flags in one place, and fail
  closed when the CLI rejects them or its `system/init` check fails.
- **Log lines leave the machine.** Redaction is pattern-based and can miss a secret. The confirmation shows exactly
  what is sent, so the user is the last check.
- **Memory with warm clusters.** Covered by the count, the memory budget and the idle limit. Watch storms on a warm
  cluster cost CPU, but no view redraws for them. The target is under 1% CPU with 3 idle warm clusters.
- **Only one AI CLI.** Codex users get no Analyze button in v1. Support is reconsidered when `codex exec` can turn its
  tools off (ADR-0011).
