# Multi-cluster set: kwok measurements

Method for [Sealos/flint#447](https://github.com/Sealos/flint/issues/447): memory and CPU of a 5-member cluster set at
10,000 pods per member, next to [m1-watch-measurements.md](m1-watch-measurements.md). The set runtime (#79) does not
exist yet, so **no 5-member number has been measured**.

## Setup

[kwok](https://kwok.sigs.k8s.io/) runs real API servers with simulated nodes and pods, so 10,000 pods need no kubelet.
`scripts/kwok-set.sh` creates five clusters, `flint-kwok-1` to `flint-kwok-5`, each with 10,000 simulated pods in
`default`:

```sh
scripts/kwok-set.sh create          # 5 clusters, 10,000 pods each; --members N and --pods N change both
scripts/kwok-set.sh report          # pods per cluster, and container CPU and memory when docker is present
scripts/kwok-set.sh delete          # removes only flint-kwok-1 to flint-kwok-5
```

Safety: the script writes only to its own scratch kubeconfig (`FLINT_KWOK_KUBECONFIG`, default
`.dev/kwok-set/kubeconfig`), refuses `~/.kube/config` and `.dev/kubeconfig`, ignores the caller's `KUBECONFIG`, uses
only the contexts it created (`kwok-flint-kwok-N`) and deletes only clusters with its own name prefix.

## Method

1. Create the set and check that `report` shows 10,000 pods in each cluster.
2. Start the app or test harness with `KUBECONFIG` set to the scratch file and a scratch `FLINT_CONFIG_DIR`, with the
   five contexts as set members (needs the set runtime).
3. Record, once the lists are synced and again during churn (scale one cluster's pods down and up):
   - RSS of the Flint process (`ps -o rss`), before activation and with all five members synced.
   - CPU as user plus system time over activation and over the churn window.
   - Time until every member's pods list is synced.
4. Record the host (chip, memory), the Flint commit and the kwok version. Measure the kwok clusters' own memory
   separately (`report`), so it is not counted as Flint's.

Check free memory before creating 50,000 simulated pods; five API servers and their stores are not small.

## Results

| Measure | 1 member | 5 members |
|---|---|---|
| Pods synced after activation | not measured | **not measured** (needs the set runtime) |
| RSS with every member synced | not measured | **not measured** |
| CPU for activation and initial lists | not measured | **not measured** |
| RSS and CPU during churn | not measured | **not measured** |

Host, commit and kwok version: _to be filled in with the measurement._

No baseline was run when this was written: `kwokctl` was not installed on the machine, and the script was checked
only with `bash -n` and `shellcheck`.
