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
  - `KUBECONFIG` is set, so the app never falls back to `~/.kube/config`. Without it, `HOME` must be a scratch
    directory (not the account's home, from the user database) with no `.kube/config`, so there is nothing to fall
    back to (UP-02's "no kubeconfig" test).
  - With `FLINT_AUTOMATION_EXIT_ON_STDIN_EOF=1` the app quits when its stdin closes, so it never outlives the
    harness that started it.
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
    `kubeconfig` (`{loaded, paths, env}`), `clusters` (with each one's `label`, `connection`, and `presence`: `live`
    only for the watched cluster, `idle` for one connected but not watched), sidebar `banners`, `active_cluster`,
    `main` (`no_contexts` when no kubeconfig file had a context), `header` (`namespace_button` and `watch_mode` are
    null while hidden, as on a failed cluster; `watch_mode_source`: `default` or `cluster`), `nav` (sections, and the
    entries of open ones), `list` (`count` as the toolbar shows it, columns as drawn and up to 500 rows in display
    order, each with `cells` and `cells_by_column`, Secret cells masked, with its `banners` and `placeholder`) and `detail` (which object, tab, fetch state, and its sections with
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

A Rust crate of helpers and integration tests: the fake API server (below), and these.

- **`App`** (`flint_e2e::app`): runs `flint` built with `--features automation` (see "Binaries" below) with:
  - a scratch `FLINT_CONFIG_DIR` (mode 0700, under `/tmp` so the socket path stays short) and a scratch `HOME`;
  - a `settings.toml` with a fixed `path` (no login shell);
  - exactly the `KUBECONFIG` the test gives, or none with `AppOptions::no_kubeconfig()` (the app then looks in the
    scratch `HOME`, finds nothing, and the automation channel allows that only because `HOME` is not the account's
    home and has no `.kube/config`). Inherited `KUBECONFIG` and `FLINT_*` variables are dropped, and
    `AppOptions::env` refuses `KUBECONFIG`, `HOME`, `FLINT_CONFIG_DIR` and `FLINT_AUTOMATION_*`.

  It waits for the socket, and fails at once with the app's log if the channel refuses to start (exit code 3).
  Typed calls, one connection each: `select_cluster`, `open_view`, `select_row`, `dispatch` (an action by name),
  `type_text`, `press_key`, `state`, `actions`, `wait_for(pointer, Test::…)` (20 s) and `wait_for_within`,
  `screenshot(name)` and `screenshot_themes(name)` (light and dark, through the View > Appearance actions). A failed
  call's error shows the channel's message, the last state (trimmed) and the end of the app's log, so a test that
  fails through `?` explains itself.
- **The app never outlives the test.** It runs in its own process group with stdin on a pipe the harness holds, and
  `FLINT_AUTOMATION_EXIT_ON_STDIN_EOF=1` makes it quit when that pipe closes, even if the test process is killed.
  Dropping an `App` asks it to quit (2 s), then kills the group, and prints the end of its log (libtest shows it for
  failing tests). Scratch directories record their owner's pid, and the next run removes those of dead processes.
  `App::quit` keeps the scratch directories so `AppOptions::reuse` can relaunch on them. Only one `App` runs at a time
  in a test process; `App::launch` waits for the previous one to be dropped.
- **`Mcp`** (`flint_e2e::mcp`): `Mcp::start` runs `flint-mcp`; `Mcp::flint` runs `flint mcp` from the app binary,
  for tests that check the app and the server agree. Both get the given kubeconfig (an empty one is refused), the
  given config directory (with a fixed-`path` `settings.toml` if it has none) and a scratch `HOME`. JSON-RPC over
  stdio: `request`, `tools`, `call`, `call_json`.
- **`Backend`** (`flint_e2e::backend`): `Backend::Fake` starts a fake API server. `Backend::Kind` reads
  `FLINT_TEST_KUBECONFIG`, refuses a current context whose server is not `https` on 127.0.0.1, localhost or ::1
  (parsed as a URL, no user info), and gives the app a scratch copy holding only that context, its cluster and its
  user. Without the variable, Kind tests are skipped with a printed message. Tests that only browse use
  `Backend::from_env()`, so `FLINT_E2E_BACKEND=kind` runs them on kind; tests that script watches or inject faults
  use the fake server.
- **Tests are named after user paths** (`up_03_connect_to_a_cluster`, `up_20_browse_pods_with_live_updates`, …) and
  tagged `// covers UP-xx`. `tests/app/` holds the app paths (`m0.rs`, `m1.rs`), `tests/mcp.rs` the MCP paths. Paths
  left out say why at the top of their file: UP-05 (Finder launch, real cloud auth: manual), UP-14 to UP-16 (not
  built yet), UP-26 (a load test against kind), and UP-13's live switch with macOS (a test must not change the
  system appearance; dark mode itself is tested).
- **Keys:** tests dispatch registered actions by name rather than pressing shortcuts, which differ by platform, and
  press only plain keys (`escape`, `backspace`).

**Binaries.** `FLINT_E2E_APP_BIN` names a prebuilt `flint` (built with `--features automation`, without
`--release`) and `FLINT_E2E_MCP_BIN` a prebuilt `flint-mcp`. Without them, the first test that needs one runs
`cargo build --locked` for it (once per test process, announced on stderr). That first build of the app takes
minutes; later runs are incremental. CI builds first and sets the variables.

**Running app e2e locally.** They open a window, so they need macOS and a logged-in GUI session (a locked screen is
fine: screenshots render offscreen). They are `#[ignore]`d, and the file is `cfg(target_os = "macos")`, so plain
`cargo test -p flint-e2e` (and Linux CI) runs only the MCP and fake-server tests.

```sh
# Against the fake API server (about a minute, after the first build).
FLINT_AUTOMATION_SCREENSHOTS="$PWD/target/e2e-screenshots" \
  cargo test -p flint-e2e --test app -- --ignored

# The browsing paths against the kind dev cluster instead (scripts/dev-cluster.sh up).
FLINT_E2E_BACKEND=kind FLINT_TEST_KUBECONFIG="$PWD/.dev/kubeconfig" \
  cargo test -p flint-e2e --test app -- --ignored

# With a prebuilt binary, as CI runs them:
cargo build -p flint-app --features automation
FLINT_E2E_APP_BIN="$PWD/target/debug/flint" cargo test -p flint-e2e --test app -- --ignored
```

Screenshots are named `upXX-<view>-light.png` and `upXX-<view>-dark.png`; a rerun replaces the previous run's
files. Without `FLINT_AUTOMATION_SCREENSHOTS` they go to the scratch config directory and are deleted with it.

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

The `demo` namespace (`scripts/cluster-fixtures.sh`) gives the views variety: running, ImagePullBackOff,
CrashLoopBackOff and Completed pods with their Warning events, Deployments, a ReplicaSet history, a Job and a CronJob, a
StatefulSet with a PVC, Services, an Ingress, the podinfo Helm release Secrets, and the CRD `gadgets.fixtures.flint.dev`
(printer columns Size, Color, Age) with one instance. Objects that are listed but not recorded (a kube-system pod) are
served as a minimal object built from the row.

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

**Re-recording** (after bumping `NODE_IMAGE`, see "Pinned versions" below, or changing `scripts/cluster-fixtures.sh`):
recreate the dev cluster (`scripts/dev-cluster.sh down` then `up`), wait until `crash-loop` is in CrashLoopBackOff
with a few restarts, `broken-image` in ImagePullBackOff and `fixture-job` complete, re-record, then run
`cargo test -p flint-e2e`. Review the fixture diff: new columns or kinds are expected; Secret values, local paths and
namespaces left by other tests are not.

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

### Memory leaks

Rust's ownership rules make classic leaks rare, so Flint checks for the kinds that remain:

- **Views that are never released** (a callback or task holding a strong entity handle, a reference cycle).
  View tests that close something assert it was freed: take `cx.leak_detector_snapshot()` before opening a list,
  detail pane, dock tab or terminal, close it, run until parked, then `cx.assert_no_new_leaks(&snapshot)` (or
  `weak.assert_released()` for one entity). Every view that can be closed gets such a test; the dock's, the log tab's and the shell
  tab's are in `ui/dock_tests.rs`. CI builds the app
  tests with `GPUI_LEAK_DETECTION=1`, so a failure lists each leaked handle; locally add `LEAK_BACKTRACE=1` to
  see where it was created. Never capture `cx.entity()` in a frame, timer or task callback: capture
  `cx.weak_entity()` and `update` it, so a closed view can be dropped.
- **Leaks and memory errors in native code** (the C libraries GPUI and kube link, and Flint's few `unsafe`
  blocks): the nightly `Memory` workflow runs the `flint-core` and `flint-mcp` tests under AddressSanitizer
  with LeakSanitizer on Linux. Run it by hand with "Run workflow", or locally on Linux with
  `RUSTFLAGS=-Zsanitizer=address cargo +nightly test -p flint-core -p flint-mcp --lib --tests --target x86_64-unknown-linux-gnu`
  (on macOS, in a Linux container with `--target aarch64-unknown-linux-gnu`; set `ASAN_SYMBOLIZER_PATH` to an
  `llvm-symbolizer` for readable stacks). The `flint-mcp` tests spawn the instrumented binary and check its exit
  status, so a leak in the server process fails them too. Tests must not leak either: never `Box::leak` a
  fixture to get a `&'static str`; have the fake own `String`s instead. Fix a leak rather than suppress it; suppress only
  a third-party or process-lifetime allocation Flint cannot free, with a narrow `leak:<symbol>` line and a comment
  saying why in `.github/lsan-suppressions.txt` (none is needed today; `memory.yml` would pass it with
  `LSAN_OPTIONS=suppressions=…`), never a wildcard.
- **Memory that keeps growing** (watch caches, log buffers, terminal scrollback, queues): a soak test runs the
  app against the fake API server with lists, log tabs and an exec session open, samples the process's
  memory, and fails if it is still climbing after warm-up. It runs nightly, not on PRs.

### Where things run in CI

| Job | Runner | Runs |
|---|---|---|
| `test (<crate>)` | Linux | Unit tests for `flint-core`, `flint-mcp` and `flint-e2e` (the fake API server, the harness, and the MCP e2e tests against the fake server, with `flint-mcp` built first and named by `FLINT_E2E_MCP_BIN`), one job per crate |
| `linux (flint-app)` | Linux | The app's clippy and view tests on GPUI's Linux backend (headless test platform, no display). Flint ships for macOS only; this is a cheap second check |
| `leak-sanitizer (core, mcp)` | Linux, nightly | `flint-core` and `flint-mcp` tests under AddressSanitizer and LeakSanitizer (`memory.yml`) |
| `macos (flint-app)` | macOS | The app's clippy, view tests and docs, the same with `--features automation`, and a release build checked for automation code. A step in it runs the `flint-e2e` app tests (`--test app -- --ignored`) against the fake API server and uploads the screenshots (light and dark) as artifacts ([Sealos/flint#42](https://github.com/Sealos/flint/issues/42)). The only CI job that needs macOS |
| `cluster-e2e` | Linux | kind via `helm/kind-action`. Core and MCP cluster tests (`--include-ignored`) and MCP e2e |

`cluster-e2e` needs `flint-core` and `flint-mcp` to build on Linux, which is part of the portability goal.
App e2e runs on PRs, inside `macos (flint-app)`. `cluster-e2e` runs on PRs that touch `flint-core` or `flint-mcp`, on master, and nightly.

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

App e2e saves a screenshot of every key view in light and dark (`App::screenshot_themes`, through the View >
Appearance actions; `FLINT_APPEARANCE` forces one theme at launch). CI uploads them as a workflow
artifact and links them from a PR comment. They are evidence for reviewers, not a gate: font rendering differs between
runner images, so pixel diffs would be flaky.

## Safety

Tests never touch real clusters. Cluster tests and e2e tests require `FLINT_TEST_KUBECONFIG` or the fake server, and
always set a scratch `FLINT_CONFIG_DIR`. The automation channel refuses to run without one.
