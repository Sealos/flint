//! View tests for the workspace's actions: through the registry, through clicks and keys, and
//! with focus on the workspace itself (the actions forwarded to the list and detail pane).

use flint_core::settings::AiExposureState;
use flint_core::{KindId, NavEntry};
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use crate::ui::detail::DetailTab;
use crate::ui::registry::{self, ActionError};
use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace, pods};

/// Selects `cluster` and waits until the workspace follows.
async fn activate(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &'static str) {
    dispatch(
        cx,
        t,
        "workspace::SelectCluster",
        json!({ "cluster": cluster }),
    )
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some(cluster)
    })
    .await;
}

/// Opens Pods on the active cluster with `rows`, fed by hand.
async fn open_pods(cx: &mut TestAppContext, t: &TestWorkspace, rows: &[(&str, &str, i64)]) {
    let entry = NavEntry {
        id: KindId::pods(),
        label: "Pods".into(),
        kind: "Pod".into(),
        api_version: "v1".into(),
        namespaced: true,
        listable: true,
        watchable: true,
    };
    let workspace = t.workspace.clone();
    let snapshot = pods(rows);
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.open_kind(&entry, window, cx);
            let list = ws.list.clone().unwrap();
            list.update(cx, |list, cx| {
                list.stop_watching();
                list.set_snapshot(snapshot, cx);
            });
        })
    })
    .unwrap();
    let first = gpui_kit::SharedString::from(format!("row-uid-{}", rows[0].1));
    cx.wait_for(t.window, WAIT, move |window, _| {
        window.try_find(first.clone()).is_some()
    })
    .await;
}

fn dispatch(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    name: &str,
    args: Value,
) -> Result<(), ActionError> {
    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch(name, args, window, cx)
    })
    .unwrap()
}

fn selected(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    t.workspace.read_with(cx, |ws, cx| {
        ws.list.as_ref().and_then(|l| l.read(cx).selected_name())
    })
}

fn focus_workspace(cx: &mut TestAppContext, t: &TestWorkspace) {
    let focus = t.workspace.read_with(cx, |ws, _| ws.focus.clone());
    cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    cx.run_until_parked();
}

// A registered action reaches the workspace with nothing else focused, does what the cluster
// row's click does, and is refused when it would do nothing.
#[gpui_kit::test]
async fn registered_actions_drive_the_workspace(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;

    // With no cluster active, cluster-bound actions are not offered.
    let names = |cx: &mut TestAppContext| -> Vec<&'static str> {
        cx.update_window(t.window, |_, window, cx| {
            registry::available(window, cx).map(|s| s.name).collect()
        })
        .unwrap()
    };
    let offered = names(cx);
    assert!(offered.contains(&"workspace::SelectCluster"), "{offered:?}");
    assert!(offered.contains(&"flint::Quit"));
    assert!(!offered.contains(&"workspace::OpenView"), "{offered:?}");
    assert!(!offered.contains(&"workspace::ShowAllNamespaces"));
    assert!(!offered.contains(&"workspace::RetryConnection"));

    assert!(matches!(
        dispatch(
            cx,
            &t,
            "workspace::SelectCluster",
            json!({"cluster": "gamma"})
        ),
        Err(ActionError::Rejected { .. })
    ));
    activate(cx, &t, "beta").await;
    let offered = names(cx);
    assert!(offered.contains(&"workspace::OpenView"), "{offered:?}");
    assert!(offered.contains(&"workspace::ShowAllNamespaces"));
    // No list or detail pane is open, so their actions are not offered.
    assert!(!offered.contains(&"resource_list::SelectNext"));
    assert!(!offered.contains(&"detail::Close"));
    assert_eq!(
        dispatch(cx, &t, "detail::Refresh", Value::Null),
        Err(ActionError::NotAvailable("detail::Refresh".into()))
    );
    // Hiding a hidden cluster would do nothing.
    assert!(matches!(
        dispatch(cx, &t, "workspace::HideFromAi", json!({"cluster": "beta"})),
        Err(ActionError::Rejected { .. })
    ));
    // Namespace names are trimmed; blank ones are refused.
    dispatch(
        cx,
        &t,
        "workspace::SetNamespaces",
        json!({"namespaces": [" demo "]}),
    )
    .unwrap();
    let filter = t
        .workspace
        .read_with(cx, |ws, _| ws.ui_state.namespace_filter("beta").to_vec());
    assert_eq!(filter, ["demo"]);
    assert!(matches!(
        dispatch(
            cx,
            &t,
            "workspace::AddNamespace",
            json!({"namespace": "  "})
        ),
        Err(ActionError::Rejected { .. })
    ));
}

// Every list and detail action works with focus on the workspace, forwarded to the open views.
#[gpui_kit::test]
async fn list_and_detail_actions_work_from_the_workspace(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_pods(cx, &t, &[("demo", "api", 0), ("demo", "web", 1)]).await;
    focus_workspace(cx, &t);

    dispatch(
        cx,
        &t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert_eq!(selected(cx, &t).as_deref(), Some("api"));
    focus_workspace(cx, &t);
    dispatch(cx, &t, "resource_list::SelectNext", Value::Null).unwrap();
    assert_eq!(selected(cx, &t).as_deref(), Some("web"));
    assert!(matches!(
        dispatch(
            cx,
            &t,
            "resource_list::SelectRow",
            json!({"namespace": "demo", "name": "nope"})
        ),
        Err(ActionError::Rejected { .. })
    ));
    // The kind has no -o wide columns, so the toggle is not offered.
    assert_eq!(
        dispatch(cx, &t, "resource_list::ToggleWide", Value::Null),
        Err(ActionError::NotAvailable(
            "resource_list::ToggleWide".into()
        ))
    );

    focus_workspace(cx, &t);
    dispatch(cx, &t, "detail::ShowYaml", Value::Null).unwrap();
    let tab = t
        .workspace
        .read_with(cx, |ws, cx| ws.detail.as_ref().map(|d| d.read(cx).tab()));
    assert_eq!(tab, Some(DetailTab::Yaml));
    dispatch(cx, &t, "detail::Close", Value::Null).unwrap();
    cx.run_until_parked();
    assert!(t.workspace.read_with(cx, |ws, _| ws.detail.is_none()));
    assert_eq!(selected(cx, &t), None);
}

// Clicks keep focus where it is (a click in the detail pane leaves it on the list), and the
// list's keys work from there; Esc closes the detail pane.
#[gpui_kit::test]
async fn clicks_keep_focus_and_keys_follow(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_pods(
        cx,
        &t,
        &[("demo", "api", 0), ("demo", "db", 2), ("demo", "web", 1)],
    )
    .await;

    cx.update_window(t.window, |_, window, cx| window.click("row-uid-api", cx))
        .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("detail").is_some()
    })
    .await;
    assert_eq!(selected(cx, &t).as_deref(), Some("api"));
    cx.update_window(t.window, |_, window, cx| {
        window.click("detail", cx);
        window.press("down", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(selected(cx, &t).as_deref(), Some("db"));
    let list_focused = cx
        .update_window(t.window, |_, window, cx| {
            let list = t.workspace.read(cx).list.clone().unwrap();
            list.read(cx).focus_handle().is_focused(window)
        })
        .unwrap();
    assert!(list_focused);

    // With focus on the workspace, the arrows still move the selection and Esc closes the pane.
    focus_workspace(cx, &t);
    cx.update_window(t.window, |_, window, cx| window.press("down", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(selected(cx, &t).as_deref(), Some("web"));
    focus_workspace(cx, &t);
    cx.update_window(t.window, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(t.workspace.read_with(cx, |ws, _| ws.detail.is_none()));
}

// A cluster switch drops the focused list; focus comes back to the workspace, so its actions
// stay available.
#[gpui_kit::test]
async fn focus_comes_back_after_a_cluster_switch(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_pods(cx, &t, &[("demo", "api", 0)]).await;
    activate(cx, &t, "beta").await;
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |window, cx| {
        workspace.read(cx).focus.is_focused(window)
    })
    .await;
    let available = cx
        .update_window(t.window, |_, window, cx| {
            registry::find("workspace::ShowAllNamespaces")
                .unwrap()
                .is_available(window, cx)
        })
        .unwrap();
    assert!(available);
    dispatch(cx, &t, "workspace::ShowAllNamespaces", Value::Null).unwrap();
}

// What a palette shows: the actions available from the element focused before it opened, with
// their live shortcuts.
#[gpui_kit::test]
async fn a_palette_can_list_actions_with_shortcuts(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_pods(cx, &t, &[("demo", "api", 0)]).await;
    let list = t.workspace.read_with(cx, |ws, _| ws.list.clone().unwrap());
    let (names, shortcut) = cx
        .update_window(t.window, |_, window, cx| {
            let focus = list.read(cx).focus_handle().clone();
            let names: Vec<&str> = registry::available_in(&focus, window, cx)
                .map(|s| s.name)
                .collect();
            let shortcut = registry::find("resource_list::FocusFilter")
                .unwrap()
                .shortcut(cx);
            (names, shortcut)
        })
        .unwrap();
    assert!(names.contains(&"resource_list::SelectNext"), "{names:?}");
    assert!(names.contains(&"workspace::OpenView"), "{names:?}");
    assert!(!names.contains(&"detail::Close"), "{names:?}");
    let shortcut = shortcut.expect("Focus Filter has a key");
    assert!(shortcut.to_lowercase().contains('f'), "{shortcut}");
}

// The confirmation's buttons are registered actions too.
#[gpui_kit::test]
async fn the_expose_dialog_answers_confirm_and_cancel(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let has_dialog = |cx: &mut TestAppContext| {
        cx.update_window(t.window, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    };

    dispatch(cx, &t, "workspace::ExposeToAi", json!({"cluster": "alpha"})).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    dispatch(cx, &t, "ui::Cancel", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    assert!(!has_dialog(cx));

    dispatch(cx, &t, "workspace::ExposeToAi", json!({"cluster": "alpha"})).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    dispatch(cx, &t, "ui::Confirm", Value::Null).unwrap();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let model = model.read(cx);
        model
            .clusters
            .iter()
            .find(|c| c.info.name == "alpha")
            .is_some_and(|c| model.ai_exposure(c) == AiExposureState::Exposed)
    })
    .await;
    assert!(!has_dialog(cx));
}

// SetPaneWidth resizes like a drag: measured from the panes as shown, and a request a narrow
// window cannot show is not saved, so a wider window gets the remembered width back.
#[gpui_kit::test]
async fn set_pane_width_keeps_the_width_a_narrow_window_cannot_show(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "alpha").await;
    open_pods(cx, &t, &[("demo", "api", 0)]).await;
    dispatch(
        cx,
        &t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    cx.run_until_parked();
    assert!(t.workspace.read_with(cx, |ws, _| ws.detail.is_some()));
    let panes = |cx: &mut TestAppContext| t.workspace.read_with(cx, |ws, _| ws.panes);
    let before = panes(cx);

    // 1100 px leaves the detail pane no more than it shows already: nothing moves, nothing saved.
    dispatch(
        cx,
        &t,
        "workspace::SetPaneWidth",
        json!({"pane": "detail", "width": 600}),
    )
    .unwrap();
    cx.run_until_parked();
    assert_eq!(panes(cx), before);

    // A change the window can show is kept.
    dispatch(
        cx,
        &t,
        "workspace::SetPaneWidth",
        json!({"pane": "clusters", "width": 240}),
    )
    .unwrap();
    cx.run_until_parked();
    assert_eq!(panes(cx).clusters, Some(240));
    assert!(matches!(
        dispatch(
            cx,
            &t,
            "workspace::SetPaneWidth",
            json!({"pane": "clusters", "width": -1})
        ),
        Err(ActionError::Rejected { .. })
    ));
}
