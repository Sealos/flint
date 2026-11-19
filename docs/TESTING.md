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

A Rust crate of integration tests and helpers. The fake API server (below) is there today; `App` and `Mcp` follow
in [Sealos/flint#40](https://github.com/Sealos/flint/issues/40).

- **`App`:** starts `flint` (built with `--features automation`) with a scratch `FLINT_CONFIG_DIR` and the given
  `KUBECONFIG`. It connects to the socket and exposes typed calls such as `app.open_view("pods")`,
  `app.wait_for(rows >= 2)` and `app.screenshot("pods-dark")`.
- **`Mcp`:** starts `flint mcp` and speaks JSON-RPC over stdio.
- **Tests are named after user paths** (`up_03_connect`, `up_20_live_list`, …) and tag them with `// covers UP-xx`.
- **Each test picks its backend:** `Backend::Kind` (`FLINT_TEST_KUBECONFIG`) or `Backend::Fake` (the fake API
  server).

### Fake API server

GitHub's macOS runners have no Docker, so app e2e in CI talks to a fake API server instead of kind. It lives in
`flint_e2e::fake_api`, and its tests run in plain `cargo test` (no cluster, on Linux in CI).

A test, start to finish:

```rust
let server = FakeApiServer::start().await?;
server.write_kubeconfig(&dir.join("kubeconfig"), Some("demo"))?; // context "flint-e2e-fake"
let core = /* CoreHandle on that kubeconfig, WatchTiming shortened */;
let configmaps = server.gvr("configmaps");
let mut sub = core.subscribe(CONTEXT, &KindId::new("", "configmaps"));
server.wait_for_watches(&configmaps, 1, WAIT).await?;          // the list is live
let fresh = server.new_row(&configmaps, Some("demo"), "fresh", &[("Data", "1")]);
server.send_watch(&configmaps, WatchStep::Added(fresh));        // ADDED to every open watch
until(&mut sub, |s| s.rows.iter().any(|r| &*r.name == "fresh")).await;
server.inject(Fault::forbidden().on(&server.gvr("secrets")).verb(Verb::List).cluster_wide());
server.send_watch(&configmaps, WatchStep::Expired);            // 410: the core lists again
```

`crates/flint-e2e/tests/core.rs` has complete versions of these.

**What's in the fixtures** (`crates/flint-e2e/fixtures/`, recorded from the kind dev cluster):

| File | Holds |
|---|---|
| `version.json` | `GET /version` |
| `discovery/api.json`, `discovery/apis.json` | Aggregated discovery (`apidiscovery.k8s.io/v2`). Legacy discovery is not recorded. |
| `openapi/v3.json`, `openapi/v3/apis/apps/v1.json` | The OpenAPI v3 index and one document, descriptions dropped, one line |
| `tables/<group>/<version>/<resource>.json` | The Table list (`includeObject=Metadata`) of every listable kind at its preferred version, all namespaces, row `managedFields` dropped |
| `objects/<group>/<version>/<resource>.json` | Full objects: namespace `demo`, namespaces, nodes, and core events in `default` and `demo` (Table events are limited the same way) |

Objects that are listed but not recorded (a kube-system pod) are served as a minimal object built from the row.

**Recording:** `KUBECONFIG=.dev/kubeconfig scripts/record-fixtures.sh`. It refuses to run unless `KUBECONFIG` names
exactly one file whose current context is `kind-flint-dev` on an `https://` server at 127.0.0.1, localhost or ::1,
with no proxy. The new set is written next to the old one and swapped in only when it is complete, loads, and holds
no credentials, so a failed run changes nothing.

- Secrets are masked with `flint_core::redact::mask_secret` (values become base64 of `redacted`, keys stay), and
  Secret Table cells outside the safe columns are hidden. ConfigMaps labelled `owner=helm` (Helm's ConfigMap storage)
  are masked the same way. Other ConfigMaps are kept: their content is useful to tests, and strings shaped like a
  certificate, key, token or gzip blob are replaced everywhere.
- Uids are derived from each object's group, resource, namespace and name. Resource versions, machine ids and
  container ids are fixed.
- Timestamps are stored as their age at recording time before `2026-01-01T00:00:00Z` ("fixture now"), rounded (10 s
  under a minute, then minutes, hours, days), so re-recording an unchanged cluster gives no diff. The server shifts
  them to its start (`Options::now`) and renders `Age` cells from them, so ages look the same every day. Other
  server-rendered ages (`Last Seen`) are fixed.
- `tests/fixtures.rs` checks the committed fixtures for credentials, local paths and Secret values.

**Bumping Kubernetes:** set the kind node image (`kind create cluster --image kindest/node:<version>`) in
`scripts/dev-cluster.sh`, recreate the cluster
(`scripts/dev-cluster.sh down && scripts/dev-cluster.sh up`), wait for metrics-server to serve, re-record, then run
`cargo test -p flint-e2e`. Review the fixture diff: new columns or kinds are expected, Secret values or local paths are
not.

**Replay:** `FakeApiServer::start()` (or `start_blocking` without a runtime) serves the fixtures over HTTPS on
`127.0.0.1` with a CA generated per server.

- Aggregated discovery, Table or plain lists (by `Accept`), single objects, pod logs (`set_log`), field and label
  selectors, `limit`/`continue`. Writes get 405: the server is read-only. Table lists refuse field selectors other
  than `metadata.name` and `metadata.namespace` with 400.
- Watches, like a real server: without a resource version they start with the current state as `ADDED`; from a
  version they replay the changes since (a short log), or send a 410 if it is older; streaming lists
  (`sendInitialEvents`, which needs `resourceVersionMatch=NotOlderThan`) send the state and, with bookmarks allowed,
  the end marker. Only the first event carries `columnDefinitions`, bookmarks have a fake row, and an expired watch
  is an `ERROR` 410 inside HTTP 200. A `MODIFIED` that leaves a watch's selector arrives as `DELETED`.
- Quirks: metrics.k8s.io lists have no resource version, ignore `limit`, and cannot be watched (405).
  `Options::plain_only` answers chosen resources without Tables (UP-28). `Options::streaming_lists: false` rejects
  streaming lists so clients fall back to list and watch.
- `script_watch` / `script_watch_in` queue steps (`Added`, `Modified`, `Deleted`, `Bookmark`, `Expired`, `Wait`,
  `End`, `Drop`) for the next watch. `send_watch` sends a step to every open watch. `set_row`, `remove_row`,
  `set_object` and `remove_object` change data silently, so only a relist shows it. Objects follow their rows.
- `inject(Fault)` refuses (403 with the real RBAC message, or any status), delays, or drops the connection of
  matching requests (by resource, verb, namespace scope, and count).
- Methods that take a `Gvr` panic on one the fixtures don't serve, with a suggestion. `server.gvr("deployments")`
  looks one up by plural.
- `requests()` records every request, for asserting relists and retries.
- **Wiring:** `write_kubeconfig(path, namespace)` writes a kubeconfig that trusts the server's CA.
  `write_kubeconfigs` puts several servers in one file under their own context names (UP-01, UP-06, UP-24).
- **Timing:** `CoreConfig::watch_timing` shortens the core's retry waits; e2e tests use 10–50 ms.

### Where things run in CI

| Job | Runner | Runs |
|---|---|---|
| `test (<crate>)` | Linux | Unit tests for `flint-core`, `flint-mcp` and `flint-e2e` (the fake API server and its tests), one job per crate |
| `linux (flint-app)` | Linux | The app's clippy and view tests on GPUI's Linux backend (headless test platform, no display). Flint ships for macOS only; this is a cheap second check |
| `macos (flint-app)` | macOS | The app's clippy, view tests and docs, and a release build. The only CI job that needs macOS |
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
