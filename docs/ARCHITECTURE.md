# Flint architecture

Flint is a native desktop app for managing Kubernetes clusters, in the space of Lens and Freelens. It exists because
Electron-based tools are slow to start, heavy on memory, and sluggish on large clusters. Flint's main goal is **speed
and low memory**. Native look and feel comes second.

This document describes the shape of the system. The reasons behind the hard-to-reverse choices are recorded as ADRs
in [`docs/adr/`](./adr/). The journeys every release must keep working, with stable IDs for tests, are in
[`docs/USER-PATHS.md`](./USER-PATHS.md). How they are tested is in [`docs/TESTING.md`](./TESTING.md).

## Scope

### v1 goals

- macOS only. The core stays portable so Linux and Windows can follow ([ADR-0001](./adr/0001-rust-and-gpui.md)).
- Multiple clusters, loaded from the user's kubeconfig files, shown in one window with a cluster sidebar. The
  clusters used most recently stay warm, so switching between them is instant
  ([ADR-0012](./adr/0012-keep-recently-used-clusters-warm.md)).
- Browse every resource kind, including CRDs, with live updates ([ADR-0003](./adr/0003-generic-resource-views.md)).
- Pod logs: follow, search, pick a container, show previous logs, toggle timestamps, and highlight errors and
  warnings.
- AI log analysis: send selected log lines, redacted, to the user's own Claude Code CLI (`claude`) and read the
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

- Linux and Windows builds.
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
| Settings | TOML in `~/Library/Application Support/Flint/` |
| Helm | The user's `helm` CLI, called as a subprocess |
| Packaging | `cargo-packager` (or `cargo-bundle`), universal binary via `lipo`, `codesign` with hardened runtime for both app and DMG, `notarytool` and `stapler`, GitHub Actions on macOS |
| Distribution | Signed, notarized DMG on GitHub Releases, plus a Homebrew cask |

## Workspace layout

```
crates/
  flint-core/   Kubernetes logic: kubeconfig, connections, discovery, watches, stores, actions,
                logs, exec, port-forward, metrics, Helm. Depends on tokio and kube. Never on GPUI.
  flint-mcp/    The MCP server (stdio): tool definitions, cluster opt-in checks, output limits. Depends on flint-core.
                Redaction rules live in `flint_core::redact`, shared with the app.
                Its `flint-mcp` binary is the same server without GPUI, for Linux CI only; not shipped.
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
  Events come back on another such channel, which a GPUI task drains to update entities.
- Watches can change faster than the UI should redraw. The core marks the store as changed and sends at most one
  change event per view per frame budget (about 100 ms). The view then reads the latest snapshot. Individual watch
  events are never forwarded to the UI one by one.
- Snapshots are shared, read-only data (`Arc`). Views never hold locks on core state.

## Clusters

- A **cluster** in Flint is one kubeconfig context. Kubeconfig files are read from `KUBECONFIG`, `~/.kube/config`,
  and any extra paths the user adds in settings. Flint watches those files and reloads when they change.
- Connecting runs the context's auth: client certificates, tokens, and exec plugins (`aws`,
  `gke-gcloud-auth-plugin`, `kubelogin`, …). `kube` handles all of these.
- **PATH:** apps started from Finder do not get the user's shell `PATH`, so exec plugins and `helm` would not be
  found. When Flint starts, it runs the user's shell once as an interactive login shell (`$SHELL -ilc env`), so that
  both `.zprofile` and `.zshrc` are read. The call has a timeout. If it fails, Flint falls back to the current `PATH`
  plus `/opt/homebrew/bin:/usr/local/bin`. A `path` value in `settings.toml` overrides all of this. Flint sets the
  result as its own process `PATH` before starting the tokio runtime or any threads, because `kube` starts exec
  plugins with Flint's own environment. VS Code does the same.
- One window shows a cluster sidebar. Exactly one cluster is **active**, meaning the one in view.
- The last N clusters the user selected (`warm_clusters`, default 3, counting the active one) stay **warm**: connected,
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
  sortable, filterable Cluster column, or group them by cluster with a toggle. Columns are the union by name; the
  resource tree lists every kind any member serves, with a `2/3` badge when only some do. Every member of an open set
  is connected and watched like the active cluster. Rows keep their cluster: details and actions act on the row's
  own cluster. Each cluster may have an optional colour tag, shown in the sidebar, the Cluster column and the detail
  header.
- **Protection:** every cluster is protected (read-only in Flint) by default, except clusters on the user's machine
  and clusters marked trusted. A session unlock (one confirmation naming the cluster and server) lasts until Flint
  quits ([ADR-0015](./adr/0015-clusters-are-protected-by-default.md)).
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

### Curated views

About 10 core kinds get hand-built detail views: Pod, Deployment, StatefulSet, DaemonSet, Job, CronJob, Service,
Ingress, Node, and Namespace. Every other kind uses the generic detail view, which shows metadata, conditions, events,
and YAML. A curated view is an addition to the generic view, never a replacement: each kind can always show its YAML.

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

- **Sessions** (`flint_core::logs`): `CoreHandle::open_logs` returns a `LogSession`; dropping it stops the stream.
  Sessions belong to no cluster slot, so they keep running across cluster switches. `timestamps=true` is always
  requested, so showing timestamps is a view toggle, and a dropped stream resumes from the last timestamp (with
  backoff, skipping lines already shown). A followed stream that ends is reopened only while the container can log
  again (it restarts, or the pod still runs).
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
  id from each snapshot, so following costs only the new lines. Timestamps, wrapping and a workload's
  `[pod/container]` prefix are view toggles, and a workload tab counts its streams in the toolbar. Copy and Save
  (after a confirmation, then the native save dialog) write lines as shown, not redacted. Its pure parts (snapshot
  diff, line text and highlights, state wording, save text) are in `log_model.rs`.
- **Severity** is classified once per line on arrival. **Search** (`LogSearch`) remembers how far it scanned, so
  following with a search open scans only new lines; there is no index. **Selection** (`LineSelection`) is a range
  of line ids that shrinks when its start is evicted, and is what copy and AI log analysis read.
- **Highlighting:** the view colors each line by its severity with the theme's error and warning tokens. The rules
  live in `logs::classify`: case-insensitive whole words; `ERROR`, `FATAL`, `panic`, and any word ending in
  `Exception` or `Error` (such as `NullPointerException`) mark an error; `WARN` and `WARNING` mark a warning; an
  error wins over a warning.
- **Copy** puts the selected lines on the clipboard as shown (with timestamps only when they are shown) and without
  redaction. AI log analysis redacts what it sends (see below). In a workload session copy keeps the
  `[pod/container]` prefix unless the view hides it (`LineSelection::text_as`).
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

`flint-app/src/ui/dock.rs`: a resizable panel under the main area, with one tab per session that outlives navigation
(log tabs and shell tabs now; AI log analysis later). Tabs belong to no cluster. Opening a session that
already has a tab shows that tab. Cmd-J shows and hides the dock; Cmd-W closes the active tab while focus is in the
dock (elsewhere it closes the window). A new tab kind implements `DockItem` and adds a `DockTab` and `TabKey` variant;
the module documentation lists the steps.

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
- **Shell choice.** Without an explicit command, `bash` is tried, then `/bin/sh`. A missing executable is reported on
  the exec status channel before the command prints anything (Docker prints it on the TTY and exits with 126 or
  127 instead), which tells it apart from a shell that ran and failed. Keystrokes typed meanwhile go to the shell that starts.
- **Shell tab** (`flint-app/src/ui/terminal_view.rs`): opened by Shell in a pod's detail pane (`detail::OpenShell`,
  ⇧⌘E; a container picker when the pod has several app containers, `detail::OpenContainerShell`). While its command
  runs, Shell on the same container shows the tab; once it ended, Shell opens a new one beside it. The grid is painted
  from each snapshot with the theme's mono font: one quad per run of equal background, then each row's text shaped
  with every glyph forced to the cell width (wide characters and combining marks shaped on their own), so columns
  stay aligned. Colors come from the theme: the default foreground, background, cursor (the accent) and selection are
  theme tokens, the 16 ANSI colors are a `terminal` palette per theme in `themes/flint.json` (checked for contrast in
  both themes), 16 to 255 are xterm's table (`terminal_palette.rs`). Bold, italic, underline, strikethrough, dim,
  hidden and inverse are drawn; the cursor is a block, beam or underline, hollow without focus; the bell flashes the
  screen. The grid size comes from the element's size and the font's advance; a change is sent after 80 ms without
  further change (the first at once). The header says Connecting, Running (command), Exited (code) or Failed
  (reason); after the end the screen stays scrollable and copyable. Closing the tab drops the session.
- **Keys in a shell tab.** The tab's key context (`Terminal`) binds Tab, Shift-Tab, Esc, Return, Backspace, Delete,
  Space, the arrows, Home/End, Page Up/Down and Ctrl with a letter or symbol to nothing
  (`registry::TERMINAL_PASSTHROUGH`); it is the deepest context, so those keys skip the app's bindings (focus
  traversal, list keys, and the app's Ctrl shortcuts on Linux) and reach the tab's key handler, which encodes them
  with the program's modes (`terminal_keys.rs`: application cursor keys, xterm modifier parameters, Ctrl codes,
  function keys; Alt as Meta except on macOS, where Option composes text). Text, including what an input method
  composes, arrives through the platform text input. What stays the app's: Cmd shortcuts on macOS; on Linux,
  Ctrl-Shift ones (copy is Ctrl-Shift-C, paste Ctrl-Shift-V) and Ctrl-Tab for the dock. Known conflicts: on Linux the
  app's `secondary-` shortcuts (Ctrl) do not work while a shell has focus; Shift-Page Up/Down and Shift-Home/End
  scroll the tab instead of reaching the program; mouse reporting (programs that ask for clicks) is not sent, the
  mouse always selects.
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

Delete, scale, rollout restart, cordon, and drain are typed actions in `flint-core`, each with a confirmation in the
UI. Every write refuses on a protected cluster ([ADR-0015](./adr/0015-clusters-are-protected-by-default.md)), and
the UI offers a session unlock in its place. Destructive actions show the cluster and namespace in the confirmation. Kubernetes has no drain API, so drain
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
member, worst first, under a totals strip. `get_cluster_summary` gives agents the same summary.

### Metrics

Flint polls `metrics.k8s.io/v1beta1` `PodMetrics` and `NodeMetrics` about every 15 s, and only for views that are
open. If metrics-server is not installed, the metric columns are hidden and a hint is shown
([ADR-0005](./adr/0005-metrics-server-only.md)).

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
| `list_clusters` | Clusters marked Expose to AI: name, server, and whether a connection works |
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
- **Scope:** an exposed cluster may also list the namespaces and kinds an agent may read
  ([ADR-0013](./adr/0013-agent-trust-model.md)). Empty means everything. Requests outside the scope fail like an
  unexposed cluster. With a namespace scope, cluster-scoped kinds are hidden unless the kinds scope names them.
- **Redaction:** Secret `data` and `stringData` values are replaced with a marker, and only the keys are kept.
  `metadata.managedFields` is removed from all output. Pod logs and event messages go through a shared redactor that
  replaces values that look like credentials (auth headers, JWTs, private keys, cloud access keys, `password=`-style
  pairs), and the result says how many were replaced.
- **Activity log:** every call is recorded (time, cluster, tool, what it named, result size, cut short or denied),
  never the returned content. The app shows it per cluster.
- **Bounded output:** lists and logs have default and maximum sizes (for example 200 objects and 500 log lines). When
  output is cut short, the result says so, so a large cluster cannot flood an agent's context.
- **stdout is the protocol channel:** all logging goes to stderr.

The read functions (dynamic list, get, events, logs) live in `flint-core`, where the UI uses them as well. Only the
tool layer and the policy (opt-in, limits) live in `flint-mcp`. The redaction rules live in `flint_core::redact`, so
the app and the MCP server hide the same things.

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
- **Command palette (Cmd-Shift-P):** a fuzzy search over every action. Each feature registers its actions, such as
  switch cluster, change namespace, toggle watch mode, toggle Expose to AI, disconnect a cluster, open the diagram,
  or analyze the selected log lines.
- **Action registry (`flint-app/src/ui/registry.rs`):** every interactive control is a named GPUI action with a Title
  Case label, a description, a category (Flint, Cluster, Namespace, View, List, Detail), typed arguments (so a palette
  can offer pickers), default keys, and whether the palette shows it (automation-only actions such as a pane width or
  a column header are hidden).
  - Views own the handlers and list them once (a `bind_actions` function per view). The workspace also forwards the
    list's, the detail pane's and the navigation's actions, so they and their keys work wherever focus is.
  - The workspace installs guards: when each action is enabled, and whether given arguments would do anything.
    `registry::dispatch`, `perform` and `dispatch_in` check them first and refuse instead of dispatching a no-op; the
    handlers call the same checks.
  - Every click is `registry::on_click(action)`, so clicks, keys, the palette and the test automation channel take
    one path. `scripts/check-click-actions.sh` (CI) fails on a click handler that is not.
  - Plain keys (arrows, Esc) are bound in `Workspace && !Input`, so they never fire while typing in a field.
  - A palette lists `registry::available_in(previous_focus)` with `ActionSpec::shortcut` (the live binding), and
    dispatches with `registry::dispatch_in(previous_focus, …)`.
  - A new control is not done until it is in the registry; a unit test per view checks that it binds every action
    of its scope.
- **Resource switcher (Cmd-P):** fuzzy search over the kinds and objects of the active cluster, jumping straight to
  a list or a detail view. It searches the in-memory stores, so it never makes network calls.

## Settings and state

- `settings.toml`: `appearance`, `ai_exposed_clusters`, extra kubeconfig paths, watch mode (global plus per-cluster), log buffer size (`log_buffer_lines`, `log_buffer_mib`), a `path`
  override, and a `helm` binary override. From M5.5 also `warm_clusters`, `warm_memory_mib`,
  `log_analysis_clusters` (context, server URL, CLI, binary path, and whether to confirm each send), the log
  analysis line limit, and a `claude` binary override.
- `state.toml`: window layout (the widths of the cluster column, navigation and detail pane, and the dock's height,
  which the user drags), cluster order and pins in the sidebar, the last view open in each cluster, and the namespace
  filter of each cluster.
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
| M5.5 | Multi-cluster and AI: warm clusters for instant switching, cluster sets (super mode) combining 2 to 5 clusters in one view with named sets, colour tags and a set dashboard, AI log analysis through the user's Claude Code CLI |
| M6 | Universal (arm64 + x86_64) build, signing with hardened runtime, signed DMG, notarization and stapling, Homebrew cask, first public release |
| M7 | Fleet actions on a cluster set: rollout restart, scale, apply YAML and delete, with a per-cluster dry-run preview, one cluster at a time, stopping at the first failure |

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
- **Exec plugin latency.** Some auth plugins take seconds to run. Cache tokens until they expire (`kube` does this)
  and show connection progress in the UI.
- **No `helm` installed.** Covered by an install hint. Bundling the binary is a possible follow-up.
- **AI CLI flags change.** `claude` is young, and its flags can change. Keep the flags in one place, and fail
  closed when the CLI rejects them or its `system/init` check fails.
- **Log lines leave the machine.** Redaction is pattern-based and can miss a secret. The confirmation shows exactly
  what is sent, so the user is the last check.
- **Memory with warm clusters.** Covered by the count, the memory budget and the idle limit. Watch storms on a warm
  cluster cost CPU, but no view redraws for them. The target is under 1% CPU with 3 idle warm clusters.
- **Only one AI CLI.** Codex users get no Analyze button in v1. Support is reconsidered when `codex exec` can turn its
  tools off (ADR-0011).
