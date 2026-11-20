//! The workspace's registered actions ([`crate::ui::registry`]): the sidebar, the header and
//! the main area, their handlers, and the guards the registry checks before dispatching. The
//! controls dispatch these actions, and the guards and handlers share their check functions, so
//! a click, a key, the palette and automation all take one path.
//!
//! The workspace also forwards the list's, the detail pane's, the navigation's, the dock's and
//! the active log or shell tab's actions to them, and installs their guards, so those work and
//! are checked wherever focus is.

use flint_core::settings::AiExposureState;
use flint_core::{
    ClusterSummary, ConnectionState, LogSubject, NavEntry, PodSet, WatchMode, WorkloadTarget,
};
use gpui_kit::*;

use super::super::detail::{DetailView, Opening, refused};
use super::super::dock::Dock;
use super::super::focus::release_unfocused_input;
use super::super::list::ResourceList;
use super::super::log_view::{LogRequest, LogView};
use super::super::nav::NavPanel;
use super::super::registry::{
    ActionError, ActivateTab, AddNamespace, Bind, ClearSelection, Close, CloseActiveTab,
    CloseDetail, CloseTab, CopyLines, CopyText, DefaultWatchAllKinds, DefaultWatchOpenViewsOnly,
    DragSelection, ExposeToAi, ExtendSelectionDown, ExtendSelectionUp, FocusFilter, FocusSearch,
    Forward, ForwardCurrent, Guards, HideFromAi, JumpToEnd, JumpToStart, LeaveSearch, NextMatch,
    NextTab, OnView, OpenContainerLogs, OpenContainerShell, OpenLogs, OpenPreviousLogs,
    OpenSelectorLogs, OpenShell, OpenView, PaneName, PasteText, PrevMatch, PrevTab, ReconnectLogs,
    Refresh, RestartShell, RetryConnection, SaveLines, SaveLogs, SaveMatchingLines, SaveScope,
    SaveSelectedLines, ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop, SelectAllLines,
    SelectCluster, SelectFirst, SelectLast, SelectLine, SelectNext, SelectNextLine, SelectPrev,
    SelectPrevLine, SelectRow, SelectText, SetDockHeight, SetNamespaces, SetPaneWidth,
    ShowAllNamespaces, ShowOverview, ShowTabList, ShowYaml, SortBy, ToggleAnnotations,
    ToggleLabels, ToggleManagedFields, ToggleNamespace, ToggleNavSection, TogglePause,
    ToggleSecretValue, ToggleSources, ToggleTimestamps, ToggleWide, ToggleWrap,
    UseDefaultWatchMode, WatchAllKinds, WatchOpenViewsOnly,
};
use super::super::terminal_view::TerminalView;
use super::super::ui_state;
use super::{Pane, Workspace, confirm_expose, expose_dialog};

impl Workspace {
    /// Every workspace action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<SelectCluster>(Self::select_cluster)
            .bind::<ExposeToAi>(Self::expose_to_ai)
            .bind::<HideFromAi>(Self::hide_from_ai)
            .bind::<RetryConnection>(Self::retry_connection)
            .bind::<WatchAllKinds>(|this, _, _, cx| {
                this.set_watch_mode(true, Some(WatchMode::All), cx)
            })
            .bind::<WatchOpenViewsOnly>(|this, _, _, cx| {
                this.set_watch_mode(true, Some(WatchMode::OnView), cx)
            })
            .bind::<UseDefaultWatchMode>(|this, _, _, cx| this.set_watch_mode(true, None, cx))
            .bind::<DefaultWatchAllKinds>(|this, _, _, cx| {
                this.set_watch_mode(false, Some(WatchMode::All), cx)
            })
            .bind::<DefaultWatchOpenViewsOnly>(|this, _, _, cx| {
                this.set_watch_mode(false, Some(WatchMode::OnView), cx)
            })
            .bind::<OpenView>(Self::open_view)
            .bind::<SetNamespaces>(Self::set_namespaces)
            .bind::<ToggleNamespace>(Self::toggle_namespace)
            .bind::<AddNamespace>(Self::add_namespace)
            .bind::<ShowAllNamespaces>(|this, _, _, cx| {
                if this.cluster.is_some() {
                    this.set_namespace_filter(Vec::new(), cx)
                }
            })
            .bind::<SetPaneWidth>(Self::set_pane_width)
            .bind::<OpenSelectorLogs>(Self::open_selector_logs)
            .bind::<SetDockHeight>(|this, action, window, cx| match check_dock_height(action) {
                Ok(()) => {
                    let height = f32::from(window.viewport_size().height);
                    this.resize_dock(action.height, height, cx);
                }
                Err(e) => tracing::warn!("{e}"),
            })
    }

    /// Registers the workspace's actions on its root element, and forwards the open views'.
    pub(super) fn register_actions<E: InteractiveElement>(
        &self,
        element: E,
        cx: &Context<Self>,
    ) -> E {
        let element = Self::bind_actions(OnView::new(element, cx)).into_element();
        let element =
            ResourceList::bind_actions(Forward::new(element, self.list.as_ref())).into_element();
        // The detail pane follows the selected row, which may change between frames.
        let detail = ForwardCurrent::new(element, self, cx, |ws| ws.detail.as_ref());
        let element = DetailView::bind_actions(detail).into_element();
        let element = Dock::bind_actions(Forward::new(element, Some(&self.dock))).into_element();
        // Close Dock Tab only while a tab is shown, so the Window menu's item is disabled
        // otherwise and Cmd-W falls through to Close Window.
        let shown = self.dock.read(cx).shown_tab().map(|_| self.dock.clone());
        let element =
            Dock::bind_shown_tab_actions(Forward::new(element, shown.as_ref())).into_element();
        let log = self.dock.read(cx).active_log().cloned();
        let element = LogView::bind_actions(Forward::new(element, log.as_ref())).into_element();
        let shell = self.dock.read(cx).active_terminal().cloned();
        let element =
            TerminalView::bind_actions(Forward::new(element, shell.as_ref())).into_element();
        NavPanel::bind_actions(Forward::new(element, Some(&self.nav))).into_element()
    }

    /// The guards the registry checks before dispatching: when each action is enabled, and
    /// whether its arguments would do anything. The handlers use the same checks.
    pub(super) fn guards(this: WeakEntity<Self>) -> Guards<Self> {
        Guards::new(this)
            .add::<SelectCluster>(
                |_, _| true,
                |ws, a, cx| ws.check_cluster::<SelectCluster>(&a.cluster, cx),
            )
            .add::<ExposeToAi>(Self::settings_ok, |ws, a, cx| ws.check_expose(a, cx))
            .add::<HideFromAi>(Self::settings_ok, |ws, a, cx| ws.check_hide(a, cx))
            .when::<RetryConnection>(|ws, cx| ws.retry_enabled(cx))
            .when::<WatchAllKinds>(|ws, cx| ws.cluster.is_some() && ws.settings_ok(cx))
            .when::<WatchOpenViewsOnly>(|ws, cx| ws.cluster.is_some() && ws.settings_ok(cx))
            .when::<UseDefaultWatchMode>(|ws, cx| ws.cluster.is_some() && ws.settings_ok(cx))
            .when::<DefaultWatchAllKinds>(Self::settings_ok)
            .when::<DefaultWatchOpenViewsOnly>(Self::settings_ok)
            .add::<OpenView>(
                |ws, _| ws.cluster.is_some(),
                |ws, a, cx| ws.check_open_view(a, cx),
            )
            .add::<SetNamespaces>(
                |ws, _| ws.cluster.is_some(),
                |_, a, _| match a.namespaces.iter().any(|n| n.trim().is_empty()) {
                    true => Err(ActionError::rejected::<SetNamespaces>(
                        "empty namespace name",
                    )),
                    false => Ok(()),
                },
            )
            .add::<ToggleNamespace>(
                |ws, _| ws.cluster.is_some(),
                |_, a, _| check_name::<ToggleNamespace>(&a.namespace),
            )
            .add::<AddNamespace>(
                |ws, _| ws.cluster.is_some(),
                |_, a, _| check_name::<AddNamespace>(&a.namespace),
            )
            .when::<ShowAllNamespaces>(|ws, _| ws.cluster.is_some())
            .add::<SetPaneWidth>(|_, _| true, |ws, a, _| ws.check_pane_width(a))
            .add::<ToggleNavSection>(
                |ws, cx| ws.nav.read(cx).has_sections(),
                |ws, a, cx| ws.nav.read(cx).check_section(a),
            )
            // The list's.
            .when::<SelectPrev>(|ws, cx| list_wide(ws, cx).is_some())
            .when::<SelectNext>(|ws, cx| list_wide(ws, cx).is_some())
            .when::<SelectFirst>(|ws, cx| list_wide(ws, cx).is_some())
            .when::<SelectLast>(|ws, cx| list_wide(ws, cx).is_some())
            .when::<CloseDetail>(|ws, cx| list_wide(ws, cx).is_some())
            .when::<FocusFilter>(|ws, cx| list_wide(ws, cx).is_some())
            .when::<ToggleWide>(|ws, cx| list_wide(ws, cx) == Some(true))
            .add::<SelectRow>(
                |ws, cx| list_wide(ws, cx).is_some(),
                |ws, a, cx| match &ws.list {
                    Some(l) => l.read(cx).check_select_row(a),
                    None => Err(ActionError::unavailable::<SelectRow>()),
                },
            )
            .add::<SortBy>(
                |ws, cx| list_wide(ws, cx).is_some(),
                |ws, a, cx| match &ws.list {
                    Some(l) => l.read(cx).check_sort(a),
                    None => Err(ActionError::unavailable::<SortBy>()),
                },
            )
            // The detail pane's.
            .when::<Refresh>(|ws, _| ws.detail.is_some())
            .when::<Close>(|ws, _| ws.detail.is_some())
            .when::<ShowOverview>(|ws, _| ws.detail.is_some())
            .when::<ShowYaml>(|ws, _| ws.detail.is_some())
            .when::<ToggleLabels>(|ws, _| ws.detail.is_some())
            .when::<ToggleAnnotations>(|ws, _| ws.detail.is_some())
            .when::<ToggleManagedFields>(|ws, cx| {
                ws.detail
                    .as_ref()
                    .is_some_and(|d| d.read(cx).has_managed_fields())
            })
            .add::<ToggleSecretValue>(
                |ws, cx| ws.detail.as_ref().is_some_and(|d| d.read(cx).is_secret()),
                |ws, a, cx| match &ws.detail {
                    Some(d) => d.read(cx).check_secret_key(a),
                    None => Err(ActionError::unavailable::<ToggleSecretValue>()),
                },
            )
            // Logs and Shell act on the selected row, which the detail pane shows (UP-47).
            .when::<OpenLogs>(|ws, cx| {
                ws.sessions_open_here(cx)
                    && detail(ws, cx).is_some_and(|d| d.log_plan::<OpenLogs>(None, false).is_ok())
            })
            .when::<OpenPreviousLogs>(|ws, cx| {
                ws.sessions_open_here(cx)
                    && detail(ws, cx)
                        .is_some_and(|d| d.log_plan::<OpenPreviousLogs>(None, true).is_ok())
            })
            .add::<OpenContainerLogs>(
                |ws, cx| {
                    ws.sessions_open_here(cx)
                        && detail(ws, cx).is_some_and(|d| !d.log_containers().is_empty())
                },
                |ws, a, cx| match detail(ws, cx) {
                    Some(d) => d
                        .log_plan::<OpenContainerLogs>(Some(&a.container), a.previous)
                        .map(|_| ()),
                    None => Err(ActionError::unavailable::<OpenContainerLogs>()),
                },
            )
            // A pod or a workload's pods not known yet: Shell waits for them.
            .when::<OpenShell>(|ws, cx| {
                ws.sessions_open_here(cx)
                    && detail(ws, cx).is_some_and(|d| d.shell_check::<OpenShell>(None).is_ok())
            })
            .add::<OpenContainerShell>(
                |ws, cx| {
                    // Another container may take a shell while the default one is not running.
                    ws.sessions_open_here(cx)
                        && detail(ws, cx).is_some_and(|d| {
                            d.shell_check::<OpenContainerShell>(None).is_ok()
                                || d.shell_picker().is_some()
                        })
                },
                |ws, a, cx| match detail(ws, cx) {
                    Some(d) => d.shell_check::<OpenContainerShell>(Some(&a.container)),
                    None => Err(ActionError::unavailable::<OpenContainerShell>()),
                },
            )
            .add::<OpenSelectorLogs>(
                |ws, cx| ws.cluster.is_some() && ws.sessions_open_here(cx),
                |_, a, _| check_selector(a),
            )
            // The dock's.
            .when::<CloseActiveTab>(|ws, cx| {
                let dock = ws.dock.read(cx);
                dock.is_visible() && dock.active_id().is_some()
            })
            .when::<NextTab>(|ws, cx| {
                let dock = ws.dock.read(cx);
                dock.is_visible() && dock.tab_count() > 1
            })
            .when::<PrevTab>(|ws, cx| {
                let dock = ws.dock.read(cx);
                dock.is_visible() && dock.tab_count() > 1
            })
            .when::<ShowTabList>(|ws, cx| ws.dock.read(cx).tab_count() > 0)
            .when::<ReconnectLogs>(|ws, cx| ws.dock.read(cx).restartable(true, cx).is_some())
            .when::<RestartShell>(|ws, cx| ws.dock.read(cx).restartable(false, cx).is_some())
            .add::<ActivateTab>(
                |ws, cx| ws.dock.read(cx).tab_count() > 0,
                |ws, a, cx| ws.dock.read(cx).check_tab::<ActivateTab>(a.tab),
            )
            .add::<CloseTab>(
                |ws, cx| ws.dock.read(cx).tab_count() > 0,
                |ws, a, cx| ws.dock.read(cx).check_tab::<CloseTab>(a.tab),
            )
            .add::<SetDockHeight>(|_, _| true, |_, a, _| check_dock_height(a))
            // The active shell tab's.
            .when::<CopyText>(|ws, cx| shell(ws, cx).is_some_and(TerminalView::has_selection))
            .when::<PasteText>(|ws, cx| shell(ws, cx).is_some_and(TerminalView::is_live))
            .when::<ScrollPageUp>(|ws, cx| shell(ws, cx).is_some_and(TerminalView::can_scroll_back))
            .when::<ScrollToTop>(|ws, cx| shell(ws, cx).is_some_and(TerminalView::can_scroll_back))
            .when::<ScrollPageDown>(|ws, cx| {
                shell(ws, cx).is_some_and(TerminalView::is_scrolled_back)
            })
            .when::<ScrollToBottom>(|ws, cx| {
                shell(ws, cx).is_some_and(TerminalView::is_scrolled_back)
            })
            .add::<SelectText>(
                |ws, cx| shell(ws, cx).is_some(),
                |ws, a, cx| match shell(ws, cx) {
                    Some(t) => t.check_cells(a),
                    None => Err(ActionError::unavailable::<SelectText>()),
                },
            )
            // The active log tab's.
            .when::<TogglePause>(|ws, cx| log(ws, cx).is_some_and(LogView::can_pause))
            .when::<JumpToEnd>(|ws, cx| log(ws, cx).is_some_and(LogView::has_lines))
            .when::<JumpToStart>(|ws, cx| log(ws, cx).is_some_and(LogView::has_lines))
            .when::<ToggleTimestamps>(|ws, cx| log(ws, cx).is_some())
            .when::<ToggleWrap>(|ws, cx| log(ws, cx).is_some())
            .when::<ToggleSources>(|ws, cx| log(ws, cx).is_some_and(LogView::is_workload))
            .when::<FocusSearch>(|ws, cx| log(ws, cx).is_some())
            .when::<NextMatch>(|ws, cx| log(ws, cx).is_some_and(LogView::has_matches))
            .when::<PrevMatch>(|ws, cx| log(ws, cx).is_some_and(LogView::has_matches))
            .when::<SelectPrevLine>(|ws, cx| log(ws, cx).is_some_and(LogView::has_lines))
            .when::<SelectNextLine>(|ws, cx| log(ws, cx).is_some_and(LogView::has_lines))
            .when::<ExtendSelectionUp>(|ws, cx| log(ws, cx).is_some_and(LogView::has_lines))
            .when::<ExtendSelectionDown>(|ws, cx| log(ws, cx).is_some_and(LogView::has_lines))
            .when::<SelectAllLines>(|ws, cx| log(ws, cx).is_some_and(LogView::has_lines))
            .when::<ClearSelection>(|ws, cx| log(ws, cx).is_some_and(LogView::has_selection))
            .when::<LeaveSearch>(|ws, cx| log(ws, cx).is_some())
            .when::<CopyLines>(|ws, cx| log(ws, cx).is_some_and(LogView::has_selection))
            .when::<SaveLogs>(|ws, cx| {
                log(ws, cx).is_some_and(|l| l.check_save(SaveScope::All).is_ok())
            })
            .when::<SaveMatchingLines>(|ws, cx| {
                log(ws, cx).is_some_and(|l| l.check_save(SaveScope::Matches).is_ok())
            })
            .when::<SaveSelectedLines>(|ws, cx| {
                log(ws, cx).is_some_and(|l| l.check_save(SaveScope::Selection).is_ok())
            })
            .add::<SaveLines>(
                |ws, cx| log(ws, cx).is_some_and(LogView::has_lines),
                |ws, a, cx| match log(ws, cx) {
                    Some(l) => l.check_save(a.lines),
                    None => Err(ActionError::unavailable::<SaveLines>()),
                },
            )
            .add::<SelectLine>(
                |ws, cx| log(ws, cx).is_some_and(LogView::has_lines),
                |ws, a, cx| match log(ws, cx) {
                    Some(l) => l.check_line::<SelectLine>(a.line),
                    None => Err(ActionError::unavailable::<SelectLine>()),
                },
            )
            .add::<DragSelection>(
                |ws, cx| log(ws, cx).is_some_and(LogView::has_selection),
                |ws, a, cx| match log(ws, cx) {
                    Some(l) => l.check_line::<DragSelection>(a.line),
                    None => Err(ActionError::unavailable::<DragSelection>()),
                },
            )
    }

    /// Gives focus back after it left with a closed view (or a hidden dock): to the open list,
    /// or the workspace.
    pub(super) fn reclaim_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = match &self.list {
            Some(list) => list.read(cx).focus_handle().clone(),
            None => self.focus.clone(),
        };
        window.focus(&focus, cx);
        // A text field that had focus in the closed view (a list's filter, a log tab's search).
        release_unfocused_input(window, cx);
    }

    fn settings_ok(&self, cx: &App) -> bool {
        self.model.read(cx).settings_error.is_none()
    }

    pub(super) fn cluster_summary<'a>(
        &self,
        name: &str,
        cx: &'a App,
    ) -> Option<&'a ClusterSummary> {
        self.model
            .read(cx)
            .clusters
            .iter()
            .find(|c| c.info.name == name)
    }

    fn check_cluster<A: Action>(&self, name: &str, cx: &App) -> Result<(), ActionError> {
        match self.cluster_summary(name, cx) {
            Some(_) => Ok(()),
            None => Err(ActionError::rejected::<A>(format!(
                "no kubeconfig context named {name}"
            ))),
        }
    }

    /// [`ExposeToAi`]: a context with a server, not exposed already. Returns the server and the
    /// one exposed before, if the context moved.
    fn exposure_change(
        &self,
        action: &ExposeToAi,
        cx: &App,
    ) -> Result<(String, Option<String>), ActionError> {
        if !self.settings_ok(cx) {
            return Err(ActionError::rejected::<ExposeToAi>(
                "settings.toml is invalid",
            ));
        }
        self.check_cluster::<ExposeToAi>(&action.cluster, cx)?;
        let cluster = self.cluster_summary(&action.cluster, cx).expect("checked");
        let Some(server) = cluster.info.server.clone() else {
            return Err(ActionError::rejected::<ExposeToAi>(
                "the context has no API server",
            ));
        };
        match self.model.read(cx).ai_exposure(cluster) {
            AiExposureState::Exposed => Err(ActionError::rejected::<ExposeToAi>(
                "the cluster is exposed already",
            )),
            AiExposureState::Hidden => Ok((server, None)),
            AiExposureState::ServerChanged { exposed_server } => Ok((server, Some(exposed_server))),
        }
    }

    fn check_expose(&self, action: &ExposeToAi, cx: &App) -> Result<(), ActionError> {
        self.exposure_change(action, cx).map(|_| ())
    }

    /// [`HideFromAi`]: an exposed context (or a stale exposure); hiding a hidden one is a no-op.
    fn check_hide(&self, action: &HideFromAi, cx: &App) -> Result<(), ActionError> {
        if !self.settings_ok(cx) {
            return Err(ActionError::rejected::<HideFromAi>(
                "settings.toml is invalid",
            ));
        }
        self.check_cluster::<HideFromAi>(&action.cluster, cx)?;
        let cluster = self.cluster_summary(&action.cluster, cx).expect("checked");
        match self.model.read(cx).ai_exposure(cluster) {
            AiExposureState::Hidden => Err(ActionError::rejected::<HideFromAi>(
                "the cluster is hidden already",
            )),
            _ => Ok(()),
        }
    }

    /// [`RetryConnection`]: the active cluster's connection or discovery failed.
    fn retry_enabled(&self, cx: &App) -> bool {
        let model = self.model.read(cx);
        let failed = model
            .active_cluster()
            .is_some_and(|c| matches!(c.state, ConnectionState::Failed(_)));
        let discovery_failed = model
            .active
            .as_ref()
            .and_then(|c| model.discovery.get(c))
            .is_some_and(|d| d.error.is_some() && d.discovery.is_none());
        failed || discovery_failed
    }

    fn check_open_view(&self, action: &OpenView, cx: &App) -> Result<(), ActionError> {
        match self.nav_entry(&action.kind, cx) {
            Some(_) => Ok(()),
            None => Err(ActionError::rejected::<OpenView>(format!(
                "the active cluster does not serve {} (or discovery has not finished)",
                action.kind
            ))),
        }
    }

    fn check_pane_width(&self, action: &SetPaneWidth) -> Result<(), ActionError> {
        if !action.width.is_finite() || action.width <= 0. {
            return Err(ActionError::rejected::<SetPaneWidth>(
                "width must be positive",
            ));
        }
        if action.pane == PaneName::Detail && self.detail.is_none() {
            return Err(ActionError::rejected::<SetPaneWidth>(
                "no detail pane is open",
            ));
        }
        Ok(())
    }

    /// Why log and shell sessions cannot open on `cluster` now (it is not connected), if so.
    /// A cluster still connecting may: the object to act on is loading too.
    pub(in crate::ui) fn sessions_blocked(&self, cluster: &str, cx: &App) -> Option<String> {
        match self.cluster_summary(cluster, cx).map(|c| &c.state) {
            Some(ConnectionState::Connected(_) | ConnectionState::Connecting) => None,
            // Already a short, safe summary (`ClusterError::summary`).
            Some(ConnectionState::Failed(error)) => {
                Some(format!("{cluster} is not connected: {error}"))
            }
            Some(ConnectionState::Disconnected) => Some(format!("{cluster} is not connected")),
            None => Some(format!("no kubeconfig context named {cluster}")),
        }
    }

    /// Whether sessions can open on the cluster of the object shown, or else the active one.
    fn sessions_open_here(&self, cx: &App) -> bool {
        let cluster = match &self.detail {
            Some(d) => Some(d.read(cx).object().cluster.clone()),
            None => self.cluster.clone(),
        };
        cluster.is_some_and(|c| self.sessions_blocked(&c, cx).is_none())
    }

    /// Opens the merged logs of the pods matching a label selector on the active cluster.
    fn open_selector_logs(
        &mut self,
        action: &OpenSelectorLogs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = check_selector(action) {
            let why = super::super::detail::reason(&e);
            self.refuse(refused(Opening::Logs, &why), cx);
            return;
        }
        let Some(cluster) = self.cluster.clone() else {
            self.refuse(refused(Opening::Logs, "no cluster is selected"), cx);
            return;
        };
        let request = LogRequest {
            cluster,
            subject: LogSubject::Workload(WorkloadTarget {
                namespace: action.namespace.trim().to_owned(),
                pods: PodSet::Selector(action.selector.trim().to_owned()),
                container: None,
            }),
            previous: false,
        };
        self.open_logs(request, true, window, cx);
    }

    fn select_cluster(&mut self, action: &SelectCluster, _: &mut Window, cx: &mut Context<Self>) {
        match self.check_cluster::<SelectCluster>(&action.cluster, cx) {
            Ok(()) => {
                let cluster = action.cluster.clone();
                self.model.update(cx, |m, _| m.activate(cluster));
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    /// Opens the confirmation naming the context and its server (and the previous server, when
    /// the context moved); confirming exposes it.
    fn expose_to_ai(&mut self, action: &ExposeToAi, window: &mut Window, cx: &mut Context<Self>) {
        match self.exposure_change(action, cx) {
            Ok((server, previous)) => {
                let model = self.model.clone();
                let dialog = expose_dialog(&action.cluster, &server, previous.as_deref());
                confirm_expose(window, cx, model, action.cluster.clone(), server, dialog);
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    fn hide_from_ai(&mut self, action: &HideFromAi, _: &mut Window, cx: &mut Context<Self>) {
        match self.check_hide(action, cx) {
            Ok(()) => {
                let cluster = action.cluster.clone();
                self.model.update(cx, |m, _| m.hide_from_ai(cluster));
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    fn retry_connection(&mut self, _: &RetryConnection, _: &mut Window, cx: &mut Context<Self>) {
        if self.retry_enabled(cx) {
            self.retry(cx);
        } else {
            // Not ours to handle: let another binding of the same key (Refresh) have it.
            cx.propagate();
        }
    }

    /// The watch-mode menu: the active cluster's own mode (`None`: follow the default), or the
    /// default for all clusters.
    fn set_watch_mode(&mut self, own: bool, mode: Option<WatchMode>, cx: &mut Context<Self>) {
        if !self.settings_ok(cx) {
            tracing::warn!("watch mode: settings.toml is invalid");
            return;
        }
        let Some((target, mode)) = watch_mode_change(own, mode, self.cluster.as_deref()) else {
            tracing::warn!("watch mode: no active cluster");
            return;
        };
        self.model.update(cx, |m, _| m.set_watch_mode(target, mode));
    }

    /// Opens a kind as clicking it in the navigation does.
    fn open_view(&mut self, action: &OpenView, window: &mut Window, cx: &mut Context<Self>) {
        match self.nav_entry(&action.kind, cx) {
            Some(entry) => self.open_kind(&entry, window, cx),
            None => tracing::warn!("{}", self.check_open_view(action, cx).unwrap_err()),
        }
    }

    /// The active cluster's navigation entry for `kind`, named as `kubectl get` takes it (Kind,
    /// plural, singular or short name, optionally `.group`), through the core resolver.
    pub(in crate::ui) fn nav_entry(&self, kind: &str, cx: &App) -> Option<NavEntry> {
        let model = self.model.read(cx);
        let active = model.active.as_ref()?;
        let discovery = model.discovery.get(active)?.discovery.as_ref()?;
        discovery.nav_entry(kind).ok().cloned()
    }

    fn set_namespaces(&mut self, action: &SetNamespaces, _: &mut Window, cx: &mut Context<Self>) {
        if self.cluster.is_none() {
            return;
        }
        let names = action
            .namespaces
            .iter()
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .collect();
        self.set_namespace_filter(ui_state::normalize(names), cx);
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

    /// Adds a namespace typed by name, as Enter in the filter's field does.
    pub(super) fn add_namespace(
        &mut self,
        action: &AddNamespace,
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
        let mut next = self.ui_state.namespace_filter(&cluster).to_vec();
        next.push(namespace.to_owned());
        self.set_namespace_filter(ui_state::normalize(next), cx);
    }

    /// Sets a pane's width, as dragging its edge does (same limits, same delayed save).
    fn set_pane_width(
        &mut self,
        action: &SetPaneWidth,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_pane_width(action) {
            tracing::warn!("{e}");
            return;
        }
        let pane = match action.pane {
            PaneName::Clusters => Pane::Clusters,
            PaneName::Navigation => Pane::Navigation,
            PaneName::Detail => Pane::Detail,
        };
        let window_width = f32::from(window.viewport_size().width);
        self.resize_pane(pane, action.width, window_width, cx);
    }
}

/// The detail pane, if one is open.
fn detail<'a>(ws: &Workspace, cx: &'a App) -> Option<&'a DetailView> {
    ws.detail.as_ref().map(|d| d.read(cx))
}

/// The shell tab the dock shows, if it is visible and shows one.
fn shell<'a>(ws: &Workspace, cx: &'a App) -> Option<&'a TerminalView> {
    ws.dock.read(cx).active_terminal().map(|t| t.read(cx))
}

/// The log tab the dock shows, if it is visible and shows one.
fn log<'a>(ws: &Workspace, cx: &'a App) -> Option<&'a LogView> {
    ws.dock.read(cx).active_log().map(|l| l.read(cx))
}

/// [`OpenSelectorLogs`] takes a namespace and a selector that selects something: an empty
/// one would follow every pod in the namespace.
fn check_selector(action: &OpenSelectorLogs) -> Result<(), ActionError> {
    if action.namespace.trim().is_empty() {
        return Err(ActionError::rejected::<OpenSelectorLogs>(
            "empty namespace name",
        ));
    }
    if action.selector.trim().is_empty() {
        return Err(ActionError::rejected::<OpenSelectorLogs>(
            "empty label selector",
        ));
    }
    Ok(())
}

/// [`SetDockHeight`] takes a positive height.
fn check_dock_height(action: &SetDockHeight) -> Result<(), ActionError> {
    if action.height.is_finite() && action.height > 0. {
        Ok(())
    } else {
        Err(ActionError::rejected::<SetDockHeight>(
            "height must be positive",
        ))
    }
}

/// Whether a list is open, and whether its kind has `-o wide` columns.
fn list_wide(ws: &Workspace, cx: &App) -> Option<bool> {
    ws.list.as_ref().map(|l| l.read(cx).has_wide_columns())
}

/// A namespace name, trimmed, must not be empty.
fn check_name<A: Action>(namespace: &str) -> Result<(), ActionError> {
    if namespace.trim().is_empty() {
        Err(ActionError::rejected::<A>("empty namespace name"))
    } else {
        Ok(())
    }
}

/// The settings change a watch-mode choice makes: the cluster (`None`: the default) and the
/// mode (`None`: follow the default). `None` when it needs an active cluster and there is none,
/// or asks the default to follow itself.
fn watch_mode_change(
    own: bool,
    mode: Option<WatchMode>,
    active: Option<&str>,
) -> Option<(Option<String>, Option<WatchMode>)> {
    if own {
        Some((Some(active?.to_owned()), mode))
    } else {
        Some((None, Some(mode?)))
    }
}

#[cfg(test)]
mod tests {
    use flint_core::WatchMode;

    use super::watch_mode_change;
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    #[test]
    fn every_workspace_action_is_bound() {
        assert_binds_scope(Scope::Workspace, super::Workspace::bind_actions);
    }

    #[test]
    fn watch_mode_choices_match_the_menu() {
        assert_eq!(
            watch_mode_change(true, Some(WatchMode::OnView), Some("kind-a")),
            Some((Some("kind-a".into()), Some(WatchMode::OnView)))
        );
        assert_eq!(
            watch_mode_change(true, None, Some("kind-a")),
            Some((Some("kind-a".into()), None))
        );
        assert_eq!(watch_mode_change(true, Some(WatchMode::All), None), None);
        assert_eq!(
            watch_mode_change(false, Some(WatchMode::All), None),
            Some((None, Some(WatchMode::All)))
        );
        assert_eq!(watch_mode_change(false, None, None), None);
    }
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod view_tests;

#[cfg(test)]
#[path = "header_tests.rs"]
mod header_tests;

#[cfg(test)]
#[path = "expose_tests.rs"]
mod expose_tests;

#[cfg(test)]
#[path = "row_actions_tests.rs"]
mod row_actions_tests;

#[cfg(test)]
#[path = "dock_state_tests.rs"]
mod dock_state_tests;
