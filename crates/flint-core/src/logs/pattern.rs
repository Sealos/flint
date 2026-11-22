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
/// How many bytes one line's highlighting may search, counting a whole rescan per match:
/// iterating matches can be quadratic in the text even with a linear engine.
const HIGHLIGHT_WORK: usize = 128 * 1024;
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
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(PatternError::TooLong);
    }
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

/// The non-empty matches of `regex` in `text` to highlight, within the limits above. A match
/// that ends where the text was cut is left out: `$` or `\b` may have matched only there.
pub(super) fn bounded_ranges(regex: &Regex, text: &str) -> Vec<Range<usize>> {
    let mut cut = text.len().min(HIGHLIGHT_BYTES);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let hay = &text[..cut];
    let mut out = Vec::new();
    let (mut work, mut from) = (0, 0);
    for found in regex.find_iter(hay) {
        work += hay.len() - from;
        from = found.end();
        if !found.range().is_empty() && (cut == text.len() || found.end() < cut) {
            out.push(found.range());
        }
        if work >= HIGHLIGHT_WORK || out.len() == HIGHLIGHT_MATCHES {
            break;
        }
    }
    out
}

/// Stops a scan running on the core's threads: the scan ends at the next line, having scanned
/// the lines before it. Clones share the signal, and so do clones of the search or filter that
/// handed it out.
#[derive(Debug, Clone, Default)]
pub struct ScanCancel(Arc<AtomicBool>);

impl ScanCancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
