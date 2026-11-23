//! CPU and memory columns for the pod and node lists, from the cluster feed's metrics poll. A
//! list makes no request of its own.
//!
//! The columns are appended to the snapshot's definitions and rows hold no cell for them. Their
//! values live in [`UsageValues`], two integers per row, built off the UI thread with the rows
//! they belong to. A value the poll did not report is [`UNKNOWN_TEXT`], never zero, and sorts
//! last in both directions.

use std::collections::BTreeMap;
use std::sync::Arc;

use flint_core::metrics::{Cpu, Memory, PodSample, Usage};
use flint_core::telemetry::{Availability, ClusterTelemetry, PodKey, UsageRead};
use flint_core::{Column, ColumnRole, ColumnType, KindId, KindSnapshot, MetricsError, Row};

/// What a cell or detail line says for a value the poll did not report.
pub(in crate::ui) const UNKNOWN_TEXT: &str = "unknown";

/// What a pod or node list says when the cluster has no metrics-server.
pub(in crate::ui) const NOT_INSTALLED_HINT: &str = "CPU and memory columns need metrics-server, \
     which this cluster does not have. The cluster overview shows how to install it.";

/// Room for the widest usual value, such as `1048576Mi`.
const USAGE_CHARS: usize = 9;

/// A stored value the poll did not report.
const UNKNOWN: u32 = u32::MAX;

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
    /// No feed, or its first poll has not finished: no columns yet, and nothing to explain.
    #[default]
    Hidden,
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
            return Self::Hidden;
        };
        // Held values of a feed that can no longer read its cluster are not current.
        let current = !matches!(
            telemetry.snapshot.availability,
            Availability::CannotBeChecked(_)
        );
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
        !matches!(self, Self::Hidden | Self::NotInstalled)
    }

    /// Usage of each of `rows`, in their order. Reuses one key, so a row allocates nothing.
    pub(in crate::ui) fn values(&self, rows: &[Arc<Row>]) -> UsageValues {
        match self {
            Self::Hidden | Self::NotInstalled => UsageValues::default(),
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
                    usage.get(&key).map(|sample| sample.usage)
                }))
            }
        }
    }
}

/// Each row's CPU in millicores and memory in MiB, rounded up, by index into the snapshot's rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::ui) struct UsageValues {
    cpu: Box<[u32]>,
    memory: Box<[u32]>,
}

impl UsageValues {
    fn collect(usage: impl ExactSizeIterator<Item = Option<Usage>>) -> Self {
        let (mut cpu, mut memory) = (
            Vec::with_capacity(usage.len()),
            Vec::with_capacity(usage.len()),
        );
        for usage in usage {
            cpu.push(usage.map_or(UNKNOWN, |u| stored(u.cpu.millis())));
            memory.push(usage.map_or(UNKNOWN, |u| stored(u.memory.bytes().div_ceil(MIB))));
        }
        Self {
            cpu: cpu.into(),
            memory: memory.into(),
        }
    }

    fn of_role(&self, role: ColumnRole) -> Option<&[u32]> {
        match role {
            ColumnRole::CpuUsage => Some(&self.cpu),
            ColumnRole::MemoryUsage => Some(&self.memory),
            _ => None,
        }
    }

    /// The text of `row`'s cell, if `role` is a usage column.
    pub(in crate::ui) fn text(&self, role: ColumnRole, row: usize) -> Option<String> {
        let value = self.of_role(role)?.get(row).copied().unwrap_or(UNKNOWN);
        Some(match (value, role) {
            (UNKNOWN, _) => UNKNOWN_TEXT.to_owned(),
            (value, ColumnRole::CpuUsage) => format!("{value}m"),
            (value, _) => format!("{value}Mi"),
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
        let value = |row: u32| values.get(row as usize).copied().unwrap_or(UNKNOWN);
        let mut sorted = rows.to_vec();
        sorted.sort_by(|&a, &b| {
            let (a, b) = (value(a), value(b));
            match (a == UNKNOWN, b == UNKNOWN) {
                (false, false) if descending => b.cmp(&a),
                (false, false) => a.cmp(&b),
                (unknown_a, unknown_b) => unknown_a.cmp(&unknown_b),
            }
        });
        Some(sorted.into())
    }
}

/// Below [`UNKNOWN`], so a huge value can never read as a missing one.
fn stored(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(UNKNOWN).min(UNKNOWN - 1)
}

/// Sets the measured width of the usage columns, whose text rows do not hold.
pub(in crate::ui) fn fit_widths(widths: &mut [usize], columns: &[Column]) {
    for (width, column) in widths.iter_mut().zip(columns) {
        if matches!(column.role, ColumnRole::CpuUsage | ColumnRole::MemoryUsage) {
            *width = USAGE_CHARS;
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
    /// The last snapshot given usage columns and the result, which shares its rows' contents.
    decorated: Option<(Arc<KindSnapshot>, Arc<KindSnapshot>)>,
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
        if self.source.shows_columns() != source.shows_columns() {
            self.decorated = None;
        }
        self.source = source;
        true
    }

    pub(in crate::ui) fn source(&self) -> &UsageSource {
        &self.source
    }

    pub(in crate::ui) fn not_installed(&self) -> bool {
        self.source == UsageSource::NotInstalled
    }

    pub(in crate::ui) fn set_values(&mut self, values: Arc<UsageValues>) {
        self.values = values;
    }

    /// The text of a usage column's cell for the row at `row` of the snapshot drawn.
    pub(in crate::ui) fn cell(&self, role: ColumnRole, row: usize) -> Option<String> {
        self.values.text(role, row)
    }

    /// `snapshot` with the usage columns while they show. Its rows are shared, not copied.
    pub(in crate::ui) fn decorate(&mut self, snapshot: &Arc<KindSnapshot>) -> Arc<KindSnapshot> {
        if !self.source.shows_columns() {
            return Arc::clone(snapshot);
        }
        if let Some((raw, decorated)) = &self.decorated
            && Arc::ptr_eq(raw, snapshot)
        {
            return Arc::clone(decorated);
        }
        let columns = self.columns_for(&snapshot.columns);
        let decorated = Arc::new(KindSnapshot {
            columns,
            ..KindSnapshot::clone(snapshot)
        });
        self.decorated = Some((Arc::clone(snapshot), Arc::clone(&decorated)));
        decorated
    }

    fn columns_for(&mut self, server: &Arc<[Column]>) -> Arc<[Column]> {
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

    fn usage(millis: u64, mib: u64) -> Usage {
        Usage {
            cpu: Cpu::from_nanos(millis * 1_000_000),
            memory: Memory::from_bytes(mib * MIB),
        }
    }

    fn row(namespace: Option<&str>, name: &str) -> Arc<Row> {
        Arc::new(Row {
            uid: name.into(),
            name: name.into(),
            namespace: namespace.map(Into::into),
            resource_version: "1".into(),
            generation: None,
            created: None,
            labels: Box::new([]),
            cells: Box::new([]),
        })
    }

    fn sample(millis: u64, mib: u64) -> PodSample {
        PodMetrics {
            namespace: "ns".into(),
            name: "pod".into(),
            timestamp: None,
            window: None,
            containers: vec![ContainerMetrics {
                name: "app".into(),
                usage: usage(millis, mib),
            }],
        }
        .sample()
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
    fn a_stored_row_takes_eight_bytes() {
        let rows: Vec<_> = (0..4).map(|i| row(None, &i.to_string())).collect();
        let values = UsageSource::Unknown.values(&rows);
        let bytes = std::mem::size_of_val(&*values.cpu) + std::mem::size_of_val(&*values.memory);
        assert_eq!(bytes, 8 * rows.len());
    }

    #[test]
    fn columns_show_only_with_a_source_and_share_rows() {
        let raw = Arc::new(KindSnapshot {
            columns: vec![Column::synthetic(
                "Name",
                ColumnType::String,
                ColumnRole::Name,
            )]
            .into(),
            rows: vec![row(None, "a")],
            ..Default::default()
        });
        let mut usage = ListUsage::default();
        assert!(Arc::ptr_eq(&usage.decorate(&raw), &raw));

        assert!(usage.set_source(UsageSource::NotInstalled));
        assert!(usage.not_installed());
        assert!(Arc::ptr_eq(&usage.decorate(&raw), &raw));

        assert!(usage.set_source(UsageSource::Unknown));
        assert!(!usage.set_source(UsageSource::Unknown));
        let decorated = usage.decorate(&raw);
        assert_eq!(decorated.columns.len(), 3);
        assert_eq!(decorated.columns[1].key(), "flint:cpu-usage");
        assert_eq!(decorated.columns[2].key(), "flint:memory-usage");
        assert!(Arc::ptr_eq(&decorated.rows[0], &raw.rows[0]));
        assert!(Arc::ptr_eq(&usage.decorate(&raw), &decorated));

        let next = Arc::new(KindSnapshot::clone(&raw));
        assert!(Arc::ptr_eq(
            &usage.decorate(&next).columns,
            &decorated.columns
        ));
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
        let mut widths = [20, 5, 8];
        fit_widths(&mut widths, &columns);
        assert_eq!(widths, [20, USAGE_CHARS, USAGE_CHARS]);
    }
}
