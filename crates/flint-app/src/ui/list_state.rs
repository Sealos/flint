//! The resource list's part of the visible view tree ([`crate::ui::view_state`]).
// module-tests: list_tests.rs

use flint_core::redact::is_safe_secret_column;
use flint_core::table::unix_now;
use flint_core::{Feed, ListFormat, SyncState};

use gpui_kit::*;

use super::super::object::MASK;
use super::super::view_state::{
    Banner, Focus, Level, ListState, RowState, SortState, error_text, sync_word,
};
use super::{ResourceList, cell_text, limited_text, list_model};

impl ResourceList {
    /// The sort in effect, if its column is in the snapshot drawn: its header and direction.
    fn sort_shown(&self) -> Option<(&str, bool)> {
        let col = self.drawn_sort_column()?;
        let descending = self.query.sort.as_ref()?.descending;
        Some((&self.shown.columns[col].name, descending))
    }

    /// The list as drawn: the columns on screen, the rows passing the filters (in order), and
    /// what replaces or tops them.
    pub fn view_state(&self) -> ListState {
        let snapshot = &self.shown;
        let columns: Vec<usize> = if self.drawn.is_empty() {
            list_model::visible_columns(&snapshot.columns, self.show_wide)
        } else {
            self.drawn.iter().map(|&(col, _)| col).collect()
        };
        let secrets = self.kind.as_str() == "secrets";
        let selected = self.selected.as_ref().map(|s| &*s.uid);
        let now = unix_now();
        let rows = self
            .rows
            .iter()
            .take(ListState::ROW_LIMIT)
            .map(|&i| {
                let row = &snapshot.rows[i as usize];
                let cells: Vec<String> = columns
                    .iter()
                    .map(|&col| {
                        if secrets && !is_safe_secret_column(&snapshot.columns[col].name) {
                            MASK.to_owned()
                        } else {
                            cell_text(row, col, now).to_string()
                        }
                    })
                    .collect();
                RowState {
                    id: format!("row-{}", row.uid),
                    namespace: row.namespace.as_deref().map(str::to_owned),
                    name: row.name.to_string(),
                    selected: selected == Some(&*row.uid),
                    cells_by_column: columns
                        .iter()
                        .zip(&cells)
                        .map(|(&col, cell)| (snapshot.columns[col].name.to_string(), cell.clone()))
                        .collect(),
                    cells,
                }
            })
            .collect();
        ListState {
            kind: self.kind.to_string(),
            label: self.label.clone(),
            sync: sync_word(&self.latest.state, self.latest.feed == Feed::Poll),
            total: snapshot.rows.len(),
            shown: self.rows.len(),
            count: self.row_count().text,
            columns: columns
                .iter()
                .map(|&col| snapshot.columns[col].name.to_string())
                .collect(),
            rows,
            rows_truncated: self.rows.len() > ListState::ROW_LIMIT,
            filter: self.query.text.clone(),
            sort: self.sort_shown().map(|(column, descending)| SortState {
                column: column.to_owned(),
                descending,
            }),
            wide: self.show_wide,
            placeholder: self.placeholder_kind(),
            banners: self.banner_states(),
        }
    }

    /// Whether focus is on the rows or in the text filter.
    pub fn focus_in(&self, window: &Window, cx: &App) -> Option<Focus> {
        if self.focus.is_focused(window) {
            Some(Focus::List)
        } else if self.filter.focus_handle(cx).is_focused(window) {
            Some(Focus::ListFilter)
        } else {
            None
        }
    }

    /// Which message replaces the rows; the same cases as `placeholder`.
    fn placeholder_kind(&self) -> Option<&'static str> {
        let snapshot = &self.shown;
        match &snapshot.state {
            SyncState::Idle | SyncState::Loading if snapshot.rows.is_empty() => Some("loading"),
            SyncState::Unavailable => Some("unavailable"),
            SyncState::Failed { .. } if snapshot.rows.is_empty() => Some("failed"),
            SyncState::Reconnecting { .. } if snapshot.rows.is_empty() => Some("reconnecting"),
            _ if self.rows.is_empty() => {
                let filtered = !self.query.text.trim().is_empty()
                    || (snapshot.namespaced && !self.query.namespaces.is_empty());
                Some(if !snapshot.rows.is_empty() && filtered {
                    "no_match"
                } else {
                    "empty"
                })
            }
            _ => None,
        }
    }

    /// The banners over the rows; the same cases as `banners`.
    fn banner_states(&self) -> Vec<Banner> {
        let snapshot = &self.shown;
        let mut banners = Vec::new();
        if let Some(text) = self.filtered_out_text() {
            banners.push(Banner::new("list-filtered-out", Level::Info, text));
        }
        if let Some(limit) = &snapshot.limited_to
            && !limit.listed.is_empty()
        {
            banners.push(Banner::new(
                "list-limited",
                Level::Info,
                limited_text(&self.label, limit),
            ));
        }
        let state = match &snapshot.state {
            SyncState::Reconnecting { error } => Some((
                Level::Warning,
                match error {
                    Some(e) => format!("Reconnecting: {}", error_text(e)),
                    None => "Reconnecting".to_owned(),
                },
            )),
            SyncState::Failed { error } if !snapshot.rows.is_empty() => {
                Some((Level::Danger, format!("Failed: {}", error_text(error))))
            }
            _ if snapshot.format == ListFormat::Fallback => Some((
                Level::Muted,
                "No table: name, namespace and age only".to_owned(),
            )),
            _ => None,
        };
        if let Some((level, text)) = state {
            banners.push(Banner::new("list-banner", level, text));
        }
        banners
    }
}
