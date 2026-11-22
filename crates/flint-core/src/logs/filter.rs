//! [`LogFilter`]: the lines a log tab shows when it is filtered, kept up to date snapshot by
//! snapshot, as [`super::LogSearch`] keeps its matches (#264).
//!
//! A line passes when it matches the search (with "Matching lines only") and, with "Errors and
//! warnings", its severity is a warning or an error ([`super::Severity`], the classifier that
//! colours lines). Each passing line brings up to `context` lines before and after it, as
//! `grep -C` does. Like the search, the filter scans only lines it has not seen and forgets
//! evicted ones, so following a chatty pod costs time for the new lines only; the first scan of
//! a full buffer runs off the UI thread ([`crate::CoreHandle::filter_logs`]).

use std::collections::VecDeque;

use super::buffer::LogLines;
use super::line::{LineId, LogLine, Severity};
use super::pattern::ScanCancel;
use super::search::{SearchMatcher, SearchQuery};

/// Most lines of context a filter keeps around each passing line.
pub const MAX_CONTEXT: usize = 100;

/// What a filtered tab shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterSpec {
    /// Only lines matching this query (`None`: any line). An empty query matches nothing, so a
    /// front end passes `None` while the search is empty.
    pub query: Option<SearchQuery>,
    /// Only warnings and errors.
    pub errors_only: bool,
    /// Lines shown before and after each line that passes, up to [`MAX_CONTEXT`].
    pub context: usize,
}

impl FilterSpec {
    /// Whether this filters anything: a tab with neither toggle shows every line.
    pub fn is_active(&self) -> bool {
        self.query.is_some() || self.errors_only
    }
}

/// The lines of one session that a [`FilterSpec`] shows, kept up to date snapshot by snapshot.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    spec: FilterSpec,
    matcher: Option<SearchMatcher>,
    /// The ids shown, ascending: passing lines and their context.
    rows: VecDeque<LineId>,
    /// Lines below this id have been scanned.
    scanned_to: LineId,
    /// Lines still to show after the last passing line.
    after: usize,
    /// The last lines scanned and not shown, newest last, at most `context` of them: the
    /// context before the next passing line.
    before: VecDeque<LineId>,
    /// Ends a scan early; clones of this filter share it.
    cancel: ScanCancel,
}

impl LogFilter {
    pub fn new(mut spec: FilterSpec) -> Self {
        spec.context = spec.context.min(MAX_CONTEXT);
        LogFilter {
            matcher: spec.query.as_ref().map(SearchQuery::matcher),
            spec,
            ..LogFilter::default()
        }
    }

    pub fn spec(&self) -> &FilterSpec {
        &self.spec
    }

    /// Stops a scan of this filter or of a clone of it: the scan ends at the next line, and
    /// [`LogFilter::pending`] counts the lines it left.
    pub fn cancel_handle(&self) -> ScanCancel {
        self.cancel.clone()
    }

    /// Whether `line` passes, context aside.
    fn passes(&self, line: &LogLine) -> bool {
        let severe =
            !self.spec.errors_only || matches!(line.severity, Severity::Warning | Severity::Error);
        severe && self.matcher.as_ref().is_none_or(|m| m.matches_line(line))
    }

    /// Catches up with `lines`: drops rows that were evicted and scans lines not seen yet.
    /// Pass snapshots of one session only, in order.
    pub fn update(&mut self, lines: &LogLines) {
        let first = lines.first_id();
        self.evict(first);
        // Lines were evicted before they were scanned (a long pause): context does not reach
        // across the gap.
        if first > self.scanned_to {
            self.after = 0;
            self.before.clear();
        }
        let from = self.scanned_to.max(first);
        let context = self.spec.context;
        let mut reached = lines.end_id();
        for (id, line) in lines.range(from, lines.end_id()) {
            if self.cancel.is_cancelled() {
                reached = id;
                break;
            }
            if self.passes(line) {
                self.rows.extend(self.before.drain(..));
                self.rows.push_back(id);
                self.after = context;
            } else if self.after > 0 {
                self.rows.push_back(id);
                self.after -= 1;
            } else if context > 0 {
                if self.before.len() == context {
                    self.before.pop_front();
                }
                self.before.push_back(id);
            }
        }
        self.scanned_to = self.scanned_to.max(reached);
    }

    /// Drops the rows before `first` (evicted from the buffer) without scanning anything, and
    /// says how many went: a view keeps its list to the lines present between scans.
    pub fn evict(&mut self, first: LineId) -> usize {
        let drop = self.rows.partition_point(|id| *id < first);
        self.rows.drain(..drop);
        self.before.retain(|id| *id >= first);
        drop
    }

    /// How many lines of `lines` the next [`LogFilter::update`] would scan.
    pub fn pending(&self, lines: &LogLines) -> u64 {
        lines
            .end_id()
            .saturating_sub(self.scanned_to.max(lines.first_id()))
    }

    /// The line ids shown, oldest first.
    pub fn rows(&self) -> &VecDeque<LineId> {
        &self.rows
    }

    /// Where line `id` is among the rows, if it is shown.
    pub fn index_of(&self, id: LineId) -> Option<usize> {
        self.rows.binary_search(&id).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::buffer::LogBuffer;

    fn buffer(lines: &[&str], limit: usize) -> LogBuffer {
        let mut buffer = LogBuffer::new(limit);
        for text in lines {
            buffer.push(LogLine::parse(text, false));
        }
        buffer
    }

    fn rows(filter: &LogFilter) -> Vec<LineId> {
        filter.rows().iter().copied().collect()
    }

    fn matching(text: &str, context: usize) -> FilterSpec {
        FilterSpec {
            query: Some(SearchQuery::new(text)),
            errors_only: false,
            context,
        }
    }

    // covers UP-48
    #[test]
    fn matching_lines_only_with_context_around_each() {
        let mut lines = buffer(
            &[
                "a", "b", "GET /x", "c", "d", "e", "GET /y", "GET /z", "f", "g", "h",
            ],
            100,
        );
        let mut filter = LogFilter::new(matching("get", 0));
        filter.update(&lines.view());
        assert_eq!(rows(&filter), [2, 6, 7]);

        // One line of context: neighbours once, overlapping context merged.
        let mut filter = LogFilter::new(matching("get", 1));
        filter.update(&lines.view());
        assert_eq!(rows(&filter), [1, 2, 3, 5, 6, 7, 8]);
        assert_eq!(filter.index_of(5), Some(3));
        assert_eq!(filter.index_of(4), None);

        // Following: new lines are scanned, the after-context carries over, and lines before a
        // new match come in.
        lines.push(LogLine::parse("i", false));
        lines.push(LogLine::parse("GET /w", false));
        filter.update(&lines.view());
        assert_eq!(rows(&filter), [1, 2, 3, 5, 6, 7, 8, 11, 12]);
        lines.push(LogLine::parse("after", false));
        lines.push(LogLine::parse("not shown", false));
        filter.update(&lines.view());
        assert_eq!(*filter.rows().back().unwrap(), 13);
        assert_eq!(filter.pending(&lines.view()), 0);
    }

    // covers UP-48: rows of evicted lines go without a scan, the rest stay.
    #[test]
    fn evicting_drops_only_the_rows_before_the_first_line() {
        let lines = buffer(&["GET /a", "b", "GET /c", "d", "GET /e"], 100);
        let mut filter = LogFilter::new(matching("get", 0));
        filter.update(&lines.view());
        assert_eq!(filter.evict(3), 2);
        assert_eq!(rows(&filter), [4]);
        assert_eq!(filter.evict(3), 0);
        assert_eq!(filter.pending(&lines.view()), 0, "nothing to scan again");
    }

    // covers UP-48
    #[test]
    fn errors_only_reuses_the_severity_classifier_and_combines_with_a_search() {
        let lines = buffer(
            &[
                "INFO starting",
                "WARN slow request /a",
                "ERROR failed /b",
                "DEBUG noise",
                "level=error msg=\"db down\" path=/a",
                "fine",
            ],
            100,
        );
        let mut errors = LogFilter::new(FilterSpec {
            errors_only: true,
            ..FilterSpec::default()
        });
        errors.update(&lines.view());
        assert_eq!(rows(&errors), [1, 2, 4]);

        let mut both = LogFilter::new(FilterSpec {
            query: Some(SearchQuery::new("/a")),
            errors_only: true,
            context: 0,
        });
        both.update(&lines.view());
        assert_eq!(rows(&both), [1, 4]);
        assert!(!FilterSpec::default().is_active());
        assert!(both.spec().is_active());
    }

    // covers UP-48
    #[test]
    fn evicted_rows_go_and_context_never_reaches_before_the_buffer() {
        let mut lines = buffer(&["x", "x", "hit", "x", "x"], 4);
        let mut filter = LogFilter::new(matching("hit", 3));
        filter.update(&lines.view());
        // The buffer starts at line 1: the context stops there.
        assert_eq!(rows(&filter), [1, 2, 3, 4]);
        for _ in 0..3 {
            lines.push(LogLine::parse("x", false));
        }
        filter.update(&lines.view());
        // Lines 0..4 are evicted; 5 is the last of the hit's after-context.
        assert_eq!(rows(&filter), [4, 5]);
        // A gap: lines evicted unscanned end the after-context of the hit before them.
        let mut gap = buffer(&["x", "hit"], 4);
        let mut filter = LogFilter::new(matching("hit", 3));
        filter.update(&gap.view());
        assert_eq!(rows(&filter), [0, 1]);
        for _ in 0..10 {
            gap.push(LogLine::parse("x", false));
        }
        filter.update(&gap.view());
        assert!(filter.rows().is_empty(), "{:?}", filter.rows());
        // Context is capped.
        let wide = LogFilter::new(matching("hit", 10_000));
        assert_eq!(wide.spec().context, MAX_CONTEXT);
    }

    // #309: a regex filters like a plain query, and a cancelled scan leaves the rest pending.
    #[test]
    fn a_regex_filter_keeps_context_and_stops_when_cancelled() {
        let lines = buffer(&["a", "status=500", "b", "status=200", "c"], 100);
        let spec = FilterSpec {
            query: Some(SearchQuery {
                regex: true,
                ..SearchQuery::new(r"status=5\d\d")
            }),
            errors_only: false,
            context: 1,
        };
        let mut filter = LogFilter::new(spec.clone());
        filter.update(&lines.view());
        assert_eq!(rows(&filter), [0, 1, 2]);

        let mut cancelled = LogFilter::new(spec);
        cancelled.cancel_handle().cancel();
        cancelled.update(&lines.view());
        assert!(cancelled.rows().is_empty());
        assert_eq!(cancelled.pending(&lines.view()), 5);
    }
}
