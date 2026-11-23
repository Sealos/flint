//! A log tab's filtered view (#264, UP-48): "Matching lines only" (with N lines of context),
//! "Errors and warnings", or both. A core [`LogFilter`] keeps the ids of the lines shown, scanning
//! only new lines when the tab is drawn; the first scan of a new filter, and a long catch-up,
//! run on the core's threads ([`flint_core::CoreHandle::filter_logs`]), so a 100k-line buffer
//! filters without holding up a frame. The list then has one item per row the filter shows, and every place
//! that goes from an item to a line, and back, goes through [`LogView::row_id`] and
//! [`LogView::row_index`].
// module-tests: log_tests.rs

use flint_core::logs::{GUARD_SCAN_BYTES, GUARD_SCAN_LINES, LogLines, MAX_CONTEXT, ScanCancel};
use flint_core::{FilterSpec, LineId, LogFilter, SearchQuery};
use gpui_kit::*;

use super::LogView;

/// Stops the scan it watches when it is replaced, told to, or dropped with the tab, so that a
/// scan nobody wants lets go of its snapshot at the next line.
#[derive(Debug, Default)]
pub(super) struct ScanStop(Option<ScanCancel>);

impl ScanStop {
    /// Watches a new scan, stopping the one before it.
    pub(super) fn watch(&mut self, scan: ScanCancel) {
        self.stop();
        self.0 = Some(scan);
    }

    pub(super) fn stop(&mut self) {
        if let Some(scan) = self.0.take() {
            scan.cancel();
        }
    }

    /// The scan landed: nothing is left to stop.
    pub(super) fn finished(&mut self) {
        self.0 = None;
    }
}

impl Drop for ScanStop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Most text a frame tests with a plain query on the UI thread: a few milliseconds of substring
/// search, whatever the number of lines.
pub(super) const PLAIN_CATCH_UP_BYTES: usize = 4 * 1024 * 1024;

/// What scans may still test on the UI thread: one budget for everything a frame scans, so the
/// search and the filter together stay within [`GUARD_SCAN_LINES`] and [`GUARD_SCAN_BYTES`] for a
/// regex, and [`PLAIN_CATCH_UP_BYTES`] for plain text.
#[derive(Debug, Clone, Copy)]
pub(super) struct UiScanBudget {
    lines: u64,
    bytes: usize,
    plain_bytes: usize,
}

impl Default for UiScanBudget {
    fn default() -> Self {
        UiScanBudget {
            lines: GUARD_SCAN_LINES as u64,
            bytes: GUARD_SCAN_BYTES,
            plain_bytes: PLAIN_CATCH_UP_BYTES,
        }
    }
}

/// Whether testing the newest `pending` lines with `query` is short enough for the UI thread,
/// by lines and by text, counting the sources it also tests. A scan that fits takes its share of
/// `budget`, and one that does not takes nothing.
pub(super) fn catch_up_fits_ui(
    query: &SearchQuery,
    pending: u64,
    lines: &LogLines,
    budget: &mut UiScanBudget,
) -> bool {
    let (max_lines, max_bytes) = if query.regex {
        (budget.lines, budget.bytes)
    } else {
        (super::CATCH_UP_ON_UI, budget.plain_bytes)
    };
    if pending > max_lines {
        return false;
    }
    let end = lines.end_id();
    let mut bytes = 0;
    let fits = lines.range(end - pending, end).all(|(_, line)| {
        bytes += line.text.len();
        if query.sources
            && let Some(source) = &line.source
        {
            bytes += source.label().len();
        }
        bytes <= max_bytes
    });
    if fits {
        if query.regex {
            budget.lines -= pending;
            budget.bytes -= bytes;
        } else {
            budget.plain_bytes -= bytes;
        }
    }
    fits
}

impl LogView {
    /// What the filter shows now: matching lines only when that is on and there is a search.
    pub(super) fn filter_spec(&self) -> FilterSpec {
        FilterSpec {
            query: (self.matching_only && !self.query.text.is_empty()).then(|| self.query.clone()),
            errors_only: self.errors_only,
            context: self.context.min(MAX_CONTEXT),
        }
    }

    /// Whether the tab shows a filtered view (or is computing one).
    pub fn is_filtered(&self) -> bool {
        self.filter.is_some()
    }

    /// The rows the filter shows, while one is on and has scanned.
    pub(super) fn filter_rows(&self) -> Option<&std::collections::VecDeque<LineId>> {
        self.filter.as_ref().map(LogFilter::rows)
    }

    /// How many items the list has: every line, or the rows the filter shows.
    pub(super) fn row_count(&self) -> usize {
        match &self.filter {
            None => self.snapshot.lines.len(),
            Some(filter) => filter.rows().len(),
        }
    }

    /// The line drawn as item `ix`.
    pub(super) fn row_id(&self, ix: usize) -> Option<LineId> {
        match &self.filter {
            None => self.snapshot.lines.at(ix).map(|(id, _)| id),
            Some(filter) => filter
                .rows()
                .get(ix)
                .copied()
                .filter(|id| self.snapshot.lines.contains(*id)),
        }
    }

    /// The item line `id` is drawn as, if it is shown.
    pub(super) fn row_index(&self, id: LineId) -> Option<usize> {
        let lines = &self.snapshot.lines;
        if !lines.contains(id) {
            return None;
        }
        match &self.filter {
            None => Some((id - lines.first_id()) as usize),
            Some(filter) => filter.index_of(id),
        }
    }

    /// The newest row shown.
    pub(super) fn last_row(&self) -> Option<LineId> {
        self.row_count()
            .checked_sub(1)
            .and_then(|ix| self.row_id(ix))
    }

    /// The row `delta` rows from line `id` (which may be hidden: then from where it would be),
    /// clamped to the rows shown.
    pub(super) fn row_from(&self, id: LineId, delta: i64) -> Option<LineId> {
        let count = self.row_count();
        if count == 0 {
            return None;
        }
        let at = match (&self.filter, self.row_index(id)) {
            (_, Some(ix)) => ix as i64,
            (Some(filter), None) => {
                let after = filter.rows().partition_point(|r| *r < id) as i64;
                if delta < 0 { after } else { after - 1 }
            }
            (None, None) => return None,
        };
        let target = (at + delta).clamp(0, count as i64 - 1) as usize;
        self.row_id(target)
    }

    /// Starts the filter over for what is set now. The first scan runs on the core's threads
    /// after `delay` (a query being typed); until it lands, what was shown stays (every line,
    /// or the previous filter's rows, kept up to date). Turning the last toggle off shows every
    /// line again at once.
    pub(super) fn refilter(&mut self, delay: Option<std::time::Duration>, cx: &mut Context<Self>) {
        self.filter_generation += 1;
        // A scan or catch-up of the old filter is moot now.
        self.filter_scan.stop();
        self.catching_up = false;
        self._catch_up = Task::ready(());
        let spec = self.filter_spec();
        if !spec.is_active() {
            self.filtering = false;
            self._filter = Task::ready(());
            if self.filter.is_some() {
                let anchor = self.scroll_anchor();
                self.filter = None;
                // Only a filtered tab uses it.
                *self.shown_matches.borrow_mut() = None;
                self.rows_generation += 1;
                self.list.reset(self.snapshot.lines.len());
                self.scroll_back_to(anchor);
            }
            cx.notify();
            return;
        }
        self.filtering = true;
        let generation = self.filter_generation;
        let core = self.core.clone();
        let filter = LogFilter::new(spec);
        self.filter_scan.watch(filter.cancel_handle());
        self._filter = cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let Ok(lines) = this.read_with(cx, |view, _| view.snapshot.lines.clone()) else {
                return;
            };
            let filter = core.filter_logs(filter, lines).await;
            let _ = this.update(cx, |view, cx| {
                if view.filter_generation != generation {
                    return;
                }
                view.filter_scan.finished();
                let anchor = view.scroll_anchor();
                let rows = view.install_filter(filter);
                view.list.reset(rows);
                view.scroll_back_to(anchor);
                view.filtering = false;
                // Lines that came while it scanned are caught up with when the tab is drawn.
                cx.notify();
            });
        });
        cx.notify();
    }

    /// Brings the filter up to the lines shown before the tab is drawn, as the search does: a
    /// hidden tab scans nothing until it is shown. A filter more than [`super::CATCH_UP_ON_UI`]
    /// lines behind (resumed after a long pause) catches up on the core's threads, keeping the
    /// rows shown until it lands.
    pub(super) fn catch_up_filter(&mut self, budget: &mut UiScanBudget, cx: &mut Context<Self>) {
        if self.catching_up {
            return;
        }
        let Some(filter) = &self.filter else {
            return;
        };
        // Budgeted by the filter installed here, not one still on its way.
        let pending = filter.pending(&self.snapshot.lines);
        let fits = match &filter.spec().query {
            Some(query) => catch_up_fits_ui(query, pending, &self.snapshot.lines, budget),
            None => pending <= super::CATCH_UP_ON_UI,
        };
        if fits {
            // What the list holds now, before the update changes it.
            let shown = self.shown_rows();
            if let Some(filter) = &mut self.filter {
                filter.update(&self.snapshot.lines);
            }
            self.splice_rows(shown);
            return;
        }
        // A new filter on its way replaces this one, caught up or not.
        if self.filtering {
            return;
        }
        self.catching_up = true;
        let generation = self.filter_generation;
        let core = self.core.clone();
        let behind = filter.clone();
        let scan = behind.cancel_handle();
        let lines = self.snapshot.lines.clone();
        self.filter_scan.watch(scan);
        self._catch_up = cx.spawn(async move |this, cx| {
            let caught_up = core.filter_logs(behind, lines).await;
            let _ = this.update(cx, |view, cx| {
                if view.filter_generation != generation {
                    return;
                }
                view.filter_scan.finished();
                view.catching_up = false;
                let shown = view.shown_rows();
                view.install_filter(caught_up);
                view.splice_rows(shown);
                cx.notify();
            });
        });
    }

    /// Puts `filter`, scanned on the core's threads, in place of the one shown, without the
    /// lines evicted while it scanned. What was found from the old rows is found again
    /// ([`LogView::shown_matches`]): rows with the same ends can differ in between. Returns how
    /// many rows it has; the caller updates the list.
    pub(super) fn install_filter(&mut self, mut filter: LogFilter) -> usize {
        filter.evict(self.snapshot.lines.first_id());
        let rows = filter.rows().len();
        self.filter = Some(filter);
        self.rows_generation += 1;
        rows
    }

    /// What the list holds now: how many rows, and the last row's id.
    fn shown_rows(&self) -> (usize, Option<LineId>) {
        self.filter
            .as_ref()
            .map_or((self.list.item_count(), None), |f| {
                (f.rows().len(), f.rows().back().copied())
            })
    }

    /// Splices the list from `shown` (as [`LogView::shown_rows`] said before the filter's rows
    /// changed) to the filter's rows now: rows evicted at the top, new rows at the bottom, so
    /// the scroll position and the rows' measured heights stay with their lines.
    fn splice_rows(&mut self, (old_len, old_back): (usize, Option<LineId>)) {
        let Some(filter) = &self.filter else {
            return;
        };
        let rows = filter.rows();
        let appended = match old_back {
            Some(back) => rows.len() - rows.partition_point(|id| *id <= back),
            None => rows.len().saturating_sub(old_len),
        };
        let evicted = old_len.saturating_sub(rows.len() - appended);
        let was_empty = old_len == evicted;
        if evicted > 0 {
            self.list.splice(0..evicted, 0);
            // The new first row may gain a day separator.
            if self.timestamps && self.list.item_count() > 0 {
                self.list.remeasure_items(0..1);
            }
        }
        if appended > 0 {
            let held = self.held_anchor.take();
            let end = self.list.item_count();
            self.list.splice(end..end, appended);
            // The first rows after a filter showed none go back to the place it held, unless
            // the list follows the tail.
            if was_empty && !self.list.is_following_tail() {
                self.scroll_back_to(held);
            }
        }
    }

    /// Finds the matches of the long lines drawn whole on a background thread, one batch at a
    /// time: the UI thread only tests the start of a line with a regex. Holds the snapshot only
    /// while it runs.
    pub(super) fn scan_whole_lines(&mut self, cx: &mut Context<Self>) {
        if self.whole_line_scanning {
            return;
        }
        let ids = self.highlights.borrow_mut().take_wanted();
        if ids.is_empty() {
            return;
        }
        self.whole_line_scanning = true;
        let query = self.query.clone();
        let lines = self.snapshot.lines.clone();
        let stop = ScanCancel::default();
        self.whole_line_scan.watch(stop.clone());
        self._whole_lines = cx.spawn(async move |this, cx| {
            let matcher = query.matcher();
            let found = cx
                .background_spawn(async move {
                    let mut found = Vec::with_capacity(ids.len());
                    for id in ids {
                        if stop.is_cancelled() {
                            break;
                        }
                        if let Some(line) = lines.get(id) {
                            found.push((id, matcher.whole_line_ranges(&line.text)));
                        }
                    }
                    found
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.whole_line_scanning = false;
                view.whole_line_scan.finished();
                view.highlights.borrow_mut().store_whole(&query, found);
                cx.notify();
            });
        });
    }

    /// Drops a whole-line scan of a query that is no longer searched, and what was asked of it:
    /// its lines are asked for again if the query comes back.
    pub(super) fn stop_whole_line_scan(&mut self) {
        self.whole_line_scan.stop();
        self.whole_line_scanning = false;
        self._whole_lines = Task::ready(());
        self.highlights.borrow_mut().forget();
    }

    /// The line at the top of the list, unless it follows the tail: where the list goes back
    /// to when its rows are replaced ([`LogView::scroll_back_to`]).
    fn scroll_anchor(&self) -> Option<LineId> {
        // A filter that showed nothing kept the place from before it.
        if self.row_count() == 0 {
            return self.held_anchor;
        }
        if self.list.is_following_tail() {
            return None;
        }
        self.row_id(self.list.logical_scroll_top().item_ix)
    }

    /// Scrolls the list, its rows just replaced, back to `anchor`, or the first row after it
    /// (the last row when none is), so changing a filter keeps the reader's place. With no rows,
    /// the place is held for the next rows.
    fn scroll_back_to(&mut self, anchor: Option<LineId>) {
        self.held_anchor = None;
        let Some(id) = anchor else {
            return;
        };
        let count = self.row_count();
        if count == 0 {
            self.held_anchor = Some(id);
            return;
        }
        let ix = match self.filter_rows() {
            Some(rows) => rows.partition_point(|row| *row < id),
            None => id.saturating_sub(self.snapshot.lines.first_id()) as usize,
        };
        self.list.scroll_to(ListOffset {
            item_ix: ix.min(count - 1),
            offset_in_item: px(0.),
        });
    }

    /// How many rows are shown between `from` and `to` (line ids, inclusive), without listing
    /// them.
    pub(super) fn rows_count_between(&self, from: LineId, to: LineId) -> usize {
        let (from, to) = (
            from.min(to),
            from.max(to)
                .min(self.snapshot.lines.end_id().saturating_sub(1)),
        );
        let from = from.max(self.snapshot.lines.first_id());
        self.filter_rows().map_or(0, |rows| {
            let start = rows.partition_point(|id| *id < from);
            let end = rows.partition_point(|id| *id <= to);
            end.saturating_sub(start)
        })
    }

    /// The rows shown between `from` and `to` (line ids, inclusive): what a copy or save of a
    /// selection writes in a filtered tab.
    pub(super) fn rows_between(&self, from: LineId, to: LineId) -> Vec<LineId> {
        let (from, to) = (from.min(to), from.max(to));
        self.filter_rows().map_or_else(Vec::new, |rows| {
            let start = rows.partition_point(|id| *id < from);
            let end = rows.partition_point(|id| *id <= to);
            rows.range(start..end)
                .copied()
                .filter(|id| self.snapshot.lines.contains(*id))
                .collect()
        })
    }
}
