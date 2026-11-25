//! The last hours of node CPU and memory for dashboard sparklines (ADR-0017).
//!
//! [`TrendStore`] keeps a window of samples per series, in memory only: 6 hours by default, from
//! 1 to 24 ([`TrendStore::set_window_hours`]). It is pure: callers hand it each poll's result and the time, and it never reads a clock or a client.
//!
//! - **Gaps stay gaps.** A failed poll, an empty answer, a skipped poll or a total that would
//!   miss a node leaves no sample, and reads show `None`, never zero.
//! - **Steps follow elapsed time.** A poll waits [`POLL_INTERVAL`] after the last fetch *ended*,
//!   so the real period is a little longer. Fixed wall-clock slots would show a false gap every
//!   so often; rounding the time since the last sample does not.
//! - **Memory follows what was collected, within a hard bound.** A ring grows as samples arrive
//!   instead of being allocated at the window's length. At most [`MAX_NODES`] node series and
//!   [`MAX_CLUSTERS`] totals hold 2 rings each of 8 bytes per step: about 46 MiB at 6 hours and
//!   179 MiB at 24 hours, plus 16 bytes per step for each cluster's poll times. A cluster that
//!   goes cold is dropped with [`TrendStore::drop_cluster`].
//! - **A shorter window trims the oldest samples; a longer one keeps what was collected.**

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use jiff::Timestamp;

use crate::cluster::ClusterId;
use crate::metrics::{Cpu, Memory, MetricsList, MetricsResult, NodeMetrics, POLL_INTERVAL, Usage};

/// Samples in one hour at [`POLL_INTERVAL`].
pub const STEPS_PER_HOUR: usize = (3_600_000 / POLL_INTERVAL.as_millis()) as usize;

/// The history length when nothing sets one, in hours.
pub const DEFAULT_HOURS: u32 = 6;

/// The longest history, in hours.
pub const MAX_HOURS: u32 = 24;

/// Samples kept per series for a window of `hours`, clamped to 1 to [`MAX_HOURS`].
pub const fn steps_for_hours(hours: u32) -> usize {
    let hours = if hours < 1 {
        1
    } else if hours > MAX_HOURS {
        MAX_HOURS
    } else {
        hours
    };
    hours as usize * STEPS_PER_HOUR
}

/// How many slots a growing ring asks for at a time, so it neither reallocates on every sample
/// nor holds much more than it has collected.
const GROWTH_CHUNK: usize = 256;

/// The most clusters a store holds. Further clusters get no trend until one is dropped.
pub const MAX_CLUSTERS: usize = 32;

/// The most nodes with their own series in one cluster, so that one large cluster cannot take
/// the whole of [`MAX_NODES`]. The cluster total still counts every node.
pub const MAX_NODES_PER_CLUSTER: usize = 500;

/// The most nodes with their own series across all clusters.
pub const MAX_NODES: usize = 2000;

/// The longest node name that gets a series: the Kubernetes limit for a DNS subdomain.
const MAX_NODE_NAME: usize = 253;

/// How many steps a node may be absent and still count as part of the cluster. While it is, the
/// total is a gap rather than a sudden drop. After that the node counts as removed.
const NODE_ABSENT_STEPS: u64 = 4;

const CADENCE_MILLIS: i128 = POLL_INTERVAL.as_millis() as i128;

/// The stored value of a step with no sample. Real values are clamped below it.
const GAP: u64 = u64::MAX;

/// Which series of a cluster to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendScope<'a> {
    /// The sum over all nodes.
    Total,
    Node(&'a str),
}

/// Which resource of a series to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendResource {
    /// In nanocores.
    Cpu,
    /// In bytes.
    Memory,
}

/// Whole steps from `earlier` to `later`, rounded to the nearest. Negative if `later` is earlier.
fn steps_between(earlier: Timestamp, later: Timestamp) -> i128 {
    let millis = later.duration_since(earlier).as_millis();
    (millis + CADENCE_MILLIS / 2).div_euclid(CADENCE_MILLIS)
}

/// Appends `value` after `steps - 1` copies of `gap`, dropping the oldest entries beyond
/// `window`. The deque only grows by what it is about to hold, in chunks, never past `window`.
fn push_steps<T: Copy>(values: &mut VecDeque<T>, steps: u64, gap: T, value: T, window: usize) {
    if steps >= window as u64 {
        values.clear();
        values.push_back(value);
        return;
    }
    let steps = steps as usize;
    let excess = (values.len() + steps).saturating_sub(window);
    values.drain(..excess.min(values.len()));
    let needed = values.len() + steps;
    if needed > values.capacity() {
        let extra = steps.max(GROWTH_CHUNK);
        values.reserve_exact(extra.min(window - values.len()));
    }
    values.extend(std::iter::repeat_n(gap, steps - 1));
    values.push_back(value);
}

/// Keeps the newest `window` entries and gives the rest of the memory back.
fn trim<T>(values: &mut VecDeque<T>, window: usize) {
    let excess = values.len().saturating_sub(window);
    values.drain(..excess);
    values.shrink_to(window);
}

/// One resource's samples, oldest first, ending at the step `newest`. Steps before the first one
/// held, or after the newest, read as gaps.
#[derive(Debug)]
struct Ring {
    values: VecDeque<u64>,
    /// The step of the last value.
    newest: u64,
}

impl Ring {
    fn new(step: u64) -> Self {
        Self {
            values: VecDeque::from([GAP]),
            newest: step,
        }
    }

    /// Stores `value` at `step`, which is never older than the newest, and fills the steps
    /// skipped since then with gaps. Keeps at most `window` steps.
    fn put(&mut self, step: u64, value: u64, window: usize) {
        let value = value.min(GAP - 1);
        match step - self.newest {
            0 => {
                if let Some(last) = self.values.back_mut() {
                    *last = value;
                }
            }
            steps => push_steps(&mut self.values, steps, GAP, value, window),
        }
        self.newest = step;
    }

    fn get(&self, step: u64) -> Option<u64> {
        let back = usize::try_from(self.newest.checked_sub(step)?).ok()?;
        let index = self.values.len().checked_sub(1 + back)?;
        let value = self.values[index];
        (value != GAP).then_some(value)
    }

    /// Keeps the newest `window` steps.
    fn trim(&mut self, window: usize) {
        trim(&mut self.values, window);
    }
}

/// The CPU and memory rings of one node or of the total.
#[derive(Debug)]
struct Series {
    cpu: Ring,
    memory: Ring,
}

impl Series {
    fn new(step: u64) -> Self {
        Self {
            cpu: Ring::new(step),
            memory: Ring::new(step),
        }
    }

    fn put(&mut self, step: u64, usage: Usage, window: usize) {
        self.cpu.put(step, usage.cpu.nanos(), window);
        self.memory.put(step, usage.memory.bytes(), window);
    }

    fn trim(&mut self, window: usize) {
        self.cpu.trim(window);
        self.memory.trim(window);
    }

    fn newest(&self) -> u64 {
        self.cpu.newest
    }

    fn ring(&self, resource: TrendResource) -> &Ring {
        match resource {
            TrendResource::Cpu => &self.cpu,
            TrendResource::Memory => &self.memory,
        }
    }
}

/// One cluster's series and its position in time.
#[derive(Debug)]
struct ClusterTrend {
    step: u64,
    last_at: Timestamp,
    /// A poll failed since the last sample, so the next one must not look contiguous.
    gap_pending: bool,
    /// When each step's poll finished, aligned with a [`Ring`] that ends at `step`: `None` for a
    /// step no poll took.
    times: VecDeque<Option<Timestamp>>,
    total: Series,
    nodes: BTreeMap<String, Series>,
}

impl ClusterTrend {
    fn new(at: Timestamp) -> Self {
        Self {
            step: 0,
            last_at: at,
            gap_pending: false,
            times: VecDeque::from([Some(at)]),
            total: Series::new(0),
            nodes: BTreeMap::new(),
        }
    }

    /// Moves to the step of a sample taken `at`. `false` if it is too soon after the last one: it
    /// then belongs to the current step, whose time and cadence stay those of its first poll.
    fn advance(&mut self, at: Timestamp, window: usize) -> bool {
        // A clock that went back cannot say how long it was, so that is a gap of unknown length.
        let mut steps = if at < self.last_at {
            2
        } else {
            steps_between(self.last_at, at)
        };
        if steps == 0 {
            return false;
        }
        if self.gap_pending {
            steps = steps.max(2);
        }
        let steps = steps.min(window as i128) as u64;
        self.step += steps;
        push_steps(&mut self.times, steps, None, Some(at), window);
        self.last_at = at;
        self.gap_pending = false;
        let step = self.step;
        self.nodes
            .retain(|_, series| step - series.newest() < window as u64);
        true
    }

    /// Keeps the newest `window` steps, and drops a node whose samples are all older.
    fn trim(&mut self, window: usize) {
        trim(&mut self.times, window);
        self.total.trim(window);
        let step = self.step;
        self.nodes
            .retain(|_, series| step - series.newest() < window as u64);
        for series in self.nodes.values_mut() {
            series.trim(window);
        }
    }

    /// Writes one list at the current step. At most `room` nodes get a new series.
    fn write(&mut self, list: &MetricsList<NodeMetrics>, mut room: usize, window: usize) {
        let step = self.step;
        let mut seen = BTreeSet::new();
        let mut complete = list.skipped == 0;
        let mut total = Usage::default();
        for node in &list.items {
            if !seen.insert(node.name.as_str()) {
                complete = false;
                continue;
            }
            total = Usage {
                cpu: Cpu::from_nanos(total.cpu.nanos().saturating_add(node.usage.cpu.nanos())),
                memory: Memory::from_bytes(
                    total
                        .memory
                        .bytes()
                        .saturating_add(node.usage.memory.bytes()),
                ),
            };
        }
        complete &= !self.nodes.iter().any(|(name, series)| {
            !seen.contains(name.as_str()) && step - series.newest() <= NODE_ABSENT_STEPS
        });

        let mut written = BTreeSet::new();
        for node in &list.items {
            if !written.insert(node.name.as_str()) {
                continue;
            }
            if let Some(series) = self.nodes.get_mut(&node.name) {
                series.put(step, node.usage, window);
            } else if room > 0 && node.name.len() <= MAX_NODE_NAME {
                room -= 1;
                let mut series = Series::new(step);
                series.put(step, node.usage, window);
                self.nodes.insert(node.name.clone(), series);
            }
        }
        if complete {
            self.total.put(step, total, window);
        }
    }

    /// Steps between the newest sample and `now`, not counting the one still expected.
    fn steps_behind(&self, now: Timestamp, window: usize) -> u64 {
        (steps_between(self.last_at, now) - 1).clamp(0, window as i128) as u64
    }
}

/// The in-memory trends of every connected cluster.
#[derive(Debug)]
pub struct TrendStore {
    clusters: BTreeMap<ClusterId, ClusterTrend>,
    /// Steps kept per series.
    window: usize,
}

impl Default for TrendStore {
    fn default() -> Self {
        Self {
            clusters: BTreeMap::new(),
            window: steps_for_hours(DEFAULT_HOURS),
        }
    }
}

impl TrendStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keeps `hours` of history (1 to [`MAX_HOURS`]) from now on. A shorter window trims the
    /// oldest samples at once; a longer one keeps what is collected and fills as samples arrive.
    pub fn set_window_hours(&mut self, hours: u32) {
        let window = steps_for_hours(hours);
        if window < self.window {
            for trend in self.clusters.values_mut() {
                trend.trim(window);
            }
        }
        self.window = window;
    }

    /// How many steps each series reads and keeps.
    pub fn window(&self) -> usize {
        self.window
    }

    /// Takes one NodeMetrics poll result, finished at `at`. An error or an empty list leaves a
    /// gap and keeps the samples so far.
    pub fn record(&mut self, cluster: &str, at: Timestamp, result: &MetricsResult<NodeMetrics>) {
        let list = match result {
            Ok(list) if !list.items.is_empty() => list,
            _ => {
                if let Some(trend) = self.clusters.get_mut(cluster) {
                    trend.gap_pending = true;
                }
                return;
            }
        };
        if let Some(trend) = self.clusters.get_mut(cluster) {
            // A poll too soon after the last one, such as a new feed's first, updates the
            // current step, so the newest values are never those of a feed that stopped.
            trend.advance(at, self.window);
        } else if self.clusters.len() >= MAX_CLUSTERS {
            return;
        } else {
            self.clusters
                .insert(cluster.to_owned(), ClusterTrend::new(at));
        }
        let used: usize = self.clusters.values().map(|c| c.nodes.len()).sum();
        let window = self.window;
        if let Some(trend) = self.clusters.get_mut(cluster) {
            let room = MAX_NODES_PER_CLUSTER
                .saturating_sub(trend.nodes.len())
                .min(MAX_NODES.saturating_sub(used));
            trend.write(list, room, window);
        }
    }

    /// Forgets a cluster, for when it goes cold.
    pub fn drop_cluster(&mut self, cluster: &str) {
        self.clusters.remove(cluster);
    }

    /// The last [`TrendStore::window`] steps up to `now`, oldest first, with `None` for each gap.
    /// `None` if the store has no such cluster or series.
    pub fn series(
        &self,
        cluster: &str,
        scope: TrendScope<'_>,
        resource: TrendResource,
        now: Timestamp,
    ) -> Option<impl Iterator<Item = Option<u64>> + '_> {
        let trend = self.clusters.get(cluster)?;
        let series = match scope {
            TrendScope::Total => &trend.total,
            TrendScope::Node(name) => trend.nodes.get(name)?,
        };
        let ring = series.ring(resource);
        let right = trend.step + trend.steps_behind(now, self.window);
        Some(
            (0..self.window as u64)
                .rev()
                .map(move |back| right.checked_sub(back).and_then(|step| ring.get(step))),
        )
    }

    /// When each step [`Self::series`] reads up to `now` was first polled, oldest first and
    /// aligned with it: `None` for a step no poll took, such as one still expected or one in a
    /// gap.
    /// `None` if the store has no such cluster.
    pub fn sample_times(
        &self,
        cluster: &str,
        now: Timestamp,
    ) -> Option<impl Iterator<Item = Option<Timestamp>> + '_> {
        let trend = self.clusters.get(cluster)?;
        let newest = trend.step;
        let right = newest + trend.steps_behind(now, self.window);
        Some((0..self.window as u64).rev().map(move |back| {
            let step = right.checked_sub(back)?;
            let back = usize::try_from(newest.checked_sub(step)?).ok()?;
            let index = trend.times.len().checked_sub(1 + back)?;
            trend.times[index]
        }))
    }

    /// How many rings are held, each of at most [`TrendStore::window`] values.
    pub fn ring_count(&self) -> usize {
        self.clusters
            .values()
            .map(|trend| 2 * (1 + trend.nodes.len()))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::SafeErrorText;
    use crate::metrics::MetricsError;

    const CLUSTER: &str = "one";
    /// Steps in the default window.
    const TREND_SAMPLES: usize = DEFAULT_HOURS as usize * STEPS_PER_HOUR;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).unwrap()
    }

    fn at_millis(millis: i64) -> Timestamp {
        Timestamp::from_millisecond(1_800_000_000_000 + millis).unwrap()
    }

    fn node(name: &str, cpu: u64, memory: u64) -> NodeMetrics {
        NodeMetrics {
            name: name.to_owned(),
            timestamp: None,
            window: None,
            usage: Usage {
                cpu: Cpu::from_nanos(cpu),
                memory: Memory::from_bytes(memory),
            },
        }
    }

    fn list(items: Vec<NodeMetrics>) -> MetricsResult<NodeMetrics> {
        Ok(MetricsList { items, skipped: 0 })
    }

    fn one(value: u64) -> MetricsResult<NodeMetrics> {
        list(vec![node("a", value, value * 2)])
    }

    fn failed() -> MetricsResult<NodeMetrics> {
        Err(MetricsError::Failed(SafeErrorText::reduced("failed")))
    }

    fn read(store: &TrendStore, scope: TrendScope<'_>, now: Timestamp) -> Vec<Option<u64>> {
        store
            .series(CLUSTER, scope, TrendResource::Cpu, now)
            .expect("series")
            .collect()
    }

    fn tail(
        store: &TrendStore,
        scope: TrendScope<'_>,
        now: Timestamp,
        n: usize,
    ) -> Vec<Option<u64>> {
        let all = read(store, scope, now);
        assert_eq!(all.len(), TREND_SAMPLES);
        all[TREND_SAMPLES - n..].to_vec()
    }

    #[test]
    fn samples_read_oldest_first_for_each_resource() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &one(1));
        store.record(CLUSTER, at(15), &one(2));
        assert_eq!(
            tail(&store, TrendScope::Total, at(15), 3),
            [None, Some(1), Some(2)]
        );
        let memory: Vec<_> = store
            .series(
                CLUSTER,
                TrendScope::Node("a"),
                TrendResource::Memory,
                at(15),
            )
            .unwrap()
            .collect();
        assert_eq!(memory[TREND_SAMPLES - 2..], [Some(2), Some(4)]);
    }

    #[test]
    fn a_measured_zero_is_a_sample() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &one(0));
        assert_eq!(tail(&store, TrendScope::Total, at(0), 1), [Some(0)]);
    }

    #[test]
    fn a_failed_poll_is_a_gap_not_zero() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &one(5));
        store.record(CLUSTER, at(15), &failed());
        store.record(CLUSTER, at(30), &one(7));
        assert_eq!(
            tail(&store, TrendScope::Total, at(30), 3),
            [Some(5), None, Some(7)]
        );
    }

    #[test]
    fn every_error_state_and_an_empty_list_leave_a_gap() {
        for result in [
            Err(MetricsError::NotInstalled),
            Err(MetricsError::Forbidden(SafeErrorText::reduced("no"))),
            failed(),
            list(vec![]),
        ] {
            let mut store = TrendStore::new();
            store.record(CLUSTER, at(0), &one(5));
            store.record(CLUSTER, at(15), &result);
            store.record(CLUSTER, at(30), &one(7));
            assert_eq!(
                tail(&store, TrendScope::Total, at(30), 3),
                [Some(5), None, Some(7)]
            );
        }
    }

    #[test]
    fn a_failure_between_close_samples_still_shows() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &one(5));
        store.record(CLUSTER, at(5), &failed());
        store.record(CLUSTER, at(15), &one(7));
        assert_eq!(
            tail(&store, TrendScope::Total, at(15), 3),
            [Some(5), None, Some(7)]
        );
    }

    #[test]
    fn a_failure_alone_creates_nothing() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &failed());
        assert!(
            store
                .series(CLUSTER, TrendScope::Total, TrendResource::Cpu, at(0))
                .is_none()
        );
        assert_eq!(store.ring_count(), 0);
    }

    #[test]
    fn a_poll_that_runs_a_little_late_never_shows_a_false_gap() {
        let mut store = TrendStore::new();
        let mut last = at(0);
        for i in 0..TREND_SAMPLES as i64 {
            last = at_millis(i * 15_200);
            store.record(CLUSTER, last, &one(1));
        }
        assert!(
            read(&store, TrendScope::Total, last)
                .iter()
                .all(Option::is_some)
        );
    }

    #[test]
    fn a_sample_too_soon_updates_the_current_step() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &one(5));
        store.record(CLUSTER, at(3), &one(9));
        assert_eq!(tail(&store, TrendScope::Total, at(3), 2), [None, Some(9)]);
        let times: Vec<_> = store.sample_times(CLUSTER, at(3)).unwrap().collect();
        assert_eq!(times.last(), Some(&Some(at(0))), "the step keeps its time");
        store.record(CLUSTER, at(15), &one(7));
        assert_eq!(
            tail(&store, TrendScope::Total, at(15), 2),
            [Some(9), Some(7)],
            "and its cadence"
        );
    }

    #[test]
    fn a_clock_that_goes_back_leaves_a_gap_and_keeps_working() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(1000), &one(5));
        store.record(CLUSTER, at(100), &one(6));
        store.record(CLUSTER, at(115), &one(7));
        assert_eq!(
            tail(&store, TrendScope::Total, at(115), 4),
            [Some(5), None, Some(6), Some(7)]
        );
    }

    #[test]
    fn time_without_samples_reads_as_trailing_gaps() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &one(5));
        assert_eq!(tail(&store, TrendScope::Total, at(15), 1), [Some(5)]);
        assert_eq!(
            tail(&store, TrendScope::Total, at(45), 3),
            [Some(5), None, None]
        );
        assert!(
            read(&store, TrendScope::Total, at(100_000))
                .iter()
                .all(Option::is_none)
        );
    }

    #[test]
    fn the_ring_keeps_only_the_newest_samples() {
        let mut store = TrendStore::new();
        let total = TREND_SAMPLES as i64 + 10;
        for i in 0..total {
            store.record(CLUSTER, at(i * 15), &one(i as u64));
        }
        let values = read(&store, TrendScope::Total, at((total - 1) * 15));
        let expected: Vec<_> = (10..total as u64).map(Some).collect();
        assert_eq!(values, expected);
    }

    #[test]
    fn a_long_outage_clears_the_whole_window() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &one(5));
        let later = 15 * (TREND_SAMPLES as i64 + 5);
        store.record(CLUSTER, at(later), &one(6));
        let values = read(&store, TrendScope::Total, at(later));
        assert_eq!(values.iter().flatten().count(), 1);
        assert_eq!(values[TREND_SAMPLES - 1], Some(6));
    }

    #[test]
    fn a_partial_answer_is_a_gap_in_the_total_only() {
        let mut store = TrendStore::new();
        store.record(
            CLUSTER,
            at(0),
            &list(vec![node("a", 1, 1), node("b", 2, 2)]),
        );
        let partial = Ok(MetricsList {
            items: vec![node("a", 3, 3)],
            skipped: 1,
        });
        store.record(CLUSTER, at(15), &partial);
        assert_eq!(tail(&store, TrendScope::Total, at(15), 2), [Some(3), None]);
        assert_eq!(
            tail(&store, TrendScope::Node("a"), at(15), 2),
            [Some(1), Some(3)]
        );
        assert_eq!(
            tail(&store, TrendScope::Node("b"), at(15), 2),
            [Some(2), None]
        );
    }

    #[test]
    fn a_node_missing_from_the_list_gaps_the_total_until_it_counts_as_removed() {
        let mut store = TrendStore::new();
        store.record(
            CLUSTER,
            at(0),
            &list(vec![node("a", 1, 1), node("b", 2, 2)]),
        );
        for i in 1..=5 {
            store.record(CLUSTER, at(i * 15), &one(1));
        }
        assert_eq!(
            tail(&store, TrendScope::Total, at(75), 6),
            [Some(3), None, None, None, None, Some(1)]
        );
    }

    #[test]
    fn a_duplicate_node_is_not_counted_twice() {
        let mut store = TrendStore::new();
        store.record(
            CLUSTER,
            at(0),
            &list(vec![node("a", 1, 1), node("a", 9, 9)]),
        );
        assert_eq!(tail(&store, TrendScope::Total, at(0), 1), [None]);
        assert_eq!(tail(&store, TrendScope::Node("a"), at(0), 1), [Some(1)]);
    }

    #[test]
    fn a_value_at_the_maximum_is_not_read_as_a_gap() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &list(vec![node("a", u64::MAX, 0)]));
        assert_eq!(
            tail(&store, TrendScope::Total, at(0), 1),
            [Some(u64::MAX - 1)]
        );
    }

    #[test]
    fn a_node_gone_for_the_whole_window_frees_its_series() {
        let mut store = TrendStore::new();
        store.record(
            CLUSTER,
            at(0),
            &list(vec![node("a", 1, 1), node("b", 2, 2)]),
        );
        assert_eq!(store.ring_count(), 6);
        store.record(CLUSTER, at(15 * TREND_SAMPLES as i64), &one(1));
        assert_eq!(store.ring_count(), 4);
        assert!(
            store
                .series(CLUSTER, TrendScope::Node("b"), TrendResource::Cpu, at(0))
                .is_none()
        );
    }

    #[test]
    fn dropping_a_cluster_frees_it_and_leaves_the_others() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at(0), &one(1));
        store.record("two", at(0), &one(2));
        store.drop_cluster(CLUSTER);
        assert!(
            store
                .series(CLUSTER, TrendScope::Total, TrendResource::Cpu, at(0))
                .is_none()
        );
        assert!(
            store
                .series("two", TrendScope::Total, TrendResource::Cpu, at(0))
                .is_some()
        );
        assert_eq!(store.ring_count(), 4);
    }

    #[test]
    fn the_cluster_count_is_bounded_and_a_drop_makes_room() {
        let mut store = TrendStore::new();
        for i in 0..MAX_CLUSTERS {
            store.record(&format!("c{i}"), at(0), &one(1));
        }
        store.record("late", at(0), &one(1));
        assert!(
            store
                .series("late", TrendScope::Total, TrendResource::Cpu, at(0))
                .is_none()
        );
        store.drop_cluster("c0");
        store.record("late", at(0), &one(1));
        assert!(
            store
                .series("late", TrendScope::Total, TrendResource::Cpu, at(0))
                .is_some()
        );
    }

    #[test]
    fn node_series_are_bounded_but_the_total_counts_every_node() {
        let many = |prefix: &str| -> MetricsResult<NodeMetrics> {
            list(
                (0..MAX_NODES_PER_CLUSTER + 100)
                    .map(|i| node(&format!("{prefix}-{i}"), 1, 1))
                    .collect(),
            )
        };
        let mut store = TrendStore::new();
        for c in 0..5 {
            store.record(&format!("c{c}"), at(0), &many("n"));
        }
        assert_eq!(store.ring_count(), 2 * (5 + MAX_NODES));
        let total: Vec<_> = store
            .series("c4", TrendScope::Total, TrendResource::Cpu, at(0))
            .unwrap()
            .collect();
        assert_eq!(
            total[TREND_SAMPLES - 1],
            Some(MAX_NODES_PER_CLUSTER as u64 + 100)
        );
    }

    #[test]
    fn an_oversized_node_name_gets_no_series() {
        let mut store = TrendStore::new();
        let name = "n".repeat(MAX_NODE_NAME + 1);
        store.record(CLUSTER, at(0), &list(vec![node(&name, 4, 4)]));
        assert_eq!(store.ring_count(), 2);
        assert_eq!(tail(&store, TrendScope::Total, at(0), 1), [Some(4)]);
    }

    #[test]
    fn each_step_reads_the_time_it_was_polled() {
        let mut store = TrendStore::new();
        store.record(CLUSTER, at_millis(0), &one(1));
        store.record(CLUSTER, at_millis(15_200), &one(2));
        store.record(CLUSTER, at_millis(61_000), &one(3));
        let now = at_millis(61_000);
        let times: Vec<_> = store.sample_times(CLUSTER, now).unwrap().collect();
        let values = read(&store, TrendScope::Total, now);
        assert_eq!(times.len(), values.len());
        let n = times.len();
        assert_eq!(
            times[n - 5..],
            [
                Some(at_millis(0)),
                Some(at_millis(15_200)),
                None,
                None,
                Some(at_millis(61_000)),
            ]
        );
        assert!(store.sample_times("missing", now).is_none());
    }
    fn fill(store: &mut TrendStore, samples: i64) {
        fill_from(store, 0, samples);
    }

    fn fill_from(store: &mut TrendStore, from: i64, to: i64) {
        for i in from..to {
            store.record(
                CLUSTER,
                at(i * 15),
                &list(vec![node("a", i as u64, 1), node("b", 1, 1)]),
            );
        }
    }

    fn ring_capacity(store: &TrendStore) -> usize {
        let trend = &store.clusters[CLUSTER];
        trend.total.cpu.values.capacity()
    }

    #[test]
    fn hours_map_to_steps_and_clamp() {
        assert_eq!(steps_for_hours(1), 240);
        assert_eq!(steps_for_hours(6), 1440);
        assert_eq!(steps_for_hours(24), 5760);
        assert_eq!(steps_for_hours(0), 240);
        assert_eq!(steps_for_hours(99), 5760);
        assert_eq!(TrendStore::new().window(), 1440);
    }

    #[test]
    fn rings_grow_as_samples_arrive_not_to_the_full_window() {
        let mut store = TrendStore::new();
        store.set_window_hours(24);
        fill(&mut store, 10);
        let capacity = ring_capacity(&store);
        assert!((10..=GROWTH_CHUNK + 1).contains(&capacity), "{capacity}");
        let times = store.clusters[CLUSTER].times.capacity();
        assert!(times <= GROWTH_CHUNK + 1, "{times}");
        // Grown past a chunk, a ring still never holds more than the window.
        fill_from(&mut store, 10, 410);
        assert!(ring_capacity(&store) < 1000);
        fill_from(&mut store, 410, 6410);
        assert!(ring_capacity(&store) <= store.window());
        assert_eq!(store.clusters[CLUSTER].total.cpu.values.len(), 5760);
    }

    #[test]
    fn shortening_trims_the_oldest_samples_and_frees_memory() {
        let mut store = TrendStore::new();
        let total = 1000;
        fill(&mut store, total);
        let now = at((total - 1) * 15);
        store.set_window_hours(1);
        let values = read(&store, TrendScope::Total, now);
        assert_eq!(values.len(), 240);
        // The total of step i is i + 1 (node a) + 1 (node b) - 1... a reads i, b reads 1.
        assert_eq!(values[0], Some(total as u64 - 240 + 1));
        assert_eq!(values[239], Some(total as u64));
        let node: Vec<_> = store
            .series(CLUSTER, TrendScope::Node("a"), TrendResource::Cpu, now)
            .unwrap()
            .collect();
        assert_eq!(node.len(), 240);
        assert_eq!(node[0], Some(total as u64 - 240));
        assert!(ring_capacity(&store) <= 240);
        let times: Vec<_> = store.sample_times(CLUSTER, now).unwrap().collect();
        assert_eq!(times.len(), 240);
        assert_eq!(times[239], Some(now));
        assert_eq!(times[0], Some(at((total - 240) * 15)));
    }

    #[test]
    fn lengthening_keeps_what_was_collected() {
        let mut store = TrendStore::new();
        store.set_window_hours(1);
        let total = 300;
        fill(&mut store, total);
        let now = at((total - 1) * 15);
        store.set_window_hours(6);
        let values = read(&store, TrendScope::Node("a"), now);
        assert_eq!(values.len(), 1440);
        // 240 samples were kept at one hour; the older 60 are gone, the rest read as before.
        let held: Vec<_> = values.iter().flatten().copied().collect();
        assert_eq!(held, (60..300).collect::<Vec<u64>>());
        assert!(values[..1440 - 240].iter().all(Option::is_none));
        // New samples extend into the longer window.
        store.record(CLUSTER, at(total * 15), &one(7));
        assert_eq!(
            store
                .series(
                    CLUSTER,
                    TrendScope::Node("a"),
                    TrendResource::Cpu,
                    at(total * 15)
                )
                .unwrap()
                .flatten()
                .count(),
            241
        );
    }

    #[test]
    fn a_node_keeps_the_full_window_like_the_total() {
        let mut store = TrendStore::new();
        store.set_window_hours(12);
        fill(&mut store, 3000);
        let now = at(2999 * 15);
        let node: Vec<_> = store
            .series(CLUSTER, TrendScope::Node("b"), TrendResource::Cpu, now)
            .unwrap()
            .collect();
        assert_eq!(node.len(), 2880);
        assert!(node.iter().all(|v| *v == Some(1)));
    }

    #[test]
    fn shrinking_drops_a_node_with_nothing_left_in_the_window() {
        let mut store = TrendStore::new();
        store.record(
            CLUSTER,
            at(0),
            &list(vec![node("gone", 1, 1), node("a", 1, 1)]),
        );
        for i in 1..500 {
            store.record(CLUSTER, at(i * 15), &one(1));
        }
        assert!(store.clusters[CLUSTER].nodes.contains_key("gone"));
        store.set_window_hours(1);
        assert!(!store.clusters[CLUSTER].nodes.contains_key("gone"));
        assert!(store.clusters[CLUSTER].nodes.contains_key("a"));
    }
}
