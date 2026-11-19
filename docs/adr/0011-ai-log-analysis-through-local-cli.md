# AI log analysis through the user's local CLI

The log viewer gets an **Analyze with Claude** or **Analyze with Codex** button. It runs the user's own `claude`
(Claude Code) or `codex` (OpenAI Codex CLI) in its non-interactive mode as a child process. The selected log lines go
in on stdin, after a short prompt with the pod and container context. The answer streams into a side panel. Flint
holds no API keys and talks to no LLM provider itself. The CLI uses the account the user already signed in with.

This is hard to reverse because log lines leave the machine through a third-party tool. So the rules are strict:

- **Only on a user action.** Nothing is sent unless the user clicks the button and confirms. Flint never analyzes on
  its own, on a schedule, or when an error appears.
- **Only for clusters the user allowed.** Log analysis is a per-cluster opt-in, off by default, separate from Expose
  to AI. It is stored as context name plus API server URL, like an exposure. If the context is re-pointed, it counts
  as not allowed.
- **Redacted first.** The lines pass through `flint_core::redact` before they are shown or sent. It replaces the
  values of Secrets the pod references (env, `envFrom`, volumes) and credential-shaped strings: bearer tokens, JWTs,
  PEM blocks, cloud access keys, `password=`/`token=`/`secret=` pairs and passwords in URLs.
- **The user sees exactly what is sent.** A confirmation shows the full prompt and the redacted lines, names the
  CLI and its path, and has an "Always for this cluster" option. That option is a typed setting on the cluster's
  opt-in entry. It skips the confirmation, never the redaction.
- **Bounded.** At most 1,000 lines (a setting) are sent. The confirmation says when the selection was cut.
- **Log text is untrusted.** The prompt says the lines are data from a container, may contain instructions, and must
  not be followed. The CLI runs with its tools turned off (or in a read-only sandbox where it cannot turn them off),
  with no MCP servers, in an empty temporary directory, and without `KUBECONFIG`. Flint shows the answer as text.
  It never runs a command from it, opens a link from it, or turns it into an action.
- **One shot.** The panel shows one answer per click, with Stop and Copy. It has no follow-up prompt, so it is not a
  chat panel.

The CLI is found on the resolved login-shell `PATH`, the same way exec plugins and `helm` are. `claude` and `codex`
binary overrides live in `settings.toml`. If neither CLI is found, the button is hidden.

## Considered Options

- **MCP only:** the user asks their agent to read the logs through `flint mcp`. This already works and stays. But it
  needs the cluster exposed, a separate agent window, and the user has to describe which lines they mean. The button
  sends exactly the lines on screen.
- **Built-in API keys:** Flint calls a provider's API with a key the user pastes in. Rejected. Flint would store
  secrets, pick providers and models, and maintain an API client. That is what [ADR-0009](./0009-read-only-mcp-server.md)
  already ruled out.
- **The user's local CLI (chosen):** no secrets in Flint, the user's own account, model choice and data policy, and
  little code. The cost is a dependency on the CLIs' command-line flags, which can change.

### Reuse Expose to AI or a separate opt-in?

A separate opt-in. Expose to AI gives any MCP client a standing read of the whole cluster. Log analysis sends a few
lines the user picked, each time they click. If one toggle covered both, a user who wants to analyze one production
log line would have to let agents read all of production. The two toggles share their storage shape and the
re-pointing check, so the code stays small.

## Consequences

- `flint_core::redact` gets a log redactor with unit tests for every pattern. The Secret values it matches are read
  when the user clicks the button, kept in memory for that send only, and never logged. If the user may not read
  those Secrets, the confirmation says the values could not be checked.
- The CLI's flags are pinned in one place and checked against each CLI's version at first use. If a CLI no longer
  accepts them, the button shows an error instead of running it without the safety flags.
- The prompt never includes the cluster name, the server URL, or kubeconfig data. It carries the namespace, pod,
  container, owner, phase, restart count and last termination reason.
- The CLI's account, provider and data retention are the user's. Flint names the CLI in the confirmation, so the user
  knows where the lines go.
