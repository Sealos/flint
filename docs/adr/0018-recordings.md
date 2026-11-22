# Recordings keep a workload's logs, events and metrics in SQLite while Flint runs

Kubernetes keeps only the current and the previous container's logs, and events expire after about an hour. A pod
that crash-loops overnight, or a pod replaced by a rollout, loses the evidence unless something was watching. Log tabs
follow a workload across restarts, but only while the tab is open and only in memory. Flint therefore adds
**recordings**: a pinned workload, pod or label selector whose logs, events and pod metrics Flint keeps capturing in
the background while the app runs, stored on disk.

- **What is captured:** container logs of every matching pod (the workload-log machinery of #62: one stream per pod
  container, following restarts and replacements, deduplicated), the workload's and its pods' Kubernetes events, and
  each pod's CPU and memory samples every 15 s from metrics-server when it is installed.
- **When:** only while Flint runs. There is no background helper when the app is closed.
- **Starting:** by hand ("Record" on a workload, pod, selector or dashboard problem), or **automatically on trouble**
  on clusters where the user enabled it (off by default): CrashLoopBackOff, OOMKilled or a failed Job starts a
  recording of the affected workload, capped at 10 pods, which stops 30 minutes after the workload is healthy again.
  Automatic recordings show in the sidebar and as a notification.
- **Storage:** one SQLite database in Flint's data directory (`recordings.sqlite`, owner-only permissions, WAL mode),
  with typed tables (recordings, lines with timestamp, pod, container and text, events, metric samples) — no JSON
  blobs. Logs are stored as written, not redacted: a redaction false positive would destroy the exact line needed.
- **Retention:** typed settings with defaults of 7 days, 500 MB per recording and 2 GB in total. The oldest data goes
  first; starred recordings are kept until deleted.
- **Credentials:** recordings never run exec-plugin sign-ins in the background
  ([ADR-0012](./0012-keep-recently-used-clusters-warm.md)). When a recorded cluster's credentials expire, the
  recording pauses with a "sign-in needed" badge and a notification; signing in resumes it, and the gap is marked.
- **Viewing:** a Recordings list (sidebar and palette). A recording opens as a timeline: logs in the log tab below,
  events and CPU/memory tracks above on a shared time axis; clicking an event moves the logs to that moment.
- **Agents:** recordings are not available through MCP. The MCP server stays at its six read-only tools
  ([ADR-0009](./0009-read-only-mcp-server.md)).
- **Export:** a time range as plain-text logs plus events and metrics as CSV, with the "not redacted" warning that
  Save logs shows.

## Considered Options

- **Open log tabs only (today):** loses everything from before a tab was opened, and everything after Flint quits.
- **Memory only:** quitting, updating or a crash drops the history the user was keeping.
- **A background helper that records while Flint is closed** (LaunchAgent, systemd user service): catches overnight
  problems with the laptop's lid closed, but adds a second long-lived process with cluster credentials, its own
  lifecycle and its own sign-in story. Deferred.
- **Plain files per recording (JSON lines or text):** simpler to write, but time-range queries across lines, events
  and metrics, and retention by size and age, want an indexed store.
- **Redacting or encrypting at rest:** redaction can destroy evidence irreversibly; encryption adds key management
  and slows search. Owner-only files match how shells and kubectl cache sensitive data.
- **Automatic recording without guards:** a bad rollout could record hundreds of pods; hence the opt-in, the pod cap
  and the auto-stop.

## Consequences

- Flint gains a SQLite dependency (bundled, so no system library) and a schema with migrations.
- Recordings keep their cluster connected like a warm cluster, and count toward the memory and watch budgets; the
  soak test covers a running recording.
- The data directory now holds sensitive logs; the docs say where and how to delete them.
