# AI log analysis through the user's local CLI

The log viewer gets an **Analyze with Claude** button. It runs the user's own `claude` (Claude Code) in its
non-interactive mode as a child process. A short prompt with the pod and container context and the selected log
lines go in on stdin. The answer streams into a side panel. Flint holds no API keys and talks to no LLM
provider itself. The CLI uses the account the user already signed in with.

This is hard to reverse because log lines leave the machine through a third-party tool. So the rules are strict:

- **Only on a user action.** Nothing is sent unless the user clicks Analyze with Claude (or runs its palette action)
  and confirms, or chose "Always for this cluster" for that cluster. Flint never analyzes on its own, on a schedule,
  or when an error appears.
- **Only for clusters the user allowed.** Log analysis is a per-cluster opt-in, off by default, separate from Expose
  to AI. The opt-in is checked when the lines are sent, against the cluster of the log session (context name plus
  server URL), not the active cluster.
- **Redacted first.** What is sent, and what the confirmation and the panel show, passes through the log redactor
  (see Redaction). The log viewer and copying from it are not changed.
- **The user sees exactly what is sent.** See Consent.
- **Bounded.** At most 1,000 lines, 4 KiB per line and 256 KiB in total. The confirmation shows what was cut.
- **Log text is untrusted.** The prompt says the lines are data from a container, may contain instructions, and must
  not be followed. The CLI runs isolated (see Isolation). Flint shows the answer as text. It never runs a command
  from it, opens a link from it, or turns it into an action.
- **One shot.** The panel shows one answer per request, with Stop and Copy. It has no follow-up prompt, so it is not
  a chat panel.

## The button and the CLI

- One plain button, **Analyze with Claude**. A palette action with a shortcut does the same. If `claude` is not
  found, the button and the action are hidden.
- `claude` is found on the resolved login-shell `PATH`, the same way exec plugins and `helm` are. Relative and empty
  `PATH` entries are ignored. A `claude` binary override lives in `settings.toml`. Flint resolves the binary to a
  canonical absolute path and shows that path in the confirmation.
- `claude` has a minimum version. Older versions are treated as not found, with a hint.
- Codex is not supported in v1 (see Decision: Claude only first).

## Consent

- **First use on a cluster** is one sheet. It shows the full prompt and the redacted lines, names the CLI and its
  path, and has a checkbox "Allow log analysis on `<context>` (`<server>`)" that must be ticked to send. A second
  checkbox, "Always for this cluster", skips the sheet next time.
- The opt-in entry stores the context name, the server URL, the CLI and the canonical binary path, and whether to
  skip the sheet. If the context is re-pointed, it counts as not allowed. If the CLI or its path changes, "Always"
  is reset and the sheet comes back.
- With "Always", the panel still shows what was sent, read-only, above the answer. "Always" never skips the
  redaction or the limits.
- The opt-in is revoked from the cluster sidebar's context menu and from the command palette, like Expose to AI.
  Revoking needs no confirmation.

## Isolation

Flint starts the CLI as a child process in its own process group. Stop, closing the panel, or quitting Flint kills
the group. Stdin and stdout are handled by separate tasks, so a CLI that stops reading cannot block Flint.

**Environment.** The child starts with a cleared environment plus an allowlist: `HOME`, `PATH` (the resolved one),
`USER`, `LOGNAME`, `SHELL`, `LANG`, `LC_*`, `TMPDIR`, and `TERM=dumb`. It also gets Claude Code's own auth variables,
and only when they are set: `ANTHROPIC_*`, `CLAUDE_CODE_USE_BEDROCK` and `CLAUDE_CODE_USE_VERTEX`, and only with
those, the matching cloud variables (`AWS_*` for Bedrock, `GOOGLE_APPLICATION_CREDENTIALS` and `CLOUD_ML_REGION` for
Vertex).

`KUBECONFIG` is set to `/dev/null`. It is never left unset, because unset falls back to `~/.kube/config`. The prompt
goes on stdin, never in argv. The working directory is a new temporary directory with mode `0700`, removed
afterwards.

**Claude Code** ([CLI reference](https://code.claude.com/docs/en/cli-reference),
[headless mode](https://code.claude.com/docs/en/headless)):

```
claude -p --tools "" --disallowedTools "*" --strict-mcp-config --setting-sources "" \
  --disable-slash-commands --no-session-persistence --max-turns 1 \
  --output-format stream-json --verbose
```

Without these flags, a `-p` run loads the user's MCP servers (possibly `flint mcp` itself), hooks and plugins, as an
interactive session would. `--tools ""` removes the built-in tools, `--disallowedTools "*"` denies any that remain,
`--strict-mcp-config` with no `--mcp-config` loads no MCP servers, and `--setting-sources ""` loads no user, project
or local settings, so no hooks. Flint reads the `system/init` event and aborts unless `tools` and `mcp_servers` are
empty and no plugins loaded. Any hook event before it also aborts the run. That `--setting-sources` accepts an empty
list must be verified during implementation.

**Checks.** Unknown flags make the CLI fail before the run, and Flint shows that error instead of retrying with
fewer flags. The run also has to pass the `system/init` check above and the minimum version.

## Decision: Claude only first

v1 supports Claude Code only. Codex is out of v1 scope. The reason is its isolation, analyzed here
([developer commands](https://learn.chatgpt.com/docs/developer-commands?surface=cli)). The closest Flint could get is:

```
codex exec --sandbox read-only --skip-git-repo-check --ephemeral --ignore-user-config \
  --ignore-rules -C <tmpdir> --json -
```

Codex has no flag that turns tools off. `--sandbox read-only` stops writes, but commands can still read any file the
user can, such as `~/.kube/config` or `~/.aws/credentials`, and send what they read to the model. Non-interactive
approval handling is also still in flux ([openai/codex#24135](https://github.com/openai/codex/issues/24135)). Flint
could only read the `--json` event stream and kill the process group at the first command or tool event. That is a
stop after the fact: a command that started before the kill may already have run. Claude Code's isolation rests on
tools being absent, which Flint can check before anything is sent.

**Revisit when** `codex exec` can disable tools. The opt-in entry already stores which CLI it was given for, so a
second CLI needs no settings migration.

## Redaction

The log redactor is new. It joins the existing `flint_core::redact` module, which already holds Secret masking and
error-message redaction shared by the app and `flint mcp`. It replaces:

- **Credential shapes**, in `key=value`, JSON, YAML and header form: `password`, `passwd`, `token`, `secret`,
  `api_key`, `access_key`, `client_secret` and similar keys; `Authorization: Basic` and `Bearer`; `X-Api-Key`;
  `Cookie` and `Set-Cookie`.
- **Connection strings:** passwords in URLs (`user:pass@`), `AccountKey=`, `SharedAccessSignature=`.
- **Vendor-prefixed tokens:** `ghp_`, `github_pat_`, `xoxb-`/`xoxa-`/`xoxp-`, `sk_live_`, `sk-`, `AIza`, `glpat-`,
  `AKIA`/`ASIA` access key IDs, and JWTs.
- **Private keys:** PEM blocks, including PEM with escaped `\n` inside JSON, and the `private_key` of GCP service
  account JSON.
- **Known values:** the values of Secrets the pod references (env `secretKeyRef`, `envFrom`, secret volumes), with
  their base64, URL-encoded and JSON-escaped forms; literal `env[].value` from the pod spec; and values from
  ConfigMaps the pod references. Only values of 8 or more characters are matched, at token boundaries.

Known values are read when the user clicks Analyze. That is a cluster read, made only then. If the user may not read
those Secrets or ConfigMaps, the confirmation says the values could not be checked. Matching uses Aho-Corasick for
known values and a `RegexSet` for shapes. Known values are kept in memory for that send only, never logged, and
zeroized afterwards.

**Limits.** Secrets injected outside the pod spec (Vault agents, CSI drivers, External Secrets written to files) are
unknown to Flint and caught only if they match a shape. Personal data is out of scope. The confirmation is the last
check, so the user sees everything before it is sent.

## Considered Options

- **MCP only:** the user asks their agent to read the logs through `flint mcp`. This already works and stays. But it
  needs the cluster exposed, a separate agent window, and the user has to describe which lines they mean. The button
  sends exactly the lines on screen.
- **Built-in API keys:** Flint calls a provider's API with a key the user pastes in. Rejected. Flint would store
  secrets, pick providers and models, and maintain an API client. That is what [ADR-0009](./0009-read-only-mcp-server.md)
  already ruled out.
- **The user's local CLI (chosen, Claude Code first):** no secrets in Flint, the user's own account, model choice and data policy, and
  little code. The cost is a dependency on the CLIs' flags and event formats, which can change.
- **Claude's `--bare` mode:** it skips hooks, plugins and MCP servers in one flag, but it never reads the user's
  subscription login and needs `ANTHROPIC_API_KEY`. Rejected, because most users sign in without a key.

### Reuse Expose to AI or a separate opt-in?

A separate opt-in. Expose to AI gives any MCP client a standing read of the whole cluster. Log analysis sends a few
lines the user picked, each time they ask. If one toggle covered both, a user who wants to analyze one production
log line would have to let agents read all of production. The two opt-ins share their storage shape and the
re-pointing check, so the code stays small.

## Consequences

- The redactor gets unit tests for every pattern and every encoded form.
- The flags and minimum version are kept in one place. When Claude Code changes its flags or events, analysis fails
  closed until Flint is updated.
- The prompt never includes the cluster name, the server URL, or kubeconfig data. It carries the namespace, pod,
  container, owner, phase, restart count and last termination reason.
- The CLI's account, provider and data retention are the user's. Flint names the CLI and its path in the
  confirmation, so the user knows where the lines go.
