//! The discovery-driven navigation (#7): built-in kinds in fixed sections, then CRDs and
//! aggregated APIs grouped by API group, then the remaining built-ins ("Other"), each kind with
//! a subtle marker of its watch state. Every section but Workloads starts collapsed, so custom
//! resources are reachable without scrolling past every built-in kind.
//!
//! It takes focus (Tab, or a click) and is used from the keyboard too (UP-33): the arrows move a
//! cursor over the section headers and the kinds of open sections, Enter opens a kind or opens
//! and closes a section, Right opens a section and Left closes it. The cursor shows with the
//! theme's focus ring while the navigation has focus.

use std::collections::HashMap;
use std::sync::Arc;

use flint_core::{
    ClusterDiscovery, ClusterId, KindId, NavEntry, NavSection, SectionKind, SyncState,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::list::problems::ProblemBadge;
use super::registry::{self, OpenClusterOverview, OpenView, RetryConnection, ToggleNavSection};
use super::set_kinds::{MemberKinds, SetKinds};
use super::theme::status_dot;

/// Which sections are open, per cluster. Sections the user never toggled follow
/// [`collapsed_by_default`].
#[derive(Debug, Default, Clone)]
pub struct Collapsed(HashMap<String, bool>);

impl Collapsed {
    pub fn is_collapsed(&self, section: &NavSection) -> bool {
        self.0
            .get(&section.title)
            .copied()
            .unwrap_or_else(|| collapsed_by_default(section))
    }

    pub fn toggle(&mut self, section: &NavSection) {
        let next = !self.is_collapsed(section);
        self.0.insert(section.title.clone(), next);
    }

    /// Opens the section holding `kind`, so the open list is visible in the navigation.
    pub fn reveal(&mut self, sections: &[NavSection], kind: &KindId) {
        if let Some(section) = sections
            .iter()
            .find(|s| s.entries.iter().any(|e| e.id == *kind))
        {
            self.0.insert(section.title.clone(), false);
        }
    }
}

/// The section open by default: the kinds used most.
const OPEN_BY_DEFAULT: &str = "Workloads";

/// Every section starts collapsed except Workloads, the same rule for built-in and custom
/// sections. The section of the open kind opens on its own ([`Collapsed::reveal`]).
pub fn collapsed_by_default(section: &NavSection) -> bool {
    !(section.kind == SectionKind::BuiltIn && section.title == OPEN_BY_DEFAULT)
}

/// The marker a kind's watch state earns in the navigation. A healthy or unwatched kind gets
/// none, to keep the navigation quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Loading,
    Reconnecting,
    Failed,
}

pub fn marker(state: &SyncState) -> Option<Marker> {
    match state {
        SyncState::Loading => Some(Marker::Loading),
        // A denial is settled by its retries, and then is no fault to flag: the tooltip says it.
        s if s.is_checking_access() => Some(Marker::Loading),
        s if s.is_no_access() => None,
        SyncState::Reconnecting { .. } | SyncState::RetryingRefusal { .. } => {
            Some(Marker::Reconnecting)
        }
        SyncState::Failed { .. } => Some(Marker::Failed),
        SyncState::Idle | SyncState::Synced | SyncState::Unavailable => None,
    }
}

/// A kind's watch state in its tooltip.
fn state_text(state: Option<&SyncState>) -> String {
    match state {
        None | Some(SyncState::Idle) => "not watched".to_owned(),
        Some(SyncState::Loading) => "loading".to_owned(),
        Some(SyncState::Synced) => "live".to_owned(),
        Some(SyncState::Reconnecting { error }) => match error {
            Some(e) => format!("reconnecting: {e}"),
            None => "reconnecting".to_owned(),
        },
        Some(s @ SyncState::RetryingRefusal { error, .. }) if s.is_checking_access() => {
            format!("checking access: {error}")
        }
        Some(SyncState::RetryingRefusal { error, .. }) => format!("reconnecting: {error}"),
        Some(s @ SyncState::Failed { error, .. }) if s.is_no_access() => {
            format!("no access: {error}")
        }
        Some(SyncState::Failed { error, .. }) => format!("failed: {error}"),
        Some(SyncState::Unavailable) => "unavailable".to_owned(),
    }
}

/// The height of a section header or kind row.
const ROW_HEIGHT: f32 = 24.;

/// A row the keyboard cursor can be on: a section header, by title, or a kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavRow {
    Section(String),
    Kind(KindId),
}

/// What the navigation asks of the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEvent {
    /// Open this kind's list, as clicking it does.
    Open(KindId),
    /// Open the cluster overview.
    OpenOverview,
}

pub struct NavPanel {
    cluster: Option<ClusterId>,
    discovery: Option<Arc<ClusterDiscovery>>,
    /// While a cluster set is open: what its members serve, shown instead of `discovery`.
    set: Option<SetKinds>,
    error: Option<String>,
    collapsed: HashMap<ClusterId, Collapsed>,
    states: HashMap<KindId, SyncState>,
    /// Problem counts of the cluster or set on screen, by kind.
    problems: HashMap<KindId, ProblemBadge>,
    selected: Option<KindId>,
    /// The cluster overview is open in the main area.
    overview: bool,
    scroll: ScrollHandle,
    scroll_to_selected: bool,
    /// The cluster's connection failed, so no discovery is coming.
    connection_failed: bool,
    /// The cluster waits for its kubeconfig to be confirmed, so no discovery is coming yet.
    awaiting_confirmation: bool,
    focus: FocusHandle,
    /// The keyboard cursor, once the arrows moved it; until then (or once its row is hidden)
    /// it is on the open kind, else the first row ([`NavPanel::cursor`]).
    cursor: Option<NavRow>,
    /// Bring the cursor's row into view on the next frame.
    scroll_to_cursor: bool,
}

impl EventEmitter<NavEvent> for NavPanel {}

impl NavPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        NavPanel {
            focus: cx.focus_handle(),
            cursor: None,
            scroll_to_cursor: false,
            cluster: None,
            discovery: None,
            set: None,
            error: None,
            collapsed: HashMap::new(),
            states: HashMap::new(),
            problems: HashMap::new(),
            selected: None,
            overview: false,
            scroll: ScrollHandle::new(),
            scroll_to_selected: false,
            connection_failed: false,
            awaiting_confirmation: false,
        }
    }

    /// Shows `cluster`'s navigation (`None`: nothing is active).
    pub fn set_cluster(
        &mut self,
        cluster: Option<ClusterId>,
        discovery: Option<Arc<ClusterDiscovery>>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.cluster != cluster {
            self.states.clear();
            self.problems.clear();
            self.selected = None;
            self.overview = false;
            self.cursor = None;
        }
        let changed = self.cluster != cluster
            || self.error != error
            || !same_discovery(&self.discovery, &discovery);
        self.cluster = cluster;
        self.discovery = discovery;
        self.error = error;
        if changed {
            cx.notify();
        }
    }

    /// Shows what the open set's `members` serve (`None`: no set is open), with `selected`
    /// marked: switching the active cluster inside a set keeps the set's kind open.
    pub fn set_cluster_set(
        &mut self,
        members: Option<Vec<MemberKinds>>,
        selected: Option<KindId>,
        cx: &mut Context<Self>,
    ) {
        match (members, &self.set) {
            (None, None) => {}
            (None, Some(_)) => {
                self.set = None;
                cx.notify();
            }
            (Some(members), Some(set)) if set.built_from(&members) => {}
            (Some(members), _) => {
                self.set = Some(SetKinds::new(members));
                cx.notify();
            }
        }
        if selected.is_some() && self.selected != selected {
            self.set_selected(selected, cx);
        }
    }

    /// Shows the problem counts of the cluster or set on screen.
    pub fn set_problem_badges(
        &mut self,
        badges: HashMap<KindId, ProblemBadge>,
        cx: &mut Context<Self>,
    ) {
        if self.problems != badges {
            self.problems = badges;
            cx.notify();
        }
    }

    /// The open set's kinds, if a set is open.
    pub(in crate::ui) fn set_kinds(&self) -> Option<&SetKinds> {
        self.set.as_ref()
    }

    /// What the tree shows, and the name its open sections are kept under.
    pub(in crate::ui) fn tree(&self) -> Option<(&ClusterId, &ClusterDiscovery)> {
        shown_tree(&self.set, &self.cluster, &self.discovery)
    }

    pub fn set_connection_failed(&mut self, failed: bool, cx: &mut Context<Self>) {
        if self.connection_failed != failed {
            self.connection_failed = failed;
            cx.notify();
        }
    }

    pub fn set_awaiting_confirmation(&mut self, waiting: bool, cx: &mut Context<Self>) {
        if self.awaiting_confirmation != waiting {
            self.awaiting_confirmation = waiting;
            cx.notify();
        }
    }

    pub fn states(&self) -> &HashMap<KindId, SyncState> {
        &self.states
    }

    pub fn set_states(&mut self, states: Vec<(KindId, SyncState)>, cx: &mut Context<Self>) -> bool {
        let states: HashMap<KindId, SyncState> = states.into_iter().collect();
        if states == self.states {
            return false;
        }
        self.states = states;
        cx.notify();
        true
    }

    pub fn set_selected(&mut self, kind: Option<KindId>, cx: &mut Context<Self>) {
        if let (Some(kind), Some((cluster, discovery))) =
            (&kind, shown_tree(&self.set, &self.cluster, &self.discovery))
        {
            self.collapsed
                .entry(cluster.clone())
                .or_default()
                .reveal(&discovery.navigation.sections, kind);
        }
        self.scroll_to_selected = self.selected != kind;
        // The cursor follows the open kind, however it was opened.
        if let Some(kind) = &kind {
            self.cursor = Some(NavRow::Kind(kind.clone()));
            self.overview = false;
        }
        self.selected = kind;
        cx.notify();
    }

    /// Marks the cluster overview as open or closed in the main area.
    pub fn set_overview(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.overview == open {
            return;
        }
        self.overview = open;
        if open {
            self.selected = None;
        }
        cx.notify();
    }

    /// Whether the overview is marked as open.
    #[cfg(test)]
    pub fn overview_open(&self) -> bool {
        self.overview
    }

    /// The navigation's own focus: Tab cycles to it, and its keys work while it has it.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The rows the cursor can be on, top to bottom: every section header, and the listable
    /// kinds of the open sections.
    pub(in crate::ui) fn rows(&self) -> Vec<NavRow> {
        let Some((cluster, discovery)) = self.tree() else {
            return Vec::new();
        };
        let collapsed = self.collapsed.get(cluster).cloned().unwrap_or_default();
        let mut rows = Vec::new();
        for section in &discovery.navigation.sections {
            rows.push(NavRow::Section(section.title.clone()));
            if !collapsed.is_collapsed(section) {
                rows.extend(
                    section
                        .entries
                        .iter()
                        .filter(|e| e.listable)
                        .map(|e| NavRow::Kind(e.id.clone())),
                );
            }
        }
        rows
    }

    /// Where the cursor is: where the arrows left it while that row shows, else on the open
    /// kind, else on the first row.
    pub(in crate::ui) fn cursor(&self) -> Option<NavRow> {
        let rows = self.rows();
        self.cursor
            .clone()
            .filter(|c| rows.contains(c))
            .or_else(|| {
                self.selected
                    .clone()
                    .map(NavRow::Kind)
                    .filter(|c| rows.contains(c))
            })
            .or_else(|| rows.first().cloned())
    }

    /// The overview's row. The arrows move over sections and kinds; from the keyboard, the
    /// overview opens through the command palette.
    fn render_overview(&self, theme: &Theme) -> impl IntoElement {
        h_flex()
            .id("nav-overview")
            .role(gpui_kit::accesskit::Role::Button)
            .aria_label("Overview")
            .flex_none()
            .px_2()
            .h(px(ROW_HEIGHT))
            .rounded(theme.radius)
            .text_sm()
            .cursor_pointer()
            .relative()
            .when(self.overview, |el| {
                el.bg(theme.list_active).child(super::theme::selection_edge(
                    theme.list_active_border,
                    px(4.),
                ))
            })
            .when(!self.overview, |el| el.hover(|el| el.bg(theme.list_hover)))
            .border_1()
            .border_color(super::theme::cursor_ring(theme, false))
            .on_click(registry::on_click(OpenClusterOverview))
            .child("Overview")
            .test_support()
    }

    fn render_entry(&self, entry: &NavEntry, theme: &Theme, cursor: bool) -> AnyElement {
        let is_selected = self.selected.as_ref() == Some(&entry.id);
        let state = self.states.get(&entry.id);
        let mark = state.and_then(marker);
        let state_text = state_text(state);
        let listable = entry.listable;
        let coverage = self.set.as_ref().and_then(|s| s.coverage(&entry.id));
        let badge = coverage.and_then(|c| c.badge());
        let tip = format!(
            "{}\n{} {}{}\n{}{}",
            entry.label,
            entry.api_version,
            entry.kind,
            if entry.namespaced {
                ""
            } else {
                " (cluster-scoped)"
            },
            if listable {
                state_text
            } else {
                "cannot be listed".to_owned()
            },
            coverage
                .filter(|c| c.badge().is_some())
                .map_or_else(String::new, |c| format!("\n{}", c.detail())),
        );
        let problems = self.problems.get(&entry.id).cloned();
        let tip = match &problems {
            Some(problems) => format!("{tip}\n{}", problems.detail),
            None => tip,
        };
        let open = entry.id.to_string();
        h_flex()
            .id(SharedString::from(format!("nav-{}", entry.id)))
            // Named for assistive technology (and view tests).
            .aria_label(entry.label.clone())
            .pl_5()
            .pr_2()
            .h(px(ROW_HEIGHT))
            // Rows keep their height in the scrolling column instead of shrinking to fit it.
            .flex_none()
            .gap_1()
            .rounded(theme.radius)
            .text_sm()
            .when(!listable, |el| {
                el.text_color(theme.muted_foreground.opacity(0.6))
            })
            .when(listable, |el| {
                el.cursor_pointer().on_click(registry::on_click(OpenView {
                    kind: open,
                    ..Default::default()
                }))
            })
            // The open kind is marked with the accent, as selections are.
            .relative()
            .when(is_selected, |el| {
                el.bg(theme.list_active).child(super::theme::selection_edge(
                    theme.list_active_border,
                    px(4.),
                ))
            })
            .when(!is_selected && listable, |el| {
                el.hover(|el| el.bg(theme.list_hover))
            })
            // The keyboard cursor, while the navigation has focus.
            // Every row has the border, so the ring moving shifts nothing.
            .border_1()
            .border_color(super::theme::cursor_ring(theme, cursor))
            .child(
                div()
                    .id(SharedString::from(format!("nav-label-{}", entry.id)))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(entry.label.clone())
                    .test_support(),
            )
            .children(badge.map(|badge| {
                div()
                    .id(SharedString::from(format!("nav-coverage-{}", entry.id)))
                    .flex_none()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .font_features(super::list::tabular_figures())
                    .child(badge)
                    .test_support()
            }))
            .children(problems.map(|problems| {
                div()
                    .id(SharedString::from(format!("nav-problems-{}", entry.id)))
                    .flex_none()
                    .text_xs()
                    .text_color(if problems.text == "?" {
                        theme.muted_foreground
                    } else {
                        theme.danger
                    })
                    .font_features(super::list::tabular_figures())
                    .aria_label(problems.detail)
                    .child(problems.text)
                    .test_support()
            }))
            .children(mark.map(|m| match m {
                // Static, not a spinner: dozens of kinds load at once after activation, and
                // animating them all would keep the window redrawing.
                Marker::Loading => ring(theme.muted_foreground),
                Marker::Reconnecting => dot(theme.warning),
                Marker::Failed => dot(theme.danger),
            }))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .test_support()
            .into_any_element()
    }
}

/// The open set's kinds, none until a member has reported some, else the active cluster's. Takes
/// the fields, so a caller can change the panel's other fields while holding the result.
fn shown_tree<'a>(
    set: &'a Option<SetKinds>,
    cluster: &'a Option<ClusterId>,
    discovery: &'a Option<Arc<ClusterDiscovery>>,
) -> Option<(&'a ClusterId, &'a ClusterDiscovery)> {
    if let Some(set) = set {
        if !set.has_kinds() {
            return None;
        }
        let key = cluster.as_ref().or_else(|| set.first_member())?;
        return Some((key, set.discovery()));
    }
    Some((cluster.as_ref()?, discovery.as_deref()?))
}

fn same_discovery(a: &Option<Arc<ClusterDiscovery>>, b: &Option<Arc<ClusterDiscovery>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

fn ring(color: Hsla) -> AnyElement {
    status_dot(color, false)
        .mx_1()
        .flex_none()
        .into_any_element()
}

fn dot(color: Hsla) -> AnyElement {
    status_dot(color, true)
        .mx_1()
        .flex_none()
        .into_any_element()
}

impl Render for NavPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut selected_row = None;
        let mut cursor_row = None;
        let focused = self.focus.is_focused(window);
        let cursor = if focused { self.cursor() } else { None };
        match (self.tree(), self.cluster.is_some() || self.set.is_some()) {
            (None, false) => {}
            (None, true) => {
                // A failed connection has its Retry in the main area; only a failed discovery
                // (with a connection) offers one here.
                // An open set's kinds are still coming; the active cluster's state is not theirs.
                let own = self.set.is_none();
                let retry = own
                    && !self.connection_failed
                    && !self.awaiting_confirmation
                    && self.error.is_some();
                rows.push(
                    (v_flex()
                        .flex_none()
                        .px_2()
                        .gap_2()
                        .items_start()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(if own && self.awaiting_confirmation {
                            "Waiting for kubeconfig to be confirmed."
                        } else if own && self.connection_failed {
                            "Connection failed."
                        } else if own && self.error.is_some() {
                            "Discovery failed. Flint retries on its own."
                        } else {
                            "Discovering resources…"
                        })
                        .when(retry, |el| {
                            el.child(
                                Button::new("nav-retry")
                                    .xsmall()
                                    .outline()
                                    .label("Retry")
                                    .on_click(registry::on_click(RetryConnection)),
                            )
                        }))
                    .into_any_element(),
                );
            }
            (Some((cluster, discovery)), _) => {
                let collapsed = self.collapsed.get(cluster).cloned().unwrap_or_default();
                let mut first_group = true;
                // The overview is of the active cluster.
                if self.cluster.is_some() {
                    rows.push(self.render_overview(&theme).into_any_element());
                }
                for (i, section) in discovery.navigation.sections.iter().enumerate() {
                    if section.kind == SectionKind::ApiGroup && first_group {
                        first_group = false;
                        rows.push(
                            (div()
                                .id("nav-custom-resources")
                                .flex_none()
                                .px_2()
                                .pt_3()
                                .pb_1()
                                .text_xs()
                                .font_semibold()
                                .text_color(theme.muted_foreground)
                                .child("CUSTOM RESOURCES"))
                            .into_any_element(),
                        );
                    }
                    if section.kind == SectionKind::Other && !first_group {
                        // Set the remaining built-ins apart from the custom resources above.
                        rows.push(div().flex_none().h(px(8.)).into_any_element());
                    }
                    let is_collapsed = collapsed.is_collapsed(section);
                    let toggle_section = section.title.clone();
                    let ring = cursor.as_ref() == Some(&NavRow::Section(section.title.clone()));
                    if ring {
                        cursor_row = Some(rows.len());
                    }
                    rows.push(
                        (h_flex()
                            .id(("nav-section", i))
                            .flex_none()
                            .aria_label(section.title.clone())
                            .px_1()
                            .h(px(ROW_HEIGHT))
                            .gap_1()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .text_xs()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .hover(|el| el.bg(theme.list_hover))
                            .border_1()
                            .border_color(super::theme::cursor_ring(&theme, ring))
                            .child(
                                Icon::new(if is_collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .xsmall(),
                            )
                            .child(
                                div()
                                    .id(("nav-section-label", i))
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(if section.kind == SectionKind::ApiGroup {
                                        section.title.clone()
                                    } else {
                                        section.title.to_uppercase()
                                    })
                                    // A title cut to the sidebar's width shows whole here.
                                    .tooltip({
                                        let full = section.title.clone();
                                        move |window, cx| {
                                            Tooltip::new(full.clone()).build(window, cx)
                                        }
                                    })
                                    .test_support(),
                            )
                            .when(is_collapsed, |el| {
                                el.child(
                                    div()
                                        .font_features(super::list::tabular_figures())
                                        .child(section.entries.len().to_string()),
                                )
                            })
                            .on_click(registry::on_click(ToggleNavSection {
                                section: toggle_section,
                            }))
                            .test_support())
                        .into_any_element(),
                    );
                    if !is_collapsed {
                        for entry in &section.entries {
                            if self.selected.as_ref() == Some(&entry.id) {
                                selected_row = Some(rows.len());
                            }
                            let ring = cursor.as_ref() == Some(&NavRow::Kind(entry.id.clone()));
                            if ring {
                                cursor_row = Some(rows.len());
                            }
                            rows.push(self.render_entry(entry, &theme, ring));
                        }
                    }
                }
            }
        }
        if let Some(error) = &self.error {
            let full = error.clone();
            rows.push(
                (div()
                    .id("discovery-error")
                    .flex_none()
                    .px_2()
                    .pt_2()
                    .text_xs()
                    .text_color(theme.danger)
                    .truncate()
                    .child(format!("Discovery: {error}"))
                    .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx)))
                .into_any_element(),
            );
        }
        // Bring the open kind into view once, after it was opened from outside the navigation
        // (FLINT_VIEW, or switching back to a cluster).
        if let Some(ix) = selected_row.filter(|_| self.scroll_to_selected) {
            self.scroll_to_selected = false;
            self.scroll.scroll_to_item(ix);
        }
        // And the cursor's, once the arrows moved it.
        if let Some(ix) = cursor_row.filter(|_| self.scroll_to_cursor) {
            self.scroll_to_cursor = false;
            self.scroll.scroll_to_item(ix);
        }
        v_flex()
            .id("nav")
            .key_context(registry::NAVIGATION)
            .track_focus(&self.focus)
            .map(|el| Self::register_actions(el, cx))
            .track_scroll(&self.scroll)
            .size_full()
            .overflow_y_scroll()
            .gap_px()
            .px_1p5()
            .py_1()
            .children(rows)
            .test_support()
    }
}

#[path = "nav_actions.rs"]
mod actions;

#[cfg(any(test, feature = "automation"))]
#[path = "nav_state.rs"]
mod state;

#[cfg(test)]
mod tests {
    use super::{Collapsed, Marker, collapsed_by_default, marker, state_text};
    use flint_core::{KindId, NavEntry, NavSection, RefusalCause, SectionKind, SyncState};

    fn section(title: &str, kind: SectionKind, plural: &str) -> NavSection {
        NavSection {
            title: title.into(),
            kind,
            entries: vec![NavEntry {
                id: KindId::new(
                    if kind == SectionKind::ApiGroup {
                        title
                    } else {
                        ""
                    },
                    plural,
                ),
                label: plural.into(),
                kind: plural.into(),
                api_version: "v1".into(),
                namespaced: true,
                listable: true,
                watchable: true,
            }],
        }
    }

    // covers UP-22
    #[test]
    fn only_workloads_start_open_and_sections_toggle() {
        let workloads = section("Workloads", SectionKind::BuiltIn, "pods");
        let group = section("example.com", SectionKind::ApiGroup, "widgets");
        assert!(!collapsed_by_default(&workloads));
        assert!(collapsed_by_default(&group));
        assert!(collapsed_by_default(&section(
            "Network",
            SectionKind::BuiltIn,
            "services"
        )));
        assert!(collapsed_by_default(&section(
            "Other",
            SectionKind::Other,
            "bindings"
        )));

        let mut collapsed = Collapsed::default();
        assert!(collapsed.is_collapsed(&group));
        collapsed.toggle(&group);
        assert!(!collapsed.is_collapsed(&group));
        collapsed.toggle(&workloads);
        assert!(collapsed.is_collapsed(&workloads));
    }

    // covers UP-22
    #[test]
    fn opening_a_crd_list_reveals_its_group() {
        let sections = vec![
            section("Workloads", SectionKind::BuiltIn, "pods"),
            section("example.com", SectionKind::ApiGroup, "widgets"),
        ];
        let mut collapsed = Collapsed::default();
        collapsed.reveal(&sections, &KindId::new("example.com", "widgets"));
        assert!(!collapsed.is_collapsed(&sections[1]));
        assert!(!collapsed.is_collapsed(&sections[0]));
    }

    // covers UP-27
    #[test]
    fn only_unhealthy_or_loading_kinds_get_a_marker() {
        assert_eq!(marker(&SyncState::Synced), None);
        assert_eq!(marker(&SyncState::Idle), None);
        assert_eq!(marker(&SyncState::Loading), Some(Marker::Loading));
        assert_eq!(
            marker(&SyncState::Reconnecting { error: None }),
            Some(Marker::Reconnecting)
        );
        assert_eq!(
            marker(&SyncState::Failed {
                error: "gone".into(),
                cause: Some(RefusalCause::NotFound),
            }),
            Some(Marker::Failed)
        );
    }

    // covers UP-27
    #[test]
    fn a_denied_kind_is_calm_and_a_missing_one_is_not_called_denied() {
        let refused = |retrying: bool, cause| {
            let error = "refused".to_owned();
            if retrying {
                SyncState::RetryingRefusal { error, cause }
            } else {
                SyncState::Failed { error, cause }
            }
        };
        let denied = Some(RefusalCause::Denied);
        let missing = Some(RefusalCause::NotFound);
        assert_eq!(marker(&refused(true, denied)), Some(Marker::Loading));
        assert_eq!(marker(&refused(false, denied)), None);
        assert_eq!(marker(&refused(true, missing)), Some(Marker::Reconnecting));
        assert_eq!(marker(&refused(false, None)), Some(Marker::Failed));
        let text = |state: SyncState| state_text(Some(&state));
        assert_eq!(text(refused(true, denied)), "checking access: refused");
        assert_eq!(text(refused(false, denied)), "no access: refused");
        assert_eq!(text(refused(true, missing)), "reconnecting: refused");
        assert_eq!(text(refused(false, missing)), "failed: refused");
        assert_eq!(text(refused(false, None)), "failed: refused");
    }
}
