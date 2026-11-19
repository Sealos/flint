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
  service, the `podinfo` Helm release, metrics-server, and some variety (`scripts/cluster-fixtures.sh`): failing pods,
  a Job and a CronJob, a StatefulSet with a PVC, an Ingress, and the CRD `gadgets.fixtures.flint.dev`.
- Scratch kubeconfigs are hand-written per test (unreachable servers, duplicates, broken YAML).
- The fake API server (`flint_e2e::fake_api`, see [`TESTING.md`](./TESTING.md#fake-api-server)) replays responses
  recorded from that cluster, with scripted watches and injected faults, where kind is not available.
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
- Kinds are named as `kubectl get` takes them, ignoring case: Kind, plural, singular or short name (`Deployment`,
  `deployments`, `deployment`, `deploy`), optionally with `.group`. A Kind, plural or singular match beats a short
  name; then the core group beats built-in groups (`apps`, `*.k8s.io`), which beat CRDs. A name still matching
  kinds in several groups is an error that lists them.
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
- **When** I switch to another cluster and back, and the old cluster is not kept warm (`warm_clusters = 1`, or it went
  cold).
- **Then** the old cluster's watches stop. Switching back loads fresh data. Log, exec and port-forward sessions keep
  running across the switch. (Before M5.5 every cluster behaves this way. Warm clusters are UP-78.)
- Tests: cluster, ui (from M5.5 with `warm_clusters = 1`).

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
  fall back to `sh`. When the command exits, its exit code is shown and the output stays readable until I close the
  tab.
- Tests: unit (terminal model, session logic), cluster, ui.

### UP-42 Sessions outlive navigation
- **Then** log and shell tabs keep running when I change views or clusters, and end only when I close them.
- Tests: ui.

### UP-43 Spot errors in a log stream
- **When** I follow a pod's logs and turn timestamps on and off.
- **Then** error lines are marked in the error color and warning lines in the warning color, in both themes.
  Matching ignores case and works on whole words: `ERROR`, `FATAL`, `panic`, and words ending in `Exception` or
  `Error` are errors, `WARN` and `WARNING` are warnings. Timestamps appear and disappear without reloading the
  stream. Searching keeps the highlighting.
- Tests: unit (pattern matching), cluster, ui.

### UP-44 Select and copy log lines
- **When** I select a range of lines, with the mouse or Shift and the arrow keys, and press Cmd-C.
- **Then** exactly those lines are copied as they appear, with timestamps only if they are shown, and without
  redaction. Following pauses while I select.
- Tests: ui.

### UP-45 Follow a workload's logs
- **When** I open logs for a Deployment, StatefulSet, DaemonSet, ReplicaSet or Job, or for a label selector.
- **Then** one tab shows the lines of every pod and container merged in arrival order, each prefixed with its pod and
  container (the prefix can be hidden). Pods that start, restart or replace deleted ones are followed without
  reopening the tab. A pod whose stream fails does not stop the others, and the tab shows how many streams are
  live, failed or over the limit. Search, severity, timestamps and selection work as for one pod.
- Tests: unit, cluster, ui.

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

### UP-54 Protected clusters are read-only
- **When** I connect to a remote cluster I have not trusted, and try to edit, delete, scale, restart, drain, exec or
  port-forward.
- **Then** the action is disabled and says the cluster is protected, with an Unlock button in place. Browsing, logs and
  details work as usual. A cluster on my machine (kind, minikube, Docker Desktop, OrbStack) starts unlocked.
- Tests: unit (core refuses writes on protected clusters), ui, e2e.

### UP-55 Unlock or trust a cluster
- **When** I unlock a protected cluster, or mark it trusted.
- **Then** an unlock asks once, naming the cluster and its server, and lasts until Flint quits; the sidebar shows an open
  lock, and Lock Again relocks it. Trust lasts across restarts and is pinned to the server URL: if the context is
  re-pointed, the cluster is protected again.
- Tests: unit (settings, pinning), ui.

## M4: Curated views, metrics and the resource diagram

### UP-60 Understand a workload at a glance
- **When** I open a pod, deployment or node.
- **Then** I see container states and restarts, rollout progress, endpoints, and node capacity and allocation.
- Tests: cluster, ui.

### UP-61 See resource usage
- **Then** pod and node lists show CPU and memory from metrics-server, refreshed about every 15 s. If metrics-server is
  missing, the columns are hidden and a hint is shown.
- Tests: cluster (with and without metrics-server).

### UP-62 See how a namespace's workloads fit together
- **When** I open the diagram for the `demo` namespace.
- **Then** I see each Deployment around its ReplicaSets and their Pods, StatefulSets and DaemonSets around their
  Pods, and CronJob, Job and Pods on three levels. Each node has its status color. Clicking a node opens its detail
  view. No API request is made beyond the watches the diagram needs. This holds in both watch modes.
- Tests: unit (tree from owner uids), cluster (mode All and On view), ui.

### UP-63 The diagram follows the cluster
- **When** I scale or roll out a deployment while the diagram is open.
- **Then** pods and ReplicaSets appear, change color and disappear within about a second. Objects whose owner is not
  in the namespace, or that have no owner, are shown on their own, not hidden.
- Tests: cluster, ui.

### UP-64 A large namespace stays readable
- **Given** a namespace with hundreds of pods, old empty ReplicaSets and many finished Jobs.
- **Then** empty ReplicaSets and finished Jobs start collapsed, a group of more than about 50 pods shows a count,
  and past the node limit the rest is one summary node. The diagram opens and scrolls without freezing the UI.
- Tests: unit (collapsing, limits), cluster (generated load), ui.

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

### UP-73 Choose the local port
- **When** I forward a pod or service port and type a local port, or leave it empty.
- **Then** Flint uses my port, or picks a free one and shows it. A port in use is refused before anything starts.
  I can forward several ports of the same target at once. Forwards listen on `127.0.0.1` only.
- Tests: unit (port choice), cluster, ui.

### UP-74 Open a forwarded HTTP port
- **Given** a forward to a port named `http`, `https` or `web`, or with `appProtocol` `http` or `https`.
- **Then** the forwards panel shows its target, local and remote port and state, and an Open in browser action that
  opens `http://localhost:<port>`, or `https://` for an `https` port. An unnamed port offers Open in browser from
  its context menu. A forward to a service survives the loss of the pod behind it if another pod is ready.
- Tests: cluster, ui.

---

## M5.5: Multi-cluster and AI

M5.5 was added after M5 and M6 were numbered, so it shares the 7x block with M5 and uses UP-75 to UP-79.

### UP-75 Analyze log lines with Claude Code
- **Given** `claude` on my login-shell `PATH`.
- **When** I select log lines and press Analyze with Claude (or its palette action), tick "Allow log analysis on
  `<context>` (`<server>`)" the first time, and confirm.
- **Then** the answer streams into a side panel, below what was sent. Stop ends the process and anything it started.
  Nothing in the answer runs, opens or changes anything. The opt-in is checked against the log's own cluster, even
  if I have switched to another one since. A re-pointed context counts as not allowed. If Claude Code's session
  report shows any tool, MCP server or plugin, nothing is sent.
- Tests: unit (prompt, allow policy, process environment), ui (with a fake `claude` on `PATH`).

### UP-76 See exactly what leaves my Mac
- **When** the confirmation opens.
- **Then** it shows the full prompt and the lines as they will be sent, with credential shapes, vendor tokens,
  private keys and the values of Secrets, env and ConfigMaps the pod references already replaced. It names the CLI
  and its path. Lines past 1,000, past 4 KiB each or past 256 KiB in total are cut, and it says so. If Flint could
  not read the pod's Secrets or ConfigMaps, it says those values were not checked. With "Always for this cluster",
  later sends skip the sheet but are still redacted, and the panel shows what was sent. "Always" resets if the
  binary path changes. The fake `claude` receives byte for byte what was shown.
- Tests: unit (redaction patterns and encodings, limits), ui (fake `claude`).

### UP-77 Claude Code not installed
- **Given** no `claude` on the resolved `PATH` (relative and empty entries ignored), nor in settings, nor in a
  supported version. Having only `codex` installed counts as not installed in v1.
- **Then** the log viewer shows no Analyze with Claude button, the palette has no Analyze action, and there is no
  error.
- Tests: unit (CLI lookup), ui.

### UP-78 Switch between recent clusters instantly
- **When** I use clusters A, B and C, then switch back to A.
- **Then** A's lists show current data at once, with no loading state (in mode All, Events, Leases and
  EndpointSlices reload, because warm clusters drop them). The sidebar marks A as active and B and C as
  warm. Changes made on a warm cluster while I was away are already there. Relaunching Flint connects nothing
  until I select a cluster.
- Tests: unit (warm set), cluster, ui.

### UP-79 Warm clusters stay bounded and quiet
- **When** I use more clusters than `warm_clusters`, the background clusters go over `warm_memory_mib`, a warm
  cluster sits unused for 30 minutes, the Mac sleeps, or a warm cluster's credential expires.
- **Then** the least recently used (or the affected) cluster goes cold, and the sidebar shows it. An expired
  credential shows a "sign-in needed" badge, and no exec plugin runs until I click the cluster. Background errors
  show only in the sidebar, never as toasts. Disconnect makes a warm cluster cold by hand. With 3 idle warm
  clusters, Flint uses under 1% CPU.
- Tests: unit (eviction, budget, idle limit), cluster, manual (exec-plugin cluster).

---

### UP-82 Combine clusters into one view
- **When** I Cmd-click two to five clusters in the sidebar, open Pods, and sort by Status.
- **Then** one list shows the pods of every selected cluster with a Cluster column (and each cluster's colour tag), the
  crashing ones first. The group-by-cluster toggle shows one section per cluster instead. Opening a row shows its
  detail from its own cluster.
- Tests: unit (merging, union columns), ui, e2e (two kind clusters).

### UP-83 Save and reopen a cluster set
- **When** I save a selection as a named set, quit, and open the set again.
- **Then** the same clusters open combined. If one member's context now points to a different server, it is left out
  and the set marks it changed.
- Tests: unit (settings, pinning), ui.

### UP-84 Kinds and columns that differ between clusters
- **When** a set's clusters run different Kubernetes versions, or a CRD exists on only some of them.
- **Then** the resource tree lists the CRD with a `2/3` badge, its combined list names the cluster that lacks it, and
  columns that only some clusters return are shown with blank cells for the others. One unreachable member shows a
  note and doesn't block the others' rows.
- Tests: unit, e2e.

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

## M7: Fleet actions (after 1.0)

### UP-100 Restart a workload on several clusters
- **When** I select a Deployment in a combined view and choose Rollout Restart on 3 clusters.
- **Then** a preview lists each target cluster with a dry-run result and a checkbox. The restart runs one cluster at a
  time and stops at the first failure, saying which clusters were done and which were not. Locked clusters in the list
  need their name typed first.
- Tests: unit (sequencing, stop on failure), e2e.

### UP-101 Apply one manifest to several clusters
- **When** I apply a YAML manifest to a cluster set.
- **Then** I see a server-side dry-run diff per cluster before anything changes, can drop clusters from the target list,
  and the apply runs one cluster at a time as above. Scale and delete follow the same flow.
- Tests: unit, e2e.

## Always true

### UP-90 Flint never writes kubeconfig files
- Tests: unit (no write paths), review.

### UP-91 Nothing reaches a cluster unless I selected it or exposed it to AI
- Tests: unit, ui (UP-01).

### UP-92 Destructive actions always show the cluster and namespace
- Tests: ui, review.

### UP-93 Nothing is sent to an AI tool without my action
- Log lines reach a CLI only after I click and confirm, or after I chose "Always for this cluster" for that cluster.
  Flint never sends on its own.
- Tests: unit, review.
