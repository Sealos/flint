//! View tests for the fleet home screen (#806): shown on launch and from the sidebar's Home
//! entry, cards only for the clusters kept watching, and the active cluster kept meanwhile.

use std::collections::BTreeMap;

use flint_core::keep_watching::{BackgroundSignIn, KeepWatchView, KeepWatching};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::json;

use crate::ui::registry;
use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace};
use crate::ui::view_state::{AppState, HomeCardState, HomeRowState, Main};

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

fn dispatch(cx: &mut TestAppContext, t: &TestWorkspace, name: &str, args: serde_json::Value) {
    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch(name, args, window, cx)
    })
    .unwrap()
    .unwrap();
}

fn view(status: KeepWatching) -> KeepWatchView {
    KeepWatchView {
        status,
        sign_in: BackgroundSignIn::NotNeeded,
        command: None,
        shown_sign_in: None,
        definition: Some(1),
        refusal: None,
    }
}

fn keep_watching(cx: &mut TestAppContext, t: &TestWorkspace, views: &[(&str, KeepWatching)]) {
    let views: BTreeMap<String, KeepWatchView> = views
        .iter()
        .map(|(cluster, status)| ((*cluster).to_owned(), view(*status)))
        .collect();
    t.model.update(cx, |model, cx| {
        model.set_keep_watching_for_test(views);
        cx.notify();
    });
}

fn row(cluster: &str, keep_watching: bool) -> HomeRowState {
    HomeRowState {
        cluster: cluster.to_owned(),
        label: cluster.to_owned(),
        keep_watching,
    }
}

// covers UP-01: launch shows the home screen, listing every context with its actions when none
// is kept watching, and connects nothing.
#[gpui_kit::test]
async fn launch_shows_home_with_every_cluster_listed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    keep_watching(
        cx,
        &t,
        &[
            ("kind-alpha", KeepWatching::Off),
            ("kind-beta", KeepWatching::Off),
        ],
    );
    render(cx, &t);
    let s = state(cx, &t);
    assert_eq!(
        s.main,
        Main::Home {
            watched: vec![],
            unwatched: vec![row("kind-alpha", true), row("kind-beta", true)],
        }
    );
    assert_eq!(s.active_cluster, None);
    assert!(shown(cx, &t, "home"));
    assert!(shown(cx, &t, "home-no-watched"));
    assert!(shown(cx, &t, "home-entry"));
    for cluster in ["kind-alpha", "kind-beta"] {
        assert_eq!(
            s.clusters
                .iter()
                .find(|c| c.name == cluster)
                .unwrap()
                .connection,
            "disconnected"
        );
    }
}

// Clusters kept watching get a card, in sidebar order, saying whether they watch or need a
// sign-in; the rest stay in the list.
#[gpui_kit::test]
async fn watched_clusters_get_cards(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    keep_watching(
        cx,
        &t,
        &[
            ("kind-alpha", KeepWatching::Off),
            ("kind-beta", KeepWatching::NeedsConfirming),
        ],
    );
    render(cx, &t);
    let Main::Home { watched, unwatched } = state(cx, &t).main else {
        panic!("not the home screen");
    };
    assert_eq!(unwatched, [row("kind-alpha", true)]);
    assert_eq!(watched.len(), 1);
    let HomeCardState {
        cluster, status, ..
    } = &watched[0];
    assert_eq!((cluster.as_str(), *status), ("kind-beta", "sign-in needed"));
    assert!(shown(cx, &t, "home-card-kind-beta"));
    assert!(shown(cx, &t, "home-open-kind-beta"));
    assert!(!shown(cx, &t, "home-no-watched"));
}

// covers UP-03: Home opens over the active cluster without disconnecting it, and selecting the
// cluster again goes back to its view.
#[gpui_kit::test]
async fn home_keeps_the_active_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    dispatch(
        cx,
        &t,
        "workspace::SelectCluster",
        json!({"cluster": "kind-alpha"}),
    );
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some("kind-alpha")
    })
    .await;
    render(cx, &t);
    assert!(!matches!(state(cx, &t).main, Main::Home { .. }));

    dispatch(cx, &t, "workspace::OpenHome", json!(null));
    render(cx, &t);
    let s = state(cx, &t);
    assert!(matches!(s.main, Main::Home { .. }), "{:?}", s.main);
    assert_eq!(s.active_cluster.as_deref(), Some("kind-alpha"));
    assert_ne!(
        s.clusters
            .iter()
            .find(|c| c.name == "kind-alpha")
            .unwrap()
            .connection,
        "disconnected"
    );
    assert!(shown(cx, &t, "home"));

    dispatch(
        cx,
        &t,
        "workspace::SelectCluster",
        json!({"cluster": "kind-alpha"}),
    );
    render(cx, &t);
    let s = state(cx, &t);
    assert!(!matches!(s.main, Main::Home { .. }), "{:?}", s.main);
    assert_eq!(s.active_cluster.as_deref(), Some("kind-alpha"));
    assert!(!shown(cx, &t, "home"));
}

// Choosing another cluster while Home shows over the active one leaves Home for it.
#[gpui_kit::test]
async fn selecting_another_cluster_leaves_home(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    dispatch(
        cx,
        &t,
        "workspace::SelectCluster",
        json!({"cluster": "kind-alpha"}),
    );
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some("kind-alpha")
    })
    .await;
    render(cx, &t);
    dispatch(cx, &t, "workspace::OpenHome", json!(null));
    render(cx, &t);
    assert!(matches!(state(cx, &t).main, Main::Home { .. }));
    dispatch(
        cx,
        &t,
        "workspace::SelectCluster",
        json!({"cluster": "kind-beta"}),
    );
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some("kind-beta")
    })
    .await;
    render(cx, &t);
    assert!(!matches!(state(cx, &t).main, Main::Home { .. }));
}
