//! View tests for what the window says about a cluster: similar names stay distinct on every
//! surface, a changed URL that displays the same is explained, a missing namespace names itself
//! and keeps its way out, and an unreadable kubeconfig explains the actions it disables.

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext, px, size};
use serde_json::{Value, json};

use super::super::{trust_dialog, unlock_dialog, writes_paused_reason};
use super::row_actions_tests::{
    activate, dispatch, draw, open_list, pod_detail, pods_entry, show_fetched,
};
use crate::ui::resource_actions::{available, offered};
use crate::ui::support_tests::{
    TestWorkspace, kubeconfig_yaml, open_workspace, open_workspace_with, pods,
};
use crate::ui::view_state::AppState;

const SIMILAR_A: &str = "team-platform-production-aaaa-eu-west-1-cluster";
const SIMILAR_B: &str = "team-platform-production-bbbb-eu-west-1-cluster";

fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> AppState {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.view_state(window, cx))
    })
    .unwrap()
}

fn texts(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<String> {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        gpui_kit::base::test_support::texts(window)
            .into_iter()
            .map(|text| text.text().to_owned())
            .collect()
    })
    .unwrap()
}

fn set_provisional(cx: &mut TestAppContext, t: &TestWorkspace, provisional: bool) {
    t.model.update(cx, |model, cx| {
        let alpha = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        alpha.provisional = provisional;
        cx.notify();
    });
    draw(cx, t);
}

fn shell_disabled(cx: &mut TestAppContext, t: &TestWorkspace) -> bool {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        window.find("detail-shell").disabled() == Some(true)
    })
    .unwrap()
}

#[gpui_kit::test]
async fn similar_names_differ_on_every_surface(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let path = dir.join("kubeconfig");
        let contexts = [
            (SIMILAR_A, servers.silent.as_str()),
            (SIMILAR_B, servers.refused.as_str()),
        ];
        std::fs::write(&path, kubeconfig_yaml(&contexts)).unwrap();
        vec![path]
    })
    .await;

    let shown = state(cx, &t);
    assert_ne!(shown.clusters[0].label, shown.clusters[1].label);

    let mut crumbs = Vec::new();
    for context in [SIMILAR_A, SIMILAR_B] {
        activate(cx, &t, context).await;
        draw(cx, &t);
        crumbs.push(state(cx, &t).header.expect("a header").breadcrumb[0].clone());
    }
    assert_ne!(crumbs[0], crumbs[1], "the header");

    let names = t.model.read_with(cx, |model, _| model.names());
    let (a, b) = (names.with_real(SIMILAR_A), names.with_real(SIMILAR_B));
    assert_ne!(
        unlock_dialog(&a, SIMILAR_A, "https://a").title,
        unlock_dialog(&b, SIMILAR_B, "https://a").title
    );
    assert_ne!(
        trust_dialog(&a, SIMILAR_A, "https://a", None).title,
        trust_dialog(&b, SIMILAR_B, "https://a", None).title
    );
    assert_ne!(
        writes_paused_reason(&names.exact(SIMILAR_A)),
        writes_paused_reason(&names.exact(SIMILAR_B))
    );
}

// The pins differ from the context's URL only in user info, which is never shown.
#[gpui_kit::test]
async fn a_changed_url_that_displays_the_same_is_explained(cx: &mut TestAppContext) {
    const SERVER: &str = "https://127.0.0.1:9";
    const PINNED: &str = "https://someone:hidden-value@127.0.0.1:9";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        format!(
            "protect_local_clusters = true\n\
             trusted_clusters = [{{ context = \"kind-alpha\", server = \"{PINNED}\" }}]\n\
             ai_exposed_clusters = [{{ context = \"kind-alpha\", server = \"{PINNED}\" }}]\n"
        ),
    )
    .unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, _| {
        let path = dir.join("kubeconfig");
        std::fs::write(&path, kubeconfig_yaml(&[("kind-alpha", SERVER)])).unwrap();
        vec![path]
    })
    .await;
    draw(cx, &t);

    let tip = t.description(cx, "cluster-kind-alpha").unwrap_or_default();
    for pin in ["trusted", "exposed"] {
        assert!(
            tip.contains(&format!("{pin} at another URL that shows the same here")),
            "{tip}"
        );
    }
    assert!(
        !tip.contains("hidden-value") && !tip.contains("someone"),
        "{tip}"
    );
}

#[gpui_kit::test]
async fn a_missing_namespace_is_named_and_the_rows_come_back(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    t.dispatch(
        cx,
        "workspace::SetNamespaces",
        json!({"namespaces": ["dem"]}),
    )
    .unwrap();
    t.wait_for_element(cx, "show-all-namespaces").await;
    assert_eq!(
        t.label(cx, "missing-namespace-text").as_deref(),
        Some("No Pods in namespace dem")
    );

    t.click(cx, "show-all-namespaces");
    t.wait_for_element(cx, "row-uid-web").await;
    assert_eq!(t.row_count(cx), 1);
}

// A redraw before the recompute must not name a selection the rows were not computed for.
#[gpui_kit::test]
async fn the_message_waits_for_the_namespaces_selected(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    t.dispatch(
        cx,
        "workspace::SetNamespaces",
        json!({"namespaces": ["dem"]}),
    )
    .unwrap();
    t.wait_for_element(cx, "show-all-namespaces").await;

    let list = t.list(cx).unwrap();
    let permits = list.update(cx, |list, _| list.hold_computes());
    t.dispatch(
        cx,
        "workspace::SetNamespaces",
        json!({"namespaces": ["demo"]}),
    )
    .unwrap();
    draw(cx, &t);
    assert!(t.find(cx, "missing-namespace-text").is_none());
    assert!(t.find(cx, "show-all-namespaces").is_none());
    assert!(t.find(cx, "list-filtering").is_some());

    permits.send(()).await.unwrap();
    t.wait_for_element(cx, "row-uid-web").await;
}

// A long selection stays one summarized line, and the button stays above a tall dock.
#[gpui_kit::test]
async fn show_all_stays_reachable_in_a_small_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    let long: Vec<String> = (0..12)
        .map(|i| format!("{}-{i}", "missing".repeat(12)))
        .collect();
    t.dispatch(
        cx,
        "workspace::SetNamespaces",
        json!({ "namespaces": long }),
    )
    .unwrap();
    t.wait_for_element(cx, "show-all-namespaces").await;
    let title = t.label(cx, "missing-namespace-text").unwrap();
    assert!(title.ends_with("and 10 more"), "{title}");

    t.dispatch(cx, "dock::ToggleDock", Value::Null).unwrap();
    t.dispatch(cx, "workspace::SetDockHeight", json!({"height": 400}))
        .unwrap();
    VisualTestContext::from_window(t.window, cx).simulate_resize(size(px(760.), px(420.)));
    draw(cx, &t);
    let (button, dock) = cx
        .update_window(t.window, |_, window, _| {
            (
                window.find("show-all-namespaces").bounds(),
                window.find("dock-pane").bounds(),
            )
        })
        .unwrap();
    assert!(button.size.height > px(0.));
    assert!(button.bottom() <= dock.top(), "{button:?} above {dock:?}");
}

#[gpui_kit::test]
async fn an_unreadable_kubeconfig_explains_the_disabled_actions(cx: &mut TestAppContext) {
    const WHY: &str = "a kubeconfig file can't be read";
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    dispatch(
        cx,
        &t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    draw(cx, &t);
    show_fetched(cx, &t, pod_detail("web"));
    assert!(!shell_disabled(cx, &t));
    let flag = |shown: &AppState| shown.clusters[0].kubeconfig_unreadable;
    assert!(!flag(&state(cx, &t)));

    set_provisional(cx, &t, true);
    let shown = state(cx, &t);
    assert!(flag(&shown));
    assert_eq!(shown.clusters[0].connection, "connecting");
    assert!(shell_disabled(cx, &t));
    assert!(
        texts(cx, &t)
            .iter()
            .any(|s| s.starts_with("Can't open a shell") && s.contains(WHY)),
        "the caption says why"
    );
    let detail = t.detail(cx).unwrap();
    t.workspace.read_with(cx, |ws, cx| {
        let edit = detail.read(cx).edit_blocked().unwrap();
        assert!(edit.contains(WHY), "{edit}");
        for action in offered(ws, detail.read(cx), cx) {
            let why = available(ws, action, cx).unwrap_err();
            assert!(why.contains(WHY), "{why}");
        }
    });

    set_provisional(cx, &t, false);
    assert!(!flag(&state(cx, &t)));
    assert!(!shell_disabled(cx, &t));
    assert!(!texts(cx, &t).iter().any(|s| s.contains(WHY)));
    let edit = detail.read_with(cx, |d, _| d.edit_blocked());
    assert!(!edit.is_some_and(|why| why.contains(WHY)));
}
