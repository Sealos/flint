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

The test runs in `WatchMode::All`, the default. `FLINT_KWOK_WATCH_MODE=on-view` selects the mode of the first
measurement below.

Safety: the script writes only to its own scratch kubeconfig, refuses `~/.kube/config` and `.dev/kubeconfig`, ignores
the caller's `KUBECONFIG`, uses only the contexts it created and deletes only clusters with its own run prefix. The
test (`crates/flint-core/tests/kwok_set.rs`) also refuses the default kubeconfig through any link, any other context
name, and any server that is not the `127.0.0.1` endpoint kwokctl records for that cluster.

## Summary

| 5 members, 10,000 pods each | `OnView` at `8713495b` | `All` with the feed at `2172951f` |
|---|---|---|
| All members synced after | 3.08 to 3.30 s | 3.52 to 3.57 s |
| CPU for activation and initial lists | 620 to 666 ms | 2.07 to 2.33 s |
| Footprint after churn | 44 MiB | 686 to 711 MiB |
| CPU over 30 s idle | 7.27 to 7.95 ms | 72.74 to 103.05 ms |

**Budgets.** Neither #447 nor this document sets a budget for a running set. The closest figures are the warm-cluster
targets of [ADR-0012](../adr/0012-keep-recently-used-clusters-warm.md), planned in #247 and not implemented:

- **Memory, compliance unverified:** `warm_memory_mib` is 128 MiB for the background clusters and limits the estimated
  size of their row stores, which these runs did not measure. In `All`, at the "after churn" checkpoint, the
  whole-process footprint of the 5-member runs (686 to 711 MiB) less that of the 1-cluster runs (168 to 170 MiB) is
  516 to 543 MiB. That delta includes allocations outside the row stores, such as typed telemetry objects, so it does
  not show whether the ADR's budget is met.
- **CPU, within the target:** under 1% of one core with 3 idle warm clusters. Five idle members used 0.24 to 0.34%,
  with no test-generated churn during the idle window. KWOK still renews node Leases, so the window is not free of
  watch traffic.

## Method (`OnView`, first measurement)

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

## Results (`OnView`)

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

## Results (`All`, with the telemetry feed)

Release build of `2172951f` plus the mode switch in the test, with the same environment, clusters, churn and memory
and CPU measures. Each member runs the kinds of mode All and a feed with 7 typed watches. "All members synced" now
also waits until every feed reports all 7 kinds current. It does not wait for the other kinds of mode All, so some of
their initial list cost can fall after that point. "Churn seen after" and "CPU during churn" end when the pod lists
show every label. The feed's separate pod watch may still be processing those updates, so its remaining CPU can fall
outside that figure.

**Noise:** other builds and test clusters ran throughout. The 1-minute load average read between 15.6 and 34.7 on 12
cores at the run boundaries. The `OnView` runs saw about 50, so differences in timing between the two tables are not
precise. Each column has three runs, in order.

| Measure | 1 cluster (10,000 pods) | 5-member set (50,000 pods) |
|---|---|---|
| Pod lists synced after | 449, 526, 797 ms | 3.17, 2.95, 2.96 s |
| All members synced after | 1.05, 1.03, 1.04 s | 3.57, 3.52, 3.52 s |
| CPU for activation and initial lists | 417, 409, 517 ms | 2.33, 2.10, 2.07 s |
| Footprint when synced | 168, 169, 170 MiB | 659, 682, 665 MiB |
| Footprint after 30 s idle | 168, 169, 170 MiB | 663, 711, 700 MiB |
| Footprint after churn | 168, 169, 170 MiB | 686, 711, 700 MiB |
| RSS when synced | 179, 180, 181 MiB | 618, 723, 713 MiB |
| RSS after 30 s idle | 129, 106, 146 MiB | 520, 398, 369 MiB |
| RSS after churn | 81, 80, 148 MiB | 403, 432, 412 MiB |
| Peak RSS | 179, 180, 181 MiB | 618, 723, 713 MiB |
| CPU over 30 s idle | 13.91, 11.03, 11.81 ms | 72.74, 103.05, 96.86 ms |
| Label updates in the churn | 1,000 | 5,000 |
| Churn seen after | 18.43, 19.76, 18.40 s | 18.81, 18.76, 18.56 s |
| CPU during churn | 543, 525, 518 ms | 2.96, 3.05, 2.88 s |

RSS before the core started was 9 MiB in every run. The footprint is about 15 times the `OnView` figure for the same
set. These runs do not show how much of the increase comes from the feeds and how much from the other kinds of mode
All, because both run together in this mode.

## Not measured

- The GUI app, its rendering and frame times: this measures the core in a test process.
- The feed apart from the other watches of mode All.
- The row-store size estimate that ADR-0012's memory budget applies to.
- Real clusters, where rows carry more labels and cell text and event rates differ.
- Pod creation and deletion churn, relists, member failure and reconnects: the churn here is label updates only.
- A quiet machine: no run was made without other builds running.
- Set runtime changes after `2172951f`: these numbers were not repeated on later commits.
