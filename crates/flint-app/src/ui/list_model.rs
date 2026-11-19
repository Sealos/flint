//! The resource list's view-model logic, kept free of GPUI so it can be unit tested: which
//! columns show, how a sort survives column changes, and how wide each column is.

use std::sync::Arc;

use flint_core::{Column, ColumnRole, ColumnType, KindSnapshot, SortBy};

/// The user's sort choice, by column name. Columns can change between snapshots (an edited
/// CRD's printer columns), so the index is looked up again for every snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortSpec {
    pub column: String,
    pub descending: bool,
}

/// Indexes of the columns to show: every default column, plus the `-o wide` ones (priority 1
/// and up) when `show_wide` is set.
pub fn visible_columns(columns: &[Column], show_wide: bool) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, c)| show_wide || c.priority == 0)
        .map(|(i, _)| i)
        .collect()
}

/// Whether the kind has any `-o wide` columns, so the toggle is worth showing.
pub fn has_wide_columns(columns: &[Column]) -> bool {
    columns.iter().any(|c| c.priority > 0)
}

/// The sort for `compute_view`: `spec`'s column looked up by name in `columns`. `None` when
/// nothing is sorted or the column no longer exists, which keeps the snapshot order (namespace,
/// then name).
pub fn resolve_sort(columns: &[Column], spec: Option<&SortSpec>) -> Option<SortBy> {
    let spec = spec?;
    let column = columns.iter().position(|c| *c.name == *spec.column)?;
    Some(SortBy {
        column,
        descending: spec.descending,
    })
}

/// The sort after clicking a column header: a new column sorts ascending, then descending, then
/// back to the default order.
pub fn next_sort(current: Option<&SortSpec>, clicked: &str) -> Option<SortSpec> {
    match current {
        Some(s) if s.column == clicked && !s.descending => Some(SortSpec {
            column: clicked.to_owned(),
            descending: true,
        }),
        Some(s) if s.column == clicked => None,
        _ => Some(SortSpec {
            column: clicked.to_owned(),
            descending: false,
        }),
    }
}

/// Numbers, ages and times: drawn right-aligned with tabular figures.
pub fn is_numeric(column: &Column) -> bool {
    column.role == ColumnRole::Age
        || matches!(
            column.ty,
            ColumnType::Integer | ColumnType::Number | ColumnType::Date
        )
}

/// Width limits in characters. The name column is as wide as its longest name, between
/// `NAME_MIN_CHARS` and `MAX_CHARS`; a longer name is truncated (with a tooltip). Spare width
/// goes to a filler after the last column, so the other columns stay next to the names.
const MIN_CHARS: usize = 4;
const MAX_CHARS: usize = 48;
pub const NAME_MIN_CHARS: usize = 16;
const AGE_CHARS: usize = 7;
/// Rows sampled per column when measuring. Enough to see the widths that matter without
/// walking 10,000 rows on every update.
const SAMPLE_ROWS: usize = 2_000;

/// Each column's width in characters: the longest of its header and the sampled cells, within
/// limits. Runs on a background thread; it is the only per-row work outside `compute_view`.
pub fn measure_columns(snapshot: &KindSnapshot, now: i64) -> Vec<usize> {
    let step = (snapshot.rows.len() / SAMPLE_ROWS).max(1);
    snapshot
        .columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let cells = snapshot
                .rows
                .iter()
                .step_by(step)
                .map(|row| row.cell_text(i, now).chars().count())
                .max()
                .unwrap_or(0);
            // The header also holds the sort arrow.
            let header = column.name.chars().count() + 2;
            cells.max(header).clamp(MIN_CHARS, MAX_CHARS)
        })
        .collect()
}

/// Widths that only grow while the same columns are shown, so columns do not jitter as rows
/// change. New columns (a different `Arc`) start over.
#[derive(Debug, Default, Clone)]
pub struct ColumnWidths {
    columns: Option<Arc<[Column]>>,
    chars: Vec<usize>,
}

impl ColumnWidths {
    /// Folds a measurement of `columns` in.
    pub fn update(&mut self, columns: &Arc<[Column]>, measured: &[usize]) {
        let same = self
            .columns
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, columns) || **c == **columns);
        if !same || self.chars.len() != measured.len() {
            self.columns = Some(columns.clone());
            self.chars = measured.to_vec();
            return;
        }
        for (have, new) in self.chars.iter_mut().zip(measured) {
            *have = (*have).max(*new);
        }
    }

    /// Width in characters of column `i`, or a default before anything was measured.
    pub fn chars(&self, i: usize, column: &Column) -> usize {
        let chars = self
            .chars
            .get(i)
            .copied()
            .unwrap_or_else(|| (column.name.chars().count() + 2).max(8));
        if column.role == ColumnRole::Name {
            return chars.max(NAME_MIN_CHARS);
        }
        // Ages keep counting without new snapshots (and so without new measurements): leave
        // room for the longest form `kubectl` prints ("5m30s", "3y45d").
        if column.role == ColumnRole::Age || column.ty == ColumnType::Date {
            chars.max(AGE_CHARS)
        } else {
            chars
        }
    }
}

/// The columns to draw when `available` points are free: every default column, plus the
/// `-o wide` ones when `show_wide` is set and they fit. When even the default columns do not fit,
/// the table scrolls sideways. Returns the columns with their widths, and how many wide columns
/// were left out for lack of room.
pub fn fit_columns(
    columns: &[Column],
    width_of: impl Fn(usize) -> f32,
    show_wide: bool,
    available: Option<f32>,
) -> (Vec<(usize, f32)>, usize) {
    let all: Vec<(usize, f32)> = visible_columns(columns, show_wide)
        .into_iter()
        .map(|i| (i, width_of(i)))
        .collect();
    let total: f32 = all.iter().map(|(_, w)| w).sum();
    match available {
        Some(room) if show_wide && total > room => {
            // Drop wide columns from the right until the rest fits (or none is left).
            let mut kept = all;
            let mut dropped = 0;
            let mut total = total;
            while total > room {
                let Some(pos) = kept.iter().rposition(|(i, _)| columns[*i].priority > 0) else {
                    break;
                };
                total -= kept[pos].1;
                kept.remove(pos);
                dropped += 1;
            }
            (kept, dropped)
        }
        _ => (all, 0),
    }
}

/// Whether the rows show something that changes with time alone (ages, timestamps), so the list
/// needs redrawing every second.
pub fn has_ticking_columns(columns: &[Column]) -> bool {
    columns
        .iter()
        .any(|c| c.role == ColumnRole::Age || c.ty == ColumnType::Date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_core::{Cell, Row};

    fn col(name: &str, priority: i32, role: ColumnRole, ty: ColumnType) -> Column {
        Column {
            name: name.into(),
            ty,
            format: "".into(),
            description: "".into(),
            priority,
            role,
        }
    }

    fn pod_columns() -> Vec<Column> {
        vec![
            col("Name", 0, ColumnRole::Name, ColumnType::String),
            col("Namespace", 0, ColumnRole::Namespace, ColumnType::String),
            col("Ready", 0, ColumnRole::Value, ColumnType::String),
            col("Restarts", 0, ColumnRole::Value, ColumnType::Integer),
            col("Age", 0, ColumnRole::Age, ColumnType::String),
            col("IP", 1, ColumnRole::Value, ColumnType::String),
            col("Node", 1, ColumnRole::Value, ColumnType::String),
        ]
    }

    // covers UP-20
    #[test]
    fn wide_columns_are_hidden_until_toggled() {
        let columns = pod_columns();
        assert_eq!(visible_columns(&columns, false), vec![0, 1, 2, 3, 4]);
        assert_eq!(visible_columns(&columns, true), vec![0, 1, 2, 3, 4, 5, 6]);
        assert!(has_wide_columns(&columns));
        assert!(!has_wide_columns(&columns[..5]));
    }

    // covers UP-21
    #[test]
    fn header_clicks_cycle_ascending_descending_default() {
        let asc = next_sort(None, "Age");
        assert_eq!(
            asc,
            Some(SortSpec {
                column: "Age".into(),
                descending: false
            })
        );
        let desc = next_sort(asc.as_ref(), "Age");
        assert_eq!(desc.as_ref().map(|s| s.descending), Some(true));
        assert_eq!(next_sort(desc.as_ref(), "Age"), None);
        // Another column starts ascending again.
        let other = next_sort(desc.as_ref(), "Name").unwrap();
        assert_eq!((other.column.as_str(), other.descending), ("Name", false));
    }

    // covers UP-21, UP-22
    #[test]
    fn sort_follows_its_column_by_name_when_columns_change() {
        let spec = SortSpec {
            column: "Restarts".into(),
            descending: true,
        };
        let columns = pod_columns();
        assert_eq!(
            resolve_sort(&columns, Some(&spec)),
            Some(SortBy {
                column: 3,
                descending: true
            })
        );
        // A CRD edit inserts a column before it: the index moves, the sort stays on Restarts.
        let mut changed = columns.clone();
        changed.insert(2, col("Phase", 0, ColumnRole::Value, ColumnType::String));
        assert_eq!(
            resolve_sort(&changed, Some(&spec)),
            Some(SortBy {
                column: 4,
                descending: true
            })
        );
        // The column is gone: back to the default order rather than sorting by a stranger.
        changed.retain(|c| &*c.name != "Restarts");
        assert_eq!(resolve_sort(&changed, Some(&spec)), None);
        assert_eq!(resolve_sort(&columns, None), None);
    }

    #[test]
    fn numbers_and_ages_are_numeric() {
        let columns = pod_columns();
        let numeric: Vec<bool> = columns.iter().map(is_numeric).collect();
        assert_eq!(numeric, [false, false, false, true, true, false, false]);
    }

    fn row(name: &str, ready: &str) -> Arc<Row> {
        Arc::new(Row {
            uid: name.into(),
            name: name.into(),
            namespace: Some("default".into()),
            resource_version: "1".into(),
            generation: None,
            created: Some(0),
            labels: Box::new([]),
            cells: Box::new([Cell::Name, Cell::Namespace, Cell::Text(ready.into())]),
        })
    }

    #[test]
    fn widths_fit_content_within_limits_and_only_grow() {
        let columns: Arc<[Column]> = pod_columns()[..3].to_vec().into();
        let snapshot = KindSnapshot {
            columns: columns.clone(),
            rows: vec![row("a", "1/1"), row(&"x".repeat(80), "1/1")],
            ..Default::default()
        };
        let measured = measure_columns(&snapshot, 10);
        // Capped name, "Namespace" header (+2 for the arrow), and "Ready" header beats "1/1".
        assert_eq!(measured, vec![MAX_CHARS, 11, 7]);

        let mut widths = ColumnWidths::default();
        widths.update(&columns, &measured);
        widths.update(&columns, &[30, 11, 5]);
        assert_eq!(widths.chars(2, &columns[2]), 7, "never shrinks");
        assert_eq!(widths.chars(1, &columns[1]), 11);
        assert_eq!(
            widths.chars(0, &columns[0]),
            MAX_CHARS,
            "the name column is as wide as the longest name, capped"
        );

        // New columns start over; the name column keeps its minimum.
        let other: Arc<[Column]> = pod_columns()[..1].to_vec().into();
        widths.update(&other, &[5]);
        assert_eq!(widths.chars(0, &other[0]), NAME_MIN_CHARS);
    }

    // covers UP-20
    #[test]
    fn a_narrow_list_drops_wide_columns_first_then_scrolls() {
        let columns = pod_columns();
        let width = |_: usize| 100.0;
        // Room for everything.
        let (all, dropped) = fit_columns(&columns, width, true, Some(700.0));
        assert_eq!(all.len(), 7);
        assert_eq!(dropped, 0);
        // Narrow: the wide columns go, right to left, until the rest fits.
        let (some, dropped) = fit_columns(&columns, width, true, Some(600.0));
        assert_eq!(
            some.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5]
        );
        assert_eq!(dropped, 1);
        // Narrower than the default columns: those stay, and the table scrolls.
        let (default, dropped) = fit_columns(&columns, width, true, Some(300.0));
        assert_eq!(default.len(), 5);
        assert_eq!(dropped, 2);
        let (default, dropped) = fit_columns(&columns, width, false, Some(300.0));
        assert_eq!((default.len(), dropped), (5, 0));
        // Before the first layout the room is unknown: show what was asked for.
        assert_eq!(fit_columns(&columns, width, true, None).0.len(), 7);
    }

    #[test]
    fn only_lists_with_ages_or_times_tick() {
        let columns = pod_columns();
        assert!(has_ticking_columns(&columns));
        assert!(!has_ticking_columns(&columns[..4]));
    }
}
