//! The navigation's registered actions ([`crate::ui::registry`]), their handlers, and the
//! checks the workspace's guards share with them. Opening a kind
//! ([`crate::ui::registry::OpenView`]) belongs to the workspace, which owns the open list: the
//! keyboard's Enter on a kind asks it to ([`NavEvent::Open`]).

use flint_core::NavSection;
use gpui_kit::*;

use super::super::registry::{
    ActionError, Bind, CollapseSection, ExpandSection, OnView, OpenClusterOverview,
    OpenSelectedKind, SelectNextKind, SelectPreviousKind, ToggleNavSection,
};
use super::{NavEvent, NavPanel, NavRow};

impl NavPanel {
    /// The navigation actions that work from anywhere in the window, and their handlers, for
    /// [`Bind`]: the workspace forwards these.
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleNavSection>(Self::toggle_section)
            .bind::<OpenClusterOverview>(|_, _, _, cx| cx.emit(NavEvent::OpenOverview))
    }

    /// The keyboard cursor's actions: only while the navigation has focus, so they are bound
    /// on its own element and not forwarded.
    pub fn bind_cursor_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<SelectPreviousKind>(|this, _, _, cx| this.move_cursor(-1, cx))
            .bind::<SelectNextKind>(|this, _, _, cx| this.move_cursor(1, cx))
            .bind::<OpenSelectedKind>(|this, _, _, cx| this.open_cursor(cx))
            .bind::<ExpandSection>(|this, _, _, cx| this.expand_cursor(cx))
            .bind::<CollapseSection>(|this, _, _, cx| this.collapse_cursor(cx))
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        let element = Self::bind_actions(OnView::new(element, cx)).into_element();
        Self::bind_cursor_actions(OnView::new(element, cx)).into_element()
    }

    /// Moves the cursor `step` rows, stopping at the first and the last row, as trees do.
    fn move_cursor(&mut self, step: isize, cx: &mut Context<Self>) {
        let rows = self.rows();
        let Some(at) = self
            .cursor()
            .and_then(|c| rows.iter().position(|r| *r == c))
        else {
            return;
        };
        let next = (at as isize + step).clamp(0, rows.len() as isize - 1) as usize;
        self.cursor = Some(rows[next].clone());
        self.scroll_to_cursor = true;
        cx.notify();
    }

    /// Enter: opens the kind under the cursor, or opens or closes its section.
    fn open_cursor(&mut self, cx: &mut Context<Self>) {
        match self.cursor() {
            Some(NavRow::Kind(kind)) => cx.emit(NavEvent::Open(kind)),
            Some(NavRow::Section(title)) => self.set_collapsed(&title, None, cx),
            None => {}
        }
    }

    /// Right: opens the section under the cursor.
    fn expand_cursor(&mut self, cx: &mut Context<Self>) {
        if let Some(NavRow::Section(title)) = self.cursor() {
            self.set_collapsed(&title, Some(false), cx);
        }
    }

    /// Left: closes the section under the cursor, or the section of the kind under it, which
    /// the cursor moves to.
    fn collapse_cursor(&mut self, cx: &mut Context<Self>) {
        let title = match self.cursor() {
            Some(NavRow::Section(title)) => title,
            Some(NavRow::Kind(kind)) => match self.section_of(&kind) {
                Some(section) => section.title.clone(),
                None => return,
            },
            None => return,
        };
        self.cursor = Some(NavRow::Section(title.clone()));
        self.scroll_to_cursor = true;
        self.set_collapsed(&title, Some(true), cx);
    }

    /// Opens (`Some(false)`), closes (`Some(true)`) or toggles (`None`) the section `title`.
    fn set_collapsed(&mut self, title: &str, collapsed: Option<bool>, cx: &mut Context<Self>) {
        let (Some(cluster), Some(section)) = (self.cluster.clone(), self.section(title)) else {
            return;
        };
        let section = section.clone();
        let sections = self.collapsed.entry(cluster).or_default();
        if collapsed.is_none_or(|c| c != sections.is_collapsed(&section)) {
            sections.toggle(&section);
            cx.notify();
        }
    }

    fn section_of(&self, kind: &flint_core::KindId) -> Option<&NavSection> {
        self.discovery
            .as_ref()?
            .navigation
            .sections
            .iter()
            .find(|s| s.entries.iter().any(|e| e.id == *kind))
    }

    /// Whether the navigation shows sections (a cluster's discovery arrived).
    pub(in crate::ui) fn has_sections(&self) -> bool {
        self.cluster.is_some() && self.discovery.is_some()
    }

    fn section(&self, title: &str) -> Option<&NavSection> {
        self.discovery
            .as_ref()?
            .navigation
            .sections
            .iter()
            .find(|s| s.title.eq_ignore_ascii_case(title))
    }

    /// [`ToggleNavSection`] takes a section title as shown.
    pub(in crate::ui) fn check_section(
        &self,
        action: &ToggleNavSection,
    ) -> Result<(), ActionError> {
        match self.section(&action.section) {
            Some(_) => Ok(()),
            None => Err(ActionError::rejected::<ToggleNavSection>(format!(
                "no section {}",
                action.section
            ))),
        }
    }

    /// Opens or closes the section titled `action.section`.
    fn toggle_section(
        &mut self,
        action: &ToggleNavSection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(cluster), Some(section)) = (self.cluster.clone(), self.section(&action.section))
        else {
            tracing::warn!(
                section = action.section,
                "ToggleNavSection: no such section"
            );
            return;
        };
        let section = section.clone();
        self.collapsed.entry(cluster).or_default().toggle(&section);
        cx.notify();
    }

    /// Whether the section titled `title` is collapsed, for tests.
    #[cfg(test)]
    pub(in crate::ui) fn is_collapsed(&self, title: &str) -> Option<bool> {
        let cluster = self.cluster.as_ref()?;
        let section = self.section(title)?;
        Some(
            self.collapsed
                .get(cluster)
                .cloned()
                .unwrap_or_default()
                .is_collapsed(section),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flint_core::{ClusterDiscovery, NavSection, Navigation, SectionKind};
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::NavPanel;
    use crate::ui::registry::{Scope, ToggleNavSection, tests::assert_binds_scope};

    #[test]
    fn every_navigation_action_is_bound() {
        assert_binds_scope(Scope::Navigation, |names| {
            NavPanel::bind_cursor_actions(NavPanel::bind_actions(names))
        });
    }

    // ToggleNavSection opens and closes a section by title, as clicking its header does.
    #[gpui_kit::test]
    fn toggle_nav_section_opens_and_closes(cx: &mut TestAppContext) {
        let section = |title: &str, kind| NavSection {
            title: title.into(),
            kind,
            entries: Vec::new(),
        };
        let discovery = ClusterDiscovery {
            navigation: Navigation {
                sections: vec![
                    section("Workloads", SectionKind::BuiltIn),
                    section("Network", SectionKind::BuiltIn),
                ],
            },
            kinds: Vec::new(),
        };
        let nav = cx.new(NavPanel::new);
        nav.update(cx, |nav, cx| {
            nav.set_cluster(Some("a".into()), Some(Arc::new(discovery)), None, cx)
        });
        let collapsed =
            |cx: &mut TestAppContext| nav.read_with(cx, |nav, _| nav.is_collapsed("Network"));
        assert_eq!(collapsed(cx), Some(true));
        let toggle = ToggleNavSection {
            section: "network".into(),
        };
        assert!(
            nav.read_with(cx, |nav, _| nav.check_section(&toggle))
                .is_ok()
        );
        let window = cx.add_empty_window();
        window
            .update(|window, cx| nav.update(cx, |nav, cx| nav.toggle_section(&toggle, window, cx)));
        assert_eq!(collapsed(cx), Some(false));
        let missing = ToggleNavSection {
            section: "Nope".into(),
        };
        assert!(
            nav.read_with(cx, |nav, _| nav.check_section(&missing))
                .is_err()
        );
    }

    fn entries(group: &str, plurals: &[&str]) -> Vec<flint_core::NavEntry> {
        plurals
            .iter()
            .map(|plural| flint_core::NavEntry {
                id: flint_core::KindId::new(group, plural),
                label: (*plural).into(),
                kind: (*plural).into(),
                api_version: "v1".into(),
                namespaced: true,
                listable: true,
                watchable: true,
            })
            .collect()
    }

    /// A navigation panel in a window `height` tall, over `sections`.
    fn open_nav(
        cx: &mut TestAppContext,
        height: f32,
        sections: Vec<NavSection>,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<NavPanel>) {
        use gpui_kit::{Bounds, Point, WindowBounds, WindowOptions, px, size};
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::registry::init(cx);
        });
        let discovery = Arc::new(ClusterDiscovery {
            navigation: Navigation { sections },
            kinds: Vec::new(),
        });
        cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(220.), px(height)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, cx| {
                    cx.new(|cx| {
                        let mut nav = NavPanel::new(cx);
                        nav.set_cluster(Some("a".into()), Some(discovery), None, cx);
                        nav
                    })
                },
            )
            .unwrap()
        })
    }

    // covers UP-28: with sections open the rows overflow the column; they must keep their height
    // and scroll, not squeeze together and clip the last header ("OTHER").
    #[gpui_kit::test]
    fn rows_keep_their_height_and_scroll_when_sections_overflow(cx: &mut TestAppContext) {
        use gpui_kit::ScrollDelta;
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{point, px};

        let workloads = [
            "pods",
            "deployments",
            "statefulsets",
            "daemonsets",
            "replicasets",
        ];
        let config = [
            "configmaps",
            "secrets",
            "persistentvolumeclaims",
            "storageclasses",
        ];
        let sections = vec![
            NavSection {
                title: "Workloads".into(),
                kind: SectionKind::BuiltIn,
                entries: entries("", &workloads),
            },
            NavSection {
                title: "Config & Storage".into(),
                kind: SectionKind::BuiltIn,
                entries: entries("", &config),
            },
            NavSection {
                title: "example.com".into(),
                kind: SectionKind::ApiGroup,
                entries: entries("example.com", &["widgets"]),
            },
            NavSection {
                title: "Other".into(),
                kind: SectionKind::Other,
                entries: entries("", &["bindings", "podtemplates"]),
            },
        ];
        // Room for about eight rows: Workloads and Config & Storage open need twelve.
        let (window, nav) = open_nav(cx, 200., sections);
        cx.update_window(window, |_, window, cx| {
            nav.update(cx, |nav, cx| {
                nav.toggle_section(
                    &ToggleNavSection {
                        section: "Config & Storage".into(),
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap();
        cx.run_until_parked();
        let other = ("nav-section", 3usize);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let mut ids: Vec<gpui_kit::ElementId> = workloads
                .iter()
                .chain(&config)
                .map(|plural| format!("nav-{plural}").into())
                .collect();
            ids.extend((0..4usize).map(|i| ("nav-section", i).into()));
            for id in ids {
                let row = window.find(id.clone());
                assert_eq!(
                    f32::from(row.bounds().size.height),
                    24.,
                    "{id:?} is {:?} tall",
                    row.bounds().size.height
                );
            }
            // The last header is below the fold, not squeezed into it.
            let other = window.find(other).bounds();
            assert!(
                f32::from(other.origin.y) >= 200.,
                "OTHER is at {:?}",
                other.origin.y
            );
            window.scroll("nav", ScrollDelta::Pixels(point(px(0.), px(-1000.))), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let header = window.find(other);
            let bounds = header.bounds();
            assert!(header.visible(), "OTHER is not in view after scrolling");
            assert_eq!(f32::from(bounds.size.height), 24.);
            assert!(
                f32::from(bounds.origin.y + bounds.size.height) <= 200.,
                "OTHER ends at {:?}, past the window",
                bounds.origin.y + bounds.size.height
            );
        })
        .unwrap();
    }

    // covers UP-04: a failed connection offers Retry once, in the main area, not again here.
    #[gpui_kit::test]
    fn retry_shows_here_only_for_a_failed_discovery(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;

        let (window, nav) = open_nav(cx, 300., Vec::new());
        let retry_shown = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find("nav-retry").is_some()
            })
            .unwrap()
        };
        nav.update(cx, |nav, cx| {
            nav.set_cluster(Some("b".into()), None, None, cx);
            nav.set_connection_failed(true, cx);
        });
        assert!(!retry_shown(cx));
        nav.update(cx, |nav, cx| {
            nav.set_connection_failed(false, cx);
            nav.set_cluster(Some("b".into()), None, Some("discovery: 503".into()), cx);
        });
        assert!(retry_shown(cx));
    }
}

#[cfg(test)]
#[path = "nav_tests.rs"]
mod view_tests;
