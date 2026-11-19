//! The navigation's registered actions ([`crate::ui::registry`]), their handlers, and the
//! checks the workspace's guards share with them. Opening a kind
//! ([`crate::ui::registry::OpenView`]) belongs to the workspace, which owns the open list.

use flint_core::NavSection;
use gpui_kit::*;

use super::super::registry::{ActionError, Bind, OnView, ToggleNavSection};
use super::NavPanel;

impl NavPanel {
    /// Every navigation action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleNavSection>(Self::toggle_section)
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        Self::bind_actions(OnView::new(element, cx)).into_element()
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
        assert_binds_scope(Scope::Navigation, NavPanel::bind_actions);
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
        let nav = cx.new(|_| NavPanel::new());
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
}
