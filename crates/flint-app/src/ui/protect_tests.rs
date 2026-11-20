//! View tests for protected clusters (ADR-0015): Shell is disabled on a protected cluster,
//! saying why, with the unlock in place; an unlock asks first, naming the server, and lasts
//! until Lock Again; trust is kept in `settings.toml`, pinned to the server. The test servers
//! are on this machine, so the tests turn on `protect_local_clusters` to see a protected one.

use flint_core::settings::TrustState;
use flint_core::{ObjectDetail, Protection};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::super::{trust_dialog, unlock_dialog};
use crate::ui::dock::{DockItem as _, TabStatus};
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{
    TestWorkspace, WAIT, alpha_and_beta, open_fake_shell, open_workspace_with,
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

// covers UP-54, UP-55: on a protected cluster Shell is disabled, saying why, and Unlock sits
// beside it; Logs still work. Unlocking asks first, naming the cluster and its server; Esc
// keeps it protected. Once unlocked, Shell opens; Lock Again protects it again, and Restart
// Shell is refused, saying why.
#[gpui_kit::test]
async fn a_protected_cluster_disables_shell_until_unlocked(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), PROTECT_LOCAL).await;
    show_web(cx, &t).await;
    assert_eq!(protection(cx, &t, "alpha"), Protection::Protected);
    let why = "alpha is protected (read-only in Flint) until you unlock it";
    assert_eq!(disabled(cx, &t, "detail-shell"), Some(true));
    assert_eq!(disabled(cx, &t, "detail-logs"), Some(false));
    assert_eq!(disabled(cx, &t, "detail-unlock"), Some(false));
    assert_eq!(captions(cx, &t), [format!("Can't open a shell: {why}.")]);
    assert_eq!(
        t.dispatch(cx, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into()))
    );
    assert!(
        t.description(cx, "lock-alpha")
            .unwrap_or_default()
            .starts_with("Protected")
    );
    // Locking a protected cluster does nothing.
    assert!(matches!(
        t.dispatch(cx, "workspace::LockCluster", json!({"cluster": "alpha"})),
        Err(ActionError::Rejected { .. })
    ));

    t.click(cx, "detail-unlock");
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(
        t.label(cx, "unlock-description"),
        Some(unlock_dialog("alpha", &t.server).text)
    );
    assert_eq!(t.label(cx, "ok").as_deref(), Some("Unlock"));
    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;
    assert_eq!(protection(cx, &t, "alpha"), Protection::Protected);
    // Enter cancels too: only the Unlock button unlocks.
    t.click(cx, "detail-unlock");
    wait_for_dialog(cx, &t, true).await;
    t.press(cx, "enter");
    wait_for_dialog(cx, &t, false).await;
    assert_eq!(protection(cx, &t, "alpha"), Protection::Protected);

    t.click(cx, "detail-unlock");
    wait_for_dialog(cx, &t, true).await;
    t.click(cx, "ok");
    wait_for_protection(cx, &t, "alpha", Protection::Unlocked).await;
    assert!(
        t.model.read_with(cx, |m, _| m
            .core()
            .unlocks()
            .is_unlocked("alpha", &t.server)),
        "the core's gate sees the unlock"
    );
    assert_eq!(disabled(cx, &t, "detail-shell"), Some(false));
    assert_eq!(disabled(cx, &t, "detail-unlock"), None);
    assert!(captions(cx, &t).is_empty());
    assert!(
        t.description(cx, "lock-alpha")
            .unwrap_or_default()
            .starts_with("Unlocked")
    );
    // Only alpha: beta is still protected, and the unlock is not written anywhere.
    assert_eq!(protection(cx, &t, "beta"), Protection::Protected);
    assert!(!t.settings_toml().contains("alpha"));

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

    t.dispatch(cx, "workspace::LockCluster", json!({"cluster": "alpha"}))
        .unwrap();
    wait_for_protection(cx, &t, "alpha", Protection::Protected).await;
    assert!(!t.model.read_with(cx, |m, _| {
        m.core().unlocks().is_unlocked("alpha", &t.server)
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
    wait_for_protection(cx, &t, "alpha", Protection::Unlocked).await;
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
            "{PROTECT_LOCAL}trusted_clusters = [{{ context = \"beta\", server = \"{old}\" }}]\n"
        ),
    )
    .await;
    assert_eq!(protection(cx, &t, "alpha"), Protection::Protected);
    assert_eq!(protection(cx, &t, "beta"), Protection::Protected);
    let beta_trust = t.model.read_with(cx, |m, _| {
        m.trust(m.clusters.iter().find(|c| c.info.name == "beta").unwrap())
    });
    assert_eq!(
        beta_trust,
        TrustState::ServerChanged {
            trusted_server: old.into()
        }
    );
    let row = t.description(cx, "cluster-beta").unwrap_or_default();
    assert!(
        row.contains(&format!("Cluster changed: trusted at {old}")),
        "{row}"
    );

    t.dispatch(cx, "workspace::TrustCluster", json!({"cluster": "alpha"}))
        .unwrap();
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(
        t.label(cx, "trust-description"),
        Some(trust_dialog("alpha", &t.server, None).text)
    );
    assert_eq!(protection(cx, &t, "alpha"), Protection::Protected);
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    wait_for_protection(cx, &t, "alpha", Protection::Trusted).await;
    let toml = t.settings_toml();
    assert!(
        toml.contains("trusted_clusters") && toml.contains(&t.server),
        "{toml}"
    );
    assert!(
        t.description(cx, "lock-alpha")
            .unwrap_or_default()
            .starts_with("Trusted")
    );
    assert!(matches!(
        t.dispatch(cx, "workspace::TrustCluster", json!({"cluster": "alpha"})),
        Err(ActionError::Rejected { .. })
    ));

    // Beta, trusted at its old server: trusting again names both.
    t.dispatch(cx, "workspace::TrustCluster", json!({"cluster": "beta"}))
        .unwrap();
    wait_for_dialog(cx, &t, true).await;
    assert_eq!(
        t.label(cx, "trust-description"),
        Some(trust_dialog("beta", &t.refused_server, Some(old)).text)
    );
    t.press(cx, "escape");
    wait_for_dialog(cx, &t, false).await;

    // Stop Trusting protects it at once, through the gate the core shares, before the new
    // settings come back: the settings the app holds still trust it.
    let (trusted_still, now) = t.model.update(cx, |m, cx| {
        m.untrust_cluster("alpha".into(), cx);
        (
            m.settings
                .trusted_clusters
                .iter()
                .any(|p| p.context == "alpha"),
            m.protection_of("alpha"),
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
            .all(|p| p.context != "alpha")
    })
    .await;
    assert_eq!(protection(cx, &t, "alpha"), Protection::Protected);
    t.dispatch(cx, "workspace::UntrustCluster", json!({"cluster": "beta"}))
        .unwrap();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).settings.trusted_clusters.is_empty()
    })
    .await;
    assert!(matches!(
        t.dispatch(cx, "workspace::UntrustCluster", json!({"cluster": "alpha"})),
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
            .find(|c| c.info.name == "alpha")
            .unwrap()
            .info
            .server = None
    });
    assert_eq!(
        blocked(cx),
        Some(("alpha has no API server in kubeconfig".to_owned(), false))
    );
    assert_eq!(disabled(cx, &t, "terminal-restart"), Some(true));
    assert_eq!(disabled(cx, &t, "terminal-unlock"), None);
    change(cx, &|clusters| clusters.retain(|c| c.info.name != "alpha"));
    assert_eq!(
        blocked(cx),
        Some(("no kubeconfig context named alpha".to_owned(), false))
    );
    assert_eq!(disabled(cx, &t, "terminal-restart"), Some(true));
    assert_eq!(disabled(cx, &t, "terminal-unlock"), None);
}

// covers UP-54: clusters on this machine start unlocked: no lock button, Shell enabled, and
// nothing to unlock or trust.
#[gpui_kit::test]
async fn clusters_on_this_machine_start_unlocked(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_settings(cx, dir.path(), "").await;
    show_web(cx, &t).await;
    assert_eq!(protection(cx, &t, "alpha"), Protection::Local);
    assert_eq!(disabled(cx, &t, "lock-alpha"), None);
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
                    json!({"cluster": "alpha"})
                ),
                Err(ActionError::Rejected { .. })
            ),
            "{action}"
        );
    }
    let row = t.description(cx, "cluster-alpha").unwrap_or_default();
    assert!(row.contains("On this machine: unlocked"), "{row}");
}
