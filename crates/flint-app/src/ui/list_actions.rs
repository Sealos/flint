//! The resource list's registered actions ([`crate::ui::registry`]), their handlers, and the
//! checks the workspace's guards share with them. Row and header clicks dispatch
//! [`SelectRow`] and [`SortBy`], so a click and its action are one path.

use gpui_kit::*;

use super::super::registry::{
    ActionError, Bind, CloseDetail, FocusFilter, OnView, SelectFirst, SelectLast, SelectNext,
    SelectPrev, SelectRow, SortBy, ToggleWide,
};
use super::{ResourceList, find_row, list_model};

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

    /// The position of the object among the rows passing the filters.
    fn row_position(&self, namespace: Option<&str>, name: &str) -> Option<usize> {
        let row = find_row(&self.shown, namespace, name)?;
        self.position_of(&self.shown.rows[row].uid)
    }

    /// [`SelectRow`] needs the object among the rows passing the filters.
    pub(in crate::ui) fn check_select_row(&self, action: &SelectRow) -> Result<(), ActionError> {
        match self.row_position(action.namespace.as_deref(), &action.name) {
            Some(_) => Ok(()),
            None => Err(ActionError::rejected::<SelectRow>(format!(
                "no row {} in the list as shown (check the filters)",
                action.name
            ))),
        }
    }

    /// [`SortBy`] takes a column drawn now.
    pub(in crate::ui) fn check_sort(&self, action: &SortBy) -> Result<(), ActionError> {
        let columns = &self.shown.columns;
        if self
            .drawn
            .iter()
            .any(|&(col, _)| *columns[col].name == action.column)
        {
            Ok(())
        } else {
            Err(ActionError::rejected::<SortBy>(format!(
                "no column {} is drawn",
                action.column
            )))
        }
    }

    /// [`ToggleWide`] does something only when the kind has `-o wide` columns.
    pub(in crate::ui) fn has_wide_columns(&self) -> bool {
        list_model::has_wide_columns(&self.shown.columns)
    }

    /// The selected object's name, for tests.
    #[cfg(test)]
    pub(in crate::ui) fn selected_name(&self) -> Option<String> {
        self.selected.as_ref().map(|s| s.name.clone())
    }

    fn select_row(&mut self, action: &SelectRow, window: &mut Window, cx: &mut Context<Self>) {
        match self.row_position(action.namespace.as_deref(), &action.name) {
            Some(ix) => self.select_index(ix, window, cx),
            None => tracing::warn!(name = action.name, "SelectRow: no such row as shown"),
        }
    }

    fn sort_by(&mut self, action: &SortBy, _: &mut Window, cx: &mut Context<Self>) {
        match self.check_sort(action) {
            Ok(()) => self.toggle_sort(&action.column, cx),
            Err(e) => tracing::warn!("{e}"),
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
