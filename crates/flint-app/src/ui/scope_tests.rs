//! View tests for the AI scope in the Expose to AI dialog and the Edit AI Scope dialog
//! (UP-14): the pickers are fed from discovery and the cluster's namespaces, what is checked is
//! written with the exposure in one change, and the sidebar's AI tag opens the scope again.

use flint_core::settings::AiScope;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::super::{expose_dialog, scope_dialog};
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace, standard_sections};

async fn wait_for_dialog(cx: &mut TestAppContext, t: &TestWorkspace, open: bool) {
    cx.wait_for(t.window, WAIT, move |window, cx| {
        gpui_kit::component::WindowExt::has_active_dialog(window, cx) == open
    })
    .await;
}

/// Clicks `id` and draws a frame, so what the click changed is on screen.
fn click(cx: &mut TestAppContext, t: &TestWorkspace, id: &'static str) {
    t.click(cx, id);
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

fn scope_of(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<AiScope> {
    t.model.read_with(cx, |m, _| {
        m.clusters
            .iter()
            .find(|c| c.info.name == "alpha")
            .and_then(|c| m.ai_scope(c))
    })
}

async fn wait_for_scope(cx: &mut TestAppContext, t: &TestWorkspace, wanted: AiScope) {
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let m = model.read(cx);
        m.clusters
            .iter()
            .find(|c| c.info.name == "alpha")
            .and_then(|c| m.ai_scope(c))
            == Some(wanted.clone())
    })
    .await;
}

fn scope(namespaces: &[&str], kinds: &[&str]) -> AiScope {
    AiScope::new(
        namespaces.iter().map(|s| (*s).to_owned()).collect(),
        kinds.iter().map(|s| (*s).to_owned()).collect(),
    )
}

// covers UP-14: exposing offers the cluster's namespaces and kinds; what is checked is written
// with the exposure. The AI tag then says what agents may read and opens Edit AI Scope, whose
// pickers show the scope; clearing it exposes everything again, in one rewrite of the entry.
#[gpui_kit::test]
async fn the_expose_dialog_limits_what_agents_read(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.workspace.update(cx, |ws, _| {
        ws.namespaces = vec!["demo".into(), "shop".into()];
    });

    t.dispatch(cx, "workspace::ExposeToAi", json!({"cluster": "alpha"}))
        .unwrap();
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(
        t.label(cx, "expose-description"),
        Some(expose_dialog("alpha", &t.server, None).text)
    );
    for id in [
        "scope-ns-all",
        "scope-ns-demo",
        "scope-ns-shop",
        "scope-kind-all",
        "scope-kind-pods",
        "scope-kind-deployments.apps",
        "scope-kind-nodes",
    ] {
        assert!(t.find(cx, id).is_some(), "{id}");
    }
    assert!(t.find(cx, "scope-cluster-wide-note").is_none());
    click(cx, &t, "scope-ns-shop");
    assert!(t.find(cx, "scope-cluster-wide-note").is_some());
    click(cx, &t, "scope-kind-pods");
    click(cx, &t, "scope-kind-nodes");
    click(cx, &t, "scope-kind-pods");
    click(cx, &t, "scope-kind-deployments.apps");
    // Nothing is written before confirming.
    assert!(!t.settings_toml().contains("alpha"));
    t.click(cx, "ok");
    wait_for_scope(cx, &t, scope(&["shop"], &["deployments.apps", "nodes"])).await;
    wait_for_dialog(cx, &t, false).await;
    let toml = t.settings_toml();
    assert!(
        toml.contains("namespaces = [\"shop\"]") && toml.contains(&t.server),
        "{toml}"
    );

    // The AI tag says what agents may read, and opens the scope again with it checked.
    let tip = t.description(cx, "ai-scope-alpha").unwrap_or_default();
    assert!(
        tip.contains("namespaces shop; kinds deployments.apps, nodes"),
        "{tip}"
    );
    assert_eq!(t.label(cx, "ai-scope-alpha").as_deref(), Some("AI·"));
    t.click(cx, "ai-scope-alpha");
    wait_for_dialog(cx, &t, true).await;
    let saved = scope(&["shop"], &["deployments.apps", "nodes"]);
    assert_eq!(
        t.label(cx, "scope-description"),
        Some(scope_dialog("alpha", &t.server, &saved).text)
    );
    assert_eq!(t.label(cx, "ok").as_deref(), Some("Save"));
    // Escape keeps the scope.
    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;
    assert_eq!(scope_of(cx, &t), Some(saved));

    // Clearing both pickers exposes everything again.
    t.dispatch(cx, "workspace::EditAiScope", json!({"cluster": "alpha"}))
        .unwrap();
    wait_for_dialog(cx, &t, true).await;
    click(cx, &t, "scope-ns-all");
    click(cx, &t, "scope-kind-all");
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    wait_for_scope(cx, &t, AiScope::default()).await;
    let toml = t.settings_toml();
    assert!(
        !toml.contains("namespaces") && toml.contains("alpha"),
        "{toml}"
    );
    assert_eq!(t.label(cx, "ai-scope-alpha").as_deref(), Some("AI"));
}

// covers UP-14: only an exposed cluster has a scope to edit; a cluster that is not connected
// still shows its saved scope in the pickers, with a word on how to list more.
#[gpui_kit::test]
async fn edit_ai_scope_needs_an_exposed_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    assert!(matches!(
        t.dispatch(cx, "workspace::EditAiScope", json!({"cluster": "alpha"})),
        Err(ActionError::Rejected { .. })
    ));
    assert!(t.find(cx, "ai-scope-alpha").is_none());
    let server = t.server.clone();
    t.model.update(cx, |m, _| {
        m.expose_to_ai("alpha".into(), server, scope(&["shop"], &["pods"]))
    });
    wait_for_scope(cx, &t, scope(&["shop"], &["pods"])).await;
    t.dispatch(cx, "workspace::EditAiScope", json!({"cluster": "alpha"}))
        .unwrap();
    wait_for_dialog(cx, &t, true).await;
    for id in ["scope-ns-shop", "scope-kind-pods"] {
        assert!(t.find(cx, id).is_some(), "{id}");
    }
    assert!(t.find(cx, "scope-ns-demo").is_none());
}
