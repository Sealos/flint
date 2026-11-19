# Rust and GPUI for the whole app

Flint exists to be fast and light, which Electron-based Lens and Freelens are not. It ships on macOS first, and
Linux and Windows should be possible later. So the whole app is written in Rust and the UI uses GPUI, Zed's
GPU-rendered toolkit, together with `gpui-component`. One codebase can reach all three platforms, `kube` is a mature
Kubernetes client, and GPUI's virtualized lists stay smooth with tens of thousands of rows.

## Considered Options

- **Swift/AppKit UI over a Rust core (UniFFI):** the most Mac-like result, but Linux and Windows would each need
  another UI.
- **Pure Swift:** Swift Kubernetes clients are immature, there is no Helm SDK, and the app could never leave macOS.
- **C++ Qt/QML:** mature, but heavier, LGPL, and it would need a hand-written Kubernetes client.
- **Tauri:** the UI still runs in a webview, which works against the reason for the project.

## Consequences

GPUI is young and its API changes. Pin a git version and upgrade on purpose. Keep Kubernetes logic out of the UI
crate (`flint-core` must not depend on GPUI), so that replacing the toolkit would affect only `flint-app`.
