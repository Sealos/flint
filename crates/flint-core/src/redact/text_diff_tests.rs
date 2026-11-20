//! The redactor never hides less than it did: every byte the baseline (master before #224,
//! [`super::baseline`]) replaced in an input is inside a range the current redactor replaces.
//! The corpus is every string literal in the unit tests, plus each credential shape in each
//! wrapper a log line puts it in.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use super::baseline;
use super::redact_credentials;

static MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[REDACTED:[a-z0-9-]+\]").expect("marker regex"));

/// The byte ranges of `input` that were replaced to give `output` (a private key marker can
/// replace nothing); markers with nothing between them are one range. The text between markers
/// is found at its first place after the previous one, and the text after the last at the end.
fn replaced(input: &str, output: &str) -> Vec<Range<usize>> {
    // A multi-line private key body becomes the marker on a line of its own.
    let output = output.replace("\n[REDACTED:private-key]\n", "[REDACTED:private-key]");
    let output = output.as_str();
    let mut literals: Vec<&str> = Vec::new();
    let mut last = 0;
    for m in MARKER.find_iter(output) {
        literals.push(&output[last..m.start()]);
        last = m.end();
    }
    literals.push(&output[last..]);
    if literals.len() == 1 {
        return Vec::new();
    }
    assert!(
        input.starts_with(literals[0]),
        "{input:?} -> {output:?}: no prefix"
    );
    let mut ranges = Vec::new();
    let mut pos = literals[0].len();
    let count = literals.len();
    for (i, literal) in literals.iter().enumerate().skip(1) {
        if literal.is_empty() && i + 1 < count {
            continue;
        }
        let end = if i + 1 == count {
            assert!(
                input.ends_with(literal) && input.len() - literal.len() >= pos,
                "{input:?} -> {output:?}: no suffix"
            );
            input.len() - literal.len()
        } else {
            pos + input[pos..]
                .find(literal)
                .unwrap_or_else(|| panic!("{input:?} -> {output:?}: {literal:?} not found"))
        };
        ranges.push(pos..end);
        pos = end + literal.len();
    }
    ranges
}

/// The baseline bytes the current redactor shows, as text, or `None` when it hides them all.
fn shown(input: &str) -> Option<String> {
    let old = baseline::redact_credentials(input);
    let new = redact_credentials(input);
    let old_ranges = replaced(input, &old.text);
    let new_ranges = replaced(input, &new.text);
    let leaked: Vec<&str> = old_ranges
        .iter()
        .filter(|old| {
            !old.is_empty()
                && !new_ranges
                    .iter()
                    .any(|new| new.start <= old.start && old.end <= new.end)
        })
        .map(|old| &input[old.clone()])
        .collect();
    (!leaked.is_empty()).then(|| format!("{leaked:?}: old {:?}, new {:?}", old.text, new.text))
}

/// The string literals in Rust source: plain (with `\n`, `\t`, `\\`, `\"`, `\'` escapes) and
/// raw (`r"..."`, `r#"..."#`). Enough for the unit tests' inputs.
fn string_literals(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = source[i..].find('\n').map_or(bytes.len(), |n| i + n);
            }
            b'\'' => {
                // A char literal (`'"'`, `'\\''`) or a lifetime: skip it.
                let rest = &bytes[i + 1..];
                i += match rest {
                    [b'\\', _, b'\'', ..] => 4,
                    [_, b'\'', ..] => 3,
                    _ => 1,
                };
            }
            b'r' if matches!(bytes.get(i + 1), Some(b'"' | b'#'))
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) =>
            {
                let hashes = bytes[i + 1..].iter().take_while(|&&b| b == b'#').count();
                let open = i + 1 + hashes;
                if bytes.get(open) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let close = format!("\"{}", "#".repeat(hashes));
                let end = open + 1 + source[open + 1..].find(&close).expect("raw string ends");
                out.push(source[open + 1..end].to_owned());
                i = end + close.len();
            }
            b'"' => {
                let mut text = String::new();
                let mut j = i + 1;
                while bytes[j] != b'"' {
                    if bytes[j] == b'\\' {
                        j += 1;
                        match bytes[j] {
                            b'n' => text.push('\n'),
                            b't' => text.push('\t'),
                            b'\n' => {
                                // A line continuation: the next line's indent is dropped.
                                while bytes[j + 1].is_ascii_whitespace() {
                                    j += 1;
                                }
                            }
                            other => text.push(other as char),
                        }
                        j += 1;
                    } else {
                        let ch = source[j..].chars().next().expect("char");
                        text.push(ch);
                        j += ch.len_utf8();
                    }
                }
                out.push(text);
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// Placeholders the unit tests use for the prefixes secret scanners look for.
fn unfold(text: &str) -> String {
    [
        ("<<begin>>", ["-----", "BEGIN"]),
        ("<<slack>>", ["hooks.", "slack.com"]),
        ("<<aws>>", ["AK", "IA"]),
        ("<<asia>>", ["AS", "IA"]),
        ("<<aiza>>", ["AI", "za"]),
        ("<<gh>>", ["g", "h"]),
        ("<<ghpat>>", ["github", "_pat_"]),
        ("<<gl>>", ["g", "l"]),
        ("<<slk>>", ["xo", "x"]),
        ("<<xapp>>", ["xa", "pp"]),
        ("<<sk>>", ["s", "k"]),
        ("<<rk>>", ["r", "k"]),
    ]
    .iter()
    .fold(text.to_owned(), |text, (placeholder, parts)| {
        text.replace(placeholder, &parts.concat())
    })
}

/// Every string literal in the unit tests that does not already hold a marker.
fn unit_test_inputs() -> Vec<String> {
    string_literals(include_str!("text_tests.rs"))
        .into_iter()
        .map(|text| unfold(&text))
        .filter(|text| !text.contains("[REDACTED"))
        .collect()
}

/// Credential shapes, each a whole `key: value` or token as a log writes it.
fn shapes() -> Vec<String> {
    let aws = format!("{}{}", "AK", "IAIOSFODNN7EXAMPLE");
    let github = format!("{}{}", "gh", "p_16C7e42F292c6912E7710c838347Ae178B4a");
    let jwt = format!(
        "{}{}",
        "eyJhbGciOiJIUzI1NiJ9",
        ".eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
    );
    let mut shapes: Vec<String> = [
        "password=hunter2",
        "password: correct horse",
        "password=hun]ter2",
        "password=hun}ter2",
        "password=it's",
        "password: it's mine",
        "pass=123456",
        "pass: 3",
        "DB_PASS=hunter2",
        "token=abc123",
        "token: Bearer abc",
        "token=Bearer abcdef12==",
        "secret: s3cr3t",
        "api_key=k-123",
        "--password hunter2",
        "Authorization: Bearer abcdef1234567890",
        "Authorization: Basic dXNlcjpwYXNz",
        "Authorization: Digest username=\"a\", response=\"6629fae4\"",
        "Bearer abcdef12==",
        "Bearer ab12",
        "postgres://app:p4ss@db:5432/app",
        "redis://:pa/ss@redis:6379",
        "root:pw@tcp(db:3306)/app",
        "Cookie: sid=abc123; theme=dark",
        "Cookie: session=\"v4lue\"; theme=dark",
        "Set-Cookie: sid=abc123; Path=/",
        "\"auth\":\"dXNlcjpwYXNz\"",
        "client_secret=abc123&grant_type=x",
        "pass=\"123 example phrase\"",
        "pass: 3 fail: 0 token=example",
        "Cookie: session=\"example-value\"; theme=dark",
        "app:it's private@tcp(db)/app",
        "Set-Cookie: a=1; Path=/x]y",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    shapes.extend([aws, github, jwt]);
    shapes
}

/// `text` escaped as the inside of a JSON string.
fn json_escaped(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Each shape in each wrapper a log line can put it in.
fn generated_inputs() -> Vec<String> {
    let shapes = shapes();
    let mut out = Vec::new();
    for shape in &shapes {
        let json = json_escaped(shape);
        let escaped = json_escaped(&json);
        out.extend([
            shape.clone(),
            format!("level=info {shape} done"),
            format!("\"{shape}\""),
            format!("'{shape}'"),
            format!("curl -H '{shape}' https://example.com/api -v"),
            format!("{{\"msg\":\"{json}\",\"level\":\"info\"}}"),
            format!("{{\\\"msg\\\":\\\"{escaped}\\\",\\\"level\\\":\\\"info\\\"}}"),
            format!("[\"{json}\"]"),
            format!("[\"{json}\", \"x\"]"),
            format!("x={shape}"),
            format!("pass: 3 {shape}"),
            format!("pass: 3 fail: 0 {shape}"),
            format!("{shape} {shape}"),
            format!("{{\"msg\":\"GET /\\n{json}\\nHost: x\"}}"),
        ]);
    }
    for a in &shapes {
        for b in &shapes {
            out.push(format!("{a} {b}"));
        }
    }
    out
}

#[test]
fn the_redactor_hides_everything_the_baseline_hid() {
    let mut failures = Vec::new();
    for input in unit_test_inputs().iter().chain(generated_inputs().iter()) {
        // A line at a time, as logs are mostly redacted, and the whole text.
        for line in input.lines().chain([input.as_str()]) {
            if let Some(leak) = shown(line) {
                failures.push(format!("{line:?}\n    {leak}"));
            }
        }
    }
    failures.sort();
    failures.dedup();
    assert!(
        failures.is_empty(),
        "{} inputs show text the baseline hid:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn replaced_ranges_are_recovered_from_the_output() {
    assert_eq!(
        replaced("a=hunter2 b=x", "a=[REDACTED:password] b=x"),
        vec![2..9]
    );
    assert_eq!(replaced("x=1 y=2", "x=[REDACTED:a]"), vec![2..7]);
    assert_eq!(
        replaced("k=ab cd", "k=[REDACTED:a][REDACTED:b] cd"),
        vec![2..4]
    );
    assert!(replaced("plain", "plain").is_empty());
    assert_eq!(
        string_literals(r##"("a\"b", r#"c"d"#, 'x', "e\n")"##),
        vec!["a\"b", "c\"d", "e\n"]
    );
}
