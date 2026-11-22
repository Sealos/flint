# Memory and startup benchmark

How to measure Flint's memory with a large list, and what has and has not been measured. Numbers here come from
runs of `scripts/benchmark.sh`; anything not run is listed under [Not measured](#not-measured), not estimated.

## Method

`scripts/benchmark.sh` runs the UP-26 load test (`load::up_26_large_list_stays_responsive`, the nightly
`macos-load` job in `.github/workflows/memory.yml`) and wraps its JSON report with the machine, OS, toolchain and
commit. There is no second harness: the nightly test and the benchmark are the same code.

What the test does:

1. The fake API server (`flint-e2e`) serves the fixture cluster plus 10,000 generated pods in one namespace.
2. The real app starts with the automation channel, connects, and opens the Pods view. It waits for the list to
   show every pod and to be `live` (synced, watching).
3. The app's resident set size (`ps -o rss`, KiB) is read at that point: `rss_before_burst_kib`. `list_opened_ms`
   is the time from opening the view to a live list of all pods, with the app already running and connected.
4. The server force-deletes 200 pods. While that runs, the test polls the app's automation `state` and times each
   poll (`round_trip_p50_us`, `round_trip_p99_us`, `round_trip_max_us`). These are how long the app takes to
   answer a `state` request during the burst, that is, how responsive it stays. They are not how long a delete
   takes to reach the list. The time from the first delete until the list shows the new count is `converged_ms`.
   RSS is sampled every 50 ms (`rss_peak_during_burst_kib`).

It fails only when a `state` poll passes the live-list latency bound in `docs/TESTING.md`; memory is recorded, not
gated.

Isolation: only the fake API server is used. The script sets `KUBECONFIG=/dev/null` and exits when the caller's
`KUBECONFIG` is set to anything else. The test makes its own config directory.

```sh
scripts/benchmark.sh [--profile debug|optimized] [result.json]
```

- `debug` is the build CI uses.
- `optimized` is the default. The automation channel the test needs refuses `--release` at compile time, so this
  builds the dev profile with opt-level 3 and no debug info into `target/benchmark`. It is the closest build to
  a shipped one that the harness can drive. Debug assertions and the automation channel stay compiled in, so
  its memory is an optimized-dev measurement, not a release figure. Release RSS is not measured (see below).

Requirements: macOS with a GUI session (the test opens a window), `jq`, a Rust toolchain.

## Results

Each row is one run of `scripts/benchmark.sh --profile optimized`, commit `dcdf1f6c0786c8f9e7459d58335d9e5fd0843975`
(clean tree), Apple M4 Pro, 48 GiB, macOS 27.0.1, rustc 1.100.0. The fake API is on the same machine, so
network latency is zero. 10,017 pods = the 10,000 generated plus the 17 in the fixtures.

| Run | Pods | RSS live list (MiB) | RSS peak in burst (MiB) | List opened (ms) | `state` poll p50 / max during burst (ms) |
| --- | --- | --- | --- | --- | --- |
| 1 | 10,017 | 137.8 | 137.9 | 227 | 4.6 / 6.0 |
| 2 | 10,017 | 137.1 | 137.3 | 388 | 5.4 / 7.5 |

Only 12 `state` polls were timed per run, so the p50 and max are coarse. These runs' `converged_ms` was not
recorded, so no delete-to-list time is reported here. RSS is the whole app (window, GPUI,
discovery of 74 kinds) with one list open, not the list's own cost.

## Not measured

The issue (#276) asks for more than the existing test provides. None of this has been run, so none of it is
claimed:

- **Release-build RSS.** The harness cannot drive a `--release` build, so only the optimized-dev figures above exist.
- **1,000 and 5,000 pods.** The pod count is a constant in the test. Adding sizes needs a change in
  `crates/flint-e2e/tests/app/load.rs`.
- **RSS after 30 minutes with logs following.** The test is a short burst. A soak needs a log-following step in
  the harness.
- **Cold start.** `list_opened_ms` is a warm view open, not process start to first window.
- **List scroll frame time.** The automation channel has no frame-time probe.
- **A kind cluster.** Fake API only: a real API server's paging and watch behaviour is not in these numbers.
- **Freelens 2.0 and Headlamp 0.46.** They cannot be driven by this harness, and a comparison needs the same
  cluster, fixture and method for each. Until then the site must not compare against them.
- **A debug-profile run on this machine.** The disk filled during the build; the CI nightly publishes that
  profile's numbers as the `up-26-load` artifact instead.

Related: #126, #133 (UP-26).
