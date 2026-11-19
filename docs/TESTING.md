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

### Automation channel

A test-only control socket lets e2e tests and agents drive the real app.

- **Built only with `--features automation`.** Release builds contain no automation code. CI's release-build job checks
  that the symbol is absent.
- **Socket location:** a Unix socket in `FLINT_CONFIG_DIR`, readable and writable only by its owner (mode `0600`). The
  app refuses to start the channel when `FLINT_CONFIG_DIR` is not set, so it never uses the user's real config.
- **Protocol:** newline-delimited JSON. A request is `{"id", "cmd", "args"}`. Commands:
  - `select_cluster`
  - `open_view`
  - `select_row`
  - `dispatch_action` (any registered action, such as menu items or palette commands)
  - `type_text`
  - `press_key`
  - `state` (returns the visible view tree as JSON: views, rows, banners, focus)
  - `wait_for` (a predicate on `state`, with a timeout)
  - `screenshot` (captures the app window)
- The automation channel calls the same code paths as clicks and keys, never shortcuts into the core. Otherwise a
  passing e2e test proves nothing about the UI.

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
| `test (<crate>)` | macOS | Unit and view tests, one job per crate |
| `cluster-e2e` | Linux | kind via `helm/kind-action`. Core and MCP cluster tests (`--include-ignored`) and MCP e2e |
| `app-e2e` | macOS | `flint-e2e` app tests against the fake API server. Uploads screenshots (light and dark) as artifacts |

`cluster-e2e` needs `flint-core` and `flint-mcp` to build on Linux, which is part of the portability goal.
`app-e2e` runs on PRs. `cluster-e2e` runs on PRs that touch `flint-core` or `flint-mcp`, on master, and nightly.

**The MCP server without GPUI.** The `flint-mcp` crate has a binary target, `flint-mcp`, that serves the same
`flint_mcp::run_stdio` as `flint mcp` and links no GPUI. The MCP e2e tests (`crates/flint-mcp/tests/mcp_stdio.rs`)
drive that binary, so they run on Linux. A smoke test in `flint-app` checks that `flint mcp` reaches the same server.
Users still run `flint mcp`; the release ships only `flint`. A separate binary was chosen over a cargo feature on
`flint-app` because it keeps GPUI out of the Linux build graph entirely, with no feature combinations to test.

**Cluster setup.** `scripts/cluster-fixtures.sh` installs metrics-server and the `demo` fixtures on a kind cluster.
`scripts/dev-cluster.sh up` and the `cluster-e2e` job both call it, so local runs and CI test the same cluster. To run
the cluster tests locally:

```sh
scripts/dev-cluster.sh up
KUBECONFIG=.dev/kubeconfig FLINT_TEST_KUBECONFIG=.dev/kubeconfig FLINT_CONFIG_DIR="$(mktemp -d)" \
  cargo test --workspace -- --include-ignored
```

### Screenshots

App e2e saves a screenshot of every key view in light and dark (`FLINT_APPEARANCE`). CI uploads them as a workflow
artifact and links them from a PR comment. They are evidence for reviewers, not a gate: font rendering differs between
runner images, so pixel diffs would be flaky.

## Safety

Tests never touch real clusters. Cluster tests and e2e tests require `FLINT_TEST_KUBECONFIG` or the fake server, and
always set a scratch `FLINT_CONFIG_DIR`. The automation channel refuses to run without one.
