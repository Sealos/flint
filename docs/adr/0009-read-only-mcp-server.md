# Flint is a read-only, opt-in MCP server over stdio

To make Flint AI-native, the `flint` binary has an `mcp` subcommand that runs a Model Context Protocol server over
stdio. Claude Code, Claude Desktop, Cursor, and other clients launch it themselves. It reads the user's kubeconfig
and Flint's settings, and it works whether or not the app is open. The server is **read-only**. It only sees clusters
the user has marked **Expose to AI**, which is off by default, because the user's kubeconfig typically includes
production clusters. Secret values are always redacted, so they never reach an LLM provider. Flint does not ship its
own chat UI or hold LLM API keys. The user brings their own agent.

## Considered Options

- **Built-in AI chat panel:** needs provider keys, prompt plumbing, and UI, and duplicates the agents users already
  have.
- **HTTP served by the running app:** could share the app's live caches, but only works while Flint is open.
- **Write tools with in-app approval:** deferred. Read-only covers diagnosis, and no agent can change a cluster
  through Flint.
- **Expose all contexts, like kubectl:** rejected because production clusters would be visible by default.

## Consequences

The MCP process and the app share `settings.toml`. The MCP server reads the opt-in list again on every call, so
toggling a cluster in the app takes effect immediately. Any future write tool needs its own ADR.

An exposure is stored as context name plus API server URL, not as a name alone. A context name can point to a
different cluster in another `KUBECONFIG`, or after it is re-pointed (for example with
`aws eks update-kubeconfig --alias`). Without the URL, that could quietly expose a production cluster. When the URL
does not match, the cluster counts as not exposed.

The `flint-mcp` crate also builds a `flint-mcp` binary: the same server and the same policy as `flint mcp`, without
GPUI, so the MCP end-to-end tests run on Linux CI against kind. It is not shipped; AI clients register `flint mcp`.
