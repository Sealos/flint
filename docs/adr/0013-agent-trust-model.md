# Agents get a scoped, redacted and recorded view of a cluster

[ADR-0009](./0009-read-only-mcp-server.md) made `flint mcp` read-only and opt-in per cluster, pinned to the
cluster's API server URL. Read-only access and hiding Secrets are now common in other Kubernetes MCP servers, often
as "best-effort guardrails". What none of them combines is consent the user can narrow, output that is redacted
everywhere an agent reads it, and a record of what the agent read. Flint adds three things on top of ADR-0009, so a
user can expose a production cluster and still know what an agent could see and did see.

1. **Scoped exposure.** Each exposed cluster may list the namespaces and kinds an agent may read. Empty means
   everything, as today. A request outside the scope fails with the same "not found" error as an unexposed cluster,
   so an agent cannot probe what exists.
2. **Redacted logs and events.** Pod logs and event messages pass through a shared redactor in `flint-core` before
   they reach an agent. It replaces values that look like credentials: bearer and basic auth headers, JWTs,
   private key blocks, cloud access keys, vendor tokens, passwords in URLs, and `password=`, `token=` and `secret=`
   style pairs. The same applies to other message text an agent can read: list columns, and status, condition and
   container termination messages in objects. The result says how many values were replaced. The app's own views
   are not redacted. AI log analysis (ADR-0011) uses the same redactor.
3. **An agent activity log.** Every MCP call is recorded: when, which cluster, which tool, what it named (kind,
   namespace, object, container), how much came back, whether it was cut short or denied. Returned content is never
   recorded. The app shows the log per cluster.

## Considered Options

- **Rely on RBAC:** the right long-term control, but users run Flint with their own powerful credentials. Asking them
  to mint a separate service account per cluster before using an agent is the friction that makes people expose
  everything.
- **Redact only Secrets (today):** credentials leak far more often through logs and event messages than through
  Secret objects an agent asks for by name.
- **Record returned content in the activity log:** gives a full replay, but turns the log into a second copy of
  sensitive data on disk. Recording what was asked for, and how much came back, answers "what did the agent look at"
  without that risk.
- **An allowlist of tools instead of namespaces and kinds:** too coarse. `list_resources` on `secrets` and on `pods`
  are the same tool with very different risk.

## Consequences

- `ai_exposed_clusters` entries gain two optional typed fields, `namespaces` and `kinds` (lists of names). Old
  settings files keep working, and the app edits them from the Expose to AI dialog.
- With a namespace scope set, cluster-scoped kinds (nodes, CRDs, namespaces themselves) are hidden unless the kinds
  scope names them.
- The activity log is an append-only file in Flint's config directory, one typed record per call, rotated at a size
  cap. Both `flint mcp` and the app use the same `flint-core` type, so the format has one definition. The app watches
  it as it watches `settings.toml`.
- The redactor's patterns can miss a credential or redact harmless text. Misses are the worse failure, so the
  patterns err toward redacting, and each one has unit tests with real-world formats.
