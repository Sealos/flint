# Critical user paths

These are the journeys Flint must never break. Each path has a stable ID. Tests name the path they cover (in the test
name or a `// covers UP-03` comment), and every milestone's release review checks that all of that milestone's paths
have tests and pass.

Paths describe what the user does and sees, not how it is built. When behavior changes, update the path in the same
change.

## How paths are tested

The full strategy is in [`TESTING.md`](./TESTING.md).

| Level | Tool | Used for |
|---|---|---|
| **unit** | `cargo test` in `flint-core` / `flint-mcp` | Pure logic: parsing, merging, policy, redaction, validation |
| **cluster** | `cargo test` with `FLINT_TEST_KUBECONFIG` set to the dev cluster | Anything that talks to the API server |
| **mcp** | Drive `flint mcp` over stdio (JSON-RPC) | Agent-facing paths |
| **ui** | GPUI test contexts (`#[gpui::test]`) where the pinned GPUI supports them, otherwise launch with `FLINT_CLUSTER` and take screenshots | What the user sees and clicks |
| **manual** | A person, on a clean Mac or a real cloud cluster | Packaging, Gatekeeper, real exec-plugin auth |

**Fixtures:**
- `scripts/dev-cluster.sh up` creates the kind cluster `flint-dev` with namespace `demo`, the `web` deployment and
  service, the `podinfo` Helm release, and metrics-server.
- Scratch kubeconfigs are hand-written per test (unreachable servers, duplicates, broken YAML).
- Every run sets a scratch `FLINT_CONFIG_DIR`.

**Safety:** paths are only ever exercised against the kind cluster, never a real one.

---

## M0: Clusters and MCP

### UP-01 Launch and see my clusters
- **Given** a kubeconfig with several contexts (several files via `KUBECONFIG`, plus extra paths from settings).
- **When** I open Flint.
- **Then** the sidebar lists every context once, with the first definition winning across files. Long EKS ARNs show a short
  name, with the full context in a tooltip. **No network request is made.**
- Tests: unit (merge, dedupe), ui.

### UP-02 Launch with no usable kubeconfig
- **Given** a missing `~/.kube`, a `KUBECONFIG` pointing at files that don't exist, or a file with broken YAML. (An
  empty `KUBECONFIG` falls back to `~/.kube/config`, as with kubectl.)
- **When** I open Flint.
- **Then** an empty state names the paths that were searched and the value of `KUBECONFIG`. Broken files show their file
  name and the first line of the error, with details in a tooltip. Valid files still load. Nothing crashes.
- Tests: unit, ui.

### UP-03 Connect to a cluster
- **When** I select a cluster.
- **Then** it becomes active, shows "connecting", then "connected" with the server version. The UI stays responsive
  throughout.
- Tests: cluster, ui (`FLINT_CLUSTER=kind-flint-dev`).

### UP-04 Connection fails
- **Given** a context that points at an unreachable server, uses an exec plugin that is missing, or uses a plugin that
  hangs.
- **When** I select it.
- **Then** within the timeout I see an error stating the cause in plain words, and I can retry. Other clusters and the
  rest of the UI keep working.
- Tests: unit (timeouts), cluster (unreachable), ui.

### UP-05 Exec-plugin auth when launched from Finder
- **Given** an EKS, GKE or kubelogin context whose plugin is only on the `PATH` set in my shell rc files.
- **When** I launch Flint from Finder or the Dock and select the cluster.
- **Then** the plugin is found and the cluster connects.
- Tests: unit (PATH parsing, fallback), manual (real cloud cluster).

### UP-06 Kubeconfig changes on disk
- **When** a context is added, removed or re-pointed (for example by `aws eks update-kubeconfig`) while Flint runs.
- **Then** the sidebar updates without a restart. If the active cluster is removed, it is deselected with a notice. A
  file caught half-written does not make clusters flicker away.
- Tests: unit (watcher), cluster.

### UP-07 Expose a cluster to AI
- **When** I turn on Expose to AI for a cluster.
- **Then** a confirmation names the context and its server. Only after I confirm does the row show an "AI" badge, and
  `settings.toml` stores the context name and server URL. Turning it off needs no confirmation. A stray key press
  cannot toggle it.
- Tests: unit (settings write), ui.

### UP-08 An exposed context is re-pointed
- **Given** an exposed context that now points at a different server.
- **Then** Flint shows it as "cluster changed" and not exposed. `flint mcp` treats it as unknown until I expose it
  again.
- Tests: unit (policy), mcp, ui.

### UP-09 Connect an AI agent
- **When** I register Flint, for example with `claude mcp add flint -- flint mcp`, and the agent calls `list_clusters`.
- **Then** only exposed clusters are listed, with reachability. If none are exposed, the result includes a hint to use the
  toggle in Flint. The Flint app does not need to be open.
- Tests: mcp.

### UP-10 Agent inspects a cluster
- **When** the agent lists kinds and resources, gets an object, and reads events and pod logs on an exposed cluster.
- **Then** results are correct and bounded. Long lists say they were truncated and give a continue token. Secret values
  and copies of them in annotations are redacted. `managedFields` is removed. Errors are readable sentences, not
  debug dumps.
- Tests: unit (redaction, truncation), mcp + cluster.

### UP-11 Agent asks for a cluster it may not see
- **When** the agent names a cluster that is unexposed, re-pointed or nonexistent.
- **Then** all three cases return the same "not found" error, and no connection is attempted.
- Tests: unit, mcp.

### UP-12 Settings change outside the app
- **When** `settings.toml` is edited by hand or by another Flint process, or is malformed.
- **Then** the app and `flint mcp` agree on which clusters are exposed. A malformed file shows a banner in the app, and
  `flint mcp` exposes nothing rather than guessing.
- Tests: unit, mcp.

### UP-14 Limit what an agent may read
- **When** I expose a cluster and limit it to some namespaces and kinds in the Expose to AI dialog.
- **Then** the agent sees only those namespaces and kinds. Asking for anything else returns the same "not found" error
  as an unexposed cluster. Cluster-scoped kinds are hidden unless I named them. Clearing the scope exposes everything
  again.
- Tests: unit (policy), mcp + cluster, ui.

### UP-15 Credentials in logs don't reach the agent
- **When** an agent reads pod logs or events that contain a token, a password pair, a private key or a cloud access
  key.
- **Then** each value is replaced with a marker, and the result says how many were replaced. The rest of the line is
  unchanged.
- Tests: unit (redactor patterns), mcp + cluster.

### UP-16 See what an agent read
- **When** an agent has called Flint's MCP tools, and I open a cluster's agent activity.
- **Then** I see each call: when, which tool, what it named, how much came back, and whether it was cut short or
  denied. No returned content is stored. The view updates while the agent works, even though `flint mcp` is a separate
  process.
- Tests: unit (record format, rotation), mcp, ui.

### UP-13 Appearance follows macOS
- **When** macOS switches between light and dark.
- **Then** Flint follows live. (The full themes come in M1.5. In M0, dark mode must at least work.)
- Tests: ui.

---

## M1: Browsing resources

### UP-20 Browse a kind with live updates
- **When** I open Pods (all namespaces) on the active cluster.
- **Then** the list shows the same columns `kubectl get` shows, and changes appear within about a second: scaling,
  deleting, a pod going from Pending to Running. Age keeps counting.
- Tests: cluster, ui.

### UP-21 Find what I'm looking for
- **When** I filter by namespace (one or several), type a text filter, and sort by a column.
- **Then** the list narrows and sorts instantly. The namespace choice is remembered per cluster.
- Tests: unit (filter and sort), ui.

### UP-22 Browse a CRD
- **Given** a CRD with `additionalPrinterColumns`.
- **Then** it appears in the navigation under its API group, and its list shows those columns.
- Tests: cluster.

### UP-23 Inspect an object
- **When** I open an object.
- **Then** I see its metadata, labels, owners, conditions, related events, and read-only YAML with syntax highlighting.
  `managedFields` is hidden.
- Tests: cluster, ui.

### UP-24 Switch clusters
- **When** I switch to another cluster and back.
- **Then** the old cluster's watches stop. Switching back loads fresh data. Log, exec and port-forward sessions keep
  running across the switch.
- Tests: cluster, ui.

### UP-25 Change the watch mode
- **When** I set the watch mode to "On view", globally or for one cluster.
- **Then** only open views are watched, and the setting persists.
- Tests: unit, cluster.

### UP-26 Large cluster stays smooth
- **Given** 10,000 pods and a burst of updates, such as scaling down 200 pods.
- **Then** scrolling stays smooth, the UI never freezes, and memory stays within the budget recorded in
  [Sealos/flint#8](https://github.com/Sealos/flint/issues/8).
- Tests: cluster (generated load), with numbers recorded.

### UP-27 Recover from a broken watch
- **When** a watch expires (410), the API server restarts, or the network drops.
- **Then** the list re-syncs on its own with a visible "reconnecting" state. It never shows stale data as if it were
  current.
- Tests: unit (410 handling), cluster (restart the kind control plane).

### UP-28 An API that doesn't support Table
- **Then** the list falls back to name, namespace and age columns instead of failing. metrics.k8s.io is polled, not
  watched.
- Tests: unit, cluster.

---

## M1.5: Look and keyboard

### UP-30 Command palette
- **When** I press Cmd-Shift-P and type part of an action name.
- **Then** matching actions appear with their shortcuts, and Enter runs the selected one.
- Tests: ui.

### UP-31 Jump to a resource
- **When** I press Cmd-P and type part of a kind or object name.
- **Then** results come from memory instantly, even with tens of thousands of objects, and Enter opens the list or the
  object.
- Tests: unit (fuzzy matching), ui.

### UP-32 Choose light or dark
- **When** I set the appearance to Light, Dark or System.
- **Then** the theme changes immediately and persists. Orange marks only selection, focus and primary actions.
  Status colors stay readable.
- Tests: ui (screenshots of both themes).

### UP-33 Use the app from the keyboard
- **Then** I can move through clusters and lists with the arrow keys, open with Enter, and quit with Cmd-Q.
- Tests: ui.

---

## M2: Logs and exec

### UP-40 Read pod logs
- **When** I open logs for a pod.
- **Then** I can pick a container, follow new lines, see the previous container's logs, search, and pause following.
  Memory stays bounded on chatty pods.
- Tests: cluster, ui.

### UP-41 Shell into a container
- **When** I open a shell on a pod.
- **Then** I get an interactive terminal that resizes with the window and supports copy and paste. Images without `bash`
  fall back to `sh`.
- Tests: cluster, ui.

### UP-42 Sessions outlive navigation
- **Then** log and shell tabs keep running when I change views or clusters, and end only when I close them.
- Tests: ui.

---

## M3: Editing and actions

### UP-50 Edit with inline validation
- **When** I edit an object's YAML.
- **Then** syntax errors are underlined as I type (tree-sitter), and so are schema errors from the cluster's OpenAPI
  schema: unknown fields, wrong types, missing required fields.
- Tests: unit (validator), cluster (the dev cluster's schema), ui.

### UP-51 Review and apply
- **When** I press Review, then Apply.
- **Then** I see a diff from the server dry-run, then the change is applied. Webhook or policy rejections are shown
  before anything is applied.
- Tests: cluster.

### UP-52 Someone else changed it
- **Given** the object changed after I opened it.
- **When** I apply.
- **Then** Flint refuses to overwrite, and offers to reload and diff again.
- Tests: cluster.

### UP-53 Destructive actions are deliberate
- **When** I delete, scale, rollout-restart, cordon or drain.
- **Then** a confirmation names the cluster, namespace and object. Drain reports pods blocked by a PodDisruptionBudget
  instead of hanging.
- Tests: cluster, ui.

---

## M4: Curated views and metrics

### UP-60 Understand a workload at a glance
- **When** I open a pod, deployment or node.
- **Then** I see container states and restarts, rollout progress, endpoints, and node capacity and allocation.
- Tests: cluster, ui.

### UP-61 See resource usage
- **Then** pod and node lists show CPU and memory from metrics-server, refreshed about every 15 s. If metrics-server is
  missing, the columns are hidden and a hint is shown.
- Tests: cluster (with and without metrics-server).

---

## M5: Port-forward and Helm

### UP-70 Port-forward to a pod or service
- **When** I forward a port and open it.
- **Then** the local address works in the browser. The forwards panel lists it, and Stop ends it. A local port that is
  already in use, or a pod that dies, gives a clear error.
- Tests: cluster, ui.

### UP-71 Inspect Helm releases
- **Then** I can see all releases with their status, history, values and manifest.
- Tests: cluster (`podinfo`).

### UP-72 Change a Helm release
- **When** I roll back, upgrade with edited values, or uninstall.
- **Then** each action asks for confirmation and the result is visible. If `helm` is not installed, I see an install
  hint instead of a failure. Works with Helm v3 and v4.
- Tests: cluster.

---

## M6: Install

### UP-80 Install from the DMG
- **When** I install from the DMG on a clean Mac.
- **Then** the app opens without Gatekeeper warnings, on both Apple Silicon and Intel.
- Tests: manual.

### UP-81 Install with Homebrew
- **When** I run `brew install --cask flint`.
- **Then** the app is installed and `flint mcp` is on my `PATH`.
- Tests: manual, plus a CI smoke test.

---

## Always true

### UP-90 Flint never writes kubeconfig files
- Tests: unit (no write paths), review.

### UP-91 Nothing reaches a cluster unless I selected it or exposed it to AI
- Tests: unit, ui (UP-01).

### UP-92 Destructive actions always show the cluster and namespace
- Tests: ui, review.
