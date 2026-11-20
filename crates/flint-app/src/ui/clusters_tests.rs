//! View tests for the cluster sidebar: the contexts it lists, the empty and error states, and
//! which of a row's controls are enabled. The sidebar is drawn by the workspace from
//! [`ClustersModel`].

use std::sync::Arc;

use flint_core::settings::AiExposureState;
use flint_core::{ConnectionState, Event, Settings};
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{Context, TestAppContext};
use serde_json::json;

use super::ClustersModel;
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{WAIT, kubeconfig_yaml, open_workspace, open_workspace_with};

/// Test hook: feeds an event in as the core's event stream does (`support_tests.rs`).
impl ClustersModel {
    pub(in crate::ui) fn feed(&mut self, event: Event, cx: &mut Context<Self>) {
        self.apply(event, cx);
    }
}

const EKS: &str = "arn:aws:eks:eu-west-1:123456789012:cluster/payments-prod";

// covers UP-01: every context once, across `KUBECONFIG`-style files and a path from settings,
// the first definition winning. An EKS ARN shows its short name, with the full context in the
// tooltip. Nothing connects until a row is clicked.
#[gpui_kit::test]
async fn the_sidebar_lists_every_context_once_without_connecting(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let extra = dir.path().join("extra.yaml");
    std::fs::write(
        dir.path().join("settings.toml"),
        format!("kubeconfig_paths = [{:?}]\n", extra.display().to_string()),
    )
    .unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let server = servers.silent.as_str();
        let first = dir.join("first.yaml");
        let second = dir.join("second.yaml");
        std::fs::write(
            &first,
            kubeconfig_yaml(&[("alpha", server), (EKS, "https://127.0.0.1:9")]),
        )
        .unwrap();
        // `alpha` again, on another server: the first file's wins.
        std::fs::write(
            &second,
            kubeconfig_yaml(&[("alpha", "https://elsewhere.invalid"), ("gamma", server)]),
        )
        .unwrap();
        std::fs::write(&extra, kubeconfig_yaml(&[("delta", server)])).unwrap();
        vec![first, second]
    })
    .await;

    let clusters: Vec<(String, Option<String>, ConnectionState)> = t.model.read_with(cx, |m, _| {
        m.clusters
            .iter()
            .map(|c| (c.info.name.clone(), c.info.server.clone(), c.state.clone()))
            .collect()
    });
    let names: Vec<&str> = clusters.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, ["alpha", EKS, "gamma", "delta"]);
    assert_eq!(clusters[0].1.as_deref(), Some(t.server.as_str()));
    assert!(
        clusters
            .iter()
            .all(|(_, _, s)| *s == ConnectionState::Disconnected)
    );

    // One row per context, in that order, each with its own AI toggle.
    let rows = t.ids_with_prefix(cx, "expose-");
    let expected: Vec<String> = names.iter().map(|n| format!("expose-{n}")).collect();
    assert_eq!(rows, expected);

    // The ARN's row: its short name, and the full context, server and state in the tooltip.
    let row = t.find(cx, format!("cluster-{EKS}")).unwrap();
    assert_eq!(row.label(), Some("cluster/payments-prod"));
    let tip = row.description().unwrap();
    assert!(
        tip.starts_with(&format!("{EKS}\nhttps://127.0.0.1:9\nNot connected\n")),
        "{tip}"
    );
    assert_eq!(t.label(cx, "cluster-alpha").as_deref(), Some("alpha"));

    // Nothing is selected, the navigation is hidden, and no request reached a server.
    assert_eq!(t.model.read_with(cx, |m, _| m.active.clone()), None);
    assert!(t.find(cx, "nav-edge").is_none());
    assert!(!t.available(cx).contains(&"workspace::OpenView"));
    assert!(!t.server_was_contacted());

    // A click on a row selects that cluster.
    t.click(cx, "cluster-gamma");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).shown_cluster() == Some("gamma")
    })
    .await;
    assert!(t.find(cx, "nav-edge").is_some());
}

// covers UP-02: no usable kubeconfig. The sidebar's empty state names the paths searched (and
// which are missing) and `KUBECONFIG`; a broken file is reported by name, with the path and the
// error's first line in its tooltip, next to the contexts of the valid ones. Nothing crashes.
#[gpui_kit::test]
async fn no_usable_kubeconfig_shows_what_was_searched(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.yaml");
    let missing_path = missing.clone();
    let base = vec![missing.clone()];
    let t = open_workspace_with(cx, dir.path(), move |_, _| base).await;
    let (clusters, paths, errors) = t.model.read_with(cx, |m, _| {
        (
            m.clusters.len(),
            m.kubeconfig_paths.clone(),
            m.kubeconfig_errors.len(),
        )
    });
    assert_eq!(clusters, 0);
    assert_eq!(paths, [(missing, false)]);
    assert_eq!(errors, 0);
    assert!(t.ids_with_prefix(cx, "expose-").is_empty());
    assert!(t.find(cx, "kubeconfig-empty").is_some());
    let env = match std::env::var_os("KUBECONFIG") {
        Some(v) if !v.is_empty() => v.to_string_lossy().into_owned(),
        Some(_) => "(set but empty)".to_owned(),
        None => "(not set)".to_owned(),
    };
    assert_eq!(
        t.label(cx, "kubeconfig-env"),
        Some(format!("KUBECONFIG: {env}"))
    );
    assert_eq!(
        t.label(cx, ("searched-path", 0usize)),
        Some(format!("{} (missing)", missing_path.display()))
    );
    assert!(matches!(
        t.dispatch(cx, "workspace::SelectCluster", json!({"cluster": "alpha"})),
        Err(ActionError::Rejected { .. })
    ));

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let server = servers.silent.as_str();
        let broken = dir.join("broken.yaml");
        let good = dir.join("good.yaml");
        std::fs::write(&broken, "apiVersion: v1\nclusters: [\n").unwrap();
        std::fs::write(&good, kubeconfig_yaml(&[("good", server)])).unwrap();
        vec![broken, good]
    })
    .await;
    let (names, errors) = t.model.read_with(cx, |m, _| {
        (
            m.clusters
                .iter()
                .map(|c| c.info.name.clone())
                .collect::<Vec<_>>(),
            m.kubeconfig_errors.clone(),
        )
    });
    assert_eq!(names, ["good"]);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].0.ends_with("broken.yaml"), "{errors:?}");
    assert_eq!(t.ids_with_prefix(cx, "expose-"), ["expose-good"]);
    assert!(t.find(cx, "kubeconfig-empty").is_none());
    let error = t.find(cx, "kubeconfig-error-0").unwrap();
    assert!(
        error.label().unwrap().starts_with("broken.yaml: "),
        "{error:?}"
    );
    let tip = error.description().unwrap();
    assert!(tip.starts_with(&errors[0].0.display().to_string()), "{tip}");
}

// covers UP-04: a context whose connection failed can still be exposed: its AI toggle stays
// enabled.
#[gpui_kit::test]
async fn a_failed_connection_leaves_the_toggles_enabled(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.activate(cx, "beta").await;
    t.wait_for_state(cx, "beta", |s| matches!(s, ConnectionState::Failed(_)))
        .await;
    let toggle = t.find(cx, "expose-beta").unwrap();
    assert_ne!(toggle.disabled(), Some(true));
    assert!(
        toggle
            .description()
            .unwrap()
            .starts_with("Hidden from AI agents")
    );
    // Nothing is watched on a failed cluster, so the header has no watch mode.
    assert!(t.find(cx, "watch-mode").is_none());
    assert!(t.available(cx).contains(&"workspace::ExposeToAi"));
    assert!(t.find(cx, "settings-error").is_none());
}

// covers UP-12: a malformed settings.toml shows a banner at the top of the sidebar, exposes
// nothing, and disables every AI toggle and the watch mode, whose actions are refused. Once the
// file is fixed, they work again.
#[gpui_kit::test]
async fn a_malformed_settings_file_shows_a_banner_and_disables_the_toggles(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "ai_exposed_clusters = [{ context = \"alpha\" \n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.wait_for_element(cx, "settings-error").await;
    let banner = t.find(cx, "settings-error").unwrap();
    assert_eq!(
        banner.label(),
        Some("settings.toml is invalid. Nothing is exposed to AI until it is fixed.")
    );
    assert!(!banner.description().unwrap_or_default().is_empty());
    t.activate(cx, "alpha").await;
    for id in ["expose-alpha", "expose-beta", "watch-mode"] {
        assert_eq!(t.find(cx, id).unwrap().disabled(), Some(true), "{id}");
    }
    let available = t.available(cx);
    assert!(
        !available.contains(&"workspace::ExposeToAi"),
        "{available:?}"
    );
    assert!(!available.contains(&"workspace::WatchAllKinds"));
    assert!(available.contains(&"workspace::SelectCluster"));
    let exposure = t.model.read_with(cx, |m, _| {
        let alpha = m.clusters.iter().find(|c| c.info.name == "alpha").unwrap();
        m.ai_exposure(alpha)
    });
    assert_eq!(exposure, AiExposureState::Hidden);

    // Fixed on disk, as the core reports it: the banner goes and the toggles work again.
    t.feed(
        cx,
        Event::SettingsChanged {
            settings: Arc::new(Settings::default()),
            error: None,
        },
    );
    assert!(t.find(cx, "settings-error").is_none());
    assert_ne!(t.find(cx, "expose-beta").unwrap().disabled(), Some(true));
}
