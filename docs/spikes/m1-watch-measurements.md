# M1 watch manager: load measurements

Done-when numbers for [Sealos/flint#8](https://github.com/Sealos/flint/issues/8) (UP-26). Measured on 2026-11-19
against the throwaway kind cluster `flint-dev` (Kubernetes **v1.37.0**, one node), on an Apple M4 Pro, release build.

## Setup

- Namespace `m1-load` with **3,000 ConfigMaps** (two labels, two short data keys each) and a Deployment `load` with
  **200 replicas** of `registry.k8s.io/pause:3.10` (`terminationGracePeriodSeconds: 0`). kind's node allows 110 pods,
  so about 100 were Running and 100 Pending; all 200 are objects that the pod watch carries.
- The core ran with watch mode **All**, with the pods and ConfigMaps lists subscribed (as if the views were open).
- Test: `crates/flint-core/tests/watch_load.rs` (`measure_all_mode_during_scale_down`, ignored by default):

  ```sh
  FLINT_TEST_KUBECONFIG=.dev/kubeconfig FLINT_LOAD_NAMESPACE=m1-load \
    cargo test --release -p flint-core --test watch_load -- --ignored --nocapture
  ```

  RSS is the test process (`ps -o rss`): the core runtime, kube client, all stores, and the test harness. CPU is
  the process's user plus system time (`getrusage`). The setup and cleanup ran through `kubectl`, outside that process.

## Results

Two runs (the second after the review fixes, on a freshly created namespace):

| Measure | Run 1 | Run 2 |
|---|---|---|
| Kinds watched in All mode | 62 | 62 |
| Rows held for the two open lists | 213 pods, 3,015 ConfigMaps | same |
| Pods and ConfigMaps synced after activation (streaming lists) | 123 ms | 118 ms |
| RSS before activation | 7 MiB | 7 MiB |
| RSS with every kind synced | 19 MiB | 19 MiB |
| CPU for activation and all initial lists | 72 ms | 81 ms |
| Scale-down 200 → 0: last pod gone after | 8.6 s | 8.6 s |
| Pods view notifications during the scale-down | **84** | **85** |
| Ceiling at one notification per 100 ms over that window | 87 | 88 |
| CPU during the scale-down | 250 ms | 346 ms (about 3–4% of one core) |
| RSS peak during the scale-down | 19 MiB | 19 MiB |

The spike counted about 330 watch events for a 200-pod scale-down. The view was told about a change 84–85 times, never
more than once per 100 ms, and every notification carried a complete snapshot. Before #8 the UI would have
re-rendered once per event.

## Reading the numbers

- **Memory:** about 12 MiB for 62 watched kinds and roughly 3,500 rows in all. Rows keep only identity, labels and
  cells (interned per kind). `managedFields`, annotations and owner references are never stored. On a real cluster,
  the cost per row grows with labels and cell text, not with object size.
- **CPU:** the scale-down cost is mostly JSON parsing of watch events and rebuilding the pods snapshot (213 rows,
  sorted) at most 10 times a second. Snapshots are built only for kinds with an open subscription. Kinds watched
  because of All mode with no open view pay only for parsing.
- **Not measured here:** 10,000 pods (kind's 110-pod node limit), a real cluster's event churn in All mode, and UI
  frame times (the UI is not built yet). Repeat with the UI once #9 lands.
