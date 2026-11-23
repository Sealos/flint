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
fn a_cluster_name_wraps_instead_of_clipping() {
    let mut name = side_name(EU);
    let style = name.style();
    assert_eq!(style.text.white_space, Some(WhiteSpace::Normal));
    assert_eq!(style.text.text_overflow, None);
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

fn summary(name: &str, state: ConnectionState, paused: bool) -> ClusterSummary {
    ClusterSummary {
        info: flint_core::kubeconfig::ContextInfo {
            name: name.into(),
            cluster: None,
            user: None,
            namespace: None,
            server: None,
            proxy_url: None,
            source: "/k".into(),
        },
        state,
        generation: 1,
        definition: 1,
        endpoint: 1,
        confirmed: true,
        provisional: false,
        paused,
    }
}

fn connected(name: &str) -> ClusterSummary {
    let version = flint_core::ServerVersion {
        git_version: "v1".into(),
        platform: "linux/arm64".into(),
    };
    summary(name, ConnectionState::Connected(version), false)
}

fn paused(name: &str) -> ClusterSummary {
    summary(name, ConnectionState::AwaitingConfirmation, true)
}

#[test]
fn columns_are_the_origin_then_connected_and_paused_clusters() {
    let clusters = [
        summary("idle", ConnectionState::Disconnected, false),
        connected("other"),
        paused("held"),
        connected("origin"),
    ];
    assert_eq!(
        compare_clusters("origin", &clusters),
        ["origin", "other", "held"]
    );
}

#[test]
fn a_comparison_needs_another_readable_cluster() {
    let alone = [connected("origin"), paused("held")];
    assert_eq!(can_compare("origin", &alone).unwrap_err(), NO_OTHER_CLUSTER);
    let two = [connected("origin"), connected("other")];
    assert_eq!(can_compare("origin", &two), Ok(()));
    let origin_paused = [paused("origin"), connected("other")];
    assert!(
        can_compare("origin", &origin_paused)
            .unwrap_err()
            .contains(PAUSED)
    );
}

#[test]
fn paused_and_removed_clusters_are_held_back() {
    let clusters = [connected("live"), paused("held")];
    assert_eq!(held_back("live", &clusters), None);
    assert_eq!(held_back("held", &clusters), Some(PAUSED));
    assert_eq!(held_back("gone", &clusters), Some(REMOVED));
}

#[test]
fn a_refusal_is_one_sentence() {
    assert_eq!(
        refused("alpha is not connected."),
        "Can't compare this object: alpha is not connected."
    );
}

#[test]
fn every_compare_action_is_bound() {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};
    assert_binds_scope(Scope::Compare, CompareView::bind_actions);
}

fn at(mut cluster: ClusterSummary, server: &str, definition: u64) -> ClusterSummary {
    cluster.info.server = Some(server.to_owned());
    cluster.definition = definition;
    cluster
}

// covers #449: a column stays on the definition it opened on, also through a move back.
#[test]
fn a_column_is_held_once_its_cluster_changes() {
    let opened = at(connected(EU), "https://a.example", 1);
    let pin = opened.read_pin();
    assert_eq!(
        column_held(EU, Some(&pin), std::slice::from_ref(&opened)),
        None
    );
    for moved in [
        at(connected(EU), "https://b.example", 2),
        at(connected(EU), "https://a.example", 2),
    ] {
        assert_eq!(column_held(EU, Some(&pin), &[moved]), Some(CHANGED));
    }
    let mut held = at(connected(EU), "https://b.example", 2);
    held.paused = true;
    assert_eq!(column_held(EU, Some(&pin), &[held]), Some(PAUSED));
    assert_eq!(column_held(EU, Some(&pin), &[]), Some(REMOVED));
}

// covers #449: removing `team` must not leave two columns both called `team`.
#[test]
fn retained_columns_keep_names_told_apart() {
    const PLAIN: &str = "team";
    const ARN: &str = "arn:aws:eks:eu-west-1:123456789012:cluster/team";
    let columns = [PLAIN.to_owned(), ARN.to_owned()];
    let names = column_names(&columns, &[connected(ARN)], &BTreeMap::new());
    assert_ne!(names.exact(PLAIN), names.exact(ARN));
}

#[test]
fn a_menu_object_names_its_kind_exactly() {
    let object = CompareObject {
        cluster: EU.to_owned(),
        kind: "deployments.apps".to_owned(),
        namespace: Some("demo".to_owned()),
        name: "web".to_owned(),
    };
    assert_eq!(object.target().kind, KindId::new("apps", "deployments"));
}

// The core would answer "not connected" for this cluster, so the paused reason shows no request
// was made. The toggles change what is shown without reading again.
#[gpui_kit::test]
fn a_paused_cluster_is_not_read_and_the_controls_work(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let dir = tempfile::tempdir().unwrap();
    let core = flint_core::CoreHandle::start(flint_core::CoreConfig {
        config_dir: dir.path().to_owned(),
        settings: flint_core::Settings::default(),
        settings_error: None,
        kubeconfig_base: Vec::new(),
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    })
    .unwrap();
    let source = cx.new(|cx| ClustersModel::new(core, flint_core::Settings::default(), None, cx));
    let view = cx.new(|cx| {
        source.update(cx, |model, _| {
            model.clusters = vec![connected(EU), paused(US)];
        });
        CompareView::new(
            source.clone(),
            EU.to_owned(),
            target(KindId::new("", "configmaps")),
            vec![EU.to_owned(), US.to_owned()],
            cx,
        )
    });
    cx.run_until_parked();
    let held = format!("{UNAVAILABLE}: {PAUSED}");
    view.read_with(cx, |view, _| {
        let side = &view.model().headers[1];
        assert_eq!(side.status, held);
        assert!(side.problem);
        assert!(!view.show_same() && !view.include_volatile());
    });
    view.update(cx, |view, cx| {
        view.set_show_same(true, cx);
        view.set_include_volatile(true, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(view.show_same() && view.include_volatile());
        assert_eq!(view.model().headers[1].status, held);
    });
}
