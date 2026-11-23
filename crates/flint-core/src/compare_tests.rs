//! The cross-cluster comparison: matching, ignored fields, redaction and unreadable sides.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::cluster::ClusterError;

fn detail(object: Value) -> SideRead {
    SideRead::Present(Arc::new(ObjectDetail {
        object,
        yaml: String::new(),
        managed_fields: Some(json!([{"manager": "kubectl"}])),
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    }))
}

fn target(kind: KindId) -> CompareTarget {
    CompareTarget {
        kind,
        namespace: Some("demo".into()),
        name: "web".into(),
    }
}

fn deployments() -> CompareTarget {
    target(KindId::new("apps", "deployments"))
}

fn secrets() -> CompareTarget {
    target(KindId::new("", "secrets"))
}

fn deployment(image: &str, uid: &str) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": "web",
            "namespace": "demo",
            "uid": uid,
            "resourceVersion": uid,
            "generation": 3,
            "creationTimestamp": "2026-01-01T00:00:00Z",
            "labels": {"app.kubernetes.io/name": "web"},
            "annotations": {"deployment.kubernetes.io/revision": uid},
            "ownerReferences": [{"kind": "Thing", "name": "owner", "uid": uid}],
        },
        "spec": {"template": {"spec": {"containers": [{"name": "app", "image": image}]}}},
        "status": {"readyReplicas": 1},
    })
}

fn secret(value: &str, annotation: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": "web",
            "namespace": "demo",
            "annotations": {
                "kubectl.kubernetes.io/last-applied-configuration": annotation,
                "custom/copy": annotation,
                "meta.helm.sh/release-name": "web",
            },
        },
        "type": "Opaque",
        "data": {"password": value},
        "stringData": {"token": value},
    })
}

fn row<'a>(comparison: &'a Comparison, path: &str) -> &'a CompareRow {
    comparison
        .rows
        .iter()
        .find(|row| row.path == path)
        .unwrap_or_else(|| panic!("no row {path}"))
}

const IMAGE: &str = "spec.template.spec.containers[name=\"app\"].image";

#[test]
fn a_changed_field_is_the_only_difference() {
    let reads = [
        detail(deployment("web:1", "a")),
        detail(deployment("web:2", "b")),
    ];
    let result = compare(&deployments(), &reads, CompareOptions::default());
    let differing: Vec<&str> = result
        .rows
        .iter()
        .filter(|row| row.state == RowState::Differs)
        .map(|row| row.path.as_str())
        .collect();
    assert_eq!(differing, [IMAGE]);
    assert_eq!(result.verdict(), Verdict::Differs { fields: 1 });
    assert_eq!(
        row(&result, IMAGE).values,
        [
            FieldValue::Value(json!("web:1")),
            FieldValue::Value(json!("web:2"))
        ]
    );
}

#[test]
fn volatile_fields_are_ignored_by_default_and_kept_on_request() {
    let reads = [
        detail(deployment("web:1", "a")),
        detail(deployment("web:1", "b")),
    ];
    let result = compare(&deployments(), &reads, CompareOptions::default());
    assert_eq!(result.verdict(), Verdict::Same);
    for gone in [
        "uid",
        "resourceVersion",
        "managedFields",
        "status",
        "revision",
    ] {
        assert!(
            result.rows.iter().all(|row| !row.path.contains(gone)),
            "{gone}"
        );
    }

    let all = compare(
        &deployments(),
        &reads,
        CompareOptions {
            include_volatile: true,
        },
    );
    assert_eq!(row(&all, "metadata.uid").state, RowState::Differs);
    assert_eq!(
        row(&all, "metadata.managedFields[0].manager").state,
        RowState::Same
    );
}

#[test]
fn list_items_match_by_name_whatever_their_order() {
    let containers = |first, second| {
        json!({"kind": "Pod", "apiVersion": "v1", "spec": {"containers": [
            {"name": first, "image": format!("{first}:1")},
            {"name": second, "image": format!("{second}:1")},
        ]}})
    };
    let reads = [detail(containers("a", "b")), detail(containers("b", "a"))];
    let result = compare(&target(KindId::pods()), &reads, CompareOptions::default());
    assert_eq!(result.verdict(), Verdict::Same);
}

#[test]
fn a_key_holding_dots_is_not_a_nested_path() {
    let reads = [
        detail(deployment("web:1", "a")),
        detail(deployment("web:1", "b")),
    ];
    let result = compare(&deployments(), &reads, CompareOptions::default());
    row(&result, "metadata.labels[\"app.kubernetes.io/name\"]");
}

#[test]
fn a_string_and_a_number_are_different_values() {
    let object = |value: Value| json!({"kind": "Thing", "apiVersion": "v1", "spec": {"a": value}});
    let reads = [detail(object(json!("1"))), detail(object(json!(1)))];
    let result = compare(&deployments(), &reads, CompareOptions::default());
    assert_eq!(row(&result, "spec.a").state, RowState::Differs);
}

// Different values, and equal ones, must read the same: a comparison can't be an equality oracle.
#[test]
fn secret_values_are_never_kept_or_compared() {
    for options in [
        CompareOptions::default(),
        CompareOptions {
            include_volatile: true,
        },
    ] {
        let different = [
            detail(secret("c2VjcmV0LW9uZQ==", "copy-one")),
            detail(secret("c2VjcmV0LXR3bw==", "copy-two")),
        ];
        let equal = [
            detail(secret("c2VjcmV0LW9uZQ==", "copy-one")),
            detail(secret("c2VjcmV0LW9uZQ==", "copy-one")),
        ];
        let shown = compare(&secrets(), &different, options);
        assert_eq!(shown, compare(&secrets(), &equal, options));
        let text = format!("{shown:?}");
        for value in ["c2VjcmV0", "copy-one", "copy-two"] {
            assert!(!text.contains(value), "{value} in {text}");
        }
        for path in ["data.password", "stringData.token"] {
            let hidden = row(&shown, path);
            assert_eq!(hidden.state, RowState::Hidden);
            assert_eq!(hidden.values, [FieldValue::Hidden, FieldValue::Hidden]);
        }
        assert_eq!(
            row(&shown, "metadata.annotations[\"custom/copy\"]").state,
            RowState::Hidden
        );
        assert_eq!(
            row(
                &shown,
                "metadata.annotations[\"meta.helm.sh/release-name\"]"
            )
            .values[0],
            FieldValue::Value(json!("web"))
        );
        assert!(matches!(
            shown.verdict(),
            Verdict::SameExceptHidden { fields: 3.. }
        ));
    }
}

#[test]
fn a_secret_key_on_one_side_is_a_difference_without_its_value() {
    let mut extra = secret("b25l", "x");
    extra["data"]["extra"] = json!("dG9wLXNlY3JldA==");
    let reads = [detail(secret("b25l", "x")), detail(extra)];
    let result = compare(&secrets(), &reads, CompareOptions::default());
    let key = row(&result, "data.extra");
    assert_eq!(key.state, RowState::Differs);
    assert_eq!(key.values, [FieldValue::NotSet, FieldValue::Hidden]);
    assert!(!format!("{result:?}").contains("dG9wLXNlY3JldA"));
}

#[test]
fn a_secret_field_of_an_unexpected_shape_is_hidden_whole() {
    let mut odd = secret("b25l", "x");
    odd["data"] = json!("raw-value");
    odd["metadata"]["annotations"] = json!("raw-annotation");
    let reads = [detail(odd.clone()), detail(odd)];
    let result = compare(&secrets(), &reads, CompareOptions::default());
    let text = format!("{result:?}");
    assert!(!text.contains("raw-value") && !text.contains("raw-annotation"));
    assert_eq!(row(&result, "data").values[0], FieldValue::Hidden);
    assert_eq!(
        row(&result, "metadata.annotations").values[0],
        FieldValue::Hidden
    );
}

#[test]
fn only_the_servers_not_found_for_this_kind_means_missing() {
    let kind = deployments();
    let summary = |text: &str| ClusterError::Api(text.to_owned()).summary().to_string();
    let not_found = summary("NotFound: deployments.apps \"web\" not found");
    assert!(matches!(
        SideRead::from_fetch(&kind, Err(not_found)),
        SideRead::Missing
    ));
    for error in [
        summary("NotFound: the server could not find the requested resource"),
        summary("NotFound: namespaces \"demo\" not found"),
        summary("forbidden: User \"jane\" cannot get deployments.apps"),
        "\"prod\" is not connected".to_owned(),
        "\"prod\" does not serve deployments.apps".to_owned(),
        "the core stopped before answering".to_owned(),
    ] {
        assert!(
            matches!(
                SideRead::from_fetch(&kind, Err(error.clone())),
                SideRead::Unavailable(ref why) if *why == error
            ),
            "{error}"
        );
    }
}

#[test]
fn a_missing_side_is_told_from_an_unreadable_one() {
    let reads = [
        detail(deployment("web:1", "a")),
        detail(deployment("web:1", "b")),
        SideRead::Missing,
        SideRead::Unavailable("\"prod\" is not connected".into()),
    ];
    let result = compare(&deployments(), &reads, CompareOptions::default());
    assert_eq!(result.verdict(), Verdict::MatchOnSome);
    assert_eq!(result.sides[2], SideState::Missing);
    assert_eq!(
        result.sides[3],
        SideState::Unavailable("\"prod\" is not connected".into())
    );
    let image = row(&result, IMAGE);
    assert_eq!(image.state, RowState::Same);
    assert_eq!(image.values[2], FieldValue::NoObject);
    assert_eq!(image.values[3], FieldValue::Unknown);
}

#[test]
fn fewer_than_two_objects_are_not_called_the_same() {
    let reads = [
        detail(deployment("web:1", "a")),
        SideRead::Unavailable("timed out".into()),
    ];
    let result = compare(&deployments(), &reads, CompareOptions::default());
    assert_eq!(result.verdict(), Verdict::NotComparable);
    assert!(
        result
            .rows
            .iter()
            .all(|row| row.state == RowState::NotCompared)
    );

    let none = [SideRead::Missing, SideRead::Unavailable("x".into())];
    let result = compare(&deployments(), &none, CompareOptions::default());
    assert_eq!(result.verdict(), Verdict::NotComparable);
    assert!(result.rows.is_empty());
}

#[test]
fn an_answer_still_on_its_way_gives_no_verdict() {
    let reads = [
        detail(deployment("web:1", "a")),
        detail(deployment("web:2", "b")),
        SideRead::Reading,
    ];
    let result = compare(&deployments(), &reads, CompareOptions::default());
    assert_eq!(result.verdict(), Verdict::Reading);
    assert_eq!(row(&result, IMAGE).values[2], FieldValue::Unknown);
}
