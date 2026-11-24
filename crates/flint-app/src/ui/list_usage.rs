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
use flint_core::telemetry::{ClusterTelemetry, PodKey, UsageRead};
use flint_core::{Column, ColumnRole, ColumnType, KindId, MetricsError, Row};

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
}

impl UsageSubject {
    pub(in crate::ui) fn of(kind: &KindId) -> Option<Self> {
        if *kind == KindId::pods() {
            Some(Self::Pods)
        } else if *kind == KindId::new("", "nodes") {
            Some(Self::Nodes)
        } else {
            None
        }
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
    Pods(Arc<BTreeMap<PodKey, PodSample>>),
    Nodes(Arc<BTreeMap<String, Usage>>),
}

impl UsageSource {
    pub(in crate::ui) fn of(subject: UsageSubject, telemetry: Option<&ClusterTelemetry>) -> Self {
        let Some(telemetry) = telemetry else {
            return Self::NoFeed;
        };
        // The poll reports its own failures. Only a stopped feed's values are no longer current.
        let current = !telemetry.stopped;
        match subject {
            UsageSubject::Pods => Self::from_read(&telemetry.reads.pod_usage, current, Self::Pods),
            UsageSubject::Nodes => {
                Self::from_read(&telemetry.reads.node_usage, current, Self::Nodes)
            }
        }
    }

    fn from_read<K, V>(
        read: &UsageRead<K, V>,
        current: bool,
        known: fn(Arc<BTreeMap<K, V>>) -> Self,
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
        matches!(self, Self::Pods(_) | Self::Nodes(_))
    }

    /// Why the columns are missing, when that needs saying.
    pub(in crate::ui) fn hint(&self) -> Option<&'static str> {
        match self {
            Self::NoFeed => Some(NO_FEED_HINT),
            Self::NotInstalled => Some(NOT_INSTALLED_HINT),
            Self::Hidden | Self::Unknown | Self::Pods(_) | Self::Nodes(_) => None,
        }
    }

    /// Usage of each of `rows`, in their order. Reuses one key, so a row allocates nothing.
    pub(in crate::ui) fn values(&self, rows: &[Arc<Row>]) -> UsageValues {
        match self {
            Self::Hidden | Self::NoFeed | Self::NotInstalled => UsageValues::default(),
            Self::Unknown => UsageValues::collect(rows.iter().map(|_| None)),
            Self::Nodes(usage) => {
                UsageValues::collect(rows.iter().map(|row| usage.get(&*row.name).copied()))
            }
            Self::Pods(usage) => {
                let mut key = PodKey::default();
                UsageValues::collect(rows.iter().map(|row| {
                    key.0.clear();
                    key.0.push_str(row.namespace.as_deref()?);
                    key.1.clear();
                    key.1.push_str(&row.name);
                    usage
                        .get(&key)
                        .filter(|sample| sample.follows_creation(row.created))
                        .map(|sample| sample.usage)
                }))
            }
        }
    }
}

/// Each row's raw CPU in nanocores and memory in bytes, by index into the snapshot's rows.
/// Rounded only for display, so that close values still sort apart.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::ui) struct UsageValues {
    /// Whether the poll reported the row. Its amounts are meaningless otherwise.
    known: Box<[bool]>,
    cpu: Box<[u64]>,
    memory: Box<[u64]>,
}

impl UsageValues {
    fn collect(usage: impl ExactSizeIterator<Item = Option<Usage>>) -> Self {
        let rows = usage.len();
        let (mut known, mut cpu, mut memory) = (
            Vec::with_capacity(rows),
            Vec::with_capacity(rows),
            Vec::with_capacity(rows),
        );
        for usage in usage {
            known.push(usage.is_some());
            cpu.push(usage.map_or(0, |u| u.cpu.nanos()));
            memory.push(usage.map_or(0, |u| u.memory.bytes()));
        }
        Self {
            known: known.into(),
            cpu: cpu.into(),
            memory: memory.into(),
        }
    }

    fn of_role(&self, role: ColumnRole) -> Option<&[u64]> {
        match role {
            ColumnRole::CpuUsage => Some(&self.cpu),
            ColumnRole::MemoryUsage => Some(&self.memory),
            _ => None,
        }
    }

    /// `row`'s amount in `values`, if the poll reported the row.
    fn known_value(&self, values: &[u64], row: usize) -> Option<u64> {
        self.known
            .get(row)
            .copied()
            .unwrap_or(false)
            .then(|| values.get(row).copied())
            .flatten()
    }

    /// The text of `row`'s cell, if `role` is a usage column.
    pub(in crate::ui) fn text(&self, role: ColumnRole, row: usize) -> Option<String> {
        let values = self.of_role(role)?;
        Some(match self.known_value(values, row) {
            Some(value) => display(role, value),
            None => UNKNOWN_TEXT.to_owned(),
        })
    }

    /// The characters the widest value of the usage column `role` needs.
    fn chars(&self, role: ColumnRole) -> usize {
        let widest = self.of_role(role).and_then(|values| {
            (0..values.len())
                .filter_map(|row| self.known_value(values, row))
                .max()
        });
        widest.map_or(USAGE_CHARS, |value| {
            display(role, value).len().max(USAGE_CHARS)
        })
    }

    /// Orders `rows` by the usage column `role`. Ties keep their order, and unknown values stay
    /// last whichever way the column sorts. `None` if `role` is not a usage column.
    pub(in crate::ui) fn sorted(
        &self,
        rows: &[u32],
        role: ColumnRole,
        descending: bool,
    ) -> Option<Arc<[u32]>> {
        let values = self.of_role(role)?;
        let value = |row: u32| self.known_value(values, row as usize);
        let mut sorted = rows.to_vec();
        sorted.sort_by(|&a, &b| match (value(a), value(b)) {
            (Some(a), Some(b)) if descending => b.cmp(&a),
            (Some(a), Some(b)) => a.cmp(&b),
            (a, b) => a.is_none().cmp(&b.is_none()),
        });
        Some(sorted.into())
    }
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
        if matches!(column.role, ColumnRole::CpuUsage | ColumnRole::MemoryUsage) {
            *width = usage.chars(column.role);
        }
    }
}

/// Leaves out Memory, then CPU, while the drawn columns are wider than `room`, so usage never
/// narrows a name or status. A column for which `keep` holds stays: the list is sorted by it.
pub(in crate::ui) fn shed_usage(
    drawn: &mut Vec<(usize, f32)>,
    columns: &[Column],
    room: f32,
    keep: impl Fn(usize) -> bool,
) {
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
        UsageSource::Pods(Arc::new([(key, sample)].into_iter().collect()))
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
        let values = UsageValues::collect(
            [
                Some(parsed("100100000n", "5Pi")),
                Some(parsed("100900000n", "6Pi")),
            ]
            .into_iter(),
        );
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

    fn nodes(values: &[(&str, u64, u64)]) -> UsageSource {
        UsageSource::Nodes(Arc::new(
            values
                .iter()
                .map(|&(name, cpu, memory)| (name.to_owned(), usage(cpu, memory)))
                .collect(),
        ))
    }

    #[test]
    fn only_pods_and_nodes_have_usage() {
        assert_eq!(UsageSubject::of(&KindId::pods()), Some(UsageSubject::Pods));
        assert_eq!(
            UsageSubject::of(&KindId::new("", "nodes")),
            Some(UsageSubject::Nodes)
        );
        assert_eq!(UsageSubject::of(&KindId::new("apps", "deployments")), None);
        assert_eq!(
            UsageSubject::of(&KindId::new("metrics.k8s.io", "pods")),
            None
        );
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
        let source = UsageSource::Pods(Arc::new(map.into_iter().collect()));
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
        let values = UsageValues::collect([Some(tiny), Some(huge)].into_iter());
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
    fn a_stored_row_takes_seventeen_bytes() {
        let rows: Vec<_> = (0..4).map(|i| row(None, &i.to_string())).collect();
        let values = UsageSource::Unknown.values(&rows);
        let bytes = std::mem::size_of_val(&*values.known)
            + std::mem::size_of_val(&*values.cpu)
            + std::mem::size_of_val(&*values.memory);
        assert_eq!(bytes, 17 * rows.len());
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
            shed_usage(&mut drawn, &columns, room, |i| Some(i) == sorted);
            drawn.iter().map(|&(i, _)| i).collect::<Vec<_>>()
        };
        assert_eq!(kept(200., None), [0, 1, 2]);
        assert_eq!(kept(150., None), [0, 1]);
        assert_eq!(kept(100., None), [0]);
        assert_eq!(kept(100., Some(2)), [0, 2]);
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
        assert_eq!(widths, [20, USAGE_CHARS, USAGE_CHARS]);

        let huge = Usage {
            cpu: Cpu::from_nanos(1),
            memory: "5Pi".parse().unwrap(),
        };
        let values = UsageValues::collect([Some(huge)].into_iter());
        let mut widths = vec![20];
        fit_widths(&mut widths, &columns, &values);
        assert_eq!(widths, [20, USAGE_CHARS, "5368709120Mi".len()]);
    }
}
