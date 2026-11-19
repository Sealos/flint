//! The resource list's registered actions ([`crate::ui::registry`]) and their handlers. Each
//! handler takes the path its control does: [`SelectRow`] selects like a row click,
//! [`SortBy`] like a header click.

use gpui_kit::*;

use super::super::registry::{
    Bind, CloseDetail, FocusFilter, OnView, SelectFirst, SelectLast, SelectNext, SelectPrev,
    SelectRow, SortBy, ToggleWide,
};
use super::{ResourceList, find_row};

impl ResourceList {
    /// Every list action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<SelectPrev>(Self::select_prev)
            .bind::<SelectNext>(Self::select_next)
            .bind::<SelectFirst>(Self::select_first)
            .bind::<SelectLast>(Self::select_last)
            .bind::<CloseDetail>(Self::close_detail)
            .bind::<SelectRow>(Self::select_row)
            .bind::<SortBy>(Self::sort_by)
            .bind::<ToggleWide>(Self::toggle_wide)
            .bind::<FocusFilter>(Self::focus_filter)
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }

    fn select_row(&mut self, action: &SelectRow, window: &mut Window, cx: &mut Context<Self>) {
        let uid = find_row(&self.shown, action.namespace.as_deref(), &action.name)
            .map(|i| self.shown.rows[i].uid.clone());
        match uid.and_then(|uid| self.position_of(&uid)) {
            Some(ix) => self.select_index(ix, window, cx),
            None => tracing::warn!(
                name = action.name,
                "SelectRow: no such row in the list as shown"
            ),
        }
    }

    fn sort_by(&mut self, action: &SortBy, _: &mut Window, cx: &mut Context<Self>) {
        if self.shown.columns.iter().any(|c| *c.name == action.column) {
            self.toggle_sort(&action.column, cx);
        } else {
            tracing::warn!(column = action.column, "SortBy: no such column");
        }
    }

    fn toggle_wide(&mut self, _: &ToggleWide, _: &mut Window, cx: &mut Context<Self>) {
        self.show_wide = !self.show_wide;
        cx.notify();
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.filter.focus_handle(cx);
        window.focus(&focus, cx);
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    // The handlers run through a window in `list_tests.rs` (`registered_actions_drive_the_list`).
    #[test]
    fn every_list_action_is_bound() {
        assert_binds_scope(Scope::List, super::ResourceList::bind_actions);
    }
}
