//! The resource list's registered actions ([`crate::ui::registry`]), their handlers, and the
//! checks the workspace's guards share with them. Row and header clicks dispatch
//! [`SelectRow`] and [`SortBy`], so a click and its action are one path.

use gpui_kit::*;

use super::super::registry::{
    ActionError, Bind, CloseDetail, FocusFilter, FocusRows, OnView, OpenSelectedRow, SelectFirst,
    SelectFromFilter, SelectLast, SelectNext, SelectPrev, SelectRow, SortBy, ToggleProblemsOnly,
    ToggleWide,
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
            .bind::<ToggleProblemsOnly>(Self::toggle_problems)
            .bind::<FocusFilter>(Self::focus_filter)
            .bind::<FocusRows>(Self::focus_rows)
            .bind::<SelectFromFilter>(Self::select_from_filter)
            .bind::<OpenSelectedRow>(Self::open_selected_row)
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

    /// [`SortBy`] takes a column drawn now that is not masked: by its `key`
    /// ([`flint_core::Column::key`]) when given, else by its header.
    pub(in crate::ui) fn check_sort(&self, action: &SortBy) -> Result<(), ActionError> {
        if self.sort_key(action).is_some() {
            Ok(())
        } else {
            Err(ActionError::rejected::<SortBy>(format!(
                "no sortable column {} is drawn",
                action.key.as_deref().unwrap_or(&action.column)
            )))
        }
    }

    /// The key of the drawn, unmasked column `action` names: the one with its `key` when given,
    /// else the first one headed `column`.
    pub(in crate::ui) fn sort_key(&self, action: &SortBy) -> Option<String> {
        let columns = &self.shown.columns;
        let mut drawn = self
            .drawn
            .iter()
            .filter(|&&(col, _)| !self.is_masked(columns, col))
            .map(|&(col, _)| &columns[col]);
        let found = match &action.key {
            Some(key) => drawn.find(|c| c.key() == *key),
            None => drawn.find(|c| *c.name == *action.column),
        };
        found.map(|c| c.key())
    }

    /// The names of the unmasked columns drawn, left to right: what [`SortBy`] takes.
    pub(in crate::ui) fn drawn_column_names(&self) -> Vec<String> {
        self.drawn
            .iter()
            .filter(|&&(col, _)| !self.is_masked(&self.shown.columns, col))
            .map(|&(col, _)| self.shown.columns[col].name.to_string())
            .collect()
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

    /// The text the rows are filtered by.
    pub(in crate::ui) fn filter_text(&self) -> &str {
        &self.query.text
    }

    /// Puts back the filter a list was left with (Go Back). The rows drawn for the old text go
    /// at once: until the new text's rows arrive there is no row to click, open a menu on or
    /// move to, so no action can land on a row the filter hides.
    pub(in crate::ui) fn restore_filter(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The field can already hold the text while the rows drawn are still another text's.
        let field_matches = self.query.text == text;
        if field_matches && (self.restoring_filter || self.rows_text == text) {
            return;
        }
        self.query.text = text.to_owned();
        self.select_first_in = None;
        self.restoring_filter = true;
        self.rows = std::sync::Arc::from(Vec::new());
        self.selected_ix = None;
        if !field_matches {
            self.filter
                .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        }
        self.recompute(cx);
        cx.notify();
    }

    fn select_row(&mut self, action: &SelectRow, window: &mut Window, cx: &mut Context<Self>) {
        match self.row_position(action.namespace.as_deref(), &action.name) {
            Some(ix) => self.select_index(ix, window, cx),
            None => tracing::warn!(name = action.name, "SelectRow: no such row as shown"),
        }
    }

    fn sort_by(&mut self, action: &SortBy, _: &mut Window, cx: &mut Context<Self>) {
        match self.check_sort(action) {
            Ok(()) => self.toggle_sort(action, cx),
            Err(e) => tracing::warn!("{e}"),
        }
    }

    fn toggle_wide(&mut self, _: &ToggleWide, _: &mut Window, cx: &mut Context<Self>) {
        self.show_wide = !self.show_wide;
        // A remembered sort on a wide column applies only while Wide is on.
        self.recompute(cx);
        cx.notify();
    }

    fn toggle_problems(&mut self, _: &ToggleProblemsOnly, _: &mut Window, cx: &mut Context<Self>) {
        self.problems.toggle();
        self.recompute(cx);
        cx.notify();
    }

    /// Whether the Problems toggle applies to this kind.
    pub(in crate::ui) fn has_problem_filter(&self) -> bool {
        super::problems::ProblemListKind::of(&self.kind).is_some()
    }

    /// Takes the newest problems of this list's cluster.
    pub(in crate::ui) fn set_problems(
        &mut self,
        values: std::collections::HashMap<flint_core::ClusterId, super::problems::ClusterProblems>,
        cx: &mut Context<Self>,
    ) {
        if self.problems.update(values) {
            if self.problems.is_on() {
                self.recompute(cx);
            }
            cx.notify();
        }
    }

    /// Says when the toggle is on and this kind could not be checked.
    pub(super) fn problems_note(&self) -> Option<String> {
        let cluster = self.cluster.as_str();
        self.problems
            .unchecked_note(&self.kind, &self.label, std::iter::once((cluster, cluster)))
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.select_first_in = None;
        let focus = self.filter.focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// Back to the rows from the filter; the filter keeps its text.
    fn focus_rows(&mut self, _: &FocusRows, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Down in the filter: back to the rows, on the first one if nothing is selected or waiting
    /// to be. A selection the filter hides stays selected. While the filter's rows are being
    /// computed, the selection waits for them, so it never lands on a row the filter hides.
    fn select_from_filter(
        &mut self,
        _: &SelectFromFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        if self.computing {
            self.select_first_in = Some(window.window_handle());
        } else {
            self.select_first_if_none(window, cx);
        }
        cx.notify();
    }

    /// Enter on the rows: asks for the selected row as a click does, so a narrow window brings
    /// its detail to the front. The key stays with a control or dialog that has focus, and a
    /// selection that is not among the rows shown opens nothing.
    fn open_selected_row(
        &mut self,
        _: &OpenSelectedRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus.is_focused(window)
            || gpui_kit::component::WindowExt::has_active_dialog(window, cx)
        {
            cx.propagate();
            return;
        }
        let Some(selected) = &self.selected else {
            return;
        };
        if self.position_of(&selected.uid).is_some() {
            let row = SelectRow {
                namespace: selected.namespace.clone(),
                name: selected.name.clone(),
            };
            window.dispatch_action(Box::new(row), cx);
        }
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
