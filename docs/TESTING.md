# Testing

How Flint is tested, from single functions up to the running app. [`USER-PATHS.md`](./USER-PATHS.md) lists *what*
must keep working. This document covers *how* it is tested. The choices behind it are in
[ADR-0010](./adr/0010-testing-layers.md).

## Layers

| Layer | What it proves | Runs where | Gating |
|---|---|---|---|
| **Unit** | One module's logic in isolation | `cargo test`, every PR (one CI job per crate) | Yes |
| **View** | A GPUI view reacts correctly to state, actions and keys | Headless `#[gpui_kit::test]`, every PR | Yes |
| **Cluster** | Core and MCP behave correctly against a real API server | Linux CI with kind; locally with `scripts/dev-cluster.sh` | Yes on master and nightly |
| **App e2e** | The real app, driven like a user, completes a user path | macOS CI against the fake API server; locally against kind | Yes |
| **Screenshots** | Key views look right in light and dark | Captured during app e2e and attached to the PR | No (reviewed by people) |
| **Manual** | Things only a real machine shows: Gatekeeper, real cloud auth | By hand at release | Release checklist |

### Unit tests per module

Every source module has unit tests, either a `#[cfg(test)] mod tests` or a `// module-tests: <file>` line that points to
its test file. `lib.rs`, `main.rs` and `mod.rs` are wiring and are exempt. `scripts/check-module-tests.sh` enforces
this in CI. `scripts/module-tests-allowlist.txt` lists the modules that predate the rule. The list may only shrink.

### View tests

Headless GPUI tests (`#[gpui_kit::test]`, `TestAppContext`/`VisualTestContext`) build a view on top of a real
`CoreHandle` or a fake one. They dispatch actions and keystrokes, then assert on the view's state: rows, selection,
banners, enabled controls. They don't render pixels and don't need a screen, so they run in normal CI. Every view
module gets at least one.

Whole-window tests (`ui/workspace_tests.rs`, helpers in `ui/support_tests.rs`) open the workspace over two kubeconfig
contexts on 127.0.0.1: one whose server never answers (so its views stay up) and one that refuses connections. They
drive the app through registered actions, clicks and keys, and check that the three agree. Every click handler must
perform a registered action; `scripts/check-click-actions.sh` enforces this in CI.

### Automation channel

A test-only control socket lets e2e tests and agents drive the real app. The code is in
`crates/flint-app/src/ui/automation/`.

- **Test builds only.** Built only with `--features automation`, and only without `--release` (a `compile_error!`
  refuses both together). CI's `macos (flint-app)` job runs flint-app's clippy and tests with the feature, and checks
  that its release binary has none of the channel's strings (the binary is stripped, so there are no symbols to look
  for).
- **Where it may run.** Before anything starts, the app checks, and exits with code 3 and a reason if any check fails,
  so a harness fails fast instead of waiting for a socket:
  - `FLINT_CONFIG_DIR` is set, is not the real config directory, belongs to the user and is not writable by group or
    others. It is created with mode `0700` when missing.
  - `KUBECONFIG` is set, so the app never falls back to `~/.kube/config`.
  - The socket path, `$FLINT_CONFIG_DIR/automation.sock`, fits macOS's limit of 103 bytes (`sun_path` is 104 with its
    NUL), so keep `FLINT_CONFIG_DIR` short (`mktemp -d` is fine).
  - No other Flint is serving on that socket. A stale socket nobody answers on is replaced.
- **The socket.** Mode `0600`. It is bound in a fresh private directory (`mkdtemp`, `0700`) and moved into place with
  link and unlink, so nobody can connect before its mode is set and an existing file is never replaced. Connections from
  another user are refused. On quit the app removes it, if it is still the one it made.
- **Protocol:** newline-delimited JSON. A request is `{"id", "cmd", "args"}`; `id` is echoed back and `args` may be
  left out when a command takes none. A response is `{"id", "ok": true, "result"}` or
  `{"id", "ok": false, "error": {"code", "message", "data"?}}`, with `code` one of `bad_request`, `unknown_command`,
  `invalid_args`, `not_found` (no such action), `not_available` (nothing handles it now, or it is disabled),
  `rejected` (available, but the arguments would do nothing: an unknown cluster, kind, row or column), `timeout`,
  `failed`. At most 16 requests are in flight at once; more get `failed` at once. Commands:
  - `select_cluster {cluster}`: the sidebar click.
  - `open_view {kind}`: the navigation click. `kind` matches like `FLINT_VIEW`: `pods`, `deployments.apps`.
  - `select_row {namespace?, name}`: the row click, among the rows passing the filters.
  - These three dispatch `workspace::SelectCluster`, `workspace::OpenView` and `resource_list::SelectRow`, so the
    registry's guards check them exactly as they check a click.
  - `dispatch_action {name, args?}`: any registered action by name (`workspace::SetNamespaces`, `detail::ShowYaml`,
    `flint::AppearanceDark`), and any other GPUI action, such as the dialog's `ui::Confirm`. With nothing focused it
    dispatches from the workspace, as a click would.
  - `actions`: every registered action with its label, arguments (and their kinds), shortcut, and whether it is
    `enabled` and `available` now. Ask it when a dispatch says `not_available`.
  - `type_text {text}`: one keystroke per character (Shift for capitals and shifted symbols), to the focused element.
    At most 4 KiB.
  - `press_key {key}`: a key or chord in GPUI syntax (`escape`, `enter`, `cmd-w`, `?`).
  - `state`: the visible view tree: `focus`, `dialog` (`{title, text, buttons}` while one is open), `appearance`,
    `kubeconfig` (`{loaded, paths, env}`), `clusters` (with each one's `label`), sidebar `banners`, `active_cluster`,
    `main`, `header` (with `watch_mode_source`: `default` or `cluster`), `nav` (sections, and the entries of open ones),
    `list` (columns as drawn and up to 500 rows in display order, each with `cells` and `cells_by_column`, Secret cells
    masked, with its `banners` and `placeholder`) and `detail` (which object, tab, fetch state, and its sections with
    entry counts; never values). Errors in it are sanitized summaries.
  - `wait_for {pointer, <test>, timeout_ms?}`: polls `state` every 50 ms, on the app's clock, until the value at a
    JSON Pointer passes one test, and returns that state. Tests: `equals`, `not_equals`, `at_least`, `at_most`
    (numbers, or an array's length), `exists`, `contains` (a substring, or an array element matching), and `any` (an
    array element matching an object). An object matches when every field it gives matches, so
    `{"pointer": "/list/rows", "any": {"name": "web-1", "cells_by_column": {"Status": "Running"}}}` waits for a running
    row. Default timeout 5 s, at most 120 s. A timeout says what it saw (for a pointer that leads nowhere, the keys
    where it stopped) and carries the last state in `error.data`. A `wait_for` holds its connection until it ends: to
    act meanwhile, use a second connection.
  - `screenshot {name?}`: draws the window for about half a second (so layout and short animations settle), renders
    it offscreen to a PNG named `name` (a plain file name; `.png` is added; a timestamp when left out), and returns
    its path and size in pixels. It goes in `$FLINT_AUTOMATION_SCREENSHOTS` when set (an absolute, existing directory
    the harness owns), else in `$FLINT_CONFIG_DIR/screenshots/`. The file is new (never replacing one, never through
    a link) and mode `0600`. Offscreen rendering needs no screen-recording permission and works while the screen is
    locked.
- Commands that act draw the window before they return (as GPUI does before handling a key), so the next command
  sees the new focus and views even while the window is hidden. They do not wait for data: follow them with
  `wait_for`.
- The automation channel calls the same code paths as clicks and keys, never shortcuts into the core. Otherwise a
  passing e2e test proves nothing about the UI. It reaches controls through the action registry
  (`crates/flint-app/src/ui/registry.rs`), where every interactive control is a named action; the command palette
  uses the same registry. Test builds open dialogs without their entrance animation.

Try it by hand:

```sh
cargo build -p flint-app --features automation
export FLINT_CONFIG_DIR=$(mktemp -d)
KUBECONFIG=.dev/kubeconfig FLINT_CLUSTER=kind-flint-dev target/debug/flint &
echo '{"id":1,"cmd":"open_view","args":{"kind":"pods"}}' | nc -U "$FLINT_CONFIG_DIR/automation.sock"
```

### E2E harness: `crates/flint-e2e`

A Rust crate of integration tests and helpers:

- **`App`:** starts `flint` (built with `--features automation`) with a scratch `FLINT_CONFIG_DIR` and the given
  `KUBECONFIG`. It connects to the socket and exposes typed calls such as `app.open_view("pods")`,
  `app.wait_for(rows >= 2)` and `app.screenshot("pods-dark")`.
- **`Mcp`:** starts `flint mcp` and speaks JSON-RPC over stdio.
- **Tests are named after user paths** (`up_03_connect`, `up_20_live_list`, …) and tag them with `// covers UP-xx`.
- **Each test picks its backend:** `Backend::Kind` (`FLINT_TEST_KUBECONFIG`) or `Backend::Fake` (the fake API
  server).

### Fake API server

GitHub's macOS runners have no Docker, so app e2e in CI talks to a fake API server instead of kind.

- **Recording:** `scripts/record-fixtures.sh` captures real responses from the kind dev cluster into
  `crates/flint-e2e/fixtures/`: discovery, OpenAPI, Table lists, objects and events. Secrets are scrubbed and uids and
  timestamps are normalized. Re-record whenever the supported Kubernetes version changes.
- **Replay:** a small Rust HTTPS server in `flint-e2e` serves those fixtures. It also serves scripted watch streams
  (ADDED, MODIFIED, DELETED, BOOKMARK, and an ERROR 410) and can inject failures (a 403, a slow response, a dropped
  connection), so reconnect and permission paths are testable without a real cluster.
- **Wiring:** the test writes a kubeconfig that points at the fake server's address and its self-signed CA.

### Where things run in CI

| Job | Runner | Runs |
|---|---|---|
| `test (<crate>)` | Linux | Unit tests for `flint-core` and `flint-mcp`, one job per crate |
| `linux (flint-app)` | Linux | The app's clippy and view tests on GPUI's Linux backend (headless test platform, no display). Flint ships for macOS only; this is a cheap second check |
| `macos (flint-app)` | macOS | The app's clippy, view tests and docs, the same with `--features automation`, and a release build checked for automation code. The only CI job that needs macOS |
| `cluster-e2e` | Linux | kind via `helm/kind-action`. Core and MCP cluster tests (`--include-ignored`) and MCP e2e |
| `app-e2e` | macOS | `flint-e2e` app tests against the fake API server. Uploads screenshots (light and dark) as artifacts |

`cluster-e2e` needs `flint-core` and `flint-mcp` to build on Linux, which is part of the portability goal.
`app-e2e` runs on PRs. `cluster-e2e` runs on PRs that touch `flint-core` or `flint-mcp`, on master, and nightly.

**The MCP server without GPUI.** The `flint-mcp` crate has a binary target, `flint-mcp`, that runs the same
`flint_mcp::serve_stdio_main` as `flint mcp` and links no GPUI. The MCP e2e tests (`crates/flint-mcp/tests/mcp_stdio.rs`)
drive that binary, so they run on Linux. A smoke test in `flint-app` checks that `flint mcp` reaches the same server.
Users still run `flint mcp`; the release ships only `flint`. A separate binary was chosen over a cargo feature on
`flint-app` because it keeps GPUI out of the Linux build graph entirely, with no feature combinations to test.

**Cluster setup.** `scripts/cluster-fixtures.sh` installs metrics-server and the `demo` fixtures on a kind cluster and
expires old resourceVersions, as on a cluster that has run for a while: it compacts etcd and restarts the API server, whose
watch caches on a new cluster still reach back to resourceVersion 1. A watch from an old version then gets 410.
`scripts/dev-cluster.sh up` and
the `cluster-e2e` job both use it, so local runs and CI test the same cluster. It refuses any context that is not a
local kind cluster. To run the cluster tests locally:

```sh
scripts/dev-cluster.sh up
KUBECONFIG=.dev/kubeconfig FLINT_TEST_KUBECONFIG=.dev/kubeconfig FLINT_CONFIG_DIR="$(mktemp -d)" \
  cargo test -p flint-core -p flint-mcp --no-fail-fast -- --include-ignored
```

The nightly run also sets `FLINT_TEST_SLOW=1`, so the API-server restart test (UP-27) runs there. Under `CI`, the
live-list latency bound is 5 s instead of 2 s (`FLINT_TEST_LATENCY_MS` overrides it).

**Pinned versions.** The kind node image (`NODE_IMAGE` in `scripts/dev-cluster.sh`, also used by CI), metrics-server
and the podinfo chart (top of `scripts/cluster-fixtures.sh`) and the kind version in the workflow are pinned. To bump
one, change it there, recreate the dev cluster (`scripts/dev-cluster.sh down` then `up`), and run the cluster tests. A
new Kubernetes version also means re-recording the fake API server's fixtures.

### Screenshots

App e2e saves a screenshot of every key view in light and dark (`FLINT_APPEARANCE`). CI uploads them as a workflow
artifact and links them from a PR comment. They are evidence for reviewers, not a gate: font rendering differs between
runner images, so pixel diffs would be flaky.

## Safety

Tests never touch real clusters. Cluster tests and e2e tests require `FLINT_TEST_KUBECONFIG` or the fake server, and
always set a scratch `FLINT_CONFIG_DIR`. The automation channel refuses to run without one.
