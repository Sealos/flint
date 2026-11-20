//! The command palette's matching, apart from drawing: fuzzy scoring of a typed query against
//! labels, the order the matches are listed in (recently run actions first), and the recent
//! actions themselves. The palette runs this when the query changes, never while it draws.

use std::collections::VecDeque;
use std::ops::Range;

/// Points for each query character matched.
const MATCH: i32 = 16;
/// Extra points for a character that starts a word ("L" in "Open Logs", "C" in "SortBy").
const WORD_START: i32 = 24;
/// Extra points for a character right after the previous match.
const CONSECUTIVE: i32 = 20;
/// Points lost per character skipped between two matches, and before the first.
const GAP: i32 = 1;
/// Points lost when the query matched only the second haystack (the category and the label).
const ALT_PENALTY: i32 = 24;
/// How many actions the palette remembers as recently run.
pub const RECENTS: usize = 16;

/// How well a query matched a text, and where: the byte ranges of the matched characters, for
/// highlighting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Score {
    pub points: i32,
    pub ranges: Vec<Range<usize>>,
}

/// Matches `query` against `text` as a subsequence, ignoring case and the query's spaces:
/// "opl" matches "Open Logs". `None` when it does not match. Prefers characters that start
/// words and runs of consecutive characters, so "ol" finds the "O" and "L" of "Open Logs".
pub fn fuzzy(query: &str, text: &str) -> Option<Score> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if query.is_empty() {
        return Some(Score {
            points: 0,
            ranges: Vec::new(),
        });
    }
    // Most candidates do not match at all: a cheap pass rules them out before the scoring.
    if !is_subsequence(&query, text) {
        return None;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let word_start = |k: usize| {
        let c = chars[k].1;
        match k.checked_sub(1).map(|p| chars[p].1) {
            None => true,
            Some(prev) => {
                !prev.is_alphanumeric()
                    || (prev.is_lowercase() && c.is_uppercase())
                    || (prev.is_alphabetic() != c.is_alphabetic())
            }
        }
    };
    // The text in lower case, as the query is: a character may lower to several ("İ" to "i̇"),
    // each remembering the character it came from (its index in `chars`).
    let mut lower: Vec<char> = Vec::with_capacity(chars.len());
    let mut origin: Vec<usize> = Vec::with_capacity(chars.len());
    let mut starts: Vec<bool> = Vec::with_capacity(chars.len());
    for (k, &(_, c)) in chars.iter().enumerate() {
        for (i, l) in c.to_lowercase().enumerate() {
            lower.push(l);
            origin.push(k);
            starts.push(i == 0 && word_start(k));
        }
    }
    let n = lower.len();
    let m = query.len();
    if m > n {
        return None;
    }
    let bonus = |j: usize| MATCH + if starts[j] { WORD_START } else { 0 };

    // best[i][j]: the best score matching query[..=i] with query[i] at text position j, and
    // where query[i - 1] was matched (for reading the positions back).
    const NONE: i32 = i32::MIN / 2;
    let mut best = vec![vec![NONE; n]; m];
    let mut from = vec![vec![usize::MAX; n]; m];
    for j in 0..n {
        if lower[j] == query[0] {
            best[0][j] = bonus(j) - GAP * j as i32;
        }
    }
    for i in 1..m {
        // The best of best[i - 1][k] + GAP * k over k < j - 1 (a gap before j), as j moves on.
        let mut gapped: (i32, usize) = (NONE, usize::MAX);
        for j in i..n {
            if j >= 2 {
                let k = j - 2;
                if best[i - 1][k] > NONE {
                    let value = best[i - 1][k] + GAP * k as i32;
                    if value > gapped.0 {
                        gapped = (value, k);
                    }
                }
            }
            if lower[j] != query[i] {
                continue;
            }
            let mut score = NONE;
            let mut parent = usize::MAX;
            if best[i - 1][j - 1] > NONE {
                score = best[i - 1][j - 1] + CONSECUTIVE;
                parent = j - 1;
            }
            if gapped.0 > NONE {
                let value = gapped.0 - GAP * (j as i32 - 1);
                if value > score {
                    score = value;
                    parent = gapped.1;
                }
            }
            if parent != usize::MAX {
                best[i][j] = score + bonus(j);
                from[i][j] = parent;
            }
        }
    }
    let (mut j, points) = (0..n)
        .filter(|&j| best[m - 1][j] > NONE)
        .map(|j| (j, best[m - 1][j]))
        .max_by_key(|&(j, points)| (points, std::cmp::Reverse(j)))?;
    let mut positions = vec![j; m];
    for i in (1..m).rev() {
        j = from[i][j];
        positions[i - 1] = j;
    }
    Some(Score {
        points,
        ranges: merge(positions.iter().map(|&p| {
            let (at, c) = chars[origin[p]];
            at..at + c.len_utf8()
        })),
    })
}

/// Whether the lowercase `query` appears in order in `text`, ignoring case. Byte by byte for
/// ASCII, the usual case (names, labels).
fn is_subsequence(query: &[char], text: &str) -> bool {
    if text.is_ascii() && query.iter().all(char::is_ascii) {
        let mut wanted = query.iter().map(|&c| c as u8);
        let mut next = wanted.next();
        for b in text.bytes() {
            match next {
                None => return true,
                Some(w) if b.to_ascii_lowercase() == w => next = wanted.next(),
                Some(_) => {}
            }
        }
        return next.is_none();
    }
    let mut wanted = query.iter().peekable();
    for c in text.chars().flat_map(char::to_lowercase) {
        if wanted.peek() == Some(&&c) {
            wanted.next();
        }
    }
    wanted.peek().is_none()
}

/// Joins touching byte ranges ("O", "p" into "Op").
fn merge(ranges: impl Iterator<Item = Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            // Two lowered characters of one character ("İ") give its range twice.
            Some(last) if last.end == range.end => {}
            Some(last) if last.end == range.start => last.end = range.end,
            _ => merged.push(range),
        }
    }
    merged
}

/// Something the palette can list: its label, and optionally a second text it also matches
/// (the category before the label, or a context's full name), with less weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub alt: Option<String>,
    /// Its place among the recently run actions, 0 for the latest; `None` if it is not one.
    pub recent: Option<usize>,
}

/// A listed candidate: its index in the candidates, and the label's matched bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub index: usize,
    pub ranges: Vec<Range<usize>>,
}

/// The candidates matching `query`, best first. With no query: the recent ones (latest first),
/// then the rest in their own order. With one: by score, so a closer match always wins; among
/// equal scores the recent ones first (latest first), then in their own order.
pub fn rank(query: &str, candidates: &[Candidate]) -> Vec<Hit> {
    if query.trim().is_empty() {
        let mut hits: Vec<(usize, &Candidate)> = candidates.iter().enumerate().collect();
        hits.sort_by_key(|(index, c)| (c.recent.unwrap_or(usize::MAX), *index));
        return hits
            .into_iter()
            .map(|(index, _)| Hit {
                index,
                ranges: Vec::new(),
            })
            .collect();
    }
    let mut scored: Vec<(i32, usize, Hit)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, c)| {
            let (points, ranges) = match fuzzy(query, &c.label) {
                Some(score) => (score.points, score.ranges),
                None => {
                    let alt = fuzzy(query, c.alt.as_deref()?)?;
                    (alt.points - ALT_PENALTY, Vec::new())
                }
            };
            Some((
                points,
                c.recent.unwrap_or(usize::MAX),
                Hit { index, ranges },
            ))
        })
        .collect();
    scored.sort_by_key(|(points, recent, hit)| (std::cmp::Reverse(*points), *recent, hit.index));
    scored.into_iter().map(|(_, _, hit)| hit).collect()
}

/// The actions run from the palette, latest first, by name; at most [`RECENTS`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Recents(VecDeque<String>);

impl Recents {
    /// Notes that `name` ran now.
    pub fn used(&mut self, name: &str) {
        self.0.retain(|n| n != name);
        self.0.push_front(name.to_owned());
        self.0.truncate(RECENTS);
    }

    /// Where `name` is among the recent actions, 0 for the latest.
    pub fn rank(&self, name: &str) -> Option<usize> {
        self.0.iter().position(|n| n == name)
    }
}

/// Moves a selection of `count` rows by `step`, wrapping around at either end.
pub fn step(selected: usize, count: usize, step: isize) -> usize {
    if count == 0 {
        return 0;
    }
    (selected as isize + step).rem_euclid(count as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::{Candidate, Recents, fuzzy, rank, step};

    fn candidates(labels: &[&str]) -> Vec<Candidate> {
        labels
            .iter()
            .map(|l| Candidate {
                label: (*l).to_owned(),
                alt: None,
                recent: None,
            })
            .collect()
    }

    fn labels(query: &str, candidates: &[Candidate]) -> Vec<String> {
        rank(query, candidates)
            .into_iter()
            .map(|h| candidates[h.index].label.clone())
            .collect()
    }

    #[test]
    fn matches_a_subsequence_ignoring_case_and_spaces() {
        assert!(fuzzy("opl", "Open Logs").is_some());
        assert!(fuzzy("OPEN logs", "Open Logs").is_some());
        assert!(fuzzy("lo", "Open Logs").is_some());
        assert!(fuzzy("xyz", "Open Logs").is_none());
        assert!(fuzzy("sgol", "Open Logs").is_none());
        assert!(fuzzy("a much longer query", "Open").is_none());
        assert_eq!(fuzzy("", "Open").unwrap().points, 0);
    }

    #[test]
    fn highlights_the_matched_bytes_preferring_word_starts() {
        // "ol": the O of Open and the L of Logs, not the l in "Toggle".
        assert_eq!(fuzzy("ol", "Open Logs").unwrap().ranges, [0..1, 5..6]);
        assert_eq!(fuzzy("logs", "Open Logs").unwrap().ranges, vec![5..9_usize]);
        // Camel case starts a word too.
        assert_eq!(fuzzy("sb", "SortBy").unwrap().ranges, [0..1, 4..5]);
        // Multi-byte characters keep their whole width.
        assert_eq!(fuzzy("é", "Café").unwrap().ranges, vec![3..5_usize]);
    }

    // Case folds as Unicode lower case says, on both sides: "İ" lowers to two characters.
    #[test]
    fn unicode_case_folds_on_both_sides() {
        let score = fuzzy("İs", "İstanbul").expect("matches");
        assert_eq!(score.ranges, vec![0..3_usize]);
        assert!(fuzzy("is", "İstanbul").is_some());
        assert!(fuzzy("ÉCOLE", "école").is_some());
        assert!(
            fuzzy("straße", "STRASSE").is_none(),
            "lower case only, no full folding"
        );
    }

    #[test]
    fn word_starts_and_runs_score_higher() {
        let starts = fuzzy("sc", "Select Cluster…").unwrap().points;
        let middle = fuzzy("sc", "Describe").unwrap().points;
        assert!(starts > middle, "{starts} <= {middle}");
        let run = fuzzy("log", "Open Logs").unwrap().points;
        let spread = fuzzy("log", "Blowing").unwrap().points;
        assert!(run > spread, "{run} <= {spread}");
    }

    #[test]
    fn ranks_by_score_then_registry_order() {
        let c = candidates(&[
            "Toggle Labels",
            "Open Logs",
            "Open Previous Logs",
            "Open Logs by Label Selector…",
        ]);
        assert_eq!(
            labels("open logs", &c),
            [
                "Open Logs",
                "Open Logs by Label Selector…",
                "Open Previous Logs"
            ]
        );
        assert_eq!(
            labels("", &c),
            c.iter().map(|c| c.label.clone()).collect::<Vec<_>>()
        );
        assert!(labels("zzz", &c).is_empty());
    }

    #[test]
    fn matches_the_second_text_with_less_weight() {
        let c = vec![
            Candidate {
                label: "Toggle Pause".into(),
                alt: Some("Logs Toggle Pause".into()),
                recent: None,
            },
            Candidate {
                label: "Open Logs".into(),
                alt: Some("Logs Open Logs".into()),
                recent: None,
            },
        ];
        assert_eq!(labels("logs", &c), ["Open Logs", "Toggle Pause"]);
        // A match in the second text only highlights nothing in the label.
        assert!(rank("logs pause", &c)[0].ranges.is_empty());
    }

    // covers UP-30: recently run actions come first.
    #[test]
    fn recent_actions_rank_first() {
        let mut recents = Recents::default();
        recents.used("b");
        recents.used("c");
        recents.used("b");
        assert_eq!(recents.rank("b"), Some(0));
        assert_eq!(recents.rank("c"), Some(1));
        assert_eq!(recents.rank("a"), None);

        let mut c = candidates(&["Open Logs", "Open Shell", "Open Kind…"]);
        c[2].recent = Some(0);
        c[1].recent = Some(1);
        assert_eq!(labels("", &c), ["Open Kind…", "Open Shell", "Open Logs"]);
        // With a query, among equal matches.
        assert_eq!(
            labels("open", &c),
            ["Open Kind…", "Open Shell", "Open Logs"]
        );
        // A recent action that does not match is not listed.
        assert_eq!(labels("logs", &c), ["Open Logs"]);

        // A closer match wins over a recent one: "op" is the start of Open Logs, but only two
        // letters scattered in Toggle Pause.
        let mut c = candidates(&["Toggle Pause", "Open Logs"]);
        c[0].recent = Some(0);
        assert_eq!(labels("op", &c), ["Open Logs", "Toggle Pause"]);
        assert_eq!(labels("", &c), ["Toggle Pause", "Open Logs"]);
    }

    #[test]
    fn remembers_a_bounded_number_of_recents() {
        let mut recents = Recents::default();
        for i in 0..super::RECENTS + 4 {
            recents.used(&i.to_string());
        }
        assert_eq!(recents.rank("0"), None);
        assert_eq!(recents.rank(&(super::RECENTS + 3).to_string()), Some(0));
    }

    #[test]
    fn the_selection_wraps_around() {
        assert_eq!(step(0, 3, -1), 2);
        assert_eq!(step(2, 3, 1), 0);
        assert_eq!(step(1, 3, 1), 2);
        assert_eq!(step(0, 0, 1), 0);
    }

    // The palette ranks on each keystroke. Typing "kube-system" letter by letter over 5,000
    // namespaces takes well under a frame here (about 8 ms in this unoptimized test build).
    // The limit is 50 ms so a shared CI runner does not fail it, while a quadratic slowdown
    // still would.
    #[test]
    fn ranking_thousands_of_namespaces_fits_in_a_frame() {
        let c: Vec<Candidate> = (0..5000)
            .map(|i| Candidate {
                label: format!("team-{i}-payments-service-{}", i % 7),
                alt: None,
                recent: None,
            })
            .chain(std::iter::once(Candidate {
                label: "kube-system".into(),
                alt: None,
                recent: None,
            }))
            .collect();
        let query = "kube-system";
        let mut slowest = std::time::Duration::ZERO;
        for end in 1..=query.len() {
            // The best of three, so a busy test machine's pauses do not count.
            let spent = (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    assert!(!rank(&query[..end], &c).is_empty());
                    start.elapsed()
                })
                .min()
                .unwrap();
            slowest = slowest.max(spent);
        }
        assert_eq!(rank(query, &c).len(), 1);
        assert!(
            slowest < std::time::Duration::from_millis(50),
            "{slowest:?}"
        );
    }
}
