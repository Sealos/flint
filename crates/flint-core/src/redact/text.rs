//! Credential redaction for free text: pod logs and event messages served to agents
//! (ADR-0013). Each value that looks like a credential becomes `[REDACTED:<kind>]`; the text
//! around it is kept, so a line still reads as it did.
//!
//! Logs can be large, so most text never reaches a regex. One Aho-Corasick pass over the whole
//! text finds trigger words (`bearer`, `token`, `://`, `AKIA`, ...). Only a line holding a
//! trigger is matched, and only against the rules that trigger can start. Private key blocks
//! span lines, so they are found first, over the whole text. The `regex` crate runs in linear
//! time, so no input can make a pattern backtrack catastrophically.
//!
//! The rules err toward redacting: a harmless value next to `password=` is hidden too.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::{Captures, Regex};

/// Text with credential-shaped values replaced, and how many were replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted<'a> {
    /// The text, borrowed when nothing was replaced.
    pub text: Cow<'a, str>,
    /// How many values were replaced.
    pub count: usize,
}

/// How every marker starts; the kind and `]` follow.
const MARKER_PREFIX: &str = "[REDACTED:";

/// Replaces every value in `text` that looks like a credential with `[REDACTED:<kind>]`:
/// `Authorization` header values, bearer tokens, passwords in URLs, AWS, GCP, GitHub, GitLab
/// and Slack keys and tokens, Slack webhook paths, Azure SAS signatures, JWTs, private key
/// blocks, and the values of `password`, `secret`, `token` and `api_key` style keys (`=`, `:`,
/// JSON and YAML). Keys, user names, hosts and the rest of each line are kept.
pub fn redact_credentials(text: &str) -> Redacted<'_> {
    let mut out = Builder::new(text);
    let mut from = 0;
    for (span, replacement) in private_key_spans(text) {
        redact_lines(text, from..span.start, &mut out);
        out.replace(span.clone(), replacement);
        out.count += 1;
        from = span.end;
    }
    redact_lines(text, from..text.len(), &mut out);
    out.finish()
}

/// The output, allocated at the first replacement.
struct Builder<'a> {
    src: &'a str,
    out: Option<String>,
    /// Bytes of `src` already copied to (or replaced in) `out`.
    done: usize,
    count: usize,
}

impl<'a> Builder<'a> {
    fn new(src: &'a str) -> Self {
        Builder {
            src,
            out: None,
            done: 0,
            count: 0,
        }
    }

    /// Replaces `range` (at or after everything replaced so far) with `with`.
    fn replace(&mut self, range: Range<usize>, with: &str) {
        let out = self
            .out
            .get_or_insert_with(|| String::with_capacity(self.src.len()));
        out.push_str(&self.src[self.done..range.start]);
        out.push_str(with);
        self.done = range.end;
    }

    fn finish(mut self) -> Redacted<'a> {
        let text = match self.out.take() {
            Some(mut out) => {
                out.push_str(&self.src[self.done..]);
                Cow::Owned(out)
            }
            None => Cow::Borrowed(self.src),
        };
        Redacted {
            text,
            count: self.count,
        }
    }
}

// --- Private key blocks ------------------------------------------------------------------------

static PEM_BEGIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----").unwrap());
static PEM_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----").unwrap());

/// The bodies of private key blocks, between the BEGIN and END lines, and what replaces each.
/// The BEGIN and END lines are kept. A block cut off by a log tail is redacted too: an END with
/// no BEGIN before it hides everything from the start of the text (or the previous block), and
/// a BEGIN with no END hides everything after it.
fn private_key_spans(text: &str) -> Vec<(Range<usize>, &'static str)> {
    if memchr::memmem::find(text.as_bytes(), b"PRIVATE KEY").is_none() {
        return Vec::new();
    }
    let mut marks: Vec<(usize, usize, bool)> = PEM_BEGIN
        .find_iter(text)
        .map(|m| (m.start(), m.end(), true))
        .chain(PEM_END.find_iter(text).map(|m| (m.start(), m.end(), false)))
        .collect();
    marks.sort_unstable();
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    let mut last_end = 0;
    for (start, end, begin) in marks {
        match (begin, open) {
            (true, None) => open = Some(end),
            (true, Some(_)) => {}
            (false, from) => {
                let from = from.unwrap_or(last_end);
                spans.push(from..start);
                open = None;
                last_end = end;
            }
        }
    }
    if let Some(from) = open {
        spans.push(from..text.len());
    }
    spans
        .into_iter()
        .map(|span| {
            let multi_line = text[span.clone()].contains('\n');
            let with = if multi_line {
                "\n[REDACTED:private-key]\n"
            } else {
                "[REDACTED:private-key]"
            };
            (span, with)
        })
        .collect()
}

// --- Line rules --------------------------------------------------------------------------------

/// One line rule: a regex whose `v` group (or `v1`..`v4`) is the value to replace, and the
/// marker kind for a match, or `None` to leave that match alone.
struct Rule {
    regex: Regex,
    kind: fn(&Captures, &str) -> Option<&'static str>,
}

/// Rule bits, in the order the rules run: header and URL forms first, so `Authorization: Bearer
/// eyJ...` becomes one marker, then vendor shapes, then generic key-value pairs.
const AUTH: u16 = 1 << 0;
const BEARER: u16 = 1 << 1;
const URL_PASSWORD: u16 = 1 << 2;
const SLACK_WEBHOOK: u16 = 1 << 3;
const AZURE_SAS: u16 = 1 << 4;
const AWS_KEY_ID: u16 = 1 << 5;
const AWS_SECRET_NEAR_ID: u16 = 1 << 6;
const AWS_SECRET: u16 = 1 << 7;
const GITHUB: u16 = 1 << 8;
const GITLAB: u16 = 1 << 9;
const SLACK: u16 = 1 << 10;
const GCP: u16 = 1 << 11;
const JWT: u16 = 1 << 12;
const FLAG: u16 = 1 << 13;
const PAIR: u16 = 1 << 14;

/// Trigger words (matched ignoring ASCII case) and the rules each one can start.
const TRIGGERS: &[(&str, u16)] = &[
    ("authorization", AUTH),
    ("bearer", BEARER),
    ("://", URL_PASSWORD),
    ("hooks.slack.com", SLACK_WEBHOOK),
    ("sig=", AZURE_SAS),
    ("akia", AWS_KEY_ID | AWS_SECRET_NEAR_ID),
    ("asia", AWS_KEY_ID | AWS_SECRET_NEAR_ID),
    ("secret", AWS_SECRET | FLAG | PAIR),
    ("password", FLAG | PAIR),
    ("passwd", FLAG | PAIR),
    ("token", FLAG | PAIR),
    ("key", FLAG | PAIR),
    ("ghp_", GITHUB),
    ("gho_", GITHUB),
    ("ghu_", GITHUB),
    ("ghs_", GITHUB),
    ("ghr_", GITHUB),
    ("github_pat_", GITHUB),
    ("glpat-", GITLAB),
    ("xox", SLACK),
    ("xapp-", SLACK),
    ("aiza", GCP),
    ("eyj", JWT),
];

static TRIGGER_SEARCH: LazyLock<AhoCorasick> = LazyLock::new(|| {
    AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::Standard)
        .build(TRIGGERS.iter().map(|(word, _)| word))
        .expect("trigger words build")
});

/// Compiles `pattern` with `\b` as an ASCII word boundary, which the fast regex engines
/// support on any input.
fn rule(pattern: &str, kind: fn(&Captures, &str) -> Option<&'static str>) -> Rule {
    Rule {
        regex: Regex::new(&pattern.replace(r"\b", r"(?-u:\b)")).expect("redaction rule builds"),
        kind,
    }
}

/// Separator between a key and its value: an optional closing quote (also `\"` and `\\\"` in
/// escaped JSON), `:`, `=`, `=>`, `:=` or a URL-encoded `=`/`:` with spaces or tabs, and an
/// optional `[` (Go's header maps). Each rule adds the opening quote it accepts.
const SEP: &str = r#"\\*["']?[ \t]*(?:=>|:=|[:=]|%3[DdAa])[ \t]*\[?"#;

static RULES: LazyLock<Vec<(u16, Rule)>> = LazyLock::new(|| {
    vec![
        (
            AUTH,
            rule(
                &format!(
                    r#"(?i)\b(?:proxy-)?authorization{SEP}\\?["']?(?P<s>(?:basic|bearer|digest|token|negotiate|ntlm|apikey|aws4-hmac-sha256)[ \t]+)?(?P<v>[^\s"'\\\],;]+)"#
                ),
                |c, v| (c.name("s").is_some() || v.len() >= 16).then_some("authorization"),
            ),
        ),
        (
            BEARER,
            rule(r"(?i)\bbearer[ \t]+(?P<v>[A-Za-z0-9\-._~+/]+=*)", |_, v| {
                // "bearer authentication", "Bearer token": words, not tokens.
                let word = v.bytes().all(|b| b.is_ascii_alphabetic()) && v.len() < 20;
                (v.len() >= 8 && !word).then_some("bearer")
            }),
        ),
        (
            URL_PASSWORD,
            rule(
                // The password runs to the last `@` of the URL, so one holding `@`, `/` or `#`
                // is hidden whole.
                r#"\b[A-Za-z][A-Za-z0-9+.\-]*://[^\s:/?#@"'<>\[\]]*:(?P<v>[^\s"'<>]+)@[^\s/@"'<>]*"#,
                |_, _| Some("password"),
            ),
        ),
        (
            SLACK_WEBHOOK,
            rule(
                r"(?i)hooks\.slack\.com/(?:services|workflows|triggers)/(?P<v>[A-Za-z0-9/_\-]+)",
                |_, _| Some("slack-webhook"),
            ),
        ),
        (
            AZURE_SAS,
            rule(r#"[?&]sig=(?P<v>[^&\s"'<>]+)"#, |_, _| {
                Some("azure-sas-signature")
            }),
        ),
        (
            AWS_KEY_ID,
            rule(
                r"(?:^|[^A-Za-z0-9])(?P<v>(?:AKIA|ASIA)[A-Z0-9]{16})\b",
                |_, _| Some("aws-access-key-id"),
            ),
        ),
        (
            AWS_SECRET_NEAR_ID,
            // A 40-character secret on the same line as an access key id (CSV, env dumps). Git
            // SHAs are lowercase hex, so mixed case is required.
            rule(
                r"(?:^|[^A-Za-z0-9/+=])(?P<v>[A-Za-z0-9/+]{40})(?:$|[^A-Za-z0-9/+=])",
                |_, v| {
                    let upper = v.bytes().any(|b| b.is_ascii_uppercase());
                    let lower = v.bytes().any(|b| b.is_ascii_lowercase());
                    (upper && lower).then_some("aws-secret-access-key")
                },
            ),
        ),
        (
            AWS_SECRET,
            rule(
                &format!(
                    r#"(?i)(?:aws_?)?secret_?access_?key{SEP}\\?["']?(?P<v>[A-Za-z0-9/+=]{{16,}})"#
                ),
                |_, _| Some("aws-secret-access-key"),
            ),
        ),
        (
            GITHUB,
            rule(
                r"(?:^|[^A-Za-z0-9])(?P<v>gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})",
                |_, _| Some("github-token"),
            ),
        ),
        (
            GITLAB,
            rule(
                r"(?:^|[^A-Za-z0-9])(?P<v>glpat-[A-Za-z0-9_\-]{20,})",
                |_, _| Some("gitlab-token"),
            ),
        ),
        (
            SLACK,
            rule(
                r"(?:^|[^A-Za-z0-9])(?P<v>xox[abposre]-[A-Za-z0-9\-]{10,}|xapp-[0-9]+-[A-Za-z0-9\-]{10,})",
                |_, _| Some("slack-token"),
            ),
        ),
        (
            GCP,
            rule(
                r"(?:^|[^A-Za-z0-9])(?P<v>AIza[0-9A-Za-z_\-]{35})",
                |_, _| Some("gcp-api-key"),
            ),
        ),
        (
            JWT,
            rule(
                r"\b(?P<v>eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]*)",
                |_, _| Some("jwt"),
            ),
        ),
        (
            FLAG,
            // `--password hunter2`: a command-line flag with its value after a space.
            rule(
                r#"(?i)(?:^|[ \t])(?P<k>--?[A-Za-z0-9_\-]*(?:password|passwd|secret|token|api[_\-]?key|apikey))[ \t]+(?P<v>"[^"\n]*"|'[^'\n]*'|[^\s\-"'][^\s"']*)"#,
                |c, _| Some(key_kind(c)),
            ),
        ),
        (
            PAIR,
            // The key must end with the keyword, so `token_count=5` is left alone. The value is
            // a quoted string (plain, or escaped inside JSON) or a bare word.
            rule(
                &format!(
                    r#"(?i)(?P<k>[A-Za-z0-9_.\-]*(?:password|passwd|secret|token|api[_\-]?key|apikey|access[_\-]?key|secret[_\-]?key|private[_\-]?key|account[_\-]?key)){SEP}(?:\\+"(?P<v1>[^"\\\n]*)\\+"|"(?P<v2>(?:[^"\\\n]|\\.)*)"|'(?P<v3>[^'\n]*)'|\\*["']?(?P<v4>[^\s"'\\=:>][^\s"'\\]*))"#
                ),
                |c, v| {
                    // A private key's BEGIN line (the block itself is redacted whole), values
                    // that are not values, and values already masked by the workload.
                    if v.is_empty()
                        || v.starts_with("-----")
                        || v.bytes().all(|b| b == b'*')
                        || NOT_VALUES.iter().any(|w| v.eq_ignore_ascii_case(w))
                    {
                        return None;
                    }
                    Some(key_kind(c))
                },
            ),
        ),
    ]
});

/// The marker kind for a credential key (the `k` group): password, token, secret or key.
fn key_kind(caps: &Captures) -> &'static str {
    let key = caps
        .name("k")
        .map_or("", |k| k.as_str())
        .to_ascii_lowercase();
    if key.contains("pass") {
        "password"
    } else if key.contains("token") {
        "token"
    } else if key.contains("secret") {
        "secret"
    } else {
        "key"
    }
}

/// Values after a credential key that are not credentials: empty markers, masks other tools
/// already wrote, and YAML block scalar indicators (the value is on the next lines).
const NOT_VALUES: &[&str] = &[
    "null",
    "true",
    "false",
    "none",
    "nil",
    "<nil>",
    "[REDACTED]",
    "<redacted>",
    "REDACTED",
    "[FILTERED]",
    "|",
    "|-",
    "|+",
    ">-",
    ">+",
];

/// Runs the line rules over the lines of `text[range]` that hold a trigger word.
fn redact_lines(text: &str, range: Range<usize>, out: &mut Builder) {
    if range.is_empty() {
        return;
    }
    let bytes = text.as_bytes();
    let input = aho_corasick::Input::new(text).span(range.clone());
    // The line being collected: its range and the rules its triggers start.
    let mut line: Option<(Range<usize>, u16)> = None;
    for m in TRIGGER_SEARCH.find_overlapping_iter(input) {
        let rules = TRIGGERS[m.pattern().as_usize()].1;
        if let Some((current, mask)) = &mut line
            && m.start() < current.end
        {
            *mask |= rules;
            continue;
        }
        if let Some((current, mask)) = line.take() {
            redact_line(text, current, mask, out);
        }
        let start = memchr::memrchr(b'\n', &bytes[range.start..m.start()])
            .map_or(range.start, |i| range.start + i + 1);
        let end = memchr::memchr(b'\n', &bytes[m.start()..range.end])
            .map_or(range.end, |i| m.start() + i);
        line = Some((start..end, rules));
    }
    if let Some((current, mask)) = line {
        redact_line(text, current, mask, out);
    }
}

fn redact_line(text: &str, range: Range<usize>, mask: u16, out: &mut Builder) {
    let mut line = Cow::Borrowed(&text[range.clone()]);
    for (bit, rule) in RULES.iter() {
        if mask & bit != 0
            && let Some(changed) = apply(rule, &line, &mut out.count)
        {
            line = Cow::Owned(changed);
        }
    }
    if let Cow::Owned(line) = line {
        out.replace(range, &line);
    }
}

/// Applies one rule to a line. Returns the new line, or `None` when nothing was replaced.
fn apply(rule: &Rule, line: &str, count: &mut usize) -> Option<String> {
    let mut out = String::new();
    let mut done = 0;
    for caps in rule.regex.captures_iter(line) {
        let Some(value) = ["v", "v1", "v2", "v3", "v4"]
            .iter()
            .find_map(|name| caps.name(name))
        else {
            continue;
        };
        // A marker from an earlier rule (a separator may have taken its `[`).
        let before = value.start().saturating_sub(1);
        if line.as_bytes()[before..].starts_with(MARKER_PREFIX.as_bytes())
            || value.as_str().starts_with(MARKER_PREFIX)
        {
            continue;
        }
        // Bare values run to whitespace or a quote; closing punctuation around them stays.
        let text = value.as_str().trim_end_matches([',', ';', ')', '}', ']']);
        if text.is_empty() {
            continue;
        }
        let Some(kind) = (rule.kind)(&caps, text) else {
            continue;
        };
        out.push_str(&line[done..value.start()]);
        out.push_str(MARKER_PREFIX);
        out.push_str(kind);
        out.push(']');
        done = value.start() + text.len();
        *count += 1;
    }
    (done > 0).then(|| {
        out.push_str(&line[done..]);
        out
    })
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
