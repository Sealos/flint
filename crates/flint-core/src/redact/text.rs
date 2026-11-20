//! Credential redaction for free text: pod logs and event messages served to agents
//! (ADR-0013). Each value that looks like a credential becomes `[REDACTED:<kind>]`; the text
//! around it is kept, so a line still reads as it did.
//!
//! Logs can be large, so most text never reaches a regex. One Aho-Corasick pass over the whole
//! text finds trigger words (`bearer`, `token`, `://`, `AKIA`, ...). Only a line holding a
//! trigger is matched, and only against the rules that trigger can start. Private key blocks
//! span lines, so they are found first, over the whole text. The `regex` crate runs in linear
//! time, so no input can make a pattern backtrack catastrophically, and no pattern captures a
//! value of open-ended length: a quoted value's end is found by one forward scan, and the next
//! match starts after it. So a line costs time linear in its length, however many values it
//! holds.
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
    /// Where markers were written in `text`: the byte offset of each replaced line (or private
    /// key marker) and how many markers it holds, in order.
    marks: Vec<(usize, usize)>,
}

impl Redacted<'_> {
    /// How many of the replaced values are at or after byte `offset` of `text`, when `text` is
    /// cut at the start of a line. Markers the input already held are not counted.
    pub fn count_from(&self, offset: usize) -> usize {
        self.marks
            .iter()
            .filter(|(at, _)| *at >= offset)
            .map(|(_, n)| n)
            .sum()
    }
}

/// How every marker starts; the kind and `]` follow.
const MARKER_PREFIX: &str = "[REDACTED:";

/// Replaces every value in `text` that looks like a credential with `[REDACTED:<kind>]`:
/// `Authorization` header values, bearer tokens, `Cookie` and `Set-Cookie` header values,
/// passwords in URLs and in MySQL DSNs (`user:pass@tcp(host)/db`), registry `auth` values in
/// docker config JSON, AWS, GCP, GitHub, GitLab and Slack keys and tokens, Slack webhook paths,
/// Azure SAS signatures, JWTs, private key blocks, and the values of `password`, `secret`,
/// `token` and `api_key` style keys (`=`, `:`, JSON and YAML; an unquoted value after `: ` runs
/// to the end of the line). Keys, user names, hosts and the rest of each line are kept.
pub fn redact_credentials(text: &str) -> Redacted<'_> {
    let mut out = Builder::new(text);
    let mut from = 0;
    for (span, replacement) in whole_text_spans(text) {
        redact_lines(text, from..span.start, &mut out);
        out.replace(span.clone(), replacement);
        let marker = out.written() - replacement.len() + replacement.find('[').unwrap_or(0);
        out.marks.push((marker, 1));
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
    marks: Vec<(usize, usize)>,
}

impl<'a> Builder<'a> {
    fn new(src: &'a str) -> Self {
        Builder {
            src,
            out: None,
            done: 0,
            count: 0,
            marks: Vec::new(),
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

    /// Bytes written to the output so far.
    fn written(&self) -> usize {
        self.out.as_ref().map_or(0, String::len)
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
            marks: self.marks,
        }
    }
}

/// What spans lines and is found over the whole text before the line rules run: private key
/// bodies and the items of pretty-printed cookie arrays, in order, none overlapping.
fn whole_text_spans(text: &str) -> Vec<(Range<usize>, &'static str)> {
    let mut spans = private_key_spans(text);
    spans.extend(
        cookie_array_items(text)
            .into_iter()
            .map(|item| (item, COOKIE_MARKER)),
    );
    spans.sort_by_key(|(span, _)| span.start);
    let mut end = 0;
    spans.retain(|(span, _)| {
        let keep = span.start >= end;
        if keep {
            end = span.end;
        }
        keep
    });
    spans
}

// --- Cookie arrays over several lines ----------------------------------------------------------

/// The marker for a cookie header value.
const COOKIE_MARKER: &str = "[REDACTED:cookie]";

/// A cookie header whose JSON array of values opens at the end of its line (pretty-printed).
static COOKIE_ARRAY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?im)(?:set-)?cookie"[ \t]*:[ \t]*\[[ \t]*\r?$"#).expect("cookie array builds")
});

/// The contents of each quoted item of the pretty-printed cookie arrays in `text`, so each
/// becomes `"[REDACTED:cookie]"` and the JSON still parses. An array on one line is the line
/// rule's. One forward scan per array.
fn cookie_array_items(text: &str) -> Vec<Range<usize>> {
    let mut items = Vec::new();
    let bytes = text.as_bytes();
    let mut scanned = 0;
    for m in COOKIE_ARRAY.find_iter(text) {
        let mut i = m.end().max(scanned);
        loop {
            while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
                i += 1;
            }
            if bytes.get(i) != Some(&b'"') {
                break;
            }
            let len = quoted_len(&text[i + 1..], 0);
            let item = i + 1..i + 1 + len;
            if !item.is_empty() && &text[item.clone()] != COOKIE_MARKER {
                items.push(item);
            }
            i += len + 2;
        }
        scanned = i;
    }
    items
}

// --- Private key blocks ------------------------------------------------------------------------

static PEM_BEGIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----").unwrap());
static PEM_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----").unwrap());

/// The marker that replaces a private key's body.
const PRIVATE_KEY_MARKER: &str = "[REDACTED:private-key]";

/// The bodies of private key blocks, between the BEGIN and END lines, and what replaces each.
/// The BEGIN and END lines are kept. A BEGIN with no END hides everything after it (a block cut
/// off at the end). An END with no BEGIN hides only the key lines right before it (a block whose
/// start a log tail cut off), so text that merely names the END line is kept.
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
            (false, Some(from)) => {
                spans.push(from..start);
                open = None;
                last_end = end;
            }
            (false, None) => {
                if let Some(from) = cut_off_body_start(text, last_end, start) {
                    spans.push(from..start);
                }
                last_end = end;
            }
        }
    }
    if let Some(from) = open {
        spans.push(from..text.len());
    }
    spans
        .into_iter()
        // Already redacted (the text went through here before): not counted again.
        .filter(|span| text[span.clone()].trim() != PRIVATE_KEY_MARKER)
        .map(|span| {
            let multi_line = text[span.clone()].contains('\n');
            let with = if multi_line {
                "\n[REDACTED:private-key]\n"
            } else {
                PRIVATE_KEY_MARKER
            };
            (span, with)
        })
        .collect()
}

/// Shortest run of key characters an END with no BEGIN must follow to be taken as a cut-off
/// block.
const MIN_CUT_OFF_KEY: usize = 16;

/// Where the body of a block cut off before its END at `end` starts: the base64 lines (and, in
/// escaped JSON, `\n`-separated segments) right before it, each at least as wide as the one
/// after it, no earlier than `floor`. `None` when there are fewer than [`MIN_CUT_OFF_KEY`] key
/// characters, as in "missing -----END ...".
fn cut_off_body_start(text: &str, floor: usize, end: usize) -> Option<usize> {
    let key_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    let mut start = end;
    let mut key_chars = 0;
    let mut line_end = end;
    let mut footer_line = true;
    // Key lines have one width, and only the last one is shorter, so going back each line must
    // be at least as long as the one after it: "starting" before a key line is not part of it.
    let mut width = 0;
    loop {
        let line_start = memchr::memrchr(b'\n', &text.as_bytes()[floor..line_end])
            .map_or(floor, |i| floor + i + 1);
        let line = &text[line_start..line_end];
        // Segments split at a literal `\n` (escaped JSON), from the last one back.
        let mut segment_end = line.len();
        let mut whole_line = true;
        for (i, segment) in line.rsplit("\\n").enumerate() {
            let segment_start = segment_end - segment.len();
            let trimmed = segment.trim();
            if trimmed.len() >= width && !trimmed.is_empty() && trimmed.bytes().all(key_byte) {
                key_chars += trimmed.len();
                width = trimmed.len();
                start = line_start + segment_start;
            } else if !(trimmed.is_empty() && i == 0 && footer_line) {
                // A key line at the end of a longer segment: `"private_key": "MIIE...`.
                let kept = segment.trim_end();
                let tail = kept.bytes().rev().take_while(|&b| key_byte(b)).count();
                if tail >= MIN_CUT_OFF_KEY {
                    key_chars += tail;
                    start = line_start + segment_start + kept.len() - tail;
                }
                whole_line = false;
                break;
            }
            segment_end = segment_start.saturating_sub(2);
        }
        if !whole_line || line_start == floor {
            break;
        }
        line_end = line_start - 1;
        footer_line = false;
    }
    (key_chars >= MIN_CUT_OFF_KEY).then_some(start)
}

// --- Line rules --------------------------------------------------------------------------------

/// One line rule: a regex with the value to replace in one group, and the marker kind for a
/// match, or `None` to leave that match alone. The value group is `v` (replaced as matched),
/// `vt` (a bare value: closing punctuation after it is kept), `vq` (empty, right after an
/// opening double quote: the value runs to its closing quote, with `q` holding the backslashes
/// before the opening quote), `vd` (empty: the value runs to the closing quote when `q`
/// matched, else to the end of the line) or `vl` (the value's first character: the value runs
/// to the end of the line, or to a double quote or escaped `\n` before it, see
/// [`line_value_len`]). The regex never scans an open-ended value itself; [`quoted_len`] and
/// [`line_value_len`] find its end in one forward pass, so each line costs linear time.
struct Rule {
    regex: Regex,
    kind: fn(&Captures, &str) -> Option<&'static str>,
}

/// Rule bits, in the order the rules run: header and URL forms first, so `Authorization: Bearer
/// eyJ...` becomes one marker, then vendor shapes, then generic key-value pairs.
const AUTH: u32 = 1 << 0;
const BEARER: u32 = 1 << 1;
const URL_PASSWORD: u32 = 1 << 2;
const URL_PASSWORD_LOOSE: u32 = 1 << 3;
const SLACK_WEBHOOK: u32 = 1 << 4;
const AZURE_SAS: u32 = 1 << 5;
const AWS_KEY_ID: u32 = 1 << 6;
const AWS_SECRET_NEAR_ID: u32 = 1 << 7;
const AWS_SECRET: u32 = 1 << 8;
const GITHUB: u32 = 1 << 9;
const GITLAB: u32 = 1 << 10;
const SLACK: u32 = 1 << 11;
const GCP: u32 = 1 << 12;
const VENDOR: u32 = 1 << 13;
const JWT: u32 = 1 << 14;
const FLAG: u32 = 1 << 15;
const PAIR: u32 = 1 << 16;
const COOKIE: u32 = 1 << 17;
const REGISTRY_AUTH: u32 = 1 << 18;
const MYSQL_DSN: u32 = 1 << 19;

/// Trigger words (matched ignoring ASCII case) and the rules each one can start.
const TRIGGERS: &[(&str, u32)] = &[
    ("authorization", AUTH),
    ("bearer", BEARER),
    ("cookie", COOKIE),
    ("\"auth", REGISTRY_AUTH),
    ("'auth", REGISTRY_AUTH),
    ("auth:", REGISTRY_AUTH),
    ("auth=", REGISTRY_AUTH),
    ("auth ", REGISTRY_AUTH),
    ("auth\t", REGISTRY_AUTH),
    ("@tcp", MYSQL_DSN),
    ("@udp(", MYSQL_DSN),
    ("@unix(", MYSQL_DSN),
    ("://", URL_PASSWORD | URL_PASSWORD_LOOSE),
    ("hooks.slack.com", SLACK_WEBHOOK),
    ("sig=", AZURE_SAS),
    ("akia", AWS_KEY_ID | AWS_SECRET_NEAR_ID),
    ("asia", AWS_KEY_ID | AWS_SECRET_NEAR_ID),
    ("secret", AWS_SECRET | FLAG | PAIR),
    ("pass", FLAG | PAIR),
    ("pwd", PAIR),
    ("token", FLAG | PAIR),
    ("key", FLAG | PAIR),
    ("credential", PAIR),
    ("ghp_", GITHUB),
    ("gho_", GITHUB),
    ("ghu_", GITHUB),
    ("ghs_", GITHUB),
    ("ghr_", GITHUB),
    ("github_pat_", GITHUB),
    ("glpat-", GITLAB),
    ("glrt-", GITLAB),
    ("gldt-", GITLAB),
    ("xox", SLACK),
    ("xapp-", SLACK),
    ("aiza", GCP),
    ("sk_live_", VENDOR),
    ("rk_live_", VENDOR),
    ("sk-proj-", VENDOR),
    ("sk-ant-", VENDOR),
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

/// [`SEP`] without the optional `[`, for a rule that handles a list itself.
const SEP_NO_LIST: &str = r#"\\*["']?[ \t]*(?:=>|:=|[:=]|%3[DdAa])[ \t]*"#;

/// A value after a key: double-quoted (at any JSON escape level, to its closing quote),
/// single-quoted, or bare.
const VALUE: &str =
    r#"(?:(?P<q>\\*)"(?P<vq>)|'(?P<v>[^'\n]*)'|\\*'?(?P<vt>[^\s"'\\=:>][^\s"'\\]*))"#;

/// Credential key names. The key must end with one, so `token_count=5` is left alone; `pass`
/// and `pwd` must be the whole key or follow a `_`, `-` or `.` (`DB_PASS`), so `bypass` is too.
const KEYS: &str = r"[A-Za-z0-9_.\-]*(?:password|passwd|secret|token|api[_\-]?key|apikey|access[_\-]?key|secret[_\-]?key|private[_\-]?key|account[_\-]?key|secret[_\-]?key[_\-]?base|(?:encryption|signing|master|ssh)[_\-]?key|credentials?)|(?:[A-Za-z0-9_.\-]*[_.\-])?(?:pass|pwd)";

/// Keys for command-line flags with the value in the next word.
const FLAG_KEYS: &str = r"[A-Za-z0-9_\-]*(?:password|passwd|secret|token|api[_\-]?key|apikey|credentials?)|(?:[A-Za-z0-9_\-]*[_\-])?(?:pass|pwd)";

/// A parameter of a Digest or AWS SigV4 `Authorization` value: `name=value` or `name="value"`.
const AUTH_PARAM: &str = r#"[A-Za-z0-9_\-]+=(?:\\*"[^"\\\n]*\\*"|[^\s,"'\\\]]+)"#;

static RULES: LazyLock<Vec<(u32, Rule)>> = LazyLock::new(|| {
    vec![
        (
            AUTH,
            // Digest values are hidden to the end of the header (the closing quote when the
            // header is quoted, else the end of the line), however their parameters are
            // separated or quoted, so a `response` never shows. SigV4 values are hidden through
            // their last parameter, so a `Signature` never shows.
            rule(
                &format!(
                    r#"(?i)(?:\b|\\n)(?:proxy-)?authorization{SEP}(?:(?P<q>\\*)"|\\*'?)(?:(?P<dg>digest)[ \t]+(?P<vd>)|(?P<sp>aws4-hmac-sha256)[ \t]+(?P<v>{AUTH_PARAM}(?:[ \t]*,[ \t]*{AUTH_PARAM})*)|(?:(?P<s>basic|bearer|token|negotiate|ntlm|apikey)[ \t]+)?(?P<vt>[^\s"'\\\],;]+))"#
                ),
                |c, v| {
                    (c.name("dg").is_some()
                        || c.name("sp").is_some()
                        || c.name("s").is_some()
                        || v.len() >= 16)
                        .then_some("authorization")
                },
            ),
        ),
        (
            COOKIE,
            // Every cookie in the header is hidden, with its name: the value runs to the closing
            // quote when the header is quoted, to the closing bracket of a list (a JSON array, a
            // Go header map, `[]string{...}`), else to the end of the line. Also right after an
            // escaped `\n`, as in a request dumped into a JSON string.
            rule(
                &format!(
                    r#"(?i)(?:\b|\\n)(?:set-)?cookie{SEP_NO_LIST}(?:(?P<b>\[\]string\{{|\[)(?P<vb>)|(?P<q>\\*)"(?P<vq>)|\\*(?P<a>')?(?P<vl>[^\s"'\\]))"#
                ),
                |_, _| Some("cookie"),
            ),
        ),
        (
            BEARER,
            rule(
                r#"(?i)\bbearer[ \t]+(?P<v>[A-Za-z0-9\-._~+/]+=*)(?P<f>["'][^\s,;:})\]"']?)?"#,
                |_, v| {
                    // "bearer authentication", "Bearer token": words, not tokens. A short token
                    // is taken as one when it holds a digit.
                    let word = v.bytes().all(|b| b.is_ascii_alphabetic()) && v.len() < 20;
                    let digit = v.bytes().any(|b| b.is_ascii_digit());
                    ((v.len() >= 8 || digit) && !word).then_some("bearer")
                },
            ),
        ),
        (
            URL_PASSWORD,
            // User info inside the authority: the password runs to the authority's last `@`,
            // so one holding `@` is hidden whole, and an `@` in the path or query is not used.
            rule(
                r#"\b[A-Za-z][A-Za-z0-9+.\-]*://[^\s:/?#@"'<>\[\]]*:(?P<v>[^\s/?#"'<>]+)@[^\s/?#@"'<>]*"#,
                |_, _| Some("password"),
            ),
        ),
        (
            URL_PASSWORD_LOOSE,
            // A password with an unencoded `/`, `?` or `#` splits the URL early. Taken as one
            // only when the part after the colon is not a port (`:8443/`).
            rule(
                r#"\b[A-Za-z][A-Za-z0-9+.\-]*://[^\s:/?#@"'<>\[\]]*:(?P<v>[^\s"'<>]+)@[^\s/@"'<>]*"#,
                |_, v| {
                    let split = v.contains(['/', '?', '#']);
                    let digits = v.bytes().take_while(u8::is_ascii_digit).count();
                    let port =
                        digits > 0 && matches!(v.as_bytes().get(digits), Some(b'/' | b'?' | b'#'));
                    (split && !port).then_some("password")
                },
            ),
        ),
        (
            MYSQL_DSN,
            // Go's MySQL DSN, which has no `://`: `user:pass@tcp(host:3306)/db`. The match is the
            // `@` before the protocol and the rest of the DSN; [`dsn_password`] finds the
            // password before it, from the delimiter that ends the DSN.
            rule(
                r#"(?P<vm>)@(?:tcp6?|udp|unix)\([^\s"'\\)]*\)?[^\s"'\\]*"#,
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
                    r#"(?i)(?:aws_?)?secret_?access_?key{SEP}\\*["']?(?P<v>[A-Za-z0-9/+=]{{16,}})"#
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
                r"(?:^|[^A-Za-z0-9])(?P<v>gl(?:pat|rt|dt)-[A-Za-z0-9_\-]{20,})",
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
            VENDOR,
            // Stripe live secret and restricted keys, OpenAI project keys, Anthropic keys.
            rule(
                r"(?:^|[^A-Za-z0-9])(?P<v>[sr]k_live_[A-Za-z0-9]{16,}|sk-(?:proj|ant)-[A-Za-z0-9_\-]{20,})",
                |_, v| {
                    Some(if v.starts_with("sk-ant-") {
                        "anthropic-key"
                    } else if v.starts_with("sk-proj-") {
                        "openai-key"
                    } else {
                        "stripe-key"
                    })
                },
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
            REGISTRY_AUTH,
            // A docker config's `"auth": "<base64 user:pass>"`, as JSON at any escape level, and
            // the same key in YAML or a Python dict; only a value that decodes to `user:pass`.
            rule(
                &format!(r#"(?i)(?:^|[^A-Za-z0-9_.\-])auth{SEP}{VALUE}"#),
                |_, v| is_basic_auth(v).then_some("registry-auth"),
            ),
        ),
        (
            FLAG,
            // `--password hunter2`: a command-line flag with its value after a space.
            rule(
                &format!(
                    r#"(?i)(?:^|[ \t])(?P<k>--?(?:{FLAG_KEYS}))[ \t]+(?:(?P<q>\\*)"(?P<vq>)|'(?P<v>[^'\n]*)'|(?P<vt>[^\s\-"'][^\s"']*))"#
                ),
                |c, v| (!v.is_empty()).then(|| key_kind(c)),
            ),
        ),
        (
            PAIR,
            rule(
                // `key: value` with a space after the colon and no quote (YAML, headers,
                // prose) takes the rest of the line, so a value of several words is hidden
                // whole; `key=value`, `key:value` and quoted keys take one word.
                &format!(
                    r#"(?i)(?:^|[^A-Za-z0-9_.\-])(?P<k>{KEYS})(?:[ \t]*:[ \t]+(?P<vl>[^\s"'\\=:>])|{SEP}{VALUE})"#
                ),
                |c, v| {
                    let key = c.name("k").map_or("", |k| k.as_str()).to_ascii_lowercase();
                    // A working directory (`PWD=/app`), or a credentials file path
                    // (`GOOGLE_APPLICATION_CREDENTIALS=/path`).
                    let path_key = key == "pwd"
                        || key == "oldpwd"
                        || key.ends_with("credential")
                        || key.ends_with("credentials");
                    // A count in a test summary (`pass=1 failed=0`): only a bare `pass` key, so
                    // `DB_PASS=123456` is still hidden.
                    let short_key = key == "pass";
                    // A private key's BEGIN line (the block itself is redacted whole), values
                    // that are not values, values already masked by the workload, paths and
                    // counts.
                    if v.is_empty()
                        || v.starts_with("-----")
                        || v.bytes().all(|b| b == b'*')
                        || NOT_VALUES.iter().any(|w| v.eq_ignore_ascii_case(w))
                        || path_key && v.starts_with(['/', '~'])
                        || short_key && v.bytes().all(|b| b.is_ascii_digit())
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
    if key.contains("pass") || key.ends_with("pwd") {
        "password"
    } else if key.contains("token") {
        "token"
    } else if key.contains("secret") || key.contains("credential") {
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

/// Whether `value` is base64 of `user:password`, as a docker config's `auth` holds.
fn is_basic_auth(value: &str) -> bool {
    use base64::Engine as _;
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    const BASE64: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    value.len() >= 4
        && BASE64
            .decode(value)
            .is_ok_and(|raw| raw.len() >= 2 && raw.contains(&b':'))
}

/// Authentication schemes that can open a value; the credential is the word after, and the
/// two are hidden together.
const SCHEMES: &[&str] = &["bearer", "basic"];

static FLAG_ONLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)^(?P<k>--?(?:{FLAG_KEYS}))$")).expect("flag rule builds")
});

/// When `arg` is a command-line flag naming a credential with no value of its own
/// (`--password`), the marker kind for the argument after it. For argument lists, such as a
/// container's `args`, where the value is the next element.
pub fn credential_flag(arg: &str) -> Option<&'static str> {
    FLAG_ONLY.captures(arg).map(|c| key_kind(&c))
}

/// Runs the line rules over the lines of `text[range]` that hold a trigger word.
fn redact_lines(text: &str, range: Range<usize>, out: &mut Builder) {
    if range.is_empty() {
        return;
    }
    let bytes = text.as_bytes();
    let input = aho_corasick::Input::new(text).span(range.clone());
    // The line being collected: its range and the rules its triggers start.
    let mut line: Option<(Range<usize>, u32)> = None;
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

fn redact_line(text: &str, range: Range<usize>, mask: u32, out: &mut Builder) {
    let mut line = Cow::Borrowed(&text[range.clone()]);
    let mut count = 0;
    for (bit, rule) in RULES.iter() {
        if mask & bit != 0
            && let Some(changed) = apply(rule, &line, &mut count)
        {
            line = Cow::Owned(changed);
        }
    }
    if let Cow::Owned(line) = line {
        out.replace(range, &line);
        out.marks.push((out.written() - line.len(), count));
        out.count += count;
    }
}

/// The byte range of a rule's value in `line` (see [`Rule`]).
fn value_range(caps: &Captures, line: &str, floor: usize) -> Option<Range<usize>> {
    let whole = caps.get(0)?;
    if let Some(v) = caps.name("v") {
        // `Bearer password="x"`: what looked like a token is a key, and its quoted value goes
        // with it.
        if let Some(f) = caps
            .name("f")
            .filter(|f| f.len() == 2 && v.as_str().ends_with('='))
        {
            let quote = line.as_bytes()[f.start()];
            let end = memchr::memchr(quote, &line.as_bytes()[f.start() + 1..])
                .map_or(line.len(), |i| f.start() + 1 + i);
            return Some(v.start()..end);
        }
        return Some(v.range());
    }
    if let Some(v) = caps.name("vm") {
        return dsn_password(line, floor, v.start(), whole.end());
    }
    if let Some(v) = caps.name("vt") {
        return Some(word_value(line, v.range()));
    }
    if let Some(v) = caps.name("vl") {
        // Only a password runs to the end of the line (a passphrase); after `token: ` or
        // `secret: ` the rest is more often an error message than part of the value.
        // The first word decides: a count after a bare `pass` (`pass: 3 fail: 0`), a working
        // directory (`PWD: /app`) and a private key's BEGIN line are one word, which the pair
        // rule leaves alone.
        let key = caps.name("k");
        let name = key.map_or(String::new(), |k| k.as_str().to_ascii_lowercase());
        let first = &line[v.start()..];
        let one_word = (name == "pass" && first.starts_with(|c: char| c.is_ascii_digit()))
            || (matches!(name.as_str(), "pwd" | "oldpwd") && first.starts_with(['/', '~']))
            || key.is_some() && first.starts_with("-----");
        if one_word || key.is_some() && key_kind(caps) != "password" {
            return Some(word_value(line, v.start()..word_end(line, v.start())));
        }
        // A header in single quotes (`curl -H 'Cookie: a=b' url`) ends at the closing quote.
        let at = key.map_or(whole.start(), |k| k.start());
        let quoted = at > 0 && line.as_bytes()[at - 1] == b'\'' || caps.name("a").is_some();
        let len = line_value_len(&line[v.start()..], quoted);
        return Some(v.start()..v.start() + len);
    }
    if let Some(v) = caps.name("vb") {
        let close = if caps.name("b").is_some_and(|b| b.len() > 1) {
            b'}'
        } else {
            b']'
        };
        let rest = &line[v.start()..];
        let items = list_items(rest, close);
        return Some(v.start() + items.start..v.start() + items.end);
    }
    if let Some(v) = caps.name("vd") {
        let end = match caps.name("q") {
            Some(q) => v.start() + quoted_len(&line[v.start()..], q.len()),
            // After an escaped `\n` the header is a line inside a JSON string: it ends at the
            // next escaped `\n` or the string's closing quote.
            None if whole.as_str().starts_with('\\') => {
                v.start() + line_value_len(&line[v.start()..], false)
            }
            None => line.len(),
        };
        return Some(v.start()..end);
    }
    let v = caps.name("vq")?;
    let level = caps.name("q").map_or(0, |q| q.len());
    Some(v.start()..v.start() + quoted_len(&line[v.start()..], level))
}

/// The password of a MySQL DSN whose `@` before the protocol is at `at` and whose text ends at
/// `end`, searching no earlier than `floor`: from the first `:` of the DSN to `at`. What ends
/// the DSN tells what opened it: a double quote or apostrophe (with as many backslashes, in
/// escaped JSON) opens it at the last such quote before `at`, so the password can hold spaces,
/// brackets, apostrophes and more escaped quotes; anything else, the last space. `None` when
/// there is no `:` (no password) or the password already holds a marker (a URL rule hid it).
fn dsn_password(line: &str, floor: usize, at: usize, end: usize) -> Option<Range<usize>> {
    let bytes = line.as_bytes();
    let escapes = bytes[end..].iter().take_while(|&&b| b == b'\\').count();
    let start = match bytes.get(end + escapes) {
        Some(&quote @ (b'"' | b'\'')) => opening_quote(&bytes[floor..at], quote, escapes)
            .map(|i| floor + i + 1)
            .unwrap_or(floor),
        _ => bytes[floor..at]
            .iter()
            .rposition(u8::is_ascii_whitespace)
            .map_or(floor, |i| floor + i + 1),
    };
    // `"dsn": root:pw@tcp(...)`: the separator after a quoted key is not the DSN's.
    let dsn = line[start..at].trim_start_matches([' ', '\t', ':', '=']);
    let start = at - dsn.len();
    let colon = start + memchr::memchr(b':', dsn.as_bytes())?;
    let password = colon + 1..at;
    (!password.is_empty() && !line[password.clone()].contains(MARKER_PREFIX)).then_some(password)
}

/// Where the last `quote` in `bytes` with exactly `escapes` backslashes before it is (an even
/// number when `escapes` is 0): the one that opens a string closed by the same quote. One
/// backward scan.
fn opening_quote(bytes: &[u8], quote: u8, escapes: usize) -> Option<usize> {
    let mut to = bytes.len();
    while let Some(i) = memchr::memrchr(quote, &bytes[..to]) {
        let backslashes = bytes[..i].iter().rev().take_while(|&&b| b == b'\\').count();
        let opens = if escapes == 0 {
            backslashes % 2 == 0
        } else {
            backslashes == escapes
        };
        if opens {
            return Some(i);
        }
        to = i;
    }
    None
}

/// The part of a list that starts `rest` to hide: from inside the first item's opening quote
/// to the last item's closing quote when the items are quoted (a JSON array, `[]string{...}`),
/// so one marker stays a quoted item and the list still parses; else every item.
fn list_items(rest: &str, close: u8) -> Range<usize> {
    let bytes = rest.as_bytes();
    let len = list_len(rest, close);
    let first = bytes[..len]
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(len);
    let escapes = bytes[first..len]
        .iter()
        .take_while(|&&b| b == b'\\')
        .count();
    let last = bytes[..len]
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    let quoted = bytes.get(first + escapes) == Some(&b'"')
        && last > first + escapes + 1
        && bytes[last - 1] == b'"'
        && bytes[..last - 1]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count()
            == escapes;
    if quoted {
        first + escapes + 1..last - 1 - escapes
    } else {
        0..len
    }
}

/// The length of a list's items that start `rest`, up to the `close` byte that ends the list:
/// one outside quoted items, so a `]` inside a cookie's value does not end it. Quotes count at
/// the escape level of the first item (`"a"`, or `\"a\"` in escaped JSON). With no `close`
/// the list runs to the end. One forward scan.
fn list_len(rest: &str, close: u8) -> usize {
    let bytes = rest.as_bytes();
    let first = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let lead = bytes[first..].iter().take_while(|&&b| b == b'\\').count();
    let level = if bytes.get(first + lead) == Some(&b'"') {
        lead
    } else {
        0
    };
    let mut quoted = false;
    let mut backslashes = 0;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\\' => {
                backslashes += 1;
                continue;
            }
            b'"' => {
                let toggles = if level == 0 {
                    backslashes % 2 == 0
                } else {
                    backslashes == level
                };
                if toggles {
                    quoted = !quoted;
                }
            }
            _ if b == close && !quoted => return i,
            _ => {}
        }
        backslashes = 0;
    }
    bytes.len()
}

/// The length of a double-quoted value that starts `rest`, up to its closing quote. `level` is
/// how many backslashes escaped the opening quote: 0 in plain JSON, 1 in JSON inside a JSON
/// string, and so on. A quote with fewer backslashes closes the value (an even number at level
/// 0); one with more is part of it. With no closing quote the value runs to the end.
fn quoted_len(rest: &str, level: usize) -> usize {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while let Some(at) = memchr::memchr(b'"', &bytes[i..]) {
        let quote = i + at;
        let backslashes = bytes[..quote]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count();
        let closes = if level == 0 {
            backslashes % 2 == 0
        } else {
            backslashes <= level
        };
        if closes {
            return quote - backslashes;
        }
        i = quote + 1;
    }
    rest.len()
}

/// Where a bare word that starts at `from` ends: at whitespace or a quote.
fn word_end(line: &str, from: usize) -> usize {
    from + line.as_bytes()[from..]
        .iter()
        .take_while(|&&b| !b.is_ascii_whitespace() && !matches!(b, b'"' | b'\'' | b'\\'))
        .count()
}

/// A bare value matched at `word`, without the closing punctuation after it. A scheme
/// (`token=Bearer abc`) is hidden with the word after it, unless that word is another field.
fn word_value(line: &str, word: Range<usize>) -> Range<usize> {
    let trimmed = |range: Range<usize>| {
        range.start
            ..range.start
                + line[range]
                    .trim_end_matches([',', ';', ')', '}', ']'])
                    .len()
    };
    let value = trimmed(word.clone());
    if value.end == word.end
        && SCHEMES
            .iter()
            .any(|s| line[value.clone()].eq_ignore_ascii_case(s))
    {
        let from = value.end
            + line.as_bytes()[value.end..]
                .iter()
                .take_while(|&&b| b == b' ' || b == b'\t')
                .count();
        let token = trimmed(from..word_end(line, from));
        if from > value.end
            && !token.is_empty()
            && !line[token.clone()].starts_with(MARKER_PREFIX)
            && !is_field(line, token.clone())
        {
            return value.start..token.end;
        }
    }
    value
}

/// A credential key and the separator after it, at the start of the text: the start of a
/// field the pair rule hides, with any separator, spacing and quoting it accepts.
static CREDENTIAL_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)^(?:{KEYS}){SEP_NO_LIST}")).expect("field rule builds")
});

/// Whether the word at `word` starts another field rather than continuing a value: a flag
/// (`--verbose`), an assignment (`user=app`), a credential key with any separator the pair
/// rule accepts (`password= "x"`, `secret : x`), or a key whose quoted value follows
/// (`user="x"`, `user: "a"`). Base64 padding (`Zm9v==`) before a closing quote is not one.
fn is_field(line: &str, word: Range<usize>) -> bool {
    let text = &line[word.clone()];
    let after = &line.as_bytes()[word.end..];
    let opens_quote = matches!(after.first(), Some(b'"' | b'\''))
        && after
            .get(1)
            .is_some_and(|b| !b.is_ascii_whitespace() && !matches!(b, b',' | b';' | b'}' | b')'));
    text.starts_with('-')
        || CREDENTIAL_FIELD.is_match(&line[word.start..])
        || text.trim_end_matches('=').contains('=')
        || text.ends_with(':')
        || (text.ends_with('=') && opens_quote)
}

/// The length of an unquoted value that starts `rest` and runs to the end of the line: up to
/// the first unescaped double quote (the end of the string the line is quoted in), an escaped
/// `\n` (the end of a line inside a JSON string), or, when `apostrophe` (a single-quoted
/// header), an apostrophe; without trailing spaces. An escaped quote (`\"`) is part of the
/// value, and so is a quoted value after `=` (a cookie's `session="v"`, or a field the value
/// runs into, `pass="x"`, also escaped). One forward scan.
fn line_value_len(rest: &str, apostrophe: bool) -> usize {
    let bytes = rest.as_bytes();
    let opens_value = |at: usize| at > 0 && bytes[at - 1] == b'=';
    let mut i = 0;
    let end = loop {
        let Some(at) = memchr::memchr3(b'"', b'\\', b'\'', &bytes[i..]).map(|at| i + at) else {
            break bytes.len();
        };
        match bytes[at] {
            b'\'' if apostrophe => break at,
            b'\'' => i = at + 1,
            b'"' if opens_value(at) => match memchr::memchr(b'"', &bytes[at + 1..]) {
                Some(close) => i = at + 1 + close + 1,
                None => break bytes.len(),
            },
            b'"' => break at,
            _ => match bytes.get(at + 1) {
                Some(b'n') => break at,
                Some(b'"') if opens_value(at) => {
                    match memchr::memmem::find(&bytes[at + 2..], b"\\\"") {
                        Some(close) => i = at + 2 + close + 2,
                        None => break bytes.len(),
                    }
                }
                // Any other escape pair, `\"` and `\\` among them, is part of the value.
                _ => i = (at + 2).min(bytes.len()),
            },
        }
    };
    rest[..end].trim_end().len()
}

/// Applies one rule to a line. Returns the new line, or `None` when nothing was replaced.
fn apply(rule: &Rule, line: &str, count: &mut usize) -> Option<String> {
    let mut out = String::new();
    let mut done = 0;
    let mut at = 0;
    while at <= line.len() {
        let Some(caps) = rule.regex.captures_at(line, at) else {
            break;
        };
        let whole = caps.get(0).expect("group 0");
        let mut next = whole.end().max(whole.start() + 1);
        if let Some(mut value) = value_range(&caps, line, at) {
            // A value can run past the match (to its closing quote, the end of the line, or
            // the word after a scheme); the next match starts after it. A rest-of-line value
            // that is not hidden (`pass: 3 fail: 0 token=x`) is searched again from its start.
            let rest_of_line = caps.name("vl").is_some();
            next = next.max(value.end);
            if rest_of_line {
                // A passphrase whose first words an earlier rule hid: the rest is hidden too.
                while line[value.clone()].starts_with(MARKER_PREFIX)
                    && let Some(close) = line[value.clone()].find(']')
                {
                    let rest = &line[value.start + close + 1..value.end];
                    value.start = value.end - rest.trim_start().len();
                }
            }
            let text = &line[value.clone()];
            // A marker from an earlier rule (a separator may have taken its `[`).
            let before = value.start.saturating_sub(1);
            let marked = line.as_bytes()[before..].starts_with(MARKER_PREFIX.as_bytes())
                || text.starts_with(MARKER_PREFIX);
            if !marked
                && !text.is_empty()
                && let Some(kind) = (rule.kind)(&caps, text)
            {
                out.push_str(&line[done..value.start]);
                out.push_str(MARKER_PREFIX);
                out.push_str(kind);
                out.push(']');
                done = value.end;
                *count += 1;
            } else if rest_of_line {
                // Only a value the rule rejects whole is searched again (a mask, `null`), and
                // the first word decides paths, counts and key headers (see `value_range`), so
                // no long rest of a line is scanned twice.
                next = whole.end().max(whole.start() + 1);
            }
        }
        at = next;
        while at < line.len() && !line.is_char_boundary(at) {
            at += 1;
        }
    }
    (done > 0).then(|| {
        out.push_str(&line[done..]);
        out
    })
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "text_baseline_tests.rs"]
mod baseline;

#[cfg(test)]
#[path = "text_diff_tests.rs"]
mod diff_tests;
