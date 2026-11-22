//! The bounds on a regex log search (#309): what a pattern may compile to, how much text a
//! guard or a highlight may test, and how a scan is stopped.
//!
//! The `regex` crate matches in time linear in the text, whatever the pattern, so no pattern can
//! backtrack without end. What is left to bound is the pattern's compiled size (here), the text
//! tested on the UI thread ([`GUARD_SCAN_LINES`], [`GUARD_SCAN_BYTES`], [`bounded_ranges`]), and a
//! scan nobody wants any more ([`ScanCancel`]).

use std::cell::RefCell;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};

/// The longest pattern compiled.
pub const MAX_PATTERN_BYTES: usize = 1024;
/// The most memory a compiled pattern, and each of its lazy DFA caches, may use.
const COMPILED_SIZE_LIMIT: usize = 1 << 20;
/// How deeply groups and repetitions may nest.
const NEST_LIMIT: u32 = 64;
/// Compiled patterns kept per thread: highlighting asks for one for every row of every frame.
const CACHED_PATTERNS: usize = 4;

/// Most lines not scanned yet that a guard or a state poll tests for a regex query.
pub const GUARD_SCAN_LINES: usize = 2_000;
/// Most bytes of such lines tested.
pub const GUARD_SCAN_BYTES: usize = 256 * 1024;

/// How much of a line is highlighted for a regex query.
const HIGHLIGHT_BYTES: usize = 4 * 1024;
/// How far past [`HIGHLIGHT_BYTES`] the matcher reads, so that a match near the cut is decided
/// by the real line. A match that would only be told apart further on than this can still be
/// shortened: the bound on the text searched comes first.
const HIGHLIGHT_LOOKAHEAD: usize = 256;
/// How long one line's highlighting may take. Iterating matches can be quadratic in the text
/// even with a linear engine, and how far the engine read for a match cannot be seen from its
/// result, so the time spent is what is measured. Far above what an ordinary pattern needs.
const HIGHLIGHT_TIME: Duration = Duration::from_millis(2);
/// Most matches highlighted in one line.
const HIGHLIGHT_MATCHES: usize = 256;

/// Why a pattern cannot be used, in words for the search field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    /// Longer than [`MAX_PATTERN_BYTES`].
    TooLong,
    /// Compiles to more than the size limit.
    TooBig,
    /// Not a valid pattern: what the parser said.
    Invalid(String),
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternError::TooLong => {
                write!(f, "Pattern is longer than {MAX_PATTERN_BYTES} bytes")
            }
            PatternError::TooBig => f.write_str("Pattern is too complex"),
            PatternError::Invalid(reason) => write!(f, "Invalid pattern: {reason}"),
        }
    }
}

impl std::error::Error for PatternError {}

type Compiled = Result<Regex, PatternError>;

thread_local! {
    /// The patterns compiled last on this thread, newest last.
    static CACHE: RefCell<Vec<(String, bool, Compiled)>> = const { RefCell::new(Vec::new()) };
}

/// `pattern` compiled within the limits, from this thread's cache when it was compiled lately.
pub(super) fn compile(pattern: &str, case_sensitive: bool) -> Compiled {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(PatternError::TooLong);
    }
    CACHE.with_borrow_mut(|cache| {
        if let Some(at) = cache
            .iter()
            .position(|(p, case, _)| *case == case_sensitive && p == pattern)
        {
            let entry = cache.remove(at);
            let compiled = entry.2.clone();
            cache.push(entry);
            return compiled;
        }
        let compiled = build(pattern, case_sensitive);
        if cache.len() == CACHED_PATTERNS {
            drop(cache.remove(0));
        }
        cache.push((pattern.to_owned(), case_sensitive, compiled.clone()));
        compiled
    })
}

fn build(pattern: &str, case_sensitive: bool) -> Compiled {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(COMPILED_SIZE_LIMIT)
        .dfa_size_limit(COMPILED_SIZE_LIMIT)
        .nest_limit(NEST_LIMIT)
        .build()
        .map_err(|e| match e {
            regex::Error::CompiledTooBig(_) => PatternError::TooBig,
            regex::Error::Syntax(text) => PatternError::Invalid(syntax_reason(&text)),
            other => PatternError::Invalid(other.to_string()),
        })
}

/// The parser's reason, without the copy of the pattern and the caret it draws above it.
fn syntax_reason(text: &str) -> String {
    text.lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("error: "))
        .unwrap_or_else(|| text.trim())
        .to_owned()
}

/// The non-empty matches of `regex` in `text` to highlight, within the limits above. The matcher
/// reads a little past the cut, so `$`, `\b` or a longer alternative see the real line there; a
/// match that crosses the cut is left out.
pub(super) fn bounded_ranges(regex: &Regex, text: &str) -> Vec<Range<usize>> {
    ranges_within(regex, text, HIGHLIGHT_TIME)
}

fn ranges_within(regex: &Regex, text: &str, time: Duration) -> Vec<Range<usize>> {
    let cut = char_floor(text, HIGHLIGHT_BYTES);
    let hay = &text[..char_floor(text, HIGHLIGHT_BYTES + HIGHLIGHT_LOOKAHEAD)];
    let started = Instant::now();
    let mut out = Vec::new();
    for found in regex.find_iter(hay) {
        if found.end() > cut {
            break;
        }
        if !found.range().is_empty() {
            out.push(found.range());
        }
        if out.len() == HIGHLIGHT_MATCHES || started.elapsed() >= time {
            break;
        }
    }
    out
}

/// The largest char boundary of `text` at or below `at`.
fn char_floor(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Stops a scan running on the core's threads: the scan ends at the next line, having scanned
/// the lines before it, and the next update resumes from there.
///
/// A handle reaches the search or filter it came from and any clone made before their next scan,
/// which is how a scan moved to another thread is stopped. One cancel stops one scan, the first
/// to see it. Each scan then leaves its search or filter with a new signal, so a handle covers
/// one scan: take a new one for the next.
#[derive(Debug, Clone, Default)]
pub struct ScanCancel(Arc<AtomicBool>);

impl ScanCancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether a cancel is waiting for a scan to see it.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Whether to stop here. Uses the cancel up, so that a clone sharing it can still scan.
    pub(super) fn claim(&self) -> bool {
        self.is_cancelled() && self.0.swap(false, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A limit no test reaches, for tests about what is matched and not about time.
    const UNHURRIED: Duration = Duration::from_secs(60);

    #[test]
    fn errors_say_what_is_wrong_in_one_line() {
        let error = compile("(unclosed", true).unwrap_err();
        assert_eq!(error.to_string(), "Invalid pattern: unclosed group");
        let error = compile(r"(a)\1", true).unwrap_err();
        assert!(
            error.to_string().starts_with("Invalid pattern: "),
            "{error}"
        );
        assert!(!error.to_string().contains('\n'), "{error}");
        assert_eq!(
            compile(&"a".repeat(MAX_PATTERN_BYTES + 1), true).unwrap_err(),
            PatternError::TooLong
        );
        assert_eq!(
            compile(r"\w{1000}{1000}", false).unwrap_err(),
            PatternError::TooBig
        );
        let nested = format!("{}a{}", "(".repeat(100), ")".repeat(100));
        assert!(matches!(
            compile(&nested, true),
            Err(PatternError::Invalid(_))
        ));
    }

    #[test]
    fn the_cache_keeps_case_apart_and_stays_bounded() {
        assert!(compile("needle", true).unwrap().find("NEEDLE").is_none());
        assert!(compile("needle", false).unwrap().find("NEEDLE").is_some());
        for n in 0..CACHED_PATTERNS * 3 {
            compile(&format!("p{n}"), true).unwrap();
        }
        CACHE.with_borrow(|cache| assert_eq!(cache.len(), CACHED_PATTERNS));
    }

    #[test]
    fn an_oversize_pattern_is_refused_without_being_cached() {
        let long = "a".repeat(MAX_PATTERN_BYTES + 1);
        assert_eq!(compile(&long, true).unwrap_err(), PatternError::TooLong);
        CACHE.with_borrow(|cache| {
            assert!(cache.iter().all(|(p, ..)| p.len() <= MAX_PATTERN_BYTES));
        });
        assert!(compile(&"a".repeat(MAX_PATTERN_BYTES), true).is_ok());
    }

    // Empty matches cost next to nothing, so they must not use up the line's budget.
    #[test]
    fn empty_matches_do_not_starve_a_later_real_match() {
        let regex = compile("x*", true).unwrap();
        let text = format!("{}xx{}", "a".repeat(1000), "a".repeat(HIGHLIGHT_BYTES));
        let expected = 1000..1002;
        assert_eq!(
            ranges_within(&regex, &text, UNHURRIED),
            std::slice::from_ref(&expected)
        );
    }

    // However cheap each match looks, the time limit ends the line.
    #[test]
    fn highlighting_stops_when_its_time_is_up() {
        let regex = compile("a", true).unwrap();
        let found = ranges_within(&regex, &"a".repeat(100), Duration::ZERO);
        assert_eq!(found, std::slice::from_ref(&(0..1)));
    }

    // What the real line holds decides a match near the cut.
    #[test]
    fn a_match_that_continues_past_the_cut_is_left_out() {
        let regex = compile("abc|a", true).unwrap();
        let text = format!("{}abc{}", "-".repeat(HIGHLIGHT_BYTES - 2), "-".repeat(10));
        assert!(ranges_within(&regex, &text, UNHURRIED).is_empty());
        // A match that really ends at the cut is kept: the next character shows it.
        let regex = compile(r"a\b", true).unwrap();
        let text = format!("{}a {}", "-".repeat(HIGHLIGHT_BYTES - 1), "-".repeat(10));
        let expected = HIGHLIGHT_BYTES - 1..HIGHLIGHT_BYTES;
        assert_eq!(
            ranges_within(&regex, &text, UNHURRIED),
            std::slice::from_ref(&expected)
        );
    }

    #[test]
    fn highlights_skip_empty_matches_and_keep_offsets() {
        let regex = compile("x*", true).unwrap();
        assert_eq!(bounded_ranges(&regex, "axxbx"), [1..3, 4..5]);
        let regex = compile(r"\d+", true).unwrap();
        let text = "öö 12 ä 345";
        let ranges = bounded_ranges(&regex, text);
        assert_eq!(&text[ranges[0].clone()], "12");
        assert_eq!(&text[ranges[1].clone()], "345");
    }

    // A cut must not make `$` match where the line does not end, nor split a character.
    #[test]
    fn a_long_line_is_cut_without_inventing_a_match() {
        let regex = compile("a$", true).unwrap();
        assert!(bounded_ranges(&regex, &"a".repeat(HIGHLIGHT_BYTES * 2)).is_empty());
        let regex = compile("ö", true).unwrap();
        let text = format!("{}{}", "x".repeat(HIGHLIGHT_BYTES - 1), "ö".repeat(4));
        assert!(bounded_ranges(&regex, &text).is_empty());
    }

    #[test]
    fn highlighting_stops_at_its_limits() {
        let regex = compile("a", true).unwrap();
        let found = bounded_ranges(&regex, &"a".repeat(HIGHLIGHT_BYTES));
        assert!(!found.is_empty() && found.len() <= HIGHLIGHT_MATCHES);
        // Every match costs a scan to the end of the text here.
        let regex = compile("a*b|a", true).unwrap();
        let found = bounded_ranges(&regex, &"a".repeat(HIGHLIGHT_BYTES));
        assert!(found.len() < HIGHLIGHT_BYTES / 4, "{}", found.len());
    }
}
