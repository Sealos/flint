//! List rows and the GitOps manager read from their own metadata: no extra request, one detection
//! ([`crate::gitops`]), and nothing about a Secret's manager reaches its list filter.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::read::KindInfo;
use crate::store::{KindSnapshot, Store};
use crate::view::{ViewQuery, compute};

const TRACKING: &str = "argocd.argoproj.io/tracking-id";

fn wire_row(name: &str, labels: Value, annotations: Value) -> Value {
    json!({"cells": [name], "object": {"kind": "PartialObjectMetadata", "metadata": {
        "name": name, "namespace": "demo", "uid": name,
        "labels": labels, "annotations": annotations,
    }}})
}

fn rows(wire: Vec<Value>) -> Vec<Arc<Row>> {
    let table = json!({"kind": "Table", "metadata": {"resourceVersion": "1"},
        "columnDefinitions": [{"name": "Name", "type": "string", "format": "name"}],
        "rows": wire});
    let object: WireObject = serde_json::from_value(table).unwrap();
    let mut layout = None;
    object_rows(
        object,
        true,
        RowKind::default(),
        &mut layout,
        &mut Interner::default(),
        0,
    )
    .into_iter()
    .map(Arc::new)
    .collect()
}

fn mixed() -> Vec<Arc<Row>> {
    rows(vec![
        wire_row(
            "argo",
            json!({}),
            json!({TRACKING: "shop:apps/Deployment:demo/argo"}),
        ),
        wire_row(
            "flux",
            json!({"kustomize.toolkit.fluxcd.io/name": "apps",
                   "kustomize.toolkit.fluxcd.io/namespace": "flux-system"}),
            json!({}),
        ),
        wire_row(
            "helm",
            json!({}),
            json!({"helm.toolkit.fluxcd.io/name": "web"}),
        ),
        wire_row(
            "lone",
            json!({"app.kubernetes.io/instance": "shop"}),
            json!({}),
        ),
        wire_row("plain", json!({"app": "web"}), json!({"note": "text"})),
    ])
}

fn filtered(kind: &str, rows: Vec<Arc<Row>>, text: &str) -> Vec<String> {
    let snapshot = KindSnapshot {
        kind: crate::read::KindId::new("", kind),
        rows,
        ..Default::default()
    };
    let query = ViewQuery {
        text: text.into(),
        ..Default::default()
    };
    compute(&snapshot, &query, 0)
        .into_iter()
        .map(|i| snapshot.rows[i as usize].name.to_string())
        .collect()
}

// covers UP-23: the manager comes from the metadata the Table already carries, and the common
// instance label alone is not one.
#[test]
fn a_row_names_its_argo_or_flux_manager() {
    let rows = mixed();
    let managers: Vec<_> = rows.iter().map(|r| r.managed_by()).collect();
    assert_eq!(
        managers,
        [
            Some("Argo CD: shop"),
            Some("Flux: flux-system/apps"),
            Some("Flux HelmRelease: web"),
            None,
            None,
        ]
    );
}

// covers UP-23: a list filters to every managed object, or to one controller's.
#[test]
fn a_list_filters_to_managed_objects() {
    assert_eq!(
        filtered("pods", mixed(), "managed"),
        ["argo", "flux", "helm"]
    );
    assert_eq!(filtered("pods", mixed(), "ARGO"), ["argo"]);
    assert_eq!(filtered("pods", mixed(), "flux"), ["flux", "helm"]);
}

// covers UP-23: the row's reserved entry is not a label to search, and a label cannot forge it.
#[test]
fn the_manager_entry_is_not_a_searchable_label() {
    assert!(filtered("pods", mixed(), "flint:managed").is_empty());
    let forged = rows(vec![wire_row(
        "forged",
        json!({crate::gitops::ROW_MANAGER_KEY: "Argo CD: fake"}),
        json!({}),
    )]);
    assert_eq!(forged[0].managed_by(), None);
}

// covers UP-10: a Secret list's filter cannot be used to probe a masked annotation.
#[test]
fn a_secret_list_does_not_match_on_its_manager() {
    let secrets = rows(vec![wire_row(
        "tracked",
        json!({}),
        json!({TRACKING: "probe-value:/Secret:demo/tracked"}),
    )]);
    for text in ["managed", "argo", "probe-value"] {
        assert!(
            filtered("secrets", secrets.clone(), text).is_empty(),
            "{text}"
        );
    }
}

// A marker that is not text is skipped without failing the list.
#[test]
fn a_marker_that_is_not_text_does_not_fail_the_list() {
    let odd = rows(vec![wire_row("odd", json!({}), json!({TRACKING: 7}))]);
    assert_eq!(odd.len(), 1);
    assert_eq!(odd[0].managed_by(), None);
}

fn deployments() -> KindInfo {
    KindInfo {
        resource: kube::core::ApiResource {
            group: "apps".into(),
            version: "v1".into(),
            api_version: "apps/v1".into(),
            kind: "Deployment".into(),
            plural: "deployments".into(),
        },
        namespaced: true,
        verbs: Vec::new(),
        singular: "deployment".into(),
        short_names: Vec::new(),
        scale: Default::default(),
    }
}

/// The manager a watched Deployment's row shows, carrying `labels` and tracking id `id`.
fn watched_manager(labels: Value, id: &str) -> Option<String> {
    let table = json!({"kind": "Table", "metadata": {"resourceVersion": "1"},
        "columnDefinitions": [{"name": "Name", "type": "string", "format": "name"}],
        "rows": [wire_row("web", labels, json!({TRACKING: id}))]});
    let mut store = Store::new(true).with_kind(&deployments());
    store.apply(serde_json::from_value(table).unwrap(), false, 0);
    let snapshot = store.snapshot(KindSnapshot::default());
    snapshot.rows[0].managed_by().map(str::to_owned)
}

// covers UP-23: an id copied from another group or kind does not outrank Flux, as in the detail
// view, and the id naming this very object still does.
#[test]
fn a_flux_object_with_an_id_copied_from_another_kind_is_not_argo() {
    let flux = || {
        json!({"kustomize.toolkit.fluxcd.io/name": "apps",
               "kustomize.toolkit.fluxcd.io/namespace": "flux-system"})
    };
    for copied in [
        "shop:apps/StatefulSet:demo/web",
        "shop:example.com/Deployment:demo/web",
        "shop:/Deployment:demo/web",
    ] {
        assert_eq!(
            watched_manager(flux(), copied).as_deref(),
            Some("Flux: flux-system/apps"),
            "{copied}"
        );
    }
    assert_eq!(
        watched_manager(flux(), "shop:apps/Deployment:demo/web").as_deref(),
        Some("Argo CD: shop")
    );
}

// covers UP-23: without another manager, a copied id still counts, so nothing loses its marker.
#[test]
fn a_copied_id_alone_still_names_argo() {
    assert_eq!(
        watched_manager(json!({}), "shop:apps/StatefulSet:demo/web").as_deref(),
        Some("Argo CD: shop")
    );
}
