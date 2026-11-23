# Views can combine a set of clusters into one table

**Amended by [ADR-0020](./0020-namespace-columns-and-overview.md)** (deferred until cluster sets are scheduled). In the
Namespaces list, same-named namespaces will merge into one row with a child row per member.

[ADR-0012](./0012-keep-recently-used-clusters-warm.md) keeps recently used clusters connected but shows one cluster
at a time. Users who run the same workloads on several clusters (dev, staging, prod regions) need to answer "where
is it broken?" without clicking through each cluster. Flint therefore lets the user combine **2 to 5 clusters** into
one view, which this document calls a **cluster set**, and the UI calls super mode.

- **Choosing clusters:** Cmd-click clusters in the sidebar to combine them now, or open a **named set** saved from a
  selection (for example "prod regions"). Sets are a typed `cluster_sets` setting: a name and its members, each
  member pinned to its context name and API server URL, as AI exposure is. A member whose context was re-pointed is
  left out, and the set shows it as changed.
- **One table:** rows from every member are interleaved in one virtualized list with a **Cluster** column that sorts
  and filters like any other, so sorting by Status puts every crashing pod first. A **group by cluster** toggle
  switches to one collapsible section per cluster, each with its own count. Both use the same rows.
- **Columns** are the union, by name, of the columns the members return. A cell whose cluster lacks that column is
  blank.
- **Kinds:** the resource tree lists every kind any member serves, with a `2/3` badge when only some do. Opening it
  combines the members that serve it, and the list header names the ones that don't.
- **Colour tags:** each cluster may have an optional colour from a fixed palette of theme tokens (a typed setting),
  shown as a dot in the sidebar and the Cluster column, and as a strip in the detail header. Clusters without one
  show their name.
- **Rows keep their cluster:** opening a row shows its detail from its own cluster, and every action on it acts on
  that cluster only (and obeys [ADR-0015](./0015-clusters-are-protected-by-default.md)).
- **Connections:** every member of an open set is connected and watched as an active cluster is, under the watch
  mode rules of ADR-0008. Closing the set treats every member that is not the active cluster as cold under ADR-0008: the watches stop, the
  client and discovery cache stay, and logs and exec sessions the user started keep running. A port-forward on such a
  member keeps running only if the cluster has keep watching on (ADR-0008). Warm
  clusters ([ADR-0012](./0012-keep-recently-used-clusters-warm.md), #247) are not scheduled before cluster sets. If
  warm clusters ship later, members don't count against `warm_clusters`, and closing the set returns them to the
  warm rules. Each member's errors show in its sidebar row and as a one-line note in the combined list, and never block
  the other members' rows.

## Considered Options

- **Separate windows per cluster:** no new design, but no single view answers "which cluster has the problem?".
- **Grouped sections only:** easier to scan per cluster, but nothing can be sorted across clusters, which is the main
  reason to combine them.
- **Only the columns, or kinds, all members share:** tidier, but hides CRD columns whose versions differ, and makes a
  CRD installed on 2 of 3 clusters impossible to combine.
- **Combine the warm clusters automatically:** no new concept, but the members change as the user browses, which is
  unacceptable once actions can target them.
- **Fleets of dozens of clusters:** would need polled summaries rather than live watches. Out of scope; sets are
  capped at 5 members.

## Consequences

- ADR-0012's "views show one cluster at a time" no longer holds when a set is open. ARCHITECTURE.md's non-goal of
  side-by-side cluster views is replaced by this decision.
- The list engine gains a cluster dimension: each row carries its cluster, and the combined snapshot merges member
  snapshots without copying rows (`Arc` rows, as today).
- Memory and CPU grow with the number of members. The kwok measurements planned for warm clusters also cover a
  5-member set.
- **Fleet actions** (the same action on several clusters at once) build on sets in v0.7. Their safety rules are
  in ADR-0015.
