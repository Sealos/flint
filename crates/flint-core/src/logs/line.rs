//! One log line: the timestamp the API server put in front of it, its text (without terminal
//! escape sequences and control characters, see [`strip_controls`]), its severity (classified
//! once when the line arrives), and, in a workload session, the pod and container it came from.

use std::borrow::Cow;
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

/// The pod and container a line of a workload session came from. One per stream, shared by all
/// of its lines, so tagging a line costs one pointer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LineSource {
    pod: Box<str>,
    container: Box<str>,
    /// `pod/container`, what the prefix shows and what search matches.
    label: Box<str>,
}

impl LineSource {
    pub fn new(pod: &str, container: &str) -> Self {
        LineSource {
            pod: pod.into(),
            container: container.into(),
            label: format!("{pod}/{container}").into(),
        }
    }

    pub fn pod(&self) -> &str {
        &self.pod
    }

    pub fn container(&self) -> &str {
        &self.container
    }

    /// `pod/container`.
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// What [`LogLine::display_as`] puts in front of a line's text. Both are view toggles: every
/// line carries its timestamp and source, so turning them on or off needs no reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineFormat {
    /// The timestamp, when the line has one.
    pub timestamps: bool,
    /// The `[pod/container]` prefix, when the line has a source (workload sessions only).
    pub source: bool,
}

impl Default for LineFormat {
    /// No timestamps; the source prefix on.
    fn default() -> Self {
        LineFormat {
            timestamps: false,
            source: true,
        }
    }
}

/// One line as stored in the buffer. Cheap to clone (the text and the source are shared).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// The timestamp the API server prefixed (Flint always asks for `timestamps=true`), or `None`
    /// if the line did not start with one.
    pub timestamp: Option<Timestamp>,
    /// The line without its timestamp, trailing newline, or carriage return, and without
    /// terminal escape sequences and control characters other than tab ([`strip_controls`]).
    pub text: Arc<str>,
    pub severity: Severity,
    /// The line was longer than [`MAX_LINE_BYTES`] and was cut.
    pub truncated: bool,
    /// The pod and container the line came from: set in workload sessions, `None` in pod
    /// sessions.
    pub source: Option<Arc<LineSource>>,
}

impl LogLine {
    /// Splits off the leading RFC 3339 timestamp, if any, strips terminal escape sequences and
    /// control characters from the rest ([`strip_controls`]), and classifies it.
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
        let text = strip_controls(text);
        LogLine {
            timestamp,
            severity: classify(&text),
            text: Arc::from(text.as_ref()),
            truncated,
            source: None,
        }
    }

    /// The memory this line takes in the buffer: the line itself, the shared text, and the
    /// text's reference counts. What the byte budget counts. The source is shared by every line of
    /// its stream, so a line only pays for its pointer, which `size_of::<LogLine>()` includes.
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of::<LogLine>() + 2 * std::mem::size_of::<usize>() + self.text.len()
    }

    /// [`LogLine::heap_bytes`] of the longest line kept.
    pub fn max_heap_bytes() -> usize {
        std::mem::size_of::<LogLine>() + 2 * std::mem::size_of::<usize>() + MAX_LINE_BYTES
    }

    /// The line as shown: with its `[pod/container]` prefix when it has a source, and its
    /// timestamp when `timestamps` is on and it has one. [`LogLine::display_as`] can leave out the
    /// prefix.
    pub fn display(&self, timestamps: bool) -> String {
        self.display_as(LineFormat {
            timestamps,
            source: true,
        })
    }

    /// The line as shown with `format`: `[pod/container] <timestamp> <text>`, each part present
    /// when the format asks for it and the line has it.
    pub fn display_as(&self, format: LineFormat) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(self.text.len() + 64);
        if let Some(source) = self.source.as_ref().filter(|_| format.source) {
            out.push('[');
            out.push_str(source.label());
            out.push_str("] ");
        }
        if let Some(ts) = self.timestamp.as_ref().filter(|_| format.timestamps) {
            let _ = write!(out, "{ts} ");
        }
        out.push_str(&self.text);
        out
    }
}

/// `text` without terminal escape sequences and control characters, which many programs write
/// to their logs (colors such as `\x1b[32mINFO\x1b[0m`, cursor movement, window titles). They
/// are removed, not interpreted: log lines are shown, searched, copied and saved as plain text,
/// and a color code glued to a word (`\x1b[33mWARN`) would hide it from [`classify`].
///
/// - **Escape sequences** (ECMA-48): CSI (`ESC [` … final byte, which covers SGR colors and
///   cursor codes), OSC (`ESC ]` … BEL or ST, such as titles and hyperlinks), the other control
///   strings (DCS, SOS, PM, APC, up to ST), and two- and three-byte escapes (`ESC 7`,
///   `ESC ( B`). Their 8-bit C1 forms (U+009B for CSI, …) are removed the same way. A sequence
///   the line ends in the middle of (a cut line, say) is removed to the end of the line.
/// - **Control characters:** the other C0 characters except tab, DEL, and the other C1
///   characters are removed rather than replaced with a visible mark. They are not text: a
///   terminal acts on them (a bell, a backspace, a carriage return redrawing a progress bar)
///   instead of printing them, so a mark would show something the program never meant to show
///   and split the words around it for search. Tab is kept: it is text spacing.
///
/// Borrows `text` when there is nothing to remove, which is the common case.
pub fn strip_controls(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    // The end of the text already copied to `out` (or skipped).
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let Some(end) = control_end(bytes, i) else {
            i += 1;
            continue;
        };
        out.get_or_insert_with(|| String::with_capacity(text.len()))
            .push_str(&text[copied..i]);
        i = end;
        copied = end;
    }
    match out {
        None => Cow::Borrowed(text),
        Some(mut out) => {
            out.push_str(&text[copied..]);
            Cow::Owned(out)
        }
    }
}

/// When a control character or escape sequence starts at `i`, the index just past it. Every
/// index it returns is on a character boundary: sequences end at ASCII bytes or after a
/// two-byte C1 character, and stop before any other byte.
fn control_end(bytes: &[u8], i: usize) -> Option<usize> {
    match bytes[i] {
        0x1b => Some(escape_end(bytes, i + 1)),
        b'\t' => None,
        0x00..=0x1f | 0x7f => Some(i + 1),
        // C1 controls, U+0080 to U+009F, are 0xC2 0x80 to 0xC2 0x9F in UTF-8.
        0xc2 => match bytes.get(i + 1) {
            Some(&c1 @ 0x80..=0x9f) => Some(c1_end(c1, bytes, i + 2)),
            _ => None,
        },
        _ => None,
    }
}

/// The end of an escape sequence whose ESC ends at `i`.
fn escape_end(bytes: &[u8], i: usize) -> usize {
    match bytes.get(i) {
        // A lone ESC at the end of the line.
        None => i,
        Some(b'[') => csi_end(bytes, i + 1),
        Some(b']') => string_end(bytes, i + 1, true),
        Some(b'P' | b'X' | b'^' | b'_') => string_end(bytes, i + 1, false),
        // Intermediate bytes, then a final byte: `ESC ( B` selects a character set.
        Some(0x20..=0x2f) => {
            let mut end = i;
            while matches!(bytes.get(end), Some(0x20..=0x2f)) {
                end += 1;
            }
            if matches!(bytes.get(end), Some(0x30..=0x7e)) {
                end + 1
            } else {
                end
            }
        }
        // A two-byte escape: `ESC 7`, `ESC M`, `ESC =`.
        Some(0x30..=0x7e) => i + 1,
        // Not an escape sequence: only the ESC goes.
        Some(_) => i,
    }
}

/// The end of a C1 control character (its second UTF-8 byte is `c1`) that ends at `i`, with
/// the sequence it introduces.
fn c1_end(c1: u8, bytes: &[u8], i: usize) -> usize {
    match c1 {
        0x9b => csi_end(bytes, i),
        0x9d => string_end(bytes, i, true),
        0x90 | 0x98 | 0x9e | 0x9f => string_end(bytes, i, false),
        _ => i,
    }
}

/// The end of a CSI sequence whose parameters start at `i`: parameter and intermediate bytes,
/// then a final byte. A byte that cannot be part of it ends it early and stays.
fn csi_end(bytes: &[u8], mut i: usize) -> usize {
    while let Some(&b) = bytes.get(i) {
        match b {
            0x40..=0x7e => return i + 1,
            0x20..=0x3f => i += 1,
            _ => return i,
        }
    }
    i
}

/// The end of a control string (OSC, DCS, …) whose payload starts at `i`: after its string
/// terminator (`ESC \` or U+009C), or BEL for OSC (`bel`), as xterm accepts. Another ESC ends it
/// early and stays, so the escape it starts is read on its own.
fn string_end(bytes: &[u8], mut i: usize, bel: bool) -> usize {
    while let Some(&b) = bytes.get(i) {
        match b {
            0x07 if bel => return i + 1,
            0x1b if bytes.get(i + 1) == Some(&b'\\') => return i + 2,
            0x1b => return i,
            0xc2 if bytes.get(i + 1) == Some(&0x9c) => return i + 2,
            _ => i += 1,
        }
    }
    i
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

    // covers UP-45
    #[test]
    fn the_source_prefix_is_a_view_toggle() {
        let mut line = LogLine::parse("2024-05-01T12:00:00Z hello", false);
        let plain = line.clone();
        line.source = Some(Arc::new(LineSource::new("web-1", "app")));
        assert_eq!(line.display(false), "[web-1/app] hello");
        assert_eq!(line.display(true), "[web-1/app] 2024-05-01T12:00:00Z hello");
        let bare = LineFormat {
            timestamps: true,
            source: false,
        };
        assert_eq!(line.display_as(bare), "2024-05-01T12:00:00Z hello");
        assert_eq!(line.display_as(LineFormat::default()), "[web-1/app] hello");
        // A pod session's lines have no prefix either way.
        assert_eq!(plain.display_as(LineFormat::default()), "hello");
        // The source is shared: tagging costs the line no more than its pointer.
        assert_eq!(line.heap_bytes(), plain.heap_bytes());
        let source = line.source.as_ref().unwrap();
        assert_eq!(
            (source.pod(), source.container(), source.label()),
            ("web-1", "app", "web-1/app")
        );
    }

    fn stripped(text: &str) -> String {
        strip_controls(text).into_owned()
    }

    // covers UP-40
    #[test]
    fn plain_text_is_borrowed() {
        for text in ["", "hello", "tab\tseparated", "naïve café °C ©", "日本語"] {
            assert!(
                matches!(strip_controls(text), Cow::Borrowed(t) if t == text),
                "{text:?}"
            );
        }
    }

    // covers UP-40
    #[test]
    fn sgr_colors_are_stripped() {
        assert_eq!(stripped("\x1b[32mINFO\x1b[0m started"), "INFO started");
        assert_eq!(
            stripped("\x1b[1;38;5;196mbold red\x1b[m \x1b[38:2::255:0:0mtrue\x1b[39m"),
            "bold red true"
        );
    }

    // covers UP-40
    #[test]
    fn cursor_and_other_escapes_are_stripped() {
        // Cursor movement, erase line, private modes, a charset switch, save and restore.
        assert_eq!(
            stripped("\x1b[2K\x1b[1G\x1b[?25lprogress\x1b[?25h \x1b(Bdone\x1b7\x1b8"),
            "progress done"
        );
        // The 8-bit CSI.
        assert_eq!(stripped("\u{9b}31mred\u{9b}0m"), "red");
    }

    // covers UP-40
    #[test]
    fn osc_and_control_strings_are_stripped() {
        // A window title ended by BEL, a hyperlink ended by ST.
        assert_eq!(stripped("\x1b]0;my title\x07ready"), "ready");
        assert_eq!(
            stripped("see \x1b]8;;https://example.com/ü\x1b\\docs\x1b]8;;\x1b\\ now"),
            "see docs now"
        );
        assert_eq!(stripped("\x1bPq#0;2;0;0;0\x1b\\after"), "after");
        assert_eq!(stripped("\u{9d}2;title\u{9c}after"), "after");
    }

    // covers UP-40
    #[test]
    fn sequences_cut_at_the_end_of_the_line_are_stripped() {
        assert_eq!(stripped("text\x1b"), "text");
        assert_eq!(stripped("text\x1b["), "text");
        assert_eq!(stripped("text\x1b[38;5;1"), "text");
        assert_eq!(stripped("text\x1b]0;title with no end"), "text");
        assert_eq!(stripped("text\x1b("), "text");
        // A malformed CSI ends at the byte that cannot belong to it, which stays.
        assert_eq!(stripped("a\x1b[31\u{e9}b"), "a\u{e9}b");
        // An OSC interrupted by another escape: both go.
        assert_eq!(stripped("\x1b]0;t\x1b[31mred"), "red");
    }

    // covers UP-40
    #[test]
    fn control_characters_other_than_tab_are_stripped() {
        assert_eq!(
            stripped("a\x00b\x07c\x08d\re\x0bf\x0cg\x7fh\u{85}i\tj"),
            "abcdefghi\tj"
        );
    }

    // covers UP-43
    #[test]
    fn lines_are_stripped_before_they_are_classified() {
        let line = LogLine::parse("2024-05-01T12:00:00Z \x1b[33mWARN\x1b[0m disk", false);
        assert_eq!(&*line.text, "WARN disk");
        assert_eq!(line.severity, Severity::Warning);
        let line = LogLine::parse("\x1b[31mERROR\x1b[0m: boom\r\n", false);
        assert_eq!(&*line.text, "ERROR: boom");
        assert_eq!(line.severity, Severity::Error);
        assert_eq!(line.display(false), "ERROR: boom");
        // Classified as raw text, the code would have hidden the word.
        assert_eq!(classify("\x1b[33mWARN\x1b[0m"), Severity::Normal);
    }

    // covers UP-43
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
