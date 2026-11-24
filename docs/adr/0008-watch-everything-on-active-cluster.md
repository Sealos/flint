# Watch every kind on the active cluster, with an on-view mode as an option

**Amended by [ADR-0012](./0012-keep-recently-used-clusters-warm.md).** The last few clusters the user selected stay
warm and keep their watches. Only clusters outside that set drop their watches as described below.

By default, while a cluster is active (shown in the window), Flint watches every discovered kind that can be listed
and watched, so moving between lists is instant. A setting, global with a per-cluster override, switches to **on
view** mode, which watches a kind only while one of its views is open. Clusters that are not active drop all their
watches but keep their client and discovery cache. Switching back starts the watches again with a fresh list.

This trades some memory for speed on the cluster being worked on, and stops memory from growing with the number of
clusters open. Sessions the user started (logs, exec) are not watches and keep running after a switch. A port-forward
keeps running after a switch only if its cluster has keep watching on. It always stops when its context is re-pointed
or redefined (even if put back), paused, disconnected or signed out, when keep watching or its exec consent is turned
off, and on quit.

## Considered Options

- **Watch on view with a few minutes of linger:** lower memory, but the first visit to each list still has to load.
- **Watch only the visible list:** lowest memory, but every navigation reloads.
