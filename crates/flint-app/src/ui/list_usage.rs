//! CPU and memory columns for the pod and node lists, from the cluster feed's metrics poll. A
//! list makes no request of its own.
//!
//! The columns are appended to the snapshot's definitions and rows hold no cell for them. Their
//! values live in [`UsageValues`], the raw amounts of each row, built off the UI thread with the
//! rows they belong to. A value the poll did not report is [`UNKNOWN_TEXT`], never zero, and
//! sorts last in both directions.

use std::collections::BTreeMap;
use std::sync::Arc;

use flint_core::metrics::{Cpu, Memory, PodSample, Usage};
use flint_core::telemetry::{ClusterCapacity, ClusterTelemetry, PodIndex, PodKey, UsageRead};
use flint_core::thresholds::{ThresholdMetric, Thresholds};
use flint_core::{Column, ColumnRole, ColumnType, KindId, MetricsError, Row};

use super::list_usage_bar::{self, Bar, NODE_LABELS, POD_LABELS, Scale};
use super::list_usage_workloads::{OwnerChain, Totals, WorkloadTotals};

/// What a cell or detail line says for a value the poll did not report.
pub(in crate::ui) const UNKNOWN_TEXT: &str = "unknown";

/// What a pod or node list says when the cluster has no metrics-server.
pub(in crate::ui) const NOT_INSTALLED_HINT: &str = "CPU and memory columns need metrics-server, \
     which this cluster does not have. The cluster overview shows how to install it.";

/// What a pod or node list says when nothing polls the cluster's metrics.
pub(in crate::ui) const NO_FEED_HINT: &str = "CPU and memory columns appear while the cluster \
     overview is open or the cluster is watched in the background.";

/// Room for the widest usual value, such as `1048576Mi`.
const USAGE_CHARS: usize = 9;

/// Room, in characters, for the bar before the value.
pub(in crate::ui) const BAR_CHARS: usize = 7;

const MIB: u64 = 1024 * 1024;

/// CPU as `kubectl top` prints it, with any non-zero use at least `1m`.
pub(in crate::ui) fn cpu_text(cpu: Cpu) -> String {
    format!("{}m", cpu.millis())
}

/// Memory in MiB, with any non-zero use at least `1Mi`.
pub(in crate::ui) fn memory_text(memory: Memory) -> String {
    format!("{}Mi", memory.bytes().div_ceil(MIB))
}

/// The kinds with usage columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum UsageSubject {
    Pods,
    Nodes,
    /// A workload kind: its use is the total over its current pods.
    Workloads(OwnerChain),
}

impl UsageSubject {
    /// The kinds whose detail pane shows usage: only pods and nodes so far.
    pub(in crate::ui) fn of_detail(kind: &KindId) -> Option<Self> {
        Self::of(kind).filter(|subject| !matches!(subject, Self::Workloads(_)))
    }

    pub(in crate::ui) fn of(kind: &KindId) -> Option<Self> {
        if *kind == KindId::pods() {
            return Some(Self::Pods);
        }
        if *kind == KindId::new("", "nodes") {
            return Some(Self::Nodes);
        }
        // A ReplicaSet's, Job's, StatefulSet's or DaemonSet's pods name it as their controller. A
        // Deployment's and a CronJob's are found through the ReplicaSets and Jobs they control.
        let chain = match (kind.group(), kind.plural()) {
            ("apps", "replicasets" | "statefulsets" | "daemonsets") | ("batch", "jobs") => {
                OwnerChain::Direct
            }
            ("apps", "deployments") => OwnerChain::ViaReplicaSets,
            ("batch", "cronjobs") => OwnerChain::ViaJobs,
            _ => return None,
        };
        Some(Self::Workloads(chain))
    }
}

/// What the cluster's metrics poll says for one kind.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::ui) enum UsageSource {
    /// The feed's first poll has not finished: no columns yet, and nothing to explain.
    #[default]
    Hidden,
    /// No feed runs for the cluster: no columns, and a hint that says when they appear.
    NoFeed,
    /// The cluster does not serve the metrics API: no columns, and a hint.
    NotInstalled,
    /// The API exists but gives no current values: the columns stay, every value unknown.
    Unknown,
    /// Pods' use, and what each requests and is limited to while the feed can say.
    Pods(Arc<BTreeMap<PodKey, PodSample>>, Option<PodIndex>),
    /// Nodes' use, and what each offers and its pods request while the feed can say.
    Nodes(Arc<BTreeMap<String, Usage>>, Option<ClusterCapacity>),
    /// Pods' use, which pod belongs to which workload, and what each workload's pods add up to.
    Workloads(
        Arc<BTreeMap<PodKey, PodSample>>,
        PodIndex,
        OwnerChain,
        WorkloadTotals,
    ),
}

impl UsageSource {
    /// The source `telemetry` gives. A workload kind's totals are added up here, which walks
    /// every pod: run it off the UI thread.
    #[cfg(test)]
    pub(in crate::ui) fn of(subject: UsageSubject, telemetry: Option<&ClusterTelemetry>) -> Self {
        Self::after(subject, telemetry, None)
    }

    /// [`Self::of`], reusing the totals of `previous` when it holds the same readings and pods:
    /// an update to something else adds nothing up again.
    pub(in crate::ui) fn after(
        subject: UsageSubject,
        telemetry: Option<&ClusterTelemetry>,
        previous: Option<&Self>,
    ) -> Self {
        let Some(telemetry) = telemetry else {
            return Self::NoFeed;
        };
        // The poll reports its own failures. Only a stopped feed's values are no longer current.
        let current = !telemetry.stopped;
        match subject {
            UsageSubject::Pods => {
                let index = telemetry.snapshot.pods.clone();
                Self::from_read(&telemetry.reads.pod_usage, current, |usage| {
                    Self::Pods(usage, index)
                })
            }
            UsageSubject::Workloads(chain) => {
                let index = telemetry.snapshot.pods.clone();
                Self::from_read(&telemetry.reads.pod_usage, current, |usage| match index {
                    Some(index) if chain.is_known(&index) => {
                        Self::workloads_after(usage, index, chain, previous)
                    }
                    // Without the pods, or the ReplicaSets or Jobs that say whose they are, a
                    // workload with no pod found is not a workload with none.
                    _ => Self::Unknown,
                })
            }
            UsageSubject::Nodes => {
                let capacity = telemetry.snapshot.capacity.clone();
                Self::from_read(&telemetry.reads.node_usage, current, |usage| {
                    Self::Nodes(usage, capacity)
                })
            }
        }
    }

    /// Pods' use over the workloads they belong to, with the totals worked out.
    #[cfg(test)]
    pub(in crate::ui) fn workloads(
        usage: Arc<BTreeMap<PodKey, PodSample>>,
        index: PodIndex,
        chain: OwnerChain,
    ) -> Self {
        Self::workloads_after(usage, index, chain, None)
    }

    fn workloads_after(
        usage: Arc<BTreeMap<PodKey, PodSample>>,
        index: PodIndex,
        chain: OwnerChain,
        previous: Option<&Self>,
    ) -> Self {
        let kept = match previous {
            Some(Self::Workloads(old_usage, old_index, old_chain, totals))
                if Arc::ptr_eq(old_usage, &usage) && *old_index == index && *old_chain == chain =>
            {
                Some(totals.clone())
            }
            _ => None,
        };
        let totals = kept.unwrap_or_else(|| WorkloadTotals::of(&index, &usage, chain));
        Self::Workloads(usage, index, chain, totals)
    }

    /// The totals of the workload with this `uid`, as the source holds them: no walk over pods.
    /// `None` if no pod of it is running and reporting, or the source holds no workloads.
    pub(in crate::ui) fn workload_totals(&self, uid: &str) -> Option<Totals> {
        match self {
            Self::Workloads(.., totals) => totals.get(uid),
            _ => None,
        }
    }

    fn from_read<K, V>(
        read: &UsageRead<K, V>,
        current: bool,
        known: impl FnOnce(Arc<BTreeMap<K, V>>) -> Self,
    ) -> Self {
        match read {
            UsageRead::Waiting => Self::Hidden,
            UsageRead::Unavailable(MetricsError::NotInstalled) => Self::NotInstalled,
            UsageRead::Current(values) if current => known(Arc::clone(values)),
            UsageRead::Current(_) | UsageRead::TooLarge | UsageRead::Unavailable(_) => {
                Self::Unknown
            }
        }
    }

    fn shows_columns(&self) -> bool {
        !matches!(self, Self::Hidden | Self::NoFeed | Self::NotInstalled)
    }

    fn has_readings(&self) -> bool {
        matches!(self, Self::Pods(..) | Self::Nodes(..) | Self::Workloads(..))
    }

    /// Why the columns are missing, when that needs saying.
    pub(in crate::ui) fn hint(&self) -> Option<&'static str> {
        match self {
            Self::NoFeed => Some(NO_FEED_HINT),
            Self::NotInstalled => Some(NOT_INSTALLED_HINT),
            Self::Hidden
            | Self::Unknown
            | Self::Pods(..)
            | Self::Nodes(..)
            | Self::Workloads(..) => None,
        }
    }

    /// Usage of each of `rows`, in their order. Reuses one key, so a row allocates nothing.
    pub(in crate::ui) fn values(&self, rows: &[Arc<Row>]) -> UsageValues {
        match self {
            Self::Hidden | Self::NoFeed | Self::NotInstalled => UsageValues::default(),
            Self::Unknown => UsageValues::collect(rows.iter().map(|_| RowUsage::unknown()), false),
            Self::Nodes(usage, capacity) => UsageValues::collect(
                rows.iter().map(|row| {
                    let Some(&used) = usage.get(&*row.name) else {
                        return RowUsage::unknown();
                    };
                    let Some(node) = capacity.as_ref().and_then(|c| c.node(&row.name)) else {
                        return RowUsage::budget_unknown(used);
                    };
                    // Without allocatable there is nothing to scale to: what pods request is a
                    // tick on that bar, not a bar of its own.
                    let scale = |requested: Option<u64>, allocatable: Option<u64>| {
                        allocatable
                            .map_or_else(Scale::default, |end| Scale::new(requested, Some(end)))
                    };
                    RowUsage::measured(
                        used,
                        scale(
                            node.requests.cpu.map(Cpu::nanos),
                            node.allocatable.cpu.map(Cpu::nanos),
                        ),
                        scale(
                            node.requests.memory.map(Memory::bytes),
                            node.allocatable.memory.map(Memory::bytes),
                        ),
                    )
                }),
                true,
            ),
            Self::Workloads(.., totals) => {
                UsageValues::collect(
                    rows.iter().map(|row| match totals.get(&row.uid) {
                        // No pod is running or reporting: nothing to show, which is not unknown.
                        None => RowUsage::idle(),
                        Some(totals) if !totals.budgeted => {
                            RowUsage::budget_unknown(totals.usage())
                        }
                        Some(totals) => RowUsage::measured(
                            totals.usage(),
                            Scale::new(totals.cpu.request, totals.cpu.limit),
                            Scale::new(totals.memory.request, totals.memory.limit),
                        ),
                    }),
                    false,
                )
            }
            Self::Pods(usage, index) => {
                let mut key = PodKey::default();
                UsageValues::collect(
                    rows.iter().map(|row| {
                        let mut find = || {
                            key.0.clear();
                            key.0.push_str(row.namespace.as_deref()?);
                            key.1.clear();
                            key.1.push_str(&row.name);
                            usage
                                .get(&key)
                                .filter(|sample| sample.follows_creation(row.created))
                        };
                        let Some(sample) = find() else {
                            return RowUsage::unknown();
                        };
                        // The budget only fits a sample of the containers it was read for.
                        let budget = index
                            .as_ref()
                            .and_then(|index| index.get(&row.uid))
                            .and_then(|pod| pod.budget.for_sample(sample));
                        let Some(budget) = budget else {
                            return RowUsage::budget_unknown(sample.usage);
                        };
                        RowUsage::measured(
                            sample.usage,
                            Scale::new(
                                budget.requests.cpu.map(Cpu::nanos),
                                budget.limits.cpu.map(Cpu::nanos),
                            ),
                            Scale::new(
                                budget.requests.memory.map(Memory::bytes),
                                budget.limits.memory.map(Memory::bytes),
                            ),
                        )
                    }),
                    false,
                )
            }
        }
    }
}

/// What a row's reading is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Reading {
    /// The poll did not report the row. Its amounts are meaningless.
    #[default]
    Unknown,
    /// A workload with no pod that is running and reporting: no value, and not a failed reading.
    Idle,
    /// Reported, and compared with what the row's scales say.
    Measured,
    /// Reported, but what to compare it with is not known: the feed has no record of the pod, or
    /// the sample covers other containers than the record's budget.
    BudgetUnknown,
}

/// One row's reading and what it is compared with.
#[derive(Debug, Clone, Copy)]
struct RowUsage {
    reading: Reading,
    usage: Usage,
    cpu: Scale,
    memory: Scale,
}

impl RowUsage {
    fn unknown() -> Self {
        Self {
            reading: Reading::Unknown,
            usage: Usage::default(),
            cpu: Scale::default(),
            memory: Scale::default(),
        }
    }

    fn idle() -> Self {
        Self {
            reading: Reading::Idle,
            ..Self::unknown()
        }
    }

    fn budget_unknown(usage: Usage) -> Self {
        Self {
            reading: Reading::BudgetUnknown,
            usage,
            ..Self::unknown()
        }
    }

    fn measured(usage: Usage, cpu: Scale, memory: Scale) -> Self {
        Self {
            reading: Reading::Measured,
            usage,
            cpu,
            memory,
        }
    }
}

/// Each row's raw CPU in nanocores and memory in bytes, by index into the snapshot's rows.
/// Rounded only for display, so that close values still sort apart.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::ui) struct UsageValues {
    /// Whether the poll reported the row. Its amounts are meaningless otherwise.
    known: Box<[Reading]>,
    cpu: Box<[u64]>,
    memory: Box<[u64]>,
    /// What each row's CPU and memory are compared with.
    cpu_scale: Box<[Scale]>,
    memory_scale: Box<[Scale]>,
    /// The rows are nodes: the bar's wording differs.
    nodes: bool,
}

impl UsageValues {
    fn collect(rows: impl ExactSizeIterator<Item = RowUsage>, nodes: bool) -> Self {
        let len = rows.len();
        let (mut known, mut cpu, mut memory) = (
            Vec::with_capacity(len),
            Vec::with_capacity(len),
            Vec::with_capacity(len),
        );
        let (mut cpu_scale, mut memory_scale) = (Vec::with_capacity(len), Vec::with_capacity(len));
        for row in rows {
            known.push(row.reading);
            cpu.push(row.usage.cpu.nanos());
            memory.push(row.usage.memory.bytes());
            cpu_scale.push(row.cpu);
            memory_scale.push(row.memory);
        }
        Self {
            known: known.into(),
            cpu: cpu.into(),
            memory: memory.into(),
            cpu_scale: cpu_scale.into(),
            memory_scale: memory_scale.into(),
            nodes,
        }
    }

    fn of_role(&self, role: ColumnRole) -> Option<(&[u64], &[Scale])> {
        match role {
            ColumnRole::CpuUsage => Some((&self.cpu, &self.cpu_scale)),
            ColumnRole::MemoryUsage => Some((&self.memory, &self.memory_scale)),
            _ => None,
        }
    }

    /// `row`'s amount in `values`, if the poll reported the row.
    fn known_value(&self, values: &[u64], row: usize) -> Option<u64> {
        match self.known.get(row) {
            Some(Reading::Measured | Reading::BudgetUnknown) => values.get(row).copied(),
            _ => None,
        }
    }

    /// The text of `row`'s cell, if `role` is a usage column.
    pub(in crate::ui) fn text(&self, role: ColumnRole, row: usize) -> Option<String> {
        let (values, _) = self.of_role(role)?;
        Some(match self.known_value(values, row) {
            Some(value) => display(role, value),
            None if self.known.get(row) == Some(&Reading::Idle) => String::new(),
            None => UNKNOWN_TEXT.to_owned(),
        })
    }

    /// The bar of `row`'s cell, if it has a reading and something to compare it with.
    /// `thresholds` say when it turns to a warning.
    pub(in crate::ui) fn bar(
        &self,
        role: ColumnRole,
        row: usize,
        thresholds: &Thresholds,
    ) -> Option<Bar> {
        let (values, scales) = self.of_role(role)?;
        let used = self.known_value(values, row)?;
        let metric = match role {
            ColumnRole::CpuUsage => ThresholdMetric::CpuPercent,
            _ => ThresholdMetric::MemoryPercent,
        };
        list_usage_bar::bar(used, *scales.get(row)?, thresholds.get(metric))
    }

    /// What hovering `row`'s cell says, if it has a reading.
    pub(in crate::ui) fn hover(&self, role: ColumnRole, row: usize) -> Option<String> {
        let (values, scales) = self.of_role(role)?;
        let used = self.known_value(values, row)?;
        if self.known.get(row) == Some(&Reading::BudgetUnknown) {
            return Some(list_usage_bar::hover_without_budget(used, |amount| {
                display(role, amount)
            }));
        }
        let labels = if self.nodes { NODE_LABELS } else { POD_LABELS };
        Some(list_usage_bar::hover(
            used,
            *scales.get(row)?,
            labels,
            |amount| display(role, amount),
        ))
    }

    /// The characters the widest value of the usage column `role` needs, without the bar.
    fn chars(&self, role: ColumnRole) -> usize {
        let widest = self.of_role(role).and_then(|(values, _)| {
            (0..values.len())
                .filter_map(|row| self.known_value(values, row))
                .max()
        });
        widest.map_or(USAGE_CHARS, |value| {
            display(role, value).len().max(USAGE_CHARS)
        })
    }

    /// Orders `rows` by the usage column `role`: by share of the limit, else of the request, then
    /// the rows with neither by raw value. Ties keep their order, and unknown values stay last
    /// whichever way the column sorts. `None` if `role` is not a usage column.
    pub(in crate::ui) fn sorted(
        &self,
        rows: &[u32],
        role: ColumnRole,
        descending: bool,
    ) -> Option<Arc<[u32]>> {
        let (values, scales) = self.of_role(role)?;
        let value = |row: u32| {
            let row = row as usize;
            Some((self.known_value(values, row)?, *scales.get(row)?))
        };
        let mut sorted = rows.to_vec();
        sorted.sort_by(|&a, &b| match (value(a), value(b)) {
            (Some(a), Some(b)) => list_usage_bar::compare(a, b, descending),
            (a, b) => a.is_none().cmp(&b.is_none()),
        });
        Some(sorted.into())
    }
}

fn is_usage(role: ColumnRole) -> bool {
    matches!(role, ColumnRole::CpuUsage | ColumnRole::MemoryUsage)
}

/// A raw amount as its column shows it.
fn display(role: ColumnRole, value: u64) -> String {
    match role {
        ColumnRole::CpuUsage => cpu_text(Cpu::from_nanos(value)),
        _ => memory_text(Memory::from_bytes(value)),
    }
}

/// Extends the server columns' measured `widths` to `columns`, with the usage columns' widths,
/// whose text rows do not hold.
pub(in crate::ui) fn fit_widths(widths: &mut Vec<usize>, columns: &[Column], usage: &UsageValues) {
    widths.resize(columns.len(), 0);
    for (width, column) in widths.iter_mut().zip(columns) {
        if is_usage(column.role) {
            *width = usage.chars(column.role) + BAR_CHARS;
        }
    }
}

/// Makes room for the other columns while the drawn columns are wider than `room`, so usage never
/// narrows a name or status: first the usage columns give up their bars (`bar` wide each, and a
/// column drawn that much narrower than it asks for draws no bar), then Memory goes, then CPU. A
/// column for which `keep` holds stays: the list is sorted by it.
pub(in crate::ui) fn shed_usage(
    drawn: &mut Vec<(usize, f32)>,
    columns: &[Column],
    room: f32,
    bar: f32,
    keep: impl Fn(usize) -> bool,
) {
    if drawn.iter().map(|&(_, width)| width).sum::<f32>() > room {
        for (i, width) in drawn.iter_mut() {
            if is_usage(columns[*i].role) {
                *width -= bar;
            }
        }
    }
    for role in [ColumnRole::MemoryUsage, ColumnRole::CpuUsage] {
        if drawn.iter().map(|&(_, width)| width).sum::<f32>() <= room {
            return;
        }
        drawn.retain(|&(i, _)| columns[i].role != role || keep(i));
    }
}

/// One list's usage: where it comes from, and the values of the rows drawn.
#[derive(Debug, Default)]
pub(in crate::ui) struct ListUsage {
    source: UsageSource,
    /// The values of the snapshot drawn, replaced with it.
    values: Arc<UsageValues>,
    /// Moves whenever the readings stop being valid, so a job started before cannot land.
    generation: u64,
    /// The last server columns and the same with usage columns, so that snapshots share them.
    columns: Option<ColumnPair>,
    /// When a bar turns to a warning: the Settings thresholds in force for the cluster.
    thresholds: Thresholds,
}

/// A kind's server columns, and the same with the usage columns.
type ColumnPair = (Arc<[Column]>, Arc<[Column]>);

impl ListUsage {
    /// Takes the poll's newest state. `true` if it changes what the list shows.
    pub(in crate::ui) fn set_source(&mut self, source: UsageSource) -> bool {
        if self.source == source {
            return false;
        }
        if !source.has_readings() {
            self.generation += 1;
            self.values = Arc::default();
        }
        self.source = source;
        true
    }

    /// Takes the thresholds in force. `true` if they change what the list shows.
    pub(in crate::ui) fn set_thresholds(&mut self, thresholds: Thresholds) -> bool {
        std::mem::replace(&mut self.thresholds, thresholds) != thresholds
    }

    /// The bar of a usage column's cell for the row at `row` of the snapshot drawn.
    pub(in crate::ui) fn bar(&self, role: ColumnRole, row: usize) -> Option<Bar> {
        self.values.bar(role, row, &self.thresholds)
    }

    /// Whether a usage column drawn `width` wide has room for its bar as well as its values.
    pub(in crate::ui) fn bar_fits(
        &self,
        role: ColumnRole,
        width: f32,
        char_width: f32,
        padding: f32,
    ) -> bool {
        let full = (self.values.chars(role) + BAR_CHARS) as f32 * char_width + padding;
        width + 0.5 >= full
    }

    /// What hovering a usage column's cell says for the row at `row` of the snapshot drawn.
    pub(in crate::ui) fn hover(&self, role: ColumnRole, row: usize) -> Option<String> {
        self.values.hover(role, row)
    }

    pub(in crate::ui) fn source(&self) -> &UsageSource {
        &self.source
    }

    pub(in crate::ui) fn generation(&self) -> u64 {
        self.generation
    }

    pub(in crate::ui) fn hint(&self) -> Option<&'static str> {
        self.source.hint()
    }

    /// Whether the columns are missing only because nothing polls the cluster. An ordinary
    /// state in On view mode, so the list says it in its toolbar and keeps its height.
    pub(in crate::ui) fn no_feed(&self) -> bool {
        self.source == UsageSource::NoFeed
    }

    pub(in crate::ui) fn set_values(&mut self, values: Arc<UsageValues>) {
        self.values = values;
    }

    /// The text of a usage column's cell for the row at `row` of the snapshot drawn.
    pub(in crate::ui) fn cell(&self, role: ColumnRole, row: usize) -> Option<String> {
        self.values.text(role, row)
    }

    /// The columns to draw for `server`'s: with the usage columns while they show. Kept apart
    /// from the snapshot, so that a new snapshot copies no row references.
    pub(in crate::ui) fn columns_for(&mut self, server: &Arc<[Column]>) -> Arc<[Column]> {
        if !self.source.shows_columns() {
            return Arc::clone(server);
        }
        if let Some((known, with_usage)) = &self.columns
            && Arc::ptr_eq(known, server)
        {
            return Arc::clone(with_usage);
        }
        let with_usage: Arc<[Column]> = server
            .iter()
            .cloned()
            .chain([
                Column::synthetic("CPU", ColumnType::Number, ColumnRole::CpuUsage),
                Column::synthetic("Memory", ColumnType::Number, ColumnRole::MemoryUsage),
            ])
            .collect();
        self.columns = Some((Arc::clone(server), Arc::clone(&with_usage)));
        with_usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_core::metrics::{ContainerMetrics, PodMetrics};
    use flint_core::telemetry::Availability;

    fn usage(millis: u64, mib: u64) -> Usage {
        Usage {
            cpu: Cpu::from_nanos(millis * 1_000_000),
            memory: Memory::from_bytes(mib * MIB),
        }
    }

    /// When [`sample`]'s window ends; it lasts [`WINDOW_SECS`].
    const SAMPLED_AT: i64 = 1_000;
    const WINDOW_SECS: i64 = 15;

    fn row(namespace: Option<&str>, name: &str) -> Arc<Row> {
        created_row(namespace, name, Some(SAMPLED_AT - WINDOW_SECS))
    }

    fn created_row(namespace: Option<&str>, name: &str, created: Option<i64>) -> Arc<Row> {
        Arc::new(Row {
            uid: name.into(),
            name: name.into(),
            namespace: namespace.map(Into::into),
            resource_version: "1".into(),
            generation: None,
            created,
            labels: Box::new([]),
            cells: Box::new([]),
        })
    }

    fn sample(millis: u64, mib: u64) -> PodSample {
        PodMetrics {
            namespace: "ns".into(),
            name: "pod".into(),
            timestamp: Some(jiff::Timestamp::from_second(SAMPLED_AT).unwrap()),
            window: Some(jiff::SignedDuration::from_secs(WINDOW_SECS)),
            containers: vec![ContainerMetrics {
                name: "app".into(),
                usage: usage(millis, mib),
            }],
        }
        .sample()
    }

    fn one_pod(sample: PodSample) -> UsageSource {
        let key = ("a".to_owned(), "web".to_owned());
        UsageSource::Pods(Arc::new([(key, sample)].into_iter().collect()), None)
    }

    #[test]
    fn a_replaced_pod_shows_no_usage_of_the_old_one() {
        let source = one_pod(sample(5, 7));
        let replacement = [created_row(Some("a"), "web", Some(SAMPLED_AT + 1))];
        let values = source.values(&replacement);
        assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), UNKNOWN_TEXT);
        assert_eq!(
            values.text(ColumnRole::MemoryUsage, 0).unwrap(),
            UNKNOWN_TEXT
        );
        // Created while the window was open: the reading may cover the pod before it.
        let during = [created_row(Some("a"), "web", Some(SAMPLED_AT - 1))];
        assert_eq!(
            source
                .values(&during)
                .text(ColumnRole::CpuUsage, 0)
                .unwrap(),
            UNKNOWN_TEXT
        );
        let unproved = [created_row(Some("a"), "web", None)];
        assert_eq!(
            source
                .values(&unproved)
                .text(ColumnRole::CpuUsage, 0)
                .unwrap(),
            UNKNOWN_TEXT
        );
        let same_pod = [created_row(
            Some("a"),
            "web",
            Some(SAMPLED_AT - WINDOW_SECS),
        )];
        assert_eq!(
            source
                .values(&same_pod)
                .text(ColumnRole::CpuUsage, 0)
                .unwrap(),
            "5m"
        );
    }

    #[test]
    fn forbidden_watches_do_not_hide_successful_metrics() {
        let key = ("a".to_owned(), "web".to_owned());
        let reads = flint_core::telemetry::SideReads {
            pod_usage: UsageRead::Current(Arc::new([(key, sample(5, 7))].into_iter().collect())),
            ..Default::default()
        };
        let source = |stopped| {
            let telemetry = ClusterTelemetry::for_tests(
                Availability::CannotBeChecked(None),
                reads.clone(),
                stopped,
            );
            UsageSource::of(UsageSubject::Pods, Some(&telemetry))
        };
        let values = source(false).values(&[row(Some("a"), "web")]);
        assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), "5m");
        assert_eq!(source(true), UsageSource::Unknown);
    }

    #[test]
    fn sorting_keeps_full_precision_and_huge_memory_displays_exactly() {
        let parsed = |cpu: &str, memory: &str| Usage {
            cpu: cpu.parse().unwrap(),
            memory: memory.parse().unwrap(),
        };
        let values = collect(&[
            Some(parsed("100100000n", "5Pi")),
            Some(parsed("100900000n", "6Pi")),
        ]);
        let down = values.sorted(&[0, 1], ColumnRole::CpuUsage, true).unwrap();
        assert_eq!(&*down, &[1, 0]);
        let down = values
            .sorted(&[0, 1], ColumnRole::MemoryUsage, true)
            .unwrap();
        assert_eq!(&*down, &[1, 0]);
        assert_eq!(
            values.text(ColumnRole::MemoryUsage, 0).unwrap(),
            "5368709120Mi"
        );
        assert_eq!(
            values.text(ColumnRole::MemoryUsage, 1).unwrap(),
            "6442450944Mi"
        );
    }

    fn collect(usage: &[Option<Usage>]) -> UsageValues {
        UsageValues::collect(
            usage.iter().map(|&usage| match usage {
                Some(usage) => RowUsage::measured(usage, Scale::default(), Scale::default()),
                None => RowUsage::unknown(),
            }),
            false,
        )
    }

    fn nodes(values: &[(&str, u64, u64)]) -> UsageSource {
        UsageSource::Nodes(
            Arc::new(
                values
                    .iter()
                    .map(|&(name, cpu, memory)| (name.to_owned(), usage(cpu, memory)))
                    .collect(),
            ),
            None,
        )
    }

    fn budgeted_pod(spec: serde_json::Value) -> PodIndex {
        use flint_core::telemetry::PodRecord;
        let spec = serde_json::from_value(spec).unwrap();
        let record = PodRecord {
            namespace: "a".into(),
            name: "web".into(),
            created: Some(SAMPLED_AT - WINDOW_SECS),
            budget: flint_core::metrics::PodBudgetCheck::of(&spec),
            node: None,
            owner: None,
            finished: false,
        };
        PodIndex::for_tests([("web", record)], Default::default())
    }

    fn web_row() -> Arc<Row> {
        let mut row = row(Some("a"), "web");
        Arc::get_mut(&mut row).unwrap().uid = "web".into();
        row
    }

    #[test]
    fn a_pod_bar_ends_at_its_limit_with_a_tick_at_its_request() {
        let index = budgeted_pod(serde_json::json!({"containers": [{
            "name": "app",
            "resources": {
                "requests": {"cpu": "10m", "memory": "100Mi"},
                "limits": {"cpu": "20m"}
            }
        }]}));
        let key = ("a".to_owned(), "web".to_owned());
        let source = UsageSource::Pods(
            Arc::new([(key, sample(10, 150))].into_iter().collect()),
            Some(index),
        );
        let values = source.values(&[web_row()]);
        let thresholds = Thresholds::default();
        let cpu = values.bar(ColumnRole::CpuUsage, 0, &thresholds).unwrap();
        assert_eq!((cpu.fill, cpu.tick), (0.5, Some(0.5)));
        // A request and no limit: the bar ends at the request, and use past it is over.
        let memory = values.bar(ColumnRole::MemoryUsage, 0, &thresholds).unwrap();
        assert_eq!(memory.fill, 1.);
        assert_eq!(memory.level, list_usage_bar::BarLevel::Over);
        let hover = values.hover(ColumnRole::MemoryUsage, 0).unwrap();
        assert!(hover.contains("No limit"), "{hover}");
    }

    #[test]
    fn a_pod_with_no_budget_or_no_reading_has_no_bar() {
        let key = ("a".to_owned(), "web".to_owned());
        let unbudgeted = budgeted_pod(serde_json::json!({"containers": [{"name": "app"}]}));
        let thresholds = Thresholds::default();
        let source = |index| {
            UsageSource::Pods(
                Arc::new([(key.clone(), sample(10, 10))].into_iter().collect()),
                index,
            )
        };
        for index in [Some(unbudgeted), None] {
            let values = source(index).values(&[web_row()]);
            assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), "10m");
            assert_eq!(values.bar(ColumnRole::CpuUsage, 0, &thresholds), None);
        }
        let missing = UsageSource::Unknown.values(&[web_row()]);
        assert_eq!(missing.bar(ColumnRole::CpuUsage, 0, &thresholds), None);
        assert_eq!(missing.hover(ColumnRole::CpuUsage, 0), None);
    }

    #[test]
    fn a_node_bar_ends_at_allocatable_with_a_tick_at_what_pods_request() {
        use flint_core::telemetry::{Allocatable, NodeCapacity, Reserved};
        let capacity = ClusterCapacity {
            nodes: Arc::new([NodeCapacity {
                name: "n1".into(),
                allocatable: Allocatable {
                    cpu: Some(Cpu::from_nanos(4_000_000_000)),
                    memory: None,
                    pods: None,
                },
                requests: Reserved {
                    cpu: Some(Cpu::from_nanos(1_000_000_000)),
                    memory: None,
                },
                limits: Reserved::default(),
                pods: None,
            }]),
        };
        let source = UsageSource::Nodes(
            Arc::new([("n1".to_owned(), usage(2000, 5))].into_iter().collect()),
            Some(capacity),
        );
        let values = source.values(&[row(None, "n1")]);
        let cpu = values
            .bar(ColumnRole::CpuUsage, 0, &Thresholds::default())
            .unwrap();
        assert_eq!((cpu.fill, cpu.tick), (0.5, Some(0.25)));
        assert_eq!(
            values.bar(ColumnRole::MemoryUsage, 0, &Thresholds::default()),
            None
        );
        assert!(
            values
                .hover(ColumnRole::CpuUsage, 0)
                .unwrap()
                .contains("Allocatable")
        );
    }

    #[test]
    fn usage_columns_sort_by_share_of_the_limit_then_request_then_raw_value() {
        let mk =
            |cpu: u64, scale: Scale| RowUsage::measured(usage(cpu, 1), scale, Scale::default());
        let values = UsageValues::collect(
            [
                mk(500, Scale::default()),
                mk(20, Scale::new(None, Some(100_000_000))),
                mk(90, Scale::new(Some(100_000_000), None)),
                mk(100, Scale::default()),
            ]
            .into_iter(),
            false,
        );
        let order = [0, 1, 2, 3];
        let up = values.sorted(&order, ColumnRole::CpuUsage, false).unwrap();
        assert_eq!(&*up, &[1, 2, 3, 0]);
        let down = values.sorted(&order, ColumnRole::CpuUsage, true).unwrap();
        assert_eq!(&*down, &[2, 1, 0, 3]);
    }

    #[test]
    fn thresholds_change_only_when_they_differ() {
        let mut usage = ListUsage::default();
        assert!(!usage.set_thresholds(Thresholds::default()));
        let mut custom = flint_core::thresholds::ThresholdOverrides::default();
        custom.set(ThresholdMetric::CpuPercent, Some(50)).unwrap();
        assert!(usage.set_thresholds(Thresholds::default().merged(&custom)));
    }

    #[test]
    fn pods_nodes_and_workloads_have_usage() {
        assert_eq!(UsageSubject::of(&KindId::pods()), Some(UsageSubject::Pods));
        assert_eq!(
            UsageSubject::of(&KindId::new("", "nodes")),
            Some(UsageSubject::Nodes)
        );
        let workload = |group: &str, plural: &str| UsageSubject::of(&KindId::new(group, plural));
        for (group, plural) in [
            ("apps", "replicasets"),
            ("apps", "statefulsets"),
            ("apps", "daemonsets"),
            ("batch", "jobs"),
        ] {
            assert_eq!(
                workload(group, plural),
                Some(UsageSubject::Workloads(OwnerChain::Direct))
            );
        }
        assert_eq!(
            workload("apps", "deployments"),
            Some(UsageSubject::Workloads(OwnerChain::ViaReplicaSets))
        );
        assert_eq!(
            workload("batch", "cronjobs"),
            Some(UsageSubject::Workloads(OwnerChain::ViaJobs))
        );
        assert_eq!(
            UsageSubject::of_detail(&KindId::pods()),
            Some(UsageSubject::Pods)
        );
        assert_eq!(workload("", "services"), None);
        assert_eq!(workload("metrics.k8s.io", "pods"), None);
    }

    /// What the UI thread reads of thousands of pods' totals is held by the source: with the pods
    /// themselves gone, the rows and the detail pane still get the totals, so reading them walks
    /// no pod.
    #[test]
    fn workload_totals_of_thousands_of_pods_are_read_without_walking_them() {
        use flint_core::telemetry::{Controllers, PodOwner, PodRecord};
        const PODS: usize = 5_000;
        const WORKLOADS: usize = 50;
        let spec = serde_json::from_value(serde_json::json!({"containers": [{
            "name": "app",
            "resources": {"requests": {"cpu": "10m"}, "limits": {"cpu": "20m"}}
        }]}))
        .unwrap();
        let names: Vec<String> = (0..PODS).map(|n| format!("pod-{n}")).collect();
        let records = names.iter().enumerate().map(|(n, name)| {
            let record = PodRecord {
                namespace: "a".into(),
                name: name.as_str().into(),
                created: Some(SAMPLED_AT - WINDOW_SECS),
                budget: flint_core::metrics::PodBudgetCheck::of(&spec),
                node: None,
                owner: Some(PodOwner {
                    group: "apps".into(),
                    kind: "StatefulSet".into(),
                    name: "db".into(),
                    uid: format!("sts-{}", n % WORKLOADS).into(),
                }),
                finished: false,
            };
            (&*Box::leak(format!("uid-{n}").into_boxed_str()), record)
        });
        let index = PodIndex::for_tests(records, Controllers::new());
        let samples: Arc<BTreeMap<PodKey, PodSample>> = Arc::new(
            names
                .iter()
                .map(|name| (("a".to_owned(), name.clone()), sample(10, 10)))
                .collect(),
        );
        let source = UsageSource::workloads(samples.clone(), index, OwnerChain::Direct);
        let UsageSource::Workloads(_, _, _, totals) = source else {
            unreachable!()
        };
        // The same source, now without a single pod to walk.
        let source = UsageSource::Workloads(
            samples,
            PodIndex::for_tests([], Controllers::new()),
            OwnerChain::Direct,
            totals,
        );

        let per_workload = PODS / WORKLOADS;
        let total = source.workload_totals("sts-7").unwrap();
        assert_eq!(total.pods, per_workload);
        assert_eq!(total.cpu.used, per_workload as u64 * 10_000_000);
        assert_eq!(total.cpu.limit, Some(per_workload as u64 * 20_000_000));
        assert_eq!(source.workload_totals("none"), None);

        let rows: Vec<_> = (0..WORKLOADS)
            .map(|n| {
                let mut row = row(Some("a"), "w");
                Arc::get_mut(&mut row).unwrap().uid = format!("sts-{n}").into();
                row
            })
            .collect();
        let values = source.values(&rows);
        let shown = format!("{}m", per_workload * 10);
        assert!((0..WORKLOADS).all(|n| values.text(ColumnRole::CpuUsage, n).unwrap() == shown));
    }

    #[test]
    fn a_workload_with_no_reporting_pod_shows_no_value_and_sorts_after_the_others() {
        use flint_core::telemetry::{Controllers, PodOwner, PodRecord};
        let owner = PodOwner {
            group: "apps".into(),
            kind: "StatefulSet".into(),
            name: "db".into(),
            uid: "sts".into(),
        };
        let spec = serde_json::from_value(serde_json::json!({"containers": [{
            "name": "app",
            "resources": {"requests": {"cpu": "10m"}, "limits": {"cpu": "20m"}}
        }]}))
        .unwrap();
        let record = PodRecord {
            namespace: "a".into(),
            name: "web".into(),
            created: Some(SAMPLED_AT - WINDOW_SECS),
            budget: flint_core::metrics::PodBudgetCheck::of(&spec),
            node: None,
            owner: Some(owner),
            finished: false,
        };
        let index = PodIndex::for_tests([("web", record)], Controllers::new());
        let key = ("a".to_owned(), "web".to_owned());
        let source = UsageSource::workloads(
            Arc::new([(key, sample(10, 10))].into_iter().collect()),
            index,
            OwnerChain::ViaReplicaSets,
        );
        let uid_row = |uid: &str| {
            let mut row = row(Some("a"), uid);
            Arc::get_mut(&mut row).unwrap().uid = uid.into();
            row
        };
        let rows = [uid_row("empty"), uid_row("sts")];
        let values = source.values(&rows);
        assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), "");
        assert_eq!(values.text(ColumnRole::CpuUsage, 1).unwrap(), "10m");
        let thresholds = Thresholds::default();
        assert_eq!(values.bar(ColumnRole::CpuUsage, 0, &thresholds), None);
        let bar = values.bar(ColumnRole::CpuUsage, 1, &thresholds).unwrap();
        assert_eq!((bar.fill, bar.tick), (0.5, Some(0.5)));
        let order = [0, 1];
        let up = values.sorted(&order, ColumnRole::CpuUsage, false).unwrap();
        assert_eq!(&*up, &[1, 0]);
        let down = values.sorted(&order, ColumnRole::CpuUsage, true).unwrap();
        assert_eq!(&*down, &[1, 0]);
    }

    #[test]
    fn a_missing_value_is_unknown_and_a_measured_zero_is_zero() {
        let rows = [row(None, "idle"), row(None, "missing")];
        let values = nodes(&[("idle", 0, 0)]).values(&rows);
        assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), "0m");
        assert_eq!(values.text(ColumnRole::MemoryUsage, 0).unwrap(), "0Mi");
        assert_eq!(values.text(ColumnRole::CpuUsage, 1).unwrap(), UNKNOWN_TEXT);
        assert_eq!(
            values.text(ColumnRole::MemoryUsage, 1).unwrap(),
            UNKNOWN_TEXT
        );
        assert_eq!(values.text(ColumnRole::CpuUsage, 9).unwrap(), UNKNOWN_TEXT);
        assert_eq!(values.text(ColumnRole::Value, 0), None);
    }

    #[test]
    fn a_failed_poll_leaves_every_value_unknown() {
        let values = UsageSource::Unknown.values(&[row(None, "a")]);
        assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), UNKNOWN_TEXT);
    }

    #[test]
    fn pods_are_matched_by_namespace_and_name() {
        let map = [(("a".to_owned(), "web".to_owned()), sample(5, 7))];
        let source = UsageSource::Pods(Arc::new(map.into_iter().collect()), None);
        let rows = [row(Some("a"), "web"), row(Some("b"), "web")];
        let values = source.values(&rows);
        assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), "5m");
        assert_eq!(values.text(ColumnRole::CpuUsage, 1).unwrap(), UNKNOWN_TEXT);
    }

    #[test]
    fn small_use_rounds_up_and_a_huge_value_is_not_unknown() {
        let tiny = Usage {
            cpu: Cpu::from_nanos(1),
            memory: Memory::from_bytes(1),
        };
        let huge = Usage {
            cpu: Cpu::from_nanos(u64::MAX),
            memory: Memory::from_bytes(u64::MAX),
        };
        let values = collect(&[Some(tiny), Some(huge)]);
        assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), "1m");
        assert_eq!(values.text(ColumnRole::MemoryUsage, 0).unwrap(), "1Mi");
        assert_ne!(values.text(ColumnRole::CpuUsage, 1).unwrap(), UNKNOWN_TEXT);
        assert_ne!(
            values.text(ColumnRole::MemoryUsage, 1).unwrap(),
            UNKNOWN_TEXT
        );
    }

    #[test]
    fn sorting_is_numeric_stable_and_keeps_unknown_last_both_ways() {
        let rows = [
            row(None, "ten"),
            row(None, "none"),
            row(None, "nine"),
            row(None, "also-nine"),
        ];
        let values = nodes(&[("ten", 10, 1), ("nine", 9, 2), ("also-nine", 9, 3)]).values(&rows);
        let order = [0, 1, 2, 3];
        let up = values.sorted(&order, ColumnRole::CpuUsage, false).unwrap();
        assert_eq!(&*up, &[2, 3, 0, 1]);
        let down = values.sorted(&order, ColumnRole::CpuUsage, true).unwrap();
        assert_eq!(&*down, &[0, 2, 3, 1]);
        let memory = values
            .sorted(&order, ColumnRole::MemoryUsage, true)
            .unwrap();
        assert_eq!(&*memory, &[3, 2, 0, 1]);
        assert!(values.sorted(&order, ColumnRole::Value, false).is_none());
    }

    #[test]
    fn a_stored_row_takes_forty_nine_bytes() {
        let rows: Vec<_> = (0..4).map(|i| row(None, &i.to_string())).collect();
        let values = UsageSource::Unknown.values(&rows);
        let bytes = std::mem::size_of_val(&*values.known)
            + std::mem::size_of_val(&*values.cpu)
            + std::mem::size_of_val(&*values.memory)
            + std::mem::size_of_val(&*values.cpu_scale)
            + std::mem::size_of_val(&*values.memory_scale);
        assert_eq!(bytes, 49 * rows.len());
    }

    #[test]
    fn columns_show_only_with_a_source_and_are_shared() {
        let server: Arc<[Column]> = vec![Column::synthetic(
            "Name",
            ColumnType::String,
            ColumnRole::Name,
        )]
        .into();
        let mut usage = ListUsage::default();
        assert!(Arc::ptr_eq(&usage.columns_for(&server), &server));

        assert!(usage.set_source(UsageSource::NotInstalled));
        assert_eq!(usage.hint(), Some(NOT_INSTALLED_HINT));
        assert!(Arc::ptr_eq(&usage.columns_for(&server), &server));

        assert!(usage.set_source(UsageSource::Unknown));
        assert!(!usage.set_source(UsageSource::Unknown));
        let shown = usage.columns_for(&server);
        assert_eq!(shown.len(), 3);
        assert_eq!(shown[1].key(), "flint:cpu-usage");
        assert_eq!(shown[2].key(), "flint:memory-usage");
        assert!(Arc::ptr_eq(&usage.columns_for(&server), &shown));
    }

    #[test]
    fn no_feed_has_its_own_hint_and_no_columns() {
        assert_eq!(
            UsageSource::of(UsageSubject::Pods, None),
            UsageSource::NoFeed
        );
        let mut usage = ListUsage::default();
        assert_eq!(usage.hint(), None);
        usage.set_source(UsageSource::NoFeed);
        assert_eq!(usage.hint(), Some(NO_FEED_HINT));
        assert_ne!(NO_FEED_HINT, NOT_INSTALLED_HINT);
        let server: Arc<[Column]> = vec![Column::synthetic(
            "Name",
            ColumnType::String,
            ColumnRole::Name,
        )]
        .into();
        assert!(Arc::ptr_eq(&usage.columns_for(&server), &server));
    }

    #[test]
    fn only_losing_the_readings_invalidates_running_jobs() {
        let mut usage = ListUsage::default();
        usage.set_source(one_pod(sample(1, 1)));
        let before = usage.generation();
        usage.set_source(one_pod(sample(2, 2)));
        assert_eq!(usage.generation(), before);
        usage.set_source(UsageSource::NotInstalled);
        assert_ne!(usage.generation(), before);
    }

    #[test]
    fn a_narrow_list_drops_memory_then_cpu_but_keeps_the_sorted_one() {
        let columns = [
            Column::synthetic("Name", ColumnType::String, ColumnRole::Name),
            Column::synthetic("CPU", ColumnType::Number, ColumnRole::CpuUsage),
            Column::synthetic("Memory", ColumnType::Number, ColumnRole::MemoryUsage),
        ];
        let all = vec![(0, 100.), (1, 50.), (2, 50.)];
        let kept = |room: f32, sorted: Option<usize>| {
            let mut drawn = all.clone();
            shed_usage(&mut drawn, &columns, room, 0., |i| Some(i) == sorted);
            drawn.iter().map(|&(i, _)| i).collect::<Vec<_>>()
        };
        assert_eq!(kept(200., None), [0, 1, 2]);
        assert_eq!(kept(150., None), [0, 1]);
        assert_eq!(kept(100., None), [0]);
        assert_eq!(kept(100., Some(2)), [0, 2]);
    }

    #[test]
    fn a_narrow_list_drops_the_bars_before_a_usage_column() {
        let columns = [
            Column::synthetic("Name", ColumnType::String, ColumnRole::Name),
            Column::synthetic("CPU", ColumnType::Number, ColumnRole::CpuUsage),
            Column::synthetic("Memory", ColumnType::Number, ColumnRole::MemoryUsage),
        ];
        let all = vec![(0, 100.), (1, 70.), (2, 70.)];
        let shed = |room: f32| {
            let mut drawn = all.clone();
            shed_usage(&mut drawn, &columns, room, 20., |_| false);
            drawn
        };
        assert_eq!(shed(240.), all);
        // Without bars the columns are 50 wide each and still fit.
        assert_eq!(shed(200.), [(0, 100.), (1, 50.), (2, 50.)]);
        assert_eq!(shed(150.), [(0, 100.), (1, 50.)]);
    }

    #[test]
    fn a_reading_with_no_known_budget_says_so_and_a_node_needs_allocatable() {
        let key = ("a".to_owned(), "web".to_owned());
        let source = UsageSource::Pods(
            Arc::new([(key, sample(10, 10))].into_iter().collect()),
            None,
        );
        let values = source.values(&[web_row()]);
        assert_eq!(
            values.hover(ColumnRole::CpuUsage, 0).unwrap(),
            "Using 10m\nRequest and limit: unknown"
        );

        use flint_core::telemetry::{Allocatable, NodeCapacity, Reserved};
        let capacity = ClusterCapacity {
            nodes: Arc::new([NodeCapacity {
                name: "n1".into(),
                allocatable: Allocatable::default(),
                requests: Reserved {
                    cpu: Some(Cpu::from_nanos(1_000_000_000)),
                    memory: None,
                },
                limits: Reserved::default(),
                pods: None,
            }]),
        };
        let source = UsageSource::Nodes(
            Arc::new([("n1".to_owned(), usage(2000, 5))].into_iter().collect()),
            Some(capacity),
        );
        let values = source.values(&[row(None, "n1")]);
        assert_eq!(
            values.bar(ColumnRole::CpuUsage, 0, &Thresholds::default()),
            None
        );
        assert_eq!(values.text(ColumnRole::CpuUsage, 0).unwrap(), "2000m");
    }

    #[test]
    fn usage_columns_are_wide_enough_for_their_values() {
        let columns = [
            Column::synthetic("Name", ColumnType::String, ColumnRole::Name),
            Column::synthetic("CPU", ColumnType::Number, ColumnRole::CpuUsage),
            Column::synthetic("Memory", ColumnType::Number, ColumnRole::MemoryUsage),
        ];
        let mut widths = vec![20];
        fit_widths(&mut widths, &columns, &UsageValues::default());
        assert_eq!(
            widths,
            [20, USAGE_CHARS + BAR_CHARS, USAGE_CHARS + BAR_CHARS]
        );

        let huge = Usage {
            cpu: Cpu::from_nanos(1),
            memory: "5Pi".parse().unwrap(),
        };
        let values = collect(&[Some(huge)]);
        let mut widths = vec![20];
        fit_widths(&mut widths, &columns, &values);
        assert_eq!(
            widths,
            [
                20,
                USAGE_CHARS + BAR_CHARS,
                "5368709120Mi".len() + BAR_CHARS
            ]
        );
    }
}
