//! The window's visible view tree ([`crate::ui::view_state`]), assembled from the workspace
//! and the views it holds.
// module-tests: workspace_actions.rs

use flint_core::settings::AiExposureState;
use flint_core::{Appearance, ConnectionState, display};
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestQueryExt as _;
use gpui_kit::*;

use super::super::clusters::{Presence, state_name};
use super::super::confirm;
use super::super::menu_record;
use super::super::row_menu;
use super::super::view_state::{
    ActionAvailability, AppState, Banner, ClusterState, DialogState, Focus, HeaderState,
    KubeconfigPath, KubeconfigState, Level, Main, MenuEntryState, MenuState, connection_error_text,
    error_text,
};
use super::{Workspace, header_controls, namespace_button_text, watch_mode_label};

impl Workspace {
    /// What the window shows now, as data.
    pub fn view_state(&self, window: &mut Window, cx: &mut Context<Self>) -> AppState {
        let menu = open_menu(self, window, cx);
        let focus = self.focus_state(menu.is_some(), window, cx);
        let dialog = confirm::shown(window, cx).map(|d| DialogState {
            title: d.title,
            text: d.text,
            // A notice (About Flint) has no Cancel.
            buttons: [d.ok, d.cancel]
                .into_iter()
                .filter(|b| !b.is_empty())
                .collect(),
        });
        let window_height = f32::from(window.viewport_size().height);
        let dock = self
            .dock
            .read(cx)
            .view_state(self.dock_height(window_height), window, cx);
        let cursor = self.sidebar_cursor(cx);
        let model = self.model.read(cx);
        let active = model.active.clone();
        let names = model.names();
        let states = self.nav.read(cx).states();

        let clusters = model
            .clusters
            .iter()
            .map(|cluster| {
                let error = match &cluster.state {
                    ConnectionState::Failed(e) => Some(connection_error_text(e)),
                    _ => None,
                };
                let is_active = active.as_deref() == Some(cluster.info.name.as_str());
                ClusterState {
                    name: cluster.info.name.clone(),
                    label: names.of(&cluster.info.name),
                    active: is_active,
                    connection: state_name(&cluster.state),
                    presence: Presence::of(&cluster.state, is_active, states).name(),
                    error,
                    ai: match model.ai_exposure(cluster) {
                        AiExposureState::Hidden => "hidden",
                        AiExposureState::Exposed => "exposed",
                        AiExposureState::ServerChanged { .. } => "server_changed",
                    },
                    protection: model.protection(cluster).name(),
                    cursor: cursor.as_deref() == Some(cluster.info.name.as_str()),
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
        if let Some(error) = self.action_error() {
            banners.push(Banner::new(
                "action-error",
                Level::Danger,
                error_text(error),
            ));
        }

        let list = self.list.as_ref().map(|l| l.read(cx));
        let detail = self.detail.as_ref().map(|d| d.read(cx));
        let active_cluster = model.active_cluster();
        let main = match active_cluster.map(|c| &c.state) {
            None if model.loaded && model.clusters.is_empty() => Main::NoContexts,
            None => Main::NoCluster,
            Some(ConnectionState::Failed(e)) => Main::Failed {
                error: connection_error_text(e),
            },
            Some(ConnectionState::AwaitingConfirmation) => Main::AwaitingConfirmation,
            Some(_) if list.is_some() => Main::List,
            Some(ConnectionState::Connected(v)) => Main::Connected {
                version: v.git_version.clone(),
            },
            Some(_) => Main::Connecting,
        };
        let header = active_cluster.map(|cluster| {
            let name = &cluster.info.name;
            let filter = self.ui_state.namespace_filter(name).to_vec();
            let controls = header_controls(cluster, list);
            let mut breadcrumb = vec![names.of(name)];
            if let Some(list) = list {
                breadcrumb.push(list.label().to_owned());
            }
            if let Some(detail) = detail {
                breadcrumb.push(detail.object().name.clone());
            }
            HeaderState {
                breadcrumb,
                namespace_button: controls
                    .namespaces
                    .then(|| namespace_button_text(&filter, list.and_then(|l| l.limited_to()))),
                namespaces: filter,
                watch_mode: controls
                    .watch_mode
                    .then(|| watch_mode_label(model.settings.watch_mode_for(name)).to_owned()),
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
            detail: detail.map(|d| {
                let mut state = d.view_state();
                state.actions = row_action_states(self, cx);
                state
            }),
            dock,
            menu,
            palette: self.palette.as_ref().map(|(p, _)| p.read(cx).view_state()),
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

    fn focus_state(&self, menu_open: bool, window: &mut Window, cx: &mut App) -> Focus {
        if window.focused(cx).is_none() {
            return Focus::None;
        }
        if self.focus.is_focused(window) {
            return Focus::Workspace;
        }
        if self.sidebar_focus.is_focused(window) {
            return Focus::Sidebar;
        }
        if self.nav.read(cx).focus_handle().is_focused(window) {
            return Focus::Navigation;
        }
        if self
            .palette
            .as_ref()
            .is_some_and(|(p, _)| p.read(cx).has_focus(window, cx))
        {
            return Focus::Palette;
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
        if self
            .detail
            .as_ref()
            .is_some_and(|d| d.read(cx).focus_handle().is_focused(window))
        {
            return Focus::Detail;
        }
        if self.namespace_input.focus_handle(cx).is_focused(window) {
            return Focus::NamespaceInput;
        }
        match self.dock.read(cx).focus_in(window, cx) {
            // A menu drawn in the dock (the tab list, a log toolbar's Save or "⋯") has focus.
            Some(_) if menu_open => return Focus::Menu,
            Some(focus) => return focus,
            None => {}
        }
        if window.has_active_dialog(cx) {
            return Focus::Dialog;
        }
        if menu_open {
            return Focus::Menu;
        }
        Focus::Other
    }
}

/// Logs, Previous Logs and Shell for the selected row, as its menu shows them.
fn row_action_states(ws: &Workspace, cx: &App) -> Vec<ActionAvailability> {
    let Some(a) = row_menu::row_actions(ws, cx) else {
        return Vec::new();
    };
    if a.subject == crate::ui::detail::Subject::Other {
        return Vec::new();
    }
    let state = |action, can: &row_menu::Availability| ActionAvailability {
        action,
        enabled: can.is_ok(),
        reason: can.as_ref().err().cloned(),
    };
    let mut states = vec![state("Logs", &a.logs)];
    if let Some(previous) = &a.previous {
        states.push(state("Previous Logs", previous));
    }
    states.push(state("Shell", &a.shell));
    states
}

/// The context menu or drop-down drawn in the last frame, if any (read from the drawn
/// elements: GPUI keeps a menu's items to itself). A row's menu is described from the row's
/// actions, which it follows while open ([`row_menu::menu_state`]); a menu that recorded what
/// it was built with ([`menu_record`]: a log tab's toolbar menus, the detail pane's pickers, the
/// dock's tab list) by that; any other by its drawn entries, where a note cannot be told from a
/// disabled entry.
fn open_menu(ws: &Workspace, window: &Window, cx: &App) -> Option<MenuState> {
    let menu_id = ElementId::Name("popup-menu".into());
    let elements = window.elements();
    // The outermost menu: a submenu is drawn inside its menu.
    let path = elements
        .iter()
        .filter(|e| e.path().last() == Some(&menu_id))
        .map(|e| e.path().to_vec())
        .min_by_key(Vec::len)?;
    let row = path.iter().rev().find_map(|id| match id {
        ElementId::Name(name) if name.starts_with("row-menu-") => Some(name.to_string()),
        _ => None,
    });
    if let Some(row) = row
        && let Some(actions) = row_menu::row_actions(ws, cx)
    {
        let (note, entries) = row_menu::menu_state(&actions);
        return Some(MenuState {
            owner: Some(row),
            note,
            entries,
        });
    }
    // The menu's entity: the view drawn as `popup-menu`.
    let entity = match path.iter().rev().nth(1) {
        Some(ElementId::View(entity)) => Some(*entity),
        _ => None,
    };
    if let Some((owner, entries)) = entity.and_then(|e| menu_record::built(e, cx)) {
        return Some(MenuState {
            owner: Some(owner.to_owned()),
            note: None,
            entries: entries
                .into_iter()
                .map(|e| MenuEntryState {
                    label: e.label,
                    enabled: e.enabled,
                    reason: None,
                    checked: e.checked,
                    submenu: Vec::new(),
                })
                .collect(),
        });
    }
    let mut items: Vec<(u64, MenuEntryState)> = elements
        .iter()
        // The menu's own rows, not a submenu's.
        .filter(|e| {
            e.path().len() > path.len()
                && e.path().starts_with(&path)
                && !e.path()[path.len()..].iter().any(
                    |id| matches!(id, ElementId::Name(n) if n == "submenu" || n == "popup-menu"),
                )
        })
        .filter_map(|e| match (e.path().last(), e.label()) {
            // A separator has no label.
            (Some(ElementId::Integer(ix)), Some(label)) => Some((
                *ix,
                MenuEntryState {
                    label: label.to_owned(),
                    enabled: e.disabled() != Some(true),
                    reason: None,
                    checked: e.checked(),
                    submenu: Vec::new(),
                },
            )),
            _ => None,
        })
        .collect();
    items.sort_by_key(|(ix, _)| *ix);
    Some(MenuState {
        owner: None,
        note: None,
        entries: items.into_iter().map(|(_, entry)| entry).collect(),
    })
}
