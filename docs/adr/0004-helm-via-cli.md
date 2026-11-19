# Helm through the user's `helm` CLI, not an SDK

Helm's SDK only exists in Go, and Flint's core is Rust. For listing, history, values, upgrade, rollback, and
uninstall, Flint runs the user's `helm` binary with `--kube-context`, JSON output where Helm supports it, and
`KUBECONFIG` set to the same files Flint loaded. This needs
very little code, always matches the Helm version the user already uses, and keeps Flint a single-language,
single-binary app. If `helm` is not found on the resolved `PATH`, the Helm section shows an install hint.

## Considered Options

- **Go sidecar linking `helm.sh/v3`:** removes the dependency on an installed `helm`, but adds a second language,
  a second binary, and an IPC protocol.
- **Decoding Helm release Secrets in Rust:** works for reading releases, but writes would still need the CLI, so
  there would be two code paths.
