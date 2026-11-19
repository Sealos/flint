//! The detail pane's registered actions ([`crate::ui::registry`]) and their handlers, which do
//! what the pane's buttons, tabs and toggles do.

use gpui_kit::*;

use super::super::registry::{
    Bind, Close, OnView, Refresh, ShowTab, TabName, ToggleAnnotations, ToggleLabels,
    ToggleManagedFields, ToggleSecretValue,
};
use super::{DetailEvent, DetailTab, DetailView};

impl DetailView {
    /// Every detail action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<Refresh>(|view, _, window, cx| view.refresh(window, cx))
            .bind::<Close>(|_, _, _, cx| cx.emit(DetailEvent::Close))
            .bind::<ShowTab>(Self::show_tab)
            .bind::<ToggleManagedFields>(|view, _, window, cx| {
                view.toggle_managed_fields(window, cx)
            })
            .bind::<ToggleLabels>(|view, _, _, cx| {
                view.show_labels = !view.show_labels;
                cx.notify();
            })
            .bind::<ToggleAnnotations>(|view, _, _, cx| {
                view.show_annotations = !view.show_annotations;
                cx.notify();
            })
            .bind::<ToggleSecretValue>(Self::toggle_secret_value)
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }

    fn show_tab(&mut self, action: &ShowTab, _: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab(action.tab);
        cx.notify();
    }

    fn toggle_secret_value(
        &mut self,
        action: &ToggleSecretValue,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_secret {
            return;
        }
        if !self.revealed.remove(&action.key) {
            self.revealed.insert(action.key.clone());
        }
        cx.notify();
    }
}

fn tab(name: TabName) -> DetailTab {
    match name {
        TabName::Overview => DetailTab::Overview,
        TabName::Yaml => DetailTab::Yaml,
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, TabName, tests::assert_binds_scope};

    use super::{DetailTab, tab};

    #[test]
    fn every_detail_action_is_bound() {
        assert_binds_scope(Scope::Detail, super::DetailView::bind_actions);
    }

    #[test]
    fn tab_names_map_to_tabs() {
        assert_eq!(tab(TabName::Overview), DetailTab::Overview);
        assert_eq!(tab(TabName::Yaml), DetailTab::Yaml);
    }
}
