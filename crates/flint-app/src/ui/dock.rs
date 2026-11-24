//! The bottom dock (#13): a resizable panel under the main area with one tab per session that
//! outlives navigation (UP-42): log tabs, shell tabs (#14) and a cluster's agent activity
//! (UP-16); AI log analysis (#56) next. Tabs
//! keep running when the user switches clusters or views, and end only when closed. Each tab
//! names the cluster its session runs on and is pinned to it: the context and the API server it
//! pointed at ([`TabCluster`]). The strip scrolls the active tab into view, and a tab list at its
//! end names every tab. The dock's height is kept in `state.toml` (`[panes] dock`); its tabs are
//! not, since their sessions end with the app.
//!
//! # Adding a tab kind
//!
//! 1. Implement [`DockItem`] for the tab's view: its [`TabKey`] (so opening the same thing again
//!    shows the existing tab while it answers for it, [`dock_model::reopens`]), title, tooltip,
//!    [`TabStatus`] and focus handle. The view notifies when its title or status changes; the dock redraws the
//!    tab bar then.
//! 2. Add a [`DockTab`] variant holding the view's entity and a [`TabKey`] variant, and fill in
//!    the `match`es in this file ([`DockTab::item`], [`DockTab::view`], `observe`), and the
//!    workspace's `restart_tab` if its session can start again.
//! 3. Open it with [`Dock::open`] from wherever the user asks for it (the workspace opens log
//!    tabs on the detail pane's `OpenLogs`).
//! 4. Give its controls a registry scope and a `bind_actions`, and have the workspace forward
//!    them to the active tab of that kind (see `Workspace::register_actions`, which does it for
//!    [`Dock::active_log`] and [`Dock::active_terminal`]) and guard them, as for any view.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use flint_core::ClusterId;
use flint_core::compare::CompareTarget;

use gpui_kit::base::{TestSupportExt as _, TestTextExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::activity_view::ActivityView;
use super::compare::CompareView;
pub use super::dock_model::TabStatus;
use super::dock_model::{self, LiveSessions, TabCluster, chip_labels, chip_tooltip};
use super::focus::release_unfocused_input;
use super::helm_view::HelmView;
use super::log_view::{LogRequest, LogView};
use super::menu_fit;
use super::menu_record;
use super::registry::{
    self, ActionError, ActivateTab, Bind, CloseActiveTab, CloseTab, NextTab, OnView, PrevTab,
    ReconnectLogs, RestartShell, ShowTabList, ToggleDock,
};
use super::terminal_model::ShellRequest;
use super::terminal_view::TerminalView;

/// The dock's height before the user drags it.
pub const DEFAULT_HEIGHT: f32 = 280.;
/// The lowest the dock gets: its tab bar, a toolbar and a few lines.
pub const MIN_HEIGHT: f32 = 120.;
/// The dock leaves the main area at least this much: its headers and several list rows.
const MIN_MAIN_HEIGHT: f32 = 280.;
/// The same under the two-row header, whose rows, detail title and tabs, and a failed fetch's
/// message with its Retry button need 256.
const MIN_MAIN_HEIGHT_TWO_ROW_HEADER: f32 = MIN_MAIN_HEIGHT;
/// What a tab whose context left kubeconfig says of it.
const REMOVED: &str = "This context is no longer in kubeconfig.";
/// What an empty dock says.
const EMPTY: &str = "No logs or shells open.";
/// The tab bar's height.
const TAB_BAR_HEIGHT: f32 = 28.;

/// The dock's height in a window `window_height` tall: as saved (or the default), within
/// [`MIN_HEIGHT`] and what leaves the main area [`MIN_MAIN_HEIGHT`], or
/// [`MIN_MAIN_HEIGHT_TWO_ROW_HEADER`] under a two-row header. The saved height is untouched, so
/// a taller window or another layout gets it back.
pub fn dock_height(saved: Option<u32>, window_height: f32, two_row_header: bool) -> f32 {
    let min_main = if two_row_header {
        MIN_MAIN_HEIGHT_TWO_ROW_HEADER
    } else {
        MIN_MAIN_HEIGHT
    };
    let max = (window_height - min_main).max(MIN_HEIGHT);
    saved
        .map_or(DEFAULT_HEIGHT, |h| h as f32)
        .clamp(MIN_HEIGHT, max)
}

/// What a tab's session reads or runs. With the tab's [`TabCluster`] it identifies the tab:
/// opening a request on the same cluster shows the open tab while it answers for it
/// ([`dock_model::reopens`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TabKey {
    Logs(LogRequest),
    Shell(ShellRequest),
    /// A cluster's agent activity: no session, one tab per cluster.
    Activity(ClusterId),
    /// An object compared across clusters, opened from this context: no session.
    Compare(ClusterId, CompareTarget),
    /// A cluster's Helm releases: one tab per cluster and definition.
    Helm(ClusterId),
}

impl TabKey {
    /// The context the session runs on.
    pub fn context(&self) -> &str {
        match self {
            TabKey::Logs(request) => &request.cluster,
            TabKey::Shell(request) => &request.cluster,
            TabKey::Activity(cluster) | TabKey::Compare(cluster, _) | TabKey::Helm(cluster) => {
                cluster
            }
        }
    }
}

/// What the dock needs from a tab's view.
pub trait DockItem {
    fn tab_key(&self) -> TabKey;
    fn tab_title(&self) -> SharedString;
    fn tab_tooltip(&self) -> SharedString;
    fn tab_status(&self) -> TabStatus;
    /// The status as the tab list says it: what the tab itself shows, if it says more.
    fn tab_status_text(&self) -> String {
        self.tab_status().word().to_owned()
    }
    /// Focused when the tab is shown.
    fn tab_focus(&self) -> FocusHandle;
    /// Whether Reconnect Logs or Restart Shell can start its session again: once it ended or
    /// failed.
    fn can_restart(&self) -> bool {
        matches!(self.tab_status(), TabStatus::Ended | TabStatus::Failed)
    }
}

/// A tab's view, by kind.
#[derive(Clone)]
pub enum DockTab {
    Logs(Entity<LogView>),
    Shell(Entity<TerminalView>),
    Activity(Entity<ActivityView>),
    Compare(Entity<CompareView>),
    Helm(Entity<HelmView>),
}

impl DockTab {
    pub fn item<'a>(&self, cx: &'a App) -> &'a dyn DockItem {
        match self {
            DockTab::Logs(view) => view.read(cx),
            DockTab::Shell(view) => view.read(cx),
            DockTab::Activity(view) => view.read(cx),
            DockTab::Compare(view) => view.read(cx),
            DockTab::Helm(view) => view.read(cx),
        }
    }

    fn view(&self) -> AnyView {
        match self {
            DockTab::Logs(view) => view.clone().into(),
            DockTab::Shell(view) => view.clone().into(),
            DockTab::Activity(view) => view.clone().into(),
            DockTab::Compare(view) => view.clone().into(),
            DockTab::Helm(view) => view.clone().into(),
        }
    }
}

struct Tab {
    id: u64,
    tab: DockTab,
    /// The cluster its session runs on: part of its identity, and drawn beside its title.
    cluster: TabCluster,
    /// Its context was re-pointed at another server since it opened.
    repointed: bool,
    /// The title and status drawn, so the tab bar redraws only when they change, not for every
    /// snapshot of a chatty session.
    shown: (SharedString, TabStatus),
    _observe: Subscription,
}

/// Redraws the tab bar when tab `id`'s view changes its title or status.
fn observe(tab: &DockTab, id: u64, cx: &mut Context<Dock>) -> Subscription {
    match tab {
        DockTab::Logs(view) => cx.observe(view, move |dock, _, cx| dock.tab_changed(id, cx)),
        DockTab::Shell(view) => cx.observe(view, move |dock, _, cx| dock.tab_changed(id, cx)),
        DockTab::Activity(view) => cx.observe(view, move |dock, _, cx| dock.tab_changed(id, cx)),
        DockTab::Compare(view) => cx.observe(view, move |dock, _, cx| dock.tab_changed(id, cx)),
        DockTab::Helm(view) => cx.observe(view, move |dock, _, cx| dock.tab_changed(id, cx)),
    }
}

/// What the tab bar shows of a tab.
fn tab_shown(tab: &DockTab, cx: &App) -> (SharedString, TabStatus) {
    let item = tab.item(cx);
    (item.tab_title(), item.tab_status())
}

pub enum DockEvent {
    /// The dock hid (or closed its last tab) while it had focus: focus should go back to the
    /// main area.
    Blurred,
    /// Reconnect Logs or Restart Shell asked for tab `0`'s session to start again: the
    /// workspace builds the new session, in a tab beside it ([`Dock::open_beside`]).
    Restart(u64),
}

impl EventEmitter<DockEvent> for Dock {}

pub struct Dock {
    tabs: Vec<Tab>,
    active: Option<u64>,
    visible: bool,
    /// The log tab whose session was last told it is shown ([`Dock::sync_shown_log`]).
    shown_log: Option<u64>,
    next_id: u64,
    focus: FocusHandle,
    /// The tab strip's scroll position: the active tab is scrolled into view.
    strip: ScrollHandle,
    /// The tab bar's width when last painted, and whether it changed since: the next frame
    /// scrolls the active tab into view again.
    strip_width: Rc<(Cell<f32>, Cell<bool>)>,
    /// The tab bar's bounds when last painted: where the tab list opens.
    bar: Rc<Cell<Bounds<Pixels>>>,
    /// The open tab list, until it is dismissed.
    tab_menu: Option<(Entity<PopupMenu>, Subscription)>,
    /// Every kubeconfig context, and the chip each one's tabs show ([`chip_labels`]).
    /// `None` until kubeconfig has been read.
    contexts: Option<Vec<String>>,
    labels: HashMap<String, String>,
}

impl Dock {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Dock {
            tabs: Vec::new(),
            active: None,
            visible: false,
            shown_log: None,
            next_id: 1,
            focus: cx.focus_handle(),
            strip: ScrollHandle::new(),
            strip_width: Rc::default(),
            bar: Rc::default(),
            tab_menu: None,
            contexts: None,
            labels: HashMap::new(),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// The tabs' ids, left to right.
    #[cfg(test)]
    pub fn tab_ids(&self) -> Vec<u64> {
        self.tabs.iter().map(|t| t.id).collect()
    }

    pub fn active_id(&self) -> Option<u64> {
        self.active
    }

    pub fn tab(&self, id: u64) -> Option<&DockTab> {
        self.tabs.iter().find(|t| t.id == id).map(|t| &t.tab)
    }

    fn active_tab(&self) -> Option<&DockTab> {
        self.tab(self.active?)
    }

    /// Scrolls the tab strip so the active tab is in view (on the next layout): with many tabs
    /// a tab just opened or shown would otherwise sit past the strip's edge.
    fn reveal_active(&self) {
        if let Some(ix) = self
            .active
            .and_then(|id| self.tabs.iter().position(|t| t.id == id))
        {
            self.strip.scroll_to_item(ix);
        }
    }

    /// The log tab shown, if the dock is visible and showing one: where log actions go.
    pub fn active_log(&self) -> Option<&Entity<LogView>> {
        match self.active_tab().filter(|_| self.visible)? {
            DockTab::Logs(view) => Some(view),
            DockTab::Shell(_) | DockTab::Activity(_) | DockTab::Compare(_) | DockTab::Helm(_) => {
                None
            }
        }
    }

    /// The shell tab shown, if the dock is visible and showing one: where terminal actions go.
    /// Every shell tab's view, shown or not.
    pub fn terminals(&self) -> impl Iterator<Item = &Entity<TerminalView>> {
        self.tabs.iter().filter_map(|t| match &t.tab {
            DockTab::Shell(view) => Some(view),
            DockTab::Logs(_) | DockTab::Activity(_) | DockTab::Compare(_) | DockTab::Helm(_) => {
                None
            }
        })
    }

    /// Every log tab's view, shown or not.
    pub fn log_views(&self) -> impl Iterator<Item = &Entity<LogView>> {
        self.tabs.iter().filter_map(|t| match &t.tab {
            DockTab::Logs(view) => Some(view),
            DockTab::Shell(_) | DockTab::Activity(_) | DockTab::Compare(_) | DockTab::Helm(_) => {
                None
            }
        })
    }

    /// The Helm tab shown, if the dock is visible and showing one: where its actions go.
    pub fn active_helm(&self) -> Option<&Entity<HelmView>> {
        match self.active_tab().filter(|_| self.visible)? {
            DockTab::Helm(view) => Some(view),
            _ => None,
        }
    }

    /// The compare tab shown, if the dock is visible and showing one: where its actions go.
    pub fn active_compare(&self) -> Option<&Entity<CompareView>> {
        match self.active_tab().filter(|_| self.visible)? {
            DockTab::Compare(view) => Some(view),
            _ => None,
        }
    }

    pub fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        match self.active_tab().filter(|_| self.visible)? {
            DockTab::Shell(view) => Some(view),
            DockTab::Logs(_) | DockTab::Activity(_) | DockTab::Compare(_) | DockTab::Helm(_) => {
                None
            }
        }
    }

    /// The sessions closing the dock would end: its shells and following log tabs still at
    /// work, shown or not ([`dock_model::ends_a_session`]).
    pub fn live_sessions(&self, cx: &App) -> LiveSessions {
        let mut live = LiveSessions::default();
        for t in &self.tabs {
            let item = t.tab.item(cx);
            let (reads_to_end, count) = match item.tab_key() {
                TabKey::Shell(_) => (false, &mut live.shells),
                TabKey::Logs(request) => (request.previous, &mut live.logs),
                // Runs no session: closing it ends nothing.
                TabKey::Activity(_) | TabKey::Compare(..) | TabKey::Helm(_) => continue,
            };
            if dock_model::ends_a_session(item.tab_status(), reads_to_end) {
                *count += 1;
            }
        }
        live
    }

    /// The id of the tab whose session `key` names on `cluster` (its context and server).
    pub fn find(&self, key: &TabKey, cluster: &TabCluster, cx: &App) -> Option<u64> {
        self.tabs
            .iter()
            .find(|t| {
                if t.cluster != *cluster || t.repointed {
                    return false;
                }
                let item = t.tab.item(cx);
                let reads_to_end = matches!(key, TabKey::Logs(request) if request.previous);
                item.tab_key() == *key && dock_model::reopens(item.tab_status(), reads_to_end)
            })
            .map(|t| t.id)
    }

    /// The tab Reconnect Logs (`logs`) or Restart Shell acts on: the tab shown, if it is a log
    /// tab (a shell tab) whose session ended or failed.
    pub fn restartable(&self, logs: bool, cx: &App) -> Option<u64> {
        let id = self.active.filter(|_| self.visible)?;
        let tab = self.tab(id)?;
        let kind = matches!(
            (tab, logs),
            (DockTab::Logs(_), true) | (DockTab::Shell(_), false)
        );
        (kind && tab.item(cx).can_restart()).then_some(id)
    }

    /// Opens a new session built by `build`, on `cluster`, in a tab right after tab `id`, and
    /// shows it (Reconnect Logs, Restart Shell). The ended tab stays, with its lines or
    /// scrollback, until it is closed: starting again never discards output. Returns the new
    /// tab's id.
    pub fn open_beside(
        &mut self,
        id: u64,
        cluster: TabCluster,
        build: impl FnOnce(&mut Window, &mut Context<Self>) -> DockTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let ix = self.tabs.iter().position(|t| t.id == id)?;
        let tab = build(window, cx);
        let new = self.next_id;
        self.next_id += 1;
        let shown = tab_shown(&tab, cx);
        self.tabs.insert(
            ix + 1,
            Tab {
                id: new,
                _observe: observe(&tab, new, cx),
                tab,
                cluster,
                repointed: false,
                shown,
            },
        );
        self.tab_added(ix + 1, cx);
        self.activate(new, window, cx);
        Some(new)
    }

    /// Tab `id`'s title and cluster, as its accessible label names them.
    #[cfg(test)]
    pub fn full_title(&self, id: u64, cx: &App) -> Option<String> {
        let tab = self.tabs.iter().find(|t| t.id == id)?;
        let title = tab.tab.item(cx).tab_title();
        Some(dock_model::full_title(&title, &tab.cluster))
    }

    /// What the tab list says of tab `id`: its title, its cluster's chip and its status.
    pub fn list_entry(&self, id: u64, cx: &App) -> Option<String> {
        let tab = self.tabs.iter().find(|t| t.id == id)?;
        let item = tab.tab.item(cx);
        Some(dock_model::list_label(
            &item.tab_title(),
            &self.label(&tab.cluster.context),
            &item.tab_status_text(),
        ))
    }

    /// Whether tab `id`'s context was re-pointed at another server since it opened.
    #[cfg(test)]
    pub fn is_repointed(&self, id: u64) -> bool {
        self.tabs.iter().any(|t| t.id == id && t.repointed)
    }

    /// The chip of `context`'s tabs: told apart from every other context in kubeconfig.
    fn label(&self, context: &str) -> String {
        match self.labels.get(context) {
            Some(label) => label.clone(),
            None => chip_labels(&[context.to_owned()])
                .remove(context)
                .unwrap_or_default(),
        }
    }

    /// Takes in kubeconfig's contexts, so chips tell apart contexts whose names read alike, and
    /// updates the chips. The active tab is scrolled into view again: chips before it may have
    /// grown.
    pub fn set_contexts(&mut self, contexts: Vec<String>, cx: &mut Context<Self>) {
        if self.contexts.as_ref() == Some(&contexts) {
            return;
        }
        self.contexts = Some(contexts);
        self.relabel(cx);
        self.reveal_active();
    }

    /// Whether `context` has left kubeconfig (its tabs keep running).
    fn is_removed(&self, context: &str) -> bool {
        self.contexts
            .as_ref()
            .is_some_and(|contexts| !contexts.iter().any(|c| c == context))
    }

    /// Labels every context in kubeconfig and every one with an open tab, so a tab on a
    /// context that left kubeconfig never reads like one still there, and tells the shells'
    /// toolbars.
    fn relabel(&mut self, cx: &mut Context<Self>) {
        let mut all = self.contexts.clone().unwrap_or_default();
        for tab in &self.tabs {
            if !all.contains(&tab.cluster.context) {
                all.push(tab.cluster.context.clone());
            }
        }
        self.labels = chip_labels(&all);
        for ix in 0..self.tabs.len() {
            self.show_cluster(ix, cx);
        }
        cx.notify();
    }

    /// Labels a new tab `ix`: a context no chip covers yet (one that left kubeconfig) is labelled
    /// with the others, so its chip reads unlike theirs.
    fn tab_added(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.labels.contains_key(&self.tabs[ix].cluster.context) {
            self.show_cluster(ix, cx);
        } else {
            self.relabel(cx);
        }
    }

    /// Tells tab `ix`'s view (a shell's toolbar) its chip: label, tooltip, and whether its
    /// context points elsewhere now.
    fn show_cluster(&self, ix: usize, cx: &mut Context<Self>) {
        let tab = &self.tabs[ix];
        if let DockTab::Shell(view) = &tab.tab {
            let label = self.label(&tab.cluster.context);
            let removed = self.is_removed(&tab.cluster.context);
            let mut tooltip = chip_tooltip(&tab.cluster);
            if removed {
                tooltip.push_str(&format!("\n{REMOVED}"));
            }
            let stale = tab.repointed || removed;
            view.update(cx, |view, cx| view.set_cluster(label, tooltip, stale, cx));
        }
    }

    /// Ends the shells whose context now points at another API server than when they opened
    /// (`server_of` gives a context's server now, `None` for a context that is gone, which is
    /// left alone). A shell stays connected to the server it started on; its tab would
    /// otherwise claim the context's new cluster. Log tabs are left to the core, which checks
    /// their context again and ends them itself.
    pub fn check_servers(
        &mut self,
        server_of: impl Fn(&str) -> Option<Option<String>>,
        cx: &mut Context<Self>,
    ) {
        for ix in 0..self.tabs.len() {
            let tab = &mut self.tabs[ix];
            if tab.repointed {
                continue;
            }
            let Some(now) = server_of(&tab.cluster.context) else {
                continue;
            };
            if tab.cluster.is_at(now.as_deref()) {
                continue;
            }
            tab.repointed = true;
            // A shell that already ended keeps saying how it ended.
            if let DockTab::Shell(view) = &tab.tab
                && view.read(cx).is_live()
            {
                let note = dock_model::repointed_note(&tab.cluster.context);
                view.update(cx, |view, cx| view.end(note, cx));
            }
            self.show_cluster(ix, cx);
            cx.notify();
        }
    }

    /// [`ActivateTab`] and [`CloseTab`] take the id of an open tab.
    pub fn check_tab<A: Action>(&self, id: u64) -> Result<(), ActionError> {
        match self.tab(id) {
            Some(_) => Ok(()),
            None => Err(ActionError::rejected::<A>(format!("no dock tab {id}"))),
        }
    }

    /// Shows the tab for `key`, building it with `build` if none is open, and focuses it.
    /// Returns its id.
    pub fn open(
        &mut self,
        key: TabKey,
        cluster: TabCluster,
        build: impl FnOnce(&mut Window, &mut Context<Self>) -> DockTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.add(key, cluster, build, window, cx);
        self.activate(id, window, cx);
        id
    }

    /// Adds the tab for `key` (built by `build`) behind the active one, unless it is open: for
    /// a tab that opens while the user works in another tab, which keeps the dock and focus.
    pub fn add(
        &mut self,
        key: TabKey,
        cluster: TabCluster,
        build: impl FnOnce(&mut Window, &mut Context<Self>) -> DockTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        if let Some(id) = self.find(&key, &cluster, cx) {
            return id;
        }
        let tab = build(window, cx);
        let id = self.next_id;
        self.next_id += 1;
        let shown = tab_shown(&tab, cx);
        self.tabs.push(Tab {
            id,
            _observe: observe(&tab, id, cx),
            tab,
            cluster,
            repointed: false,
            shown,
        });
        self.tab_added(self.tabs.len() - 1, cx);
        if self.active.is_none() {
            self.active = Some(id);
            self.visible = true;
            self.reveal_active();
        }
        self.sync_shown_log(cx);
        cx.notify();
        id
    }

    /// Whether keyboard focus is in the dock (one of its tabs).
    pub fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    /// Redraws the tab bar when tab `id`'s title or status changed.
    fn tab_changed(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let now = tab_shown(&self.tabs[ix].tab, cx);
        if self.tabs[ix].shown != now {
            self.tabs[ix].shown = now;
            cx.notify();
        }
    }

    /// Tells the log sessions which tab is shown, if any: the shared log memory budget trims
    /// the tabs viewed least recently first, and never the one shown. Run where the shown tab
    /// or the dock's visibility changes, and on every render for anything else (a hidden dock
    /// is not drawn, so hiding it cannot rely on the render).
    fn sync_shown_log(&mut self, cx: &App) {
        let shown = self.shown_tab();
        for tab in &self.tabs {
            if let DockTab::Shell(view) = &tab.tab {
                view.read(cx).set_dock_shown(shown == Some(tab.id));
            }
        }
        let now = self
            .active
            .filter(|_| self.visible)
            .filter(|id| matches!(self.tab(*id), Some(DockTab::Logs(_))));
        if now == self.shown_log {
            return;
        }
        for (id, shown) in [(self.shown_log, false), (now, true)] {
            if let Some(DockTab::Logs(view)) = id.and_then(|id| self.tab(id)) {
                view.read(cx).set_shown(shown);
            }
        }
        self.shown_log = now;
    }

    /// Shows the dock with tab `id` and focuses it.
    fn activate(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab(id).is_none() {
            return;
        }
        self.active = Some(id);
        self.visible = true;
        self.reveal_active();
        self.focus_active(window, cx);
        self.sync_shown_log(cx);
        cx.notify();
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            let focus = tab.item(cx).tab_focus();
            window.focus(&focus, cx);
        }
    }

    /// Closes tab `id`, ending its session. The tab next to it is shown; closing the last one
    /// hides the dock.
    fn close(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let had_focus = self.focus.contains_focused(window, cx);
        self.tabs.remove(ix);
        if self.active == Some(id) {
            self.active = self
                .tabs
                .get(ix)
                .or_else(|| ix.checked_sub(1).and_then(|i| self.tabs.get(i)))
                .map(|t| t.id);
            if self.active.is_some() {
                if had_focus {
                    self.focus_active(window, cx);
                }
            } else {
                self.visible = false;
                if had_focus {
                    // Not left on the closed tab: the dock holds it until its owner takes it.
                    window.focus(&self.focus, cx);
                    cx.emit(DockEvent::Blurred);
                }
            }
        }
        // The closed tab's search field, focused now or when another tab was shown.
        release_unfocused_input(window, cx);
        // Closing a tab before the active one moves it left, maybe out of view.
        self.reveal_active();
        self.sync_shown_log(cx);
        cx.notify();
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible {
            let had_focus = self.focus.contains_focused(window, cx);
            self.visible = false;
            if had_focus {
                cx.emit(DockEvent::Blurred);
            }
        } else {
            self.visible = true;
            self.focus_active(window, cx);
        }
        // Hidden, the dock is not drawn, so it says so here.
        self.sync_shown_log(cx);
        cx.notify();
    }

    /// Shows the tab `step` places to the right (left, when negative), wrapping around.
    fn cycle(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len() as isize;
        let Some(ix) = self
            .active
            .and_then(|id| self.tabs.iter().position(|t| t.id == id))
        else {
            return;
        };
        if count < 2 {
            return;
        }
        let next = (ix as isize + step).rem_euclid(count) as usize;
        let id = self.tabs[next].id;
        self.activate(id, window, cx);
    }

    /// Opens the tab list under the tab bar's end: every tab with its cluster and status;
    /// picking one shows it. Shows the dock first if it is hidden.
    fn show_tab_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        self.visible = true;
        let entries: Vec<(u64, String, bool)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let label = self.list_entry(tab.id, cx)?;
                Some((tab.id, label, self.active == Some(tab.id)))
            })
            .collect();
        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            menu_fit::dismiss_when_shrunk(window, cx);
            let menu = menu_fit::fit_menu(menu, window);
            menu_record::record(cx, "dock-tab-list", || {
                entries
                    .iter()
                    .map(|(_, label, active)| menu_record::Entry {
                        label: label.clone(),
                        enabled: true,
                        checked: Some(*active),
                    })
                    .collect()
            });
            entries.iter().fold(menu, |menu, (id, label, active)| {
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .checked(*active)
                        .on_click(registry::on_click(ActivateTab { tab: *id })),
                )
            })
        });
        // Where focus goes back to when the list closes without anything else taking it
        // (Escape, a click on nothing focusable). A picked tab, or what a click elsewhere
        // focused, keeps it.
        let before = window.focused(cx);
        let own = menu.focus_handle(cx);
        own.focus(window, cx);
        let dismissed = cx.subscribe_in(
            &menu,
            window,
            move |dock, _, _: &DismissEvent, window, cx| {
                dock.tab_menu = None;
                cx.notify();
                // After the click that dismissed it has run, so what it focused is known.
                let (before, own) = (before.clone(), own.clone());
                cx.defer_in(window, move |dock, window, cx| {
                    let stranded = match window.focused(cx) {
                        None => true,
                        Some(focused) => focused == own,
                    };
                    if !stranded {
                        return;
                    }
                    match before {
                        Some(before) => window.focus(&before, cx),
                        None => dock.focus_active(window, cx),
                    }
                });
            },
        );
        self.tab_menu = Some((menu, dismissed));
        cx.notify();
    }

    /// The open tab list, for tests.
    #[cfg(test)]
    pub(in crate::ui) fn tab_menu_for_tests(&self) -> Option<Entity<PopupMenu>> {
        self.tab_menu.as_ref().map(|(menu, _)| menu.clone())
    }

    /// The tab shown, if the dock is visible and has one: what Close Dock Tab closes.
    pub fn shown_tab(&self) -> Option<u64> {
        self.active.filter(|_| self.visible)
    }

    /// The focus of the tab shown, if any: where Tab moves focus to in the dock (UP-33).
    pub fn shown_tab_focus(&self, cx: &App) -> Option<FocusHandle> {
        self.shown_tab()?;
        Some(self.active_tab()?.item(cx).tab_focus())
    }

    /// The dock's actions on the tab shown, for [`Bind`]: bound only while a tab is shown
    /// ([`Dock::shown_tab`]), so the Window menu's Close Dock Tab is disabled otherwise. Cmd-W
    /// then falls through to its lower-precedence binding, Close Window (#177).
    pub fn bind_shown_tab_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<CloseActiveTab>(|dock, _, window, cx| match dock.shown_tab() {
            Some(id) => dock.close(id, window, cx),
            None => cx.propagate(),
        })
    }

    /// Every other dock action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleDock>(|dock, _, window, cx| dock.toggle(window, cx))
            .bind::<ShowTabList>(|dock, _, window, cx| dock.show_tab_list(window, cx))
            .bind::<NextTab>(|dock, _, window, cx| dock.cycle(1, window, cx))
            .bind::<PrevTab>(|dock, _, window, cx| dock.cycle(-1, window, cx))
            .bind::<ActivateTab>(|dock, action, window, cx| {
                match dock.check_tab::<ActivateTab>(action.tab) {
                    Ok(()) => dock.activate(action.tab, window, cx),
                    Err(e) => tracing::warn!("{e}"),
                }
            })
            .bind::<ReconnectLogs>(|dock, _, _, cx| match dock.restartable(true, cx) {
                Some(id) => cx.emit(DockEvent::Restart(id)),
                // Not ours (a running tab, or a shell): let Cmd-R reach Restart Shell or Refresh.
                None => cx.propagate(),
            })
            .bind::<RestartShell>(|dock, _, _, cx| match dock.restartable(false, cx) {
                Some(id) => cx.emit(DockEvent::Restart(id)),
                None => cx.propagate(),
            })
            .bind::<CloseTab>(|dock, action, window, cx| {
                match dock.check_tab::<CloseTab>(action.tab) {
                    Ok(()) => dock.close(action.tab, window, cx),
                    Err(e) => tracing::warn!("{e}"),
                }
            })
    }

    fn render_tab(&self, tab: &Tab, theme: &Theme, cx: &App) -> AnyElement {
        let item = tab.tab.item(cx);
        let active = self.active == Some(tab.id);
        let title = item.tab_title();
        let label = dock_model::full_title(&title, &tab.cluster);
        let mut tooltip = item.tab_tooltip().to_string();
        if let Some(server) = &tab.cluster.server {
            tooltip.push_str(&format!(" ({})", flint_core::redact::server_url(server)));
        }
        if tab.repointed {
            tooltip.push_str("\nIts context now points at a different API server.");
        }
        let removed = self.is_removed(&tab.cluster.context);
        if removed {
            tooltip.push_str(&format!("\n{REMOVED}"));
        }
        let dot = match item.tab_status() {
            TabStatus::Starting | TabStatus::Ended => theme.muted_foreground,
            TabStatus::Live => theme.success,
            TabStatus::Retrying => theme.warning,
            TabStatus::Failed => theme.danger,
        };
        let id = tab.id;
        h_flex()
            .id(("dock-tab", id as usize))
            .h_full()
            .flex_none()
            .pl_2()
            .pr_1()
            .gap_1()
            .text_xs()
            .border_r_1()
            .border_color(theme.border)
            .cursor_pointer()
            .relative()
            .aria_label(label)
            .map(|el| {
                if active {
                    el.bg(theme.tab_active)
                        .text_color(theme.tab_active_foreground)
                        // The accent marks the active tab, as it marks selections.
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(px(2.))
                                .bg(theme.primary),
                        )
                } else {
                    el.bg(theme.tab)
                        .text_color(theme.tab_foreground)
                        .hover(|el| el.bg(theme.tab_active))
                }
            })
            .on_click(registry::on_click(ActivateTab { tab: id }))
            .child(super::theme::status_dot(dot, true).flex_none())
            // The cluster first: a shell on production must not pass for one on a dev cluster.
            // The tab's own tooltip names the context and server.
            .child(cluster_chip(
                ("dock-tab-cluster", id as usize),
                self.label(&tab.cluster.context),
                tab.repointed || removed,
                None,
                theme,
            ))
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .font_family(theme.mono_font_family.clone())
                    .child(dock_model::shown_title(&title)),
            )
            .child(
                Button::new(("dock-tab-close", id as usize))
                    .icon(IconName::Close)
                    .xsmall()
                    .ghost()
                    .tab_stop(false)
                    .tooltip(format!("Close tab ({})", registry::key_hint("secondary-w")))
                    .on_click(registry::on_click_contained(CloseTab { tab: id })),
            )
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .test_support()
            .into_any_element()
    }
}

/// The cluster a session runs on, as a small outlined label: on a dock tab and in a shell's
/// toolbar. Struck through in the warning color once its context points elsewhere.
pub fn cluster_chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    repointed: bool,
    tooltip: Option<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    div()
        .id(id)
        .when_some(tooltip, |el, tooltip| {
            el.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        })
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .whitespace_nowrap()
        .text_size(px(10.))
        .text_color(if repointed {
            theme.warning
        } else {
            theme.muted_foreground
        })
        .when(repointed, |el| el.line_through())
        .child(label.into())
        .test_support()
}

impl Dock {
    /// The button that lists every tab (title, cluster and status) and shows the one picked:
    /// how to reach a tab scrolled out of the strip ([`ShowTabList`]).
    fn render_tab_list(&self) -> impl IntoElement {
        Button::new("dock-tab-list")
            .icon(IconName::ChevronsUpDown)
            .xsmall()
            .ghost()
            .tab_stop(false)
            .tooltip(format!(
                "All tabs ({})",
                registry::key_hint("secondary-shift-j")
            ))
            .on_click(registry::on_click(ShowTabList))
    }

    fn render_hide(&self) -> impl IntoElement {
        Button::new("dock-hide")
            .icon(IconName::ChevronDown)
            .xsmall()
            .ghost()
            .tab_stop(false)
            .tooltip(format!(
                "Hide the dock ({})",
                registry::key_hint("secondary-j")
            ))
            .on_click(registry::on_click(ToggleDock))
    }

    /// What the dock shows with no tab open, instead of a tab strip: how to open one.
    fn render_empty(&self, theme: &Theme) -> AnyElement {
        v_flex()
            .id("dock-empty")
            .flex_1()
            .relative()
            .items_center()
            .justify_center()
            .gap_1()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(div().child(EMPTY).test_text(EMPTY))
            .child(div().text_xs().child(format!(
                "Right-click a pod or workload and choose Logs ({}) or Shell ({}).",
                registry::key_hint("secondary-l"),
                registry::key_hint("secondary-shift-e")
            )))
            .test_support()
            .into_any_element()
    }
}

impl Render for Dock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_shown_log(cx);
        let theme = cx.theme().clone();
        let root = v_flex()
            .id("dock")
            .key_context(registry::DOCK)
            .track_focus(&self.focus)
            .map(|el| {
                let bind = Self::bind_actions(OnView::new(el, cx));
                match self.shown_tab() {
                    Some(_) => Self::bind_shown_tab_actions(bind).into_element(),
                    None => bind.into_element(),
                }
            })
            .size_full()
            .bg(theme.background);
        let Some(active) = self.active_tab() else {
            // The hide button keeps its place in the element tree (under `dock`), so it is the
            // same control with and without tabs.
            return root
                .relative()
                .child(self.render_empty(&theme))
                .child(div().absolute().top_1().right_1().child(self.render_hide()))
                .test_support();
        };
        let content = div()
            .flex_1()
            .min_h_0()
            .child(active.view())
            .into_any_element();
        let tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .map(|tab| self.render_tab(tab, &theme, cx))
            .collect();
        // A strip that changes width (the window or a pane was resized) scrolls the active tab
        // into view again. The width is known only once laid out, so it is checked while
        // painting, and the next frame scrolls (with the strip's new bounds).
        if self.strip_width.1.take() {
            self.reveal_active();
        }
        let watch_width = {
            let watch = self.strip_width.clone();
            let bar = self.bar.clone();
            let dock = cx.weak_entity();
            canvas(
                move |bounds, _, cx| {
                    bar.set(bounds);
                    let now = f32::from(bounds.size.width);
                    let before = watch.0.replace(now);
                    if before > 0. && (now - before).abs() > 0.5 {
                        watch.1.set(true);
                        // Not while drawing (a refresh then is ignored): the dock asks for the
                        // next frame once this one is done.
                        let dock = dock.clone();
                        cx.defer(move |cx| {
                            let _ = dock.update(cx, |_, cx| cx.notify());
                        });
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };
        root.child(
            h_flex()
                .h(px(TAB_BAR_HEIGHT))
                .flex_none()
                .relative()
                .bg(theme.tab_bar)
                .border_b_1()
                .border_color(theme.border)
                .child(watch_width)
                .child(
                    h_flex()
                        .id("dock-tabs")
                        .h_full()
                        .flex_1()
                        .min_w_0()
                        .overflow_x_scroll()
                        .track_scroll(&self.strip)
                        .children(tabs)
                        .test_support(),
                )
                .child(
                    h_flex()
                        .px_1()
                        .flex_none()
                        .child(self.render_tab_list())
                        .child(self.render_hide()),
                ),
        )
        .child(content)
        .when_some(self.tab_menu.as_ref(), |el, (menu, _)| {
            let bar = self.bar.get();
            el.child(deferred(
                anchored()
                    .anchor(Anchor::TopRight)
                    .position(point(bar.right(), bar.bottom()))
                    .snap_to_window_with_margin(px(8.))
                    .child(menu.clone()),
            ))
        })
        .test_support()
    }
}

#[cfg(any(test, feature = "automation"))]
#[path = "dock_state.rs"]
mod state;

#[cfg(test)]
mod tests {
    use super::{DEFAULT_HEIGHT, MIN_HEIGHT, dock_height};
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    #[test]
    fn every_dock_action_is_bound() {
        assert_binds_scope(Scope::Dock, |names| {
            super::Dock::bind_shown_tab_actions(super::Dock::bind_actions(names))
        });
    }

    // covers UP-42: the dock keeps a usable height and leaves the main area room.
    #[test]
    fn the_dock_height_stays_within_its_limits() {
        assert_eq!(dock_height(None, 800., false), DEFAULT_HEIGHT);
        assert_eq!(dock_height(Some(400), 800., false), 400.);
        assert_eq!(dock_height(Some(50), 800., false), MIN_HEIGHT);
        // A short window: the main area keeps its room, down to the dock's minimum.
        assert_eq!(dock_height(Some(700), 800., false), 520.);
        assert_eq!(dock_height(None, 420., false), 140.);
        assert_eq!(dock_height(Some(700), 200., false), MIN_HEIGHT);
        // A two-row header keeps the same room, with the same lower limit.
        assert_eq!(dock_height(Some(700), 480., false), 200.);
        assert_eq!(dock_height(Some(700), 480., true), 200.);
        assert_eq!(dock_height(Some(200), 480., true), 200.);
        assert_eq!(dock_height(Some(700), 200., true), MIN_HEIGHT);
    }
}
