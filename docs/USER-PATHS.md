# Critical user paths

These are the journeys Flint must never break. Each path has a stable ID. Tests name the path they cover (in the test
name or a `// covers UP-03` comment), and every milestone's release review checks that all of that milestone's paths
have tests and pass.

Paths describe what the user does and sees, not how it is built. When behavior changes, update the path in the same
change.

A path marked **Status: planned** is not built yet. It names the issue and milestone that track it. It does not gate
an earlier release, and gets no tests until it ships. Its tests must pass in the change that removes the marker.

A path marked **Status: not planned** is kept for reference only. Nothing tracks it and it gets no tests.

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
- A cluster can have a custom name (right-click it, or Rename Cluster… in the palette), kept in `settings.toml` by context
  name; kubeconfig is never written (UP-90). The row's second line still starts with the real name, the tooltip has the
  full context and server, and an unlock or trust confirmation names both. A name that reads like another context's name
  or short name, or another custom name, is not used. Shell tab chips, MCP, `FLINT_CLUSTER`, exposure and trust keep the
  context name.
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
  rest of the UI keep working. A plugin that hangs is killed at the timeout, with anything it started; retrying while
  an attempt runs does not start a second plugin.
- Tests: unit (timeouts, hanging plugin processes), cluster (unreachable), ui.

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
  and copies of them in annotations are redacted. `managedFields` is removed. `get_resource` names the Argo CD or Flux
  object that manages the object, on a `# managed_by=` first line. Errors are readable sentences, not
  debug dumps. A name, pod, namespace or container that is not a single valid path segment (`a/b`, `..`, `%2F`) is
  refused before any request, with the same not-found error as a missing object.
- An argument a tool does not have is refused with an error naming it and the accepted arguments, so a filter Flint
  cannot apply never returns unfiltered data. Standard request metadata (`_meta`) is not an argument.
  The activity log records a call refused for an unsupported argument by tool and failure only, with no argument
  values or cluster, so it shows on no cluster's Activity tab.
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
- **Then** the agent sees only those namespaces and kinds. Asking for anything else returns one "outside the scope"
  error, byte for byte the same whether what it named exists or not. A cluster that is not exposed stays "not found".
  Cluster-scoped kinds are hidden unless I named them. Clearing the scope exposes everything
  again.
- **And** kinds written by hand resolve as `kubectl get` names them. A kind that matches nothing or several grants
  nothing. `list_clusters` reports it in `unmatched_kinds`, and a namespace that does not exist in
  `unmatched_namespaces`. When it could not check everything (too many namespaces, a refused or slow lookup, part of
  API discovery failing), it says so in `check_incomplete`, without naming anything outside the scope.
- Tests: unit (policy), mcp (fake API server), ui.

### UP-15 Credentials in logs don't reach the agent
- **When** an agent reads pod logs or events that contain a token, a password pair (also a passphrase of several words
  after `password: `), a cookie, a private key, a registry `auth` value, a database DSN or a cloud access key,
  through `get_pod_logs`, `get_events`, `list_resources` or `get_resource` (messages and message lists,
  annotations, and container `args` and `command`).
- **Then** each value is replaced with a marker such as `[REDACTED:password]`, and the result says how many were
  replaced. The rest of the line is unchanged: keys, URL users and hosts, header schemes. The server's instructions
  tell the agent about the markers and the count.
- **And** a log line the 256 KiB cap would cut in the middle is left out whole, so no value is returned without its
  label; when the last line alone is longer than the cap, the log is empty and a `note` says why. ConfigMap data
  is shown as the cluster returns it.
- **And** Flint's own log viewer and events still show the text as written.
- Tests: unit (redactor patterns, ordinary lines untouched, throughput), mcp, core.

### UP-16 See what an agent read
- **When** an agent has called Flint's MCP tools, and I open a cluster's agent activity.
- **Then** I see each call: when, which tool, what it named, how much came back, and whether it was cut short or
  denied. Listing the clusters counts as a call on each cluster it probed. No returned content is stored. The view
  updates while the agent works, even though `flint mcp` is a separate process. The cluster's "AI" badge opens it,
  with Edit AI Scope there.
- Tests: unit (record format, rotation), core (live reload), mcp, ui.

### UP-13 Appearance follows macOS
- **When** macOS switches between light and dark.
- **Then** Flint follows live. (The full themes come in M1.5. In M0, dark mode must at least work.)
- Tests: ui.

---

### UP-17 Agent asks what's wrong
- **Status:** not planned. The MCP server stays at its six read-only tools ([ADR-0009](./adr/0009-read-only-mcp-server.md)).
- **When** an agent calls `get_cluster_summary` for an exposed cluster.
- **Then** it gets the dashboard's problem counts, usage and recent warnings, limited to the cluster's exposure scope
  and redacted, and the call appears in the activity log.

## M1: Browsing resources

### UP-20 Browse a kind with live updates
- **When** I open Pods (all namespaces) on the active cluster.
- **Then** the list shows the same columns `kubectl get` shows, and changes appear within about a second: scaling,
  deleting, a pod going from Pending to Running. Age keeps counting.
- Tests: cluster, ui.

### UP-21 Find what I'm looking for
- **When** I filter by namespace (one or several), type a text filter, and sort by a column.
- **Then** the list narrows and sorts instantly. The namespace choice is remembered per cluster, and the sort per
  cluster and kind, also across restarts. A remembered sort whose column is gone (a CRD's printer columns changed)
  falls back to the kind's default order.
- Tests: unit (filter, sort, remembered sort), ui.

### UP-22 Browse a CRD
- **Given** a CRD with `additionalPrinterColumns`.
- **Then** it appears in the navigation under its API group, and its list shows those columns.
- Tests: cluster.

### UP-23 Inspect an object
- **When** I open an object.
- **Then** I see its metadata, labels, owners, conditions, related events, and read-only YAML with syntax highlighting.
  `managedFields` is hidden. An object Argo CD or Flux manages says so in the header, naming the application or
  Kustomization or HelmRelease, read from its labels and annotations. A Secret tracked by Argo CD shows only the tool.
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
- **When** I press Cmd-Shift-P (Ctrl-Shift-P on Linux) and type part of an action name.
- **Then** only actions that can run now are listed, each with its category and shortcut, the ones I ran recently
  first; typing filters them as I type, loosely ("opl" finds Open Logs), and the closest match comes first. Quit and
  Close Window come last and are not selected for me, unless what I typed clearly names them: at least three
  characters that start the name or a word in it ("quit", "close w"). A query that only starts a later word ("flint")
  lists every other match first. When a short or scattered query lists only them,
  Enter does nothing until an arrow key selects one. The arrows
  or Ctrl-N and Ctrl-P move the selection, Enter (or a click) runs it where focus was before the palette opened, and
  Esc closes the palette and puts focus back. An action that needs something asks for it next: a cluster, a namespace
  (one I type is fine, and it comes after the listed matches), a kind, a column or a container from a list, or typed text such
  as a label selector; the heading shows what I picked so far, and Esc or Backspace in the empty field goes back a
  step. If it cannot run, the palette says why and stays open. Switching to another app and back keeps it open. Every menu item is in the palette, so on Linux, where there is no menu bar, it reaches them all.
- Tests: unit (matching and ranking), ui.

### UP-31 Jump to a resource
- **When** I press Cmd-P (Ctrl-P on Linux) and type part of a kind or object name, or a kind and a name ("pods web").
- **Then** the kinds the active cluster serves and the objects Flint already holds in memory are listed, each row with
  its kind, its namespace/name and its cluster; results come from memory instantly, even with tens of thousands of
  objects, and searching never makes a request to the cluster. The arrows move the selection; Enter on a kind opens its
  list, and on an object opens its kind's list with the object selected and its details shown; when the namespace
  filter hides it, the filter is left as it is and the list says the object is filtered out; when it no longer
  exists, the list says so. Event objects rank after all other matches. Within each of those two groups the exact
  name comes first ("web" finds the Deployment web before web-cfg), and an object matching only by scattered letters
  is left out when another in its group contains the query as a run. Each kind says
  how many of its objects are in memory, and in On view mode the switcher says only watched kinds list their
  objects. Esc closes it and puts focus back. Cmd-Shift-P swaps it for the command
  palette and, on macOS, Cmd-P back; on Linux, Ctrl-P there moves the selection up. With nothing in memory yet it lists the kinds; with no cluster active it says to select one.
- Tests: unit (fuzzy matching, 50,000 objects per keystroke, reading the stores without a request), ui.

### UP-32 Choose light or dark
- **When** I set the appearance to Light, Dark or System.
- **Then** the theme changes immediately and persists. Orange marks only selection, focus and primary actions.
  Status colors stay readable.
- Tests: ui (screenshots of both themes).

### UP-33 Use the app from the keyboard
- **Then** I can move through clusters and lists with the arrow keys, open with Enter, and quit with Cmd-Q. Tab and
  Shift-Tab move focus between the panes: the cluster sidebar, the navigation, the list, the detail pane and the
  dock's tab (a text field and a shell keep Tab for themselves). The header's namespace and watch-mode buttons are not
  Tab stops; they work by mouse and from the command palette. The namespace picker takes focus as it opens, Tab,
  Shift-Tab and the arrows go round its controls, and Esc closes it and returns focus to its button; a dialog closes
  it. The pane with focus shows its cursor with the focus ring. In the sidebar the arrows move between clusters and Enter connects to the one under the cursor. In the
  navigation the arrows move over sections and kinds, Enter opens a kind (its list takes focus) or opens and closes a
  section, Right opens a section and Left closes it (from a kind, its own section). In a list, Cmd-F moves to its text
  filter, and Esc, Enter or Down there moves back to the rows, keeping the filter. With nothing selected, Down also
  selects the first row and opens it. Enter on the rows opens the selected row; in a narrow window it brings the
  detail back, and Cmd-Shift-B (Ctrl-Shift-B on Linux) returns to the list. Cmd-[ goes back to the kind and
  object shown before, on the active cluster, never to the place already shown.
- Tests: ui.

---

## M2: Logs and exec

### UP-40 Read pod logs
- **When** I open logs for a pod (Logs in its detail pane or its row's context menu, or Cmd-L; UP-47).
- **Then** a tab opens in the bottom dock (or the open one for that container is shown). I can pick a container, follow
  new lines, see the previous container's logs, search with highlighted matches and move between them, and pause
  following, with the number of lines held back shown. Scrolling up stops following; scrolling back to the bottom (or
  Jump to Newest Line) follows again; while it does not follow, the toolbar says "Not following" beside Jump. The tab
  says whether it is connecting, streaming, reconnecting (and which attempt), waiting for a pod that isn't scheduled
  yet or a container that hasn't started (and why, such as `ImagePullBackOff`), waiting for a crash-looping container
  to restart (back-off) or ended, and why in plain words (no permission, can't reach the API server) with the
  server's text under them; an unreachable node is told apart from an unreachable API server. Once a stream has
  ended there is nothing to pause. A previous instance whose log cannot be read says so (the container may have
  been removed), with Reconnect, instead of showing the kubelet's answer as a line. The toolbar
  stays one row in a narrow dock (the toggles and Save move into a "⋯" menu), and one notice line at a time shows
  under it. Esc in the search field clears the search, then returns to the lines; Esc in the lines clears the
  selection and never closes the detail pane. Memory stays bounded on chatty pods.
- Tests: cluster, ui.

### UP-41 Shell into a container
- **When** I open a shell on a pod (Shell in its detail pane or its row's context menu, or Shift-Cmd-E), or on a
  workload (in its first ready pod; UP-47). When the pod has several containers, the button and the menu let me pick
  one, the default marked with Shift-Cmd-E beside it; a container that is not running is listed but disabled (and
  Shift-Cmd-E says so when it is the default), and a native sidecar is offered like any app container. Shift-Cmd-E
  opens the default container at once.
- **Then** a tab opens in the bottom dock with an interactive terminal that resizes with the dock and the window.
  Colors, bold, italic, underline, inverse and wide characters show as in other terminals, in both themes, and
  full-screen programs (`top`, `vi`, `less`) work. Tab, Esc, the arrows and Ctrl keys go to the shell; Cmd-C and
  Cmd-V (Ctrl-Shift-C and Ctrl-Shift-V on Linux) copy the mouse selection and paste. The wheel scrolls back through
  the output. Images without `bash` fall back to `sh`. The tab says whether the shell is connecting, running, exited
  (with its exit code) or failed (and why, once, in plain words such as "You don't have permission to open a shell
  in this pod." or "Flint couldn't sign in to the cluster.", then what the server said in full), and the output
  stays readable and copyable until I close the tab; Restart shell (Cmd-R; Ctrl+Shift+R on Linux) starts a new shell
  in a tab beside it. The tab and its toolbar name the cluster the shell runs on, and two clusters never read the
  same. Pasting several lines into a shell that would run each one as it arrives asks first, and a huge clipboard is
  refused. If the context is re-pointed at another API server, the shell ends and says why.
- Tests: unit (terminal model, session logic, key encoding, colors, grid size), cluster, ui.

### UP-42 Sessions outlive navigation
- **Then** log and shell tabs keep running when I change views or clusters, and end only when I close them (the tab's
  close button, or Cmd-W wherever focus is while the dock shows a tab; with no tab shown, Cmd-W closes the window,
  and Shift-Cmd-W always does). A closed tab frees everything it held. Closing the window (Shift-Cmd-W, its close
  button, or Cmd-W with no tab shown) or quitting (Cmd-Q) while a shell runs or a log tab follows asks first, naming
  how many sessions end. Enter and Escape cancel; confirming needs the Quit or Close Window button (a click, or Space
  when it has focus). With none running, nothing is asked. Cmd-J shows and hides the
  dock, and its height is remembered. Every tab names its cluster beside its title, and two clusters never read the
  same. With many tabs, the tab I open or show stays in view, also when the window is resized, and the tab list
  (Shift-Cmd-J, or the button at the strip's end) shows every tab with its cluster and status. Opening the same logs
  or shell again shows its tab while it runs; once it ended or failed, a fresh tab opens beside it, and Reconnect
  (logs) or Restart shell starts it again in a new tab beside it, keeping the ended one's output. An empty dock says
  how to open logs or a shell. Log tabs share one memory budget (`log_total_mib`): over it, the tabs I looked at least
  recently drop their oldest lines first, never the one shown, keep streaming, and say "Older lines dropped to save
  memory". An ended tab can give back all its lines, and then says "All lines dropped to save memory."
- Tests: unit, ui.

### UP-43 Spot errors in a log stream
- **When** I follow a pod's logs and turn timestamps on and off.
- **Then** error lines are marked in the error color and warning lines in the warning color, in both themes.
  Matching ignores case and works on whole words: `ERROR`, `FATAL`, `panic`, and words ending in `Exception` or
  `Error` are errors, `WARN` and `WARNING` are warnings. Timestamps appear and disappear without reloading the
  stream; they show muted, in local time to the millisecond (the toggle's tooltip names the zone), all one width,
  with the full timestamp on hover and in copies and saves, and a dated separator where a day begins. Wrapped lines hang past the timestamp. Searching keeps the highlighting, and the current match
  stands out from the others.
- Tests: unit (pattern matching), cluster, ui.

### UP-44 Select and copy log lines
- **When** I select a range of lines, with the mouse or Shift and the arrow keys, and press Cmd-C.
- **Then** exactly those lines are copied as they appear, with timestamps only if they are shown, and without
  redaction. Following pauses while I select.
- Tests: ui.

### UP-45 Follow a workload's logs
- **When** I open logs for a Deployment, StatefulSet, DaemonSet, ReplicaSet or Job (Logs on its row or in its detail
  pane, or Cmd-L), or for a label selector (Open Logs by Label Selector in the palette, which asks for the namespace
  and then the selector).
- **Then** one tab, titled with the kind and name, shows the lines of every pod and container merged in arrival order, each prefixed with its pod (the
  part of its name that tells it from the others, and its container when it has several) in a colour of its own; the
  full pod and container are on hover and in copies, and the prefix can be hidden. Pods that start, restart or replace deleted ones are followed without
  reopening the tab. A pod whose stream fails does not stop the others, and the tab shows how many streams are
  live, failed or over the limit. Search, severity, timestamps and selection work as for one pod.
- Tests: unit, cluster, ui.

### UP-46 Save logs to a file
- **When** I save a log tab's lines: all of them (Cmd-S), the search matches (Shift-Cmd-S), or the selection
  (Option-Cmd-S), also from the Save menu (inside the "⋯" menu in a narrow dock) or the palette.
- **Then** a confirmation says how many lines will be written and that the file is not redacted; after it, the native
  save dialog writes them as shown, with timestamps only if they are shown (in full), and the tab says how many lines
  went where.
- Tests: unit, ui.

### UP-47 Logs and a shell from any pod or workload row
- **When** I right-click a pod or workload row (Deployment, StatefulSet, DaemonSet, ReplicaSet, Job), or select it
  and press Cmd-L, Shift-Cmd-L or Shift-Cmd-E (also from the palette).
- **Then** the row is selected and its menu offers Logs, Previous Logs (pods) and Shell: a pod's logs and shell, a
  workload's logs of all pods (UP-45) and a shell in its first ready pod. When the pod has several containers, each
  entry picks one, the default container marked (Previous Logs lists those that restarted); the keys open the default
  container at once. Each opens its dock tab, or shows the one already open. A workload's shell opens in a pod listed
  when I ask, never in one listed earlier. Chosen before the row's pod has loaded, an entry waits for it, says so,
  and gives up after 30 seconds; a tab that opens late does not take focus from where I went meanwhile, nor hide the
  dock tab I am typing in. What does not apply is disabled, with the reason under it in the menu, and on the pane's
  buttons both in their tooltips and in a caption under them that I can read without hovering (Logs and Shell stay,
  disabled, when the pod could not be fetched, and while the cluster is not connected): a pod that never restarted
  (Previous Logs), a workload with no ready pod (as last listed, until it changes), a pod that finished or is being
  deleted, a pod or workload that was deleted (Shell), or a cluster that is not connected. An action that cannot run
  anyway (a key, or a wait that ended badly) says what and why in one sentence ("Can't open a shell: no pod of
  Deployment web is ready.") in a strip with a warning icon above the dock, covering nothing, until the next one
  opens, I select something else, or ten seconds pass.
- Tests: unit, cluster, ui.

### UP-48 Filter a log to what matters
- **When** I search a log tab and turn on "Matching" (M), turn on "Errors" (E), or both, and choose how many lines
  of context to keep around each line (0 to 10 from the toolbar, up to 100 from the palette).
- **Then** the tab shows only the lines that match (and are warnings or errors, with "Errors"), with the context
  around them, and keeps doing so as lines arrive; the toolbar counts them against the buffer ("12 of 1,234
  lines"). A filtered tab with nothing to show says why and that new lines show as they come. Next and Previous
  Match, selecting, copying and saving work on the rows shown: Save writes them, Save Matching Lines the matches
  among them. Turning the toggles off shows every line again; so does "Matching" while the search is empty. A full
  buffer (100,000 lines) is filtered without the tab stalling: what was shown stays until the filter is ready, and
  the toolbar says it is filtering. In a narrow tab the toggles move to the More menu.
- Tests: unit, ui.

---

## M3: Editing and actions

### UP-50 Edit with inline validation
- **Status:** planned (#31, After 1.0), not built yet.
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
- **When** I delete, scale, rollout-restart, cordon, uncordon or drain from a row's context menu.
- **Then** a confirmation names the cluster, server, namespace and object, after any GitOps warning. Delete and Drain
  take the button: Enter cancels them. Scale asks for the count first and shows from and to. Drain first offers two
  unchecked options (delete emptyDir data, force pods without a controller), each saying what is lost; completed and
  failed pods are removed whatever they say. Drain's warning also names GitOps-managed pods it would evict. Each
  action rechecks the live object first. If the object changed, nothing is run again: a new review opens, saying
  so. Drain reports what it did, also when it stopped early:
  cordoned, evicted, blocked and why (a PodDisruptionBudget is named), skipped, not tried, may still be terminating.
- Tests: cluster, ui.

---

### UP-54 Protected clusters are read-only
- **When** I connect to a remote cluster I have not trusted, and try to edit, delete, scale, restart, drain, exec or
  port-forward.
- **Then** the action is disabled and says the cluster is protected, with an Unlock button in place. Browsing, logs and
  details work as usual. A cluster whose API server kubeconfig names by a loopback address or name (kind, Docker
  Desktop, OrbStack, minikube on macOS) starts unlocked. One on another address stays protected even if it runs on
  my machine, such as minikube's Docker driver on Linux (`192.168.49.2`): I trust it once.
- Tests: unit (core refuses writes on protected clusters), ui, e2e. Shell and the resource actions (UP-53) are
  gated today; the rest join the same gate as they are built.

### UP-55 Unlock or trust a cluster
- **When** I unlock a protected cluster, or mark it trusted.
- **Then** an unlock asks once, naming the cluster and its server, and lasts until Flint quits; the sidebar shows an open
  lock, and Lock Again relocks it. Trust lasts across restarts and is pinned to the server URL: if the context is
  re-pointed, the cluster is protected again.
- Tests: unit (settings, pinning), ui, e2e.

---

### UP-56 See only the kinds in use
- **Status:** planned (#194, After 1.0), not built yet.
- **Given** watch mode All, and a cluster where Replication Controllers, most of the Network section and two CRD groups
  have no objects.
- **When** I connect, wait for the lists to load, and look at the resource tree.
- **Then** each section lists only its kinds with at least one object anywhere in the cluster, and ends in a dimmed
  "… N more" row that expands in place. For kinds watched across the whole cluster, changing my namespace filter
  changes nothing. A collapsed section's count is its kinds in use; a section with none in use shows only its
  "… N more" row. The two empty CRD groups fold into one "… 2 empty groups" row at the end. Kinds I may not list sit
  under "N more" with the dimmed "cannot be listed" text and red dot, and hovering says why. No counts appear next to
  names. Cmd-P still finds every hidden kind, and opening one keeps it visible until I navigate away. The expanded
  rows are remembered per cluster.
- Tests: unit (in-use state, folding), ui, e2e (fake API server).

### UP-57 The tree follows the cluster without jumping
- **Status:** planned (#194, After 1.0), not built yet.
- **When** I connect and the lists are still loading, then create the first Job in a cluster that had none, and later
  delete the last Gadget while the tree is open.
- **Then** while a kind's first list is loading or failing (other than forbidden), it stays shown; it folds at most
  once, after its first list finds nothing, and a watch that reconnects changes nothing. Jobs appears at once.
  Gadgets stays where it is until I connect again or switch back to the cluster. A kind I may list only in some
  namespaces is in use when the namespaces my filter names have an object of it. In a cluster set, a kind shows when
  any member has an object. With watch mode On view, the full tree shows as before, and no extra requests are made.
- Tests: unit, ui, e2e (fake API server: slow first lists, reconnects, request log).

### UP-58 Read workload health at a glance
- **Status:** planned (#196, After 1.0), not built yet.
- **When** I open Deployments, StatefulSets, DaemonSets, ReplicaSets or Replication Controllers.
- **Then** one Ready column shows "ready/desired" with a dot: green when all are ready (also "4/3" during a surge),
  amber when some are not or when none are yet while it rolls out, red when none are and it is not rolling out, grey
  and dimmed at 0/0. "· 3 current" shows only when current differs, and hover shows the kind's details (up-to-date
  and available for Deployments). Sorting by Ready puts red first, then amber, green and grey, each by the share ready
  (grey by name alone), then by name. During a rollout the cell shows a static ↻ and "rolling out", with no animation; a paused Deployment
  says "paused"; a Deployment past its progress deadline turns red with the condition's message on hover. Wide adds
  the server's raw columns next to Ready. Jobs and CronJobs keep their own columns. An agent's `list_resources` still
  gets the server's columns.
- Tests: unit (status rules per kind), mcp, ui, e2e (fake API server with full objects and a rollout, #129).

### UP-59 Old ReplicaSet revisions stay out of the way
- **Status:** planned (#197, After 1.0), not built yet.
- **Given** Deployments that have rolled out many times, one rollout still scaling its old ReplicaSet down, and one
  orphan ReplicaSet scaled to 0.
- **When** I open Replica Sets.
- **Then** the header says how many old revisions are hidden (for example "84 old revisions hidden") with a Show
  toggle. Only ReplicaSets with pods wanted or still running, and the orphan, are listed. A text filter that matches an
  old revision shows it dimmed. Show is remembered per cluster.
- Tests: unit (owner and replica rules), ui.

M3 used every number in the 5x block, so its later paths continue at UP-121.

### UP-121 I can only access one namespace
- **Given** an identity whose RBAC allows it to work in namespace `demo` only, on a cluster that is not protected.
- **When** I select an object and open its row menu or the detail pane.
- **Then** each action the cluster clearly forbids (Edit, Delete, Scale, Rollout Restart, Cordon, Uncordon, Drain, Logs,
  Shell) is disabled and names the missing permission, for example "your access does not allow delete on deployments
  in demo". A check that fails or times out does not disable an action by itself, and the API server decides
  any action left enabled. An answer never outlives the connection it came from: after a re-pointed context or a
  changed definition the actions are enabled until the cluster answers again. Port-forward joins this list when it is
  built (UP-70).
- Tests: unit, e2e (fake API server, `a_namespace_limited_identity_sees_what_it_may_do`,
  `a_repointed_context_does_not_reuse_an_answer`, `a_changed_definition_does_not_reuse_an_answer`,
  `a_provisional_context_sends_no_check`, `a_namespace_limited_user_sees_forbidden_actions_disabled`).

## M4: Curated views, metrics and the resource diagram

### UP-60 Understand a workload at a glance
- **When** I open a pod, deployment or node.
- **Then** I see container states and restarts, rollout progress, endpoints, and node capacity and allocation.
- Tests: cluster, ui.

### UP-61 See resource usage
- **Status:** planned (#101, not in 0.5.0), not built yet.
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

### UP-65 See what's wrong when I connect
- **Status:** planned (#101, not in 0.5.0), not built yet.
- **When** I connect to a cluster, or pick Overview in the resource tree.
- **Then** the dashboard shows counts of failing pods, not-ready nodes, recent Warning events, pending PVCs, failed Jobs
  and workloads short of replicas. Clicking a count opens the list filtered to exactly those objects. Counts update
  live.
- Tests: unit (problem rules), ui, e2e (fake API server).

### UP-66 See how full the cluster is
- **Status:** planned (#101, not in 0.5.0), not built yet.
- **When** metrics-server is installed and I look at the dashboard's usage tiles.
- **Then** I see CPU and memory used against allocatable, and requests and limits against capacity, per node and in
  total, each with a sparkline of the last hour that keeps growing while I stay connected. Without metrics-server the
  tiles say how to install it.
- Tests: unit (ring buffer, aggregation), ui.

### UP-67 Set warning thresholds
- **Status:** planned (#102, not in 0.5.0), not built yet.
- **When** I open the dashboard's Thresholds panel and set CPU warnings at 80% for this cluster only.
- **Then** this cluster's tiles warn from 80%, other clusters keep the global default, and `settings.toml` holds the
  typed override.
- Tests: unit (settings), ui.

### UP-68 Follow recent activity
- **Status:** planned (#101, not in 0.5.0), not built yet.
- **When** a rollout starts or a Warning event fires while the dashboard is open.
- **Then** it appears at the top of the activity feed within seconds, and clicking it opens the object.
- Tests: ui, e2e.

### UP-69 Find the namespace that is broken or full
- **Status:** planned (#195, After 1.0), not built yet.
- **When** I open Namespaces.
- **Then** besides Name, Status and Age, each row shows Problems (failing or crash-looping pods, and workloads short of
  replicas that are not rolling out, counted by the dashboard's rules), Pods, and CPU and Memory as "used / requested".
  With nothing requested the cell shows usage alone and says "no requests" on hover. Without metrics-server the used
  part is blank and says so on hover; when I may not read metrics, the hover says that instead. A namespace Flint
  cannot watch shows "unavailable", not 0. The list is sorted by name; clicking a column sorts by it, and the choice is
  remembered per cluster (#205). Counts update live.
- Tests: unit (aggregation, problem rules, effective requests), ui, e2e (fake API server with full pod objects and
  metrics, #129).

M4 used every number in the 6x block, so its later paths continue at UP-120.

### UP-120 Open a namespace's overview
- **Status:** planned (#198, After 1.0), not built yet.
- **When** I click the `demo` row in Namespaces.
- **Then** an overview of `demo` opens in the main view in place of the list, and Back returns to it. It shows problem
  counts, recent Warning events and rollouts, CPU and memory usage against requests with sparklines, and
  ResourceQuota use and LimitRange limits when the namespace has them. A count opens its list with a filter for
  `demo` of that list's own; my namespace filter does not change. The sparklines start when I open the overview or
  the Namespaces list and fill in from then, up to an hour. Its YAML tab shows the namespace object, and Show YAML in
  the row's context menu does too.
- Tests: unit (namespace scope, series start and drop), ui, e2e.

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

Multi-cluster management, watching and alerting are the next focus and are not in 0.5.0, so cluster sets (UP-82 to
UP-85) are not built yet. Warm clusters (UP-78, UP-79) are after 1.0 (#247). AI log analysis (UP-75 to UP-77) is not scheduled.

M5.5 was added after M5 and M6 were numbered, so it shares the 7x block with M5 and uses UP-75 to UP-79.

### UP-75 Analyze log lines with Claude Code
- **Status:** planned, not scheduled on the roadmap, not built yet.
- **Given** `claude` on my login-shell `PATH`.
- **When** I select log lines and press Analyze with Claude (or its palette action), tick "Allow log analysis on
  `<context>` (`<server>`)" the first time, and confirm.
- **Then** the answer streams into a side panel, below what was sent. Stop ends the process and anything it started.
  Nothing in the answer runs, opens or changes anything. The opt-in is checked against the log's own cluster, even
  if I have switched to another one since. A re-pointed context counts as not allowed. If Claude Code's session
  report shows any tool, MCP server or plugin, nothing is sent.
- Tests: unit (prompt, allow policy, process environment), ui (with a fake `claude` on `PATH`).

### UP-76 See exactly what leaves my Mac
- **Status:** planned, not scheduled on the roadmap, not built yet.
- **When** the confirmation opens.
- **Then** it shows the full prompt and the lines as they will be sent, with credential shapes, vendor tokens,
  private keys and the values of Secrets, env and ConfigMaps the pod references already replaced. It names the CLI
  and its path. Lines past 1,000, past 4 KiB each or past 256 KiB in total are cut, and it says so. If Flint could
  not read the pod's Secrets or ConfigMaps, it says those values were not checked. With "Always for this cluster",
  later sends skip the sheet but are still redacted, and the panel shows what was sent. "Always" resets if the
  binary path changes. The fake `claude` receives byte for byte what was shown.
- Tests: unit (redaction patterns and encodings, limits), ui (fake `claude`).

### UP-77 Claude Code not installed
- **Status:** planned, not scheduled on the roadmap, not built yet.
- **Given** no `claude` on the resolved `PATH` (relative and empty entries ignored), nor in settings, nor in a
  supported version. Having only `codex` installed counts as not installed in v1.
- **Then** the log viewer shows no Analyze with Claude button, the palette has no Analyze action, and there is no
  error.
- Tests: unit (CLI lookup), ui.

### UP-78 Switch between recent clusters instantly
- **Status:** planned (#247, After 1.0), not built yet.
- **When** I use clusters A, B and C, then switch back to A.
- **Then** A's lists show current data at once, with no loading state (in mode All, Events, Leases and
  EndpointSlices reload, because warm clusters drop them). The sidebar marks A as active and B and C as
  warm. Changes made on a warm cluster while I was away are already there. Relaunching Flint connects nothing
  until I select a cluster.
- Tests: unit (warm set), cluster, ui.

### UP-79 Warm clusters stay bounded and quiet
- **Status:** planned (#247, After 1.0), not built yet.
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

### UP-85 Compare a cluster set at a glance
- **When** I open the dashboard of a cluster set.
- **Then** each member has a card with its colour tag, problem counts and usage sparklines, the unhealthiest first,
  under a totals strip. Clicking a count opens the combined list filtered to it.
- Tests: unit (ordering, totals), ui, e2e.

### UP-86 One row per namespace across a cluster set
Deferred until cluster sets are scheduled (#199).
- **Given** a set of three clusters that all have a `payments` namespace, one of them terminating it, and one without
  metrics-server.
- **When** I open Namespaces in the set.
- **Then** `payments` is one row saying "3 clusters", with Problems, Pods, CPU and Memory summed, status Terminating
  and the oldest age. Usage is marked partial and compared only with the requests of the clusters that report usage.
  Its chevron shows one child row per cluster with its colour tag and its own values.
- Tests: unit (merging, worst status, partial usage), ui, e2e (two kind clusters).

### UP-87 Compare one namespace across a cluster set
Deferred until cluster sets are scheduled (#199).
- **When** I click the merged `payments` row.
- **Then** an overview shows a card per cluster for `payments` (problems, usage, sparklines) under a totals strip,
  and clicking a card opens that cluster's `payments` overview.
- Tests: unit (totals), ui, e2e.

## M6: Install

### UP-80 Install from the DMG
- **When** I install from the DMG on a clean Mac.
- **Then** the app opens without Gatekeeper warnings, on both Apple Silicon and Intel.
- Tests: manual, plus a CI smoke test of the DMG (`scripts/smoke-macos-app.sh`).
- Before M6, releases already ship the DMG with an ad-hoc signed `Flint.app` (#175). It installs the same way, but the
  first launch of a browser download needs Open Anyway in Privacy & Security, or removing the quarantine flag (README).
  Signing and notarization, which make the warning go away, stay in #25.

### UP-81 Install with Homebrew
- **When** I run `brew install --cask flint`.
- **Then** the app is installed and `flint mcp` is on my `PATH`.
- Tests: manual, plus a CI smoke test.

---

## M7: Fleet actions (v0.7)

### UP-100 Restart or scale a workload on several clusters
- **When** I select a Deployment in a combined view and choose Rollout Restart or Scale on 3 clusters.
- **Then** a preview lists each target cluster with a dry-run result and a checkbox. Restart and scale each run one
  cluster at a time and stop at the first failure, reporting a partial result that says which clusters were done and
  which were not. Locked clusters in the list
  need their name typed first.
- Tests: unit (sequencing, stop on failure, partial result for restart and scale), e2e.

### UP-101 Apply one manifest to several clusters
- **When** I apply a YAML manifest to a cluster set.
- **Then** I see a server-side dry-run diff per cluster before anything changes, can drop clusters from the target list,
  and the apply runs one cluster at a time as above. Delete follows the same flow.
- Tests: unit, e2e.

## M8: Recordings (after 1.0)

### UP-110 Record a workload
- **When** I choose Record on a Deployment, close its log tab and keep working, and its pods restart and get replaced.
- **Then** the recording keeps every pod's logs, the workload's events and its pods' CPU and memory, with no gaps
  across restarts, and it survives quitting and relaunching Flint.
- Tests: unit (store, retention), e2e (fake API server with pod churn), cluster.

### UP-111 Catch trouble I wasn't watching
- **When** automatic recording is on for a cluster and a pod starts crash-looping.
- **Then** a recording of its workload starts by itself (at most 10 pods), I get a notification, and it stops 30 minutes
  after the workload is healthy. On clusters where it is off, nothing records.
- Tests: unit (triggers, caps, auto-stop), e2e.

### UP-112 Replay what happened
- **When** I open a recording and click the OOMKilled event on its timeline.
- **Then** the logs jump to that moment, with the CPU and memory tracks above showing usage leading up to it.
- Tests: ui, e2e.

### UP-113 Recordings stay within their limits
- **When** recordings reach their age or size limits.
- **Then** the oldest data is removed first, starred recordings are kept, and the file is owner-only.
- Tests: unit.

### UP-114 Credentials expire while recording
- **When** a recorded cluster's token expires and its exec plugin would need to run.
- **Then** the recording pauses with a "sign-in needed" badge and a notification, no plugin runs in the background, and
  signing in resumes it with the gap marked.
- Tests: unit, ui.

### UP-115 An agent asks what happened
- **Status:** not planned. The MCP server stays at its six read-only tools ([ADR-0009](./adr/0009-read-only-mcp-server.md)).
- **When** an agent queries a recording's time range through MCP on an exposed cluster.
- **Then** it gets lines, events and metrics for that range, redacted and within the cluster's scope, and the call is
  in the activity log. Unexposed clusters' recordings are invisible to it.

### UP-116 Export a recording
- **When** I export the last 30 minutes of a recording.
- **Then** I get the logs as text and the events and metrics as CSV, after a warning that they are not redacted.
- Tests: unit, ui.

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
