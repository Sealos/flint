//! The workspace's registered actions ([`crate::ui::registry`]): the sidebar, the header and
//! the main area, their handlers, and the guards the registry checks before dispatching. The
//! controls dispatch these actions, and the guards and handlers share their check functions, so
//! a click, a key, the palette and automation all take one path.
//!
//! The workspace also forwards the list's, the detail pane's, the navigation's, the dock's and
//! the active log or shell tab's actions to them, and installs their guards, so those work and
//! are checked wherever focus is.

use flint_core::settings::{AiExposureState, AiScope, TrustState};
use flint_core::{
    ClusterSummary, ConnectionState, LogSubject, NavEntry, PodSet, Protection, WatchMode,
    WorkloadTarget,
};
use gpui_kit::base::actions::Cancel;
use gpui_kit::*;

use std::rc::Rc;

use super::super::activity_view::ActivityView;
use super::super::helm_view::{HelmView, Pane as HelmPane};
use super::super::registry::{
    RefreshHelmReleases, SelectHelmRelease, ShowHelmDetails, ShowHelmHistory, ShowHelmManifest,
    ShowHelmValues,
};
use flint_core::compare::CompareTarget;

use super::super::clusters::ClustersModel;
use super::super::compare::{self, CompareView};
use super::super::confirm::{self, DialogText};
use super::super::detail::{DetailView, Opening, refused};
use super::super::dock::{Dock, DockTab, TabKey};
use super::super::focus::release_unfocused_input;
use super::super::keep_watching;
use super::super::list::ResourceList;
use super::super::log_view::{LogRequest, LogView};
use super::super::nav::NavPanel;
use super::super::palette::{ArgInput, Choice, ChoiceSource, CommandPalette, Mode, PaletteEvent};
use super::super::registry::{
    About, ActionError, ActionSpec, ActivateTab, AddNamespace, ApplyYamlEdit, Arg, ArgKind,
    BackToYamlEdit, Bind, CancelConnection, CancelYamlEdit, ClearSelection, Close, CloseActiveTab,
    CloseDetail, CloseTab, CopyLines, CopyText, DefaultWatchAllKinds, DefaultWatchOpenViewsOnly,
    DragSelection, EditYaml, ExposeToAi, ExtendSelectionDown, ExtendSelectionUp, FocusFilter,
    FocusNextPane, FocusPreviousPane, FocusSearch, Forward, ForwardCurrent, GoBack, Guards, Hide,
    HideFromAi, HideOthers, JumpToEnd, JumpToStart, LeaveSearch, NextMatch, NextTab, OnView,
    OpenContainerLogs, OpenContainerShell, OpenLogs, OpenObject, OpenPreviousLogs, OpenSelectedRow,
    OpenSelectorLogs, OpenShell, OpenView, PaneName, PasteText, PrevMatch, PrevTab, ReconnectLogs,
    Refresh, ReloadYamlEdit, RestartShell, RetryConnection, ReviewYamlEdit, SaveLines, SaveLogs,
    SaveMatchingLines, SaveScope, SaveSelectedLines, ScrollPageDown, ScrollPageUp, ScrollToBottom,
    ScrollToTop, SelectAllLines, SelectCluster, SelectFirst, SelectLast, SelectLine, SelectNext,
    SelectNextLine, SelectPrev, SelectPrevLine, SelectRow, SelectText, SetDockHeight,
    SetNamespaces, SetPaneWidth, ShowAll, ShowAllNamespaces, ShowOverview, ShowTabList, ShowYaml,
    SortBy, ToggleAnnotations, ToggleCommandPalette, ToggleLabels, ToggleManagedFields,
    ToggleNamespace, ToggleNavSection, TogglePause, ToggleResourceSwitcher, ToggleSecretValue,
    ToggleSources, ToggleTimestamps, ToggleWide, ToggleWrap, UseDefaultWatchMode, WatchAllKinds,
    WatchOpenViewsOnly,
};
use super::super::registry::{
    BackToList, EditAiScope, Handler, ShowAgentActivity, ShowHelmReleases, ToggleKeepWatching,
};
use super::super::registry::{
    ClearClusterTag, LockCluster, RenameCluster, SetClusterTag, TrustCluster, UnlockCluster,
    UntrustCluster,
};
use super::super::registry::{
    CompareObject, OpenCompare, RefreshCompare, ToggleCompareSame, ToggleCompareVolatile,
};
use super::super::registry::{
    CordonNode, DeleteObject, DrainNode, NamespacePickerNext, NamespacePickerPrevious,
    OpenNamespaceFilter, RestartObject, ScaleObject, ShowDrainReport, UncordonNode,
};
use super::super::resource_actions::{self, ResourceAction};
use super::super::scope_picker::ScopePicker;
use super::super::terminal_view::TerminalView;
use super::super::ui_state;
use super::{
    MainPanes, Pane, Workspace, confirm_expose, confirm_trust, confirm_unlock, expose_dialog,
    scope_dialog,
};

/// A guard: the view `$view` returns (`log`, `shell`, `detail` or `list_wide`) is open and,
/// given `$check`, passes it. It expands to a closure that captures nothing, so it is the plain
/// `fn` the registry's [`Guards`] takes.
macro_rules! view_open {
    ($view:ident) => {
        |ws: &Workspace, cx: &App| $view(ws, cx).is_some()
    };
    ($view:ident, $check:expr) => {
        |ws: &Workspace, cx: &App| $view(ws, cx).is_some_and($check)
    };
}

/// [`Forward`] without the list's Enter: it opens the selected row only when the rows have focus,
/// so with focus in the navigation or the detail pane the action is not available (and Enter
/// propagates), rather than reported as done when nothing opens.
struct NotOnRows<B>(B);

impl<E: InteractiveElement> Bind<ResourceList> for NotOnRows<Forward<E, ResourceList>> {
    fn bind<A: Action>(self, handler: Handler<ResourceList, A>) -> Self {
        if std::any::TypeId::of::<A>() == std::any::TypeId::of::<OpenSelectedRow>() {
            return self;
        }
        Self(self.0.bind(handler))
    }
}

impl Workspace {
    /// Every workspace action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleCommandPalette>(|this, _, window, cx| {
            this.toggle_palette_in(Mode::Actions, window, cx)
        })
        .bind::<ToggleResourceSwitcher>(Self::toggle_switcher)
        .bind::<OpenObject>(Self::open_object)
        .bind::<GoBack>(Self::go_back)
        .bind::<BackToList>(Self::back_to_list)
        .bind::<FocusNextPane>(|this, _, window, cx| this.focus_pane(1, window, cx))
        .bind::<FocusPreviousPane>(|this, _, window, cx| this.focus_pane(-1, window, cx))
        .bind::<About>(Self::about)
        .bind::<SelectCluster>(Self::select_cluster)
        .bind::<ExposeToAi>(Self::expose_to_ai)
        .bind::<HideFromAi>(Self::hide_from_ai)
        .bind::<ToggleKeepWatching>(Self::toggle_keep_watching)
        .bind::<EditAiScope>(Self::edit_ai_scope)
        .bind::<ShowAgentActivity>(Self::show_agent_activity)
        .bind::<ShowHelmReleases>(Self::show_helm_releases)
        .bind::<RefreshHelmReleases>(|ws, _, _, cx| ws.on_helm(cx, HelmView::refresh))
        .bind::<SelectHelmRelease>(|ws, a, _, cx| {
            ws.on_helm(cx, |view, cx| view.select_named(&a.namespace, &a.name, cx));
        })
        .bind::<ShowHelmDetails>(|ws, _, _, cx| {
            ws.on_helm(cx, |view, cx| view.set_pane(HelmPane::Details, cx));
        })
        .bind::<ShowHelmHistory>(|ws, _, _, cx| {
            ws.on_helm(cx, |view, cx| view.set_pane(HelmPane::History, cx));
        })
        .bind::<ShowHelmValues>(|ws, _, _, cx| {
            ws.on_helm(cx, |view, cx| view.set_pane(HelmPane::Values, cx));
        })
        .bind::<ShowHelmManifest>(|ws, _, _, cx| {
            ws.on_helm(cx, |view, cx| view.set_pane(HelmPane::Manifest, cx));
        })
        .bind::<OpenCompare>(Self::open_compare)
        .bind::<CompareObject>(Self::compare_object)
        .bind::<UnlockCluster>(Self::unlock_cluster)
        .bind::<RenameCluster>(Self::rename_cluster)
        .bind::<SetClusterTag>(|this, action, _, cx| {
            this.write_cluster_tag::<SetClusterTag>(
                &action.cluster,
                action.endpoint,
                Some(action.tag),
                cx,
            )
        })
        .bind::<ClearClusterTag>(|this, action, _, cx| {
            this.write_cluster_tag::<ClearClusterTag>(&action.cluster, action.endpoint, None, cx)
        })
        .bind::<LockCluster>(Self::lock_cluster)
        .bind::<TrustCluster>(Self::trust_cluster)
        .bind::<UntrustCluster>(Self::untrust_cluster)
        .bind::<DeleteObject>(|this, _, window, cx| {
            resource_actions::request(this, ResourceAction::Delete, window, cx)
        })
        .bind::<ScaleObject>(|this, _, window, cx| {
            resource_actions::request(this, ResourceAction::Scale, window, cx)
        })
        .bind::<RestartObject>(|this, _, window, cx| {
            resource_actions::request(this, ResourceAction::RolloutRestart, window, cx)
        })
        .bind::<CordonNode>(|this, _, window, cx| {
            resource_actions::request(this, ResourceAction::Cordon, window, cx)
        })
        .bind::<UncordonNode>(|this, _, window, cx| {
            resource_actions::request(this, ResourceAction::Uncordon, window, cx)
        })
        .bind::<DrainNode>(|this, _, window, cx| {
            resource_actions::request(this, ResourceAction::Drain, window, cx)
        })
        .bind::<ShowDrainReport>(|_, _, window, cx| resource_actions::show_drain_report(window, cx))
        .bind::<RetryConnection>(Self::retry_connection)
        .bind::<CancelConnection>(|this, _, _, cx| {
            if this.model.read(cx).can_cancel_connection() {
                this.model.update(cx, |model, _| model.cancel_connection());
            }
        })
        .bind::<WatchAllKinds>(|this, _, _, cx| this.set_watch_mode(true, Some(WatchMode::All), cx))
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
        .bind::<OpenNamespaceFilter>(|this, _, _, cx| {
            this.namespace_popover_open = true;
            cx.notify();
        })
        .bind::<NamespacePickerNext>(|this, _, window, cx| {
            this.move_in_namespace_picker(true, window, cx)
        })
        .bind::<NamespacePickerPrevious>(|this, _, window, cx| {
            this.move_in_namespace_picker(false, window, cx)
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
            ResourceList::bind_actions(NotOnRows(Forward::new(element, self.list.as_ref())))
                .0
                .into_element();
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
        let compare = self.dock.read(cx).active_compare().cloned();
        let element =
            CompareView::bind_actions(Forward::new(element, compare.as_ref())).into_element();
        NavPanel::bind_actions(Forward::new(element, Some(&self.nav))).into_element()
    }

    /// The guards the registry checks before dispatching: when each action is enabled, and
    /// whether its arguments would do anything. The handlers use the same checks.
    pub(super) fn guards(this: WeakEntity<Self>) -> Guards<Self> {
        Guards::new(this)
            // macOS has these in its app menu; Linux desktops have nothing they would do.
            .when::<BackToList>(|ws, cx| ws.main_panes(cx) == MainPanes::DetailAlone)
            .when::<Hide>(|_, _| cfg!(target_os = "macos"))
            .when::<HideOthers>(|_, _| cfg!(target_os = "macos"))
            .when::<ShowAll>(|_, _| cfg!(target_os = "macos"))
            .add::<SelectCluster>(
                |_, _| true,
                |ws, a, cx| ws.check_cluster::<SelectCluster>(&a.cluster, cx),
            )
            .add::<ExposeToAi>(Self::settings_ok, |ws, a, cx| ws.check_expose(a, cx))
            .add::<HideFromAi>(Self::settings_ok, |ws, a, cx| ws.check_hide(a, cx))
            .add::<ToggleKeepWatching>(Self::settings_ok, |ws, a, cx| {
                ws.check_cluster::<ToggleKeepWatching>(&a.cluster, cx)
            })
            .add::<EditAiScope>(Self::settings_ok, |ws, a, cx| {
                ws.scope_change(a, cx).map(drop)
            })
            .add::<ShowAgentActivity>(
                |_, _| true,
                |ws, a, cx| ws.check_cluster::<ShowAgentActivity>(&a.cluster, cx),
            )
            .add::<ShowHelmReleases>(
                |_, _| true,
                |ws, a, cx| ws.check_cluster::<ShowHelmReleases>(&a.cluster, cx),
            )
            .add::<UnlockCluster>(|_, _| true, |ws, a, cx| ws.unlock_target(a, cx).map(drop))
            .add::<RenameCluster>(Self::settings_ok, |ws, a, cx| {
                ws.check_cluster::<RenameCluster>(&a.cluster, cx)
            })
            .add::<SetClusterTag>(Self::settings_ok, |ws, a, cx| {
                ws.tag_server::<SetClusterTag>(&a.cluster, a.endpoint, cx)
                    .map(drop)
            })
            .add::<ClearClusterTag>(Self::settings_ok, |ws, a, cx| {
                ws.tag_server::<ClearClusterTag>(&a.cluster, a.endpoint, cx)
                    .map(drop)
            })
            .add::<LockCluster>(|_, _| true, |ws, a, cx| ws.check_lock(a, cx))
            .add::<TrustCluster>(Self::settings_ok, |ws, a, cx| {
                ws.trust_change(a, cx).map(drop)
            })
            .add::<UntrustCluster>(Self::settings_ok, |ws, a, cx| ws.check_untrust(a, cx))
            .when::<RetryConnection>(|ws, cx| ws.retry_enabled(cx))
            .when::<CancelConnection>(|ws, cx| ws.model.read(cx).can_cancel_connection())
            .when::<WatchAllKinds>(|ws, cx| ws.cluster.is_some() && ws.settings_ok(cx))
            .when::<WatchOpenViewsOnly>(|ws, cx| ws.cluster.is_some() && ws.settings_ok(cx))
            .when::<UseDefaultWatchMode>(|ws, cx| ws.cluster.is_some() && ws.settings_ok(cx))
            .when::<DefaultWatchAllKinds>(Self::settings_ok)
            .when::<DefaultWatchOpenViewsOnly>(Self::settings_ok)
            .add::<OpenView>(
                |ws, _| ws.cluster.is_some() || ws.sets.is_open(),
                |ws, a, cx| ws.check_open_view(a, cx),
            )
            .add::<OpenObject>(
                |ws, _| ws.cluster.is_some(),
                |ws, a, cx| ws.check_open_object(a, cx),
            )
            .when::<GoBack>(|ws, cx| ws.can_go_back(cx))
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
            .when::<OpenNamespaceFilter>(|ws, cx| ws.namespace_filter_shown(cx))
            .when::<NamespacePickerNext>(|ws, _| ws.namespace_popover_open)
            .when::<NamespacePickerPrevious>(|ws, _| ws.namespace_popover_open)
            .add::<SetPaneWidth>(|_, _| true, |ws, a, _| ws.check_pane_width(a))
            .add::<ToggleNavSection>(
                |ws, cx| ws.nav.read(cx).has_sections(),
                |ws, a, cx| ws.nav.read(cx).check_section(a),
            )
            // The list's.
            .when::<SelectPrev>(view_open!(list_wide))
            .when::<SelectNext>(view_open!(list_wide))
            .when::<SelectFirst>(view_open!(list_wide))
            .when::<SelectLast>(view_open!(list_wide))
            .when::<CloseDetail>(view_open!(list_wide))
            .when::<OpenSelectedRow>(view_open!(list_wide))
            .when::<FocusFilter>(view_open!(list_wide))
            .when::<ToggleWide>(|ws, cx| list_wide(ws, cx) == Some(true))
            .add::<SelectRow>(view_open!(list_wide), |ws, a, cx| match &ws.list {
                Some(l) => l.read(cx).check_select_row(a),
                None => Err(ActionError::unavailable::<SelectRow>()),
            })
            .add::<SortBy>(view_open!(list_wide), |ws, a, cx| match &ws.list {
                Some(l) => l.read(cx).check_sort(a),
                None => Err(ActionError::unavailable::<SortBy>()),
            })
            // The detail pane's.
            .when::<Refresh>(view_open!(detail))
            .when::<Close>(view_open!(detail))
            .when::<ShowOverview>(view_open!(detail))
            .when::<ShowYaml>(view_open!(detail))
            .when::<ToggleLabels>(view_open!(detail))
            .when::<ToggleAnnotations>(view_open!(detail))
            .when::<ToggleManagedFields>(view_open!(detail, DetailView::has_managed_fields))
            .when::<EditYaml>(view_open!(detail, DetailView::can_start_edit))
            .when::<ReviewYamlEdit>(view_open!(detail, DetailView::can_review_edit))
            .when::<ApplyYamlEdit>(view_open!(detail, DetailView::can_apply_edit))
            .when::<ReloadYamlEdit>(view_open!(detail, DetailView::can_reload_edit))
            .when::<BackToYamlEdit>(view_open!(detail, DetailView::can_go_back_to_edit))
            .when::<CancelYamlEdit>(view_open!(detail, DetailView::can_cancel_edit))
            .add::<ToggleSecretValue>(view_open!(detail, DetailView::is_secret), |ws, a, cx| {
                match detail(ws, cx) {
                    Some(d) => d.check_secret_key(a),
                    None => Err(ActionError::unavailable::<ToggleSecretValue>()),
                }
            })
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
            // The resource actions act on the selected row, with the row menu's checks.
            .when::<DeleteObject>(|ws, cx| {
                resource_actions::available(ws, ResourceAction::Delete, cx).is_ok()
            })
            .when::<ScaleObject>(|ws, cx| {
                resource_actions::available(ws, ResourceAction::Scale, cx).is_ok()
            })
            .when::<RestartObject>(|ws, cx| {
                resource_actions::available(ws, ResourceAction::RolloutRestart, cx).is_ok()
            })
            .when::<CordonNode>(|ws, cx| {
                resource_actions::available(ws, ResourceAction::Cordon, cx).is_ok()
            })
            .when::<UncordonNode>(|ws, cx| {
                resource_actions::available(ws, ResourceAction::Uncordon, cx).is_ok()
            })
            .when::<DrainNode>(|ws, cx| {
                resource_actions::available(ws, ResourceAction::Drain, cx).is_ok()
            })
            .when::<ShowDrainReport>(|_, cx| resource_actions::has_drain_result(cx))
            .when::<OpenCompare>(|ws, cx| ws.compare_check(cx).is_ok())
            .add::<CompareObject>(
                |_, _| true,
                |ws, a, cx| {
                    compare::can_compare(&a.cluster, ws.known_clusters(cx))
                        .map_err(ActionError::rejected::<CompareObject>)
                },
            )
            .when::<RefreshHelmReleases>(|ws, cx| ws.dock.read(cx).active_helm().is_some())
            .when::<SelectHelmRelease>(|ws, cx| ws.dock.read(cx).active_helm().is_some())
            .when::<ShowHelmDetails>(|ws, cx| ws.dock.read(cx).active_helm().is_some())
            .when::<ShowHelmHistory>(|ws, cx| ws.dock.read(cx).active_helm().is_some())
            .when::<ShowHelmValues>(|ws, cx| ws.dock.read(cx).active_helm().is_some())
            .when::<ShowHelmManifest>(|ws, cx| ws.dock.read(cx).active_helm().is_some())
            .when::<RefreshCompare>(|ws, cx| ws.dock.read(cx).active_compare().is_some())
            .when::<ToggleCompareSame>(|ws, cx| ws.dock.read(cx).active_compare().is_some())
            .when::<ToggleCompareVolatile>(|ws, cx| ws.dock.read(cx).active_compare().is_some())
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
            .when::<ReconnectLogs>(|ws, cx| restartable_here(ws, true, cx))
            // Not on a protected cluster: the tab says why, with Unlock… beside it.
            .when::<RestartShell>(|ws, cx| {
                restartable_here(ws, false, cx)
                    && shell(ws, cx).is_some_and(|s| s.restart_blocked().is_none())
            })
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
            .when::<CopyText>(view_open!(shell, TerminalView::has_selection))
            .when::<PasteText>(view_open!(shell, TerminalView::is_live))
            .when::<ScrollPageUp>(view_open!(shell, TerminalView::can_scroll_back))
            .when::<ScrollToTop>(view_open!(shell, TerminalView::can_scroll_back))
            .when::<ScrollPageDown>(view_open!(shell, TerminalView::is_scrolled_back))
            .when::<ScrollToBottom>(view_open!(shell, TerminalView::is_scrolled_back))
            .add::<SelectText>(view_open!(shell), |ws, a, cx| match shell(ws, cx) {
                Some(t) => t.check_cells(a),
                None => Err(ActionError::unavailable::<SelectText>()),
            })
            // The active log tab's.
            .when::<TogglePause>(view_open!(log, LogView::can_pause))
            .when::<JumpToEnd>(view_open!(log, LogView::can_jump_to_end))
            .when::<JumpToStart>(view_open!(log, LogView::has_lines))
            .when::<ToggleTimestamps>(view_open!(log))
            .when::<ToggleWrap>(view_open!(log))
            .when::<ToggleSources>(view_open!(log, LogView::is_workload))
            .when::<FocusSearch>(view_open!(log))
            .when::<NextMatch>(view_open!(log, LogView::has_matches))
            .when::<PrevMatch>(view_open!(log, LogView::has_matches))
            .when::<SelectPrevLine>(view_open!(log, LogView::has_lines))
            .when::<SelectNextLine>(view_open!(log, LogView::has_lines))
            .when::<ExtendSelectionUp>(view_open!(log, LogView::has_lines))
            .when::<ExtendSelectionDown>(view_open!(log, LogView::has_lines))
            .when::<SelectAllLines>(view_open!(log, LogView::has_lines))
            .when::<ClearSelection>(view_open!(log, LogView::is_selecting))
            .when::<LeaveSearch>(view_open!(log))
            .when::<CopyLines>(view_open!(log, LogView::has_selection))
            .when::<SaveLogs>(view_open!(log, |l| l.check_save(SaveScope::All).is_ok()))
            .when::<SaveMatchingLines>(view_open!(log, |l| l
                .check_save(SaveScope::Matches)
                .is_ok()))
            .when::<SaveSelectedLines>(view_open!(log, |l| l
                .check_save(SaveScope::Selection)
                .is_ok()))
            .add::<SaveLines>(view_open!(log, LogView::has_lines), |ws, a, cx| {
                match log(ws, cx) {
                    Some(l) => l.check_save(a.lines),
                    None => Err(ActionError::unavailable::<SaveLines>()),
                }
            })
            .add::<SelectLine>(view_open!(log, LogView::has_lines), |ws, a, cx| {
                match log(ws, cx) {
                    Some(l) => l.check_line::<SelectLine>(a.line),
                    None => Err(ActionError::unavailable::<SelectLine>()),
                }
            })
            .add::<DragSelection>(
                view_open!(log, LogView::has_selection),
                |ws, a, cx| match log(ws, cx) {
                    Some(l) => l.check_line::<DragSelection>(a.line),
                    None => Err(ActionError::unavailable::<DragSelection>()),
                },
            )
    }

    /// Opens the command palette in `mode` (the actions available where focus is, or the
    /// resource switcher), or closes it when it is open in that mode. Open in the other mode,
    /// it switches, keeping where it came from.
    pub(super) fn toggle_palette_in(
        &mut self,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (previous, contexts) = match self.palette.take() {
            Some((palette, _)) => {
                let (origin, same) = {
                    let palette = palette.read(cx);
                    (palette.origin(window), palette.mode() == mode)
                };
                self._switcher = Task::ready(());
                window.focus(&origin, cx);
                release_unfocused_input(window, cx);
                if same {
                    cx.notify();
                    return;
                }
                // Switching modes: no menu was open over the palette.
                (origin, None)
            }
            None => {
                // An open context menu, drop-down or popover (the namespace filter) is drawn
                // over the palette and could take a click meant for it: dismiss it first, as
                // Esc would. One that knows where focus was gives it back, and the palette runs
                // its actions from there; else (focus still on the closed menu) from the
                // workspace.
                let mut previous = window.focused(cx).unwrap_or_else(|| self.focus.clone());
                let mut contexts = window.context_stack();
                if contexts
                    .iter()
                    .any(|c| c.contains(POPUP_MENU) || c.contains(POPOVER_CONTENT))
                {
                    previous.dispatch_action(&Cancel, window, cx);
                    match window.focused(cx) {
                        Some(back) if back != previous => {
                            previous = back;
                            contexts = window.context_stack();
                        }
                        _ => {
                            previous = self.focus.clone();
                            let end = contexts
                                .iter()
                                .position(|c| c.contains(super::super::registry::WORKSPACE))
                                .map_or(0, |i| i + 1);
                            contexts.truncate(end);
                        }
                    }
                }
                (previous, Some(contexts))
            }
        };
        // A popover open while focus is elsewhere (the namespace filter's, after Focus Filter)
        // closes by its open state.
        self.namespace_popover_open = false;
        let palette = match mode {
            Mode::Actions => {
                // Read while `previous` still has focus: the palette takes it next.
                let contexts = contexts.unwrap_or_else(|| window.context_stack());
                let this = cx.weak_entity();
                let choices: ChoiceSource = Rc::new(move |spec, arg, cx| match this.upgrade() {
                    Some(ws) => ws.read(cx).palette_choices(spec, arg, cx),
                    None => ArgInput::Text,
                });
                let fallback = self.focus.clone();
                cx.new(|cx| CommandPalette::new(previous, fallback, contexts, choices, window, cx))
            }
            Mode::Resources => self.new_switcher(previous, window, cx),
        };
        let dismissed = cx.subscribe_in(
            &palette,
            window,
            |this, _, event: &PaletteEvent, window, cx| match event {
                PaletteEvent::Dismissed => {
                    this.palette = None;
                    this._switcher = Task::ready(());
                    // Focus went back (or elsewhere): the palette's field must not stay the
                    // window's focused one.
                    release_unfocused_input(window, cx);
                    cx.notify();
                }
            },
        );
        // An open step's choices follow the clusters, discovery, the namespaces and the shown
        // pod; nothing new is fetched for it.
        let model = self.model.clone();
        let workspace = cx.entity();
        let shown = self.detail.clone();
        palette.update(cx, |palette, cx| {
            palette.watch(&model, cx);
            palette.watch(&workspace, cx);
            if let Some(detail) = &shown {
                palette.watch(detail, cx);
            }
        });
        self.palette = Some((palette.clone(), dismissed));
        if mode == Mode::Actions {
            // Listing reads the guards, which read this workspace: once this update is done.
            window.defer(cx, move |window, cx| {
                palette.update(cx, |palette, cx| palette.load(window, cx))
            });
        }
        cx.notify();
    }

    /// What the palette offers for an argument of an action: the values it can take now, or
    /// text to type.
    pub(in crate::ui) fn palette_choices(
        &self,
        spec: &ActionSpec,
        arg: &Arg,
        cx: &App,
    ) -> ArgInput {
        let model = self.model.read(cx);
        let choices = |items: Vec<Choice>| ArgInput::Choices { items, free: false };
        match arg.kind {
            ArgKind::Cluster => choices({
                let names = model.names();
                // Clearing is offered only where there is a tag to clear.
                let clearing = spec.name == ClearClusterTag::name_for_type();
                model
                    .clusters
                    .iter()
                    .filter(|c| !clearing || model.cluster_tag(c).is_some())
                    .map(|c| {
                        let name = c.info.name.clone();
                        let short = names.of(&name);
                        Choice {
                            detail: (short != name).then(|| name.clone()),
                            label: short,
                            value: name,
                        }
                    })
                    .collect()
            }),
            // Also one the user may not list (Add Namespace to Filter).
            ArgKind::Namespace => ArgInput::Choices {
                items: self.namespaces.iter().map(Choice::plain).collect(),
                free: true,
            },
            ArgKind::Kind => {
                let sections = model
                    .active
                    .as_ref()
                    .and_then(|c| model.discovered(c))
                    .and_then(|d| d.discovery.as_ref())
                    .map(|d| d.navigation.sections.as_slice())
                    .unwrap_or_default();
                choices(
                    sections
                        .iter()
                        .flat_map(|s| &s.entries)
                        .filter(|e| e.listable)
                        .map(|e| Choice {
                            value: e.id.to_string(),
                            label: e.label.clone(),
                            detail: Some(e.id.to_string()),
                        })
                        .collect(),
                )
            }
            ArgKind::Column => choices(
                self.list
                    .as_ref()
                    .map(|l| l.read(cx).drawn_column_names())
                    .unwrap_or_default()
                    .into_iter()
                    .map(Choice::plain)
                    .collect(),
            ),
            // A shell opens only in a running app container of the pod a shell would use (a
            // workload's first ready pod), as the detail pane's picker offers.
            ArgKind::Container if spec.name == OpenContainerShell::name_for_type() => {
                let target = detail(self, cx).and_then(|d| d.shell_pod());
                choices(
                    target
                        .map(|pod| {
                            pod.containers
                                .iter()
                                .filter(|c| !pod.not_running.contains(c))
                                .map(|c| Choice {
                                    value: c.clone(),
                                    label: c.clone(),
                                    detail: (*c == pod.default).then(|| "default".to_owned()),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                )
            }
            ArgKind::Container => choices(
                detail(self, cx)
                    .map(|d| d.log_containers())
                    .unwrap_or_default()
                    .iter()
                    .map(|c| Choice {
                        value: c.name.clone(),
                        label: c.name.clone(),
                        detail: c.init.then(|| "init".to_owned()),
                    })
                    .collect(),
            ),
            ArgKind::Choice(words) => choices(words.iter().copied().map(Choice::plain).collect()),
            ArgKind::Bool => choices(vec![
                Choice {
                    value: "true".into(),
                    label: "Yes".into(),
                    detail: None,
                },
                Choice {
                    value: "false".into(),
                    label: "No".into(),
                    detail: None,
                },
            ]),
            ArgKind::Text | ArgKind::Number | ArgKind::List => ArgInput::Text,
        }
    }

    /// About Flint: its version, in a dialog with only OK.
    fn about(&mut self, _: &About, window: &mut Window, cx: &mut Context<Self>) {
        confirm::open(window, cx, about_dialog(), |_, _| {});
    }

    /// Gives focus back after it left with a closed view (or a hidden dock): to the open list,
    /// or the workspace.
    pub(super) fn reclaim_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The picker's focused row left it (a namespace unchecked and not listed): it keeps focus,
        // and hands it to its first control as it draws. After this draw, since a focus change
        // during one schedules no frame.
        if self.namespace_popover_open && self.namespace_filter_shown(cx) {
            let lost = window.focused(cx);
            cx.defer_in(window, move |this, window, cx| {
                if window.focused(cx) != lost {
                    return;
                }
                if this.namespace_popover_open {
                    window.focus(&this.namespace_picker_focus, cx);
                    release_unfocused_input(window, cx);
                } else {
                    this.reclaim_focus(window, cx);
                }
            });
            return;
        }
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

    pub(in crate::ui) fn cluster_summary<'a>(
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

    /// Whether `cluster` is protected (ADR-0015): Shell (and every later action that changes
    /// the cluster) is refused there, saying so. A context Flint does not know is protected.
    pub(in crate::ui) fn is_protected(&self, cluster: &str, cx: &App) -> bool {
        !self.model.read(cx).protection_of(cluster).allows_changes()
    }

    /// The context `cluster`, with its server: what unlocking and trusting pin.
    fn pinnable<A: Action>(&self, cluster: &str, cx: &App) -> Result<String, ActionError> {
        self.check_cluster::<A>(cluster, cx)?;
        let summary = self.cluster_summary(cluster, cx).expect("checked");
        summary
            .info
            .server
            .clone()
            .ok_or_else(|| ActionError::rejected::<A>("the context has no API server"))
    }

    /// [`UnlockCluster`]: a protected context with a server. Returns the server.
    fn unlock_target(&self, action: &UnlockCluster, cx: &App) -> Result<String, ActionError> {
        let server = self.pinnable::<UnlockCluster>(&action.cluster, cx)?;
        let why = match self.model.read(cx).protection_of(&action.cluster) {
            Protection::Protected => return Ok(server),
            Protection::Local => "the cluster is a known local cluster, so it is not protected",
            Protection::Unlocked => "the cluster is unlocked already",
            Protection::Trusted => "the cluster is trusted, so it is not protected",
        };
        Err(ActionError::rejected::<UnlockCluster>(why))
    }

    /// [`LockCluster`]: a cluster unlocked for this session.
    fn check_lock(&self, action: &LockCluster, cx: &App) -> Result<(), ActionError> {
        self.check_cluster::<LockCluster>(&action.cluster, cx)?;
        match self.model.read(cx).protection_of(&action.cluster) {
            Protection::Unlocked => Ok(()),
            Protection::Trusted => Err(ActionError::rejected::<LockCluster>(
                "the cluster is trusted; stop trusting it to protect it",
            )),
            _ => Err(ActionError::rejected::<LockCluster>(
                "the cluster is not unlocked for this session",
            )),
        }
    }

    /// [`TrustCluster`]: a context with a server, not trusted already. Returns the server and
    /// the one trusted before, if the context moved.
    fn trust_change(
        &self,
        action: &TrustCluster,
        cx: &App,
    ) -> Result<(String, Option<String>), ActionError> {
        if !self.settings_ok(cx) {
            return Err(ActionError::rejected::<TrustCluster>(
                "settings.toml is invalid",
            ));
        }
        let server = self.pinnable::<TrustCluster>(&action.cluster, cx)?;
        let model = self.model.read(cx);
        let summary = self.cluster_summary(&action.cluster, cx).expect("checked");
        if model.protection(summary) == Protection::Local {
            return Err(ActionError::rejected::<TrustCluster>(
                "the cluster is a known local cluster, so it is not protected",
            ));
        }
        match model.trust(summary) {
            // A pin whose removal failed after Stop Trusting does not count: trusting again
            // (after the confirmation) is how to make it count.
            TrustState::Trusted if model.protection(summary) == Protection::Trusted => Err(
                ActionError::rejected::<TrustCluster>("the cluster is trusted already"),
            ),
            TrustState::Trusted | TrustState::NotTrusted => Ok((server, None)),
            TrustState::ServerChanged { trusted_server } => Ok((server, Some(trusted_server))),
        }
    }

    /// [`UntrustCluster`]: a trusted context (or a stale trust).
    fn check_untrust(&self, action: &UntrustCluster, cx: &App) -> Result<(), ActionError> {
        if !self.settings_ok(cx) {
            return Err(ActionError::rejected::<UntrustCluster>(
                "settings.toml is invalid",
            ));
        }
        self.check_cluster::<UntrustCluster>(&action.cluster, cx)?;
        let summary = self.cluster_summary(&action.cluster, cx).expect("checked");
        match self.model.read(cx).trust(summary) {
            TrustState::NotTrusted => Err(ActionError::rejected::<UntrustCluster>(
                "the cluster is not trusted",
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
            .and_then(|c| model.discovered(c))
            .is_some_and(|d| d.error.is_some() && d.discovery.is_none());
        failed || discovery_failed
    }

    fn check_open_view(&self, action: &OpenView, cx: &App) -> Result<(), ActionError> {
        self.check_listed_on::<OpenView>(action.cluster.as_deref(), action.server.as_deref(), cx)?;
        match self.view_entry(&action.kind, cx) {
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

    /// Why log and shell sessions cannot open on `cluster` now (it is paused or not connected),
    /// if so. A cluster still connecting may: the object to act on is loading too.
    pub(in crate::ui) fn sessions_blocked(&self, cluster: &str, cx: &App) -> Option<String> {
        let summary = self.cluster_summary(cluster, cx);
        if summary.is_some_and(|c| self.model.read(cx).sessions_paused(c)) {
            return Some(format!(
                "{cluster} is waiting for its kubeconfig to be confirmed"
            ));
        }
        match summary.map(|c| &c.state) {
            Some(ConnectionState::Connected(_) | ConnectionState::Connecting) => None,
            // Already a short, safe summary (`ClusterError::summary`).
            Some(ConnectionState::Failed(error)) => {
                Some(format!("{cluster} is not connected: {error}"))
            }
            Some(ConnectionState::AwaitingConfirmation) => Some(format!(
                "{cluster} is waiting for its kubeconfig to be confirmed"
            )),
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

    pub(super) fn select_cluster(
        &mut self,
        action: &SelectCluster,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.check_cluster::<SelectCluster>(&action.cluster, cx) {
            Ok(()) => {
                let cluster = action.cluster.clone();
                // The sidebar's cursor follows the cluster picked, however it was picked.
                self.sidebar_cursor = Some(cluster.clone());
                self.model.update(cx, |m, _| m.activate(cluster));
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    /// Turns keeping a cluster watched off, or on. This never lets a sign-in command run, which
    /// takes the cluster's background sign-in toggle. The core words any refusal.
    fn toggle_keep_watching(
        &mut self,
        action: &ToggleKeepWatching,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cluster = action.cluster.clone();
        let Some(view) = self.model.read(cx).keep_watching(&cluster).cloned() else {
            return;
        };
        if !keep_watching::turns_on(view.status, view.sign_in) {
            self.model
                .update(cx, |m, _| m.set_keep_watching(cluster, None));
        } else if let Some(definition) = view.definition {
            self.model
                .update(cx, |m, _| m.set_keep_watching(cluster, Some(definition)));
        }
    }

    /// Opens the confirmation naming the context and its server (and the previous server, when
    /// the context moved); confirming exposes it.
    fn expose_to_ai(&mut self, action: &ExposeToAi, window: &mut Window, cx: &mut Context<Self>) {
        match self.exposure_change(action, cx) {
            Ok((server, previous)) => {
                let model = self.model.clone();
                let dialog = expose_dialog(&action.cluster, &server, previous.as_deref());
                // A re-pointed context starts from what agents could read at its old server, so
                // exposing it again never widens it unseen.
                let saved = previous
                    .and_then(|_| self.model.read(cx).settings.ai_scope_saved(&action.cluster))
                    .unwrap_or_default();
                let picker = self.scope_picker(&action.cluster, &saved, window, cx);
                let target = (action.cluster.clone(), server);
                confirm_expose(
                    window,
                    cx,
                    model,
                    target,
                    dialog,
                    picker,
                    ClustersModel::expose_to_ai,
                );
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    /// [`EditAiScope`]: an exposed context, at the server it points at now. Returns the server
    /// and the scope it has.
    fn scope_change(
        &self,
        action: &EditAiScope,
        cx: &App,
    ) -> Result<(String, AiScope), ActionError> {
        if !self.settings_ok(cx) {
            return Err(ActionError::rejected::<EditAiScope>(
                "settings.toml is invalid",
            ));
        }
        self.check_cluster::<EditAiScope>(&action.cluster, cx)?;
        let cluster = self.cluster_summary(&action.cluster, cx).expect("checked");
        let model = self.model.read(cx);
        match (model.ai_scope(cluster), cluster.info.server.clone()) {
            (Some(scope), Some(server)) => Ok((server, scope)),
            _ => Err(ActionError::rejected::<EditAiScope>(
                "the cluster is not exposed to AI agents",
            )),
        }
    }

    /// Opens (or shows) the cluster's agent activity tab, focused (UP-16). Any context has one:
    /// a cluster hidden from agents again still shows what they read while it was exposed.
    fn show_agent_activity(
        &mut self,
        action: &ShowAgentActivity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_cluster::<ShowAgentActivity>(&action.cluster, cx) {
            tracing::warn!("{e}");
            return;
        }
        let model = self.model.clone();
        let cluster = action.cluster.clone();
        let key = TabKey::Activity(cluster.clone());
        let tab_cluster = self.tab_cluster(&key, cx);
        let server = tab_cluster.server.clone();
        self.open_tab(
            key,
            tab_cluster,
            move |_, cx| {
                DockTab::Activity(cx.new(|cx| ActivityView::new(model, cluster, server, cx)))
            },
            true,
            window,
            cx,
        );
    }

    /// Runs `apply` on the Helm releases tab shown.
    fn on_helm(
        &mut self,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut HelmView, &mut Context<HelmView>),
    ) {
        if let Some(view) = self.dock.read(cx).active_helm().cloned() {
            view.update(cx, apply);
        }
    }

    /// Opens (or shows) the cluster's Helm releases tab, focused. Strictly read-only.
    fn show_helm_releases(
        &mut self,
        action: &ShowHelmReleases,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_cluster::<ShowHelmReleases>(&action.cluster, cx) {
            tracing::warn!("{e}");
            return;
        }
        let core = self.model.read(cx).core().clone();
        let cluster = action.cluster.clone();
        let key = TabKey::Helm(cluster.clone());
        let tab_cluster = self.tab_cluster(&key, cx);
        self.open_tab(
            key,
            tab_cluster,
            move |_, cx| DockTab::Helm(cx.new(|cx| HelmView::new(core, cluster, cx))),
            true,
            window,
            cx,
        );
    }

    /// Whether the selected object can be compared across clusters now: `Err` says why not.
    pub(in crate::ui) fn compare_check(&self, cx: &App) -> Result<(), String> {
        let detail = self.detail_view().ok_or("no object is selected")?;
        let origin = &detail.read(cx).object().cluster;
        compare::can_compare(origin, &self.model.read(cx).clusters)
    }

    /// Opens (or shows) the selected object's compare tab, focused. Strictly read-only.
    fn open_compare(&mut self, _: &OpenCompare, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(why) = self.compare_check(cx) {
            self.refuse(compare::refused(&why), cx);
            return;
        }
        let Some(detail) = self.detail_view() else {
            return;
        };
        let object = detail.read(cx).object().clone();
        let target = CompareTarget {
            kind: object.kind,
            namespace: object.namespace,
            name: object.name,
        };
        self.open_compare_of(object.cluster, target, window, cx);
    }

    /// Opens (or shows) the compare tab of the exact object a row's menu was opened on.
    fn compare_object(&mut self, a: &CompareObject, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(why) = compare::can_compare(&a.cluster, self.known_clusters(cx)) {
            self.refuse(compare::refused(&why), cx);
            return;
        }
        self.open_compare_of(a.cluster.clone(), a.target(), window, cx);
    }

    fn open_compare_of(
        &mut self,
        origin: String,
        target: CompareTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let clusters = compare::compare_clusters(&origin, self.known_clusters(cx));
        let key = TabKey::Compare(origin.clone(), target.clone());
        let tab_cluster = self.tab_cluster(&key, cx);
        let model = self.model.clone();
        let columns = clusters.clone();
        let id = self.open_tab(
            key,
            tab_cluster,
            move |_, cx| {
                DockTab::Compare(cx.new(|cx| CompareView::new(model, origin, target, columns, cx)))
            },
            true,
            window,
            cx,
        );
        // A tab already open may hold a column whose cluster has changed since.
        if let Some(DockTab::Compare(view)) = self.dock.read(cx).tab(id) {
            let view = view.clone();
            view.update(cx, |view, cx| view.reopen(clusters, cx));
        }
    }

    /// Every cluster in kubeconfig, as last reported.
    pub(in crate::ui) fn known_clusters<'a>(&self, cx: &'a App) -> &'a [ClusterSummary] {
        &self.model.read(cx).clusters
    }

    /// Opens the scope dialog of an exposed cluster; saving rewrites its entry with the new
    /// scope.
    fn edit_ai_scope(&mut self, action: &EditAiScope, window: &mut Window, cx: &mut Context<Self>) {
        match self.scope_change(action, cx) {
            Ok((server, scope)) => {
                let model = self.model.clone();
                let dialog = scope_dialog(&action.cluster, &server, &scope);
                let picker = self.scope_picker(&action.cluster, &scope, window, cx);
                // Saved only if the scope on disk is still the one shown.
                let from = scope.clone();
                let target = (action.cluster.clone(), server);
                // Changes the scope only: never exposes a cluster hidden meanwhile.
                confirm_expose(
                    window,
                    cx,
                    model,
                    target,
                    dialog,
                    picker,
                    move |m: &mut ClustersModel, cluster, server, scope| {
                        m.set_ai_scope(cluster, server, from.clone(), scope)
                    },
                );
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    /// The scope pickers for `cluster`, with `scope` checked: its kinds from discovery, and its
    /// namespaces when it is the active cluster (whose namespaces are watched).
    fn scope_picker(
        &self,
        cluster: &str,
        scope: &AiScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ScopePicker> {
        let kinds = self
            .model
            .read(cx)
            .discovery
            .get(cluster)
            .and_then(|d| d.discovery.clone());
        let namespaces = if self.cluster.as_deref() == Some(cluster) {
            self.namespaces.clone()
        } else {
            Vec::new()
        };
        let scope = scope.clone();
        cx.new(|cx| {
            let kinds = kinds.as_ref().map_or(&[][..], |d| d.kinds.as_slice());
            ScopePicker::new(&namespaces, kinds, &scope, window, cx)
        })
    }

    /// Opens the confirmation naming the context and its server; confirming unlocks it until
    /// Flint quits.
    fn unlock_cluster(
        &mut self,
        action: &UnlockCluster,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.unlock_target(action, cx) {
            Ok(server) => {
                let model = self.model.clone();
                let shown = self.cluster_names(cx).with_real(&action.cluster);
                confirm_unlock(window, cx, model, &shown, action.cluster.clone(), server);
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    /// The core, for the resource actions' reviews and writes.
    pub(in crate::ui) fn core<'a>(&self, cx: &'a App) -> &'a flint_core::CoreHandle {
        self.model.read(cx).core()
    }

    /// Whether `cluster`'s discovery says `kind` can be deleted. Not known yet: no.
    pub(in crate::ui) fn kind_deletable(
        &self,
        cluster: &str,
        kind: &flint_core::KindId,
        cx: &App,
    ) -> bool {
        self.model
            .read(cx)
            .discovered(cluster)
            .and_then(|d| d.discovery.as_ref())
            .and_then(|d| d.kind(kind))
            .is_some_and(|k| k.supports("delete"))
    }

    /// Holds the review being read for a resource action; a newer one replaces it.
    pub(in crate::ui) fn set_write_review(&mut self, review: Task<()>) {
        self._write_review = review;
    }

    /// What each cluster is called on screen.
    pub(in crate::ui) fn cluster_names(&self, cx: &App) -> flint_core::display::ClusterNames {
        self.model.read(cx).names()
    }

    /// Opens the dialog that names `action.cluster`, showing its context and server.
    fn rename_cluster(
        &mut self,
        action: &RenameCluster,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_cluster::<RenameCluster>(&action.cluster, cx) {
            tracing::warn!("{e}");
            return;
        }
        let server = self
            .cluster_summary(&action.cluster, cx)
            .and_then(|c| c.info.server.clone());
        let model = self.model.clone();
        super::super::cluster_name::open(window, cx, model, action.cluster.clone(), server);
    }

    /// The server a colour tag for `cluster` is saved at: where it points now. Refused for a
    /// context with no server, or one that moved since `endpoint` was read.
    fn tag_server<A: Action>(
        &self,
        cluster: &str,
        endpoint: Option<u64>,
        cx: &App,
    ) -> Result<String, ActionError> {
        self.check_cluster::<A>(cluster, cx)?;
        let summary = self.cluster_summary(cluster, cx).expect("checked");
        if endpoint.is_some_and(|e| e != summary.endpoint) {
            return Err(ActionError::rejected::<A>(format!(
                "{cluster} now points at a different server"
            )));
        }
        summary
            .info
            .server
            .clone()
            .ok_or_else(|| ActionError::rejected::<A>(format!("{cluster} has no API server")))
    }

    /// The server the sidebar shows for `cluster` now, which a tag menu is drawn for.
    pub(in crate::ui) fn tag_menu_server(&self, cluster: &str, cx: &App) -> Option<String> {
        self.cluster_summary(cluster, cx)?.info.server.clone()
    }

    /// Sends the tag to the core, which writes it in order with every other settings change.
    fn write_cluster_tag<A: Action>(
        &mut self,
        cluster: &str,
        endpoint: Option<u64>,
        tag: Option<flint_core::settings::ClusterTag>,
        cx: &mut Context<Self>,
    ) {
        match self.tag_server::<A>(cluster, endpoint, cx) {
            Ok(server) => {
                let cluster = cluster.to_owned();
                self.model
                    .update(cx, |m, _| m.set_cluster_tag(cluster, server, tag));
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    fn lock_cluster(&mut self, action: &LockCluster, _: &mut Window, cx: &mut Context<Self>) {
        match self.check_lock(action, cx) {
            Ok(()) => self.model.update(cx, |m, cx| m.lock(&action.cluster, cx)),
            Err(e) => tracing::warn!("{e}"),
        }
    }

    /// Opens the confirmation naming the context and its server (and the previous server, when
    /// the context moved); confirming trusts it.
    fn trust_cluster(
        &mut self,
        action: &TrustCluster,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.trust_change(action, cx) {
            Ok((server, previous)) => {
                let model = self.model.clone();
                let shown = self.cluster_names(cx).with_real(&action.cluster);
                confirm_trust(
                    window,
                    cx,
                    model,
                    &shown,
                    action.cluster.clone(),
                    server,
                    previous,
                );
            }
            Err(e) => tracing::warn!("{e}"),
        }
    }

    fn untrust_cluster(&mut self, action: &UntrustCluster, _: &mut Window, cx: &mut Context<Self>) {
        match self.check_untrust(action, cx) {
            Ok(()) => {
                let cluster = action.cluster.clone();
                self.model
                    .update(cx, |m, cx| m.untrust_cluster(cluster, cx));
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
    pub(super) fn open_view(
        &mut self,
        action: &OpenView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_listed_on::<OpenView>(
            action.cluster.as_deref(),
            action.server.as_deref(),
            cx,
        ) {
            tracing::warn!("{e}");
            return;
        }
        match self.view_entry(&action.kind, cx) {
            Some(entry) => self.open_kind(&entry, window, cx),
            None => tracing::warn!("{}", self.check_open_view(action, cx).unwrap_err()),
        }
    }

    /// The entry a view of `kind` opens: from what the open set serves, and nothing else while a
    /// set is open, or from the active cluster.
    pub(in crate::ui) fn view_entry(&self, kind: &str, cx: &App) -> Option<NavEntry> {
        match self.nav.read(cx).set_kinds() {
            Some(set) => set.entry(kind),
            None => self.nav_entry(kind, cx),
        }
    }

    /// The active cluster's navigation entry for `kind`, named as `kubectl get` takes it (Kind,
    /// plural, singular or short name, optionally `.group`), through the core resolver.
    pub(in crate::ui) fn nav_entry(&self, kind: &str, cx: &App) -> Option<NavEntry> {
        let model = self.model.read(cx);
        let active = model.active.as_ref()?;
        let discovery = model.discovered(active)?.discovery.as_ref()?;
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

/// Whether the dock's shown tab can start its log (`logs`) or shell session again. Not while its
/// cluster is paused; one that is only disconnected stays available, and running it says why.
fn restartable_here(ws: &Workspace, logs: bool, cx: &App) -> bool {
    let dock = ws.dock.read(cx);
    dock.restartable(logs, cx)
        .and_then(|id| dock.tab(id))
        .is_some_and(|tab| {
            let key = tab.item(cx).tab_key();
            !ws.cluster_summary(key.context(), cx).is_some_and(|c| {
                c.state == ConnectionState::AwaitingConfirmation
                    || ws.model.read(cx).sessions_paused(c)
            })
        })
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

/// The key context gpui-component's context menus and drop-downs draw their entries in.
const POPUP_MENU: &str = "PopupMenu";
/// The key context of an open popover's content (the namespace filter's), which Cancel
/// dismisses, giving focus back.
const POPOVER_CONTENT: &str = "PopoverContent";

/// What About Flint says.
pub(in crate::ui) fn about_dialog() -> DialogText {
    DialogText {
        title: "Flint".into(),
        text: format!(
            "Version {}. A native Kubernetes desktop manager.",
            env!("CARGO_PKG_VERSION")
        ),
        ok: "OK".into(),
        cancel: String::new(),
        id: "about-text",
        default_cancel: false,
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
        assert_binds_scope(Scope::Sidebar, super::Workspace::bind_sidebar_actions);
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
#[path = "activity_tests.rs"]
mod activity_tests;
#[cfg(test)]
#[path = "scope_tests.rs"]
mod scope_tests;

#[cfg(test)]
#[path = "protect_tests.rs"]
mod protect_tests;

#[cfg(test)]
#[path = "row_actions_tests.rs"]
mod row_actions_tests;

#[cfg(test)]
#[path = "resource_actions_tests.rs"]
pub(super) mod resource_actions_tests;

#[cfg(test)]
#[path = "write_confirm_tests.rs"]
mod write_confirm_tests;

#[cfg(test)]
#[path = "dock_state_tests.rs"]
mod dock_state_tests;
