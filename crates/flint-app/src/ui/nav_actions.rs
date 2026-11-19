//! The navigation's registered actions ([`crate::ui::registry`]) and their handlers.
//! Opening a kind ([`crate::ui::registry::OpenView`]) belongs to the workspace, which owns the
//! open list.

use gpui_kit::*;

use super::super::registry::{Bind, OnView, ToggleNavSection};
use super::NavPanel;

impl NavPanel {
    /// Every navigation action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleNavSection>(Self::toggle_section)
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }

    /// Opens or closes the section titled `action.section`, as clicking its header does.
    fn toggle_section(
        &mut self,
        action: &ToggleNavSection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(cluster), Some(discovery)) = (&self.cluster, &self.discovery) else {
            return;
        };
        let Some(section) = discovery
            .navigation
            .sections
            .iter()
            .find(|s| s.title.eq_ignore_ascii_case(&action.section))
        else {
            tracing::warn!(
                section = action.section,
                "ToggleNavSection: no such section"
            );
            return;
        };
        self.collapsed
            .entry(cluster.clone())
            .or_default()
            .toggle(section);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    #[test]
    fn every_navigation_action_is_bound() {
        assert_binds_scope(Scope::Navigation, super::NavPanel::bind_actions);
    }
}
