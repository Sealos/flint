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
use super::pattern::{self, GUARD_SCAN_BYTES, GUARD_SCAN_LINES, PatternError, ScanCancel};

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

    /// Whether `text` contains the query. An empty query matches nothing.
    pub fn is_match(&self, text: &str) -> bool {
        self.matcher().is_match(text)
    }
}

/// A [`SearchQuery`] made ready to test many lines: the pattern is prepared once, and testing a
/// line allocates nothing (the lowercased text goes into a buffer kept from line to line).
///
/// - **Case-sensitive**, or ASCII with no letters: a substring search ([`memmem`]).
/// - **ASCII, either case**: the same search in the text with its ASCII lowercased, which keeps
///   byte offsets.
/// - **Other text, either case**: a substring search in the text lowercased char by char rules
///   out most lines; where it finds the pattern, chars are compared with simple lowercase
///   folding at each char boundary, so offsets stay those of the text.
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
    /// The pattern lowercased, as text and as chars.
    Folded {
        lower: memmem::Finder<'static>,
        chars: Vec<char>,
    },
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
            let chars: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
            let lower: String = chars.iter().collect();
            Find::Folded {
                lower: memmem::Finder::new(&lower).into_owned(),
                chars,
            }
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
            Find::Folded { chars, .. } => {
                self.might_fold_match(text) && folded_matches(text, chars).next().is_some()
            }
            Find::Regex(regex) => regex.is_match(text),
        }
    }

    /// Byte ranges of every non-overlapping match in `text`. The text is lowercased once,
    /// however many matches it has: a long wrapped line is highlighted in every frame.
    pub fn match_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let ranges = |finder: &memmem::Finder<'_>, hay: &[u8]| {
            let len = finder.needle().len();
            finder.find_iter(hay).map(|at| at..at + len).collect()
        };
        match &self.find {
            Find::Nothing => Vec::new(),
            Find::Exact(finder) => ranges(finder, text.as_bytes()),
            // Lowercasing ASCII keeps byte offsets, so ranges map back onto `text`.
            Find::AsciiFolded(finder) => ranges(finder, &self.lowered(text)),
            Find::Folded { chars, .. } => {
                if !self.might_fold_match(text) {
                    return Vec::new();
                }
                folded_matches(text, chars).collect()
            }
            Find::Regex(regex) => pattern::bounded_ranges(regex, text),
        }
    }

    /// `text` with its ASCII lowercased, in the buffer kept from line to line.
    fn lowered(&self, text: &str) -> std::cell::RefMut<'_, Vec<u8>> {
        let mut lower = self.scratch.borrow_mut();
        lower.clear();
        lower.extend(text.bytes().map(|b| b.to_ascii_lowercase()));
        lower
    }

    /// Whether `text`, lowercased char by char, holds the lowercased pattern: a match at a
    /// char boundary is also one there, so `false` rules a line out without comparing chars.
    fn might_fold_match(&self, text: &str) -> bool {
        let Find::Folded { lower, .. } = &self.find else {
            return true;
        };
        let mut text_lower = self.scratch.borrow_mut();
        text_lower.clear();
        let (bytes, mut i, mut utf8) = (text.as_bytes(), 0, [0; 4]);
        while let Some(&b) = bytes.get(i) {
            // Log text is mostly ASCII: only other chars are decoded.
            if b.is_ascii() {
                text_lower.push(b.to_ascii_lowercase());
                i += 1;
                continue;
            }
            let c = text[i..].chars().next().unwrap_or_default();
            for lc in c.to_lowercase() {
                text_lower.extend_from_slice(lc.encode_utf8(&mut utf8).as_bytes());
            }
            i += c.len_utf8();
        }
        lower.find(&text_lower).is_some()
    }
}

/// The non-overlapping matches of `pat` (lowercased chars) in `text`, comparing chars with
/// simple lowercase folding at each char boundary, as byte ranges of `text`.
fn folded_matches<'a>(text: &'a str, pat: &'a [char]) -> impl Iterator<Item = Range<usize>> + 'a {
    let mut at = 0;
    std::iter::from_fn(move || {
        while at < text.len() {
            let rest = &text[at..];
            if let Some(len) = folded_prefix_len(rest, pat) {
                let found = at..at + len;
                // A pattern is never empty, so a match moves on by at least one char.
                at += len.max(rest.chars().next().map_or(1, char::len_utf8));
                return Some(found);
            }
            at += rest.chars().next().map_or(1, char::len_utf8);
        }
        None
    })
}

/// How many bytes at the start of `text` match `pat` (already lowercased), if they all do.
fn folded_prefix_len(text: &str, pat: &[char]) -> Option<usize> {
    let mut p = 0;
    for (at, c) in text.char_indices() {
        if p == pat.len() {
            return Some(at);
        }
        for lc in c.to_lowercase() {
            if pat.get(p) != Some(&lc) {
                return None;
            }
            p += 1;
        }
    }
    (p == pat.len()).then_some(text.len())
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

    /// The lines not scanned yet that a guard tests. All of them for a plain query; for a regex,
    /// a window of the newest, at most [`GUARD_SCAN_LINES`] lines and about [`GUARD_SCAN_BYTES`]
    /// bytes counted from the newest line back, since guards and state polls run on the UI
    /// thread.
    fn unscanned<'a>(&self, lines: &'a LogLines) -> impl Iterator<Item = &'a LogLine> + 'a {
        let end = lines.end_id();
        let mut from = self.scanned_to.max(lines.first_id());
        if self.query.regex {
            let oldest = from.max(end.saturating_sub(GUARD_SCAN_LINES as LineId));
            let (mut at, mut budget) = (end, GUARD_SCAN_BYTES);
            while at > oldest && budget > 0 {
                at -= 1;
                budget = budget.saturating_sub(lines.get(at).map_or(0, |line| line.text.len()));
            }
            from = at;
        }
        lines.range(from, end).map(|(_, line)| line)
    }

    /// How many lines of `lines` the next [`LogSearch::update`] would scan: a front end that
    /// put off scanning (for a hidden tab) can tell a short catch-up from a long one.
    pub fn pending(&self, lines: &LogLines) -> u64 {
        lines
            .end_id()
            .saturating_sub(self.scanned_to.max(lines.first_id()))
    }

    /// Whether a line of `lines` matches, counting lines not scanned yet (tested here, without
    /// keeping the result): what an action's guard asks between two updates, which must
    /// neither count evicted matches nor miss new ones. A regex query tests a bounded part of
    /// the lines not scanned yet ([`LogSearch::update`] covers them all).
    pub fn has_match_in(&self, lines: &LogLines) -> bool {
        let (first, end) = (lines.first_id(), lines.end_id());
        let present = self.matches.partition_point(|id| *id < first);
        self.matches.get(present).is_some_and(|id| *id < end)
            || self
                .unscanned(lines)
                .any(|line| self.matcher.matches_line(line))
    }

    /// How many lines of `lines` match, counting lines not scanned yet and not evicted matches,
    /// as [`LogSearch::has_match_in`] sees them (for a regex query, a lower bound until the
    /// next update).
    pub fn count_in(&self, lines: &LogLines) -> usize {
        let first = lines.first_id();
        let present = self.matches.partition_point(|id| *id < first);
        self.matches.len() - present
            + self
                .unscanned(lines)
                .filter(|line| self.matcher.matches_line(line))
                .count()
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

    // covers UP-43: between updates, whether a line matches counts neither evicted matches
    // nor misses new ones.
    #[test]
    fn has_match_in_sees_new_lines_and_forgets_evicted_ones() {
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
            search.has_match_in(&buffer.view()),
            "a line not scanned yet matches"
        );
        assert_eq!(search.count_in(&buffer.view()), 1);
        push(&mut buffer, "error c");
        assert_eq!(search.count_in(&buffer.view()), 2);
        search.update(&buffer.view());
        assert_eq!(search.count_in(&buffer.view()), search.count());
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

    // #309
    #[test]
    fn a_regex_query_matches_highlights_and_follows_case() {
        let q = regex(r"status=5\d\d");
        assert!(q.is_match("STATUS=503"));
        assert!(!q.is_match("status=200"));
        assert_eq!(q.match_ranges("x status=500 status=501"), [2..12, 13..23]);
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

    // #309: a guard between scans tests a bounded part of what was not scanned for a regex,
    // and all of it for a plain query, as before.
    #[test]
    fn guards_test_a_bounded_part_of_the_unscanned_lines_for_a_regex() {
        let mut buffer = LogBuffer::new(GUARD_SCAN_LINES * 3);
        push(&mut buffer, "needle");
        for _ in 0..GUARD_SCAN_LINES * 2 {
            push(&mut buffer, "hay");
        }
        let lines = buffer.view();
        let bounded = LogSearch::new(regex("needle"));
        assert!(!bounded.has_match_in(&lines), "too old for a guard to test");
        assert_eq!(bounded.count_in(&lines), 0);
        let plain = LogSearch::new(SearchQuery::new("needle"));
        assert!(plain.has_match_in(&lines));
        assert_eq!(plain.count_in(&lines), 1);
        // The scan itself covers every line.
        let mut scanned = bounded.clone();
        scanned.update(&lines);
        assert_eq!(scanned.count(), 1);

        // Long lines end the guard's test by bytes, well before the line limit.
        let mut long = LogBuffer::new(100);
        let text = "x".repeat(GUARD_SCAN_BYTES);
        push(&mut long, "needle");
        push(&mut long, &text);
        assert!(!LogSearch::new(regex("needle")).has_match_in(&long.view()));
    }

    // #309: the byte budget is spent from the newest line back, so a burst of old long lines
    // cannot hide the line that just arrived.
    #[test]
    fn a_guard_spends_its_bytes_on_the_newest_lines() {
        let mut buffer = LogBuffer::new(100);
        let long = "x".repeat(GUARD_SCAN_BYTES);
        push(&mut buffer, &long);
        push(&mut buffer, &long);
        push(&mut buffer, "needle");
        let search = LogSearch::new(regex("needle"));
        assert!(search.has_match_in(&buffer.view()));
        assert_eq!(search.count_in(&buffer.view()), 1);
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
