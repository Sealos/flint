//! A log tab's filtered view (#264, UP-48): "Matching lines only" (with N lines of context),
//! "Errors and warnings", or both. A core [`LogFilter`] keeps the ids of the lines shown, scanning
//! only new lines when the tab is drawn; the first scan of a new filter, and a long catch-up,
//! run on the core's threads ([`flint_core::CoreHandle::filter_logs`]), so a 100k-line buffer
//! filters without holding up a frame. The list then has one item per row the filter shows, and every place
//! that goes from an item to a line, and back, goes through [`LogView::row_id`] and
//! [`LogView::row_index`].
// module-tests: log_tests.rs

use flint_core::logs::MAX_CONTEXT;
use flint_core::{FilterSpec, LineId, LogFilter};
use gpui_kit::*;

use super::LogView;

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
        // A catch-up of the old filter is moot now.
        self.catching_up = false;
        self._catch_up = Task::ready(());
        let spec = self.filter_spec();
        if !spec.is_active() {
            self.filtering = false;
            self._filter = Task::ready(());
            if self.filter.take().is_some() {
                self.list.reset(self.snapshot.lines.len());
            }
            cx.notify();
            return;
        }
        self.filtering = true;
        // The search is still being prepared: it filters once it is ([`LogView::use_matcher`]).
        if spec.query.is_some() && self.query_pending {
            self._filter = Task::ready(());
            cx.notify();
            return;
        }
        // The search's matcher, compiled once (never here, on the UI thread).
        let matcher = spec.query.is_some().then(|| self.matcher.clone());
        let generation = self.filter_generation;
        let core = self.core.clone();
        self._filter = cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let Ok(lines) = this.read_with(cx, |view, _| view.snapshot.lines.clone()) else {
                return;
            };
            let filter = core
                .filter_logs(LogFilter::with_matcher(spec, matcher), lines)
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.filter_generation != generation {
                    return;
                }
                view.list.reset(filter.rows().len());
                view.filter = Some(filter);
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
    pub(super) fn catch_up_filter(&mut self, cx: &mut Context<Self>) {
        if self.catching_up {
            return;
        }
        let Some(filter) = &self.filter else {
            return;
        };
        if filter.pending(&self.snapshot.lines) <= super::CATCH_UP_ON_UI {
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
        let lines = self.snapshot.lines.clone();
        self._catch_up = cx.spawn(async move |this, cx| {
            let caught_up = core.filter_logs(behind, lines).await;
            let _ = this.update(cx, |view, cx| {
                if view.filter_generation != generation {
                    return;
                }
                view.catching_up = false;
                let shown = view.shown_rows();
                view.filter = Some(caught_up);
                view.splice_rows(shown);
                cx.notify();
            });
        });
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
        if evicted > 0 {
            self.list.splice(0..evicted, 0);
            // The new first row may gain a day separator.
            if self.timestamps && self.list.item_count() > 0 {
                self.list.remeasure_items(0..1);
            }
        }
        if appended > 0 {
            let end = self.list.item_count();
            self.list.splice(end..end, appended);
        }
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
