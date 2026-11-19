//! Search over a session's lines that keeps matching new lines while the session follows.
//!
//! A [`LogSearch`] remembers how far it has scanned. Each [`LogSearch::update`] with a newer
//! snapshot scans only the lines that arrived since and forgets matches that were evicted, so
//! following a chatty pod with a search open costs time proportional to the new lines, not to
//! the buffer. There is no index: the query changes with every keystroke, and a full scan of a
//! 100k-line buffer takes milliseconds, while an n-gram index would cost about as much memory as
//! the buffer itself. Run the first scan of a new query off the UI thread
//! ([`crate::CoreHandle::search_logs`]); incremental updates are cheap anywhere.

use std::collections::VecDeque;
use std::ops::Range;

use super::buffer::LogLines;
use super::line::LineId;

/// What to look for: a plain substring of a line's text (timestamps are not searched).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    pub text: String,
    /// Off by default: `error` finds `ERROR`.
    pub case_sensitive: bool,
}

impl SearchQuery {
    pub fn new(text: impl Into<String>) -> Self {
        SearchQuery {
            text: text.into(),
            case_sensitive: false,
        }
    }

    /// Byte ranges of every non-overlapping match in `text`, for highlighting. Empty for an
    /// empty query.
    pub fn match_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let needle = self.text.as_str();
        if needle.is_empty() {
            return Vec::new();
        }
        if self.case_sensitive {
            return text
                .match_indices(needle)
                .map(|(at, m)| at..at + m.len())
                .collect();
        }
        if needle.is_ascii() {
            // Lowercasing ASCII keeps byte offsets, so ranges map back onto `text`.
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
        // Non-ASCII queries: compare char by char with simple lowercase folding, so offsets
        // stay those of `text`.
        let pat: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if let Some(len) = folded_prefix_len(&chars[i..], &pat) {
                let start = chars[i].0;
                let end = chars.get(i + len).map_or(text.len(), |(at, _)| *at);
                out.push(start..end);
                i += len.max(1);
            } else {
                i += 1;
            }
        }
        out
    }

    /// Whether `text` contains the query. An empty query matches nothing.
    pub fn is_match(&self, text: &str) -> bool {
        let needle = self.text.as_str();
        if needle.is_empty() {
            false
        } else if self.case_sensitive {
            text.contains(needle)
        } else if needle.is_ascii() {
            let (hay, pat) = (text.as_bytes(), needle.as_bytes());
            hay.windows(pat.len()).any(|w| w.eq_ignore_ascii_case(pat))
        } else {
            !self.match_ranges(text).is_empty()
        }
    }
}

/// How many chars of `chars` match `pat` (already lowercased), if they all do.
fn folded_prefix_len(chars: &[(usize, char)], pat: &[char]) -> Option<usize> {
    let mut p = 0;
    let mut used = 0;
    for (_, c) in chars {
        if p == pat.len() {
            break;
        }
        for lc in c.to_lowercase() {
            if pat.get(p) != Some(&lc) {
                return None;
            }
            p += 1;
        }
        used += 1;
    }
    (p == pat.len()).then_some(used)
}

/// The lines of one session that match a query, kept up to date snapshot by snapshot.
#[derive(Debug, Clone, Default)]
pub struct LogSearch {
    query: SearchQuery,
    /// Matching line ids, ascending.
    matches: VecDeque<LineId>,
    /// Lines below this id have been scanned.
    scanned_to: LineId,
}

impl LogSearch {
    pub fn new(query: SearchQuery) -> Self {
        LogSearch {
            query,
            matches: VecDeque::new(),
            scanned_to: 0,
        }
    }

    pub fn query(&self) -> &SearchQuery {
        &self.query
    }

    /// Catches up with `lines`: drops matches that were evicted and scans lines not seen yet.
    /// Pass snapshots of one session only, in order.
    pub fn update(&mut self, lines: &LogLines) {
        let first = lines.first_id();
        let drop = self.matches.partition_point(|id| *id < first);
        self.matches.drain(..drop);
        let from = self.scanned_to.max(first);
        if !self.query.text.is_empty() {
            self.matches.extend(
                lines
                    .range(from, lines.end_id())
                    .filter(|(_, line)| self.query.is_match(&line.text))
                    .map(|(id, _)| id),
            );
        }
        self.scanned_to = self.scanned_to.max(lines.end_id());
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
}
