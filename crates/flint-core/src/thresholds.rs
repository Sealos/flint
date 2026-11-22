//! Dashboard warning thresholds (ADR-0017): typed defaults, per-cluster overrides that merge
//! field by field, and pure evaluation of a measured value.
//!
//! The TOML shape, meant for a `thresholds` key in `settings.toml`:
//!
//! ```toml
//! [thresholds]
//! cpu_percent = 85
//!
//! [thresholds.clusters.prod]
//! memory_percent = 80
//! ```
//!
//! Loading is tolerant, so a mistake here never makes the whole settings file unreadable: an
//! invalid value falls back for that field alone, and [`ThresholdSettings::from_toml`] says which
//! field. Issues never quote a value from the file.
//!
//! To add a metric, add a [`ThresholdMetric`] variant and its entry in [`ThresholdMetric::ALL`]
//! and its `spec`.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::ops::RangeInclusive;

use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The key holding per-cluster overrides, beside the global values.
const CLUSTERS_KEY: &str = "clusters";

const PERCENT_RANGE: RangeInclusive<u32> = 1..=100;
const COUNT_RANGE: RangeInclusive<u32> = 1..=10_000;

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
}

/// A metric's TOML key, default and accepted range.
struct MetricSpec {
    key: &'static str,
    default: u32,
    range: RangeInclusive<u32>,
}

impl ThresholdMetric {
    /// Every metric, in display order.
    pub const ALL: [Self; 4] = [
        Self::CpuPercent,
        Self::MemoryPercent,
        Self::RestartsLastHour,
        Self::WarningEventsLastHour,
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
}

impl Serialize for ThresholdOverrides {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.entries())
    }
}

/// Part of the `thresholds` section that was ignored while loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThresholdIssue {
    /// `metric` held a wrong type or an out-of-range value, globally (`cluster` is `None`) or
    /// for one cluster. It falls back to the next level down.
    InvalidValue {
        cluster: Option<String>,
        metric: ThresholdMetric,
    },
    /// The section itself (`cluster` is `None`) or a cluster's entry is not a table.
    NotATable { cluster: Option<String> },
    /// `clusters` is not a table, so no per-cluster override applies.
    ClustersNotATable,
}

/// The `thresholds` section of `settings.toml`: global values over the built-in defaults, and
/// overrides per cluster, keyed by context name as the watch mode overrides are.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThresholdSettings {
    /// Global values. A metric not set here uses its built-in default.
    pub global: ThresholdOverrides,
    /// Overrides by context name. A metric not set here uses the global value.
    pub clusters: BTreeMap<String, ThresholdOverrides>,
}

impl ThresholdSettings {
    /// The thresholds in force for `cluster`: built-in defaults, then the global values, then
    /// the cluster's overrides, field by field.
    pub fn for_cluster(&self, cluster: &str) -> Thresholds {
        let thresholds = Thresholds::default().merged(&self.global);
        match self.clusters.get(cluster) {
            Some(overrides) => thresholds.merged(overrides),
            None => thresholds,
        }
    }

    /// Reads the section, keeping every valid value and listing what it ignored. Never fails.
    pub fn from_toml(value: &toml::Value) -> (Self, Vec<ThresholdIssue>) {
        let read = if value.is_table() {
            Loose::<Section>::deserialize(value.clone()).ok()
        } else {
            None
        };
        read.unwrap_or(Loose::Other).into_settings()
    }

    /// Whether the section says nothing, so it can be left out of the file.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.clusters.values().all(ThresholdOverrides::is_empty)
    }
}

impl<'de> Deserialize<'de> for ThresholdSettings {
    /// Tolerant, as [`ThresholdSettings::from_toml`]. Use that directly to report the issues.
    ///
    /// Reads straight from the deserializer, not through `toml::Value`, which rejects numbers
    /// that do not fit `i64` and reads a first key named like its datetime marker as a datetime.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Loose::<Section>::deserialize(deserializer)?
            .into_settings()
            .0)
    }
}

/// A value of any type, read without failing: a table is handed to `T`, a number is kept, and
/// anything else is skipped.
enum Loose<T> {
    Table(T),
    Number(i128),
    Other,
}

/// What a table reads its keys into.
trait TableBody<'de>: Sized {
    fn from_map<A: MapAccess<'de>>(map: A) -> Result<Self, A::Error>;
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

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Loose::Other)
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

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Loose::Other)
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
            Loose::Table(_) | Loose::Other => None,
        });
        Ok(true)
    }

    /// The valid values, with an issue for each known key holding an invalid one.
    fn resolve(&self, cluster: Option<&str>) -> (ThresholdOverrides, Vec<ThresholdIssue>) {
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
                None => issues.push(ThresholdIssue::InvalidValue {
                    cluster: cluster.map(str::to_owned),
                    metric,
                }),
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

/// The `clusters` table: one entry per context name, keys read literally.
struct Clusters(BTreeMap<String, Loose<Fields>>);

impl<'de> TableBody<'de> for Clusters {
    fn from_map<A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut entries = BTreeMap::new();
        while let Some(name) = map.next_key::<String>()? {
            entries.insert(name, map.next_value()?);
        }
        Ok(Self(entries))
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
            let issue = ThresholdIssue::NotATable { cluster: None };
            return (ThresholdSettings::default(), vec![issue]);
        };
        let (global, mut issues) = section.global.resolve(None);
        let mut clusters = BTreeMap::new();
        match section.clusters {
            None => {}
            Some(Loose::Table(Clusters(entries))) => {
                for (cluster, entry) in entries {
                    let Loose::Table(fields) = entry else {
                        issues.push(ThresholdIssue::NotATable {
                            cluster: Some(cluster),
                        });
                        continue;
                    };
                    let (overrides, found) = fields.resolve(Some(&cluster));
                    issues.extend(found);
                    if !overrides.is_empty() {
                        clusters.insert(cluster, overrides);
                    }
                }
            }
            Some(_) => issues.push(ThresholdIssue::ClustersNotATable),
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
        let clusters: BTreeMap<_, _> = self
            .clusters
            .iter()
            .filter(|(_, overrides)| !overrides.is_empty())
            .collect();
        if !clusters.is_empty() {
            map.serialize_entry(CLUSTERS_KEY, &clusters)?;
        }
        map.end()
    }
}

#[cfg(test)]
#[path = "thresholds_tests.rs"]
mod tests;
