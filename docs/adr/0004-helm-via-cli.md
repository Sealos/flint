# Helm through the user's `helm` CLI, not an SDK

Helm's SDK only exists in Go, and Flint's core is Rust. For listing, history, values, upgrade, rollback, and
uninstall, Flint runs the user's `helm` binary with `--kube-context`, JSON output where Helm supports it, and
a kubeconfig of its own written for the run. This needs
very little code, always matches the Helm version the user already uses, and keeps Flint a single-language,
single-binary app. If `helm` is not found on the resolved `PATH`, the Helm section shows an install hint.

## Credentials

Each run gets a private kubeconfig holding only the admitted context, its cluster and its user, with certificate
files replaced by the bytes the connection was built from, and an environment with no `PATH` and a home of its own.

`helm` never starts a sign-in program. A cluster whose user is an exec plugin (EKS, GKE, Azure with kubelogin) is run
only under the background sign-in consent that keep watching uses ([ADR-0012](./0012-keep-recently-used-clusters-warm.md)):

- Flint runs the plugin itself before the run, as it does for a kept cluster: unattended, with no stdin, and checked
  right before it starts. `helm` gets the token or certificate the plugin printed, in place of the plugin.
- The consent is bound to the whole definition (context, user and cluster entries). A change to any of them voids it,
  and turning it off refuses the next run and ends a running one, plugin included. It is never saved.
- Without consent the view refuses the cluster and names the toggle that gives it.

An auth provider (OIDC and the like) is refused: its command cannot be stopped once it runs, so no consent covers it.
So is a user entry that names a program beside another credential, since client-go and kube pick between them in a
different order.

## Considered Options

- **Go sidecar linking `helm.sh/v3`:** removes the dependency on an installed `helm`, but adds a second language,
  a second binary, and an IPC protocol.
- **Decoding Helm release Secrets in Rust:** works for reading releases, but writes would still need the CLI, so
  there would be two code paths.
- **Letting `helm` run the exec plugin:** the plugin would need the user's `PATH`, home and cloud variables in
  `helm`'s environment, and would start where Flint's consent and launch checks cannot reach it.
