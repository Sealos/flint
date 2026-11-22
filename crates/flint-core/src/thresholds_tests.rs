use super::ThresholdMetric::{CpuPercent, MemoryPercent, RestartsLastHour, WarningEventsLastHour};
use super::*;

fn load(text: &str) -> (ThresholdSettings, Vec<ThresholdIssue>) {
    ThresholdSettings::from_toml(&toml::Value::Table(toml::from_str(text).unwrap()))
}

fn overrides(values: &[(ThresholdMetric, u32)]) -> ThresholdOverrides {
    let mut overrides = ThresholdOverrides::default();
    for (metric, value) in values {
        overrides.set(*metric, Some(*value)).unwrap();
    }
    overrides
}

fn invalid(cluster: Option<&str>, metric: ThresholdMetric) -> ThresholdIssue {
    ThresholdIssue::InvalidValue {
        cluster: cluster.map(str::to_owned),
        metric,
    }
}

// covers UP-67
#[test]
fn defaults_are_in_range_with_unique_keys() {
    let expected = [
        (CpuPercent, "cpu_percent", 90),
        (MemoryPercent, "memory_percent", 90),
        (RestartsLastHour, "restarts_last_hour", 5),
        (WarningEventsLastHour, "warning_events_last_hour", 10),
    ];
    assert_eq!(ThresholdMetric::ALL.len(), expected.len());
    let defaults = Thresholds::default();
    for (index, (metric, key, value)) in expected.into_iter().enumerate() {
        assert_eq!(ThresholdMetric::ALL[index], metric);
        assert_eq!(metric.key(), key);
        assert_eq!(defaults.get(metric), value);
        assert!(metric.range().contains(&value), "{key}");
    }
}

// covers UP-67
#[test]
fn evaluation_is_a_pure_comparison_and_missing_data_is_unknown() {
    let cases = [
        (CpuPercent, None, ThresholdLevel::Unknown),
        (CpuPercent, Some(0), ThresholdLevel::Healthy),
        (CpuPercent, Some(89), ThresholdLevel::Healthy),
        (CpuPercent, Some(90), ThresholdLevel::Warning),
        (CpuPercent, Some(250), ThresholdLevel::Warning),
        (MemoryPercent, None, ThresholdLevel::Unknown),
        (RestartsLastHour, None, ThresholdLevel::Unknown),
        (RestartsLastHour, Some(4), ThresholdLevel::Healthy),
        (RestartsLastHour, Some(5), ThresholdLevel::Warning),
        (WarningEventsLastHour, None, ThresholdLevel::Unknown),
        (WarningEventsLastHour, Some(9), ThresholdLevel::Healthy),
        (
            WarningEventsLastHour,
            Some(u64::MAX),
            ThresholdLevel::Warning,
        ),
    ];
    let thresholds = Thresholds::default();
    for (metric, measured, level) in cases {
        assert_eq!(
            thresholds.evaluate(metric, measured),
            level,
            "{metric:?} {measured:?}"
        );
    }
}

#[test]
fn the_worst_level_never_hides_missing_data_as_healthy() {
    use ThresholdLevel::{Healthy, Unknown, Warning};
    let cases: [(&[ThresholdLevel], ThresholdLevel); 4] = [
        (&[Healthy, Healthy], Healthy),
        (&[Healthy, Unknown], Unknown),
        (&[Unknown, Warning], Warning),
        (&[Healthy, Unknown, Warning], Warning),
    ];
    for (levels, worst) in cases {
        assert_eq!(levels.iter().copied().max(), Some(worst), "{levels:?}");
    }
}

// covers UP-67
#[test]
fn overrides_merge_field_by_field() {
    let settings = ThresholdSettings {
        global: overrides(&[(CpuPercent, 80), (RestartsLastHour, 3)]),
        clusters: BTreeMap::from([(
            "prod".to_owned(),
            overrides(&[(CpuPercent, 70), (MemoryPercent, 60)]),
        )]),
    };
    let cases = [
        ("prod", CpuPercent, 70),
        ("prod", MemoryPercent, 60),
        ("prod", RestartsLastHour, 3),
        ("prod", WarningEventsLastHour, 10),
        ("dev", CpuPercent, 80),
        ("dev", MemoryPercent, 90),
        ("dev", RestartsLastHour, 3),
    ];
    for (cluster, metric, value) in cases {
        assert_eq!(
            settings.for_cluster(cluster).get(metric),
            value,
            "{cluster} {metric:?}"
        );
    }
}

#[test]
fn set_validates_the_range_and_keeps_the_old_value() {
    let cases = [
        (CpuPercent, 0, false),
        (CpuPercent, 1, true),
        (CpuPercent, 100, true),
        (CpuPercent, 101, false),
        (RestartsLastHour, 0, false),
        (RestartsLastHour, 10_000, true),
        (WarningEventsLastHour, 10_001, false),
    ];
    for (metric, value, valid) in cases {
        let mut set = overrides(&[(metric, 7)]);
        let result = set.set(metric, Some(value));
        assert_eq!(result.is_ok(), valid, "{metric:?} {value}");
        assert_eq!(set.get(metric), Some(if valid { value } else { 7 }));
    }
    let mut set = overrides(&[(CpuPercent, 7)]);
    set.set(CpuPercent, None).unwrap();
    assert!(set.is_empty());
}

#[test]
fn a_range_error_names_the_field_and_not_the_value() {
    let error = overrides(&[]).set(CpuPercent, Some(4242)).unwrap_err();
    assert_eq!(error.to_string(), "cpu_percent must be from 1 to 100");
}

// covers UP-67
#[test]
fn an_invalid_value_falls_back_for_that_field_alone() {
    for bad in [
        "0",
        "101",
        "-1",
        "85.5",
        "\"high\"",
        "true",
        "[90]",
        "99999999999",
    ] {
        let (settings, issues) = load(&format!("cpu_percent = {bad}\nmemory_percent = 70\n"));
        assert_eq!(issues, [invalid(None, CpuPercent)], "{bad}");
        let thresholds = settings.for_cluster("any");
        assert_eq!(thresholds.get(CpuPercent), 90, "{bad}");
        assert_eq!(thresholds.get(MemoryPercent), 70, "{bad}");
    }
}

// covers UP-67
#[test]
fn an_invalid_cluster_value_falls_back_to_the_global_value() {
    let (settings, issues) = load(
        "cpu_percent = 80\n\
         [clusters.prod]\ncpu_percent = 500\nmemory_percent = 60\n\
         [clusters.dev]\nrestarts_last_hour = 2\n",
    );
    assert_eq!(issues, [invalid(Some("prod"), CpuPercent)]);
    assert_eq!(settings.for_cluster("prod").get(CpuPercent), 80);
    assert_eq!(settings.for_cluster("prod").get(MemoryPercent), 60);
    assert_eq!(settings.for_cluster("dev").get(RestartsLastHour), 2);
}

#[test]
fn a_wrong_shape_loses_only_what_it_covers() {
    let (settings, issues) = load("cpu_percent = 80\nclusters = 3\n");
    assert_eq!(issues, [ThresholdIssue::ClustersNotATable]);
    assert_eq!(settings.for_cluster("prod").get(CpuPercent), 80);

    let (settings, issues) = load("[clusters]\nbroken = 1\n[clusters.prod]\nmemory_percent = 60\n");
    let broken = ThresholdIssue::NotATable {
        cluster: Some("broken".to_owned()),
    };
    assert_eq!(issues, [broken]);
    assert_eq!(settings.for_cluster("prod").get(MemoryPercent), 60);

    let (settings, issues) = ThresholdSettings::from_toml(&toml::Value::Integer(3));
    assert_eq!(issues, [ThresholdIssue::NotATable { cluster: None }]);
    assert_eq!(settings, ThresholdSettings::default());
}

#[test]
fn unknown_keys_are_ignored() {
    let (settings, issues) = load("future_metric = 3\ncpu_percent = 80\n");
    assert!(issues.is_empty());
    assert_eq!(settings.global, overrides(&[(CpuPercent, 80)]));
}

/// The way `settings.toml` will hold the section.
#[derive(Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
struct Host {
    other: u32,
    #[serde(skip_serializing_if = "ThresholdSettings::is_empty")]
    thresholds: ThresholdSettings,
}

// covers UP-67
#[test]
fn a_bad_section_does_not_fail_the_file_around_it() {
    for text in [
        "other = 4\nthresholds = 3\n",
        "other = 4\n[thresholds]\ncpu_percent = \"high\"\n",
        "other = 4\n[thresholds]\nclusters = []\n",
    ] {
        let host: Host = toml::from_str(text).unwrap();
        assert_eq!(host.other, 4, "{text}");
        assert_eq!(host.thresholds, ThresholdSettings::default(), "{text}");
    }
}

// covers UP-67
#[test]
fn settings_round_trip_through_toml_and_stay_out_when_empty() {
    let host = Host {
        other: 4,
        thresholds: ThresholdSettings {
            global: overrides(&[(CpuPercent, 80)]),
            clusters: BTreeMap::from([
                ("prod.example".to_owned(), overrides(&[(MemoryPercent, 60)])),
                ("dev".to_owned(), overrides(&[(RestartsLastHour, 2)])),
            ]),
        },
    };
    let text = toml::to_string_pretty(&host).unwrap();
    assert_eq!(toml::from_str::<Host>(&text).unwrap(), host, "{text}");

    let empty = Host {
        other: 4,
        thresholds: ThresholdSettings {
            global: ThresholdOverrides::default(),
            clusters: BTreeMap::from([("dev".to_owned(), ThresholdOverrides::default())]),
        },
    };
    let text = toml::to_string_pretty(&empty).unwrap();
    assert!(!text.contains("thresholds"), "{text}");
}
