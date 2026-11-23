//! View tests for protected clusters (ADR-0015): Shell is disabled on a protected cluster,
//! saying why, with the unlock in place; an unlock asks first, naming the server, and lasts
//! until Lock Again; trust is kept in `settings.toml`, pinned to the server. The test contexts
//! are known local clusters, so the tests turn on `protect_local_clusters` to see a protected one.

use flint_core::settings::TrustState;
use flint_core::{Event, ObjectDetail, Protection};
use gpui_kit::test::{TestAppContextExt as _, TestQueryExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::super::{expose_dialog, scope_dialog, trust_dialog, unlock_dialog};
use super::row_actions_tests::right_click;
use crate::ui::dock::{DockItem as _, TabStatus};
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{
    TestWorkspace, WAIT, alpha_and_beta, kubeconfig_yaml, open_fake_shell, open_workspace_with,
};

const PROTECT_LOCAL: &str = "protect_local_clusters = true\n";

async fn open_with_settings(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    settings: &str,
) -> TestWorkspace {
    std::fs::write(dir.join("settings.toml"), settings).unwrap();
    open_workspace_with(cx, dir, alpha_and_beta).await
}

fn protection(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) -> Protection {
    t.model.read_with(cx, |m, _| m.protection_of(cluster))
}

async fn wait_for_protection(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    cluster: &'static str,
    wanted: Protection,
) {
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).protection_of(cluster) == wanted
    })
    .await;
}

async fn wait_for_dialog(cx: &mut TestAppContext, t: &TestWorkspace, open: bool) {
    cx.wait_for(t.window, WAIT, move |window, cx| {
        gpui_kit::component::WindowExt::has_active_dialog(window, cx) == open
    })
    .await;
}

fn draw(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.run_until_parked();
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

/// Whether element `id` is drawn disabled (`None`: not drawn).
fn disabled(cx: &mut TestAppContext, t: &TestWorkspace, id: &'static str) -> Option<bool> {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        window.try_find(id).map(|b| b.disabled() == Some(true))
    })
    .unwrap()
}

/// The captions under the detail pane's buttons.
fn captions(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<String> {
    let detail = t.detail(cx).unwrap();
    detail.read_with(cx, |d, _| d.action_captions())
}

/// Selects pod `web` in `demo` and hands the pane the object: one running container.
async fn show_web(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    let detail = t.detail(cx).expect("the detail pane is open");
    let pod = ObjectDetail {
        object: json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web"},
            "spec": {"containers": [{"name": "app"}]},
            "status": {"containerStatuses": [
                {"name": "app", "restartCount": 0, "state": {"running": {}}}
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

/// What the banner over the dock says, if anything.
fn refusal(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    t.workspace
        .read_with(cx, |ws, _| ws.action_error().map(str::to_owned))
}

/// The custom names in the settings the app holds.
fn custom_names(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<(String, String)> {
    t.model.read_with(cx, |m, _| {
        m.settings
            .cluster_names
            .iter()
            .map(|(c, n)| (c.clone(), n.clone()))
            .collect()
    })
}

async fn wait_for_name(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    cluster: &'static str,
    name: Option<&'static str>,
) {
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .settings
            .cluster_names
            .get(cluster)
            .map(String::as_str)
            == name
    })
    .await;
}

async fn open_rename(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) {
    t.dispatch(
        cx,
        "workspace::RenameCluster",
        json!({ "cluster": cluster }),
    )
    .unwrap();
    wait_for_dialog(cx, t, true).await;
    draw(cx, t);
}

// covers UP-01: the Rename dialog names the real cluster; its button and Enter save, an empty
// field removes the name.
#[gpui_kit::test]
async fn the_rename_dialog_saves_with_its_button_and_with_enter(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;

    open_rename(cx, &t, "kind-alpha").await;
    let text = t.label(cx, "rename-cluster-description").unwrap();
    assert!(
        text.contains(&format!("Context: kind-alpha\nServer: {}", t.server)),
        "{text}"
    );
    t.type_text(cx, "main");
    t.click(cx, "ok");
    wait_for_dialog(cx, &t, false).await;
    wait_for_name(cx, &t, "kind-alpha", Some("main")).await;
    draw(cx, &t);
    assert_eq!(t.label(cx, "cluster-kind-alpha").as_deref(), Some("main"));
    let file = std::fs::read_to_string(dir.path().join("settings.toml")).unwrap();
    assert!(
        file.contains("[cluster_names]") && file.contains("kind-alpha = \"main\""),
        "{file}"
    );

    open_rename(cx, &t, "kind-beta").await;
    t.type_text(cx, "blue");
    t.press(cx, "enter");
    wait_for_dialog(cx, &t, false).await;
    wait_for_name(cx, &t, "kind-beta", Some("blue")).await;

    // The field starts with the current name; emptied, the context name comes back.
    open_rename(cx, &t, "kind-alpha").await;
    t.press(cx, "secondary-a");
    t.press(cx, "backspace");
    t.click(cx, "ok");
    wait_for_dialog(cx, &t, false).await;
    wait_for_name(cx, &t, "kind-alpha", None).await;
    draw(cx, &t);
    assert_eq!(
        t.label(cx, "cluster-kind-alpha").as_deref(),
        Some("kind-alpha")
    );
}

// covers UP-01: cancelling leaves the settings as they were.
#[gpui_kit::test]
async fn cancelling_the_rename_dialog_changes_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let settings = format!("{PROTECT_LOCAL}\n[cluster_names]\nkind-alpha = \"main\"\n");
    let t = open_with_settings(cx, dir.path(), &settings).await;
    let path = dir.path().join("settings.toml");
    let before = std::fs::read_to_string(&path).unwrap();
    for cancel in ["escape", "button"] {
        open_rename(cx, &t, "kind-alpha").await;
        t.type_text(cx, "-changed");
        match cancel {
            "escape" => t.press(cx, "escape"),
            _ => t.click(cx, "cancel"),
        }
        wait_for_dialog(cx, &t, false).await;
        draw(cx, &t);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "{cancel}");
        assert_eq!(
            custom_names(cx, &t),
            [("kind-alpha".to_owned(), "main".to_owned())]
        );
    }
}

// covers UP-01: a name that is not valid, or could pass for another cluster, keeps the dialog
// open, saying why, and writes nothing.
#[gpui_kit::test]
async fn the_rename_dialog_refuses_an_invalid_name(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let settings = format!("{PROTECT_LOCAL}\n[cluster_names]\nkind-alpha = \"main\"\n");
    let t = open_with_settings(cx, dir.path(), &settings).await;
    let path = dir.path().join("settings.toml");
    let before = std::fs::read_to_string(&path).unwrap();
    open_rename(cx, &t, "kind-beta").await;
    for (typed, says) in [("MAIN", "already goes by"), ("!", "at most")] {
        t.press(cx, "secondary-a");
        t.press(cx, "backspace");
        t.type_text(cx, typed);
        t.click(cx, "ok");
        draw(cx, &t);
        assert!(
            cx.update_window(t.window, |_, window, cx| {
                gpui_kit::component::WindowExt::has_active_dialog(window, cx)
            })
            .unwrap(),
            "{typed}: the dialog stays open"
        );
        let error = t.label(cx, "rename-cluster-error").unwrap();
        assert!(error.contains(says), "{typed}: {error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "{typed}");
    }
    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;
    assert_eq!(
        custom_names(cx, &t),
        [("kind-alpha".to_owned(), "main".to_owned())]
    );
}

// covers UP-54, UP-55: the row menu's Unlock entry, clicked for real, opens the confirmation with
// the full context and server, and nothing unlocks until it is confirmed.
#[gpui_kit::test]
async fn the_row_menu_unlock_entry_asks_before_unlocking(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let settings = format!("{PROTECT_LOCAL}\n[cluster_names]\nkind-alpha = \"main\"\n");
    let t = open_with_settings(cx, dir.path(), &settings).await;
    show_web(cx, &t).await;
    let wanted = unlock_dialog("main (kind-alpha)", "kind-alpha", &t.server);
    // A refusal names the real cluster, whatever the menu's short note says.
    let shell = t.workspace.read_with(cx, |ws, cx| {
        crate::ui::row_menu::row_actions(ws, cx).unwrap().shell
    });
    assert_eq!(
        shell,
        Err("main (kind-alpha) is protected (read-only in Flint) until you unlock it".to_owned())
    );

    right_click(cx, &t, "web").await;
    draw(cx, &t);
    let entry = t.find(cx, "menu-unlock").expect("the Unlock entry");
    assert_eq!(entry.label(), Some("Unlock main (kind-alpha)…"));
    assert_eq!(
        entry.description().map(str::to_owned),
        Some(format!("kind-alpha\n{}", t.server))
    );
    t.click(cx, "menu-unlock");
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(t.label(cx, "unlock-description"), Some(wanted.text.clone()));
    assert!(
        wanted
            .text
            .contains(&format!("Context: kind-alpha\nServer: {}", t.server))
    );
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);

    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;
    draw(cx, &t);
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);

    right_click(cx, &t, "web").await;
    draw(cx, &t);
    t.click(cx, "menu-unlock");
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);
    t.click(cx, "ok");
    wait_for_dialog(cx, &t, false).await;
    wait_for_protection(cx, &t, "kind-alpha", Protection::Unlocked).await;
}

// covers UP-01, UP-55: a custom name is what the sidebar shows, but the row, its tooltip and the
// unlock confirmation still say which real cluster it is.
#[gpui_kit::test]
async fn a_custom_name_never_hides_the_real_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let settings = format!("{PROTECT_LOCAL}\n[cluster_names]\nkind-alpha = \"main\"\n");
    let t = open_with_settings(cx, dir.path(), &settings).await;
    assert_eq!(t.label(cx, "cluster-kind-alpha").as_deref(), Some("main"));
    let tip = t
        .find(cx, "cluster-kind-alpha")
        .and_then(|row| row.description().map(str::to_owned))
        .unwrap();
    assert!(
        tip.starts_with(&format!("kind-alpha\n{}\n", t.server)),
        "{tip}"
    );
    t.model.read_with(cx, |m, _| {
        assert_eq!(
            m.names().subtitle("kind-alpha", Some("host")),
            "kind-alpha · host"
        );
    });

    t.dispatch(
        cx,
        "workspace::UnlockCluster",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    wait_for_dialog(cx, &t, true).await;
    let wanted = unlock_dialog("main (kind-alpha)", "kind-alpha", &t.server);
    assert_eq!(t.label(cx, "unlock-description"), Some(wanted.text.clone()));
    assert!(
        wanted
            .text
            .contains(&format!("Context: kind-alpha\nServer: {}", t.server))
    );
    let shown = cx
        .update_window(t.window, |_, window, cx| {
            crate::ui::confirm::shown(window, cx)
        })
        .unwrap()
        .unwrap();
    assert_eq!(shown.title, "Unlock main (kind-alpha)?");
    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;
    // Protection goes by the context, whatever it is called.
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);
}

// covers UP-54, UP-55: on a protected cluster Shell is disabled, saying why, and Unlock sits
// beside it; Logs still work. Unlocking asks first, naming the cluster and its server; Esc
// keeps it protected. Once unlocked, Shell opens; Lock Again protects it again, and Restart
// Shell is refused, saying why.
#[gpui_kit::test]
async fn a_protected_cluster_disables_shell_until_unlocked(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    show_web(cx, &t).await;
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);
    let why = "kind-alpha is protected (read-only in Flint) until you unlock it";
    assert_eq!(disabled(cx, &t, "detail-shell"), Some(true));
    assert_eq!(disabled(cx, &t, "detail-logs"), Some(false));
    assert_eq!(disabled(cx, &t, "detail-unlock"), Some(false));
    assert_eq!(captions(cx, &t), ["Read-only: unlock to open a shell."]);
    assert_eq!(
        t.dispatch(cx, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into()))
    );
    assert!(
        t.description(cx, "lock-kind-alpha")
            .unwrap_or_default()
            .starts_with("Protected")
    );
    // Locking a protected cluster does nothing.
    assert!(matches!(
        t.dispatch(
            cx,
            "workspace::LockCluster",
            json!({"cluster": "kind-alpha"})
        ),
        Err(ActionError::Rejected { .. })
    ));

    t.click(cx, "detail-unlock");
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(
        t.label(cx, "unlock-description"),
        Some(unlock_dialog("kind-alpha", "kind-alpha", &t.server).text)
    );
    assert_eq!(t.label(cx, "ok").as_deref(), Some("Unlock"));
    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);
    // Enter cancels too: only the Unlock button unlocks.
    t.click(cx, "detail-unlock");
    wait_for_dialog(cx, &t, true).await;
    t.press(cx, "enter");
    wait_for_dialog(cx, &t, false).await;
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);

    t.click(cx, "detail-unlock");
    wait_for_dialog(cx, &t, true).await;
    t.click(cx, "ok");
    wait_for_protection(cx, &t, "kind-alpha", Protection::Unlocked).await;
    assert!(
        t.model.read_with(cx, |m, _| m
            .core()
            .unlocks()
            .is_unlocked("kind-alpha", &t.server)),
        "the core's gate sees the unlock"
    );
    assert_eq!(disabled(cx, &t, "detail-shell"), Some(false));
    assert_eq!(disabled(cx, &t, "detail-unlock"), None);
    assert!(captions(cx, &t).is_empty());
    assert!(
        t.description(cx, "lock-kind-alpha")
            .unwrap_or_default()
            .starts_with("Unlocked")
    );
    // Only kind-alpha: kind-beta is still protected, and the unlock is not written anywhere.
    assert_eq!(protection(cx, &t, "kind-beta"), Protection::Protected);
    assert!(!t.settings_toml().contains("kind-alpha"));

    // The shell opens (and fails at once: the test cluster never connects in the core).
    t.dispatch(cx, "detail::OpenShell", Value::Null).unwrap();
    cx.run_until_parked();
    let shell = t
        .workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).active_terminal().cloned())
        .expect("a shell tab");
    let failed = shell.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        failed.read(cx).tab_status() == TabStatus::Failed
    })
    .await;

    t.dispatch(
        cx,
        "workspace::LockCluster",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    wait_for_protection(cx, &t, "kind-alpha", Protection::Protected).await;
    assert!(!t.model.read_with(cx, |m, _| {
        m.core().unlocks().is_unlocked("kind-alpha", &t.server)
    }));
    assert_eq!(disabled(cx, &t, "detail-shell"), Some(true));
    // The failed shell's Restart shell is disabled, saying why, with Unlock… beside it; the key
    // and the palette find it unavailable.
    assert_eq!(disabled(cx, &t, "terminal-restart"), Some(true));
    assert_eq!(disabled(cx, &t, "terminal-unlock"), Some(false));
    assert!(
        disabled(cx, &t, "terminal-protected").is_some(),
        "the note is drawn"
    );
    assert_eq!(
        shell.read_with(cx, |s, _| s.restart_blocked().map(|b| b.why.clone())),
        Some(why.to_owned())
    );
    let tabs = t
        .workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).tab_ids().len());
    assert_eq!(
        t.dispatch(cx, "dock::RestartShell", Value::Null),
        Err(ActionError::NotAvailable("dock::RestartShell".into()))
    );
    assert_eq!(
        t.workspace
            .read_with(cx, |ws, cx| ws.dock.read(cx).tab_ids().len()),
        tabs,
        "no tab opened"
    );
    assert_eq!(refusal(cx, &t), None);
    // Unlock in place: Restart shell is enabled again.
    t.click(cx, "terminal-unlock");
    wait_for_dialog(cx, &t, true).await;
    t.click(cx, "ok");
    wait_for_protection(cx, &t, "kind-alpha", Protection::Unlocked).await;
    assert_eq!(disabled(cx, &t, "terminal-restart"), Some(false));
    assert_eq!(disabled(cx, &t, "terminal-unlock"), None);
}

// covers UP-55: trusting asks first, naming the server, and writes the trust to settings.toml,
// pinned to the server; Stop Trusting protects the cluster again. A trust left at another
// server counts as protected, and trusting again names both servers.
#[gpui_kit::test]
async fn trust_is_kept_in_settings_and_pinned_to_the_server(cx: &mut TestAppContext) {
    let old = "https://10.0.0.1:6443";
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(
        cx,
        dir.path(),
        &format!(
            "{PROTECT_LOCAL}trusted_clusters = [{{ context = \"kind-beta\", server = \"{old}\" }}]\n"
        ),
    )
    .await;
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);
    assert_eq!(protection(cx, &t, "kind-beta"), Protection::Protected);
    let beta_trust = t.model.read_with(cx, |m, _| {
        m.trust(
            m.clusters
                .iter()
                .find(|c| c.info.name == "kind-beta")
                .unwrap(),
        )
    });
    assert_eq!(
        beta_trust,
        TrustState::ServerChanged {
            trusted_server: old.into()
        }
    );
    let row = t.description(cx, "cluster-kind-beta").unwrap_or_default();
    assert!(
        row.contains(&format!("Cluster changed: trusted at {old}")),
        "{row}"
    );

    t.dispatch(
        cx,
        "workspace::TrustCluster",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(
        t.label(cx, "trust-description"),
        Some(trust_dialog("kind-alpha", "kind-alpha", &t.server, None).text)
    );
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    wait_for_protection(cx, &t, "kind-alpha", Protection::Trusted).await;
    let toml = t.settings_toml();
    assert!(
        toml.contains("trusted_clusters") && toml.contains(&t.server),
        "{toml}"
    );
    assert!(
        t.description(cx, "lock-kind-alpha")
            .unwrap_or_default()
            .starts_with("Trusted")
    );
    assert!(matches!(
        t.dispatch(
            cx,
            "workspace::TrustCluster",
            json!({"cluster": "kind-alpha"})
        ),
        Err(ActionError::Rejected { .. })
    ));

    // Beta, trusted at its old server: trusting again names both.
    t.dispatch(
        cx,
        "workspace::TrustCluster",
        json!({"cluster": "kind-beta"}),
    )
    .unwrap();
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(
        t.label(cx, "trust-description"),
        Some(trust_dialog("kind-beta", "kind-beta", &t.refused_server, Some(old)).text)
    );
    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;

    // Stop Trusting protects it at once, through the gate the core shares, before the new
    // settings come back: the settings the app holds still trust it.
    let (trusted_still, now) = t.model.update(cx, |m, cx| {
        m.untrust_cluster("kind-alpha".into(), cx);
        (
            m.settings
                .trusted_clusters
                .iter()
                .any(|p| p.context == "kind-alpha"),
            m.protection_of("kind-alpha"),
        )
    });
    assert!(trusted_still);
    assert_eq!(now, Protection::Protected);
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .settings
            .trusted_clusters
            .iter()
            .all(|p| p.context != "kind-alpha")
    })
    .await;
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);
    t.dispatch(
        cx,
        "workspace::UntrustCluster",
        json!({"cluster": "kind-beta"}),
    )
    .unwrap();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).settings.trusted_clusters.is_empty()
    })
    .await;
    assert!(matches!(
        t.dispatch(
            cx,
            "workspace::UntrustCluster",
            json!({"cluster": "kind-alpha"})
        ),
        Err(ActionError::Rejected { .. })
    ));
}

// covers UP-55: a trusted context whose cluster entry is gone has no server: it cannot be
// unlocked or trusted, but its old trust can still be removed from the sidebar.
#[gpui_kit::test]
async fn an_old_trust_on_a_context_without_a_server_can_be_removed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "trusted_clusters = [{ context = \"gamma\", server = \"https://10.0.0.3:6443\" }]\n",
    )
    .unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let mut files = alpha_and_beta(dir, servers);
        let lost = dir.join("lost");
        std::fs::write(
            &lost,
            "apiVersion: v1\nkind: Config\nclusters: []\nusers:\n- name: u\n  user: {token: t}\n\
             contexts:\n- name: gamma\n  context: {cluster: gone, user: u}\n",
        )
        .unwrap();
        files.push(lost);
        files
    })
    .await;
    assert_eq!(protection(cx, &t, "gamma"), Protection::Protected);
    assert_eq!(disabled(cx, &t, "lock-gamma"), Some(false));
    assert!(
        t.description(cx, "lock-gamma")
            .unwrap_or_default()
            .contains("remove its old trust")
    );
    for action in ["UnlockCluster", "TrustCluster"] {
        assert!(
            matches!(
                t.dispatch(
                    cx,
                    &format!("workspace::{action}"),
                    json!({"cluster": "gamma"})
                ),
                Err(ActionError::Rejected { .. })
            ),
            "{action}"
        );
    }
    t.dispatch(cx, "workspace::UntrustCluster", json!({"cluster": "gamma"}))
        .unwrap();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).settings.trusted_clusters.is_empty()
    })
    .await;
    assert_eq!(
        disabled(cx, &t, "lock-gamma"),
        Some(true),
        "nothing left to do"
    );
}

/// Whether the accessibility tree holds exactly one element named `label`, and it is `id`.
fn named(cx: &mut TestAppContext, t: &TestWorkspace, id: &str, label: &str) -> bool {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        window
            .try_find_by_label(label)
            .is_some_and(|found| format!("{:?}", found.path().last()).contains(id))
    })
    .unwrap()
}

// The lock button's accessible name follows the cluster's real protection.
#[gpui_kit::test]
async fn the_lock_buttons_name_says_the_protection(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    let id = "lock-kind-alpha";
    let label = |state: &str| format!("Lock for kind-alpha: {state}");
    assert!(named(cx, &t, id, &label("protected, read-only")));

    let server = t.server.clone();
    t.model
        .update(cx, |m, cx| m.unlock("kind-alpha", &server, cx));
    wait_for_protection(cx, &t, "kind-alpha", Protection::Unlocked).await;
    assert!(named(cx, &t, id, &label("unlocked until Flint quits")));
    assert!(!named(cx, &t, id, &label("protected, read-only")));

    let server = t.server.clone();
    t.model
        .update(cx, |m, _| m.trust_cluster("kind-alpha".into(), server));
    wait_for_protection(cx, &t, "kind-alpha", Protection::Trusted).await;
    assert!(named(
        cx,
        &t,
        id,
        &label("trusted, unlocked across restarts")
    ));
}

// While writes are paused, an unlocked cluster's lock says so instead of unlocked.
#[gpui_kit::test]
async fn the_lock_button_says_writes_are_paused(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    let server = t.server.clone();
    t.model
        .update(cx, |m, cx| m.unlock("kind-alpha", &server, cx));
    wait_for_protection(cx, &t, "kind-alpha", Protection::Unlocked).await;
    t.model.update(cx, |model, cx| {
        let alpha = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        alpha.provisional = true;
        cx.notify();
    });
    t.wait_for_label(cx, "lock-kind-alpha", "Lock for kind-alpha: writes paused")
        .await;
    let tip = t.description(cx, "lock-kind-alpha").unwrap();
    assert!(tip.starts_with("Writes paused") && !tip.contains("Unlocked"));
}

/// Two EKS contexts in different accounts, both titled `prod`.
const PROD_A: &str = "arn:aws:eks:us-east-1:000000000000:cluster/prod";
const PROD_B: &str = "arn:aws:eks:us-east-1:000000000001:cluster/prod";

fn two_prods(
    dir: &std::path::Path,
    servers: &crate::ui::support_tests::Servers,
) -> Vec<std::path::PathBuf> {
    let kubeconfig = dir.join("kubeconfig");
    let contexts = [
        (PROD_A, servers.silent.as_str()),
        (PROD_B, servers.refused.as_str()),
    ];
    std::fs::write(&kubeconfig, kubeconfig_yaml(&contexts)).unwrap();
    vec![kubeconfig]
}

// Two contexts with one short name and one state still have lock buttons with names of their
// own.
#[gpui_kit::test]
async fn lock_buttons_of_contexts_sharing_a_short_name_differ(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.toml"), PROTECT_LOCAL).unwrap();
    let t = open_workspace_with(cx, dir.path(), two_prods).await;
    assert_eq!(protection(cx, &t, PROD_A), protection(cx, &t, PROD_B));
    let state = "protected, read-only";
    for (context, title) in [(PROD_A, "…0000 · prod"), (PROD_B, "…0001 · prod")] {
        let label = format!("Lock for {title} (context \"{context}\"): {state}");
        assert!(named(cx, &t, &format!("lock-{context}"), &label));
    }
}

// covers UP-55: when Stop Trusting's write fails, the cluster stays protected while its pin is
// still in settings.toml, and both removing the pin again and trusting it again (after the
// confirmation) are offered.
#[gpui_kit::test]
async fn a_failed_stop_trusting_can_be_retried(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    let server = t.server.clone();
    t.model
        .update(cx, |m, _| m.trust_cluster("kind-alpha".into(), server));
    wait_for_protection(cx, &t, "kind-alpha", Protection::Trusted).await;
    // As after a Stop Trusting whose write did not land.
    t.model.update(cx, |m, cx| {
        m.core().unlocks().revoke_trust("kind-alpha");
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);
    assert!(
        t.model
            .read_with(cx, |m, _| !m.settings.trusted_clusters.is_empty())
    );
    let tip = t.description(cx, "lock-kind-alpha").unwrap_or_default();
    assert!(tip.contains("remove that trust"), "{tip}");
    let available = t.available(cx);
    for action in ["workspace::UntrustCluster", "workspace::TrustCluster"] {
        assert!(available.contains(&action), "{action}: {available:?}");
    }
    t.dispatch(
        cx,
        "workspace::TrustCluster",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    wait_for_dialog(cx, &t, true).await;
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    wait_for_protection(cx, &t, "kind-alpha", Protection::Trusted).await;
}

// covers UP-54: an ended shell whose context lost its server, or left kubeconfig, says so on
// its disabled Restart shell, with no Unlock… (there is nothing to pin an unlock to).
#[gpui_kit::test]
async fn a_shell_whose_context_is_gone_offers_no_unlock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), "").await;
    let (_, mut fake) = open_fake_shell(cx, &t);
    fake.set_state(flint_core::ExecState::Exited { code: Some(0) });
    fake.pump();
    cx.run_until_parked();
    assert_eq!(disabled(cx, &t, "terminal-restart"), Some(false));
    let shell = t
        .workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).active_terminal().cloned())
        .unwrap();
    let blocked = |cx: &mut TestAppContext| {
        shell.read_with(cx, |s, _| {
            s.restart_blocked().map(|b| (b.why.clone(), b.unlock))
        })
    };
    let change = |cx: &mut TestAppContext, f: &dyn Fn(&mut Vec<flint_core::ClusterSummary>)| {
        t.model.update(cx, |m, cx| {
            f(&mut m.clusters);
            cx.notify();
        });
        cx.run_until_parked();
    };
    change(cx, &|clusters| {
        clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap()
            .info
            .server = None
    });
    assert_eq!(
        blocked(cx),
        Some((
            "kind-alpha has no API server in kubeconfig".to_owned(),
            false
        ))
    );
    assert_eq!(disabled(cx, &t, "terminal-restart"), Some(true));
    assert_eq!(disabled(cx, &t, "terminal-unlock"), None);
    change(cx, &|clusters| {
        clusters.retain(|c| c.info.name != "kind-alpha")
    });
    assert_eq!(
        blocked(cx),
        Some(("no kubeconfig context named kind-alpha".to_owned(), false))
    );
    assert_eq!(disabled(cx, &t, "terminal-restart"), Some(true));
    assert_eq!(disabled(cx, &t, "terminal-unlock"), None);
}

/// Trusts `kind-alpha` and waits until the core has finished that request.
async fn trust_alpha(cx: &mut TestAppContext, t: &TestWorkspace) {
    let server = t.server.clone();
    t.model
        .update(cx, |m, _| m.trust_cluster("kind-alpha".into(), server));
    wait_for_protection(cx, t, "kind-alpha", Protection::Trusted).await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).trust_settled > 0
    })
    .await;
}

// covers UP-55: after Stop Trusting the cluster shows as protected until the core reports it
// done, however soon the core has finished with it.
#[gpui_kit::test]
async fn stop_trusting_shows_protected_until_the_settings_arrive(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    trust_alpha(cx, &t).await;

    t.model.update(cx, |m, cx| {
        m.untrust_cluster("kind-alpha".into(), cx);
        // As once the core has written the change, before its event reaches this model.
        let unlocks = m.core().unlocks();
        let request = unlocks.revoke_trust("kind-alpha");
        unlocks.trust_removed("kind-alpha", request);
        assert!(
            !unlocks.trust_revoked("kind-alpha"),
            "the core is done with it"
        );
        assert!(!m.settings.trusted_clusters.is_empty(), "the old settings");
        assert_eq!(m.protection_of("kind-alpha"), Protection::Protected);
    });
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).settings.trusted_clusters.is_empty()
    })
    .await;
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Protected);

    // Trusting it again counts as soon as it is written.
    trust_alpha(cx, &t).await;
}

// covers UP-55: settings events can be merged. One that reports the Stop Trusting done ends it,
// even if it already holds a trust made since.
#[gpui_kit::test]
async fn a_merged_settings_event_ends_stop_trusting(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    trust_alpha(cx, &t).await;

    t.model.update(cx, |m, cx| {
        let settings = m.settings.clone();
        m.untrust_cluster("kind-alpha".into(), cx);
        let unlocks = m.core().unlocks();
        let request = unlocks.revoke_trust("kind-alpha");
        unlocks.trust_removed("kind-alpha", request);
        assert_eq!(m.protection_of("kind-alpha"), Protection::Protected);
        m.feed(
            Event::SettingsChanged {
                settings,
                error: None,
                trust_settled: request,
            },
            cx,
        );
        assert_eq!(m.protection_of("kind-alpha"), Protection::Trusted);
    });
}

// covers UP-54: known local clusters start unlocked: no lock button, Shell enabled, and
// nothing to unlock or trust.
#[gpui_kit::test]
async fn known_local_clusters_start_unlocked(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), "").await;
    show_web(cx, &t).await;
    assert_eq!(protection(cx, &t, "kind-alpha"), Protection::Local);
    assert_eq!(disabled(cx, &t, "lock-kind-alpha"), None);
    assert_eq!(disabled(cx, &t, "detail-shell"), Some(false));
    assert_eq!(disabled(cx, &t, "detail-unlock"), None);
    for action in [
        "UnlockCluster",
        "LockCluster",
        "TrustCluster",
        "UntrustCluster",
    ] {
        assert!(
            matches!(
                t.dispatch(
                    cx,
                    &format!("workspace::{action}"),
                    json!({"cluster": "kind-alpha"})
                ),
                Err(ActionError::Rejected { .. })
            ),
            "{action}"
        );
    }
    let row = t.description(cx, "cluster-kind-alpha").unwrap_or_default();
    assert!(row.contains("On this machine: unlocked"), "{row}");
}

// covers UP-33: a dialog closes the namespace picker, which would stay above it, and once the
// dialog is gone the keys work again from the picker's button.
#[gpui_kit::test]
async fn a_dialog_closes_the_namespace_picker(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 0)])
        .await;
    t.click(cx, "namespace-filter-button");
    t.wait_for_element(cx, "ns-all").await;
    draw(cx, &t);
    t.dispatch(
        cx,
        "workspace::UnlockCluster",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    wait_for_dialog(cx, &t, true).await;
    draw(cx, &t);
    assert!(t.find(cx, "ns-all").is_none(), "the picker closed");
    assert!(t.has_dialog(cx), "the dialog stays open");

    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;
    draw(cx, &t);
    assert!(t.find(cx, "ns-all").is_none(), "the picker stays closed");
    let button = t.find(cx, "namespace-filter-button").unwrap();
    assert_eq!(button.focused(), Some(true), "focus is back on the button");
}

// covers UP-33: a dialog that opened with focus in the picker's text field gives it back to the
// button, whichever key dismisses it, also after the dialog's delayed focus restore.
#[gpui_kit::test]
async fn dismissing_a_dialog_does_not_refocus_the_hidden_picker_field(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 0)])
        .await;
    for key in ["escape", "enter"] {
        t.click(cx, "namespace-filter-button");
        t.wait_for_element(cx, "ns-all").await;
        draw(cx, &t);
        t.click(cx, "ns-add");
        draw(cx, &t);
        t.dispatch(cx, "flint::About", Value::Null).unwrap();
        wait_for_dialog(cx, &t, true).await;
        draw(cx, &t);
        assert!(t.find(cx, "ns-all").is_none(), "{key}: the picker closed");

        t.press(cx, key);
        wait_for_dialog(cx, &t, false).await;
        // Past the dialog's closing delay, when it restores the focus it saved.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        draw(cx, &t);
        draw(cx, &t);
        assert!(t.find(cx, "ns-all").is_none(), "{key}: stays closed");
        let button = t.find(cx, "namespace-filter-button").unwrap();
        assert_eq!(
            button.focused(),
            Some(true),
            "{key}: focus is on the button"
        );
    }
}

const SERVER_WITH_USERINFO: &str = "https://admin:hunter2@prod.example:6443/?token=abc";
const CLEANED_SERVER: &str = "https://prod.example:6443";

fn assert_cleaned(text: &str) {
    for part in ["admin", "hunter2", "token=", "@"] {
        assert!(!text.contains(part), "{part} in {text}");
    }
}

#[test]
fn dialogs_show_the_cleaned_server_address() {
    let previous = "https://old:pass@old.example:6443";
    let texts = [
        unlock_dialog("kind-alpha", "kind-alpha", SERVER_WITH_USERINFO).text,
        trust_dialog(
            "kind-alpha",
            "kind-alpha",
            SERVER_WITH_USERINFO,
            Some(previous),
        )
        .text,
        scope_dialog("kind-alpha", SERVER_WITH_USERINFO, &Default::default()).text,
        expose_dialog("kind-alpha", SERVER_WITH_USERINFO, Some(previous)).text,
    ];
    for text in &texts {
        assert_cleaned(text);
        assert!(
            text.contains(&format!("Server: {CLEANED_SERVER}")),
            "{text}"
        );
    }
    assert!(texts[1].contains("trusted at https://old.example:6443,"));
    assert!(!texts[1].contains("pass@"));
}

#[test]
fn a_previous_server_that_shows_the_same_is_still_called_different() {
    let previous = "https://other:login@prod.example:6443";
    let text = trust_dialog(
        "kind-alpha",
        "kind-alpha",
        SERVER_WITH_USERINFO,
        Some(previous),
    )
    .text;
    assert_cleaned(&text);
    assert!(!text.contains("other:login"), "{text}");
    assert!(
        text.contains("trusted at another URL that shows the same here"),
        "{text}"
    );
}

#[gpui_kit::test]
async fn the_row_is_cleaned_while_pins_keep_the_exact_url(cx: &mut TestAppContext) {
    // On this machine, at a port nothing listens on: the test never reaches another host.
    const RAW: &str = "https://admin:hunter2@127.0.0.1:1/?token=abc";
    const CLEANED: &str = "https://127.0.0.1:1";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.toml"), PROTECT_LOCAL).unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let kubeconfig = dir.join("kubeconfig");
        let contexts = [("kind-alpha", RAW), ("kind-beta", servers.refused.as_str())];
        std::fs::write(&kubeconfig, kubeconfig_yaml(&contexts)).unwrap();
        vec![kubeconfig]
    })
    .await;
    draw(cx, &t);

    let row = t.description(cx, "cluster-kind-alpha").unwrap();
    assert_cleaned(&row);
    assert!(
        row.starts_with(&format!("kind-alpha\n{CLEANED}\n")),
        "{row}"
    );
    assert_eq!(
        t.label(cx, "cluster-server-kind-alpha").as_deref(),
        Some("127.0.0.1:1")
    );

    t.dispatch(
        cx,
        "workspace::UnlockCluster",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    wait_for_dialog(cx, &t, true).await;
    assert_cleaned(&t.label(cx, "unlock-description").unwrap());
    t.click(cx, "ok");
    wait_for_protection(cx, &t, "kind-alpha", Protection::Unlocked).await;
    wait_for_dialog(cx, &t, false).await;
    t.model.read_with(cx, |m, _| {
        let unlocks = m.core().unlocks();
        assert!(unlocks.is_unlocked("kind-alpha", RAW));
        assert!(!unlocks.is_unlocked("kind-alpha", CLEANED));
    });

    t.dispatch(
        cx,
        "workspace::TrustCluster",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    wait_for_dialog(cx, &t, true).await;
    assert_cleaned(&t.label(cx, "trust-description").unwrap());
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    wait_for_protection(cx, &t, "kind-alpha", Protection::Trusted).await;
    assert!(t.settings_toml().contains(RAW));
}
