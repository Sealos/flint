//! One log line: the timestamp the API server put in front of it, its text, and its severity,
//! classified once when the line arrives.

use std::sync::Arc;

use k8s_openapi::jiff::Timestamp;

/// A line's position in its session: assigned in arrival order, starting at 0, and never reused,
/// so it stays valid (and comparable) after older lines are evicted from the ring buffer.
pub type LineId = u64;

/// Longest line kept, in bytes. The rest of a longer line is dropped and the line is marked
/// [`LogLine::truncated`], so one runaway line cannot use unbounded memory.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// How a line is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Severity {
    #[default]
    Normal,
    Warning,
    Error,
}

/// One line as stored in the buffer. Cheap to clone (the text is shared).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// The timestamp the API server prefixed (Flint always asks for `timestamps=true`), or `None`
    /// if the line did not start with one.
    pub timestamp: Option<Timestamp>,
    /// The line without its timestamp, trailing newline, or carriage return.
    pub text: Arc<str>,
    pub severity: Severity,
    /// The line was longer than [`MAX_LINE_BYTES`] and was cut.
    pub truncated: bool,
}

impl LogLine {
    /// Splits off the leading RFC 3339 timestamp, if any, and classifies the rest.
    pub fn parse(raw: &str, truncated: bool) -> Self {
        let raw = raw.strip_suffix('\n').unwrap_or(raw);
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let (timestamp, text) = match raw.split_once(' ') {
            Some((head, rest)) => match head.parse::<Timestamp>() {
                Ok(ts) => (Some(ts), rest),
                Err(_) => (None, raw),
            },
            // A line holding only a timestamp is an empty line.
            None => match raw.parse::<Timestamp>() {
                Ok(ts) => (Some(ts), ""),
                Err(_) => (None, raw),
            },
        };
        LogLine {
            timestamp,
            severity: classify(text),
            text: Arc::from(text),
            truncated,
        }
    }

    /// The memory this line takes in the buffer: the line itself, the shared text, and the
    /// text's reference counts. What the byte budget counts.
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of::<LogLine>() + 2 * std::mem::size_of::<usize>() + self.text.len()
    }

    /// [`LogLine::heap_bytes`] of the longest line kept.
    pub fn max_heap_bytes() -> usize {
        std::mem::size_of::<LogLine>() + 2 * std::mem::size_of::<usize>() + MAX_LINE_BYTES
    }

    /// The line as shown: with its timestamp in front when `timestamps` is on and it has one.
    pub fn display(&self, timestamps: bool) -> String {
        match (&self.timestamp, timestamps) {
            (Some(ts), true) => format!("{ts} {}", self.text),
            _ => self.text.to_string(),
        }
    }
}

/// The severity of a line's text. Case-insensitive and on whole words (runs of letters, digits
/// and `_`): `ERROR`, `FATAL`, `panic`, and any word ending in `Exception` or `Error` mark an
/// error; `WARN` and `WARNING` mark a warning. An error wins over a warning.
pub fn classify(text: &str) -> Severity {
    let mut severity = Severity::Normal;
    for word in text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
    {
        if is_error_word(word) {
            return Severity::Error;
        }
        if word.eq_ignore_ascii_case("warn") || word.eq_ignore_ascii_case("warning") {
            severity = Severity::Warning;
        }
    }
    severity
}

fn is_error_word(word: &str) -> bool {
    ["fatal", "panic"]
        .iter()
        .any(|w| word.eq_ignore_ascii_case(w))
        || ends_with_ignore_ascii_case(word, "error")
        || ends_with_ignore_ascii_case(word, "exception")
}

fn ends_with_ignore_ascii_case(word: &str, suffix: &str) -> bool {
    let (w, s) = (word.as_bytes(), suffix.as_bytes());
    w.len() >= s.len() && w[w.len() - s.len()..].eq_ignore_ascii_case(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-40
    #[test]
    fn splits_the_timestamp_and_strips_line_endings() {
        let line = LogLine::parse("2024-05-01T12:00:00.123456789Z hello world\r\n", false);
        assert_eq!(
            line.timestamp,
            Some("2024-05-01T12:00:00.123456789Z".parse().unwrap())
        );
        assert_eq!(&*line.text, "hello world");
        assert_eq!(
            line.display(true),
            "2024-05-01T12:00:00.123456789Z hello world"
        );
        assert_eq!(line.display(false), "hello world");

        let bare = LogLine::parse("no timestamp here", true);
        assert_eq!(bare.timestamp, None);
        assert_eq!(&*bare.text, "no timestamp here");
        assert!(bare.truncated);
        assert_eq!(bare.display(true), "no timestamp here");

        let empty = LogLine::parse("2024-05-01T12:00:00Z", false);
        assert!(empty.timestamp.is_some());
        assert_eq!(&*empty.text, "");
        assert!(empty.heap_bytes() < line.heap_bytes());
        assert!(line.heap_bytes() <= LogLine::max_heap_bytes());
    }

    // covers UP-40
    #[test]
    fn severity_follows_the_whole_word_rules() {
        use Severity::*;
        let cases = [
            ("ERROR something broke", Error),
            ("level=error msg=x", Error),
            ("FATAL: out of memory", Error),
            ("thread 'main' panicked", Normal),
            ("panic: runtime error", Error),
            ("java.lang.NullPointerException: x", Error),
            ("raise ValueError('bad')", Error),
            ("ioerror happened", Error),
            ("Exception in thread main", Error),
            ("WARN disk almost full", Warning),
            ("[warning] slow", Warning),
            ("warnings: 3", Normal),
            ("errors: 0", Normal),
            ("ERROR_CODE=0", Normal),
            ("errorCount=0", Normal),
            ("WARN then ERROR", Error),
            ("all good", Normal),
            ("", Normal),
        ];
        for (text, expected) in cases {
            assert_eq!(classify(text), expected, "{text:?}");
        }
        // Classified on the text, not the timestamp.
        assert_eq!(
            LogLine::parse("2024-05-01T12:00:00Z WARN x", false).severity,
            Warning
        );
    }
}
