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
use std::ops::RangeInclusive;

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

    /// The valid values of `table`, with an issue for each known key holding an invalid one.
    /// Unknown keys are ignored, so a newer file still loads.
    fn from_table(table: &toml::Table, cluster: Option<&str>) -> (Self, Vec<ThresholdIssue>) {
        let mut overrides = Self::default();
        let mut issues = Vec::new();
        for metric in ThresholdMetric::ALL {
            let Some(value) = table.get(metric.key()) else {
                continue;
            };
            let parsed = value
                .as_integer()
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
        let Some(table) = value.as_table() else {
            let issue = ThresholdIssue::NotATable { cluster: None };
            return (Self::default(), vec![issue]);
        };
        let (global, mut issues) = ThresholdOverrides::from_table(table, None);
        let mut clusters = BTreeMap::new();
        match table.get(CLUSTERS_KEY).map(toml::Value::as_table) {
            None => {}
            Some(None) => issues.push(ThresholdIssue::ClustersNotATable),
            Some(Some(entries)) => {
                for (cluster, entry) in entries {
                    let Some(entry) = entry.as_table() else {
                        issues.push(ThresholdIssue::NotATable {
                            cluster: Some(cluster.clone()),
                        });
                        continue;
                    };
                    let (overrides, found) = ThresholdOverrides::from_table(entry, Some(cluster));
                    issues.extend(found);
                    if !overrides.is_empty() {
                        clusters.insert(cluster.clone(), overrides);
                    }
                }
            }
        }
        (Self { global, clusters }, issues)
    }

    /// Whether the section says nothing, so it can be left out of the file.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.clusters.values().all(ThresholdOverrides::is_empty)
    }
}

impl<'de> Deserialize<'de> for ThresholdSettings {
    /// Tolerant, as [`ThresholdSettings::from_toml`]. Use that directly to report the issues.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        Ok(Self::from_toml(&value).0)
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
