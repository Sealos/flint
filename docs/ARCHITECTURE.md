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
- Multiple clusters, loaded from the user's kubeconfig files, shown in one window with a cluster sidebar.
- Browse every resource kind, including CRDs, with live updates ([ADR-0003](./adr/0003-generic-resource-views.md)).
- Pod logs: follow, search, pick a container, show previous logs.
- Pod exec terminal.
- Edit YAML in the app. Syntax and schema errors show inline as you type, and a server-side dry-run with a diff runs
  before apply ([ADR-0006](./adr/0006-yaml-validation-by-dry-run.md)).
- Common actions: delete, scale, rollout restart, cordon and drain.
- Current CPU and memory from metrics-server ([ADR-0005](./adr/0005-metrics-server-only.md)).
- Port-forwarding, for the current session only.
- Helm releases: list, history, values, upgrade, rollback, uninstall ([ADR-0004](./adr/0004-helm-via-cli.md)).
- A read-only MCP server (`flint mcp`) so AI agents can inspect clusters the user has opted in
  ([ADR-0009](./adr/0009-read-only-mcp-server.md)).
- A dense, keyboard-first look in the style of Zed, with orange accents, light and dark themes, a command palette and
  a resource switcher.
- Open source under Apache-2.0.

### v1 non-goals

- Linux and Windows builds.
- Prometheus charts or any metric history.
- Local terminal tabs and node shells (shells into a node through a privileged pod).
- An extension or plugin API ([ADR-0007](./adr/0007-no-extension-api-in-v1.md)).
- YAML schema autocomplete.
- Saved port-forwards that come back after a restart.
- An in-app auto-updater.
- MCP write tools, an MCP server over HTTP, and a built-in AI chat panel.

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
  flint-mcp/    The MCP server (stdio): tool definitions, cluster opt-in checks, redaction. Depends on flint-core.
  flint-app/    The `flint` binary: windows, views, the terminal widget, settings UI, the bridge to core.
                `flint mcp` runs flint-mcp instead of starting the UI.
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
- One window shows a cluster sidebar. Exactly one cluster is **active**, meaning the one in view. Clusters that are not
  active keep their client and discovery cache but drop their watches ([ADR-0008](./adr/0008-watch-everything-on-active-cluster.md)).
  Sessions the user started (log streams, exec terminals, port-forwards) keep running when the user switches to
  another cluster. They end only when the user closes them or quits Flint.

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

When the user switches to another cluster, the old cluster's watches stop in both modes. Switching back starts them
again with a fresh list.

## Features

### Logs

`Api::<Pod>::log_stream` with `LogParams` (`follow`, `tail_lines`, `since_seconds`/`since_time`, `previous`,
`container`). Lines go into a
ring buffer with a size limit (a setting, default 100k lines) so long-running streams cannot grow memory without
bound. Search runs over the buffer.

### Exec terminal

`Api::<Pod>::exec` with a TTY, over the `ws` feature. Bytes go to `alacritty_terminal`, and a GPUI terminal view
draws the grid, as Zed does. Window size changes are sent to the pod as terminal resize messages. Each exec session
opens in a tab in the bottom dock.

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
UI. Destructive actions show the cluster and namespace in the confirmation. Kubernetes has no drain API, so drain
works like `kubectl drain`: cordon the node, then evict its pods through the Eviction API. Pods blocked by a
PodDisruptionBudget are reported back to the user instead of being retried forever.

### Metrics

Flint polls `metrics.k8s.io/v1beta1` `PodMetrics` and `NodeMetrics` about every 15 s, and only for views that are
open. If metrics-server is not installed, the metric columns are hidden and a hint is shown
([ADR-0005](./adr/0005-metrics-server-only.md)).

### Port-forward

`Api::<Pod>::portforward` behind a local `TcpListener`. When the target is a Service, Flint picks a ready pod behind
it. A panel lists the active forwards with stop buttons. Forwards end when the app quits.

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
tool layer and the policy (opt-in, redaction, limits) live in `flint-mcp`.

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
  switch cluster, change namespace, toggle watch mode, or toggle Expose to AI.
- **Resource switcher (Cmd-P):** fuzzy search over the kinds and objects of the active cluster, jumping straight to
  a list or a detail view. It searches the in-memory stores, so it never makes network calls.

## Settings and state

- `settings.toml`: `appearance`, `ai_exposed_clusters`, extra kubeconfig paths, watch mode (global plus per-cluster), log buffer size, a `path`
  override, and a `helm` binary override.
- `state.toml`: window layout (the widths of the cluster column, navigation and detail pane, which the user
  drags), cluster order and pins in the sidebar, the last view open in each cluster, and the namespace
  filter of each cluster.
- Flint never writes to kubeconfig files.

## Milestones

| Milestone | Delivers |
|---|---|
| M0 | Cargo workspace, GPUI window, kubeconfig loading, cluster sidebar, connect and health check, PATH resolution, read-only `flint mcp` server with Expose to AI toggle |
| M1 | Discovery-driven navigation, Table lists (spike first), watch manager with both modes, generic detail and YAML view, namespace filter |
| T | Test foundation: unit tests for every module, headless view tests, automation channel (test builds only), `flint-e2e` harness, recorded fake API server, Linux kind e2e, e2e screenshots ([TESTING.md](./TESTING.md)) |
| M1.5 | Look and keyboard: Flint Light and Flint Dark themes with orange accents that follow macOS, command palette, resource switcher |
| M2 | Log viewer, exec terminal |
| M3 | YAML editing with inline syntax and schema validation, dry-run and diff, delete/scale/restart/cordon/drain |
| M4 | Curated views for core kinds, metrics-server columns and node/pod usage |
| M5 | Port-forward panel, Helm releases |
| M6 | Universal (arm64 + x86_64) build, signing with hardened runtime, signed DMG, notarization and stapling, Homebrew cask, first public release |

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
