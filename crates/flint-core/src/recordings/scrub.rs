//! Replaces credentials in text before it is stored, and stops storing a log stream after a loss.
//!
//! [`redact_credentials`] sees one log line at a time here, so what spans lines looks like
//! nothing on its own: a PEM body, the items of a pretty-printed cookie array, or a value on the
//! line after its key. A [`LogSource`] remembers that for one run of one container's stream.
//!
//! Recordings fail closed (ADR-0018). Once a row of a run is cut, clipped, dropped or arrives out
//! of order, nothing can say what the following lines are, so the run stores no more text. Only a
//! new [`LogSource`], made when the stream really restarts, stores text again. Nothing in a line
//! can end that state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;

use super::store::{LogRow, MAX_NAME_BYTES, clipped};
use crate::redact::redact_credentials;

const PEM_MARKER: &str = "[REDACTED:private-key]";
const COOKIE_MARKER: &str = "[REDACTED:cookie]";
const VALUE_MARKER: &str = "[REDACTED:value]";
/// What is stored in place of a record that was cut.
pub(super) const CUT_MARKER: &str = "[REDACTED:cut-off]";
/// No sequence number has been lost.
const NO_LOSS: u64 = u64::MAX;
/// Stands for a value when asking the redaction rules whether a word is a credential's key.
const PROBE_VALUE: &str = "flintprobevalue0123456789";

static PEM_BEGIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN ([A-Z0-9 ]+)-----").unwrap());
static PEM_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----END ([A-Z0-9 ]+)-----").unwrap());
static COOKIE_ARRAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)"(?:set-)?cookie"[ \t]*:[ \t]*\[[ \t]*\r?$"#).unwrap());

/// Text with its credentials replaced.
pub(super) fn scrub(text: &str) -> String {
    redact_credentials(text).text.into_owned()
}

/// Where the footer of a PEM block labelled `label` ends in `text`.
fn pem_end(text: &str, label: &str) -> Option<usize> {
    PEM_END
        .captures_iter(text)
        .find(|end| &end[1] == label)
        .and_then(|end| end.get(0))
        .map(|end| end.end())
}

/// Whether `line` ends in a credential's key, so that its value is on the next line.
fn ends_in_credential_key(line: &str) -> bool {
    let Some(word) = line.split_whitespace().next_back() else {
        return false;
    };
    if !(word.ends_with([':', '=']) || word.starts_with('-')) {
        return false;
    }
    !redact_credentials(&format!("{word} {PROBE_VALUE}"))
        .text
        .contains(PROBE_VALUE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    /// A PEM body, up to the footer with this label.
    Pem(String),
    Cookies,
    /// The next line holds a credential's value.
    Value,
}

/// What only the writer touches.
#[derive(Default)]
struct RunState {
    expected: u64,
    block: Option<Block>,
    /// The gap marker's row, and the count it shows.
    marker: Option<i64>,
    marked: u64,
}

impl RunState {
    fn text(&mut self, text: &str) -> String {
        match self.block.take() {
            None => self.outside_block(text),
            Some(Block::Pem(label)) => match pem_end(text, &label) {
                Some(end) => format!("{PEM_MARKER}{}", self.outside_block(&text[end..])),
                None => {
                    self.block = Some(Block::Pem(label));
                    PEM_MARKER.to_owned()
                }
            },
            Some(Block::Cookies) => {
                let line = text.trim();
                if line.starts_with(']') {
                    return scrub(text);
                }
                if !(line.ends_with(']') || line.ends_with("],")) {
                    self.block = Some(Block::Cookies);
                }
                COOKIE_MARKER.to_owned()
            }
            Some(Block::Value) if text.trim().is_empty() => {
                self.block = Some(Block::Value);
                text.to_owned()
            }
            Some(Block::Value) => VALUE_MARKER.to_owned(),
        }
    }

    fn outside_block(&mut self, text: &str) -> String {
        let (mut out, mut rest) = (String::new(), text);
        while let Some(begin) = PEM_BEGIN.captures(rest) {
            let (Some(whole), Some(label)) = (begin.get(0), begin.get(1)) else {
                break;
            };
            out.push_str(&scrub(&rest[..whole.start()]));
            out.push_str(PEM_MARKER);
            let after = &rest[whole.end()..];
            let Some(end) = pem_end(after, label.as_str()) else {
                self.block = Some(Block::Pem(label.as_str().to_owned()));
                return out;
            };
            rest = &after[end..];
        }
        if COOKIE_ARRAY.is_match(rest) {
            self.block = Some(Block::Cookies);
        } else if ends_in_credential_key(rest) {
            self.block = Some(Block::Value);
        }
        out.push_str(&scrub(rest));
        out
    }
}

struct Run {
    pod: String,
    container: String,
    next: AtomicU64,
    /// The first sequence number known to be lost. It only goes down.
    lost_from: AtomicU64,
    /// Rows lost and not yet counted on the gap marker.
    lost: AtomicU64,
    state: Mutex<RunState>,
}

/// One run of one container's log stream. Make a new one each time the stream really restarts:
/// that is the only thing that ends a loss. Clones are the same run.
#[derive(Clone)]
pub struct LogSource(Arc<Run>);

impl LogSource {
    pub fn new(pod: impl Into<String>, container: impl Into<String>) -> Self {
        LogSource(Arc::new(Run {
            pod: clipped(pod.into(), MAX_NAME_BYTES).0,
            container: clipped(container.into(), MAX_NAME_BYTES).0,
            next: AtomicU64::new(0),
            lost_from: AtomicU64::new(NO_LOSS),
            lost: AtomicU64::new(0),
            state: Mutex::default(),
        }))
    }

    /// The next row of this stream. Rows must reach the store in the order they were made here:
    /// one that does not counts as a loss.
    pub fn row(&self, at_ms: i64, text: String, truncated: bool) -> LogRow {
        LogRow {
            at_ms,
            text,
            truncated,
            seq: self.0.next.fetch_add(1, Ordering::AcqRel),
            source: self.clone(),
        }
    }

    pub(super) fn pod(&self) -> &str {
        &self.0.pod
    }

    pub(super) fn container(&self) -> &str {
        &self.0.container
    }

    /// Identifies the run, to tell sources apart.
    pub(super) fn key(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// Whether the row numbered `seq` is at or after a loss.
    pub(super) fn is_closed_at(&self, seq: u64) -> bool {
        seq >= self.0.lost_from.load(Ordering::Acquire)
    }

    pub(super) fn has_loss(&self) -> bool {
        self.0.lost_from.load(Ordering::Acquire) != NO_LOSS
    }

    /// The row numbered `seq` was lost: from it on, this run stores no text.
    pub(super) fn close_at(&self, seq: u64) {
        self.0.lost_from.fetch_min(seq, Ordering::AcqRel);
    }

    /// Adds to the count the gap marker will show.
    pub(super) fn count_lost(&self, rows: u64) {
        self.0.lost.fetch_add(rows, Ordering::AcqRel);
    }

    /// The stored form of a row, or `None` if the run stores no text from it on.
    pub(super) fn admit(&self, row: &LogRow, max: usize) -> Option<String> {
        let mut state = self.state();
        let in_order = row.seq == state.expected;
        state.expected = state.expected.max(row.seq.saturating_add(1));
        if !in_order || row.truncated || row.text.len() > max {
            self.close_at(row.seq);
        }
        if self.is_closed_at(row.seq) {
            self.count_lost(1);
            return None;
        }
        let text = state.text(&row.text);
        if text.len() > max {
            self.close_at(row.seq);
            self.count_lost(1);
            return None;
        }
        Some(text)
    }

    /// Takes the rows to add to the gap marker: its row, if any, and the count it will show.
    pub(super) fn take_for_marker(&self) -> Option<(Option<i64>, u64, u64)> {
        let state = self.state();
        let pending = self.0.lost.swap(0, Ordering::AcqRel);
        (pending > 0).then_some((state.marker, state.marked + pending, pending))
    }

    pub(super) fn marker_written(&self, row: i64, total: u64) {
        let mut state = self.state();
        state.marker = Some(row);
        state.marked = total;
    }

    /// The marker could not be written: its rows wait for the next one.
    pub(super) fn marker_failed(&self, pending: u64) {
        self.count_lost(pending);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The text of a gap marker.
pub(super) fn gap_text(rows: u64) -> String {
    format!("{CUT_MARKER} {rows} lines not recorded until this log stream restarts")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEGIN: &str = "-----BEGIN RSA PRIVATE KEY-----";
    const END: &str = "-----END RSA PRIVATE KEY-----";
    const MAX: usize = 1024;

    fn pass(source: &LogSource, text: &str) -> Option<String> {
        let row = source.row(1, text.to_owned(), false);
        source.admit(&row, MAX)
    }

    fn source() -> LogSource {
        LogSource::new("p", "c")
    }

    #[test]
    fn a_token_in_a_line_is_replaced_and_the_rest_kept() {
        let out = pass(&source(), "login password=hunter2 ok").unwrap();
        assert!(!out.contains("hunter2"));
        assert!(out.starts_with("login password="));
    }

    #[test]
    fn a_key_over_many_lines_is_withheld_until_it_ends() {
        let s = source();
        assert_eq!(
            pass(&s, &format!("dump {BEGIN}")).unwrap(),
            format!("dump {PEM_MARKER}")
        );
        assert_eq!(pass(&s, "MIIEbodybody").unwrap(), PEM_MARKER);
        assert_eq!(
            pass(&s, &format!("tail {END} after")).unwrap(),
            format!("{PEM_MARKER} after")
        );
        assert_eq!(pass(&s, "plain").unwrap(), "plain");
    }

    #[test]
    fn every_pem_label_is_handled_the_same() {
        for label in ["CERTIFICATE", "PRIVATE KEY", "EC PARAMETERS", "X509 CRL"] {
            let s = source();
            pass(&s, &format!("-----BEGIN {label}-----")).unwrap();
            assert_eq!(pass(&s, "MIIEbody").unwrap(), PEM_MARKER, "{label}");
            pass(&s, &format!("-----END {label}-----")).unwrap();
            assert_eq!(pass(&s, "plain").unwrap(), "plain", "{label}");
            let one_line = format!("a -----BEGIN {label}-----MIIE-----END {label}----- b");
            let out = pass(&s, &one_line).unwrap();
            assert!(!out.contains("MIIE"), "{label}");
            assert!(out.ends_with(" b"), "{label}");
        }
    }

    #[test]
    fn a_footer_of_another_label_does_not_end_a_block() {
        let s = source();
        pass(&s, "-----BEGIN CERTIFICATE-----").unwrap();
        pass(&s, END).unwrap();
        assert_eq!(pass(&s, "MIIEbody").unwrap(), PEM_MARKER);
    }

    #[test]
    fn a_block_with_no_footer_stays_withheld_however_long() {
        let s = source();
        pass(&s, BEGIN).unwrap();
        for _ in 0..2000 {
            assert_eq!(pass(&s, "body").unwrap(), PEM_MARKER);
        }
    }

    #[test]
    fn a_block_in_one_source_leaves_the_others_alone() {
        let (s, other) = (source(), LogSource::new("p", "other"));
        pass(&s, BEGIN).unwrap();
        assert_eq!(pass(&other, "plain").unwrap(), "plain");
        assert_eq!(pass(&s, "MIIE").unwrap(), PEM_MARKER);
    }

    #[test]
    fn a_cookie_array_over_many_lines_is_masked_until_it_closes() {
        let s = source();
        assert_eq!(
            pass(&s, r#"  "set-cookie": ["#).unwrap(),
            r#"  "set-cookie": ["#
        );
        assert_eq!(
            pass(&s, r#"    "sessionid=abc123","#).unwrap(),
            COOKIE_MARKER
        );
        assert_eq!(pass(&s, r#"    "other=def456""#).unwrap(), COOKIE_MARKER);
        assert_eq!(pass(&s, "  ],").unwrap(), "  ],");
        assert_eq!(pass(&s, "plain").unwrap(), "plain");
    }

    #[test]
    fn a_cookie_item_holding_a_bracket_does_not_close_the_array() {
        let s = source();
        pass(&s, r#""cookie": ["#).unwrap();
        assert_eq!(pass(&s, r#""a=x]","#).unwrap(), COOKIE_MARKER);
        assert_eq!(pass(&s, r#""b=secret""#).unwrap(), COOKIE_MARKER);
    }

    #[test]
    fn a_value_on_the_line_after_its_key_is_replaced() {
        for key in [
            r#""token":"#,
            r#"{"a":1,"password":"#,
            "--token",
            "password=",
        ] {
            let s = source();
            pass(&s, key).unwrap();
            assert_eq!(pass(&s, "").unwrap(), "", "{key}");
            assert_eq!(pass(&s, r#""SECRETVALUE""#).unwrap(), VALUE_MARKER, "{key}");
            assert_eq!(pass(&s, "plain").unwrap(), "plain", "{key}");
        }
    }

    #[test]
    fn a_line_ending_in_an_ordinary_key_keeps_the_next_line() {
        let s = source();
        pass(&s, r#""name":"#).unwrap();
        assert_eq!(pass(&s, r#""web""#).unwrap(), r#""web""#);
    }

    #[test]
    fn no_line_content_ends_a_loss() {
        let s = source();
        let cut = s.row(1, "noise".to_owned(), true);
        assert_eq!(s.admit(&cut, MAX), None);
        for text in [BEGIN, END, "", "]", "plain", "-----END CERTIFICATE-----"] {
            assert_eq!(pass(&s, text), None, "{text:?}");
        }
    }

    #[test]
    fn a_new_run_stores_text_again_and_the_old_one_never_does() {
        let old = source();
        let cut = old.row(1, "noise".to_owned(), true);
        assert_eq!(old.admit(&cut, MAX), None);
        let new = source();
        assert_eq!(pass(&new, "plain").unwrap(), "plain");
        assert_eq!(pass(&old, "late"), None);
    }

    #[test]
    fn a_row_out_of_order_is_a_loss() {
        let s = source();
        let (first, second) = (
            s.row(1, "first".to_owned(), false),
            s.row(2, "second".to_owned(), false),
        );
        assert_eq!(s.admit(&second, MAX), None);
        assert_eq!(s.admit(&first, MAX), None);
        assert_eq!(pass(&s, "third"), None);
    }

    #[test]
    fn a_row_before_a_loss_is_still_stored() {
        let s = source();
        let (kept, lost) = (
            s.row(1, "kept".to_owned(), false),
            s.row(2, "lost".to_owned(), false),
        );
        s.close_at(lost.seq);
        assert_eq!(s.admit(&kept, MAX).as_deref(), Some("kept"));
        assert_eq!(pass(&s, "after"), None);
    }

    #[test]
    fn text_past_the_limit_is_a_loss() {
        let s = source();
        assert_eq!(pass(&s, &"x".repeat(MAX + 1)), None);
        assert_eq!(pass(&s, "plain"), None);
    }
}
