# Flint: rules for every agent

Flint is a native Kubernetes desktop manager in Rust and GPUI, with a read-only MCP server (`flint mcp`).
GitHub repo: `Sealos/flint`. Start with `docs/ARCHITECTURE.md`, `docs/adr/`, `docs/USER-PATHS.md` (UP-xx ids) and
`docs/TESTING.md`; the roadmap is issue #27.

## Safety (never break these)

- Treat the default kubeconfig (`~/.kube/config`) as production. Never run kubectl, helm, Flint or tests against it.
- For anything that touches a cluster, set `KUBECONFIG=<main checkout>/.dev/kubeconfig` (the kind cluster that
  `scripts/dev-cluster.sh` creates) and a temporary `FLINT_CONFIG_DIR`. Work only in namespaces you create, and delete
  them afterwards.
- Never change the git identity. Never add AI attribution to commits or PRs.
- In issues, PRs and docs, never call the project owner "the user".

## Working tree and builds

- Work in a sibling worktree, never in the main checkout:
  `git fetch -q && git worktree add -b <branch> ../flint-<topic> origin/master`.
  When a branch is merged, remove its worktree and its `target/`.
- Builds are heavy. Before you start one, check whether another cargo build is running. If one is, wait for it or
  lower `CARGO_BUILD_JOBS`.
- A read-only reviewer builds in a scratch `CARGO_TARGET_DIR` under the system temp dir, and deletes it when done.
- Screenshots: shrink them before reading, to about 1400 px on the long side. A full-size Retina PNG costs hundreds of
  KB of context.

## Gates (run them all, chained with `&&` under `set -o pipefail`)

1. `cargo fmt --all --check`
2. `cargo clippy -p flint-core -p flint-mcp -p flint-e2e --all-targets --locked -- -D warnings`
3. `cargo clippy -p flint-app --all-targets --features automation --locked -- -D warnings`
4. `cargo test` for the crates you touched (`-p flint-app --features automation` for app code)
5. `RUSTDOCFLAGS=-Dwarnings cargo doc -p flint-core -p flint-mcp -p flint-e2e --no-deps --document-private-items --locked`
6. `typos`
7. `scripts/check-module-tests.sh`
8. For app code, also `scripts/check-theme-colors.sh` and `scripts/check-click-actions.sh`.

## Commits and PRs

- Use Conventional Commits; the PR title is checked by `pr-title.yml`.
- An implementing agent commits but does not push. It writes the PR body to `../flint-<topic>-PR.md`, with
  `Closes #N`, and reports the commit SHAs.
- A reviewer is read-only. It reports confirmed problems, each with file:line and a concrete failure scenario, or the
  word "clean".
