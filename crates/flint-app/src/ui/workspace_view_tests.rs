//! View tests for what the workspace draws around the list: the cluster sidebar's rows, the
//! header's controls, and the main area's empty states.

use std::sync::Arc;

use flint_core::settings::AiExposure;
use flint_core::{ConnectionState, ServerVersion, Settings};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::json;

use crate::ui::registry;
use crate::ui::support_tests::{TestWorkspace, WAIT, open_empty_workspace, open_workspace};
use crate::ui::view_state::{AppState, Main};

fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> AppState {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.view_state(window, cx))
    })
    .unwrap()
}

fn render(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.run_until_parked();
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

fn shown(cx: &mut TestAppContext, t: &TestWorkspace, id: &'static str) -> bool {
    cx.update_window(t.window, |_, window, _| window.try_find(id).is_some())
        .unwrap()
}

fn presence(state: &AppState, name: &str) -> &'static str {
    state
        .clusters
        .iter()
        .find(|c| c.name == name)
        .unwrap()
        .presence
}

// covers UP-08: "Cluster changed" sits on its own line under the server, so the server keeps the
// row's width instead of being squeezed to "127.0.0.1:5…" beside the badge.
#[gpui_kit::test]
async fn the_cluster_changed_badge_leaves_the_server_its_room(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.model.update(cx, |model, cx| {
        model.settings = Arc::new(Settings {
            ai_exposed_clusters: vec![AiExposure {
                context: "alpha".into(),
                server: "https://10.0.0.1:6443".into(),
            }],
            ..Default::default()
        });
        cx.notify();
    });
    render(cx, &t);
    let state = state(cx, &t);
    let alpha = state.clusters.iter().find(|c| c.name == "alpha").unwrap();
    assert_eq!(alpha.ai, "server_changed");
    cx.update_window(t.window, |_, window, _| {
        let server = window.find("cluster-server-alpha").bounds();
        let badge = window.find("cluster-changed-alpha").bounds();
        let title_room = window.find("cluster-server-beta").bounds().size.width;
        assert!(
            badge.origin.y >= server.origin.y + server.size.height,
            "the badge is beside the server, not below it: {badge:?} vs {server:?}"
        );
        assert_eq!(
            server.size.width, title_room,
            "the server line is narrower than in a row without the badge"
        );
    })
    .unwrap();
}

// covers UP-24: a cluster switched away from keeps its connection but not its watches, so its
// dot says idle, not live.
#[gpui_kit::test]
async fn an_inactive_connected_cluster_is_idle_not_live(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let connect = |cx: &mut TestAppContext, active: Option<&str>| {
        let active = active.map(str::to_owned);
        t.model.update(cx, |model, cx| {
            for cluster in &mut model.clusters {
                cluster.state = ConnectionState::Connected(ServerVersion {
                    git_version: "v1.31.0".into(),
                    platform: "linux/arm64".into(),
                });
            }
            model.active = active;
            cx.notify();
        });
    };
    connect(cx, Some("alpha"));
    render(cx, &t);
    let s = state(cx, &t);
    assert_eq!(
        (presence(&s, "alpha"), presence(&s, "beta")),
        ("live", "idle")
    );
    // Switched to beta: alpha's watches stop, and so does its live dot.
    connect(cx, Some("beta"));
    render(cx, &t);
    let s = state(cx, &t);
    assert_eq!(
        (presence(&s, "alpha"), presence(&s, "beta")),
        ("idle", "live")
    );
    assert!(shown(cx, &t, "cluster-dot-alpha"));
}

// covers UP-04: a failed cluster's header has no namespace filter or watch mode (neither can
// work), and Retry shows once, in the main area.
#[gpui_kit::test]
async fn a_failed_cluster_offers_one_retry_and_no_dead_controls(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch(
            "workspace::SelectCluster",
            json!({"cluster": "beta"}),
            window,
            cx,
        )
    })
    .unwrap()
    .unwrap();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .active_cluster()
            .is_some_and(|c| matches!(c.state, ConnectionState::Failed(_)))
    })
    .await;
    render(cx, &t);
    let s = state(cx, &t);
    assert!(matches!(s.main, Main::Failed { .. }), "{:?}", s.main);
    let header = s.header.unwrap();
    assert_eq!(header.namespace_button, None);
    assert_eq!(header.watch_mode, None);
    assert!(!shown(cx, &t, "namespace-filter-button"));
    assert!(!shown(cx, &t, "watch-mode"));
    assert!(shown(cx, &t, "connection-retry"));
    assert!(!shown(cx, &t, "nav-retry"), "Retry shows twice");

    // A cluster that is still connecting keeps its controls.
    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch(
            "workspace::SelectCluster",
            json!({"cluster": "alpha"}),
            window,
            cx,
        )
    })
    .unwrap()
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some("alpha")
    })
    .await;
    render(cx, &t);
    let header = state(cx, &t).header.unwrap();
    assert_eq!(header.namespace_button.as_deref(), Some("All namespaces"));
    assert_eq!(header.watch_mode.as_deref(), Some("Live: all kinds"));
    assert!(shown(cx, &t, "watch-mode"));
}

// covers UP-02: with no contexts at all, the main area says where Flint looked and what to do,
// not "Select a cluster".
#[gpui_kit::test]
async fn no_contexts_explains_where_flint_looked(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, workspace, _model) = open_empty_workspace(cx, dir.path()).await;
    cx.run_until_parked();
    let main = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("main-no-contexts").is_some());
            // One line per path, however long: cut, not wrapped mid-name.
            let path = window.find(("searched-path", 0usize)).bounds();
            assert!(
                f32::from(path.size.height) < 20.,
                "the searched path wraps: {path:?}"
            );
            let env = window.find("kubeconfig-env").bounds();
            assert!(
                f32::from(env.size.height) < 24.,
                "KUBECONFIG wraps: {env:?}"
            );
            workspace
                .update(cx, |ws, cx| ws.view_state(window, cx))
                .main
        })
        .unwrap();
    assert_eq!(main, Main::NoContexts);
}
