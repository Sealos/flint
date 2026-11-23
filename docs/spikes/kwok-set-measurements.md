# Multi-cluster set: kwok measurements

Memory and CPU of a 5-member running set at 10,000 pods per member, for
[Sealos/flint#447](https://github.com/Sealos/flint/issues/447), next to
[m1-watch-measurements.md](m1-watch-measurements.md). Measured on 2026-11-23.

## Setup

[kwok](https://kwok.sigs.k8s.io/) runs real API servers with simulated nodes and pods, so 10,000 pods need no kubelet.
`scripts/kwok-set.sh` creates five clusters, `flint-kwok-<id>-1` to `-5` (the id comes from the scratch kubeconfig
path), each with 10,000 simulated pods in `default`, using kwokctl's docker runtime:

```sh
export FLINT_KWOK_KUBECONFIG="$(mktemp -d)/kwok-set/kubeconfig"
scripts/kwok-set.sh create          # 5 clusters, 10,000 pods each; --members N and --pods N change both
scripts/kwok-set.sh report          # pods per cluster, and container CPU and memory
FLINT_KWOK_MEMBERS=5 FLINT_KWOK_PODS=10000 \
  cargo test --release --locked -p flint-core --test kwok_set -- --ignored --nocapture
scripts/kwok-set.sh delete          # removes only this kubeconfig's five clusters
```

Safety: the script writes only to its own scratch kubeconfig, refuses `~/.kube/config` and `.dev/kubeconfig`, ignores
the caller's `KUBECONFIG`, uses only the contexts it created and deletes only clusters with its own run prefix. The
test (`crates/flint-core/tests/kwok_set.rs`) also refuses any other context name and any server not on `127.0.0.1`.

## Method

- **Environment:** Apple M4 Pro, 12 cores, 48 GiB; Docker on OrbStack with 31.4 GiB; kwokctl 0.8.0; Kubernetes
  v1.36.1; release build of the running set at `8713495b` plus the test.
- **Objects:** 5 clusters, each with 10,000 fixture pods and 91 simulated nodes. Every run confirmed 10,000 fixture
  pods per member before recording, and 50,000 rows held in the 5-member case.
- **Flint:** one `CoreHandle` in the test process, `WatchMode::OnView`, one pods subscription per member. The first
  context is active and all five are the running set. A set holds at least two clusters, so the 1-cluster baseline is
  the active cluster alone.
- **Memory:** physical footprint (`proc_pid_rusage`) is the main figure because it counts compressed pages. RSS
  (`ps`) is also given: it moved by tens of MiB between checkpoints while the footprint stayed flat.
- **CPU:** user plus system time of the test process (`getrusage`), which includes the test's own row scans.
- **Churn:** `kubectl` child processes label 1,000 pods per member. Their CPU is not counted. The churn time is mostly
  `kubectl` client throttling, so it is not a measure of Flint's latency.

**Noise:** other builds and test clusters ran on this machine throughout, with a load average of about 50 on 12 cores.
Treat timings and CPU as noisy upper-range samples, not as a benchmark. Each column has three runs, in order.

## Results

| Measure | 1 cluster (10,000 pods) | 5-member set (50,000 pods) |
|---|---|---|
| All members synced after | 556, 528, 450 ms | 3.30, 3.08, 3.29 s |
| CPU for activation and initial lists | 145, 132, 112 ms | 666, 644, 620 ms |
| Footprint when synced | 13, 13, 13 MiB | 44, 44, 44 MiB |
| Footprint after 30 s idle | 13, 12, 13 MiB | 44, 44, 44 MiB |
| Footprint after churn | 13, 13, 13 MiB | 44, 44, 44 MiB |
| RSS when synced | 23, 23, 23 MiB | 41, 54, 54 MiB |
| RSS after 30 s idle | 14, 15, 23 MiB | 33, 32, 22 MiB |
| RSS after churn | 18, 17, 23 MiB | 41, 39, 37 MiB |
| Peak RSS | 23, 23, 23 MiB | 41, 54, 54 MiB |
| CPU over 30 s idle | 0.17, 0.19, 0.23 ms | 7.64, 7.95, 7.27 ms |
| Label updates in the churn | 1,000 | 5,000 |
| Churn seen after | 18.47, 18.33, 18.32 s | 19.24, 19.07, 19.76 s |
| CPU during churn | 477, 485, 438 ms | 2.26, 2.28, 2.22 s |

RSS before the core started was 9 MiB in every run. The kwok clusters themselves used about 5.8 GiB of container
memory in total (1.0 to 1.3 GiB per member, from one `docker stats` sample after the runs); this is not Flint's memory.

An earlier 5-member run overlapped the script's final `report`, and another ran before the footprint was recorded.
Both are left out of the table.

## Not measured

- The GUI app, its rendering and frame times: this measures the core in a test process.
- `WatchMode::All`: only the pods list of each member is watched.
- Real clusters, where rows carry more labels and cell text and event rates differ.
- Pod creation and deletion churn, relists, member failure and reconnects: the churn here is label updates only.
- A quiet machine: no run was made without other builds running.
- Set runtime changes after `8713495b`: these numbers were not repeated on later commits.
