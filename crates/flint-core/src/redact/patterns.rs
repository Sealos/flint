//! PEM delimiters and cookie array openers used in free text and recorded log streams.

use std::ops::Range;
use std::sync::LazyLock;

use regex::{Match, Regex, RegexBuilder};

pub(crate) const PRIVATE_KEY_MARKER: &str = "[REDACTED:private-key]";
pub(crate) const COOKIE_MARKER: &str = "[REDACTED:cookie]";

const PEM_LABEL_CHAR: &str = "[A-Z0-9 ]";

struct PemDelimiters {
    begin: Regex,
    end: Regex,
}

impl PemDelimiters {
    fn new(label: &str) -> Self {
        let delimiter = |direction| {
            Regex::new(&format!("-----{direction} ({label})-----")).expect("PEM delimiter builds")
        };
        Self {
            begin: delimiter("BEGIN"),
            end: delimiter("END"),
        }
    }
}

static ALL_PEM: LazyLock<PemDelimiters> =
    LazyLock::new(|| PemDelimiters::new(&format!("{PEM_LABEL_CHAR}+")));

// Match private-key delimiters directly. One can share its leading dashes with the end of
// another PEM delimiter, so filtering non-overlapping matches from ALL_PEM can miss it.
static PRIVATE_KEY_PEM: LazyLock<PemDelimiters> =
    LazyLock::new(|| PemDelimiters::new(&format!("{PEM_LABEL_CHAR}*PRIVATE KEY(?: BLOCK)?")));

/// Private key delimiters, with opening delimiters first (marked `true`) and closing ones next.
pub(super) fn private_key_delimiters(text: &str) -> impl Iterator<Item = (Match<'_>, bool)> {
    PRIVATE_KEY_PEM
        .begin
        .find_iter(text)
        .map(|m| (m, true))
        .chain(PRIVATE_KEY_PEM.end.find_iter(text).map(|m| (m, false)))
}

/// The first PEM opener's span and label.
pub(crate) fn pem_begin(text: &str) -> Option<(Range<usize>, &str)> {
    let captures = ALL_PEM.begin.captures(text)?;
    Some((captures.get(0)?.range(), captures.get(1)?.as_str()))
}

/// Where the first PEM footer with this exact label ends.
pub(crate) fn pem_end(text: &str, label: &str) -> Option<usize> {
    ALL_PEM
        .end
        .captures_iter(text)
        .find(|end| &end[1] == label)
        .and_then(|end| end.get(0))
        .map(|end| end.end())
}

const COOKIE_ARRAY: &str = r#"(?i)"(?:set-)?cookie"[ \t]*:[ \t]*\[[ \t]*\r?$"#;
static COOKIE_ARRAY_IN_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(COOKIE_ARRAY).expect("cookie array builds"));
static COOKIE_ARRAY_IN_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(COOKIE_ARRAY)
        .multi_line(true)
        .build()
        .expect("multiline cookie array builds")
});

pub(crate) fn ends_in_cookie_array_opener(text: &str) -> bool {
    COOKIE_ARRAY_IN_LINE.is_match(text)
}

pub(super) fn cookie_array_openers(text: &str) -> impl Iterator<Item = Match<'_>> {
    COOKIE_ARRAY_IN_TEXT.find_iter(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_delimiters_cover_both_redaction_policies() {
        for (label, private) in [
            ("PRIVATE KEY", true),
            ("RSA PRIVATE KEY", true),
            ("DSA PRIVATE KEY", true),
            ("EC PRIVATE KEY", true),
            ("OPENSSH PRIVATE KEY", true),
            ("ENCRYPTED PRIVATE KEY", true),
            ("PGP PRIVATE KEY BLOCK", true),
            ("A1PRIVATE KEY", true),
            ("CERTIFICATE", false),
            ("PUBLIC KEY", false),
            ("EC PARAMETERS", false),
            ("X509 CRL", false),
        ] {
            let begin = format!("-----BEGIN {label}-----");
            let end = format!("-----END {label}-----");
            let text = format!("prefix {begin}body{end} suffix");
            let start = "prefix ".len();
            assert_eq!(pem_begin(&text), Some((start..start + begin.len(), label)));
            assert_eq!(pem_end(&text, label), Some(text.len() - " suffix".len()));
            assert_eq!(pem_end(&text, "OTHER"), None);
            let marks: Vec<_> = private_key_delimiters(&text)
                .map(|(m, opening)| (m.as_str(), opening))
                .collect();
            if private {
                assert_eq!(marks, [(begin.as_str(), true), (end.as_str(), false)]);
            } else {
                assert!(marks.is_empty(), "{label}");
            }
        }
    }

    #[test]
    fn cookie_array_openers_keep_their_line_boundaries() {
        for opener in ["\"cOoKiE\": [", "\"SET-COOKIE\"\t:\t[ \t\r"] {
            assert!(ends_in_cookie_array_opener(opener));
            let text = format!("{opener}\n  \"session=value\"\n]");
            assert!(!ends_in_cookie_array_opener(&text));
            let found: Vec<_> = cookie_array_openers(&text).map(|m| m.as_str()).collect();
            assert_eq!(found, [opener]);
        }
    }
}
