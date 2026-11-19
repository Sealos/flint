//! The window's visible view tree ([`crate::ui::view_state`]), assembled from the workspace
//! and the views it holds.
// module-tests: workspace_actions.rs

use flint_core::settings::AiExposureState;
use flint_core::{Appearance, ConnectionState, WatchMode, display};
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;

use super::super::view_state::{
    AppState, Banner, ClusterState, DialogState, Focus, HeaderState, KubeconfigPath,
    KubeconfigState, Level, Main, error_text,
};
use super::{Workspace, namespace_summary};

impl Workspace {
    /// What the window shows now, as data.
    pub fn view_state(&self, window: &mut Window, cx: &mut Context<Self>) -> AppState {
        let focus = self.focus_state(window, cx);
        let dialog = window.has_active_dialog(cx).then(|| match &self.dialog {
            Some(d) => DialogState {
                title: d.title.to_owned(),
                text: d.text.clone(),
                buttons: vec![d.ok.to_owned(), d.cancel.to_owned()],
            },
            None => DialogState {
                title: String::new(),
                text: String::new(),
                buttons: Vec::new(),
            },
        });
        let model = self.model.read(cx);
        let active = model.active.clone();

        let clusters = model
            .clusters
            .iter()
            .map(|cluster| {
                let (connection, error) = match &cluster.state {
                    ConnectionState::Disconnected => ("disconnected", None),
                    ConnectionState::Connecting => ("connecting", None),
                    ConnectionState::Connected(_) => ("connected", None),
                    ConnectionState::Failed(e) => ("failed", Some(error_text(e))),
                };
                ClusterState {
                    name: cluster.info.name.clone(),
                    label: display::short_name(&cluster.info.name).to_string(),
                    active: active.as_deref() == Some(cluster.info.name.as_str()),
                    connection,
                    error,
                    ai: match model.ai_exposure(cluster) {
                        AiExposureState::Hidden => "hidden",
                        AiExposureState::Exposed => "exposed",
                        AiExposureState::ServerChanged { .. } => "server_changed",
                    },
                }
            })
            .collect();

        let mut banners = Vec::new();
        if let Some(error) = &model.settings_error {
            banners.push(Banner::new(
                "settings-error",
                Level::Danger,
                error_text(error),
            ));
        }
        for (i, (path, error)) in model.kubeconfig_errors.iter().enumerate() {
            banners.push(Banner::new(
                &format!("kubeconfig-error-{i}"),
                Level::Danger,
                format!("{}: {}", display::file_name(path), error_text(error)),
            ));
        }
        if let Some(error) = &model.last_error {
            banners.push(Banner::new("last-error", Level::Danger, error_text(error)));
        }

        let list = self.list.as_ref().map(|l| l.read(cx));
        let detail = self.detail.as_ref().map(|d| d.read(cx));
        let active_cluster = model.active_cluster();
        let main = match active_cluster.map(|c| &c.state) {
            None => Main::NoCluster,
            Some(ConnectionState::Failed(e)) => Main::Failed {
                error: error_text(e),
            },
            Some(_) if list.is_some() => Main::List,
            Some(ConnectionState::Connected(v)) => Main::Connected {
                version: v.git_version.clone(),
            },
            Some(_) => Main::Connecting,
        };
        let header = active_cluster.map(|cluster| {
            let name = &cluster.info.name;
            let filter = self.ui_state.namespace_filter(name).to_vec();
            let namespaced = list.is_none_or(|l| l.shown().0.namespaced);
            let mut breadcrumb = vec![display::short_name(name).to_string()];
            if let Some(list) = list {
                breadcrumb.push(list.label().to_owned());
            }
            if let Some(detail) = detail {
                breadcrumb.push(detail.object().name.clone());
            }
            HeaderState {
                breadcrumb,
                namespace_button: namespaced.then(|| namespace_summary(&filter)),
                namespaces: filter,
                watch_mode: match model.settings.watch_mode_for(name) {
                    WatchMode::All => "Live: all kinds",
                    WatchMode::OnView => "Live: open views only",
                }
                .to_owned(),
                watch_mode_source: if model.settings.cluster_watch_mode.contains_key(name) {
                    "cluster"
                } else {
                    "default"
                },
            }
        });

        let kubeconfig = KubeconfigState {
            loaded: model.loaded,
            paths: model
                .kubeconfig_paths
                .iter()
                .map(|(path, exists)| KubeconfigPath {
                    path: path.display().to_string(),
                    exists: *exists,
                })
                .collect(),
            env: model
                .kubeconfig_env
                .as_ref()
                .map(|v| v.to_string_lossy().into_owned()),
        };
        AppState {
            focus,
            dialog,
            appearance: match model.settings.appearance {
                Appearance::System => "system",
                Appearance::Light => "light",
                Appearance::Dark => "dark",
            },
            kubeconfig,
            clusters,
            banners,
            active_cluster: active,
            main,
            header,
            nav: self.nav.read(cx).view_state(),
            list: list.map(|l| l.view_state()),
            detail: detail.map(|d| d.view_state()),
        }
    }

    /// The cluster the workspace shows, for the automation tests.
    #[cfg(all(test, feature = "automation"))]
    pub fn view_state_cluster(&self) -> Option<&str> {
        self.cluster.as_deref()
    }

    /// The open list, for the automation tests.
    #[cfg(all(test, feature = "automation"))]
    pub fn list(&self) -> Option<&Entity<super::super::list::ResourceList>> {
        self.list.as_ref()
    }

    fn focus_state(&self, window: &Window, cx: &App) -> Focus {
        if window.focused(cx).is_none() {
            return Focus::None;
        }
        if self.focus.is_focused(window) {
            return Focus::Workspace;
        }
        if let Some(focus) = self
            .list
            .as_ref()
            .and_then(|l| l.read(cx).focus_in(window, cx))
        {
            return focus;
        }
        if self
            .detail
            .as_ref()
            .is_some_and(|d| d.read(cx).yaml_focused(window, cx))
        {
            return Focus::DetailYaml;
        }
        if self.namespace_input.focus_handle(cx).is_focused(window) {
            return Focus::NamespaceInput;
        }
        Focus::Other
    }
}
