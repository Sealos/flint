//! The workspace's registered actions ([`crate::ui::registry`]): the sidebar, the header and
//! the main area. Each handler calls what the matching click calls. The workspace also
//! forwards the list's, the detail pane's and the navigation's actions to them, so those work
//! wherever focus is in the window.

use flint_core::settings::AiExposureState;
use flint_core::{NavEntry, WatchMode};
use gpui_kit::*;

use super::super::detail::DetailView;
use super::super::list::ResourceList;
use super::super::nav::NavPanel;
use super::super::registry::{
    Bind, ExposeToAi, Forward, HideFromAi, OnView, OpenView, PaneName, RetryConnection,
    SelectCluster, SetNamespaces, SetPaneWidth, SetWatchMode, ShowAllNamespaces, ToggleNamespace,
    WatchModeChoice, WatchScope,
};
use super::super::startup;
use super::super::ui_state;
use super::{Pane, Workspace, confirm_expose};

impl Workspace {
    /// Every workspace action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<SelectCluster>(Self::select_cluster)
            .bind::<ExposeToAi>(Self::expose_to_ai)
            .bind::<HideFromAi>(Self::hide_from_ai)
            .bind::<RetryConnection>(|this, _, _, cx| this.retry(cx))
            .bind::<OpenView>(Self::open_view)
            .bind::<SetNamespaces>(|this, action, _, cx| {
                this.set_namespace_filter(ui_state::normalize(action.namespaces.clone()), cx)
            })
            .bind::<ToggleNamespace>(Self::toggle_namespace)
            .bind::<ShowAllNamespaces>(|this, _, _, cx| this.set_namespace_filter(Vec::new(), cx))
            .bind::<SetWatchMode>(Self::set_watch_mode)
            .bind::<SetPaneWidth>(Self::set_pane_width)
    }

    /// Registers the workspace's actions on its root element, and forwards the open views'.
    pub(super) fn register_actions<E: InteractiveElement>(
        &self,
        element: E,
        cx: &Context<Self>,
    ) -> E {
        let element = Self::bind_actions(OnView::new(element, cx)).into_element();
        let element =
            ResourceList::bind_actions(Forward::new(element, self.list.clone())).into_element();
        let element =
            DetailView::bind_actions(Forward::new(element, self.detail.clone())).into_element();
        NavPanel::bind_actions(Forward::new(element, Some(self.nav.clone()))).into_element()
    }

    fn select_cluster(&mut self, action: &SelectCluster, _: &mut Window, cx: &mut Context<Self>) {
        let known = self
            .model
            .read(cx)
            .clusters
            .iter()
            .any(|c| c.info.name == action.cluster);
        if known {
            let cluster = action.cluster.clone();
            self.model.update(cx, |m, _| m.activate(cluster));
        } else {
            tracing::warn!(cluster = action.cluster, "SelectCluster: no such context");
        }
    }

    /// Opens the confirmation, as the sidebar's Expose to AI button (or its "Expose at the new
    /// server…" item) does.
    fn expose_to_ai(&mut self, action: &ExposeToAi, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        if model.settings_error.is_some() {
            tracing::warn!("ExposeToAi: settings.toml is invalid");
            return;
        }
        let Some(cluster) = model
            .clusters
            .iter()
            .find(|c| c.info.name == action.cluster)
        else {
            tracing::warn!(cluster = action.cluster, "ExposeToAi: no such context");
            return;
        };
        let Some(server) = cluster.info.server.clone() else {
            tracing::warn!(
                cluster = action.cluster,
                "ExposeToAi: the context has no server"
            );
            return;
        };
        let previous = match model.ai_exposure(cluster) {
            AiExposureState::Exposed => return,
            AiExposureState::Hidden => None,
            AiExposureState::ServerChanged { exposed_server } => Some(exposed_server),
        };
        let model = self.model.clone();
        confirm_expose(window, cx, model, action.cluster.clone(), server, previous);
    }

    fn hide_from_ai(&mut self, action: &HideFromAi, _: &mut Window, cx: &mut Context<Self>) {
        if self.model.read(cx).settings_error.is_some() {
            tracing::warn!("HideFromAi: settings.toml is invalid");
            return;
        }
        let cluster = action.cluster.clone();
        self.model.update(cx, |m, _| m.hide_from_ai(cluster));
    }

    /// Opens a kind as clicking it in the navigation does. `kind` matches like `FLINT_VIEW`.
    fn open_view(&mut self, action: &OpenView, window: &mut Window, cx: &mut Context<Self>) {
        match self.nav_entry(&action.kind, cx) {
            Some(entry) => self.open_kind(&entry, window, cx),
            None => tracing::warn!(
                kind = action.kind,
                "OpenView: the cluster does not serve it"
            ),
        }
    }

    /// The active cluster's navigation entry for `kind`: an exact id, or a plural without its
    /// group.
    pub(super) fn nav_entry(&self, kind: &str, cx: &App) -> Option<NavEntry> {
        let model = self.model.read(cx);
        let active = model.active.as_ref()?;
        let discovery = model.discovery.get(active)?.discovery.as_ref()?;
        let entries = || {
            discovery
                .navigation
                .sections
                .iter()
                .flat_map(|s| s.entries.iter())
        };
        entries()
            .find(|e| e.id.as_str() == kind)
            .or_else(|| entries().find(|e| startup::kind_matches(e.id.as_str(), kind)))
            .cloned()
    }

    fn toggle_namespace(
        &mut self,
        action: &ToggleNamespace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cluster) = self.cluster.clone() else {
            return;
        };
        let namespace = action.namespace.trim();
        if namespace.is_empty() {
            return;
        }
        let current = self.ui_state.namespace_filter(&cluster).to_vec();
        let next = ui_state::toggle_namespace(&current, namespace);
        self.set_namespace_filter(next, cx);
    }

    /// Picks a watch mode, as the header's watch-mode menu does.
    fn set_watch_mode(&mut self, action: &SetWatchMode, _: &mut Window, cx: &mut Context<Self>) {
        if self.model.read(cx).settings_error.is_some() {
            tracing::warn!("SetWatchMode: settings.toml is invalid");
            return;
        }
        let Some((target, mode)) = watch_mode_change(action, self.cluster.as_deref()) else {
            tracing::warn!(?action, "SetWatchMode: not a choice the menu offers");
            return;
        };
        self.model.update(cx, |m, _| m.set_watch_mode(target, mode));
    }

    /// Sets a pane's width, as dragging its edge does, and saves it.
    fn set_pane_width(&mut self, action: &SetPaneWidth, _: &mut Window, cx: &mut Context<Self>) {
        let pane = match action.pane {
            PaneName::Clusters => Pane::Clusters,
            PaneName::Navigation => Pane::Navigation,
            PaneName::Detail => Pane::Detail,
        };
        let before = self.panes;
        pane.set(&mut self.panes, action.width);
        if self.panes != before {
            let save = self.ui_state.set_panes(self.panes);
            self.persist(save, cx);
            cx.notify();
        }
    }
}

/// The settings change a watch-mode choice makes: the cluster (`None`: the default) and the
/// mode (`None`: follow the default). `None` for a choice the menu does not offer.
fn watch_mode_change(
    action: &SetWatchMode,
    active: Option<&str>,
) -> Option<(Option<String>, Option<WatchMode>)> {
    let mode = match action.mode {
        WatchModeChoice::UseDefault => None,
        WatchModeChoice::All => Some(WatchMode::All),
        WatchModeChoice::OnView => Some(WatchMode::OnView),
    };
    match action.scope {
        WatchScope::Cluster => Some((Some(active?.to_owned()), mode)),
        WatchScope::Default => Some((None, Some(mode?))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use flint_core::{CoreConfig, CoreHandle, Settings, WatchMode};
    use gpui_kit::test::TestAppContextExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use serde_json::json;

    use super::watch_mode_change;
    use crate::ui::clusters::ClustersModel;
    use crate::ui::registry::{
        self, ActionError, Scope, SetWatchMode, WatchModeChoice as Choice, WatchScope,
        tests::assert_binds_scope,
    };
    use crate::ui::ui_state::UiState;
    use crate::ui::workspace::Workspace;

    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn every_workspace_action_is_bound() {
        assert_binds_scope(Scope::Workspace, super::Workspace::bind_actions);
    }

    /// A kubeconfig with two contexts whose servers refuse connections (nothing is reached).
    const KUBECONFIG: &str = "apiVersion: v1
kind: Config
clusters:
- name: c
  cluster: {server: \"https://127.0.0.1:9\"}
users:
- name: u
  user: {token: t}
contexts:
- name: alpha
  context: {cluster: c, user: u}
- name: beta
  context: {cluster: c, user: u}
";

    /// The main window over a core that reads [`KUBECONFIG`], once the contexts are loaded.
    async fn open_workspace(
        cx: &mut TestAppContext,
        dir: &std::path::Path,
    ) -> (AnyWindowHandle, Entity<ClustersModel>) {
        cx.executor().allow_parking();
        let kubeconfig = dir.join("kubeconfig");
        std::fs::write(&kubeconfig, KUBECONFIG).unwrap();
        let core = CoreHandle::start(CoreConfig {
            config_dir: dir.to_owned(),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: vec![kubeconfig],
            home: None,
            watch_files: false,
        })
        .unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            registry::init(cx);
        });
        let ui_state = UiState::load(dir.to_owned());
        let (window, model) = cx.update(|cx| {
            let model = cx.new(|cx| ClustersModel::new(core, Settings::default(), None, cx));
            let workspace_model = model.clone();
            let (window, _) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1100.), px(700.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| Workspace::new(workspace_model, ui_state, None, None, window, cx))
                },
            )
            .unwrap();
            (window, model)
        });
        let loaded = model.clone();
        cx.wait_for(window, WAIT, move |_, cx| {
            loaded.read(cx).clusters.len() == 2
        })
        .await;
        (window, model)
    }

    // A registered action reaches the workspace with nothing else focused, and does what a click
    // on the cluster row does.
    #[gpui_kit::test]
    async fn registered_actions_drive_the_workspace(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (window, model) = open_workspace(cx, dir.path()).await;
        let dispatch = |cx: &mut TestAppContext, name: &str, args: serde_json::Value| {
            cx.update_window(window, |_, window, cx| {
                registry::dispatch(name, args, window, cx)
            })
            .unwrap()
        };

        dispatch(cx, "workspace::SelectCluster", json!({"cluster": "beta"})).unwrap();
        let active = model.clone();
        cx.wait_for(window, WAIT, move |_, cx| {
            active.read(cx).active.as_deref() == Some("beta")
        })
        .await;

        // No list or detail pane is open, so their actions are not available.
        assert_eq!(
            dispatch(cx, "resource_list::SelectNext", serde_json::Value::Null),
            Err(ActionError::NotAvailable(
                "resource_list::SelectNext".into()
            ))
        );
        assert_eq!(
            dispatch(cx, "detail::Refresh", serde_json::Value::Null),
            Err(ActionError::NotAvailable("detail::Refresh".into()))
        );
        // The workspace's own are, and so are the app's.
        let names: Vec<&str> = cx
            .update_window(window, |_, window, cx| {
                registry::available(window, cx).map(|s| s.name).collect()
            })
            .unwrap();
        assert!(names.contains(&"workspace::OpenView"), "{names:?}");
        assert!(names.contains(&"flint::Quit"), "{names:?}");
        assert!(!names.contains(&"detail::Close"), "{names:?}");
    }

    #[test]
    fn watch_mode_choices_match_the_menu() {
        let change = |scope, mode, active| watch_mode_change(&SetWatchMode { scope, mode }, active);
        assert_eq!(
            change(WatchScope::Cluster, Choice::OnView, Some("kind-a")),
            Some((Some("kind-a".into()), Some(WatchMode::OnView)))
        );
        assert_eq!(
            change(WatchScope::Cluster, Choice::UseDefault, Some("kind-a")),
            Some((Some("kind-a".into()), None))
        );
        assert_eq!(change(WatchScope::Cluster, Choice::All, None), None);
        assert_eq!(
            change(WatchScope::Default, Choice::All, None),
            Some((None, Some(WatchMode::All)))
        );
        assert_eq!(change(WatchScope::Default, Choice::UseDefault, None), None);
    }
}
