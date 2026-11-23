//! Search over a session's lines that keeps matching new lines while the session follows.
//!
//! A [`LogSearch`] remembers how far it has scanned. Each [`LogSearch::update`] with a newer
//! snapshot scans only the lines that arrived since and forgets matches that were evicted, so
//! following a chatty pod with a search open costs time proportional to the new lines, not to
//! the buffer. There is no index: the query changes with every keystroke, and a full scan of a
//! 100k-line buffer takes a few milliseconds, while an n-gram index would cost about as much
//! memory as the buffer itself. Lines are tested with a [`SearchMatcher`], prepared once per
//! query, which allocates nothing per line. Run the first scan of a new query off the UI thread
//! ([`crate::CoreHandle::search_logs`]); incremental updates are cheap anywhere, and
//! [`LogSearch::pending`] says how far behind a search is that was left alone for a while.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Range;

use memchr::memmem;
use regex::Regex;

use super::buffer::LogLines;
use super::line::{LineId, LogLine};
use super::pattern::{self, LineRanges, PatternError, ScanCancel};

/// What to look for: a plain substring of a line's text (timestamps are not searched), or a
/// regular expression with [`SearchQuery::regex`], and optionally of its `pod/container` source.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    pub text: String,
    /// Off by default: `error` finds `ERROR`.
    pub case_sensitive: bool,
    /// Also match a workload line's `pod/container` source, as the prefix shows it (set this
    /// when the prefix is shown). Off by default.
    pub sources: bool,
    /// Read `text` as a regular expression. Off by default. A pattern that does not compile
    /// matches nothing; [`SearchQuery::pattern_error`] says why.
    pub regex: bool,
}

impl SearchQuery {
    pub fn new(text: impl Into<String>) -> Self {
        SearchQuery {
            text: text.into(),
            case_sensitive: false,
            sources: false,
            regex: false,
        }
    }

    /// Why this query's pattern cannot be used, for a regex query that does not compile.
    pub fn pattern_error(&self) -> Option<PatternError> {
        (self.regex && !self.text.is_empty())
            .then(|| pattern::compile(&self.text, self.case_sensitive).err())
            .flatten()
    }

    /// The query made ready to test many lines: what [`LogSearch`] scans with.
    pub fn matcher(&self) -> SearchMatcher {
        SearchMatcher::new(self)
    }

    /// Whether `line` matches: its text, or its source when [`SearchQuery::sources`] is on.
    /// [`SearchQuery::match_ranges`] highlights the text; for a match in the prefix, pass it the
    /// source's [`label`](crate::LineSource::label) too. To test many lines, use a
    /// [`SearchMatcher`].
    pub fn matches_line(&self, line: &LogLine) -> bool {
        self.matcher().matches_line(line)
    }

    /// Byte ranges of every non-overlapping match in `text`, for highlighting. Empty for an
    /// empty query.
    pub fn match_ranges(&self, text: &str) -> Vec<Range<usize>> {
        self.matcher().match_ranges(text)
    }

    /// [`SearchQuery::match_ranges`], saying whether a limit left matches out.
    pub fn highlight_ranges(&self, text: &str) -> LineRanges {
        self.matcher().highlight_ranges(text)
    }

    /// Whether `text` contains the query. An empty query matches nothing.
    pub fn is_match(&self, text: &str) -> bool {
        self.matcher().is_match(text)
    }
}

/// A [`SearchQuery`] made ready to test many lines: the pattern is prepared once, and testing a
/// line allocates nothing (what it needs goes into buffers kept from line to line).
///
/// - **Case-sensitive**, or ASCII with no letters: a substring search ([`memmem`]).
/// - **ASCII, either case**: the same search in the text with its ASCII lowercased, which keeps
///   byte offsets.
/// - **Other text, either case**: a linear (Knuth-Morris-Pratt) search over the text's chars
///   with simple lowercase folding, reading the text in place, so offsets stay those of the
///   text and no copy of it is made.
/// - **Regex**: a linear-time engine with a compiled size limit (see `pattern`); highlighting
///   stops at a fixed amount of work per line.
#[derive(Debug, Clone, Default)]
pub struct SearchMatcher {
    find: Find,
    sources: bool,
    /// The text being tested, lowercased.
    scratch: RefCell<Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
enum Find {
    /// The empty query: matches nothing.
    #[default]
    Nothing,
    Exact(memmem::Finder<'static>),
    /// The pattern in lowercase ASCII.
    AsciiFolded(memmem::Finder<'static>),
    Folded(FoldedPattern),
    Regex(Regex),
}

impl SearchMatcher {
    pub fn new(query: &SearchQuery) -> Self {
        let text = query.text.as_str();
        let find = if text.is_empty() {
            Find::Nothing
        } else if query.regex {
            pattern::compile(text, query.case_sensitive).map_or(Find::Nothing, Find::Regex)
        } else if query.case_sensitive
            || (text.is_ascii() && !text.bytes().any(|b| b.is_ascii_alphabetic()))
        {
            Find::Exact(memmem::Finder::new(text).into_owned())
        } else if text.is_ascii() {
            Find::AsciiFolded(memmem::Finder::new(&text.to_ascii_lowercase()).into_owned())
        } else {
            Find::Folded(FoldedPattern::new(text))
        };
        SearchMatcher {
            find,
            sources: query.sources,
            scratch: RefCell::default(),
        }
    }

    /// Whether `line` matches: its text, or its source when the query covers sources.
    pub fn matches_line(&self, line: &LogLine) -> bool {
        self.is_match(&line.text)
            || (self.sources
                && line
                    .source
                    .as_ref()
                    .is_some_and(|s| self.is_match(s.label())))
    }

    /// Whether `text` contains the query.
    pub fn is_match(&self, text: &str) -> bool {
        match &self.find {
            Find::Nothing => false,
            Find::Exact(finder) => finder.find(text.as_bytes()).is_some(),
            Find::AsciiFolded(finder) => finder.find(&self.lowered(text)).is_some(),
            Find::Folded(pattern) => {
                let mut found = false;
                pattern.scan(text, |_| {
                    found = true;
                    false
                });
                found
            }
            Find::Regex(regex) => regex.is_match(text),
        }
    }

    /// Byte ranges of every non-overlapping match in `text`. The text is lowercased once,
    /// however many matches it has: a long wrapped line is highlighted in every frame.
    pub fn match_ranges(&self, text: &str) -> Vec<Range<usize>> {
        self.highlight_ranges(text).ranges
    }

    /// [`SearchMatcher::match_ranges`], saying whether a limit left matches out. Only a regex
    /// has limits.
    pub fn highlight_ranges(&self, text: &str) -> LineRanges {
        let ranges = |finder: &memmem::Finder<'_>, hay: &[u8]| {
            let len = finder.needle().len();
            finder.find_iter(hay).map(|at| at..at + len).collect()
        };
        let ranges = match &self.find {
            Find::Nothing => Vec::new(),
            Find::Exact(finder) => ranges(finder, text.as_bytes()),
            // Lowercasing ASCII keeps byte offsets, so ranges map back onto `text`.
            Find::AsciiFolded(finder) => ranges(finder, &self.lowered(text)),
            Find::Folded(pattern) => {
                let mut out = Vec::new();
                pattern.scan(text, |found| {
                    out.push(found);
                    true
                });
                out
            }
            Find::Regex(regex) => return pattern::bounded_ranges(regex, text),
        };
        LineRanges {
            ranges,
            complete: true,
        }
    }

    /// [`SearchMatcher::highlight_ranges`] over a whole long line, for a background thread: a
    /// regex is not cut to what the UI thread may test.
    pub fn whole_line_ranges(&self, text: &str) -> LineRanges {
        match &self.find {
            Find::Regex(regex) => pattern::whole_line_ranges(regex, text),
            _ => self.highlight_ranges(text),
        }
    }

    /// `text` with its ASCII lowercased, in the buffer kept from line to line.
    fn lowered(&self, text: &str) -> std::cell::RefMut<'_, Vec<u8>> {
        let mut lower = self.scratch.borrow_mut();
        lower.clear();
        lower.extend(text.bytes().map(|b| b.to_ascii_lowercase()));
        lower
    }
}

/// Marks a lowercased char that does not start a char of the text.
const INSIDE_A_CHAR: usize = usize::MAX;

/// A non-empty pattern, lowercased, ready for a Knuth-Morris-Pratt search over a text's chars as
/// they lowercase: time linear in the text, whatever the two hold.
#[derive(Debug, Clone)]
struct FoldedPattern {
    chars: Vec<char>,
    /// For each prefix of `chars`, the length of its longest proper prefix that is also a suffix.
    borders: Vec<usize>,
    /// Where the last `chars.len()` lowercased chars began in the text, as a ring.
    starts: RefCell<Vec<usize>>,
}

impl FoldedPattern {
    fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
        let mut borders = vec![0; chars.len()];
        let mut k = 0;
        for i in 1..chars.len() {
            while k > 0 && chars[i] != chars[k] {
                k = borders[k - 1];
            }
            if chars[i] == chars[k] {
                k += 1;
            }
            borders[i] = k;
        }
        FoldedPattern {
            starts: RefCell::new(vec![INSIDE_A_CHAR; chars.len()]),
            chars,
            borders,
        }
    }

    /// Calls `found` with the byte range of each non-overlapping match, leftmost first, until it
    /// returns `false`. A match starts and ends on a char of the text: a char that lowercases to
    /// several must match all of them.
    fn scan(&self, text: &str, mut found: impl FnMut(Range<usize>) -> bool) {
        let len = self.chars.len();
        let mut starts = self.starts.borrow_mut();
        starts.fill(INSIDE_A_CHAR);
        let (mut k, mut seen, mut free) = (0, 0, 0);
        for (at, c) in text.char_indices() {
            let mut lower = c.to_lowercase();
            let mut start = at;
            while let Some(lc) = lower.next() {
                starts[seen % len] = start;
                start = INSIDE_A_CHAR;
                seen += 1;
                while k > 0 && self.chars[k] != lc {
                    k = self.borders[k - 1];
                }
                if self.chars[k] == lc {
                    k += 1;
                }
                if k < len {
                    continue;
                }
                k = self.borders[len - 1];
                // The ring's oldest entry is where this match began.
                let from = starts[seen % len];
                if lower.len() == 0 && from != INSIDE_A_CHAR && from >= free {
                    free = at + c.len_utf8();
                    if !found(from..free) {
                        return;
                    }
                }
            }
        }
    }
}

/// The lines of one session that match a query, kept up to date snapshot by snapshot.
#[derive(Debug, Clone, Default)]
pub struct LogSearch {
    query: SearchQuery,
    matcher: SearchMatcher,
    /// Matching line ids, ascending.
    matches: VecDeque<LineId>,
    /// Lines below this id have been scanned.
    scanned_to: LineId,
    /// Ends the next scan early; each scan replaces it (see [`ScanCancel`]).
    cancel: ScanCancel,
}

impl LogSearch {
    pub fn new(query: SearchQuery) -> Self {
        LogSearch {
            matcher: query.matcher(),
            query,
            matches: VecDeque::new(),
            scanned_to: 0,
            cancel: ScanCancel::default(),
        }
    }

    pub fn query(&self) -> &SearchQuery {
        &self.query
    }

    /// Stops the next scan of this search or of a clone made from it now: the scan ends at the
    /// next line, [`LogSearch::pending`] counts the lines it left, and a later
    /// [`LogSearch::update`] scans them. See [`ScanCancel`].
    pub fn cancel_handle(&self) -> ScanCancel {
        self.cancel.clone()
    }

    /// Catches up with `lines`: drops matches that were evicted and scans lines not seen yet.
    /// Pass snapshots of one session only, in order.
    pub fn update(&mut self, lines: &LogLines) {
        let first = lines.first_id();
        let drop = self.matches.partition_point(|id| *id < first);
        self.matches.drain(..drop);
        let from = self.scanned_to.max(first);
        let mut reached = lines.end_id();
        let cancel = std::mem::take(&mut self.cancel);
        if !self.query.text.is_empty() {
            for (id, line) in lines.range(from, lines.end_id()) {
                if cancel.claim() {
                    reached = id;
                    break;
                }
                if self.matcher.matches_line(line) {
                    self.matches.push_back(id);
                }
            }
        }
        self.scanned_to = self.scanned_to.max(reached);
    }

    /// How many lines of `lines` the next [`LogSearch::update`] would scan: a front end that
    /// put off scanning (for a hidden tab) can tell a short catch-up from a long one.
    pub fn pending(&self, lines: &LogLines) -> u64 {
        lines
            .end_id()
            .saturating_sub(self.scanned_to.max(lines.first_id()))
    }

    /// Whether a match found so far is still in `lines`: what an action's guard asks between two
    /// updates. It tests no line, so polling it costs nothing however far behind the search is;
    /// [`LogSearch::update`] tests the rest, and [`LogSearch::pending`] says how many are left.
    pub fn has_match_in(&self, lines: &LogLines) -> bool {
        let (first, end) = (lines.first_id(), lines.end_id());
        let present = self.matches.partition_point(|id| *id < first);
        self.matches.get(present).is_some_and(|id| *id < end)
    }

    /// How many matches found so far are still in `lines`, as [`LogSearch::has_match_in`] sees
    /// them: a lower bound until the next update.
    pub fn count_in(&self, lines: &LogLines) -> usize {
        let first = lines.first_id();
        let present = self.matches.partition_point(|id| *id < first);
        self.matches.len() - present
    }

    /// Matching line ids, oldest first.
    pub fn matches(&self) -> &VecDeque<LineId> {
        &self.matches
    }

    pub fn count(&self) -> usize {
        self.matches.len()
    }

    pub fn is_match(&self, id: LineId) -> bool {
        self.matches.binary_search(&id).is_ok()
    }

    /// The first match after `id` (or the first match at all, for `None`), for "next".
    pub fn next_after(&self, id: Option<LineId>) -> Option<LineId> {
        match id {
            None => self.matches.front().copied(),
            Some(id) => {
                let at = self.matches.partition_point(|m| *m <= id);
                self.matches.get(at).copied()
            }
        }
    }

    /// The last match before `id` (or the last match at all, for `None`), for "previous".
    pub fn prev_before(&self, id: Option<LineId>) -> Option<LineId> {
        match id {
            None => self.matches.back().copied(),
            Some(id) => {
                let at = self.matches.partition_point(|m| *m < id);
                at.checked_sub(1).and_then(|i| self.matches.get(i).copied())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::buffer::LogBuffer;
    use crate::logs::line::LogLine;

    fn push(buffer: &mut LogBuffer, text: &str) {
        buffer.push(LogLine::parse(text, false));
    }

    // covers UP-40
    #[test]
    fn keeps_matching_new_lines_and_forgets_evicted_ones() {
        let mut buffer = LogBuffer::new(4);
        for t in ["GET /a", "post /b", "GET /c"] {
            push(&mut buffer, t);
        }
        let mut search = LogSearch::new(SearchQuery::new("get"));
        search.update(&buffer.view());
        assert_eq!(search.matches(), &[0, 2]);

        // Following: only the new lines are scanned; evicted matches go away.
        for t in ["get /d", "noise", "x GET"] {
            push(&mut buffer, t);
        }
        search.update(&buffer.view());
        assert_eq!(search.matches(), &[2, 3, 5]);
        assert_eq!(search.count(), 3);
        assert!(search.is_match(3) && !search.is_match(4));

        // Updating with the same snapshot again changes nothing.
        search.update(&buffer.view());
        assert_eq!(search.count(), 3);

        assert_eq!(search.next_after(None), Some(2));
        assert_eq!(search.next_after(Some(2)), Some(3));
        assert_eq!(search.next_after(Some(5)), None);
        assert_eq!(search.prev_before(None), Some(5));
        assert_eq!(search.prev_before(Some(3)), Some(2));
        assert_eq!(search.prev_before(Some(2)), None);
    }

    #[test]
    fn case_sensitivity_and_empty_queries() {
        let mut sensitive = SearchQuery::new("Err");
        sensitive.case_sensitive = true;
        assert!(sensitive.is_match("an Err here"));
        assert!(!sensitive.is_match("an err here"));
        assert!(SearchQuery::new("err").is_match("AN ERR"));
        assert!(!SearchQuery::default().is_match("anything"));

        let mut buffer = LogBuffer::new(10);
        push(&mut buffer, "x");
        let mut empty = LogSearch::new(SearchQuery::default());
        empty.update(&buffer.view());
        assert_eq!(empty.count(), 0);
    }

    // covers UP-45
    #[test]
    fn sources_are_searched_only_when_asked() {
        let mut buffer = LogBuffer::new(10);
        let source = std::sync::Arc::new(crate::logs::LineSource::new("web-7f9", "app"));
        for text in ["started", "web request", "done"] {
            let mut line = LogLine::parse(text, false);
            line.source = Some(source.clone());
            buffer.push(line);
        }
        push(&mut buffer, "untagged");
        let mut text_only = LogSearch::new(SearchQuery::new("web"));
        text_only.update(&buffer.view());
        assert_eq!(text_only.matches(), &[1]);

        let mut with_sources = SearchQuery::new("WEB-7F9/APP");
        with_sources.sources = true;
        let mut search = LogSearch::new(with_sources);
        search.update(&buffer.view());
        assert_eq!(search.matches(), &[0, 1, 2]);
    }

    // covers UP-43: a search left alone (a hidden tab) catches up in one update, forgetting
    // what was evicted meanwhile.
    #[test]
    fn a_search_left_behind_catches_up_in_one_update() {
        let mut buffer = LogBuffer::new(5);
        for t in ["error a", "ok", "ERROR b"] {
            push(&mut buffer, t);
        }
        let mut search = LogSearch::new(SearchQuery::new("error"));
        assert_eq!(search.pending(&buffer.view()), 3);
        search.update(&buffer.view());
        assert_eq!(search.pending(&buffer.view()), 0);
        for t in ["ok", "ok", "error c", "ok", "error d"] {
            push(&mut buffer, t);
        }
        // Lines 3..=7 are present; 3 to 7 were never scanned.
        assert_eq!(search.pending(&buffer.view()), 5);
        search.update(&buffer.view());
        assert_eq!(search.matches(), &[5, 7]);
        // Evicted before they were ever scanned: only what is present counts.
        for n in 0..20 {
            push(&mut buffer, &format!("error {n}"));
        }
        assert_eq!(search.pending(&buffer.view()), 5);
        search.update(&buffer.view());
        assert_eq!(search.matches(), &[23, 24, 25, 26, 27]);
    }

    // covers UP-43: between updates, guards read the matches found: evicted ones do not count,
    // and new lines wait for the budgeted update.
    #[test]
    fn has_match_in_reads_found_matches_and_forgets_evicted_ones() {
        let mut buffer = LogBuffer::new(3);
        push(&mut buffer, "error a");
        let mut search = LogSearch::new(SearchQuery::new("error"));
        search.update(&buffer.view());
        assert!(search.has_match_in(&buffer.view()));
        for t in ["ok", "ok", "ok"] {
            push(&mut buffer, t);
        }
        assert_eq!(search.count(), 1, "not updated");
        assert!(
            !search.has_match_in(&buffer.view()),
            "the match was evicted"
        );
        push(&mut buffer, "ERROR b");
        assert!(
            !search.has_match_in(&buffer.view()),
            "a line not scanned yet is left to the update"
        );
        assert_eq!(search.count_in(&buffer.view()), 0);
        search.update(&buffer.view());
        assert!(search.has_match_in(&buffer.view()));
        push(&mut buffer, "error c");
        assert_eq!(search.count_in(&buffer.view()), 1);
        search.update(&buffer.view());
        assert_eq!(search.count_in(&buffer.view()), 2);
        assert!(!LogSearch::default().has_match_in(&buffer.view()));
    }

    #[test]
    fn the_matcher_agrees_with_a_plain_search() {
        // A letterless ASCII pattern takes the substring path; any case matches the same.
        let q = SearchQuery::new("=5");
        assert_eq!(q.match_ranges("a=5 b=50 =="), [1..3, 5..7]);
        // The first byte in either case, then the rest; overlapping candidates are not lost.
        let q = SearchQuery::new("aab");
        assert_eq!(q.match_ranges("AAAB aab"), [1..4, 5..8]);
        assert!(!q.is_match("aa"));
        assert!(SearchQuery::new("b").is_match("aB"));
        // Non-ASCII: a match after multi-byte chars keeps its byte offsets; a char that folds
        // to two must match both.
        let q = SearchQuery::new("straße");
        let text = "öö STRAßE";
        let ranges = q.match_ranges(text);
        assert_eq!(&text[ranges[0].clone()], "STRAßE");
        let q = SearchQuery::new("i\u{307}x");
        assert!(q.is_match("\u{130}x"), "İ folds to i and a combining dot");
        assert!(!q.is_match("\u{130}"));
        // Case-sensitive non-ASCII is a byte search on char boundaries.
        let mut sensitive = SearchQuery::new("ö");
        sensitive.case_sensitive = true;
        assert_eq!(sensitive.match_ranges("Öö ö"), [2..4, 5..7]);
        // Sources are tested only when the query covers them.
        let mut line = LogLine::parse("plain", false);
        line.source = Some(std::sync::Arc::new(crate::logs::LineSource::new(
            "web", "app",
        )));
        let mut q = SearchQuery::new("WEB");
        assert!(!q.matcher().matches_line(&line));
        q.sources = true;
        assert!(q.matcher().matches_line(&line));
    }

    #[test]
    fn match_ranges_point_into_the_original_text() {
        let q = SearchQuery::new("ab");
        assert_eq!(q.match_ranges("xABab abA"), [1..3, 3..5, 6..8]);
        assert!(SearchQuery::default().match_ranges("ab").is_empty());

        let q = SearchQuery::new("äö");
        let text = "x ÄÖ y äö";
        let ranges = q.match_ranges(text);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&text[ranges[0].clone()], "ÄÖ");
        assert_eq!(&text[ranges[1].clone()], "äö");
        assert!(q.is_match("ÄÖ"));

        let mut sensitive = SearchQuery::new("a");
        sensitive.case_sensitive = true;
        assert_eq!(sensitive.match_ranges("aAa"), [0..1, 2..3]);
    }

    /// The ranges `match_ranges` gave before [`SearchMatcher`]: the reference the matcher
    /// must agree with.
    fn reference_ranges(query: &SearchQuery, text: &str) -> Vec<Range<usize>> {
        let needle = query.text.as_str();
        if needle.is_empty() {
            return Vec::new();
        }
        if query.case_sensitive {
            return text
                .match_indices(needle)
                .map(|(at, m)| at..at + m.len())
                .collect();
        }
        if needle.is_ascii() {
            let mut out = Vec::new();
            let (hay, pat) = (text.as_bytes(), needle.as_bytes());
            let mut at = 0;
            while at + pat.len() <= hay.len() {
                if hay[at..at + pat.len()].eq_ignore_ascii_case(pat) {
                    out.push(at..at + pat.len());
                    at += pat.len();
                } else {
                    at += 1;
                }
            }
            return out;
        }
        let pat: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let mut p = 0;
            let mut used = 0;
            let mut ok = true;
            for (_, c) in &chars[i..] {
                if p == pat.len() {
                    break;
                }
                for lc in c.to_lowercase() {
                    if pat.get(p) != Some(&lc) {
                        ok = false;
                        break;
                    }
                    p += 1;
                }
                if !ok {
                    break;
                }
                used += 1;
            }
            if ok && p == pat.len() {
                let start = chars[i].0;
                let end = chars.get(i + used).map_or(text.len(), |(at, _)| *at);
                out.push(start..end);
                i += used.max(1);
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn match_ranges_agree_with_the_reference_on_many_texts() {
        // Texts from a small alphabet, so matches are dense, overlap and sit next to each
        // other; with chars that fold to two (İ), that change byte length (K, the Kelvin
        // sign) and that are not letters.
        let alphabet: Vec<char> = "aAbB \u{e4}\u{c4}\u{df}\u{130}i\u{307}\u{212a}k=5"
            .chars()
            .collect();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let needles = [
            "a", "ab", "aa", "AbA", "=5", "5", "\u{e4}", "\u{e4}a", "\u{df}", "i\u{307}", "k",
            "\u{212a}", "a \u{c4}", "bb",
        ];
        for _ in 0..400 {
            let len = (next() % 40) as usize;
            let text: String = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            for needle in needles {
                for case_sensitive in [false, true] {
                    let query = SearchQuery {
                        case_sensitive,
                        ..SearchQuery::new(needle)
                    };
                    let expected = reference_ranges(&query, &text);
                    assert_eq!(
                        query.match_ranges(&text),
                        expected,
                        "{needle:?} in {text:?} (case-sensitive: {case_sensitive})"
                    );
                    assert_eq!(
                        query.is_match(&text),
                        !expected.is_empty(),
                        "{needle:?} in {text:?}"
                    );
                }
            }
        }
    }

    fn regex(text: &str) -> SearchQuery {
        SearchQuery {
            regex: true,
            ..SearchQuery::new(text)
        }
    }

    // A folded search is linear: a long run of a repeated char that almost matches at every
    // position would otherwise cost the pattern's length at each of them. The limit is far above
    // the linear time and far below the quadratic one.
    #[test]
    fn a_folded_search_is_linear_in_the_text() {
        let needle = format!("{}x", "\u{e9}".repeat(8_000));
        let text = format!("{}x", "\u{e9}".repeat(32_000));
        let query = SearchQuery::new(needle);
        let started = std::time::Instant::now();
        assert!(query.is_match(&text));
        let expected = 48_000..text.len();
        assert_eq!(query.match_ranges(&text), std::slice::from_ref(&expected));
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(500),
            "took {took:?}"
        );
    }

    // #309
    #[test]
    fn a_regex_query_matches_highlights_and_follows_case() {
        let q = regex(r"status=5\d\d");
        assert!(q.is_match("STATUS=503"));
        assert!(!q.is_match("status=200"));
        let text = "x status=500 status=501";
        // The first match is always kept; later ones depend on the highlight's time limit.
        assert_eq!(q.match_ranges(text)[0], 2..12);
        let compiled = crate::logs::pattern::compile(&q.text, false).unwrap();
        let unhurried = std::time::Duration::from_secs(60);
        assert_eq!(
            crate::logs::pattern::ranges_within(&compiled, text, unhurried),
            [2..12, 13..23]
        );
        let sensitive = SearchQuery {
            case_sensitive: true,
            ..q
        };
        assert!(!sensitive.is_match("STATUS=503"));
        // Without the mode, the same text is a plain substring.
        assert!(!SearchQuery::new(r"a.c").is_match("abc"));
        assert!(SearchQuery::new(r"a.c").is_match("a.c"));
        assert_eq!(SearchQuery::new("(").pattern_error(), None);
        assert!(regex(r"a.c").is_match("abc"));
    }

    // #309: a pattern that does not compile says why and matches nothing.
    #[test]
    fn an_invalid_pattern_matches_nothing_and_says_why() {
        let q = regex("(oops");
        assert_eq!(
            q.pattern_error().unwrap().to_string(),
            "Invalid pattern: unclosed group"
        );
        assert!(!q.is_match("(oops"));
        assert!(q.match_ranges("(oops").is_empty());
        assert_eq!(regex("").pattern_error(), None);
        assert!(!regex("").is_match("anything"));
    }

    // #309: patterns that make a backtracking engine run for ever finish on a full buffer of
    // the lines that trigger it. The limit is far above the time taken and only catches a hang.
    #[test]
    fn pathological_patterns_finish_on_a_full_buffer() {
        let mut buffer = LogBuffer::new(100_000);
        let line = "a".repeat(200);
        for _ in 0..100_000 {
            push(&mut buffer, &line);
        }
        let lines = buffer.view();
        let started = std::time::Instant::now();
        for pattern in [r"(a+)+$b", r"(a|aa)+b", r"(.*)*x", r"(a*)*b", r"(x+x+)+y"] {
            let mut search = LogSearch::new(regex(pattern));
            search.update(&lines);
            assert_eq!(search.count(), 0, "{pattern}");
            assert!(regex(pattern).match_ranges(&line).is_empty(), "{pattern}");
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "took {:?}",
            started.elapsed()
        );
    }

    // #309: between scans a guard reads cached matches and tests no line, for any kind of query.
    #[test]
    fn guards_read_only_cached_matches() {
        let mut buffer = LogBuffer::new(10);
        push(&mut buffer, "needle");
        let lines = buffer.view();
        let mut search = LogSearch::new(regex("needle"));
        assert!(!search.has_match_in(&lines));
        assert_eq!(search.count_in(&lines), 0);
        let plain = LogSearch::new(SearchQuery::new("needle"));
        assert!(!plain.has_match_in(&lines));
        assert_eq!(plain.count_in(&lines), 0);

        search.update(&lines);
        assert!(search.has_match_in(&lines));
        push(&mut buffer, "needle");
        assert_eq!(
            search.count_in(&buffer.view()),
            1,
            "the new line is pending"
        );
        search.update(&buffer.view());
        assert_eq!(search.count_in(&buffer.view()), 2);
    }

    // #309: a cancelled scan stops, keeps what it found, and leaves the rest pending.
    #[test]
    fn a_cancelled_scan_stops_and_leaves_the_rest_pending() {
        let mut buffer = LogBuffer::new(10);
        for t in ["hit", "hit", "hit"] {
            push(&mut buffer, t);
        }
        let mut search = LogSearch::new(regex("hit"));
        search.cancel_handle().cancel();
        search.update(&buffer.view());
        assert_eq!(search.count(), 0);
        assert_eq!(search.pending(&buffer.view()), 3);
        // A clone shares the signal.
        let running = LogSearch::new(regex("hit"));
        let clone = running.clone();
        running.cancel_handle().cancel();
        assert!(clone.cancel_handle().is_cancelled());
    }

    // #309: the next update picks up where a cancelled one stopped.
    #[test]
    fn a_cancelled_scan_resumes_on_the_next_update() {
        let mut buffer = LogBuffer::new(4);
        for t in ["hit", "no", "hit"] {
            push(&mut buffer, t);
        }
        let mut search = LogSearch::new(regex("hit"));
        search.cancel_handle().cancel();
        search.update(&buffer.view());
        assert_eq!(search.pending(&buffer.view()), 3);
        search.update(&buffer.view());
        assert_eq!(search.matches(), &[0, 2]);
        assert_eq!(search.pending(&buffer.view()), 0);

        // Cancelled again, then lines arrive and evict some that were never scanned.
        for t in ["hit", "no", "hit", "hit"] {
            push(&mut buffer, t);
        }
        search.cancel_handle().cancel();
        search.update(&buffer.view());
        assert_eq!(search.count(), 0, "all evicted");
        for t in ["no", "hit"] {
            push(&mut buffer, t);
        }
        search.update(&buffer.view());
        assert_eq!(search.matches(), &[5, 6, 8]);
        assert_eq!(search.pending(&buffer.view()), 0);
    }

    // #309: one cancel stops one scan. A handle taken before a clone is made reaches that clone
    // (how a scan moved to another thread is stopped); neither is left unable to scan.
    #[test]
    fn a_cancel_stops_one_scan_and_leaves_clones_able_to_scan() {
        let mut buffer = LogBuffer::new(10);
        for t in ["hit", "hit"] {
            push(&mut buffer, t);
        }
        let mut original = LogSearch::new(regex("hit"));
        let mut clone = original.clone();
        original.cancel_handle().cancel();
        clone.update(&buffer.view());
        assert_eq!(clone.count(), 0, "the clone's scan was stopped");
        original.update(&buffer.view());
        assert_eq!(original.count(), 2, "the cancel was used up");
        clone.update(&buffer.view());
        assert_eq!(clone.count(), 2);

        // After a scan each has its own signal.
        push(&mut buffer, "hit");
        clone.cancel_handle().cancel();
        original.update(&buffer.view());
        assert_eq!(original.count(), 3);
    }

    // #309: wherever a cancel lands in a running scan, the scans together see each line once.
    #[test]
    fn a_scan_cancelled_while_running_loses_and_repeats_no_line() {
        let mut buffer = LogBuffer::new(20_000);
        for n in 0..20_000 {
            push(&mut buffer, if n % 3 == 0 { "hit" } else { "no" });
        }
        let lines = buffer.view();
        let expected: Vec<LineId> = (0..20_000).filter(|n| n % 3 == 0).collect();
        for _ in 0..8 {
            let mut search = LogSearch::new(regex("hit"));
            let cancel = search.cancel_handle();
            let scan = std::thread::spawn({
                let lines = lines.clone();
                move || {
                    search.update(&lines);
                    search
                }
            });
            cancel.cancel();
            let mut search = scan.join().unwrap();
            search.update(&lines);
            assert_eq!(search.pending(&lines), 0);
            assert!(search.matches().iter().eq(expected.iter()));
        }
    }

    /// A quick measure of highlighting a long, wrapped line: 64 KiB with a match every dozen
    /// bytes, as `render_line` asks for every visible row in every frame. Ignored: run it in
    /// release mode with `--ignored --nocapture`.
    #[test]
    #[ignore = "measurement: run with --release --ignored --nocapture"]
    fn measure_match_ranges_on_a_dense_64k_line() {
        let text = "id=1 ERROR; ".repeat(64 * 1024 / 12);
        for (name, query) in [
            ("ASCII", SearchQuery::new("error")),
            ("non-ASCII", SearchQuery::new("err\u{f6}r")),
        ] {
            let text = if name == "ASCII" {
                text.clone()
            } else {
                text.replace("ERROR", "ERR\u{d6}R")
            };
            let rounds = 50;
            let started = std::time::Instant::now();
            let mut count = 0;
            for _ in 0..rounds {
                count = query.match_ranges(&text).len();
            }
            let now = started.elapsed() / rounds;
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                assert_eq!(reference_ranges(&query, &text).len(), count);
            }
            let reference = started.elapsed() / rounds;
            eprintln!(
                "match_ranges on a {} KiB line, {count} {name} matches: {now:?} \
                 (the reference algorithm: {reference:?})",
                text.len() / 1024
            );
        }
    }

    /// A quick measure of a search's scan: 100k lines of about 100 bytes, for an ASCII query,
    /// a non-ASCII one and a case-sensitive one. Ignored: run it in release mode with
    /// `--ignored --nocapture` to compare changes to the matcher.
    #[test]
    #[ignore = "measurement: run with --release --ignored --nocapture"]
    fn measure_scan_of_100k_lines() {
        let mut buffer = LogBuffer::new(100_000);
        for n in 0..100_000 {
            push(
                &mut buffer,
                &format!(
                    "level=info msg=\"request served\" path=/api/v1/items/{n} status=200 \
                     duration=12ms user=someone@example.com"
                ),
            );
        }
        let lines = buffer.view();
        let mut sensitive = SearchQuery::new("status=500");
        sensitive.case_sensitive = true;
        for query in [
            SearchQuery::new("timeout"),
            SearchQuery::new("größe"),
            sensitive,
        ] {
            let rounds = 20;
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                let mut search = LogSearch::new(query.clone());
                search.update(&lines);
                assert_eq!(search.count(), 0);
            }
            eprintln!(
                "scan of 100k lines for {:?} (case-sensitive: {}): {:?}",
                query.text,
                query.case_sensitive,
                started.elapsed() / rounds
            );
        }
    }
}
