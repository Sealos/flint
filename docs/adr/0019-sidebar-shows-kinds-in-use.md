# The resource tree shows the kinds in use and folds the rest

The resource tree lists every listable kind discovery returns. Most of them have no objects on a typical cluster:
Replication Controllers, Pod Templates, CSI Storage Capacities, a dozen CRD groups an operator installed. The kinds the
user works with are lost among them. Flint therefore shows only the kinds **in use**, and folds the others into a row
they can open.

- **In use:** a kind is in use when it has at least one object anywhere in the cluster: in any namespace for a
  namespaced kind, or at all for a cluster-scoped kind. For kinds watched across the whole cluster, the namespace
  filter does not change it. In a cluster set ([ADR-0014](./0014-cluster-sets-and-combined-views.md)) a kind is in use
  if any member has an object of it.
- **Folding:** the kinds not in use collapse into a dimmed **"… N more"** row at the end of their section, which
  expands in place. Whether it is expanded is remembered per cluster.
- **CRD groups** follow the same rule: empty CRDs go under the group's "… N more". A group with no objects at all
  folds into one **"… N empty groups"** row at the end of the tree.
- **Section headers:** a collapsed section's count is the number of its kinds in use. A built-in section with nothing
  in use keeps its header, shows 0 when collapsed, and holds only its "… N more" row, so sections never move.
- **The open kind stays:** the kind currently open always stays visible, in use or not, until the user navigates
  away.
- **Kinds Flint may not list** (RBAC forbidden, so their use is unknown) count as not in use. They sit under
  "N more" with today's markers for them: the dimmed "cannot be listed" text and the red failed dot, with the reason
  on hover. There is no new icon.
- **Unknown until the first list:** a kind whose first list has not finished (still loading, or failing for a reason
  other than RBAC) is unknown, and stays shown. It folds at most once, after its first list completes and finds
  nothing. A watch that restarts or reconnects keeps the kind's last known state, so nothing pops in or out.
- **Live:** a kind that gains its first object appears at once. A kind that loses its last object stays visible until
  the next connect, or until the user switches back to the cluster, so rows never vanish under the cursor.
- **Only in watch-everything mode** ([ADR-0008](./0008-watch-everything-on-active-cluster.md)): there, the watches
  already hold every kind's objects, so the answer costs no API calls. In the opt-in "on view" mode the full tree shows
  as today, and Flint makes no extra calls to find out which kinds are in use.
- **No counts** appear next to kind names. A cluster set's `2/3` badge (which members serve a kind) stays.
- **Search still finds everything:** hidden kinds stay reachable from the resource switcher (Cmd-P) and search.

## Considered Options

- **"In use" within the namespace filter:** follows what the list would show, but the tree would reshuffle every time
  the filter changes, and cluster-scoped kinds have no namespace to filter by.
- **One "Empty kinds" section at the bottom:** one place to look, but a kind would leave its familiar section when it
  empties, and Workloads would no longer always hold the workload kinds.
- **Core kinds always visible:** Pods or Services would never hide, but "core" is a judgement call, and an empty kind
  in the tree is the noise this removes.
- **User pins:** lets the user keep chosen kinds, but adds a setting to manage for what the open-kind rule and search
  already cover.
- **Probing with `limit=1` lists in on-view mode:** gives the same tree in both modes, but costs one request per kind
  on every connect, which on-view mode exists to avoid.
- **Fully live hiding:** the tree always matches the cluster, but a kind whose last pod is replaced would vanish and
  reappear under the cursor.
- **Hiding only on connect:** stable, but a kind created after connecting (a new CRD's first object, a first Job)
  would stay hidden until the next connect.
- **Showing forbidden kinds as usual:** keeps them in sight, but Flint cannot open them, so they are noise in the
  same way empty kinds are.
- **Counts next to names:** informative, but the numbers churn constantly on busy kinds, draw the eye, and add little
  to "has objects or not".

## Consequences

- The navigation model gains a state per entry (in use, not in use, unknown), derived from the watch caches, and the
  "… N more" and "… N empty groups" rows. Discovery still builds the full tree; folding is a view over it.
- Kinds that can be listed but not watched (PodMetrics and NodeMetrics, for example) have no cache to answer from.
  They are not in the in-use set, so they fold under "N more" like forbidden kinds.
- A kind the user may list only in some namespaces is listed namespace by namespace, in the namespaces the namespace
  filter names. For these kinds alone, in use covers only those namespaces, so it follows the filter.
- The Overview entry at the top of the tree ([ADR-0017](./0017-cluster-dashboards.md)) never folds.
- A cluster set folds its tree only when every member is in watch mode All. If any member is in On view mode, the set
  shows the full tree.
- `state.toml` gains the expanded "… N more" rows per cluster.
- On-view mode keeps today's full tree. A user who switches modes sees the tree change, which the mode setting's
  description says.
