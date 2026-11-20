//! The bottom dock (#13): a resizable panel under the main area with one tab per session that
//! outlives navigation (UP-42): log tabs and shell tabs (#14); AI log analysis (#56) next. Tabs belong to no cluster, so they keep running when the user switches
//! clusters or views, and end only when closed. The dock's height is kept in `state.toml`
//! (`[panes] dock`); its tabs are not, since their sessions end with the app.
//!
//! # Adding a tab kind
//!
//! 1. Implement [`DockItem`] for the tab's view: its [`TabKey`] (so opening the same thing again
//!    shows the existing tab instead of a second session), title, tooltip, [`TabStatus`] and
//!    focus handle. The view notifies when its title or status changes; the dock redraws the
//!    tab bar then.
//! 2. Add a [`DockTab`] variant holding the view's entity and a [`TabKey`] variant, and fill in
//!    the `match`es in this file ([`DockTab::item`], [`DockTab::view`], [`Dock::open`]'s
//!    observation).
//! 3. Open it with [`Dock::open`] from wherever the user asks for it (the workspace opens log
//!    tabs on the detail pane's `OpenLogs`).
//! 4. Give its controls a registry scope and a `bind_actions`, and have the workspace forward
//!    them to the active tab of that kind (see `Workspace::register_actions`, which does it for
//!    [`Dock::active_log`] and [`Dock::active_terminal`]) and guard them, as for any view.

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

pub use super::dock_model::TabStatus;
use super::dock_model::{self, TabCluster};
use super::log_view::{LogRequest, LogView};
use super::registry::{
    self, ActionError, ActivateTab, Bind, CloseActiveTab, CloseTab, NextTab, OnView, PrevTab,
    ToggleDock,
};
use super::terminal_view::{ShellKey, TerminalView};

/// The dock's height before the user drags it.
pub const DEFAULT_HEIGHT: f32 = 280.;
/// The lowest the dock gets: its tab bar, a toolbar and a few lines.
pub const MIN_HEIGHT: f32 = 120.;
/// The dock leaves the main area at least this much (its header and a few rows).
const MIN_MAIN_HEIGHT: f32 = 160.;
/// The tab bar's height.
const TAB_BAR_HEIGHT: f32 = 28.;

/// The dock's height in a window `window_height` tall: as saved (or the default), within
/// [`MIN_HEIGHT`] and what leaves the main area [`MIN_MAIN_HEIGHT`]. The saved height is
/// untouched, so a taller window gets it back.
pub fn dock_height(saved: Option<u32>, window_height: f32) -> f32 {
    let max = (window_height - MIN_MAIN_HEIGHT).max(MIN_HEIGHT);
    saved
        .map_or(DEFAULT_HEIGHT, |h| h as f32)
        .clamp(MIN_HEIGHT, max)
}

/// What identifies a tab's session: opening a tab with the key of an open one shows that one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TabKey {
    Logs(LogRequest),
    Shell(ShellKey),
}

/// What the dock needs from a tab's view.
pub trait DockItem {
    fn tab_key(&self) -> TabKey;
    fn tab_title(&self) -> SharedString;
    fn tab_tooltip(&self) -> SharedString;
    fn tab_status(&self) -> TabStatus;
    /// Focused when the tab is shown.
    fn tab_focus(&self) -> FocusHandle;
}

/// A tab's view, by kind.
#[derive(Clone)]
pub enum DockTab {
    Logs(Entity<LogView>),
    Shell(Entity<TerminalView>),
}

impl DockTab {
    pub fn item<'a>(&self, cx: &'a App) -> &'a dyn DockItem {
        match self {
            DockTab::Logs(view) => view.read(cx),
            DockTab::Shell(view) => view.read(cx),
        }
    }

    fn view(&self) -> AnyView {
        match self {
            DockTab::Logs(view) => view.clone().into(),
            DockTab::Shell(view) => view.clone().into(),
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

/// What the tab bar shows of a tab.
fn tab_shown(tab: &DockTab, cx: &App) -> (SharedString, TabStatus) {
    let item = tab.item(cx);
    (item.tab_title(), item.tab_status())
}

pub enum DockEvent {
    /// The dock hid (or closed its last tab) while it had focus: focus should go back to the
    /// main area.
    Blurred,
}

impl EventEmitter<DockEvent> for Dock {}

pub struct Dock {
    tabs: Vec<Tab>,
    active: Option<u64>,
    visible: bool,
    next_id: u64,
    focus: FocusHandle,
}

impl Dock {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Dock {
            tabs: Vec::new(),
            active: None,
            visible: false,
            next_id: 1,
            focus: cx.focus_handle(),
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

    /// The log tab shown, if the dock is visible and showing one: where log actions go.
    pub fn active_log(&self) -> Option<&Entity<LogView>> {
        match self.active_tab().filter(|_| self.visible)? {
            DockTab::Logs(view) => Some(view),
            DockTab::Shell(_) => None,
        }
    }

    /// The shell tab shown, if the dock is visible and showing one: where terminal actions go.
    pub fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        match self.active_tab().filter(|_| self.visible)? {
            DockTab::Shell(view) => Some(view),
            DockTab::Logs(_) => None,
        }
    }

    /// The id of the tab whose session `key` names on `cluster` (its context and server).
    pub fn find(&self, key: &TabKey, cluster: &TabCluster, cx: &App) -> Option<u64> {
        self.tabs
            .iter()
            .find(|t| t.cluster == *cluster && t.tab.item(cx).tab_key() == *key)
            .map(|t| t.id)
    }

    /// Tab `id`'s title and cluster, as its accessible label names them.
    #[cfg(test)]
    pub fn full_title(&self, id: u64, cx: &App) -> Option<String> {
        let tab = self.tabs.iter().find(|t| t.id == id)?;
        let title = tab.tab.item(cx).tab_title();
        Some(dock_model::full_title(&title, &tab.cluster))
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
        for tab in &mut self.tabs {
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
            if let DockTab::Shell(view) = &tab.tab {
                let note = dock_model::repointed_note(&tab.cluster.context);
                view.update(cx, |view, cx| view.end(note, cx));
            }
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
        let observe = match &tab {
            DockTab::Logs(view) => cx.observe(view, move |dock, _, cx| dock.tab_changed(id, cx)),
            DockTab::Shell(view) => cx.observe(view, move |dock, _, cx| dock.tab_changed(id, cx)),
        };
        let shown = tab_shown(&tab, cx);
        self.tabs.push(Tab {
            id,
            tab,
            cluster,
            repointed: false,
            shown,
            _observe: observe,
        });
        if self.active.is_none() {
            self.active = Some(id);
            self.visible = true;
        }
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

    /// Shows the dock with tab `id` and focuses it.
    fn activate(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab(id).is_none() {
            return;
        }
        self.active = Some(id);
        self.visible = true;
        self.focus_active(window, cx);
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
                    cx.emit(DockEvent::Blurred);
                }
            }
        }
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

    /// Every dock action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleDock>(|dock, _, window, cx| dock.toggle(window, cx))
            .bind::<CloseActiveTab>(|dock, _, window, cx| {
                match dock.active.filter(|_| dock.visible) {
                    Some(id) => dock.close(id, window, cx),
                    // Not ours: let Cmd-W close the window.
                    None => cx.propagate(),
                }
            })
            .bind::<NextTab>(|dock, _, window, cx| dock.cycle(1, window, cx))
            .bind::<PrevTab>(|dock, _, window, cx| dock.cycle(-1, window, cx))
            .bind::<ActivateTab>(|dock, action, window, cx| {
                match dock.check_tab::<ActivateTab>(action.tab) {
                    Ok(()) => dock.activate(action.tab, window, cx),
                    Err(e) => tracing::warn!("{e}"),
                }
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
            .gap_1p5()
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
            .child(div().size_1p5().flex_none().rounded_full().bg(dot))
            // The cluster first: a shell on production must not pass for one on a dev cluster.
            .child(cluster_chip(
                ("dock-tab-cluster", id as usize),
                tab.cluster.label(),
                tab.repointed,
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
    theme: &Theme,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .whitespace_nowrap()
        .text_color(if repointed {
            theme.warning
        } else {
            theme.muted_foreground
        })
        .when(repointed, |el| el.line_through())
        .child(label.into())
        .test_support()
}

impl Render for Dock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .map(|tab| self.render_tab(tab, &theme, cx))
            .collect();
        let content = match self.active_tab() {
            Some(tab) => div()
                .flex_1()
                .min_h_0()
                .child(tab.view())
                .into_any_element(),
            None => v_flex()
                .id("dock-empty")
                .flex_1()
                .items_center()
                .justify_center()
                .gap_1()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No sessions.")
                .child(div().text_xs().child(format!(
                    "Right-click a pod or workload for its logs ({}) or a shell ({}).",
                    registry::key_hint("secondary-l"),
                    registry::key_hint("secondary-shift-e")
                )))
                .test_support()
                .into_any_element(),
        };
        v_flex()
            .id("dock")
            .key_context(registry::DOCK)
            .track_focus(&self.focus)
            .map(|el| Self::bind_actions(OnView::new(el, cx)).into_element())
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .h(px(TAB_BAR_HEIGHT))
                    .flex_none()
                    .bg(theme.tab_bar)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .id("dock-tabs")
                            .h_full()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_scroll()
                            .children(tabs),
                    )
                    .child(
                        div().px_1().flex_none().child(
                            Button::new("dock-hide")
                                .icon(IconName::ChevronDown)
                                .xsmall()
                                .ghost()
                                .tab_stop(false)
                                .tooltip(format!(
                                    "Hide the dock ({})",
                                    registry::key_hint("secondary-j")
                                ))
                                .on_click(registry::on_click(ToggleDock)),
                        ),
                    ),
            )
            .child(content)
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_HEIGHT, MIN_HEIGHT, dock_height};
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    #[test]
    fn every_dock_action_is_bound() {
        assert_binds_scope(Scope::Dock, super::Dock::bind_actions);
    }

    // covers UP-42: the dock keeps a usable height and leaves the main area room.
    #[test]
    fn the_dock_height_stays_within_its_limits() {
        assert_eq!(dock_height(None, 800.), DEFAULT_HEIGHT);
        assert_eq!(dock_height(Some(400), 800.), 400.);
        assert_eq!(dock_height(Some(50), 800.), MIN_HEIGHT);
        // A short window: the main area keeps its room, down to the dock's minimum.
        assert_eq!(dock_height(Some(700), 800.), 640.);
        assert_eq!(dock_height(Some(700), 200.), MIN_HEIGHT);
    }
}
