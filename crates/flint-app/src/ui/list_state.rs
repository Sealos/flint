//! The resource list's part of the visible view tree ([`crate::ui::view_state`]).
// module-tests: list_tests.rs

use flint_core::combined::CombinedCell;
use flint_core::redact::{is_masked_column, is_secret_list};
use flint_core::table::unix_now;
use flint_core::{Feed, ListFormat, SyncState};

use gpui_kit::*;

use super::super::list_usage::NOT_INSTALLED_HINT;
use super::super::object::MASK;
use super::super::view_state::{
    Banner, Focus, Level, ListState, RowState, SortState, error_text, sync_word,
};
use super::combined::{self, CombinedResourceList};
use super::{ResourceList, limited_text, list_model};

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "opening a combined list is wired in later")
)]
impl CombinedResourceList {
    /// The list as drawn: the Cluster column first, a row's cells read through its own member,
    /// and a banner per member that is not current.
    pub fn view_state(&self) -> ListState {
        let list = &self.shown.list;
        let columns: Vec<usize> = if self.drawn.is_empty() {
            list_model::visible_columns(&self.shown.columns, self.show_wide)
        } else {
            self.drawn.iter().map(|&(col, _)| col).collect()
        };
        let names: Vec<String> = columns
            .iter()
            .map(|&col| list.columns()[col].name.to_string())
            .collect();
        let now = unix_now();
        let rows = self
            .view
            .rows()
            .iter()
            .take(ListState::ROW_LIMIT)
            .filter_map(|&at| {
                let info = list.row_info(at)?;
                let id = list.row_id(at)?;
                let cells: Vec<String> = columns
                    .iter()
                    .map(|&col| match list.cell(at, col, now) {
                        CombinedCell::Cluster { name, .. } => name.to_owned(),
                        CombinedCell::Value(text) => text.into_owned(),
                        CombinedCell::Blank => String::new(),
                        CombinedCell::Masked => MASK.to_owned(),
                    })
                    .collect();
                Some(RowState {
                    id: combined::row_id(&id.cluster, &id.uid),
                    namespace: info.namespace.map(str::to_owned),
                    name: info.name.to_owned(),
                    selected: self.selected.as_ref() == Some(&id),
                    // Reversed so the first of two equal headers wins: Flint's Cluster column
                    // over a server column also named Cluster.
                    cells_by_column: names
                        .iter()
                        .cloned()
                        .zip(cells.iter().cloned())
                        .rev()
                        .collect(),
                    cells,
                })
            })
            .collect();
        let members = list.members();
        let feed = |i: usize| self.shown.feeds[i] == Feed::Poll;
        let sync = members
            .iter()
            .enumerate()
            .find(|(_, m)| !m.state().is_current())
            .or(members.iter().enumerate().next())
            .map_or("not watched", |(i, m)| sync_word(m.state(), feed(i)));
        let banners = members
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.state().is_current())
            .map(|(i, m)| {
                let level = match m.state() {
                    SyncState::Failed { .. } => Level::Danger,
                    SyncState::Reconnecting { .. } | SyncState::RetryingRefusal { .. } => {
                        Level::Warning
                    }
                    _ => Level::Muted,
                };
                Banner::new(
                    &format!("list-member-{}", m.cluster()),
                    level,
                    format!("{}: {}", m.display_name(), sync_word(m.state(), feed(i))),
                )
            })
            .chain(members.iter().enumerate().filter_map(|(i, m)| {
                let limit = self.shown.coverage[i].limit.as_ref()?;
                Some(Banner::new(
                    &format!("list-limited-{}", m.cluster()),
                    Level::Info,
                    format!(
                        "{}: {}",
                        m.display_name(),
                        combined::limit_detail(&self.label, limit)
                    ),
                ))
            }))
            .collect();
        ListState {
            kind: self
                .shown
                .kind
                .as_ref()
                .map_or_else(String::new, ToString::to_string),
            label: self.label.clone(),
            sync,
            total: self.shown.total_rows(),
            shown: self.view.matched(),
            count: self.count(),
            columns: names,
            rows,
            rows_truncated: self.view.rows().len() > ListState::ROW_LIMIT,
            filter: self.query.text.clone(),
            sort: self.applied_sort.as_ref().and_then(|sort| {
                let col = list.column_index(&sort.column)?;
                Some(SortState {
                    column: list.columns()[col].name.to_string(),
                    descending: sort.descending,
                })
            }),
            wide: self.show_wide,
            placeholder: self.empty_state().map(|empty| match empty {
                list_model::combined::EmptyState::Loading => "loading",
                list_model::combined::EmptyState::NoRows
                | list_model::combined::EmptyState::NoRowsListed => "empty",
                list_model::combined::EmptyState::NoMatch => "no_match",
                list_model::combined::EmptyState::Unresolved => "failed",
            }),
            banners,
        }
    }
}

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
        let secrets = is_secret_list(self.kind.as_str());
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
                        if is_masked_column(secrets, &snapshot.columns, col) {
                            MASK.to_owned()
                        } else {
                            self.shown_cell(i as usize, col, now).to_string()
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
            s if snapshot.rows.is_empty() && s.is_checking_access() => Some("checking_access"),
            s if snapshot.rows.is_empty() && s.is_no_access() => Some("no_access"),
            SyncState::Failed { .. } if snapshot.rows.is_empty() => Some("failed"),
            SyncState::Reconnecting { .. } | SyncState::RetryingRefusal { .. }
                if snapshot.rows.is_empty() =>
            {
                Some("reconnecting")
            }
            _ if self.restoring_filter => Some("loading"),
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
        if let Some(text) = self.missing_text() {
            banners.push(Banner::new("list-missing", Level::Warning, text));
        }
        if let Some(text) = self.filtered_out_text() {
            banners.push(Banner::new("list-filtered-out", Level::Info, text));
        }
        if self.usage.not_installed() {
            banners.push(Banner::new(
                "list-usage-hint",
                Level::Muted,
                NOT_INSTALLED_HINT,
            ));
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
        let has_rows = !snapshot.rows.is_empty();
        let state = match &snapshot.state {
            s @ SyncState::RetryingRefusal { .. } if s.is_checking_access() && !has_rows => None,
            s @ SyncState::RetryingRefusal { error, .. } if s.is_checking_access() => Some((
                Level::Muted,
                format!("Checking access: {}", error_text(error)),
            )),
            s @ SyncState::Failed { error, .. } if s.is_no_access() && has_rows => {
                Some((Level::Muted, format!("No access: {}", error_text(error))))
            }
            SyncState::Reconnecting { error: Some(e) }
            | SyncState::RetryingRefusal { error: e, .. } => {
                Some((Level::Warning, format!("Reconnecting: {}", error_text(e))))
            }
            SyncState::Reconnecting { error: None } => {
                Some((Level::Warning, "Reconnecting".to_owned()))
            }
            SyncState::Failed { error, .. } if !snapshot.rows.is_empty() => {
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
