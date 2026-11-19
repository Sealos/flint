# Watch every kind on the active cluster, with an on-view mode as an option

By default, while a cluster is active (shown in the window), Flint watches every discovered kind that can be listed
and watched, so moving between lists is instant. A setting, global with a per-cluster override, switches to **on
view** mode, which watches a kind only while one of its views is open. Clusters that are not active drop all their
watches but keep their client and discovery cache. Switching back starts the watches again with a fresh list.

This trades some memory for speed on the cluster being worked on, and stops memory from growing with the number of
clusters open. Sessions the user started (logs, exec, port-forwards) are not watches and keep running after a switch.

## Considered Options

- **Watch on view with a few minutes of linger:** lower memory, but the first visit to each list still has to load.
- **Watch only the visible list:** lowest memory, but every navigation reloads.
