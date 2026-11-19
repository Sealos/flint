//! The resource list's view-model logic, kept free of GPUI so it can be unit tested: which
//! columns show, how a sort survives column changes, how wide each column is, and which cells
//! are colored as a status.

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

/// Narrows the Name column, then the Namespace column, down to their minimums when the drawn
/// columns are wider than `room`, so the status columns to their right stay in view instead of
/// scrolling off. Long names truncate and show in full in a tooltip.
pub fn squeeze_identity(
    drawn: &mut [(usize, f32)],
    columns: &[Column],
    room: f32,
    name_min: f32,
    namespace_min: f32,
) {
    for (role, min) in [
        (ColumnRole::Name, name_min),
        (ColumnRole::Namespace, namespace_min),
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

/// Whether the rows show something that changes with time alone (ages, timestamps), so the list
/// needs redrawing every second.
pub fn has_ticking_columns(columns: &[Column]) -> bool {
    columns
        .iter()
        .any(|c| c.role == ColumnRole::Age || c.ty == ColumnType::Date)
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

        // Mid-size: only Name gives way.
        let mut mid = drawn();
        squeeze_identity(&mut mid, &columns, 800., name_min, namespace_min);
        assert_eq!((mid[0].1, mid[1].1), (262., 170.));

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
}
