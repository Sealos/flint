//! View tests for the workspace's actions: through the registry, through clicks and keys, and
//! with focus on the workspace itself (the actions forwarded to the list and detail pane).

use flint_core::settings::AiExposureState;
use flint_core::{ClusterId, ConnectionState, Event, ServerVersion, SyncState};
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, FocusHandle, TestAppContext};
use serde_json::{Value, json};

use super::Workspace;
use crate::ui::detail::DetailTab;
use crate::ui::dock::{DockItem as _, DockTab, TabStatus};
use crate::ui::list::ResourceList;
use crate::ui::nav::NavPanel;
use crate::ui::registry::{self, ActionError};
use crate::ui::support_tests::{
    TestWorkspace, WAIT, kubeconfig_yaml, objects, open_fake_shell, open_window_over,
    open_workspace,
};
use crate::ui::view_state::Focus;

/// Test hooks into the workspace's private state, for the view tests of every module
/// (`support_tests.rs`).
impl Workspace {
    /// The cluster the main area shows.
    pub(in crate::ui) fn shown_cluster(&self) -> Option<&str> {
        self.cluster.as_deref()
    }

    pub(in crate::ui) fn list_view(&self) -> Option<Entity<ResourceList>> {
        self.list.clone()
    }

    pub(in crate::ui) fn nav_view(&self) -> Entity<NavPanel> {
        self.nav.clone()
    }

    pub(in crate::ui) fn focus_handle_for_tests(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub(in crate::ui) fn dock_view(&self) -> Entity<crate::ui::dock::Dock> {
        self.dock.clone()
    }

    /// Sets the active cluster's namespace names, as the namespace watch would.
    pub(in crate::ui) fn set_namespaces_for_tests(&mut self, names: Vec<String>) {
        self.namespaces = names;
    }

    /// The command palette, while it is open.
    pub(in crate::ui) fn palette_view(&self) -> Option<Entity<crate::ui::palette::CommandPalette>> {
        self.palette.as_ref().map(|(p, _)| p.clone())
    }

    /// The namespace filter remembered for `cluster`.
    pub(in crate::ui) fn namespace_filter_of(&self, cluster: &ClusterId) -> Vec<String> {
        self.ui_state.namespace_filter(cluster).to_vec()
    }
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
        t.dispatch(cx, "workspace::SelectCluster", json!({"cluster": "gamma"})),
        Err(ActionError::Rejected { .. })
    ));
    t.activate(cx, "kind-beta").await;
    // Nothing listens on kind-beta's port: the sidebar and the main pane say so, naming the server.
    let refused = format!(
        "connection refused by {}; is the API server running, and is the port right?",
        t.refused_server
    );
    let model = t.model.clone();
    let expected = refused.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).clusters.iter().any(|c| {
            matches!(&c.state, flint_core::ConnectionState::Failed(e) if e.as_str() == expected)
        })
    })
    .await;

    // The automation `state` sees the same: the active cluster, its row, where focus is.
    let workspace = t.workspace.clone();
    let state = cx
        .update_window(t.window, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.view_state(window, cx))
        })
        .unwrap();
    assert_eq!(state.active_cluster.as_deref(), Some("kind-beta"));
    assert!(
        state
            .clusters
            .iter()
            .any(|c| c.name == "kind-beta" && c.active)
    );
    assert_eq!(state.focus, Focus::Workspace);
    assert!(state.list.is_none() && state.detail.is_none() && state.dialog.is_none());
    assert!(state.header.is_some());

    let offered = names(cx);
    assert!(offered.contains(&"workspace::OpenView"), "{offered:?}");
    assert!(offered.contains(&"workspace::ShowAllNamespaces"));
    // No list or detail pane is open, so their actions are not offered.
    assert!(!offered.contains(&"resource_list::SelectNext"));
    assert!(!offered.contains(&"detail::Close"));
    assert_eq!(
        t.dispatch(cx, "detail::Refresh", Value::Null),
        Err(ActionError::NotAvailable("detail::Refresh".into()))
    );
    // Hiding a hidden cluster would do nothing.
    assert!(matches!(
        t.dispatch(cx, "workspace::HideFromAi", json!({"cluster": "kind-beta"})),
        Err(ActionError::Rejected { .. })
    ));
    // Namespace names are trimmed; blank ones are refused.
    t.dispatch(
        cx,
        "workspace::SetNamespaces",
        json!({"namespaces": [" demo "]}),
    )
    .unwrap();
    let filter = t.workspace.read_with(cx, |ws, _| {
        ws.ui_state.namespace_filter("kind-beta").to_vec()
    });
    assert_eq!(filter, ["demo"]);
    assert!(matches!(
        t.dispatch(cx, "workspace::AddNamespace", json!({"namespace": "  "})),
        Err(ActionError::Rejected { .. })
    ));
}

// Every list and detail action works with focus on the workspace, forwarded to the open views.
#[gpui_kit::test]
async fn list_and_detail_actions_work_from_the_workspace(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 1)])
        .await;
    t.focus_workspace(cx);

    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
    t.focus_workspace(cx);
    t.dispatch(cx, "resource_list::SelectNext", Value::Null)
        .unwrap();
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert!(matches!(
        t.dispatch(
            cx,
            "resource_list::SelectRow",
            json!({"namespace": "demo", "name": "nope"})
        ),
        Err(ActionError::Rejected { .. })
    ));
    // The kind has no -o wide columns, so the toggle is not offered.
    assert_eq!(
        t.dispatch(cx, "resource_list::ToggleWide", Value::Null),
        Err(ActionError::NotAvailable(
            "resource_list::ToggleWide".into()
        ))
    );

    t.focus_workspace(cx);
    t.dispatch(cx, "detail::ShowYaml", Value::Null).unwrap();
    let workspace = t.workspace.clone();
    let state = cx
        .update_window(t.window, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.view_state(window, cx))
        })
        .unwrap();
    assert_eq!(state.detail.as_ref().map(|d| d.tab), Some("yaml"));
    assert_eq!(state.list.as_ref().map(|l| l.shown), Some(2));
    let tab = t
        .workspace
        .read_with(cx, |ws, cx| ws.detail.as_ref().map(|d| d.read(cx).tab()));
    assert_eq!(tab, Some(DetailTab::Yaml));
    t.dispatch(cx, "detail::Close", Value::Null).unwrap();
    cx.run_until_parked();
    assert!(t.workspace.read_with(cx, |ws, _| ws.detail.is_none()));
    assert_eq!(t.selected(cx), None);
}

// Clicks keep focus where it is (a click in the detail pane leaves it on the list), and the
// list's keys work from there; Esc closes the detail pane.
#[gpui_kit::test]
async fn clicks_keep_focus_and_keys_follow(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(
        cx,
        &[("demo", "api", 0), ("demo", "db", 2), ("demo", "web", 1)],
    )
    .await;

    cx.update_window(t.window, |_, window, cx| window.click("row-uid-api", cx))
        .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("detail").is_some()
    })
    .await;
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
    cx.update_window(t.window, |_, window, cx| {
        window.click("detail", cx);
        window.press("down", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(t.selected(cx).as_deref(), Some("db"));
    let list_focused = cx
        .update_window(t.window, |_, window, cx| {
            let list = t.workspace.read(cx).list.clone().unwrap();
            list.read(cx).focus_handle().is_focused(window)
        })
        .unwrap();
    assert!(list_focused);

    // With focus on the workspace, the arrows still move the selection and Esc closes the pane.
    t.focus_workspace(cx);
    cx.update_window(t.window, |_, window, cx| window.press("down", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    t.focus_workspace(cx);
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
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    t.activate(cx, "kind-beta").await;
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
    t.dispatch(cx, "workspace::ShowAllNamespaces", Value::Null)
        .unwrap();
}

// What a palette shows: the actions available from the element focused before it opened, with
// their live shortcuts.
#[gpui_kit::test]
async fn a_palette_can_list_actions_with_shortcuts(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
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

    t.dispatch(
        cx,
        "workspace::ExposeToAi",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    // The automation state says what the dialog asks.
    let workspace = t.workspace.clone();
    let dialog = cx
        .update_window(t.window, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.view_state(window, cx).dialog)
        })
        .unwrap()
        .expect("the dialog is in the state");
    assert_eq!(dialog.title, "Expose to AI agents?");
    assert!(
        dialog.text.contains("Context: kind-alpha"),
        "{}",
        dialog.text
    );
    assert_eq!(dialog.buttons, ["Expose", "Cancel"]);
    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
    assert!(!has_dialog(cx));

    t.dispatch(
        cx,
        "workspace::ExposeToAi",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let model = model.read(cx);
        model
            .clusters
            .iter()
            .find(|c| c.info.name == "kind-alpha")
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
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    cx.run_until_parked();
    assert!(t.workspace.read_with(cx, |ws, _| ws.detail.is_some()));
    let panes = |cx: &mut TestAppContext| t.workspace.read_with(cx, |ws, _| ws.panes);
    let before = panes(cx);

    // 1100 px leaves the detail pane no more than it shows already: nothing moves, nothing saved.
    t.dispatch(
        cx,
        "workspace::SetPaneWidth",
        json!({"pane": "detail", "width": 600}),
    )
    .unwrap();
    cx.run_until_parked();
    assert_eq!(panes(cx), before);

    // A change the window can show is kept.
    t.dispatch(
        cx,
        "workspace::SetPaneWidth",
        json!({"pane": "clusters", "width": 240}),
    )
    .unwrap();
    cx.run_until_parked();
    assert_eq!(panes(cx).clusters, Some(240));
    assert!(matches!(
        t.dispatch(
            cx,
            "workspace::SetPaneWidth",
            json!({"pane": "clusters", "width": -1})
        ),
        Err(ActionError::Rejected { .. })
    ));
}

/// Selects pod `name` and shows it in the detail pane as fetched, with containers `app`
/// (restarted once, so it has previous logs) and `sidecar`.
async fn show_pod(cx: &mut TestAppContext, t: &TestWorkspace, name: &str) {
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": name}),
    )
    .unwrap();
    cx.run_until_parked();
    let detail = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    let pod = flint_core::ObjectDetail {
        object: json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name, "namespace": "demo", "uid": format!("uid-{name}")},
            "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]},
            "status": {"containerStatuses": [
                {"name": "app", "restartCount": 1},
                {"name": "sidecar", "restartCount": 0}
            ]}
        }),
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    };
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_fetched(pod, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
}

/// The dock: whether it shows, and its tabs' titles, left to right.
fn dock_tabs(cx: &mut TestAppContext, t: &TestWorkspace) -> (bool, Vec<String>) {
    t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        let titles = dock
            .tab_ids()
            .into_iter()
            .map(|id| dock.tab(id).unwrap().item(cx).tab_title().to_string())
            .collect();
        (dock.is_visible(), titles)
    })
}

fn press(cx: &mut TestAppContext, t: &TestWorkspace, key: &str) {
    cx.update_window(t.window, |_, window, cx| window.press(key, cx))
        .unwrap();
    cx.run_until_parked();
}

/// Marks every cluster connected, as the compare entry needs another one.
fn connect_all(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.workspace.update(cx, |ws, cx| {
        ws.model.update(cx, |model, cx| {
            for cluster in &mut model.clusters {
                cluster.state = flint_core::ConnectionState::Connected(flint_core::ServerVersion {
                    git_version: "v1".into(),
                    platform: "linux/arm64".into(),
                });
            }
            cx.notify();
        });
    });
    cx.run_until_parked();
}

async fn right_click_row(cx: &mut TestAppContext, handle: AnyWindowHandle, name: &str) {
    let row = gpui_kit::SharedString::from(format!("row-uid-{name}"));
    // Filtering and sorting await the core's worker, which can outlast GPUI going idle.
    cx.wait_for(handle, WAIT, |window, _| {
        window.try_find(row.clone()).is_some()
    })
    .await;
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(row, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn click_compare_entry(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.update_window(t.window, |_, window, cx| {
        window.render_frame(cx);
        let mut menu = window.within("popup-menu");
        let entry = (0usize..)
            .map_while(|ix| {
                menu.try_find(ix)
                    .map(|item| (ix, item.label().map(str::to_owned)))
            })
            .find(|(_, label)| label.as_deref() == Some("Compare Across Clusters"))
            .map(|(ix, _)| ix)
            .expect("the Compare entry");
        menu.click(entry, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

// covers #449: the menu compares the row it opened on, even if the selection moved since.
#[gpui_kit::test]
async fn a_row_menu_compares_its_own_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 1)])
        .await;
    connect_all(cx, &t);
    right_click_row(cx, t.window, "web").await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    cx.run_until_parked();
    click_compare_entry(cx, &t);
    let key = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        dock.active_compare().unwrap().read(cx).tab_key()
    });
    let crate::ui::dock::TabKey::Compare(_, target) = key else {
        panic!("not a compare tab");
    };
    assert_eq!(target.name, "web");
}

// covers #449: a read-only kind gets the entry when there is a cluster to compare with, and no
// menu at all (keeping the focus) when there is none.
#[gpui_kit::test]
async fn a_read_only_kind_is_offered_compare_alone(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // Trust the origin so cluster protection does not add an Unlock entry.
    std::fs::write(
        dir.path().join("settings.toml"),
        "trusted_clusters = [{ context = \"kind-alpha\", server = \"https://alpha.invalid\" }]\n",
    )
    .unwrap();
    let kubeconfig = dir.path().join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        kubeconfig_yaml(&[
            ("kind-alpha", "https://alpha.invalid"),
            ("kind-beta", "https://beta.invalid"),
        ]),
    )
    .unwrap();
    cx.executor().allow_parking();
    let (handle, workspace, model) = open_window_over(cx, dir.path(), vec![kubeconfig]);
    cx.wait_for(handle, WAIT, |_, cx| model.read(cx).loaded)
        .await;
    let connected = ConnectionState::Connected(ServerVersion {
        git_version: "v1".into(),
        platform: "linux/arm64".into(),
    });
    // Drive the connection states through the model so a real attempt cannot overwrite them.
    model.update(cx, |model, cx| {
        model.feed(Event::ActiveChanged(Some("kind-alpha".into())), cx);
        model.feed(
            Event::ConnectionChanged {
                cluster: "kind-alpha".into(),
                state: connected.clone(),
            },
            cx,
        );
    });
    cx.wait_for(handle, WAIT, |_, cx| {
        workspace.read(cx).shown_cluster() == Some("kind-alpha")
    })
    .await;
    let entry = flint_core::NavEntry {
        id: flint_core::KindId::new("metrics.k8s.io", "pods"),
        label: "Pod Metrics".into(),
        kind: "PodMetrics".into(),
        api_version: "metrics.k8s.io/v1beta1".into(),
        namespaced: true,
        listable: true,
        watchable: false,
    };
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.open_kind(&entry, window, cx);
            let list = ws.list.clone().unwrap();
            list.update(cx, |list, cx| {
                list.stop_watching();
                list.set_snapshot(
                    objects(
                        entry.id.clone(),
                        &[("demo", "web", 1)],
                        SyncState::Synced,
                        1,
                    ),
                    cx,
                );
            });
        });
    })
    .unwrap();
    right_click_row(cx, handle, "web").await;
    let (shown, focus) = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            (
                window.try_find("popup-menu").is_some(),
                workspace.update(cx, |ws, cx| ws.view_state(window, cx).focus),
            )
        })
        .unwrap();
    assert!(!shown, "nothing to offer with no other cluster");
    assert_eq!(focus, Focus::List);

    model.update(cx, |model, cx| {
        model.feed(
            Event::ConnectionChanged {
                cluster: "kind-beta".into(),
                state: connected,
            },
            cx,
        );
    });
    right_click_row(cx, handle, "web").await;
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let mut menu = window.within("popup-menu");
        let labels: Vec<_> = (0usize..)
            .map_while(|ix| menu.try_find(ix))
            .map(|item| item.label().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(labels, ["Compare Across Clusters"]);
        menu.click(0usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let compared = workspace.read_with(cx, |ws, cx| ws.dock.read(cx).active_compare().is_some());
    assert!(compared);
}

// covers UP-40, UP-42: Logs in the detail pane opens a tab in the dock (or shows the open one),
// the tab keeps running across a cluster switch, and Cmd-W in the dock closes tabs, not the
// window.
#[gpui_kit::test]
async fn logs_open_in_the_dock_and_outlive_a_cluster_switch(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 1)]).await;
    assert_eq!(
        t.dispatch(cx, "detail::OpenLogs", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenLogs".into())),
        "no pod is shown yet"
    );
    show_pod(cx, &t, "web").await;

    // The sidecar never restarted, so it has no previous logs.
    assert!(matches!(
        t.dispatch(
            cx,
            "detail::OpenContainerLogs",
            json!({"container": "sidecar", "previous": true})
        ),
        Err(ActionError::Rejected { .. })
    ));
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(dock_tabs(cx, &t), (true, vec!["web · app".to_owned()]));
    // Opening it again shows the same tab.
    t.focus_workspace(cx);
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    t.dispatch(cx, "detail::OpenPreviousLogs", Value::Null)
        .unwrap();
    t.dispatch(
        cx,
        "detail::OpenContainerLogs",
        json!({"container": "sidecar"}),
    )
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        dock_tabs(cx, &t).1,
        ["web · app", "web · app (previous)", "web · sidecar"]
    );
    let first = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        match dock.tab(dock.tab_ids()[0]).unwrap() {
            DockTab::Logs(view) => view.downgrade(),
            DockTab::Shell(_) | DockTab::Activity(_) | DockTab::Compare(_) => panic!("a log tab"),
        }
    });

    // The new tab has focus; Ctrl-Tab moves through the tabs.
    press(cx, &t, "ctrl-tab");
    let active = |cx: &mut TestAppContext| {
        t.workspace.read_with(cx, |ws, cx| {
            let dock = ws.dock.read(cx);
            dock.tab_ids()
                .iter()
                .position(|id| Some(*id) == dock.active_id())
        })
    };
    assert_eq!(active(cx), Some(0));

    // A cluster switch closes the list, not the dock: the sessions keep running.
    t.activate(cx, "kind-beta").await;
    cx.run_until_parked();
    assert_eq!(dock_tabs(cx, &t).1.len(), 3);
    let alive = first.upgrade().expect("the log tab survives the switch");
    assert_eq!(
        alive.read_with(cx, |v, _| v.request().cluster.clone()),
        "kind-alpha"
    );
    drop(alive);

    // The log tab's actions work from the workspace, forwarded to the tab shown.
    t.focus_workspace(cx);
    t.dispatch(cx, "logs::ToggleTimestamps", Value::Null)
        .unwrap();
    let names: Vec<&str> = cx
        .update_window(t.window, |_, window, cx| {
            registry::available(window, cx).map(|s| s.name).collect()
        })
        .unwrap();
    assert!(names.contains(&"logs::ToggleWrap"), "{names:?}");
    assert!(names.contains(&"dock::CloseActiveTab"), "{names:?}");
    assert!(!names.contains(&"logs::CopyLines"), "nothing is selected");

    // Cmd-W in the dock closes its tabs one by one; the window stays.
    let focus = t.workspace.read_with(cx, |ws, cx| {
        ws.dock.read(cx).active_log().unwrap().read(cx).tab_focus()
    });
    cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    press(cx, &t, "secondary-w");
    assert_eq!(dock_tabs(cx, &t).1.len(), 2);
    press(cx, &t, "secondary-w");
    press(cx, &t, "secondary-w");
    assert_eq!(dock_tabs(cx, &t), (false, Vec::new()));
    cx.run_until_parked();
    assert!(first.upgrade().is_none(), "closing a tab ends its session");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |window, cx| {
        workspace.read(cx).focus.is_focused(window)
    })
    .await;
}

// covers UP-42: a tab is pinned to its context and the API server it pointed at. Once the
// context is re-pointed, Logs opens a new tab on the new server instead of showing the old
// server's, and each tab names its cluster.
#[gpui_kit::test]
async fn tabs_are_pinned_to_their_server(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 1)]).await;
    show_pod(cx, &t, "web").await;
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    cx.run_until_parked();
    let labels = |cx: &mut TestAppContext| {
        t.workspace.read_with(cx, |ws, cx| {
            let dock = ws.dock.read(cx);
            dock.tab_ids()
                .into_iter()
                .map(|id| dock.full_title(id, cx).unwrap())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(labels(cx), ["web · app on kind-alpha"]);
    t.focus_workspace(cx);
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(labels(cx).len(), 1, "the same server: the same tab");

    // `kind-alpha` now points at another server (as after editing kubeconfig).
    t.model.update(cx, |model, cx| {
        let alpha = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        alpha.info.server = Some("https://10.0.0.9:6443".into());
        cx.notify();
    });
    cx.run_until_parked();
    t.focus_workspace(cx);
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(
        labels(cx),
        ["web · app on kind-alpha", "web · app on kind-alpha"]
    );
    let active = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        dock.tab_ids()
            .iter()
            .position(|id| Some(*id) == dock.active_id())
    });
    assert_eq!(active, Some(1), "the new server's tab is shown");
}

// covers UP-41, UP-42: Shell in the detail pane opens a shell tab in the dock (or shows the open
// one), with a container picker; the tab keeps running across a cluster switch, its actions
// are forwarded from the workspace, and closing it ends the session.
#[gpui_kit::test]
async fn shells_open_in_the_dock_and_outlive_a_cluster_switch(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 1)]).await;
    assert_eq!(
        t.dispatch(cx, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into())),
        "no pod is shown yet"
    );
    show_pod(cx, &t, "web").await;
    cx.update_window(t.window, |_, window, _| {
        assert!(window.try_find("detail-shell").is_some());
    })
    .unwrap();
    assert!(matches!(
        t.dispatch(
            cx,
            "detail::OpenContainerShell",
            json!({"container": "nope"})
        ),
        Err(ActionError::Rejected { .. })
    ));
    t.dispatch(cx, "detail::OpenShell", Value::Null).unwrap();
    cx.run_until_parked();
    assert_eq!(
        dock_tabs(cx, &t),
        (true, vec!["Shell: web · app".to_owned()])
    );
    let first = t.workspace.read_with(cx, |ws, cx| {
        ws.dock.read(cx).active_terminal().unwrap().downgrade()
    });
    // `kind-alpha` never connects, so the shell fails at once and says why; the tab stays.
    let failed = first.upgrade().unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        failed.read(cx).tab_status() == TabStatus::Failed
    })
    .await;
    // An ended shell does not answer for its container: Shell opens a new one beside it. (A
    // running one is shown instead; see `terminal_tests.rs`.)
    t.focus_workspace(cx);
    t.dispatch(cx, "detail::OpenShell", Value::Null).unwrap();
    t.dispatch(
        cx,
        "detail::OpenContainerShell",
        json!({"container": "sidecar"}),
    )
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        dock_tabs(cx, &t).1,
        [
            "Shell: web · app",
            "Shell: web · app",
            "Shell: web · sidecar"
        ]
    );
    let shell = t.workspace.read_with(cx, |ws, cx| {
        ws.dock.read(cx).active_terminal().unwrap().downgrade()
    });

    // A cluster switch keeps the shells.
    t.activate(cx, "kind-beta").await;
    cx.run_until_parked();
    assert_eq!(dock_tabs(cx, &t).1.len(), 3);
    let alive = shell.upgrade().expect("the shell tab survives the switch");
    assert_eq!(
        alive.read_with(cx, |v, _| v.request().container.clone()),
        Some("sidecar".to_owned())
    );
    drop(alive);

    // Its actions are forwarded from the workspace, and guarded.
    t.focus_workspace(cx);
    let names: Vec<&str> = cx
        .update_window(t.window, |_, window, cx| {
            registry::available(window, cx).map(|s| s.name).collect()
        })
        .unwrap();
    assert!(names.contains(&"terminal::SelectText"), "{names:?}");
    assert!(
        !names.contains(&"terminal::CopyText"),
        "nothing is selected"
    );
    assert!(
        !names.contains(&"terminal::ScrollToBottom"),
        "not scrolled back"
    );

    // Closing the tab drops the view, which ends the session.
    let id = t
        .workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).active_id().unwrap());
    t.dispatch(cx, "dock::CloseTab", json!({ "tab": id }))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        dock_tabs(cx, &t).1,
        ["Shell: web · app", "Shell: web · app"]
    );
    assert!(shell.upgrade().is_none(), "closing a tab ends its session");
}

// covers UP-41: Restart Shell starts a new shell in a tab beside the failed one and shows it,
// on the server the context points at now; the failed tab keeps its output.
#[gpui_kit::test]
async fn a_failed_shell_restarts_beside_itself(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 1)]).await;
    show_pod(cx, &t, "web").await;
    t.dispatch(cx, "detail::OpenShell", Value::Null).unwrap();
    cx.run_until_parked();
    let (id, first) = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        (
            dock.active_id().unwrap(),
            dock.active_terminal().unwrap().downgrade(),
        )
    });
    let failed = first.upgrade().unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        failed.read(cx).tab_status() == TabStatus::Failed
    })
    .await;
    assert_eq!(
        t.dispatch(cx, "dock::ReconnectLogs", Value::Null),
        Err(ActionError::NotAvailable("dock::ReconnectLogs".into())),
        "a shell tab"
    );
    t.dispatch(cx, "dock::RestartShell", Value::Null).unwrap();
    cx.run_until_parked();
    let (ids, active, now) = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        (
            dock.tab_ids(),
            dock.active_id(),
            dock.active_terminal().unwrap().downgrade(),
        )
    });
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], id, "the failed tab stays");
    assert_eq!(active, Some(ids[1]), "the new shell is shown, beside it");
    assert!(first.upgrade().is_some(), "the failed tab keeps its output");
    assert_ne!(now.entity_id(), first.entity_id(), "a new session");
}

// covers UP-42: the tab list opens from the keyboard (Shift-Cmd-J) and the palette, wherever
// focus is, and shows the tab picked.
#[gpui_kit::test]
async fn the_tab_list_opens_from_the_keyboard(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 1)]).await;
    show_pod(cx, &t, "web").await;
    assert_eq!(
        t.dispatch(cx, "dock::ShowTabList", Value::Null),
        Err(ActionError::NotAvailable("dock::ShowTabList".into())),
        "no tab yet"
    );
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    t.dispatch(
        cx,
        "detail::OpenContainerLogs",
        json!({"container": "sidecar"}),
    )
    .unwrap();
    cx.run_until_parked();
    t.focus_workspace(cx);
    press(cx, &t, "secondary-shift-j");
    let entries = cx
        .update_window(t.window, |_, window, _| {
            let menu = window.within("popup-menu");
            (0usize..)
                .map_while(|ix| menu.try_find(ix))
                .map(|item| item.label().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(
        entries[0].starts_with("web · app — kind-alpha · "),
        "{entries:?}"
    );
    assert!(
        entries[1].starts_with("web · sidecar — kind-alpha · "),
        "{entries:?}"
    );
    cx.update_window(t.window, |_, window, cx| {
        window.within("popup-menu").click(0usize, cx)
    })
    .unwrap();
    cx.run_until_parked();
    let (first, active) = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        (dock.tab_ids()[0], dock.active_id())
    });
    assert_eq!(active, Some(first));
    cx.update_window(t.window, |_, window, _| {
        assert!(window.try_find("popup-menu").is_none(), "the list closed");
    })
    .unwrap();
}

fn tab_status(cx: &mut TestAppContext, t: &TestWorkspace, id: u64) -> TabStatus {
    t.workspace.read_with(cx, |ws, cx| {
        ws.dock.read(cx).tab(id).unwrap().item(cx).tab_status()
    })
}

/// Points `context` at another server, or sets its connection, as a kubeconfig reload or the
/// core would.
fn change_cluster(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    context: &str,
    change: impl FnOnce(&mut flint_core::ClusterSummary),
) {
    t.model.update(cx, |model, cx| {
        let cluster = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == context)
            .unwrap();
        change(cluster);
        cx.notify();
    });
    cx.run_until_parked();
}

// covers UP-41: when a kubeconfig reload re-points a context, the workspace ends the running
// shells on it (the model's change reaches the dock), and says why.
#[gpui_kit::test]
async fn a_re_pointed_context_ends_its_shells(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let (id, mut fake) = open_fake_shell(cx, &t);
    assert_eq!(tab_status(cx, &t, id), TabStatus::Live);
    // Another change to the context (its connection) leaves the shell alone.
    change_cluster(cx, &t, "kind-alpha", |c| {
        c.state = flint_core::ConnectionState::Disconnected
    });
    assert_eq!(tab_status(cx, &t, id), TabStatus::Live);
    assert!(!fake.is_closed());
    change_cluster(cx, &t, "kind-alpha", |c| {
        c.info.server = Some("https://10.0.0.9:6443".into())
    });
    assert!(fake.is_closed(), "the shell's session ends");
    assert_eq!(tab_status(cx, &t, id), TabStatus::Ended);
}

// covers UP-41, UP-45: Restart Shell and Reconnect Logs are refused, saying why, while the
// tab's cluster is not connected; no tab opens.
#[gpui_kit::test]
async fn restarting_waits_for_a_connected_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let (id, mut fake) = open_fake_shell(cx, &t);
    fake.set_state(flint_core::ExecState::Exited { code: Some(0) });
    fake.pump();
    cx.run_until_parked();
    change_cluster(cx, &t, "kind-alpha", |c| {
        c.state = flint_core::ConnectionState::Disconnected
    });
    t.dispatch(cx, "dock::RestartShell", Value::Null).unwrap();
    let (ids, refused) = t.workspace.read_with(cx, |ws, cx| {
        (
            ws.dock.read(cx).tab_ids(),
            ws.action_error().map(str::to_owned),
        )
    });
    assert_eq!(ids, [id], "no new tab");
    assert_eq!(
        refused.as_deref(),
        Some("Can't restart the shell: kind-alpha is not connected.")
    );

    // A failed log tab whose context is gone from kubeconfig: Reconnect is refused the same way.
    let core = t.model.read_with(cx, |m, _| m.core().clone());
    let request = crate::ui::log_view::LogRequest {
        cluster: "gone".into(),
        subject: flint_core::LogSubject::Pod(flint_core::LogTarget {
            namespace: "demo".into(),
            pod: "web".into(),
            container: Some("app".into()),
        }),
        previous: false,
    };
    let server = t.server.clone();
    let logs = cx
        .update_window(t.window, |_, window, cx| {
            let dock = t.workspace.read(cx).dock.clone();
            dock.update(cx, |dock, cx| {
                dock.open(
                    crate::ui::dock::TabKey::Logs(request.clone()),
                    crate::ui::dock_model::TabCluster::new("gone", Some(&server)),
                    |window, cx| {
                        DockTab::Logs(
                            cx.new(|cx| {
                                crate::ui::log_view::LogView::new(core, request, window, cx)
                            }),
                        )
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let ws = workspace.read(cx);
        ws.dock.read(cx).tab(logs).unwrap().item(cx).can_restart()
    })
    .await;
    t.dispatch(cx, "dock::ReconnectLogs", Value::Null).unwrap();
    let (count, refused) = t.workspace.read_with(cx, |ws, cx| {
        (
            ws.dock.read(cx).tab_count(),
            ws.action_error().map(str::to_owned),
        )
    });
    assert_eq!(count, 2, "no new tab");
    assert_eq!(
        refused.as_deref(),
        Some("Can't reconnect the logs: no kubeconfig context named gone.")
    );
}

// covers UP-04, UP-42: Cmd-R in a tab whose session runs is not Restart or Reconnect: it falls
// through to Refresh, which fetches the shown object again.
#[gpui_kit::test]
async fn the_refresh_key_reaches_refresh_from_a_running_tab(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 1)]).await;
    show_pod(cx, &t, "web").await;
    // A log tab whose stream is still starting (`kind-alpha` never answers), focused.
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    cx.run_until_parked();
    let focus = t.workspace.read_with(cx, |ws, cx| {
        let log = ws.dock.read(cx).active_log().unwrap().read(cx);
        assert!(!log.can_restart());
        log.tab_focus()
    });
    cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    press(cx, &t, "secondary-r");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).detail.as_ref().is_some_and(|d| {
            d.read(cx)
                .view_state()
                .banners
                .iter()
                .any(|b| b.id == "detail-refresh-error")
        })
    })
    .await;
    assert_eq!(
        t.workspace
            .read_with(cx, |ws, cx| ws.dock.read(cx).tab_count()),
        1,
        "no restart"
    );
}

// covers UP-42: dismissing the tab list with Escape gives focus back to where it was; a click
// elsewhere leaves focus where the click put it, never on the closed list.
#[gpui_kit::test]
async fn dismissing_the_tab_list_gives_focus_back(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 1)]).await;
    show_pod(cx, &t, "web").await;
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    cx.run_until_parked();
    let menu_open = |cx: &mut TestAppContext| {
        cx.update_window(t.window, |_, window, _| {
            window.try_find("popup-menu").is_some()
        })
        .unwrap()
    };
    let workspace_focused = |cx: &mut TestAppContext| {
        cx.update_window(t.window, |_, window, cx| {
            t.workspace.read(cx).focus.is_focused(window)
        })
        .unwrap()
    };
    for dismiss in ["escape", "click"] {
        t.focus_workspace(cx);
        assert!(workspace_focused(cx));
        press(cx, &t, "secondary-shift-j");
        assert!(menu_open(cx), "{dismiss}: the list opened");
        assert!(!workspace_focused(cx), "the list has focus");
        if dismiss == "escape" {
            press(cx, &t, "escape");
        } else {
            cx.update_window(t.window, |_, window, cx| {
                // Plain text in the log tab's toolbar: a click there takes no focus itself. The
                // list covers the toolbar's right end, by a width that follows the tab's status
                // when it opened, so only the start of the status is always clear of it.
                window.render_frame(cx);
                let list = window.find("popup-menu").bounds();
                let offset = gpui_kit::point(gpui_kit::px(1.), gpui_kit::px(1.));
                let target = window.find("log-state-text").bounds().origin + offset;
                assert!(!list.contains(&target), "the click lands outside the list");
                window.click_at("log-state-text", offset, cx)
            })
            .unwrap();
            cx.run_until_parked();
        }
        assert!(!menu_open(cx), "{dismiss}: the list closed");
        if dismiss == "escape" {
            assert!(workspace_focused(cx), "escape: focus is back");
        } else {
            // The click went to the log tab, which keeps the focus it took.
            let log = cx
                .update_window(t.window, |_, window, cx| {
                    let ws = t.workspace.read(cx);
                    let log = ws.dock.read(cx).active_log().unwrap().read(cx).tab_focus();
                    log.is_focused(window)
                })
                .unwrap();
            assert!(log, "click: focus stays where the click put it");
        }
    }
}

// covers UP-42: Cmd-J shows and hides the dock, and its height is kept in state.toml.
#[gpui_kit::test]
async fn the_dock_toggles_and_remembers_its_height(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.focus_workspace(cx);
    press(cx, &t, "secondary-j");
    assert_eq!(dock_tabs(cx, &t), (true, Vec::new()));
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("dock-empty").is_some()
    })
    .await;

    t.dispatch(cx, "workspace::SetDockHeight", json!({"height": 333}))
        .unwrap();
    assert!(matches!(
        t.dispatch(cx, "workspace::SetDockHeight", json!({"height": -5})),
        Err(ActionError::Rejected { .. })
    ));
    let saved = |cx: &mut TestAppContext| t.workspace.read_with(cx, |ws, _| ws.panes.dock);
    assert_eq!(saved(cx), Some(333));
    // Taller than the 700 px window allows: the main area keeps its room.
    t.dispatch(cx, "workspace::SetDockHeight", json!({"height": 5000}))
        .unwrap();
    assert_eq!(saved(cx), Some(420));
    cx.wait_for(t.window, WAIT, |window, _| {
        window
            .try_find("dock-pane")
            .is_some_and(|p| (f32::from(p.bounds().size.height) - 420.).abs() < 1.)
    })
    .await;

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let path = dir.path().join(flint_core::settings::STATE_FILE);
    cx.wait_for(t.window, WAIT, |_, _| {
        std::fs::read_to_string(&path).is_ok_and(|s| s.contains("dock = 420"))
    })
    .await;
    assert_eq!(
        crate::ui::ui_state::UiState::load(dir.path().to_owned())
            .panes()
            .dock,
        Some(420)
    );

    t.focus_workspace(cx);
    press(cx, &t, "secondary-j");
    assert_eq!(dock_tabs(cx, &t), (false, Vec::new()));
}

// Keys meant for the list and the detail pane (Home, End, the arrows, Esc) do nothing while the
// user types in a field: the list's filter or the namespace filter's. In the list's filter,
// Down and Esc only move focus back to the rows (`keys_in_the_filter_return_to_the_rows`).
#[gpui_kit::test]
async fn keys_typed_into_a_field_leave_the_list_and_pane_alone(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(
        cx,
        &[("demo", "api", 0), ("demo", "db", 1), ("demo", "web", 2)],
    )
    .await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "db"}),
    )
    .unwrap();
    let pane = t.detail(cx).unwrap().entity_id();
    let unchanged = |cx: &mut TestAppContext, field: &str| {
        assert_eq!(t.selected(cx).as_deref(), Some("db"), "{field}");
        assert_eq!(t.detail(cx).map(|d| d.entity_id()), Some(pane), "{field}");
    };

    for field in ["list-filter", "ns-add"] {
        if field == "ns-add" {
            t.click(cx, "namespace-filter-button");
            t.wait_for_element(cx, "ns-add").await;
        }
        t.click(cx, field);
        let typing = cx
            .update_window(t.window, |_, window, cx| {
                use gpui_kit::test::TestQueryExt as _;
                window.render_frame(cx);
                window
                    .find_all_by_role(gpui_kit::Role::TextInput)
                    .iter()
                    .any(|e| e.focused() == Some(true))
            })
            .unwrap();
        assert!(typing, "{field} has focus");
        let keys: &[&str] = if field == "list-filter" {
            &["home", "end", "up", "down"]
        } else {
            &["home", "end", "up", "down", "escape"]
        };
        for key in keys {
            t.press(cx, key);
            unchanged(cx, field);
        }
    }
}

// Close Window closes the front window, from the registry (the palette, automation), its key,
// and the Window menu (an app-level dispatch).
#[gpui_kit::test]
async fn close_window_closes_the_window(cx: &mut TestAppContext) {
    use crate::ui::registry::CloseWindow;

    for how in ["registry", "key", "menu"] {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        assert!(t.available(cx).contains(&"flint::CloseWindow"));
        // The front window, as the one the user works in is.
        cx.update_window(t.window, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        match how {
            "registry" => t.dispatch(cx, "flint::CloseWindow", Value::Null).unwrap(),
            "key" => t.press_key_of(cx, "flint::CloseWindow"),
            _ => cx.update(|cx| cx.dispatch_action(&CloseWindow)),
        }
        cx.run_until_parked();
        assert!(cx.update_window(t.window, |_, _, _| ()).is_err(), "{how}");
    }
}

// covers UP-01: after a burst of changes (here, switching clusters back and forth faster than
// the UI reads), the app shows the latest: the core's queue keeps only the newest state of each
// kind, and the order of the last changes.
#[gpui_kit::test]
async fn a_burst_of_changes_ends_on_the_latest_state(cx: &mut TestAppContext) {
    use flint_core::ConnectionState;
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.model.update(cx, |m, _| {
        for n in 0..40 {
            m.activate(
                if n % 2 == 0 {
                    "kind-alpha"
                } else {
                    "kind-beta"
                }
                .into(),
            );
        }
    });
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).active.as_deref() == Some("kind-beta")
    })
    .await;
    // kind-beta's server refuses: it settles on failed; kind-alpha's never answers: still connecting.
    t.wait_for_state(cx, "kind-beta", |s| matches!(s, ConnectionState::Failed(_)))
        .await;
    t.wait_for_state(cx, "kind-alpha", |s| {
        matches!(s, ConnectionState::Connecting)
    })
    .await;
    let names: Vec<String> = t.model.read_with(cx, |m, _| {
        m.clusters.iter().map(|c| c.info.name.clone()).collect()
    });
    assert_eq!(names, ["kind-alpha", "kind-beta"]);
}

// covers UP-06 (#255): when the active cluster starts over in the core (a newer generation:
// re-pointed, even and back), its open view closes as on a switch, so nothing stays open from
// the old catalog, and its old discovery is not used.
#[gpui_kit::test]
async fn a_cluster_that_starts_over_closes_its_views(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    assert!(t.list(cx).is_some());
    t.model.update(cx, |m, cx| {
        if let Some(c) = m.clusters.iter_mut().find(|c| c.info.name == "kind-alpha") {
            c.generation += 1;
        }
        cx.notify();
    });
    cx.run_until_parked();
    assert!(t.list(cx).is_none(), "the old slot's view closed");
}
