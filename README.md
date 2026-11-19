# Flint

Flint is a native Kubernetes desktop manager for macOS, in the space of Lens and Freelens. It is written in Rust
with [GPUI](https://www.gpui.rs/) so it starts fast, stays light on memory, and keeps up with large clusters.

It also ships a read-only [MCP](https://modelcontextprotocol.io/) server, `flint mcp`, so AI agents can inspect the
clusters you choose to expose.

Status: early development (milestone M0). The app lists your kubeconfig contexts, connects to the one you select,
and shows its server version.

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

CI (`.github/workflows/ci.yml`, macOS) runs format, clippy and the tests above without a cluster.

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

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Architecture decision records](docs/adr/)

## License

Apache-2.0. See [LICENSE](LICENSE).
