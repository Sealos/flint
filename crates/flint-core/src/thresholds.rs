//! Dashboard warning thresholds (ADR-0017): typed defaults, per-cluster overrides that merge
//! field by field, and pure evaluation of a measured value.
//!
//! The TOML shape, meant for a `thresholds` key in `settings.toml`:
//!
//! ```toml
//! [thresholds]
//! cpu_percent = 85
//!
//! [[thresholds.clusters]]
//! context = "prod"
//! server = "https://prod.example:6443"
//! memory_percent = 80
//! ```
//!
//! A per-cluster entry is bound to its context and API server, as a colour tag or a trusted
//! cluster is, so it does not follow a context name to another server, and applies again when the
//! context points back.
//!
//! Loading is tolerant: an invalid value falls back for that field alone, and
//! [`ThresholdSettings::from_toml`] says which field. Only what `toml::Value` cannot represent,
//! such as an integer beyond `i64`, makes the whole settings file unreadable, because the file
//! could not be edited either. Issues never quote a value from the file, and name a cluster entry
//! by its position.
//!
//! To add a metric, add a [`ThresholdMetric`] variant and its entry in [`ThresholdMetric::ALL`]
//! and its `spec`.

use std::fmt;
use std::marker::PhantomData;
use std::ops::RangeInclusive;

use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::settings::ClusterPin;

/// The key of the section in `settings.toml`.
pub(crate) const SECTION_KEY: &str = "thresholds";
/// The key holding per-cluster overrides, beside the global values.
const CLUSTERS_KEY: &str = "clusters";
const CONTEXT_KEY: &str = "context";
const SERVER_KEY: &str = "server";

const PERCENT_RANGE: RangeInclusive<u32> = 1..=100;
const COUNT_RANGE: RangeInclusive<u32> = 1..=10_000;
/// One second to one day.
const PENDING_SECONDS_RANGE: RangeInclusive<u32> = 1..=86_400;

/// Something the dashboard warns about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThresholdMetric {
    /// CPU used, as a percentage of allocatable.
    CpuPercent,
    /// Memory used, as a percentage of allocatable.
    MemoryPercent,
    /// Container restarts in the last hour.
    RestartsLastHour,
    /// Warning events in the last hour.
    WarningEventsLastHour,
    /// Seconds a pod may stay Pending before it counts as a problem.
    PendingPodSeconds,
}

/// A metric's TOML key, default and accepted range.
struct MetricSpec {
    key: &'static str,
    default: u32,
    range: RangeInclusive<u32>,
}

impl ThresholdMetric {
    /// Every metric, in display order.
    pub const ALL: [Self; 5] = [
        Self::CpuPercent,
        Self::MemoryPercent,
        Self::RestartsLastHour,
        Self::WarningEventsLastHour,
        Self::PendingPodSeconds,
    ];

    const fn spec(self) -> MetricSpec {
        match self {
            Self::CpuPercent => MetricSpec {
                key: "cpu_percent",
                default: 90,
                range: PERCENT_RANGE,
            },
            Self::MemoryPercent => MetricSpec {
                key: "memory_percent",
                default: 90,
                range: PERCENT_RANGE,
            },
            Self::RestartsLastHour => MetricSpec {
                key: "restarts_last_hour",
                default: 5,
                range: COUNT_RANGE,
            },
            Self::WarningEventsLastHour => MetricSpec {
                key: "warning_events_last_hour",
                default: 10,
                range: COUNT_RANGE,
            },
            Self::PendingPodSeconds => MetricSpec {
                key: "pending_pod_seconds",
                default: 60,
                range: PENDING_SECONDS_RANGE,
            },
        }
    }

    /// The metric's key in `settings.toml`.
    pub const fn key(self) -> &'static str {
        self.spec().key
    }

    /// The built-in warning threshold.
    pub const fn default_value(self) -> u32 {
        self.spec().default
    }

    /// The thresholds this metric accepts.
    pub const fn range(self) -> RangeInclusive<u32> {
        self.spec().range
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// A threshold that is outside its metric's range.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{key} must be from {min} to {max}", key = .metric.key(), min = .metric.range().start(), max = .metric.range().end())]
pub struct ThresholdError {
    pub metric: ThresholdMetric,
}

/// How a measured value stands against its threshold.
///
/// Ordered by how much attention it needs, so the [`Iterator::max`] of several levels is their
/// summary: one warning makes it a warning, and missing data is never summarised as healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThresholdLevel {
    /// Measured, and below the threshold.
    Healthy,
    /// Not measured, so nothing can be said.
    Unknown,
    /// Measured, and at or above the threshold.
    Warning,
}

/// The thresholds in force for a cluster, every one in its metric's range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    values: [u32; ThresholdMetric::ALL.len()],
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            values: ThresholdMetric::ALL.map(ThresholdMetric::default_value),
        }
    }
}

impl Thresholds {
    /// The warning threshold for `metric`.
    pub fn get(&self, metric: ThresholdMetric) -> u32 {
        self.values[metric.index()]
    }

    /// How `measured` stands against the threshold for `metric`. `None` is missing data (no
    /// metrics-server, nothing to compare with) and is [`ThresholdLevel::Unknown`].
    pub fn evaluate(&self, metric: ThresholdMetric, measured: Option<u64>) -> ThresholdLevel {
        match measured {
            None => ThresholdLevel::Unknown,
            Some(value) if value >= u64::from(self.get(metric)) => ThresholdLevel::Warning,
            Some(_) => ThresholdLevel::Healthy,
        }
    }

    /// These thresholds with every value `overrides` sets replaced.
    #[must_use]
    pub fn merged(mut self, overrides: &ThresholdOverrides) -> Self {
        for metric in ThresholdMetric::ALL {
            if let Some(value) = overrides.get(metric) {
                self.values[metric.index()] = value;
            }
        }
        self
    }
}

/// Thresholds set explicitly, each one optional and in its metric's range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThresholdOverrides {
    values: [Option<u32>; ThresholdMetric::ALL.len()],
}

impl ThresholdOverrides {
    /// The value set for `metric`, if any.
    pub fn get(&self, metric: ThresholdMetric) -> Option<u32> {
        self.values[metric.index()]
    }

    /// Sets `metric`, or clears it with `None`. A value out of range changes nothing.
    pub fn set(
        &mut self,
        metric: ThresholdMetric,
        value: Option<u32>,
    ) -> Result<(), ThresholdError> {
        if value.is_some_and(|v| !metric.range().contains(&v)) {
            return Err(ThresholdError { metric });
        }
        self.values[metric.index()] = value;
        Ok(())
    }

    /// Whether nothing is set.
    pub fn is_empty(&self) -> bool {
        self.values.iter().all(Option::is_none)
    }

    fn entries(&self) -> impl Iterator<Item = (&'static str, u32)> + '_ {
        ThresholdMetric::ALL
            .into_iter()
            .filter_map(|m| Some((m.key(), self.get(m)?)))
    }

    /// Takes from `other` every metric not set here.
    fn fill_from(&mut self, other: &Self) {
        for metric in ThresholdMetric::ALL {
            let value = &mut self.values[metric.index()];
            *value = value.or(other.get(metric));
        }
    }
}

impl Serialize for ThresholdOverrides {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.entries())
    }
}

/// Part of the `thresholds` section that was ignored while loading. `entry` is a position in the
/// `clusters` list, from 0; `None` is the global values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThresholdIssue {
    /// `metric` held a wrong type or an out-of-range value. It falls back to the next level down.
    InvalidValue {
        entry: Option<usize>,
        metric: ThresholdMetric,
    },
    /// The section itself (`entry` is `None`) or a cluster entry is not a table.
    NotATable { entry: Option<usize> },
    /// `clusters` is not a list, so no per-cluster override applies.
    ClustersNotAList,
    /// A cluster entry without a usable `context` and `server`, so it applies to no cluster.
    UnboundEntry { entry: usize },
    /// A second entry for one context and server. It only fills metrics the earlier one leaves
    /// unset.
    DuplicateEntry { entry: usize },
}

impl fmt::Display for ThresholdIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = |entry: &Option<usize>| match entry {
            Some(index) => format!("{SECTION_KEY}.{CLUSTERS_KEY} entry {}", index + 1),
            None => SECTION_KEY.to_owned(),
        };
        match self {
            Self::InvalidValue { entry, metric } => {
                let error = ThresholdError { metric: *metric };
                write!(f, "{}: {error}; ignoring it", place(entry))
            }
            Self::NotATable { entry } => write!(f, "{} is not a table; ignoring it", place(entry)),
            Self::ClustersNotAList => {
                write!(f, "{SECTION_KEY}.{CLUSTERS_KEY} is not a list; ignoring it")
            }
            Self::UnboundEntry { entry } => write!(
                f,
                "{} needs a context and a server; ignoring it",
                place(&Some(*entry))
            ),
            Self::DuplicateEntry { entry } => write!(
                f,
                "{} repeats a context and server; the earlier entry wins",
                place(&Some(*entry))
            ),
        }
    }
}

/// One cluster's overrides, bound to its context and API server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterThresholds {
    pub cluster: ClusterPin,
    pub overrides: ThresholdOverrides,
}

impl Serialize for ClusterThresholds {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry(CONTEXT_KEY, &self.cluster.context)?;
        map.serialize_entry(SERVER_KEY, &self.cluster.server)?;
        for (key, value) in self.overrides.entries() {
            map.serialize_entry(key, &value)?;
        }
        map.end()
    }
}

/// The `thresholds` section of `settings.toml`: global values over the built-in defaults, and
/// overrides per context and server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThresholdSettings {
    /// Global values. A metric not set here uses its built-in default.
    pub global: ThresholdOverrides,
    /// Overrides per cluster. A metric not set here uses the global value.
    pub clusters: Vec<ClusterThresholds>,
}

impl ThresholdSettings {
    /// The thresholds in force for `context` at `server`: built-in defaults, then the global
    /// values, then that cluster's overrides, field by field. A context with no server has no
    /// overrides.
    pub fn for_cluster(&self, context: &str, server: Option<&str>) -> Thresholds {
        let thresholds = Thresholds::default().merged(&self.global);
        let overrides = server.and_then(|server| {
            self.clusters
                .iter()
                .find(|entry| entry.cluster.matches(context, server))
        });
        match overrides {
            Some(entry) => thresholds.merged(&entry.overrides),
            None => thresholds,
        }
    }

    /// Reads the section, keeping every valid value and listing what it ignored. Never fails.
    pub fn from_toml(value: &toml::Value) -> (Self, Vec<ThresholdIssue>) {
        let read = if value.is_table() {
            let mut value = value.clone();
            drop_bindings_that_are_not_text(&mut value);
            Loose::<Section>::deserialize(value).ok()
        } else {
            None
        };
        read.unwrap_or(Loose::Other).into_settings()
    }

    /// Whether the section says nothing, so it can be left out of the file.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.clusters.iter().all(|c| c.overrides.is_empty())
    }
}

impl<'de> Deserialize<'de> for ThresholdSettings {
    /// Tolerant, as [`ThresholdSettings::from_toml`]. Use that directly to report the issues.
    ///
    /// Reads through `toml::Value`, as settings edits do, so the section parses here exactly
    /// when the file can be edited: what `toml::Value` refuses makes the file unreadable.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_toml(&toml::Value::deserialize(deserializer)?).0)
    }
}

/// Removes each cluster entry's context or server that is not a string. `toml::Value` hands a
/// date to a reader as text, which would bind the entry to a context the writer cannot match.
fn drop_bindings_that_are_not_text(section: &mut toml::Value) {
    let entries = section
        .get_mut(CLUSTERS_KEY)
        .and_then(toml::Value::as_array_mut);
    for entry in entries.into_iter().flatten() {
        if let Some(entry) = entry.as_table_mut() {
            entry.retain(|key, value| !(key == CONTEXT_KEY || key == SERVER_KEY) || value.is_str());
        }
    }
}

/// A value of any type, read without failing: a table is handed to `T`, a number or a string is
/// kept, and anything else is skipped.
enum Loose<T> {
    Table(T),
    Number(i128),
    Text(String),
    Other,
}

/// What a table reads its keys into.
trait TableBody<'de>: Sized {
    fn from_map<A: MapAccess<'de>>(map: A) -> Result<Self, A::Error>;

    /// What a list reads as where this table is expected: by default, nothing.
    fn from_seq<A: SeqAccess<'de>>(mut seq: A) -> Result<Loose<Self>, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Loose::Other)
    }
}

/// A table whose contents do not matter.
struct Skipped;

impl<'de> TableBody<'de> for Skipped {
    fn from_map<A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Self)
    }
}

struct LooseVisitor<T>(PhantomData<T>);

impl<'de, T: TableBody<'de>> Visitor<'de> for LooseVisitor<T> {
    type Value = Loose<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any value")
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        T::from_map(map).map(Loose::Table)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        T::from_seq(seq)
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Loose::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Loose::Number(value.into()))
    }

    fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E> {
        Ok(Loose::Number(value))
    }

    fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E> {
        Ok(i128::try_from(value).map_or(Loose::Other, Loose::Number))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Loose::Other)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Loose::Other)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Loose::Text(value.to_owned()))
    }

    fn visit_bytes<E>(self, _: &[u8]) -> Result<Self::Value, E> {
        Ok(Loose::Other)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Loose::Other)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Loose::Other)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        IgnoredAny::deserialize(deserializer).map(|_| Loose::Other)
    }
}

impl<'de, T: TableBody<'de>> Deserialize<'de> for Loose<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LooseVisitor(PhantomData))
    }
}

/// The metric keys of one table, as read: `None` when absent, `Some(None)` when not an integer.
/// Other keys are ignored, so a newer file still loads.
#[derive(Default)]
struct Fields {
    values: [Option<Option<i128>>; ThresholdMetric::ALL.len()],
}

impl Fields {
    /// Reads the value of `key` if it names a metric. The caller reads the value otherwise.
    fn read_value<'de, A: MapAccess<'de>>(
        &mut self,
        map: &mut A,
        key: &str,
    ) -> Result<bool, A::Error> {
        let Some(metric) = ThresholdMetric::ALL.into_iter().find(|m| m.key() == key) else {
            return Ok(false);
        };
        self.values[metric.index()] = Some(match map.next_value::<Loose<Skipped>>()? {
            Loose::Number(number) => Some(number),
            Loose::Table(_) | Loose::Text(_) | Loose::Other => None,
        });
        Ok(true)
    }

    /// The valid values, with an issue for each known key holding an invalid one.
    fn resolve(&self, entry: Option<usize>) -> (ThresholdOverrides, Vec<ThresholdIssue>) {
        let mut overrides = ThresholdOverrides::default();
        let mut issues = Vec::new();
        for metric in ThresholdMetric::ALL {
            let Some(read) = self.values[metric.index()] else {
                continue;
            };
            let parsed = read
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| metric.range().contains(v));
            match parsed {
                Some(_) => overrides.values[metric.index()] = parsed,
                None => issues.push(ThresholdIssue::InvalidValue { entry, metric }),
            }
        }
        (overrides, issues)
    }
}

impl<'de> TableBody<'de> for Fields {
    fn from_map<A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut fields = Self::default();
        while let Some(key) = map.next_key::<String>()? {
            if !fields.read_value(&mut map, &key)? {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(fields)
    }
}

/// One entry of the `clusters` list, as read.
struct Entry {
    context: Option<String>,
    server: Option<String>,
    fields: Fields,
}

impl<'de> TableBody<'de> for Entry {
    fn from_map<A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut entry = Self {
            context: None,
            server: None,
            fields: Fields::default(),
        };
        let text = |value: Loose<Skipped>| match value {
            Loose::Text(text) if !text.trim().is_empty() => Some(text),
            _ => None,
        };
        while let Some(key) = map.next_key::<String>()? {
            if key == CONTEXT_KEY {
                entry.context = text(map.next_value()?);
            } else if key == SERVER_KEY {
                entry.server = text(map.next_value()?);
            } else if !entry.fields.read_value(&mut map, &key)? {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(entry)
    }
}

/// The `clusters` value: its entries when it is a list, `None` when it is a table.
struct Clusters(Option<Vec<Loose<Entry>>>);

impl<'de> TableBody<'de> for Clusters {
    fn from_map<A: MapAccess<'de>>(map: A) -> Result<Self, A::Error> {
        Skipped::from_map(map).map(|_| Self(None))
    }

    fn from_seq<A: SeqAccess<'de>>(mut seq: A) -> Result<Loose<Self>, A::Error> {
        let mut entries = Vec::new();
        while let Some(entry) = seq.next_element()? {
            entries.push(entry);
        }
        Ok(Loose::Table(Self(Some(entries))))
    }
}

/// The whole `thresholds` section, as read.
struct Section {
    global: Fields,
    clusters: Option<Loose<Clusters>>,
}

impl<'de> TableBody<'de> for Section {
    fn from_map<A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut global = Fields::default();
        let mut clusters = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == CLUSTERS_KEY {
                clusters = Some(map.next_value()?);
            } else if !global.read_value(&mut map, &key)? {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Self { global, clusters })
    }
}

impl Loose<Section> {
    fn into_settings(self) -> (ThresholdSettings, Vec<ThresholdIssue>) {
        let Self::Table(section) = self else {
            let issue = ThresholdIssue::NotATable { entry: None };
            return (ThresholdSettings::default(), vec![issue]);
        };
        let (global, mut issues) = section.global.resolve(None);
        let mut clusters: Vec<ClusterThresholds> = Vec::new();
        match section.clusters {
            None => {}
            Some(Loose::Table(Clusters(Some(entries)))) => {
                for (index, entry) in entries.into_iter().enumerate() {
                    let Loose::Table(entry) = entry else {
                        issues.push(ThresholdIssue::NotATable { entry: Some(index) });
                        continue;
                    };
                    let (Some(context), Some(server)) = (entry.context, entry.server) else {
                        issues.push(ThresholdIssue::UnboundEntry { entry: index });
                        continue;
                    };
                    let (overrides, found) = entry.fields.resolve(Some(index));
                    issues.extend(found);
                    match clusters
                        .iter_mut()
                        .find(|c| c.cluster.matches(&context, &server))
                    {
                        Some(earlier) => {
                            issues.push(ThresholdIssue::DuplicateEntry { entry: index });
                            earlier.overrides.fill_from(&overrides);
                        }
                        None if overrides.is_empty() => {}
                        None => clusters.push(ClusterThresholds {
                            cluster: ClusterPin::new(context, server),
                            overrides,
                        }),
                    }
                }
            }
            Some(_) => issues.push(ThresholdIssue::ClustersNotAList),
        }
        (ThresholdSettings { global, clusters }, issues)
    }
}

impl Serialize for ThresholdSettings {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in self.global.entries() {
            map.serialize_entry(key, &value)?;
        }
        let clusters: Vec<_> = self
            .clusters
            .iter()
            .filter(|entry| !entry.overrides.is_empty())
            .collect();
        if !clusters.is_empty() {
            map.serialize_entry(CLUSTERS_KEY, &clusters)?;
        }
        map.end()
    }
}

/// Why a threshold could not be written. Never quotes a value from the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ThresholdWriteError {
    #[error(transparent)]
    OutOfRange(#[from] ThresholdError),
    #[error("a threshold override needs a context and a server")]
    Unbound,
    #[error("thresholds is not a table; fix it in the settings file first")]
    SectionNotATable,
    #[error("thresholds.clusters is not a list; fix it in the settings file first")]
    ClustersNotAList,
}

/// Sets or clears (`value` `None`) one threshold in the raw settings `root`, globally or for
/// `cluster`, and says whether anything changed. Everything else stays, including keys and
/// entries this version cannot read. A section of the wrong shape is refused, never replaced.
pub(crate) fn write_value(
    root: &mut toml::Table,
    cluster: Option<&ClusterPin>,
    metric: ThresholdMetric,
    value: Option<u32>,
) -> Result<bool, ThresholdWriteError> {
    if value.is_some_and(|v| !metric.range().contains(&v)) {
        return Err(ThresholdError { metric }.into());
    }
    if cluster.is_some_and(|c| c.context.trim().is_empty() || c.server.trim().is_empty()) {
        return Err(ThresholdWriteError::Unbound);
    }
    let value = value.map(|v| toml::Value::Integer(v.into()));
    if value.is_none() && !root.contains_key(SECTION_KEY) {
        return Ok(false);
    }
    let section = root
        .entry(SECTION_KEY)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or(ThresholdWriteError::SectionNotATable)?;
    let changed = match cluster {
        None => put(section, metric.key(), value),
        Some(cluster) => write_cluster_value(section, cluster, metric.key(), value)?,
    };
    if section.is_empty() {
        root.remove(SECTION_KEY);
    }
    Ok(changed)
}

/// Writes `key` in every entry for `cluster`, so that a repeated entry cannot bring a cleared
/// value back. An entry left with nothing but its binding is removed.
fn write_cluster_value(
    section: &mut toml::Table,
    cluster: &ClusterPin,
    key: &str,
    value: Option<toml::Value>,
) -> Result<bool, ThresholdWriteError> {
    if value.is_none() && !section.contains_key(CLUSTERS_KEY) {
        return Ok(false);
    }
    let entries = section
        .entry(CLUSTERS_KEY)
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or(ThresholdWriteError::ClustersNotAList)?;
    let is_this = |entry: &toml::Table| {
        let field = |key| entry.get(key).and_then(toml::Value::as_str);
        matches!(
            (field(CONTEXT_KEY), field(SERVER_KEY)),
            (Some(context), Some(server)) if cluster.matches(context, server)
        )
    };
    let mut changed = false;
    let mut found = false;
    for entry in entries.iter_mut().filter_map(toml::Value::as_table_mut) {
        if is_this(entry) {
            found = true;
            changed |= put(entry, key, value.clone());
        }
    }
    match value {
        Some(value) if !found => {
            let mut entry = toml::Table::new();
            entry.insert(CONTEXT_KEY.into(), cluster.context.clone().into());
            entry.insert(SERVER_KEY.into(), cluster.server.clone().into());
            entry.insert(key.to_owned(), value);
            entries.push(toml::Value::Table(entry));
            changed = true;
        }
        Some(_) => {}
        None => entries.retain(|entry| {
            !entry
                .as_table()
                .is_some_and(|entry| is_this(entry) && entry.len() == 2)
        }),
    }
    if entries.is_empty() {
        section.remove(CLUSTERS_KEY);
    }
    Ok(changed)
}

/// Sets or removes `key`, and says whether that changed the table.
fn put(table: &mut toml::Table, key: &str, value: Option<toml::Value>) -> bool {
    match value {
        Some(value) => table.insert(key.to_owned(), value.clone()) != Some(value),
        None => table.remove(key).is_some(),
    }
}

#[cfg(test)]
#[path = "thresholds_tests.rs"]
mod tests;
