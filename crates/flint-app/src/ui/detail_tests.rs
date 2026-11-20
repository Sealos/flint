//! View tests for the detail pane in the main window: what it shows of a fetched object, its
//! tabs and toggles (through the registry and their keys), Secret values, and the banners for
//! a stale or deleted object.
//!
//! The silent cluster never answers, so the core's fetch fails ("not connected"). Tests check
//! that failure, then show an object as a finished fetch would ([`show_fetched`]).

use flint_core::{KindId, ObjectDetail, SyncState};
use gpui_kit::test::{TestAppContextExt as _, TestQueryExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use serde_json::{Value, json};

use super::{DetailTab, DetailView, Fetch};
use crate::ui::object::MASK;
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{TestWorkspace, WAIT, objects, open_workspace, standard_sections};

/// Shows `detail` as a fetch that succeeds does ([`DetailView::show_fetched`]).
fn show_fetched(cx: &mut TestAppContext, t: &TestWorkspace, detail: ObjectDetail) {
    let view = t.detail(cx).expect("the detail pane is open");
    cx.update_window(t.window, |_, window, cx| {
        view.update(cx, |view, cx| view.show_fetched(detail, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
}

/// An object as the core returns it: managedFields split off, YAML rendered.
fn fetched(mut object: Value, events: Vec<Value>) -> ObjectDetail {
    let managed_fields = object["metadata"]
        .as_object_mut()
        .and_then(|m| m.remove("managedFields"));
    ObjectDetail {
        yaml: serde_saphyr::to_string(&object).unwrap(),
        object,
        managed_fields,
        events: events
            .into_iter()
            .map(|e| serde_json::from_value(e).unwrap())
            .collect(),
        events_truncated: false,
        events_error: None,
    }
}

fn pod(name: &str) -> ObjectDetail {
    fetched(
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": name,
                "namespace": "demo",
                "uid": format!("uid-{name}"),
                "resourceVersion": "1",
                "creationTimestamp": "2026-01-01T00:00:00Z",
                "labels": {"app": "api", "tier": "backend"},
                "annotations": {"note": "hello"},
                "ownerReferences": [{
                    "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "api-5d9f",
                    "uid": "rs-1", "controller": true
                }],
                "managedFields": [{"manager": "kubectl", "operation": "Apply"}]
            },
            "spec": {"containers": [{"name": "app", "image": "nginx"}]},
            "status": {"conditions": [{
                "type": "Ready", "status": "False", "reason": "ContainersNotReady",
                "message": "containers with unready status: [app]",
                "lastTransitionTime": "2026-01-01T00:01:00Z"
            }]}
        }),
        vec![json!({
            "metadata": {"name": format!("{name}.1"), "namespace": "demo"},
            "involvedObject": {"kind": "Pod", "name": name, "namespace": "demo"},
            "type": "Warning", "reason": "BackOff", "message": "Back-off restarting",
            "count": 3, "lastTimestamp": "2026-01-01T00:02:00Z"
        })],
    )
}

fn view(cx: &mut TestAppContext, t: &TestWorkspace) -> Entity<DetailView> {
    t.detail(cx).expect("the detail pane is open")
}

/// Opens `kind` on a connected `alpha` with `rows`, and selects `name` in `demo`.
async fn open_and_select(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    kind: KindId,
    rows: &[(&str, &str, i64)],
    name: &str,
) {
    t.connect_alpha(cx, standard_sections()).await;
    t.open_kind(
        cx,
        kind.plural(),
        objects(kind.clone(), rows, SyncState::Synced, 1),
    )
    .await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": name}),
    )
    .unwrap();
}

/// Waits until the core's fetch has failed (the cluster is not connected in the core).
async fn wait_for_failed_fetch(cx: &mut TestAppContext, t: &TestWorkspace) {
    let view = view(cx, t);
    cx.wait_for(t.window, WAIT, move |_, cx| {
        matches!(&view.read(cx).fetch, Fetch::Failed(e) if e.contains("not connected"))
    })
    .await;
}

// covers UP-23: the pane shows the object's metadata, labels, owners, conditions and events,
// and read-only YAML without managedFields until asked. Its tabs switch with the registry, their
// keys and clicks, and moving to another row keeps the tab.
#[gpui_kit::test]
async fn the_detail_pane_shows_the_fetched_object(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_and_select(
        cx,
        &t,
        KindId::pods(),
        &[("demo", "api", 0), ("demo", "web", 1)],
        "api",
    )
    .await;
    wait_for_failed_fetch(cx, &t).await;
    assert!(t.find(cx, "detail").is_some());

    show_fetched(cx, &t, pod("api"));
    let v = view(cx, &t);
    let (overview, events) = v.read_with(cx, |v, _| match &v.fetch {
        Fetch::Loaded { detail, overview } => ((**overview).clone(), detail.events.len()),
        _ => panic!("not loaded"),
    });
    assert_eq!(overview.name, "api");
    assert_eq!(overview.namespace.as_deref(), Some("demo"));
    assert_eq!(
        overview.labels,
        [
            ("app".to_owned(), "api".to_owned()),
            ("tier".to_owned(), "backend".to_owned())
        ]
    );
    assert_eq!(overview.owners.len(), 1);
    assert!(overview.owners[0].controller);
    assert_eq!(overview.conditions[0].type_, "Ready");
    assert_eq!(overview.conditions[0].status, "False");
    assert_eq!(events, 1);
    let yaml = v.read_with(cx, |v, _| v.yaml_text.clone());
    assert!(yaml.contains("name: api"), "{yaml}");
    assert!(!yaml.contains("managedFields"), "{yaml}");
    // Labels start open, annotations closed.
    assert_eq!(
        v.read_with(cx, |v, _| (v.show_labels, v.show_annotations)),
        (true, false)
    );
    t.dispatch(cx, "detail::ToggleLabels", Value::Null).unwrap();
    t.dispatch(cx, "detail::ToggleAnnotations", Value::Null)
        .unwrap();
    assert_eq!(
        v.read_with(cx, |v, _| (v.show_labels, v.show_annotations)),
        (false, true)
    );
    // Not a Secret: no values to reveal.
    assert!(!t.available(cx).contains(&"detail::ToggleSecretValue"));

    // Show YAML's key shows the YAML, where managedFields can be shown.
    t.press_key_of(cx, "detail::ShowYaml");
    assert_eq!(v.read_with(cx, |v, _| v.tab()), DetailTab::Yaml);
    let yaml_tab = cx
        .update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            window.find_by_label("YAML").selected()
        })
        .unwrap();
    assert_eq!(yaml_tab, Some(true));
    assert_eq!(
        t.label(cx, "toggle-managed-fields").as_deref(),
        Some("Show managedFields")
    );
    t.click(cx, "toggle-managed-fields");
    let shown = v.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        shown.read(cx).yaml_text.contains("managedFields")
    })
    .await;
    assert_eq!(
        t.label(cx, "toggle-managed-fields").as_deref(),
        Some("Hide managedFields")
    );

    // The next row opens on the same tab; Show Overview's key goes back to the overview.
    t.dispatch(cx, "resource_list::SelectNext", Value::Null)
        .unwrap();
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    let next = view(cx, &t);
    assert_eq!(next.read_with(cx, |v, _| v.object().name.clone()), "web");
    assert_eq!(next.read_with(cx, |v, _| v.tab()), DetailTab::Yaml);
    t.press_key_of(cx, "detail::ShowOverview");
    assert_eq!(next.read_with(cx, |v, _| v.tab()), DetailTab::Overview);
    // The tabs are clickable too.
    cx.update_window(t.window, |_, window, cx| {
        window.render_frame(cx);
        let tab = window.find_by_label("YAML");
        window.click(tab.path().last().unwrap().clone(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(next.read_with(cx, |v, _| v.tab()), DetailTab::Yaml);
}

// covers UP-23: a Secret's values stay masked, in the Overview and in the YAML, until revealed
// one key at a time.
#[gpui_kit::test]
async fn secret_values_stay_masked_until_revealed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let secrets = KindId::new("", "secrets");
    open_and_select(cx, &t, secrets, &[("demo", "db-creds", 0)], "db-creds").await;
    wait_for_failed_fetch(cx, &t).await;
    // "hunter2", base64.
    show_fetched(
        cx,
        &t,
        fetched(
            json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "type": "Opaque",
                "metadata": {"name": "db-creds", "namespace": "demo", "uid": "uid-db-creds",
                             "resourceVersion": "1"},
                "data": {"password": "aHVudGVyMg=="}
            }),
            Vec::new(),
        ),
    );
    let v = view(cx, &t);
    assert!(v.read_with(cx, |v, _| v.revealed.is_empty()));
    assert_eq!(
        t.description(cx, "reveal-password").as_deref(),
        Some("Reveal value")
    );
    assert!(t.available(cx).contains(&"detail::ToggleSecretValue"));
    assert!(matches!(
        t.dispatch(cx, "detail::ToggleSecretValue", json!({"key": "token"})),
        Err(ActionError::Rejected { .. })
    ));

    t.click(cx, "reveal-password");
    assert!(v.read_with(cx, |v, _| v.revealed.contains("password")));
    assert_eq!(
        t.description(cx, "reveal-password").as_deref(),
        Some("Hide value")
    );
    t.dispatch(cx, "detail::ToggleSecretValue", json!({"key": "password"}))
        .unwrap();
    assert!(v.read_with(cx, |v, _| v.revealed.is_empty()));

    t.dispatch(cx, "detail::ShowYaml", Value::Null).unwrap();
    let yaml = v.read_with(cx, |v, _| v.yaml_text.clone());
    assert!(yaml.contains(MASK), "{yaml}");
    assert!(
        !yaml.contains("aHVudGVyMg") && !yaml.contains("hunter2"),
        "{yaml}"
    );
}

// covers UP-20, UP-23: the pane never passes stale data off as current. A refresh that fails
// keeps the last copy under an error banner; an object deleted from the list stays shown,
// marked as deleted. Esc in the list closes the pane.
#[gpui_kit::test]
async fn banners_mark_a_stale_or_deleted_object(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_and_select(
        cx,
        &t,
        KindId::pods(),
        &[("demo", "api", 0), ("demo", "web", 0)],
        "api",
    )
    .await;
    wait_for_failed_fetch(cx, &t).await;
    show_fetched(cx, &t, pod("api"));
    let v = view(cx, &t);

    t.click(cx, "detail-refresh");
    let refreshed = v.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let v = refreshed.read(cx);
        !v.fetching && v.refresh_error.is_some()
    })
    .await;
    let (loaded, error) = v.read_with(cx, |v, _| {
        (
            matches!(v.fetch, Fetch::Loaded { .. }),
            v.refresh_error.clone().unwrap(),
        )
    });
    assert!(loaded, "the last copy stays");
    assert!(error.contains("not connected"), "{error}");
    assert!(!v.read_with(cx, |v, _| v.gone));

    // The watch reports api deleted.
    t.feed_list(
        cx,
        objects(KindId::pods(), &[("demo", "web", 0)], SyncState::Synced, 2),
    )
    .await;
    assert!(v.read_with(cx, |v, _| v.gone));
    assert!(t.detail(cx).is_some());

    // Esc, with focus on the list, clears the selection and closes the pane.
    let list = t.list(cx).unwrap();
    let focus = list.read_with(cx, |l, _| l.focus_handle().clone());
    cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    t.press(cx, "escape");
    assert!(t.detail(cx).is_none());
    assert_eq!(t.selected(cx), None);
}

// covers UP-04, UP-23: Cmd-R (Refresh and Retry Connection share it) fetches the shown object
// again on a connected cluster, and retries the connection on a failed one.
#[gpui_kit::test]
async fn the_refresh_key_refetches_or_retries(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_and_select(cx, &t, KindId::pods(), &[("demo", "api", 0)], "api").await;
    wait_for_failed_fetch(cx, &t).await;
    show_fetched(cx, &t, pod("api"));
    let v = view(cx, &t);
    assert!(v.read_with(cx, |v, _| v.refresh_error.is_none()));
    let attempts = t.connection_attempts("alpha");

    t.press_key_of(cx, "detail::Refresh");
    let refetched = v.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        refetched.read(cx).refresh_error.is_some()
    })
    .await;
    assert_eq!(t.connection_attempts("alpha"), attempts, "no reconnect");

    t.activate(cx, "beta").await;
    t.wait_for_state(cx, "beta", |s| {
        matches!(s, flint_core::ConnectionState::Failed(_))
    })
    .await;
    let attempts = t.connection_attempts("beta");
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::RetryConnection");
    t.wait_for_attempts(cx, "beta", attempts + 1).await;
}

// A closed detail pane is freed, with its YAML editor and fetch task.
#[gpui_kit::test]
async fn a_closed_detail_pane_is_released(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.open_kind(
        cx,
        "pods",
        objects(KindId::pods(), &[("demo", "api", 0)], SyncState::Synced, 1),
    )
    .await;

    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    wait_for_failed_fetch(cx, &t).await;
    show_fetched(cx, &t, pod("api"));
    let pane = view(cx, &t).downgrade();
    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    cx.run_until_parked();
    assert!(t.detail(cx).is_none());
    pane.assert_released();
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
}
