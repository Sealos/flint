# Testing layers: headless view tests, a test-only automation channel, and a recorded fake API server

Flint's UI is tested in two layers. **Headless GPUI view tests** run in every PR and assert on view state. **End-to-end
tests** drive the real app through a test-only automation socket, which is compiled only with
`--features automation` and never shipped. GitHub's macOS runners cannot run Docker, so app e2e in CI uses a **fake
API server that replays responses recorded from kind**. Core and MCP run against real kind on Linux runners. E2E tests
are Rust (`crates/flint-e2e`), and screenshots are review artifacts, not a gate.

## Considered Options

- **macOS accessibility automation (XCUITest, System Events):** GPUI exposes almost no accessibility tree, so elements
  can't be found reliably.
- **Automation in every build behind an env flag:** anyone who can set the user's environment could drive the app,
  including Expose to AI. A cargo feature keeps it out of releases entirely.
- **Pixel-diff golden images:** they go flaky across runner images and font rendering, and every intended change needs
  re-blessing.
- **A self-hosted macOS runner with kind:** gives the most realism, but adds a machine to maintain and a security
  boundary for PRs.
- **A hand-written mock API:** it drifts from what real API servers send.
- **Python e2e:** a second toolchain for no real gain.

## Consequences

- `flint-core` and `flint-mcp` must build on Linux, and `flint mcp` must be buildable without GPUI, for example as
  a separate binary target or a feature that drops the UI.
- Fixtures must be re-recorded when the supported Kubernetes version changes.
- Every UI feature has to register its interactions as actions the automation channel can dispatch. That is also what
  the M1.5 command palette needs.
