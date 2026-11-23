//! Replaces credentials in text before it is stored.
//!
//! [`redact_credentials`] sees one log line at a time here, so what spans lines looks like
//! nothing on its own: the body of a private key, or the items of a pretty-printed cookie array.
//! The scrubber remembers, per source, that such a block is open and withholds its lines until
//! the block ends.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::redact::redact_credentials;

const KEY_MARKER: &str = "[REDACTED:private-key]";
const COOKIE_MARKER: &str = "[REDACTED:cookie]";

/// How many lines an unterminated block withholds. A 16384-bit PEM key is about 65 lines.
const MAX_BLOCK_LINES: u16 = 512;

/// How many sources can have a block open. A block past it withholds its whole recording, so a
/// flood of key headers costs lines rather than memory or a leak.
const MAX_OPEN_BLOCKS: usize = 4096;

static BLOCK_BEGIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----").unwrap());
static BLOCK_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----").unwrap());
static COOKIE_ARRAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)"(?:set-)?cookie"[ \t]*:[ \t]*\[[ \t]*\r?$"#).unwrap());

/// Whether `text` may open a block: cheap enough for a caller's own thread.
pub(super) fn may_open_block(text: &str) -> bool {
    memchr::memmem::find(text.as_bytes(), b"PRIVATE KEY").is_some()
        || text.trim_end().ends_with('[')
}

/// Text with its credentials replaced.
pub(super) fn scrub(text: &str) -> String {
    redact_credentials(text).text.into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Key,
    Cookies,
    /// A line that may have opened a block was lost: nothing here can say where it ends.
    Lost,
}

struct Open {
    block: Block,
    left: u16,
}

type Source = (i64, String, String);

#[derive(Default)]
pub(super) struct Scrubber {
    open: HashMap<Source, Open>,
    /// Recordings past [`MAX_OPEN_BLOCKS`]: all their lines are withheld until they end.
    sealed: HashSet<i64>,
}

impl Scrubber {
    /// The stored form of one log line of `pod`/`container`.
    pub(super) fn line(
        &mut self,
        recording: i64,
        pod: &str,
        container: &str,
        text: &str,
    ) -> String {
        if self.sealed.contains(&recording) {
            return KEY_MARKER.to_owned();
        }
        let source = (recording, pod.to_owned(), container.to_owned());
        let current = self.open.get(&source).map(|open| open.block);
        if current == Some(Block::Key) {
            if let Some(end) = BLOCK_END.find(text) {
                self.open.remove(&source);
                let rest = self.line(recording, pod, container, &text[end.end()..]);
                return format!("{KEY_MARKER}{rest}");
            }
            self.tick(&source);
            return KEY_MARKER.to_owned();
        }
        if may_open_block(text) {
            if let Some(begin) = BLOCK_BEGIN.find_iter(text).last()
                && !BLOCK_END.is_match(&text[begin.end()..])
            {
                self.begin(source, Block::Key);
                return match current {
                    Some(_) => KEY_MARKER.to_owned(),
                    None => format!("{}{KEY_MARKER}", scrub(&text[..begin.start()])),
                };
            }
            if COOKIE_ARRAY.is_match(text) {
                self.begin(source, Block::Cookies);
                return match current {
                    Some(_) => COOKIE_MARKER.to_owned(),
                    None => scrub(text),
                };
            }
        }
        match current {
            None => scrub(text),
            Some(Block::Cookies) => {
                let line = text.trim();
                if line.starts_with(']') {
                    self.open.remove(&source);
                    return scrub(text);
                }
                if line.ends_with(']') || line.ends_with("],") {
                    self.open.remove(&source);
                } else {
                    self.tick(&source);
                }
                COOKIE_MARKER.to_owned()
            }
            Some(_) => {
                self.tick(&source);
                KEY_MARKER.to_owned()
            }
        }
    }

    /// A line of this source that may have opened a block was lost.
    pub(super) fn lost_line(&mut self, recording: i64, pod: &str, container: &str) {
        let source = (recording, pod.to_owned(), container.to_owned());
        if self.open.get(&source).map(|open| open.block) != Some(Block::Key) {
            self.begin(source, Block::Lost);
        }
    }

    /// Withholds everything of `recording` until it ends.
    pub(super) fn seal(&mut self, recording: i64) {
        self.sealed.insert(recording);
    }

    /// `recording` takes no more lines.
    pub(super) fn forget(&mut self, recording: i64) {
        self.open.retain(|(id, _, _), _| *id != recording);
        self.sealed.remove(&recording);
    }

    fn begin(&mut self, source: Source, block: Block) {
        if self.open.len() >= MAX_OPEN_BLOCKS && !self.open.contains_key(&source) {
            self.sealed.insert(source.0);
            return;
        }
        let left = MAX_BLOCK_LINES;
        self.open.insert(source, Open { block, left });
    }

    fn tick(&mut self, source: &Source) {
        if let Some(open) = self.open.get_mut(source) {
            open.left -= 1;
            if open.left == 0 {
                self.open.remove(source);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEGIN: &str = "-----BEGIN RSA PRIVATE KEY-----";
    const END: &str = "-----END RSA PRIVATE KEY-----";

    #[test]
    fn a_token_in_a_line_is_replaced_and_the_rest_kept() {
        let out = Scrubber::default().line(1, "p", "c", "login password=hunter2 ok");
        assert!(!out.contains("hunter2"));
        assert!(out.starts_with("login password="));
    }

    #[test]
    fn a_key_over_many_lines_is_withheld_until_it_ends() {
        let mut s = Scrubber::default();
        let first = s.line(1, "p", "c", &format!("dump {BEGIN}"));
        assert_eq!(first, format!("dump {KEY_MARKER}"));
        assert_eq!(s.line(1, "p", "c", "MIIEbodybody"), KEY_MARKER);
        assert_eq!(
            s.line(1, "p", "c", &format!("tail {END} after")),
            format!("{KEY_MARKER} after")
        );
        assert_eq!(s.line(1, "p", "c", "plain"), "plain");
    }

    #[test]
    fn a_block_in_one_source_leaves_the_others_alone() {
        let mut s = Scrubber::default();
        s.line(1, "p", "c", BEGIN);
        assert_eq!(s.line(1, "p", "other", "plain"), "plain");
        assert_eq!(s.line(2, "p", "c", "plain"), "plain");
        assert_eq!(s.line(1, "p", "c", "MIIE"), KEY_MARKER);
    }

    #[test]
    fn an_unterminated_block_ends_after_its_limit() {
        let mut s = Scrubber::default();
        s.line(1, "p", "c", BEGIN);
        for _ in 0..MAX_BLOCK_LINES {
            assert_eq!(s.line(1, "p", "c", "body"), KEY_MARKER);
        }
        assert_eq!(s.line(1, "p", "c", "plain"), "plain");
    }

    #[test]
    fn a_cookie_array_over_many_lines_is_masked_until_it_closes() {
        let mut s = Scrubber::default();
        assert_eq!(
            s.line(1, "p", "c", r#"  "set-cookie": ["#),
            r#"  "set-cookie": ["#
        );
        assert_eq!(
            s.line(1, "p", "c", r#"    "sessionid=abc123","#),
            COOKIE_MARKER
        );
        assert_eq!(s.line(1, "p", "c", r#"    "other=def456""#), COOKIE_MARKER);
        assert_eq!(s.line(1, "p", "c", "  ],"), "  ],");
        assert_eq!(s.line(1, "p", "c", "plain"), "plain");
    }

    #[test]
    fn a_cookie_item_holding_a_bracket_does_not_close_the_array() {
        let mut s = Scrubber::default();
        s.line(1, "p", "c", r#""cookie": ["#);
        assert_eq!(s.line(1, "p", "c", r#""a=x]","#), COOKIE_MARKER);
        assert_eq!(s.line(1, "p", "c", r#""b=secret""#), COOKIE_MARKER);
    }

    #[test]
    fn a_block_past_the_limit_seals_its_recording() {
        let mut s = Scrubber::default();
        for i in 0..MAX_OPEN_BLOCKS {
            s.line(1, &format!("p{i}"), "c", BEGIN);
        }
        assert_eq!(s.line(2, "plain", "c", "plain"), "plain");
        s.line(2, "new", "c", BEGIN);
        assert_eq!(s.open.len(), MAX_OPEN_BLOCKS);
        s.forget(1);
        assert_eq!(s.line(2, "new", "c", "MIIE"), KEY_MARKER);
        assert_eq!(s.line(2, "other", "c", "MIIE"), KEY_MARKER);
        s.forget(2);
        assert_eq!(s.line(2, "new", "c", "plain"), "plain");
    }

    #[test]
    fn a_lost_line_withholds_only_its_source_whatever_others_log() {
        let mut s = Scrubber::default();
        s.lost_line(1, "p", "c");
        for _ in 0..2 * MAX_BLOCK_LINES {
            assert_eq!(s.line(1, "q", "c", "plain"), "plain");
        }
        assert_eq!(s.line(1, "p", "c", "MIIE"), KEY_MARKER);
    }

    #[test]
    fn a_key_header_during_a_lost_block_opens_its_own_block() {
        let mut s = Scrubber::default();
        s.lost_line(1, "p", "c");
        assert_eq!(s.line(1, "p", "c", BEGIN), KEY_MARKER);
        for _ in 0..MAX_BLOCK_LINES - 1 {
            assert_eq!(s.line(1, "p", "c", "body"), KEY_MARKER);
        }
        assert_eq!(
            s.line(1, "p", "c", &format!("{END} done")),
            format!("{KEY_MARKER} done")
        );
    }
}
