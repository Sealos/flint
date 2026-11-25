//! View tests for the cluster sidebar: the contexts it lists, the empty and error states, and
//! which of a row's controls are enabled. The sidebar is drawn by the workspace from
//! [`ClustersModel`].

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use flint_core::settings::AiExposureState;
use flint_core::{ConnectionState, Event, KindId, Settings, SyncState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, Context, TestAppContext, WeakEntity, point, px};
use serde_json::json;

use super::ClustersModel;
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{
    WAIT, kubeconfig_yaml, open_workspace, open_workspace_with, standard_sections,
};

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
            kubeconfig_yaml(&[("kind-alpha", server), (EKS, "https://127.0.0.1:9")]),
        )
        .unwrap();
        // `kind-alpha` again, on another server: the first file's wins.
        std::fs::write(
            &second,
            kubeconfig_yaml(&[
                ("kind-alpha", "https://elsewhere.invalid"),
                ("gamma", server),
            ]),
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
    assert_eq!(names, ["kind-alpha", EKS, "gamma", "delta"]);
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
    assert_eq!(row.label(), Some("payments-prod"));
    let tip = row.description().unwrap();
    assert!(
        tip.starts_with(&format!("{EKS}\nhttps://127.0.0.1:9\nNot connected\n")),
        "{tip}"
    );
    assert_eq!(
        t.label(cx, "cluster-kind-alpha").as_deref(),
        Some("kind-alpha")
    );

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
        t.dispatch(
            cx,
            "workspace::SelectCluster",
            json!({"cluster": "kind-alpha"})
        ),
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
    t.activate(cx, "kind-beta").await;
    t.wait_for_state(cx, "kind-beta", |s| matches!(s, ConnectionState::Failed(_)))
        .await;
    let toggle = t.find(cx, "expose-kind-beta").unwrap();
    assert_ne!(toggle.disabled(), Some(true));
    assert!(
        toggle
            .description()
            .unwrap()
            .starts_with("Hidden from AI agents")
    );
    // Nothing is watched on a failed cluster, so the header's watch mode is off.
    assert_eq!(t.find(cx, "watch-mode").unwrap().disabled(), Some(true));
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
        "ai_exposed_clusters = [{ context = \"kind-alpha\" \n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.wait_for_element(cx, "settings-error").await;
    let banner = t.find(cx, "settings-error").unwrap();
    assert_eq!(
        banner.label(),
        Some(
            "settings.toml is invalid. Until it is fixed, nothing is exposed to AI or trusted, and clusters on this machine are protected too."
        )
    );
    assert!(!banner.description().unwrap_or_default().is_empty());
    t.activate(cx, "kind-alpha").await;
    for id in ["expose-kind-alpha", "expose-kind-beta", "watch-mode"] {
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
        let alpha = m
            .clusters
            .iter()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        m.ai_exposure(alpha)
    });
    assert_eq!(exposure, AiExposureState::Hidden);

    // Fixed on disk, as the core reports it: the banner goes and the toggles work again.
    t.feed(
        cx,
        Event::SettingsChanged {
            settings: Arc::new(Settings::default()),
            error: None,
            trust_settled: 0,
        },
    );
    assert!(t.find(cx, "settings-error").is_none());
    assert_ne!(
        t.find(cx, "expose-kind-beta").unwrap().disabled(),
        Some(true)
    );
}

// An open tooltip follows the row: with the pointer resting on it through an outage and the
// recovery, it says what the row's dot says, and a routine relist does not read as trouble.
#[gpui_kit::test]
async fn an_open_tooltip_follows_the_clusters_presence(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    cx.update_window(t.window, |_, window, cx| {
        let row = window.find("cluster-kind-alpha").bounds();
        window.simulate_mouse_move(row.center(), cx);
    })
    .unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    t.wait_for_element(cx, "cluster-tooltip").await;

    let nav = t.workspace.read_with(cx, |w, _| w.nav_view());
    let mut shows = |state: SyncState, wanted: &str| {
        nav.update(cx, |nav, cx| {
            nav.set_states(vec![(KindId::pods(), state)], cx);
        });
        cx.run_until_parked();
        let tip = t.label(cx, "cluster-tooltip").unwrap();
        assert!(tip.contains(wanted), "{wanted:?} in {tip}");
        let row = t.description(cx, "cluster-kind-alpha").unwrap();
        assert_eq!(tip, row, "the tooltip and the row's description agree");
    };
    let trouble = SyncState::Reconnecting {
        error: Some("timed out".into()),
    };
    shows(trouble, "\nReconnecting: timed out\n");
    shows(SyncState::Synced, "\nConnected, Kubernetes v1.33.1\n");
    shows(
        SyncState::Reconnecting { error: None },
        "\nConnected, Kubernetes v1.33.1\n",
    );
}

// Dismissing a tooltip frees its view, and freeing the view unregisters its observers while the
// model and the watch states stay quiet, so repeated hovers leave nothing behind.
#[gpui_kit::test]
async fn a_dismissed_tooltip_stops_observing_the_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();

    let opened = Rc::new(RefCell::new(Vec::<WeakEntity<Tooltip>>::new()));
    let _opened = cx.update(|cx| {
        let opened = opened.clone();
        cx.observe_new(move |_: &mut Tooltip, _, cx| opened.borrow_mut().push(cx.weak_entity()))
    });
    cx.update_window(t.window, |_, window, cx| {
        let row = window.find("cluster-kind-alpha").bounds();
        window.simulate_mouse_move(row.center(), cx);
    })
    .unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    t.wait_for_element(cx, "cluster-tooltip").await;
    cx.update_window(t.window, |_, window, cx| {
        window.simulate_mouse_move(point(px(5000.), px(5000.)), cx);
    })
    .unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    // The window keeps the last two frames' views, so draw past both.
    for _ in 0..2 {
        cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }
    assert!(!opened.borrow().is_empty(), "the hover opened a tooltip");
    for tooltip in opened.borrow().iter() {
        tooltip.assert_released();
    }

    let nav = t.workspace.read_with(cx, |w, _| w.nav_view());
    let callback = Rc::new(());
    let view = cx.update(|cx| {
        let held = callback.clone();
        super::following_tooltip(
            "kind-alpha".into(),
            &t.model,
            &nav,
            move |cx| {
                let _ = &held;
                cx.notify();
            },
            cx,
        )
    });
    assert!(Rc::strong_count(&callback) > 1, "the tooltip is observing");
    // Inside an update: GPUI releases a dropped entity when the effect cycle ends.
    cx.update(|_| drop(view));
    cx.run_until_parked();
    assert_eq!(
        Rc::strong_count(&callback),
        1,
        "dismissal unregisters the observers before the next notification"
    );
}
