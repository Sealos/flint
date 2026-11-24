use super::ThresholdMetric::{
    CpuPercent, MemoryPercent, PendingPodSeconds, RestartsLastHour, WarningEventsLastHour,
};
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

fn invalid(entry: Option<usize>, metric: ThresholdMetric) -> ThresholdIssue {
    ThresholdIssue::InvalidValue { entry, metric }
}

const A: &str = "https://a.example:6443";
const B: &str = "https://b.example:6443";

fn cluster(context: &str, values: &[(ThresholdMetric, u32)]) -> ClusterThresholds {
    ClusterThresholds {
        cluster: ClusterPin::new(context, A),
        overrides: overrides(values),
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
        (PendingPodSeconds, "pending_pod_seconds", 60),
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
        clusters: vec![cluster("prod", &[(CpuPercent, 70), (MemoryPercent, 60)])],
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
    for (context, metric, value) in cases {
        assert_eq!(
            settings.for_cluster(context, Some(A)).get(metric),
            value,
            "{context} {metric:?}"
        );
    }
}

// covers UP-67: an override does not follow a context name to another server.
#[test]
fn an_override_applies_only_at_its_server() {
    let settings = ThresholdSettings {
        global: overrides(&[(CpuPercent, 80)]),
        clusters: vec![cluster("prod", &[(CpuPercent, 70)])],
    };
    let cases = [
        (Some(A), 70),
        (Some("https://a.example:6443/"), 70),
        (Some(B), 80),
        (None, 80),
    ];
    for (server, value) in cases {
        let thresholds = settings.for_cluster("prod", server);
        assert_eq!(thresholds.get(CpuPercent), value, "{server:?}");
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
        let thresholds = settings.for_cluster("any", Some(A));
        assert_eq!(thresholds.get(CpuPercent), 90, "{bad}");
        assert_eq!(thresholds.get(MemoryPercent), 70, "{bad}");
    }
}

// covers UP-67
#[test]
fn an_invalid_cluster_value_falls_back_to_the_global_value() {
    let (settings, issues) = load(&format!(
        "cpu_percent = 80\n\
         [[clusters]]\ncontext = \"prod\"\nserver = \"{A}\"\n\
         cpu_percent = 500\nmemory_percent = 60\n\
         [[clusters]]\ncontext = \"dev\"\nserver = \"{A}\"\nrestarts_last_hour = 2\n",
    ));
    assert_eq!(issues, [invalid(Some(0), CpuPercent)]);
    let prod = settings.for_cluster("prod", Some(A));
    assert_eq!(prod.get(CpuPercent), 80);
    assert_eq!(prod.get(MemoryPercent), 60);
    let dev = settings.for_cluster("dev", Some(A));
    assert_eq!(dev.get(RestartsLastHour), 2);
}

#[test]
fn a_wrong_shape_loses_only_what_it_covers() {
    for text in [
        "cpu_percent = 80\nclusters = 3\n",
        "cpu_percent = 80\n[clusters.prod]\ncpu_percent = 50\n",
    ] {
        let (settings, issues) = load(text);
        assert_eq!(issues, [ThresholdIssue::ClustersNotAList], "{text}");
        assert_eq!(settings.for_cluster("prod", Some(A)).get(CpuPercent), 80);
    }

    let (settings, issues) = load(&format!(
        "clusters = [3, {{ context = \"no-server\", cpu_percent = 50 }}, \
         {{ context = \" \", server = \"{A}\", cpu_percent = 50 }}, \
         {{ context = \"prod\", server = \"{A}\", memory_percent = 60 }}]\n"
    ));
    assert_eq!(
        issues,
        [
            ThresholdIssue::NotATable { entry: Some(0) },
            ThresholdIssue::UnboundEntry { entry: 1 },
            ThresholdIssue::UnboundEntry { entry: 2 },
        ]
    );
    assert_eq!(settings.clusters, [cluster("prod", &[(MemoryPercent, 60)])]);

    let (settings, issues) = ThresholdSettings::from_toml(&toml::Value::Integer(3));
    assert_eq!(issues, [ThresholdIssue::NotATable { entry: None }]);
    assert_eq!(settings, ThresholdSettings::default());
}

// covers UP-67: the earlier entry decides, a later one only fills gaps, and it is reported.
#[test]
fn a_repeated_cluster_entry_fills_gaps_and_is_reported() {
    let (settings, issues) = load(&format!(
        "[[clusters]]\ncontext = \"prod\"\nserver = \"{A}\"\ncpu_percent = 70\n\
         [[clusters]]\ncontext = \"prod\"\nserver = \"{A}/\"\n\
         cpu_percent = 50\nmemory_percent = 60\n",
    ));
    assert_eq!(issues, [ThresholdIssue::DuplicateEntry { entry: 1 }]);
    assert_eq!(
        settings.clusters,
        [cluster("prod", &[(CpuPercent, 70), (MemoryPercent, 60)])]
    );
}

#[test]
fn an_issue_never_quotes_the_file() {
    let (_, issues) = load(
        "cpu_percent = \"s3cret\"\nclusters = [{ context = \"s3cret\", cpu_percent = 1 }, \
         { context = \"s3cret\", server = \"https://s3cret\", memory_percent = 900 }]\n",
    );
    assert_eq!(issues.len(), 3);
    for issue in issues {
        assert!(!issue.to_string().contains("s3cret"), "{issue}");
    }
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
        "other = 4\n[thresholds]\nclusters = 3\n",
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
            clusters: vec![
                cluster("prod.example", &[(MemoryPercent, 60)]),
                cluster("dev", &[(RestartsLastHour, 2)]),
            ],
        },
    };
    let text = toml::to_string_pretty(&host).unwrap();
    assert_eq!(toml::from_str::<Host>(&text).unwrap(), host, "{text}");

    let empty = Host {
        other: 4,
        thresholds: ThresholdSettings {
            global: ThresholdOverrides::default(),
            clusters: vec![cluster("dev", &[])],
        },
    };
    let text = toml::to_string_pretty(&empty).unwrap();
    assert!(!text.contains("thresholds"), "{text}");
}

// covers UP-67
#[test]
fn the_section_parses_exactly_when_the_file_parses_as_a_table() {
    let entry = format!("context = \"prod\"\nserver = \"{A}\"\n");
    for text in [
        "other = 4\n[thresholds]\ncpu_percent = 9223372036854775808\n".to_owned(),
        format!("other = 4\n[[thresholds.clusters]]\n{entry}cpu_percent = 18446744073709551615\n"),
        "other = 4\n[thresholds]\n\"$__toml_private_datetime\" = 1\n".to_owned(),
        format!("other = 4\n[[thresholds.clusters]]\n\"$__toml_private_datetime\" = 1\n{entry}"),
    ] {
        assert!(toml::from_str::<toml::Table>(&text).is_err(), "{text}");
        assert!(toml::from_str::<Host>(&text).is_err(), "{text}");
    }
}

// covers UP-67
#[test]
fn a_context_named_like_the_datetime_marker_is_a_plain_string() {
    let text = format!(
        "other = 4\n[[thresholds.clusters]]\ncontext = \"$__toml_private_datetime\"\n\
         server = \"{A}\"\nmemory_percent = 60\n"
    );
    assert!(toml::from_str::<toml::Table>(&text).is_ok());
    let host: Host = toml::from_str(&text).unwrap();
    assert_eq!(host.other, 4);
    let marker = host
        .thresholds
        .for_cluster("$__toml_private_datetime", Some(A));
    assert_eq!(marker.get(MemoryPercent), 60);
}
