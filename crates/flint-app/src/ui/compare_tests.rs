//! What the compare view says about each cluster and field.

use std::collections::BTreeMap;
use std::sync::Arc;

use flint_core::ObjectDetail;
use flint_core::read::KindId;
use serde_json::{Value, json};

use super::*;
use core::prelude::v1::test;

const EU: &str = "arn:aws:eks:eu-west-1:123456789012:cluster/payments";
const US: &str = "arn:aws:eks:us-east-1:123456789012:cluster/payments";

fn present(object: Value) -> SideRead {
    SideRead::Present(Arc::new(ObjectDetail {
        object,
        yaml: String::new(),
        managed_fields: None,
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

fn config(value: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "web", "namespace": "demo"},
        "data": {"mode": value, "same": "1"},
    })
}

fn model(target: &CompareTarget, reads: &[SideRead], show_same: bool) -> CompareModel {
    let clusters = [EU.to_owned(), US.to_owned()];
    let names = ClusterNames::new([EU, US], &BTreeMap::new());
    let comparison = compare(target, reads, CompareOptions::default());
    CompareModel::new(
        target,
        &clusters[..reads.len()],
        &names,
        &comparison,
        show_same,
    )
}

#[test]
fn each_side_names_its_exact_cluster() {
    let target = target(KindId::new("", "configmaps"));
    let shown = model(
        &target,
        &[present(config("a")), present(config("b"))],
        false,
    );
    let names = ClusterNames::new([EU, US], &BTreeMap::new());
    assert_eq!(shown.headers[0].name, names.exact(EU));
    assert_eq!(shown.headers[1].name, names.exact(US));
    assert_ne!(shown.headers[0].name, shown.headers[1].name);
    assert_eq!(shown.title, "configmaps demo/web");
}

#[test]
fn only_differences_show_until_asked_for_the_rest() {
    let target = target(KindId::new("", "configmaps"));
    let reads = [present(config("a")), present(config("b"))];
    let shown = model(&target, &reads, false);
    assert_eq!(shown.verdict, "1 field differs.");
    assert_eq!(shown.rows.len(), 1);
    assert_eq!(shown.rows[0].path, "data.mode");
    assert_eq!(shown.rows[0].cells, ["a", "b"]);
    assert!(model(&target, &reads, true).rows.len() > 1);
}

#[test]
fn secret_values_draw_as_the_mask() {
    let target = target(KindId::new("", "secrets"));
    let secret = |value: &str| {
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": "web", "namespace": "demo"},
            "data": {"password": value},
        })
    };
    let reads = [present(secret("b25l")), present(secret("dHdv"))];
    let shown = model(&target, &reads, false);
    assert_eq!(
        shown.verdict,
        "No differences found. 1 hidden value was not compared."
    );
    assert_eq!(shown.rows[0].cells, [MASK, MASK]);
    let text = format!("{shown:?}");
    assert!(!text.contains("b25l") && !text.contains("dHdv"));
}

#[test]
fn an_unreadable_side_says_so_and_is_not_called_missing() {
    let target = target(KindId::new("", "configmaps"));
    let reads = [
        present(config("a")),
        SideRead::Unavailable(format!("\"{US}\" is not connected")),
    ];
    let shown = model(&target, &reads, true);
    assert!(shown.headers[1].problem);
    assert!(shown.headers[1].status.starts_with(UNAVAILABLE));
    assert!(shown.headers[1].status.contains("is not connected"));
    assert_eq!(
        shown.verdict,
        "Fewer than two clusters have a readable object."
    );
    assert!(shown.rows.iter().all(|row| row.cells[1] == UNAVAILABLE));
}

#[test]
fn a_side_still_reading_is_not_called_unavailable() {
    let target = target(KindId::new("", "configmaps"));
    let shown = model(&target, &[present(config("a")), SideRead::Reading], true);
    assert_eq!(shown.verdict, READING);
    assert_eq!(shown.headers[1].status, READING);
    assert!(shown.rows.iter().all(|row| row.cells[1] == READING));
}

#[test]
fn a_side_without_the_object_is_missing() {
    let target = target(KindId::new("", "configmaps"));
    let shown = model(&target, &[present(config("a")), SideRead::Missing], true);
    assert_eq!(shown.headers[1].status, MISSING);
    assert!(shown.rows.iter().all(|row| row.cells[1] == MISSING));
}

#[test]
fn cluster_text_stays_on_one_bounded_line() {
    let target = target(KindId::new("", "configmaps"));
    let long = "x".repeat(MAX_VALUE_CHARS + 1);
    let reads = [present(config("a\nb\u{202e}c")), present(config(&long))];
    let shown = model(&target, &reads, false);
    let cells = &shown.rows[0].cells;
    assert!(!cells[0].contains('\n') && !cells[0].contains('\u{202e}'));
    assert!(cells[1].ends_with(&format!("… ({} characters)", MAX_VALUE_CHARS + 1)));
}

#[test]
fn a_cluster_listed_twice_gets_one_column() {
    let clusters = distinct(vec![EU.to_owned(), US.to_owned(), EU.to_owned()]);
    assert_eq!(clusters, [EU, US]);
}
