//! The resource list's view-model logic, kept free of GPUI so it can be unit tested: which
//! columns show, how a sort survives column changes, how wide each column is, and which cells
//! are colored as a status.

use std::sync::Arc;

use flint_core::redact::{is_masked_column, is_secret_list};
use flint_core::{Column, ColumnRole, ColumnType, KindSnapshot, SortBy};

/// The user's sort choice, by column key ([`Column::key`]), as `state.toml` keeps it per cluster
/// and kind. Columns can change between snapshots (an edited CRD's printer columns), so the
/// index is looked up again for every snapshot.
pub type SortSpec = flint_core::settings::ListSort;

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

/// The index in `columns` of the column `spec` sorts by, looked up by key among the columns
/// drawn (`drawn`, indexes into `columns`): a remembered sort on a column that is not drawn (a
/// wide column with Wide off, or one a narrow pane leaves out) is suspended until it shows
/// again, rather than sorting by a column nobody sees.
pub fn sort_column(columns: &[Column], spec: Option<&SortSpec>, drawn: &[usize]) -> Option<usize> {
    let spec = spec?;
    drawn
        .iter()
        .copied()
        .find(|&i| columns[i].key() == spec.column)
}

/// The indexes in `new` of the columns `drawn` (indexes into `old`): the same indexes when the
/// definitions did not change, else each drawn column's match by key, the k-th column with a
/// key matching the k-th one in `old`, so one drawn column never stands for several (two
/// columns can share a header, and so a key, one of them wide and hidden).
pub fn map_drawn(old: &[Column], drawn: &[usize], new: &[Column]) -> Vec<usize> {
    if old == new {
        return drawn.to_vec();
    }
    drawn
        .iter()
        .filter_map(|&col| {
            let key = old[col].key();
            let rank = old[..col].iter().filter(|c| c.key() == key).count();
            new.iter()
                .enumerate()
                .filter(|(_, c)| c.key() == key)
                .nth(rank)
                .map(|(i, _)| i)
        })
        .collect()
}

/// The sort for `compute_view`: `spec`'s column looked up by key among the `drawn` columns.
/// `None` when nothing is sorted, or the column no longer exists (a remembered sort on a CRD
/// column since removed) or is not drawn, which keeps the snapshot order: namespace, then name.
pub fn resolve_sort(
    columns: &[Column],
    spec: Option<&SortSpec>,
    drawn: &[usize],
) -> Option<SortBy> {
    let column = sort_column(columns, spec, drawn)?;
    let spec = spec?;
    Some(SortBy {
        column,
        descending: spec.descending,
    })
}

/// The sort after clicking the header of the column with key `clicked`: a new column sorts
/// ascending, then descending, then back to the default order.
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

/// Width limits in characters. The name column is as wide as its longest name, at least
/// `NAME_MIN_CHARS` and at most `MAX_CHARS` while other columns need the room; spare width goes
/// to the name column first ([`widen_name`]), so long names show in full when the list is wide.
/// What is left goes to a filler after the last column, so the other columns stay next to the
/// names.
const MIN_CHARS: usize = 4;
const MAX_CHARS: usize = 48;
pub const NAME_MIN_CHARS: usize = 16;
/// The longest object name Kubernetes allows (a DNS subdomain), so the name column can grow
/// into spare width as far as any name needs.
const NAME_MAX_CHARS: usize = 253;
const AGE_CHARS: usize = 7;
/// Rows sampled per column when measuring. Enough to see the widths that matter without
/// walking 10,000 rows on every update.
const SAMPLE_ROWS: usize = 2_000;

/// Each column's width in characters: the longest of its header and the cells of the sampled
/// `rows` (indexes into `snapshot.rows`: the rows passing the filters, so a namespace filter
/// leaves out the widths of rows it hides), within limits. The name column is measured up to
/// the longest name. A Secret's masked column is as wide as the mask, whatever its value. Runs
/// on a background thread; it is the only per-row work outside `compute_view`.
pub fn measure_columns(snapshot: &KindSnapshot, rows: &[u32], now: i64) -> Vec<usize> {
    let step = (rows.len() / SAMPLE_ROWS).max(1);
    // Sampled in snapshot order, not display order: a sort must not change which rows are
    // measured, or it could widen a column, push it out of a narrow pane and so suspend itself.
    let stable;
    let rows = if step > 1 {
        let mut sorted = rows.to_vec();
        sorted.sort_unstable();
        stable = sorted;
        &stable[..]
    } else {
        rows
    };
    let secret = is_secret_list(snapshot.kind.as_str());
    snapshot
        .columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let cells = if is_masked_column(secret, &snapshot.columns, i) {
                super::object::MASK.chars().count()
            } else {
                rows.iter()
                    .step_by(step)
                    .map(|&row| {
                        snapshot.rows[row as usize]
                            .cell_text(i, now)
                            .chars()
                            .count()
                    })
                    .max()
                    .unwrap_or(0)
            };
            // The header also holds the sort arrow.
            let header = column.name.chars().count() + 2;
            let max = if column.role == ColumnRole::Name {
                NAME_MAX_CHARS
            } else {
                MAX_CHARS
            };
            cells.max(header).clamp(MIN_CHARS, max)
        })
        .collect()
}

/// How many rows of `snapshot` are in `namespaces` (all of them when the filter is empty or the
/// kind is cluster-scoped): what the list counts against.
pub fn rows_in_namespaces(snapshot: &KindSnapshot, namespaces: &[String]) -> usize {
    if namespaces.is_empty() || !snapshot.namespaced {
        snapshot.rows.len()
    } else {
        snapshot.in_namespaces(namespaces).count()
    }
}

/// The toolbar's row count and its tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowCount {
    /// "7", or "2 of 7" when the text filter hides some: counted within the namespace filter,
    /// which the header already names.
    pub text: String,
    /// Each count spelled out: what matches the text filter, what is in the selected
    /// namespaces, and what the cluster has.
    pub tip: String,
}

/// The count for a list of `total` rows, `scoped` of them in the selected `namespaces` (empty:
/// all), and `shown` of those passing the text filter.
pub fn row_count(shown: usize, scoped: usize, total: usize, namespaces: &[String]) -> RowCount {
    let text = if shown == scoped {
        scoped.to_string()
    } else {
        format!("{shown} of {scoped}")
    };
    let mut tip = Vec::new();
    if shown != scoped {
        tip.push(format!("{shown} match the filter"));
    }
    if scoped != total || !namespaces.is_empty() {
        let within = match namespaces {
            [one] => one.clone(),
            many => format!("{} namespaces", many.len()),
        };
        tip.push(format!("{scoped} in {within}"));
        tip.push(format!("{total} in all namespaces"));
    } else {
        tip.push(format!("{total} in the cluster"));
    }
    RowCount {
        text,
        tip: tip.join("\n"),
    }
}

/// Widths that only grow while the same columns are shown, so columns do not jitter as rows
/// change. New columns (a different `Arc`) start over.
#[derive(Debug, Default, Clone)]
pub struct ColumnWidths {
    columns: Option<Arc<[Column]>>,
    chars: Vec<usize>,
}

impl ColumnWidths {
    /// Folds a measurement of `columns` in. `restart` drops the widths so far, for when other
    /// rows are shown (a filter changed), so columns fit the rows now shown.
    pub fn update(&mut self, columns: &Arc<[Column]>, measured: &[usize], restart: bool) {
        let same = self
            .columns
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, columns) || **c == **columns);
        if restart || !same || self.chars.len() != measured.len() {
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
            return chars.clamp(NAME_MIN_CHARS, MAX_CHARS);
        }
        // Ages keep counting without new snapshots (and so without new measurements): leave
        // room for the longest form `kubectl` prints ("5m30s", "3y45d").
        if column.role == ColumnRole::Age || column.ty == ColumnType::Date {
            chars.max(AGE_CHARS)
        } else {
            chars
        }
    }

    /// Width in characters column `i` would need to show every cell in full. Only the name
    /// column can need more than [`ColumnWidths::chars`].
    pub fn wanted(&self, i: usize, column: &Column) -> usize {
        match (column.role, self.chars.get(i)) {
            (ColumnRole::Name, Some(&chars)) => chars.max(NAME_MIN_CHARS),
            _ => self.chars(i, column),
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

/// Narrows the Namespace column, then the Name column, down to their minimums when the drawn
/// columns are wider than `room`, so the status columns to their right stay in view instead of
/// scrolling off. Namespaces go first: they repeat down the list and the namespace filter names
/// them, while the name is what tells rows apart. Long names truncate in the middle and show in
/// full in a tooltip.
pub fn squeeze_identity(
    drawn: &mut [(usize, f32)],
    columns: &[Column],
    room: f32,
    name_min: f32,
    namespace_min: f32,
) {
    for (role, min) in [
        (ColumnRole::Namespace, namespace_min),
        (ColumnRole::Name, name_min),
    ] {
        let total: f32 = drawn.iter().map(|(_, w)| w).sum();
        if total <= room {
            return;
        }
        if let Some((_, width)) = drawn.iter_mut().find(|(i, _)| columns[*i].role == role)
            && *width > min
        {
            *width = (*width - (total - room)).max(min);
        }
    }
}

/// Widens the Name column into the room the drawn columns leave free, up to `name_wanted`
/// (the width its longest name needs), so a long name is not cut while the list has room.
pub fn widen_name(drawn: &mut [(usize, f32)], columns: &[Column], room: f32, name_wanted: f32) {
    let total: f32 = drawn.iter().map(|(_, w)| w).sum();
    if total >= room {
        return;
    }
    if let Some((_, width)) = drawn
        .iter_mut()
        .find(|(i, _)| columns[*i].role == ColumnRole::Name)
        && *width < name_wanted
    {
        *width = name_wanted.min(*width + (room - total));
    }
}

/// `text` cut in the middle to fit `chars` characters, so names that share a long prefix (the
/// pods of one workload) stay distinguishable by their ends. `None` when it fits as is.
pub fn fit_name(text: &str, chars: usize) -> Option<String> {
    (text.chars().count() > chars).then(|| flint_core::display::middle_truncate(text, chars))
}

/// Whether the rows show something that changes with time alone (ages, timestamps), so the list
/// needs redrawing every second.
pub fn has_ticking_columns(columns: &[Column]) -> bool {
    columns
        .iter()
        .any(|c| c.role == ColumnRole::Age || c.ty == ColumnType::Date)
}

/// Approximate advance of one character at the table's text size, for column widths.
pub const CHAR_WIDTH: f32 = 8.5;
/// A cell's horizontal padding, both sides together.
pub const CELL_PADDING: f32 = 16.;

/// What a row contributes to how its cells are drawn.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowFlags {
    /// The list is not current: its rows are already muted, so cells get no tone.
    pub stale: bool,
    /// The kind has [`has_health_columns`].
    pub health: bool,
    /// The row has finished for good ([`row_finished`]).
    pub finished: bool,
    /// This cell's column hides its value ([`flint_core::redact::is_masked_column`]).
    pub masked: bool,
}

/// A cell's text type. `from_static` must not copy: the mask is drawn in every hidden cell.
pub trait CellText: Clone + AsRef<str> + From<String> {
    fn from_static(text: &'static str) -> Self;
}

/// How one list cell is drawn, decided without GPUI. `T` is the text type: the view's shared
/// strings, so a cell that is not cut shares its text instead of copying it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellStyle<T> {
    /// The text in the cell, cut to fit if it is a name.
    pub shown: T,
    /// The full text, when it does not fit the cell.
    pub tooltip: Option<T>,
    pub tone: Tone,
    /// Right-aligned, with tabular figures.
    pub numeric: bool,
    /// A name, in the monospace font.
    pub mono: bool,
}

impl<T: CellText> CellStyle<T> {
    /// The style of `column`'s cell, `width` wide. `raw_text` is not called for a masked cell.
    pub fn of(column: &Column, raw_text: impl FnOnce() -> T, width: f32, flags: RowFlags) -> Self {
        let text = if flags.masked {
            T::from_static(super::object::MASK)
        } else {
            raw_text()
        };
        let chars = text.as_ref();
        // Stale rows are already muted; masked secrets have no status.
        let tone = if flags.stale || !flags.health {
            Tone::Normal
        } else {
            cell_tone(&column.name, chars, flags.finished)
        };
        let mono = column.role == ColumnRole::Name;
        let fits = (chars.chars().count() as f32) * CHAR_WIDTH + CELL_PADDING <= width;
        // Names of one workload share their start: cut them in the middle, so the ends that
        // tell them apart stay in view. Other cells truncate at the end.
        let room = ((width - CELL_PADDING) / CHAR_WIDTH).floor().max(0.) as usize;
        let shown = match mono.then(|| fit_name(chars, room)) {
            Some(Some(cut)) => T::from(cut),
            _ => text.clone(),
        };
        Self {
            shown,
            tooltip: (!fits).then_some(text),
            tone,
            numeric: is_numeric(column),
            mono,
        }
    }
}

/// How a cell's text is colored, so a failing object stands out from healthy ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    /// Failed or broken: CrashLoopBackOff, Error, NotReady, a failed pull, ...
    Danger,
    /// Not there yet, or partly: Pending, ContainerCreating, Terminating, `1/2` ready, restarts.
    Warning,
    /// Finished and no longer running: Completed, Succeeded.
    Muted,
}

/// Whether `kind` is a built-in kind whose Status, Phase, Ready and Restarts columns mean what
/// `kubectl` means by them. A CRD's printer column of the same name can mean anything (a
/// configured `Restarts` limit, say), so CRDs are never colored.
pub fn has_health_columns(kind: &str) -> bool {
    matches!(
        kind,
        "pods"
            | "nodes"
            | "namespaces"
            | "persistentvolumes"
            | "persistentvolumeclaims"
            | "replicationcontrollers"
            | "deployments.apps"
            | "statefulsets.apps"
            | "daemonsets.apps"
            | "replicasets.apps"
            | "jobs.batch"
    )
}

/// Whether a row has finished for good: its Status or Phase reads Completed or Succeeded. Its
/// `0/1` ready count is then expected, not a warning.
pub fn row_finished(columns: &[Column], text_of: impl Fn(usize) -> String) -> bool {
    columns.iter().enumerate().any(|(i, column)| {
        let status =
            column.name.eq_ignore_ascii_case("status") || column.name.eq_ignore_ascii_case("phase");
        status && status_tone(text_of(i).trim()) == Tone::Muted
    })
}

/// The tone of a cell of a kind with [`has_health_columns`], from its column's name and its
/// text. Only the status-like columns are colored (Status, Phase, Ready, Restarts); every other
/// cell is normal, so color keeps meaning "look here". In a `finished` row the ready count is
/// muted along with the status.
pub fn cell_tone(column: &str, text: &str, finished: bool) -> Tone {
    let text = text.trim();
    if column.eq_ignore_ascii_case("restarts") {
        // `3` or `3 (5m ago)`.
        let count: u64 = text
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        return if count > 0 {
            Tone::Warning
        } else {
            Tone::Normal
        };
    }
    if column.eq_ignore_ascii_case("ready") {
        // `ready/total` (pods, deployments, ...); other Ready columns (nodes' True/False) fall
        // through to the status words.
        if let Some((ready, total)) = text.split_once('/')
            && let (Ok(ready), Ok(total)) = (ready.parse::<u32>(), total.parse::<u32>())
        {
            return match (ready < total, finished) {
                (true, true) => Tone::Muted,
                (true, false) => Tone::Warning,
                (false, _) => Tone::Normal,
            };
        }
    }
    let status_column = ["status", "phase", "ready"]
        .iter()
        .any(|name| column.eq_ignore_ascii_case(name));
    if !status_column {
        return Tone::Normal;
    }
    status_tone(text)
}

/// The tone of a status word, as kubectl prints it. A node's `Ready,SchedulingDisabled` is
/// judged by its worst part.
fn status_tone(text: &str) -> Tone {
    text.split(',')
        .map(|part| word_tone(part.trim()))
        .max_by_key(|tone| match tone {
            Tone::Danger => 3,
            Tone::Warning => 2,
            Tone::Muted => 1,
            Tone::Normal => 0,
        })
        .unwrap_or(Tone::Normal)
}

fn word_tone(word: &str) -> Tone {
    // Init containers: `Init:0/2` is waiting, `Init:Error` and `Init:CrashLoopBackOff` failed.
    let word = word.strip_prefix("Init:").unwrap_or(word);
    match word {
        "Running" | "Ready" | "Active" | "Bound" | "Available" | "True" | "Healthy" => Tone::Normal,
        "Completed" | "Succeeded" => Tone::Muted,
        "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "Waiting"
        | "Released" | "SchedulingDisabled" | "Unknown" => Tone::Warning,
        "Failed"
        | "Error"
        | "CrashLoopBackOff"
        | "ImagePullBackOff"
        | "ErrImagePull"
        | "ErrImageNeverPull"
        | "InvalidImageName"
        | "OOMKilled"
        | "Evicted"
        | "NotReady"
        | "Lost"
        | "False"
        | "ContainerStatusUnknown"
        | "DeadlineExceeded"
        | "BackoffLimitExceeded"
        | "RunContainerError"
        | "StartError" => Tone::Danger,
        // `CreateContainerConfigError`, `CreateContainerError`, `PreStopHookError`, ...
        w if w.ends_with("Error") || w.ends_with("BackOff") => Tone::Danger,
        // `Init:0/2`: init containers still running.
        w if w.contains('/') => Tone::Warning,
        _ => Tone::Normal,
    }
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

    /// Every column of `columns` drawn.
    fn all(columns: &[Column]) -> Vec<usize> {
        (0..columns.len()).collect()
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
        let asc = next_sort(None, "flint:age");
        assert_eq!(
            asc,
            Some(SortSpec {
                column: "flint:age".into(),
                descending: false
            })
        );
        let desc = next_sort(asc.as_ref(), "flint:age");
        assert_eq!(desc.as_ref().map(|s| s.descending), Some(true));
        assert_eq!(next_sort(desc.as_ref(), "flint:age"), None);
        // Another column starts ascending again.
        let other = next_sort(desc.as_ref(), "flint:name").unwrap();
        assert_eq!(
            (other.column.as_str(), other.descending),
            ("flint:name", false)
        );
    }

    // covers UP-21, UP-22
    #[test]
    fn sort_follows_its_column_by_key_when_columns_change() {
        let spec = SortSpec {
            column: "server:Restarts".into(),
            descending: true,
        };
        let columns = pod_columns();
        assert_eq!(
            resolve_sort(&columns, Some(&spec), &all(&columns)),
            Some(SortBy {
                column: 3,
                descending: true
            })
        );
        // A CRD edit inserts a column before it: the index moves, the sort stays on Restarts.
        let mut changed = columns.clone();
        changed.insert(2, col("Phase", 0, ColumnRole::Value, ColumnType::String));
        assert_eq!(
            resolve_sort(&changed, Some(&spec), &all(&changed)),
            Some(SortBy {
                column: 4,
                descending: true
            })
        );
        // The column is gone: back to the default order rather than sorting by a stranger.
        changed.retain(|c| &*c.name != "Restarts");
        assert_eq!(resolve_sort(&changed, Some(&spec), &all(&changed)), None);
        assert_eq!(resolve_sort(&columns, None, &all(&columns)), None);
    }

    // covers UP-20, UP-21
    #[test]
    fn a_sort_on_a_wide_column_applies_only_while_wide_is_on() {
        let columns = pod_columns();
        let node = SortSpec {
            column: "server:Node".into(),
            descending: false,
        };
        let narrow = visible_columns(&columns, false);
        assert_eq!(sort_column(&columns, Some(&node), &narrow), None);
        let wide = visible_columns(&columns, true);
        assert_eq!(sort_column(&columns, Some(&node), &wide), Some(6));
        // Wide is on, but a narrow pane leaves Node out: suspended too.
        let (fitted, dropped) = fit_columns(&columns, |_| 100., true, Some(650.));
        assert_eq!(dropped, 1);
        let fitted: Vec<usize> = fitted.into_iter().map(|(i, _)| i).collect();
        assert_eq!(sort_column(&columns, Some(&node), &fitted), None);
    }

    // covers UP-21
    #[test]
    fn a_sort_keeps_to_its_column_when_another_has_the_same_header() {
        // A CRD's printer column named "Namespace", beside Flint's own Namespace column.
        let mut columns = pod_columns();
        columns.push(col("Namespace", 0, ColumnRole::Value, ColumnType::String));
        let flint = SortSpec {
            column: "flint:namespace".into(),
            descending: false,
        };
        let server = SortSpec {
            column: "server:Namespace".into(),
            descending: false,
        };
        assert_eq!(sort_column(&columns, Some(&flint), &all(&columns)), Some(1));
        assert_eq!(
            sort_column(&columns, Some(&server), &all(&columns)),
            Some(7)
        );
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

    // covers UP-20, UP-21
    #[test]
    fn sampled_widths_do_not_depend_on_the_display_order() {
        let columns: Arc<[Column]> = pod_columns()[..3].to_vec().into();
        // Three times the rows sampled, so every third is measured; the long Ready cells are on
        // the rows in between.
        let rows: Vec<Arc<Row>> = (0..(SAMPLE_ROWS * 3))
            .map(|i| {
                let ready = if i % 3 == 1 {
                    "x".repeat(40)
                } else {
                    "1/1".into()
                };
                row(&format!("pod-{i:05}"), &ready)
            })
            .collect();
        let snapshot = KindSnapshot {
            columns,
            rows,
            ..Default::default()
        };
        let in_order: Vec<u32> = (0..snapshot.rows.len() as u32).collect();
        // A sort that brings the long cells to where the sample falls.
        let mut sorted = in_order.clone();
        sorted.sort_by_key(|&i| (i % 3 != 1, i));
        assert_eq!(
            measure_columns(&snapshot, &sorted, 10),
            measure_columns(&snapshot, &in_order, 10)
        );
    }

    // covers UP-20, UP-21
    #[test]
    fn drawn_columns_map_one_to_one_onto_new_definitions() {
        // Two server columns headed "Node": the first wide (hidden), the second shown.
        let mut columns = pod_columns()[..5].to_vec();
        columns.push(col("Node", 1, ColumnRole::Value, ColumnType::String));
        columns.push(col("Node", 0, ColumnRole::Value, ColumnType::String));
        let drawn = vec![0, 1, 2, 3, 4, 6];
        // Unchanged definitions: the same indexes, never the hidden twin too.
        assert_eq!(map_drawn(&columns, &drawn, &columns), drawn);
        let node = SortSpec {
            column: "server:Node".into(),
            descending: false,
        };
        let mapped = map_drawn(&columns, &drawn, &columns);
        assert_eq!(sort_column(&columns, Some(&node), &mapped), Some(6));
        // A column inserted before them: each drawn column maps to its own match.
        let mut changed = columns.clone();
        changed.insert(2, col("Phase", 0, ColumnRole::Value, ColumnType::String));
        let mapped = map_drawn(&columns, &drawn, &changed);
        assert_eq!(mapped, vec![0, 1, 3, 4, 5, 7]);
        assert_eq!(sort_column(&changed, Some(&node), &mapped), Some(7));
        // A drawn column that is gone maps to nothing.
        changed.truncate(6);
        assert_eq!(map_drawn(&columns, &drawn, &changed), vec![0, 1, 3, 4, 5]);
    }

    #[test]
    fn widths_fit_content_within_limits_and_only_grow() {
        let columns: Arc<[Column]> = pod_columns()[..3].to_vec().into();
        let snapshot = KindSnapshot {
            columns: columns.clone(),
            rows: vec![row("a", "1/1"), row(&"x".repeat(80), "1/1")],
            ..Default::default()
        };
        let measured = measure_columns(&snapshot, &[0, 1], 10);
        // The whole name, "Namespace" header (+2 for the arrow), and "Ready" header beats "1/1".
        assert_eq!(measured, vec![80, 11, 7]);

        let mut widths = ColumnWidths::default();
        widths.update(&columns, &measured, false);
        widths.update(&columns, &[30, 11, 5], false);
        assert_eq!(widths.chars(2, &columns[2]), 7, "never shrinks");
        assert_eq!(widths.chars(1, &columns[1]), 11);
        assert_eq!(
            widths.chars(0, &columns[0]),
            MAX_CHARS,
            "the name column is as wide as the longest name, capped"
        );
        assert_eq!(
            widths.wanted(0, &columns[0]),
            80,
            "but it can take spare room up to the longest name"
        );
        assert_eq!(widths.wanted(1, &columns[1]), 11);

        // Only the rows shown are measured: "a" alone is narrower than the "Name" header.
        assert_eq!(measure_columns(&snapshot, &[0], 10)[0], 6);
        // A restart (another filter) lets columns shrink to the rows now shown.
        widths.update(&columns, &[6, 11, 5], true);
        assert_eq!(widths.chars(2, &columns[2]), 5);

        // New columns start over; the name column keeps its minimum.
        let other: Arc<[Column]> = pod_columns()[..1].to_vec().into();
        widths.update(&other, &[5], false);
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

    // covers UP-20
    #[test]
    fn status_cells_are_toned_by_how_healthy_they_read() {
        use Tone::*;
        let cases = [
            ("Status", "Running", Normal),
            ("Status", "Completed", Muted),
            ("Status", "Pending", Warning),
            ("Status", "ContainerCreating", Warning),
            ("Status", "Terminating", Warning),
            ("Status", "Init:0/2", Warning),
            ("Status", "CrashLoopBackOff", Danger),
            ("Status", "Init:CrashLoopBackOff", Danger),
            ("Status", "Error", Danger),
            ("Status", "ImagePullBackOff", Danger),
            ("Status", "CreateContainerConfigError", Danger),
            ("Status", "OOMKilled", Danger),
            ("Status", "Ready", Normal),
            ("Status", "Ready,SchedulingDisabled", Warning),
            ("Status", "NotReady", Danger),
            ("Status", "Bound", Normal),
            ("Status", "Lost", Danger),
            ("Phase", "Failed", Danger),
            ("Phase", "Succeeded", Muted),
            ("Ready", "2/2", Normal),
            ("Ready", "1/2", Warning),
            ("Ready", "0/1", Warning),
            ("Ready", "0/0", Normal),
            ("Ready", "True", Normal),
            ("Ready", "False", Danger),
            ("Restarts", "0", Normal),
            ("Restarts", "3", Warning),
            ("Restarts", "3 (5m ago)", Warning),
            // Only status-like columns: a name or label that reads "Error" is not colored.
            ("Name", "Error", Normal),
            ("Reason", "BackOff", Normal),
            ("Up-to-date", "1/2", Normal),
        ];
        for (column, text, want) in cases {
            assert_eq!(cell_tone(column, text, false), want, "{column} = {text}");
        }
        // A completed Job pod's 0/1 is expected, not a warning.
        assert_eq!(cell_tone("Ready", "0/1", true), Muted);
    }

    // covers UP-20, UP-22
    #[test]
    fn only_built_in_kinds_get_health_colors_and_finished_rows_are_recognized() {
        assert!(has_health_columns("pods"));
        assert!(has_health_columns("deployments.apps"));
        assert!(!has_health_columns("widgets.example.com"));
        assert!(!has_health_columns("configmaps"));
        let columns = vec![
            col("Name", 0, ColumnRole::Name, ColumnType::String),
            col("Ready", 0, ColumnRole::Value, ColumnType::String),
            col("Status", 0, ColumnRole::Value, ColumnType::String),
        ];
        let row = |status: &'static str| move |i: usize| ["job-x", "0/1", status][i].to_owned();
        assert!(row_finished(&columns, row("Completed")));
        assert!(!row_finished(&columns, row("Running")));
        assert!(!row_finished(&columns, row("Error")));
    }

    // covers UP-20, UP-23: a long pod name must not push Status out of the list beside an open
    // detail pane.
    #[test]
    fn long_names_give_way_so_the_status_column_stays_in_view() {
        // The real pod columns, in kubectl order.
        let columns = vec![
            col("Name", 0, ColumnRole::Name, ColumnType::String),
            col("Namespace", 0, ColumnRole::Namespace, ColumnType::String),
            col("Ready", 0, ColumnRole::Value, ColumnType::String),
            col("Status", 0, ColumnRole::Value, ColumnType::String),
            col("Restarts", 0, ColumnRole::Value, ColumnType::Integer),
            col("Age", 0, ColumnRole::Age, ColumnType::String),
        ];
        let (name_min, namespace_min) = (118., 84.);
        // Name 400, Namespace 170 (local-path-storage), Ready 60, Status 152, Restarts 80, Age 76.
        let widths = [400., 170., 60., 152., 80., 76.];
        let drawn = || -> Vec<(usize, f32)> { widths.iter().copied().enumerate().collect() };
        let status_x = |d: &[(usize, f32)]| d[0].1 + d[1].1 + d[2].1;

        let mut roomy = drawn();
        squeeze_identity(&mut roomy, &columns, 1000., name_min, namespace_min);
        assert_eq!(roomy, drawn(), "nothing changes when everything fits");

        // A little short: only Namespace gives way, and the names stay whole.
        let mut close = drawn();
        squeeze_identity(&mut close, &columns, 900., name_min, namespace_min);
        assert_eq!((close[0].1, close[1].1), (400., 132.));

        // Mid-size: Namespace is at its minimum, then Name gives way.
        let mut mid = drawn();
        squeeze_identity(&mut mid, &columns, 800., name_min, namespace_min);
        assert_eq!((mid[0].1, mid[1].1), (348., 84.));

        // The list's 360 px minimum beside a detail pane: Status starts well inside the list.
        let mut tight = drawn();
        squeeze_identity(&mut tight, &columns, 360., name_min, namespace_min);
        assert_eq!((tight[0].1, tight[1].1), (name_min, namespace_min));
        assert!(
            360. - status_x(&tight) >= 90.,
            "Status starts at {}",
            status_x(&tight)
        );
    }

    // covers UP-20, UP-25, UP-27: a long name must not be cut while the list has room.
    #[test]
    fn spare_width_goes_to_the_name_column_up_to_its_longest_name() {
        let columns = pod_columns()[..5].to_vec();
        let drawn =
            || -> Vec<(usize, f32)> { vec![(0, 200.), (1, 100.), (2, 60.), (3, 80.), (4, 70.)] };
        // 510 drawn in 900: Name grows to what its longest name needs, the rest stays a filler.
        let mut wide = drawn();
        widen_name(&mut wide, &columns, 900., 330.);
        assert_eq!(wide[0].1, 330.);
        assert_eq!(wide[1..], drawn()[1..]);
        // Less room than it wants: it takes all of it.
        let mut some = drawn();
        widen_name(&mut some, &columns, 600., 330.);
        assert_eq!(some[0].1, 290.);
        // No room, or a name that already fits: nothing changes.
        let mut full = drawn();
        widen_name(&mut full, &columns, 500., 330.);
        assert_eq!(full, drawn());
        let mut fits = drawn();
        widen_name(&mut fits, &columns, 900., 150.);
        assert_eq!(fits, drawn());
    }

    // covers UP-23: truncated pod names of one workload must stay distinguishable.
    #[test]
    fn names_are_cut_in_the_middle_keeping_their_distinct_ends() {
        let a = fit_name("podinfo-7b9c8d5f4-abcde", 14).unwrap();
        let b = fit_name("podinfo-7b9c8d5f4-vwxyz", 14).unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("abcde") && a.contains('…'), "{a}");
        assert_eq!(a.chars().count(), 14);
        assert_eq!(fit_name("api", 14), None);
    }

    impl CellText for String {
        fn from_static(text: &'static str) -> Self {
            text.to_owned()
        }
    }

    fn style(column: &Column, text: &str, width: f32, flags: RowFlags) -> CellStyle<String> {
        CellStyle::of(column, || text.to_owned(), width, flags)
    }

    fn value(name: &str) -> Column {
        col(name, 0, ColumnRole::Value, ColumnType::String)
    }

    const WIDE: f32 = 1_000.;
    const HEALTHY: RowFlags = RowFlags {
        stale: false,
        health: true,
        finished: false,
        masked: false,
    };

    fn token_snapshot(kind: flint_core::KindId, token: &str) -> KindSnapshot {
        KindSnapshot {
            kind,
            columns: vec![
                col("Name", 0, ColumnRole::Name, ColumnType::String),
                value("Token"),
            ]
            .into(),
            rows: vec![Arc::new(Row {
                uid: "u".into(),
                name: "creds".into(),
                namespace: None,
                resource_version: "1".into(),
                generation: None,
                created: Some(0),
                labels: Box::new([]),
                cells: Box::new([Cell::Name, Cell::Text(token.into())]),
            })],
            ..Default::default()
        }
    }

    #[test]
    fn a_masked_column_is_as_wide_whatever_its_value() {
        let secrets = flint_core::KindId::new("", "secrets");
        let short = measure_columns(&token_snapshot(secrets.clone(), "x"), &[0], 0);
        let long = measure_columns(&token_snapshot(secrets, &"x".repeat(40)), &[0], 0);
        assert_eq!(short, long);
        assert_eq!(short[1], crate::ui::object::MASK.chars().count());
    }

    #[test]
    fn other_kinds_measure_the_same_column_by_its_value() {
        let long = token_snapshot(flint_core::KindId::pods(), &"x".repeat(40));
        assert_eq!(measure_columns(&long, &[0], 0)[1], 40);
    }

    #[test]
    fn a_masked_cell_never_reads_its_value() {
        let flags = RowFlags {
            masked: true,
            ..RowFlags::default()
        };
        let cell = CellStyle::<String>::of(
            &value("Token"),
            || panic!("a masked value was read"),
            WIDE,
            flags,
        );
        assert_eq!(cell.shown, crate::ui::object::MASK);
        assert_eq!(cell.tooltip, None);
        // Too narrow: the tooltip offers the mask too, not the value.
        let cell = CellStyle::<String>::of(
            &value("Token"),
            || panic!("a masked value was read"),
            20.,
            flags,
        );
        assert_eq!(cell.tooltip.as_deref(), Some(crate::ui::object::MASK));
    }

    #[test]
    fn a_cell_that_is_not_masked_shows_its_value() {
        let cell = style(&value("Type"), "Opaque", WIDE, RowFlags::default());
        assert_eq!(cell.shown, "Opaque");
    }

    #[test]
    fn only_current_rows_of_health_kinds_get_a_tone() {
        let status = value("Status");
        assert_eq!(style(&status, "Error", WIDE, HEALTHY).tone, Tone::Danger);
        let stale = RowFlags {
            stale: true,
            ..HEALTHY
        };
        assert_eq!(style(&status, "Error", WIDE, stale).tone, Tone::Normal);
        let no_health = RowFlags {
            health: false,
            ..HEALTHY
        };
        assert_eq!(style(&status, "Error", WIDE, no_health).tone, Tone::Normal);
    }

    #[test]
    fn a_finished_row_mutes_its_ready_count() {
        let ready = value("Ready");
        assert_eq!(style(&ready, "0/1", WIDE, HEALTHY).tone, Tone::Warning);
        let finished = RowFlags {
            finished: true,
            ..HEALTHY
        };
        assert_eq!(style(&ready, "0/1", WIDE, finished).tone, Tone::Muted);
    }

    #[test]
    fn numeric_columns_are_marked_and_others_are_not() {
        let number = col("Restarts", 0, ColumnRole::Value, ColumnType::Integer);
        assert!(style(&number, "3", WIDE, RowFlags::default()).numeric);
        assert!(!style(&value("Node"), "3", WIDE, RowFlags::default()).numeric);
    }

    #[test]
    fn a_name_is_mono_and_cut_in_the_middle_with_its_full_text_in_the_tooltip() {
        let name = col("Name", 0, ColumnRole::Name, ColumnType::String);
        let long = "podinfo-7b9c8d5f4-abcde";
        let cell = style(&name, long, 16. + 10. * CHAR_WIDTH, RowFlags::default());
        assert!(cell.mono);
        assert!(cell.shown.contains('…'), "{}", cell.shown);
        assert!(cell.shown.ends_with("bcde"), "{}", cell.shown);
        assert_eq!(cell.tooltip.as_deref(), Some(long));
    }

    #[test]
    fn other_cells_keep_their_text_and_truncate_when_drawn() {
        let long = "a-long-node-name";
        let cell = style(&value("Node"), long, 40., RowFlags::default());
        assert!(!cell.mono);
        assert_eq!(cell.shown, long);
        assert_eq!(cell.tooltip.as_deref(), Some(long));
    }

    #[test]
    fn a_cell_has_a_tooltip_only_past_its_exact_fit() {
        let exact = 4. * CHAR_WIDTH + CELL_PADDING;
        let node = value("Node");
        assert_eq!(
            style(&node, "node", exact, RowFlags::default()).tooltip,
            None
        );
        assert!(
            style(&node, "node", exact - 0.5, RowFlags::default())
                .tooltip
                .is_some()
        );
    }

    #[test]
    fn a_cell_narrower_than_its_padding_still_has_a_style() {
        let name = col("Name", 0, ColumnRole::Name, ColumnType::String);
        let cell = style(&name, "podinfo", 4., RowFlags::default());
        assert_eq!(
            cell.shown,
            flint_core::display::middle_truncate("podinfo", 0)
        );
        assert_eq!(cell.tooltip.as_deref(), Some("podinfo"));
    }

    #[test]
    fn a_name_that_fits_is_shown_whole_without_a_tooltip() {
        let name = col("Name", 0, ColumnRole::Name, ColumnType::String);
        let cell = style(&name, "podinfo", WIDE, RowFlags::default());
        assert!(cell.mono);
        assert_eq!(cell.shown, "podinfo");
        assert_eq!(cell.tooltip, None);
    }

    // covers UP-21: the count is within the namespace filter, which the header names.
    #[test]
    fn the_count_matches_the_namespace_filter() {
        let none: [String; 0] = [];
        let demo = ["demo".to_owned()];
        let two = ["demo".to_owned(), "kube-system".to_owned()];
        // All namespaces, no text filter.
        assert_eq!(
            row_count(17, 17, 17, &none),
            RowCount {
                text: "17".into(),
                tip: "17 in the cluster".into()
            }
        );
        // One namespace selected: 7 of the cluster's 17 are in it.
        assert_eq!(
            row_count(7, 7, 17, &demo),
            RowCount {
                text: "7".into(),
                tip: "7 in demo\n17 in all namespaces".into()
            }
        );
        // And a text filter: counted against the namespace, not the cluster (was "2 of 17").
        assert_eq!(
            row_count(2, 7, 17, &demo),
            RowCount {
                text: "2 of 7".into(),
                tip: "2 match the filter\n7 in demo\n17 in all namespaces".into()
            }
        );
        assert_eq!(
            row_count(3, 9, 17, &two).tip,
            "3 match the filter\n9 in 2 namespaces\n17 in all namespaces"
        );
        // A text filter on all namespaces.
        assert_eq!(row_count(2, 17, 17, &none).text, "2 of 17");
    }

    // covers UP-21
    #[test]
    fn rows_are_counted_within_the_selected_namespaces() {
        let columns: Arc<[Column]> = pod_columns()[..3].to_vec().into();
        let in_ns = |ns: &str, name: &str| {
            let mut r = (*row(name, "1/1")).clone();
            r.namespace = Some(ns.into());
            Arc::new(r)
        };
        let snapshot = KindSnapshot {
            columns,
            namespaced: true,
            rows: vec![
                in_ns("a", "x"),
                in_ns("b", "x"),
                in_ns("b", "y"),
                in_ns("c", "z"),
            ],
            ..Default::default()
        };
        assert_eq!(rows_in_namespaces(&snapshot, &[]), 4);
        assert_eq!(rows_in_namespaces(&snapshot, &["b".into()]), 2);
        assert_eq!(rows_in_namespaces(&snapshot, &["a".into(), "c".into()]), 2);
        assert_eq!(rows_in_namespaces(&snapshot, &["gone".into()]), 0);
        let cluster_scoped = KindSnapshot {
            namespaced: false,
            ..snapshot
        };
        assert_eq!(rows_in_namespaces(&cluster_scoped, &["b".into()]), 4);
    }
}
