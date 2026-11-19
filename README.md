# Flint

Flint is a native Kubernetes desktop manager for macOS, in the space of Lens and Freelens. It is written in Rust
with [GPUI](https://www.gpui.rs/) so it starts fast, stays light on memory, and keeps up with large clusters.

It also ships a read-only [MCP](https://modelcontextprotocol.io/) server, `flint mcp`, so AI agents can inspect the
clusters you choose to expose.

Status: early development (milestone M1). The app lists your kubeconfig contexts, connects to the one you select,
and browses every kind it serves, CRDs included: live lists with the `kubectl get` columns, a namespace and text
filter, and a detail view with metadata, conditions, events and read-only YAML.

## Build and run

You need a recent stable Rust toolchain (edition 2024) and macOS with Xcode command line tools.

```sh
cargo build --release
cargo run -p flint-app            # start the app
```

Flint reads kubeconfig the way `kubectl` does: the files in `KUBECONFIG` if it is set, otherwise `~/.kube/config`,
plus any `kubeconfig_paths` from settings. It never writes to kubeconfig files, and it does not connect to any
cluster until you select one.

Settings live in `~/Library/Application Support/Flint/settings.toml` (or in `$FLINT_CONFIG_DIR` if set):

```toml
kubeconfig_paths = ["~/work/kubeconfig"]
# path = "/opt/homebrew/bin:/usr/bin:/bin"   # override the PATH taken from your login shell
# helm = "/opt/homebrew/bin/helm"
watch_mode = "all"                # or "on_view"

[cluster_watch_mode]
# big-prod-cluster = "on_view"

# Clusters flint mcp may see, written by the sidebar's Expose to AI toggle. Each entry is a
# context name and the API server URL it pointed to when you exposed it; if the context later
# points elsewhere, it is hidden from agents and the sidebar shows "Changed".
# [[ai_exposed_clusters]]
# context = "kind-flint-dev"
# server = "https://127.0.0.1:6443"
```

The app watches `settings.toml` and your kubeconfig files and picks up changes without a restart.

Set `FLINT_LOG=debug` for more logging (to stderr).

`FLINT_CLUSTER=<context>` is a development and testing aid, for example for agents that take screenshots: at startup
Flint activates that context exactly as a click would. It must name a context in the loaded kubeconfig; otherwise Flint
logs a warning and starts with nothing selected. It never selects any other context. Use it only with the dev
cluster:

```sh
KUBECONFIG=$PWD/.dev/kubeconfig FLINT_CONFIG_DIR=$PWD/.dev/flint FLINT_CLUSTER=kind-flint-dev cargo run -p flint-app
```

Two more development aids work together with `FLINT_CLUSTER`, so screenshots can show views nobody clicked to:

- `FLINT_VIEW` opens a view once the cluster's discovery arrives: a kind (`pods`, `deployments.apps`,
  `widgets.example.com`; the group may be left out), `kind/name` for a cluster-scoped object, or
  `namespace/kind/name`. Add `#yaml` to an object to open its YAML tab. Without `FLINT_CLUSTER` it is ignored.
- `FLINT_APPEARANCE=light` or `dark` forces a theme instead of following macOS.

```sh
FLINT_CLUSTER=kind-flint-dev FLINT_VIEW=demo/deployments/web#yaml FLINT_APPEARANCE=light cargo run -p flint-app
```

## Development cluster

`scripts/dev-cluster.sh` manages a throwaway [kind](https://kind.sigs.k8s.io/) cluster named `flint-dev`, with
metrics-server and a sample Helm release. Its kubeconfig is written to `.dev/kubeconfig` (gitignored), so your own
`~/.kube/config` is never touched.

```sh
scripts/dev-cluster.sh up         # create it
export KUBECONFIG=$PWD/.dev/kubeconfig
FLINT_CONFIG_DIR=$PWD/.dev/flint cargo run -p flint-app
scripts/dev-cluster.sh down       # delete it
```

## Tests

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests that need a cluster are marked `#[ignore]`, so a plain `cargo test` reports them as ignored rather than passed.
To run them, pass `--include-ignored` and point `FLINT_TEST_KUBECONFIG` at the dev cluster only (they fail if it is
unset). They also call `kubectl` with that kubeconfig.

```sh
FLINT_TEST_KUBECONFIG=$PWD/.dev/kubeconfig KUBECONFIG=$PWD/.dev/kubeconfig \
  cargo test --workspace -- --include-ignored
```

They create objects only in namespaces named `m1fix-*` (and one CRD in the `m1fix.flint.dev` group),
wait for them to be deleted again, and can be run back to back. Two tests need more and skip with a
message unless asked:

- The load measurement (`tests/watch_load.rs`, UP-26) needs a prepared namespace named by
  `FLINT_LOAD_NAMESPACE`; see its doc comment. Run it in release mode with `--nocapture`.
- The API-server restart test (`tests/watch_recovery.rs`, UP-27) stops and starts the kind control-plane
  container for about half a minute. Run it on its own, with nothing else using the cluster:

  ```sh
  FLINT_TEST_KUBECONFIG=$PWD/.dev/kubeconfig FLINT_TEST_SLOW=1 \
    cargo test -p flint-core --test watch_recovery -- --ignored --nocapture
  ```

CI (`.github/workflows/ci.yml`, macOS) runs format, clippy and the tests above without a cluster.

### Checks CI runs

- **CI (macOS):** `cargo fmt --check`, clippy with `-D warnings`, `cargo test -p <crate>` for each crate as a separate
  job, `cargo doc` with `-D warnings`, and a release build.
- **Lint (Linux):**
  - `cargo deny check` (advisories, licenses, bans, sources; config in `deny.toml`);
  - `cargo machete` (unused dependencies);
  - `typos`;
  - `actionlint`;
  - `scripts/check-module-tests.sh`.
- **PR title:** must be a Conventional Commit, because PRs are squash-merged and release-please reads the subject.
- **Dependabot:** weekly Cargo and GitHub Actions updates. `gpui-kit` is excluded because it is pinned on purpose.

**Unit tests per module:** every source module has unit tests, either a `#[cfg(test)] mod tests` or a
`// module-tests: <file>` line pointing to its test file. `lib.rs`, `main.rs` and `mod.rs` are wiring and are exempt.
`scripts/module-tests-allowlist.txt` lists the modules that predate this rule. The list may only shrink.

## MCP server for AI agents

`flint mcp` serves the Model Context Protocol over stdio. It is read-only: it can list clusters, resource kinds,
objects, events, and pod logs, and nothing else. It only sees clusters you mark **Expose to AI** (the robot button
next to a cluster in the sidebar, which asks you to confirm the context and its server; exposed clusters show an "AI"
badge). Nothing is exposed by default. An exposure is tied to the context's API server URL, so a context that is
re-pointed, or has the same name in another `KUBECONFIG`, is not exposed until you expose it again.
Secret values are always redacted, and large outputs are cut short.

Register it with Claude Code:

```sh
cargo build --release
claude mcp add flint -- "$PWD/target/release/flint" mcp
```

Other MCP clients (Claude Desktop, Cursor) take the same command: `/path/to/flint mcp`.

## Releases

Releases are cut by [release-please](https://github.com/googleapis/release-please) from
[Conventional Commits](https://www.conventionalcommits.org/) on `master`. Commit subjects and PR titles follow that
format, for example `feat(mcp): …` or `fix(core): …`.

1. Every push to `master` updates an open **release PR** with the next version and the `CHANGELOG.md` entries.
   Below 1.0, `feat` bumps the minor version and `fix` bumps the patch.
2. Merging the release PR tags `vX.Y.Z` and creates the GitHub release.
3. The same workflow then builds `flint` for Apple Silicon and Intel, combines them into a universal binary, and
   attaches `flint-vX.Y.Z-macos-universal.tar.gz` and its SHA-256 checksum to the release.

Until M6, release binaries are **unsigned** and marked as pre-release. macOS will warn when you first run them.

The version lives in the workspace `Cargo.toml` (marked `x-release-please-version`). The workflow refreshes
`Cargo.lock` on the release PR, because CI builds with `--locked`.

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Architecture decision records](docs/adr/)

## License

Apache-2.0. See [LICENSE](LICENSE).
