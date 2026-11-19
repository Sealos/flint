//! The discovery-driven navigation (#7): built-in kinds in fixed sections, then CRDs and
//! aggregated APIs grouped by API group, then the remaining built-ins ("Other"), each kind with
//! a subtle marker of its watch state. Every section but Workloads starts collapsed, so custom
//! resources are reachable without scrolling past every built-in kind.

use std::collections::HashMap;
use std::sync::Arc;

use flint_core::{
    ClusterDiscovery, ClusterId, KindId, NavEntry, NavSection, SectionKind, SyncState,
};
use gpui_kit::component::button::Button;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

pub enum NavEvent {
    Open(NavEntry),
    /// Connect (or discover) again after a failure.
    Retry,
}

impl EventEmitter<NavEvent> for NavPanel {}

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
        SyncState::Reconnecting { .. } => Some(Marker::Reconnecting),
        SyncState::Failed { .. } => Some(Marker::Failed),
        SyncState::Idle | SyncState::Synced | SyncState::Unavailable => None,
    }
}

pub struct NavPanel {
    cluster: Option<ClusterId>,
    discovery: Option<Arc<ClusterDiscovery>>,
    error: Option<String>,
    collapsed: HashMap<ClusterId, Collapsed>,
    states: HashMap<KindId, SyncState>,
    selected: Option<KindId>,
    scroll: ScrollHandle,
    scroll_to_selected: bool,
    /// The cluster's connection failed, so no discovery is coming.
    connection_failed: bool,
}

impl NavPanel {
    pub fn new() -> Self {
        NavPanel {
            cluster: None,
            discovery: None,
            error: None,
            collapsed: HashMap::new(),
            states: HashMap::new(),
            selected: None,
            scroll: ScrollHandle::new(),
            scroll_to_selected: false,
            connection_failed: false,
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
            self.selected = None;
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

    pub fn set_connection_failed(&mut self, failed: bool, cx: &mut Context<Self>) {
        if self.connection_failed != failed {
            self.connection_failed = failed;
            cx.notify();
        }
    }

    pub fn set_states(&mut self, states: Vec<(KindId, SyncState)>, cx: &mut Context<Self>) {
        let states: HashMap<KindId, SyncState> = states.into_iter().collect();
        if states != self.states {
            self.states = states;
            cx.notify();
        }
    }

    pub fn set_selected(&mut self, kind: Option<KindId>, cx: &mut Context<Self>) {
        if let (Some(kind), Some(cluster), Some(discovery)) =
            (&kind, &self.cluster, &self.discovery)
        {
            self.collapsed
                .entry(cluster.clone())
                .or_default()
                .reveal(&discovery.navigation.sections, kind);
        }
        self.scroll_to_selected = self.selected != kind;
        self.selected = kind;
        cx.notify();
    }

    fn render_entry(&self, entry: &NavEntry, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let is_selected = self.selected.as_ref() == Some(&entry.id);
        let state = self.states.get(&entry.id);
        let mark = state.and_then(marker);
        let state_text = match state {
            None | Some(SyncState::Idle) => "not watched".to_owned(),
            Some(SyncState::Loading) => "loading".to_owned(),
            Some(SyncState::Synced) => "live".to_owned(),
            Some(SyncState::Reconnecting { error }) => match error {
                Some(e) => format!("reconnecting: {e}"),
                None => "reconnecting".to_owned(),
            },
            Some(SyncState::Failed { error }) => format!("failed: {error}"),
            Some(SyncState::Unavailable) => "unavailable".to_owned(),
        };
        let listable = entry.listable;
        let tip = format!(
            "{}\n{} {}{}\n{}",
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
            }
        );
        let open = entry.clone();
        h_flex()
            .id(SharedString::from(format!("nav-{}", entry.id)))
            .pl_5()
            .pr_2()
            .h(px(24.))
            .gap_1()
            .rounded_md()
            .text_sm()
            .when(!listable, |el| {
                el.text_color(theme.muted_foreground.opacity(0.6))
            })
            .when(listable, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(NavEvent::Open(open.clone()))))
            })
            .when(is_selected, |el| {
                el.bg(theme.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
            })
            .when(!is_selected && listable, |el| {
                el.hover(|el| el.bg(theme.sidebar_accent.opacity(0.5)))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(entry.label.clone()),
            )
            .children(mark.map(|m| match m {
                // Static, not a spinner: dozens of kinds load at once after activation, and
                // animating them all would keep the window redrawing.
                Marker::Loading => ring(theme.muted_foreground),
                Marker::Reconnecting => dot(theme.warning),
                Marker::Failed => dot(theme.danger),
            }))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .into_any_element()
    }
}

impl Default for NavPanel {
    fn default() -> Self {
        Self::new()
    }
}

fn same_discovery(a: &Option<Arc<ClusterDiscovery>>, b: &Option<Arc<ClusterDiscovery>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

fn ring(color: Hsla) -> AnyElement {
    div()
        .size_1p5()
        .mx_1()
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(color)
        .into_any_element()
}

fn dot(color: Hsla) -> AnyElement {
    div()
        .size_1p5()
        .mx_1()
        .flex_none()
        .rounded_full()
        .bg(color)
        .into_any_element()
}

impl Render for NavPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut selected_row = None;
        match (&self.cluster, &self.discovery) {
            (None, _) => {}
            (Some(_), None) => {
                let failed = self.connection_failed || self.error.is_some();
                rows.push(
                    (v_flex()
                        .px_2()
                        .gap_2()
                        .items_start()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(if self.connection_failed {
                            "Connection failed."
                        } else if self.error.is_some() {
                            "Discovery failed. Flint retries on its own."
                        } else {
                            "Discovering resources…"
                        })
                        .when(failed, |el| {
                            el.child(
                                Button::new("nav-retry")
                                    .xsmall()
                                    .outline()
                                    .label("Retry")
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(NavEvent::Retry))),
                            )
                        }))
                    .into_any_element(),
                );
            }
            (Some(cluster), Some(discovery)) => {
                let collapsed = self.collapsed.get(cluster).cloned().unwrap_or_default();
                let mut first_group = true;
                for (i, section) in discovery.navigation.sections.iter().enumerate() {
                    if section.kind == SectionKind::ApiGroup && first_group {
                        first_group = false;
                        rows.push(
                            (div()
                                .px_2()
                                .pt_3()
                                .pb_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("CUSTOM RESOURCES"))
                            .into_any_element(),
                        );
                    }
                    if section.kind == SectionKind::Other && !first_group {
                        // Set the remaining built-ins apart from the custom resources above.
                        rows.push(div().h(px(8.)).into_any_element());
                    }
                    let is_collapsed = collapsed.is_collapsed(section);
                    let toggle_section = section.clone();
                    let cluster = cluster.clone();
                    rows.push(
                        (h_flex()
                            .id(("nav-section", i))
                            .px_1()
                            .h(px(24.))
                            .gap_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_xs()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .hover(|el| el.bg(theme.sidebar_accent.opacity(0.5)))
                            .child(
                                Icon::new(if is_collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .xsmall(),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(
                                if section.kind == SectionKind::ApiGroup {
                                    section.title.clone()
                                } else {
                                    section.title.to_uppercase()
                                },
                            ))
                            .when(is_collapsed, |el| {
                                el.child(
                                    div()
                                        .text_color(theme.muted_foreground.opacity(0.7))
                                        .child(section.entries.len().to_string()),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.collapsed
                                    .entry(cluster.clone())
                                    .or_default()
                                    .toggle(&toggle_section);
                                cx.notify();
                            })))
                        .into_any_element(),
                    );
                    if !is_collapsed {
                        for entry in &section.entries {
                            if self.selected.as_ref() == Some(&entry.id) {
                                selected_row = Some(rows.len());
                            }
                            rows.push(self.render_entry(entry, &theme, cx));
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
        v_flex()
            .id("nav")
            .track_scroll(&self.scroll)
            .size_full()
            .overflow_y_scroll()
            .gap_px()
            .p_2()
            .children(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::{Collapsed, Marker, collapsed_by_default, marker};
    use flint_core::{KindId, NavEntry, NavSection, SectionKind, SyncState};

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
                error: "forbidden".into()
            }),
            Some(Marker::Failed)
        );
    }
}
