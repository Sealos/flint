//! View tests for the Expose to AI toggle and its confirmation dialog: exposing asks first,
//! hiding does not, a stray key cannot flip it, and a re-pointed context shows as changed.

use flint_core::settings::AiExposureState;
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::super::expose_dialog;
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{
    TestWorkspace, WAIT, alpha_and_beta, open_workspace, open_workspace_with,
};

fn exposure(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) -> AiExposureState {
    t.model.read_with(cx, |m, _| {
        let c = m.clusters.iter().find(|c| c.info.name == cluster).unwrap();
        m.ai_exposure(c)
    })
}

async fn wait_for_exposure(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    cluster: &'static str,
    wanted: AiExposureState,
) {
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let m = model.read(cx);
        m.clusters
            .iter()
            .find(|c| c.info.name == cluster)
            .is_some_and(|c| m.ai_exposure(c) == wanted)
    })
    .await;
}

async fn wait_for_dialog(cx: &mut TestAppContext, t: &TestWorkspace, open: bool) {
    cx.wait_for(t.window, WAIT, move |window, cx| {
        gpui_kit::component::WindowExt::has_active_dialog(window, cx) == open
    })
    .await;
}

fn toggle_tip(cx: &mut TestAppContext, t: &TestWorkspace, id: &'static str) -> String {
    t.description(cx, id).unwrap_or_default()
}

// covers UP-07: clicking the toggle asks first, with Expose and Cancel; Esc cancels. Only after
// confirming is the cluster exposed, and settings.toml names the context and its server.
// Hiding needs no confirmation. Clicking the toggle does not select the cluster.
#[gpui_kit::test]
async fn exposing_asks_first_and_hiding_does_not(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    assert!(toggle_tip(cx, &t, "expose-kind-alpha").starts_with("Hidden from AI agents"));

    t.click(cx, "expose-kind-alpha");
    wait_for_dialog(cx, &t, true).await;
    // It names the context and the server it points at.
    assert_eq!(
        t.label(cx, "expose-description"),
        Some(expose_dialog("kind-alpha", &t.server, None).text)
    );
    assert_eq!(t.label(cx, "ok").as_deref(), Some("Expose"));
    assert_eq!(t.label(cx, "cancel").as_deref(), Some("Cancel"));
    assert_eq!(exposure(cx, &t, "kind-alpha"), AiExposureState::Hidden);
    assert_eq!(t.model.read_with(cx, |m, _| m.active.clone()), None);
    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;
    assert_eq!(exposure(cx, &t, "kind-alpha"), AiExposureState::Hidden);
    assert!(!t.settings_toml().contains("kind-alpha"));

    t.click(cx, "expose-kind-alpha");
    wait_for_dialog(cx, &t, true).await;
    t.click(cx, "ok");
    wait_for_exposure(cx, &t, "kind-alpha", AiExposureState::Exposed).await;
    wait_for_dialog(cx, &t, false).await;
    assert!(toggle_tip(cx, &t, "expose-kind-alpha").starts_with("Exposed to AI agents"));
    let toml = t.settings_toml();
    assert!(toml.contains("\"kind-alpha\""), "{toml}");
    assert!(toml.contains(&t.server), "{toml}");
    // Only kind-alpha.
    assert!(!toml.contains("kind-beta"), "{toml}");
    assert_eq!(exposure(cx, &t, "kind-beta"), AiExposureState::Hidden);
    // Exposing it again would do nothing.
    assert!(matches!(
        t.dispatch(
            cx,
            "workspace::ExposeToAi",
            json!({"cluster": "kind-alpha"})
        ),
        Err(ActionError::Rejected { .. })
    ));

    t.click(cx, "expose-kind-alpha");
    assert!(!t.has_dialog(cx));
    wait_for_exposure(cx, &t, "kind-alpha", AiExposureState::Hidden).await;
    assert!(!t.has_dialog(cx));
    assert!(toggle_tip(cx, &t, "expose-kind-alpha").starts_with("Hidden from AI agents"));
}

// covers UP-07: the toggle is not a tab stop, so Tab never lands on it, and Space or Enter
// with focus anywhere in the window cannot open the dialog or expose anything.
#[gpui_kit::test]
async fn a_stray_key_press_cannot_toggle_exposure(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    for focus_list in [false, true] {
        if focus_list {
            let list = t.list(cx).unwrap();
            let focus = list.read_with(cx, |l, _| l.focus_handle().clone());
            cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
                .unwrap();
        } else {
            t.focus_workspace(cx);
        }
        for _ in 0..4 {
            t.press(cx, "tab");
            for id in ["expose-kind-alpha", "expose-kind-beta"] {
                assert_ne!(t.find(cx, id).unwrap().focused(), Some(true), "{id}");
            }
            t.press(cx, "space");
            t.press(cx, "enter");
            assert!(!t.has_dialog(cx));
        }
    }
    assert_eq!(exposure(cx, &t, "kind-alpha"), AiExposureState::Hidden);
    assert_eq!(exposure(cx, &t, "kind-beta"), AiExposureState::Hidden);
    assert!(!t.settings_toml().contains("kind-alpha"));
}

// covers UP-08: an exposed context that now points at another server shows as changed and is
// not exposed. It can be exposed again at the new server (after the same confirmation), or its
// old exposure removed without one.
#[gpui_kit::test]
async fn a_repointed_context_shows_as_changed(cx: &mut TestAppContext) {
    let old = "https://10.0.0.1:6443";
    let dir = tempfile::tempdir().unwrap();
    // As an earlier session left it: kind-alpha and kind-beta exposed at servers they no longer use.
    std::fs::write(
        dir.path().join("settings.toml"),
        format!(
            "ai_exposed_clusters = [\n  {{ context = \"kind-alpha\", server = \"{old}\" }},\n  \
             {{ context = \"kind-beta\", server = \"https://10.0.0.2:6443\" }},\n]\n"
        ),
    )
    .unwrap();
    let t = open_workspace_with(cx, dir.path(), alpha_and_beta).await;
    assert_eq!(
        exposure(cx, &t, "kind-alpha"),
        AiExposureState::ServerChanged {
            exposed_server: old.into()
        }
    );
    assert!(toggle_tip(cx, &t, "expose-kind-alpha").starts_with("Cluster changed"));
    assert!(!exposure(cx, &t, "kind-alpha").is_exposed());
    let available = t.available(cx);
    assert!(
        available.contains(&"workspace::ExposeToAi"),
        "{available:?}"
    );
    assert!(available.contains(&"workspace::HideFromAi"));

    // Expose again: confirmed, then exposed at the server the context points at now.
    t.dispatch(
        cx,
        "workspace::ExposeToAi",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    wait_for_dialog(cx, &t, true).await;
    // It names the new server, and the old one.
    assert_eq!(
        t.label(cx, "expose-description"),
        Some(expose_dialog("kind-alpha", &t.server, Some(old)).text)
    );
    assert!(!exposure(cx, &t, "kind-alpha").is_exposed());
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    wait_for_exposure(cx, &t, "kind-alpha", AiExposureState::Exposed).await;
    let toml = t.settings_toml();
    assert!(toml.contains(&t.server) && !toml.contains(old), "{toml}");

    // Remove kind-beta's old exposure: no dialog.
    t.dispatch(cx, "workspace::HideFromAi", json!({"cluster": "kind-beta"}))
        .unwrap();
    assert!(!t.has_dialog(cx));
    wait_for_exposure(cx, &t, "kind-beta", AiExposureState::Hidden).await;
    assert!(toggle_tip(cx, &t, "expose-kind-beta").starts_with("Hidden from AI agents"));
}
