//! View tests for the navigation in the main window: what discovery puts in it, opening kinds
//! from it, the failure states that offer a retry, and what a cluster switch does to it.

use flint_core::{ConnectionState, Event, KindId, SyncState};
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use serde_json::{Value, json};

use super::NavPanel;
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{TestWorkspace, objects, open_workspace, pods, standard_sections};

fn nav(cx: &mut TestAppContext, t: &TestWorkspace) -> Entity<NavPanel> {
    t.workspace.read_with(cx, |ws, _| ws.nav_view())
}

fn nav_selected(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<KindId> {
    let nav = nav(cx, t);
    nav.read_with(cx, |nav, _| nav.selected.clone())
}

fn collapsed(cx: &mut TestAppContext, t: &TestWorkspace, section: &str) -> Option<bool> {
    let nav = nav(cx, t);
    nav.read_with(cx, |nav, _| nav.is_collapsed(section))
}

fn open_kind(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<KindId> {
    let list = t.list(cx)?;
    Some(list.read_with(cx, |l, _| l.kind().clone()))
}

// covers UP-03, UP-20, UP-22: the cluster shows "connecting", then "connected" with the server
// version. Discovery fills the navigation (Workloads open, the other sections and the custom
// resources collapsed, a CRD under its API group). A kind opens from the registry (a CRD by
// plural) or a click on its entry: its list opens with focus, it is marked in the navigation,
// and its section opens. Section headers toggle on click. Rows follow the watch.
#[gpui_kit::test]
async fn discovery_fills_the_navigation_and_kinds_open_from_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.activate(cx, "kind-alpha").await;
    t.wait_for_state(cx, "kind-alpha", |s| {
        matches!(s, ConnectionState::Connecting)
    })
    .await;
    let state = t.description(cx, "cluster-kind-alpha").unwrap();
    assert!(state.contains("\nConnecting\n"), "{state}");
    // Connecting, before discovery: nothing to open yet.
    assert!(!nav(cx, &t).read_with(cx, |n, _| n.has_sections()));
    assert!(matches!(
        t.dispatch(cx, "workspace::OpenView", json!({"kind": "pods"})),
        Err(ActionError::Rejected { .. })
    ));

    t.connect_alpha(cx, standard_sections()).await;
    let state = t.description(cx, "cluster-kind-alpha").unwrap();
    assert!(state.contains("Connected, Kubernetes v1.33.1"), "{state}");
    assert!(nav(cx, &t).read_with(cx, |n, _| n.has_sections()));
    assert_eq!(collapsed(cx, &t, "Workloads"), Some(false));
    assert_eq!(collapsed(cx, &t, "Network"), Some(true));
    assert_eq!(collapsed(cx, &t, "example.com"), Some(true));
    assert_eq!(nav_selected(cx, &t), None);

    // A custom resource, by plural without its group.
    t.open_kind(
        cx,
        "widgets",
        objects(
            KindId::new("example.com", "widgets"),
            &[("demo", "w1", 0)],
            SyncState::Synced,
            1,
        ),
    )
    .await;
    let widgets = KindId::new("example.com", "widgets");
    assert_eq!(open_kind(cx, &t), Some(widgets.clone()));
    assert_eq!(nav_selected(cx, &t), Some(widgets));
    assert_eq!(collapsed(cx, &t, "example.com"), Some(false));
    let list_focused = cx
        .update_window(t.window, |_, window, cx| {
            let list = t.workspace.read(cx).list_view().unwrap();
            list.read(cx).focus_handle().is_focused(window)
        })
        .unwrap();
    assert!(list_focused, "an opened list takes focus");

    // A click on an entry: Deployments.
    t.click(cx, "nav-deployments.apps");
    assert_eq!(open_kind(cx, &t), Some(KindId::new("apps", "deployments")));
    assert_eq!(
        nav_selected(cx, &t),
        Some(KindId::new("apps", "deployments"))
    );

    // Pods, then the watch adds a pod and removes another.
    t.open_kind(cx, "pods", pods(&[("demo", "api", 0), ("demo", "web", 0)]))
        .await;
    assert_eq!(nav_selected(cx, &t), Some(KindId::pods()));
    assert_eq!(t.row_count(cx), 2);
    t.feed_list(
        cx,
        objects(
            KindId::pods(),
            &[("demo", "api", 0), ("demo", "db", 0)],
            SyncState::Synced,
            2,
        ),
    )
    .await;
    assert!(t.find(cx, "row-uid-db").is_some());
    assert!(t.find(cx, "row-uid-web").is_none());

    // Section headers toggle on click (Network is the second), and by title.
    t.click(cx, ("nav-section", 1usize));
    assert_eq!(collapsed(cx, &t, "Network"), Some(false));
    t.dispatch(
        cx,
        "workspace::ToggleNavSection",
        json!({"section": "network"}),
    )
    .unwrap();
    assert_eq!(collapsed(cx, &t, "Network"), Some(true));
    // Unknown kinds are refused.
    assert!(matches!(
        t.dispatch(cx, "workspace::OpenView", json!({"kind": "gadgets"})),
        Err(ActionError::Rejected { .. })
    ));
}

/// Waits until kind-beta has started connecting `attempts` times, that attempt has failed, and the
/// main area's Retry button is drawn again.
async fn retried(cx: &mut TestAppContext, t: &TestWorkspace, attempts: usize) {
    t.wait_for_attempts(cx, "kind-beta", attempts).await;
    t.wait_for_state(cx, "kind-beta", |s| matches!(s, ConnectionState::Failed(_)))
        .await;
    t.wait_for_element(cx, "connection-retry").await;
}

// covers UP-04: a connection that fails says so in the main area with a Retry button (once:
// the navigation has none), and Retry Connection is offered (it is not while connecting). A
// failed discovery offers a Retry in the navigation.
#[gpui_kit::test]
async fn failures_offer_a_retry(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.activate(cx, "kind-alpha").await;
    assert!(!t.available(cx).contains(&"workspace::RetryConnection"));
    assert!(t.find(cx, "nav-retry").is_none());

    t.activate(cx, "kind-beta").await;
    t.wait_for_state(cx, "kind-beta", |s| matches!(s, ConnectionState::Failed(_)))
        .await;
    t.wait_for_element(cx, "connection-retry").await;
    assert!(t.find(cx, "nav-retry").is_none());
    assert!(nav(cx, &t).read_with(cx, |n, _| n.connection_failed));
    assert!(t.available(cx).contains(&"workspace::RetryConnection"));
    // Each Retry connects again, and this server refuses again. While that attempt runs the
    // cluster is "connecting" and no Retry button is drawn, so each step waits for the attempt
    // to start, then for it to fail and the button to be back.
    let attempts = t.connection_attempts("kind-beta");
    t.click(cx, "connection-retry");
    retried(cx, &t, attempts + 1).await;
    t.dispatch(cx, "workspace::RetryConnection", Value::Null)
        .unwrap();
    retried(cx, &t, attempts + 2).await;

    // Back on kind-alpha, connected, a discovery that failed with nothing to show: the navigation's
    // Retry connects (and discovers) again.
    t.activate(cx, "kind-alpha").await;
    assert!(t.find(cx, "nav-retry").is_none());
    t.wait_for_state(cx, "kind-alpha", |s| {
        matches!(s, ConnectionState::Connecting)
    })
    .await;
    t.feed(
        cx,
        Event::ConnectionChanged {
            cluster: "kind-alpha".into(),
            state: ConnectionState::Connected(flint_core::ServerVersion {
                git_version: "v1.33.1".into(),
                platform: "linux/arm64".into(),
            }),
        },
    );
    t.feed(
        cx,
        Event::DiscoveryChanged {
            cluster: "kind-alpha".into(),
            generation: crate::ui::support_tests::CURRENT_GENERATION,
            discovery: None,
            error: Some("the server could not find the requested resource".into()),
        },
    );
    t.wait_for_element(cx, "nav-retry").await;
    assert!(t.available(cx).contains(&"workspace::RetryConnection"));
    assert!(!nav(cx, &t).read_with(cx, |n, _| n.has_sections()));
    let attempts = t.connection_attempts("kind-alpha");
    t.click(cx, "nav-retry");
    t.wait_for_attempts(cx, "kind-alpha", attempts + 1).await;
}

// covers UP-24: switching clusters closes the old cluster's list and detail pane, and the old
// list is released (with it, its subscription to the old cluster). Switching back opens the
// kind that was open, as a new list that loads fresh data rather than showing the old rows.
#[gpui_kit::test]
async fn switching_clusters_closes_views_and_reopens_the_last_kind(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.open_kind(cx, "pods", pods(&[("demo", "api", 0)])).await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert!(t.detail(cx).is_some());
    let old = t.list(cx).unwrap().downgrade();
    let old_id = old.entity_id();

    t.activate(cx, "kind-beta").await;
    assert!(t.list(cx).is_none());
    assert!(t.detail(cx).is_none());
    assert_eq!(nav_selected(cx, &t), None);
    assert!(t.find(cx, "row-uid-api").is_none());
    cx.run_until_parked();
    assert!(old.upgrade().is_none(), "the old list is released");

    t.connect_alpha(cx, standard_sections()).await;
    assert_eq!(open_kind(cx, &t), Some(KindId::pods()));
    assert_ne!(t.list(cx).unwrap().entity_id(), old_id, "a new list");
    assert_eq!(nav_selected(cx, &t), Some(KindId::pods()));
    assert!(t.detail(cx).is_none());
    assert_eq!(t.selected(cx), None);
    assert!(t.find(cx, "row-uid-api").is_none(), "no stale rows");
}
