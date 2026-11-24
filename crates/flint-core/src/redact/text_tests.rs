//! Unit tests for [`super::redact_credentials`]: each rule in several real-world shapes, lines
//! that must stay as they are, and the throughput over a large synthetic log.

use super::*;

/// Placeholders for the prefixes secret scanners look for, so no token-shaped literal is in
/// the source: [`unfold`] builds the real text at run time, and [`fold`] turns the parts that
/// survive redaction back into placeholders, so expected outputs can be written with them.
const PLACEHOLDERS: &[(&str, &[&str])] = &[
    ("<<begin>>", &["-----", "BEGIN"]),
    ("<<slack>>", &["hooks.", "slack.com"]),
    ("<<aws>>", &["AK", "IA"]),
    ("<<asia>>", &["AS", "IA"]),
    ("<<aiza>>", &["AI", "za"]),
    ("<<gh>>", &["g", "h"]),
    ("<<ghpat>>", &["github", "_pat_"]),
    ("<<gl>>", &["g", "l"]),
    ("<<slk>>", &["xo", "x"]),
    ("<<xapp>>", &["xa", "pp"]),
    ("<<sk>>", &["s", "k"]),
    ("<<rk>>", &["r", "k"]),
];

/// `text` with every placeholder replaced by the text it stands for.
fn unfold(text: &str) -> String {
    PLACEHOLDERS
        .iter()
        .fold(text.to_owned(), |text, (placeholder, parts)| {
            text.replace(placeholder, &parts.concat())
        })
}

/// Redacted output with the delimiters that survive redaction written as placeholders again.
fn fold(text: &str) -> String {
    PLACEHOLDERS[..2]
        .iter()
        .fold(text.to_owned(), |text, (placeholder, parts)| {
            text.replace(&parts.concat(), placeholder)
        })
}

/// Redacts `text` (unfolded), checks the count, and returns the text (folded).
fn redacted(text: &str, count: usize) -> String {
    let text = unfold(text);
    let out = redact_credentials(&text);
    assert_eq!(out.count, count, "count for {text:?} -> {:?}", out.text);
    fold(&out.text)
}

fn same(text: &str) {
    let text = unfold(text);
    let out = redact_credentials(&text);
    assert!(
        matches!(out.text, Cow::Borrowed(_)) && out.count == 0,
        "{text:?} changed to {:?}",
        out.text
    );
}

#[test]
fn authorization_headers_keep_the_scheme() {
    for (line, want) in [
        (
            "GET /api Authorization: Bearer abc.def-123_456",
            "GET /api Authorization: Bearer [REDACTED:authorization]",
        ),
        (
            "authorization: Basic dXNlcjpwYXNzd29yZA==",
            "authorization: Basic [REDACTED:authorization]",
        ),
        (
            r#"{"headers":{"Authorization":"Bearer s3cr3t-t0ken-value"}}"#,
            r#"{"headers":{"Authorization":"Bearer [REDACTED:authorization]"}}"#,
        ),
        (
            "map[Accept:[*/*] Authorization:[Bearer s3cr3t-t0ken-value]]",
            "map[Accept:[*/*] Authorization:[Bearer [REDACTED:authorization]]]",
        ),
        (
            "Proxy-Authorization: Basic Zm9vOmJhcg==",
            "Proxy-Authorization: Basic [REDACTED:authorization]",
        ),
        (
            "authorization=0123456789abcdef0123",
            "authorization=[REDACTED:authorization]",
        ),
    ] {
        assert_eq!(redacted(line, 1), want);
    }
    // Words after "authorization", not values.
    same("authorization failed for user alice");
    same("authorization: denied");
}

#[test]
fn bearer_tokens() {
    assert_eq!(
        redacted("curl -H 'X: bearer AbCdEf0123456789'", 1),
        "curl -H 'X: bearer [REDACTED:bearer]'"
    );
    assert_eq!(
        redacted("using Bearer ya29.a0AfH6SMBx-y_z~1/2+3== for upstream", 1),
        "using Bearer [REDACTED:bearer] for upstream"
    );
    same("missing bearer token in request");
    same("Bearer authentication is enabled");
}

#[test]
fn passwords_in_urls_keep_user_and_host() {
    for (line, want) in [
        (
            "connecting to postgres://app:hunter2@db.prod:5432/app",
            "connecting to postgres://app:[REDACTED:password]@db.prod:5432/app",
        ),
        (
            "REDIS_URL=redis://:p%40ss@redis:6379/0",
            "REDIS_URL=redis://:[REDACTED:password]@redis:6379/0",
        ),
        (
            r#"{"url":"https://deploy:gh-pass_word@git.example.com/repo.git"}"#,
            r#"{"url":"https://deploy:[REDACTED:password]@git.example.com/repo.git"}"#,
        ),
        (
            "amqp://guest:guest@rabbit/ and mongodb+srv://u:p4ss@cluster0.x.net",
            "amqp://guest:[REDACTED:password]@rabbit/ and mongodb+srv://u:[REDACTED:password]@cluster0.x.net",
        ),
    ] {
        let n = want.matches("[REDACTED").count();
        assert_eq!(redacted(line, n), want);
    }
    same("GET https://example.com:8443/api/v1/pods?watch=1 200");
    same("ssh://git@github.com/org/repo.git");
    same("http://[::1]:8080/healthz");
}

#[test]
fn aws_keys() {
    assert_eq!(
        redacted("using key <<aws>>IOSFODNN7EXAMPLE for s3", 1),
        "using key [REDACTED:aws-access-key-id] for s3"
    );
    assert_eq!(
        redacted("AWS_ACCESS_KEY_ID=<<asia>>Y34FZKBOKMUTVV7A", 1),
        "AWS_ACCESS_KEY_ID=[REDACTED:aws-access-key-id]"
    );
    for (line, want) in [
        (
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "aws_secret_access_key = [REDACTED:aws-secret-access-key]",
        ),
        (
            r#"{"SecretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"}"#,
            r#"{"SecretAccessKey": "[REDACTED:aws-secret-access-key]"}"#,
        ),
        (
            "AWS_SECRET_ACCESS_KEY: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "AWS_SECRET_ACCESS_KEY: [REDACTED:aws-secret-access-key]",
        ),
    ] {
        assert_eq!(redacted(line, 1), want);
    }
    // A secret next to its key id, as in a credentials CSV.
    assert_eq!(
        redacted(
            "ci,<<aws>>IOSFODNN7EXAMPLE,wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            2
        ),
        "ci,[REDACTED:aws-access-key-id],[REDACTED:aws-secret-access-key]"
    );
    // A git SHA next to a key id is lowercase hex, not a secret.
    assert_eq!(
        redacted(
            "<<aws>>IOSFODNN7EXAMPLE commit 3f786850e387550fdab836ed7e6dc881de23001b",
            1
        ),
        "[REDACTED:aws-access-key-id] commit 3f786850e387550fdab836ed7e6dc881de23001b"
    );
    // Too short, or a word that only starts like a key id.
    same("<<aws>>1234 is not a key, nor is Asia/Tokyo or <<asia>>N");
}

#[test]
fn vendor_tokens() {
    // GitLab and Slack tokens are built at run time so the source holds nothing that
    // secret scanners would flag as a real credential.
    let gitlab = format!("remote: {}-xxxxXXXX1234567890yy rejected", "<<gl>>pat");
    let slack_bot = format!(
        "SLACK_TOKEN {}-123456789012-1234567890123-AbCdEfGhIjKlMnOp",
        "<<slk>>b"
    );
    let slack_user = format!(
        "{}-123456789012-123456789012-123456789012-abcdef",
        "<<slk>>p"
    );
    for (line, kind) in [
        (
            "token <<gh>>p_16C7e42F292c6912E7710c838347Ae178B4a in env",
            "github-token",
        ),
        (
            "<<gh>>o_16C7e42F292c6912E7710c838347Ae178B4a",
            "github-token",
        ),
        (
            "<<gh>>s_16C7e42F292c6912E7710c838347Ae178B4a",
            "github-token",
        ),
        (
            "<<gh>>u_16C7e42F292c6912E7710c838347Ae178B4a",
            "github-token",
        ),
        (
            "<<gh>>r_16C7e42F292c6912E7710c838347Ae178B4a",
            "github-token",
        ),
        (
            "<<ghpat>>11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ",
            "github-token",
        ),
        (gitlab.as_str(), "gitlab-token"),
        (slack_bot.as_str(), "slack-token"),
        (slack_user.as_str(), "slack-token"),
        ("<<slk>>a-2-123456789012-abcdefghij", "slack-token"),
        (
            "app token <<xapp>>-1-A0123456789-1234567890123-abcdef0123",
            "slack-token",
        ),
        (
            "maps key <<aiza>>SyDdI0hCZtE6vySjMm-WEfRq3CPzqKqqsHI ok",
            "gcp-api-key",
        ),
    ] {
        let out = redacted(line, 1);
        assert!(
            out.contains(&format!("[REDACTED:{kind}]")),
            "{line} -> {out}"
        );
    }
    assert_eq!(
        redacted(
            &format!(
                "posting to https://hooks.{}/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX done",
                "slack.com"
            ),
            1
        ),
        "posting to https://<<slack>>/services/[REDACTED:slack-webhook] done"
    );
    assert_eq!(
        redacted(
            "GET https://acct.blob.core.windows.net/c/b?sv=2022-11-02&se=2026-01-01&sp=r&sig=AbC%2Bd%3D 200",
            1
        ),
        "GET https://acct.blob.core.windows.net/c/b?sv=2022-11-02&se=2026-01-01&sp=r&sig=[REDACTED:azure-sas-signature] 200"
    );
    same("pushed <<gh>>p_short and <<slk>>b- and the <<gl>>pat- docs");
}

#[test]
fn jwts() {
    let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    assert_eq!(
        redacted(&format!("id_token={jwt}"), 1),
        "id_token=[REDACTED:jwt]"
    );
    assert_eq!(
        redacted(&format!("validated {jwt} for alice"), 1),
        "validated [REDACTED:jwt] for alice"
    );
    assert_eq!(
        redacted(&format!(r#"{{"jwt":"{jwt}"}}"#), 1),
        r#"{"jwt":"[REDACTED:jwt]"}"#
    );
    // A service account token without a signature segment.
    assert_eq!(
        redacted("eyJhbGciOiJub25lIn0.eyJzdWIiOiJ4In0. end", 1),
        "[REDACTED:jwt] end"
    );
    same("eyJ is how base64 JSON starts");
}

#[test]
fn private_key_blocks() {
    let pem =
        "<<begin>> RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nq83kd0==\n-----END RSA PRIVATE KEY-----";
    assert_eq!(
        redacted(&format!("loaded key:\n{pem}\nready\n"), 1),
        "loaded key:\n<<begin>> RSA PRIVATE KEY-----\n[REDACTED:private-key]\n-----END RSA PRIVATE KEY-----\nready\n"
    );
    // Escaped inside JSON, as a service account key file is logged.
    assert_eq!(
        redacted(
            r#"{"private_key": "<<begin>> PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----\n"}"#,
            1
        ),
        r#"{"private_key": "<<begin>> PRIVATE KEY-----[REDACTED:private-key]-----END PRIVATE KEY-----\n"}"#
    );
    for kind in ["EC", "OPENSSH", "ENCRYPTED", "DSA"] {
        let text =
            format!("<<begin>> {kind} PRIVATE KEY-----\nabc\n-----END {kind} PRIVATE KEY-----");
        assert!(
            redacted(&text, 1).contains("[REDACTED:private-key]"),
            "{kind}"
        );
    }
    // Cut off by a log tail: the END survives but the BEGIN does not.
    assert_eq!(
        redacted(
            "MIIEowIBAAKCAQEA\nq83kd0==\n-----END PRIVATE KEY-----\nnext",
            1
        ),
        "\n[REDACTED:private-key]\n-----END PRIVATE KEY-----\nnext"
    );
    // Cut off at the end.
    assert_eq!(
        redacted("<<begin>> PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nq83", 1),
        "<<begin>> PRIVATE KEY-----\n[REDACTED:private-key]\n"
    );
    // Lines around a block still get the line rules.
    assert_eq!(
        redacted(&format!("password=x1\n{pem}\ntoken: y2"), 3),
        "password=[REDACTED:password]\n<<begin>> RSA PRIVATE KEY-----\n[REDACTED:private-key]\n-----END RSA PRIVATE KEY-----\ntoken: [REDACTED:token]"
    );
    same("<<begin>> CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\n-----END CERTIFICATE-----");
    same("<<begin>> PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYI\n-----END PUBLIC KEY-----");
}

#[test]
fn private_key_markers_can_share_dashes_with_other_pem_markers() {
    assert_eq!(
        redacted(
            "<<begin>> CERTIFICATE-----BEGIN PGP PRIVATE KEY BLOCK-----body-----END PRIVATE KEY-----end",
            1
        ),
        "<<begin>> CERTIFICATE<<begin>> PGP PRIVATE KEY BLOCK-----[REDACTED:private-key]-----END PRIVATE KEY-----end"
    );
}

#[test]
fn key_value_pairs_hide_only_the_value() {
    for (line, want) in [
        ("password=hunter2", "password=[REDACTED:password]"),
        (
            "db_password = hunter2 user=app",
            "db_password = [REDACTED:password] user=app",
        ),
        ("PASSWD:hunter2", "PASSWD:[REDACTED:password]"),
        (
            r#"{"password":"correct horse battery"}"#,
            r#"{"password":"[REDACTED:password]"}"#,
        ),
        (
            r#"{"password": "x", "user": "a"}"#,
            r#"{"password": "[REDACTED:password]", "user": "a"}"#,
        ),
        (
            "password: 'single quoted'",
            "password: '[REDACTED:password]'",
        ),
        (
            r#"msg="{\"password\":\"hunter2\"}""#,
            r#"msg="{\"password\":\"[REDACTED:password]\"}""#,
        ),
        // A bare value runs to whitespace, so the rest of a query string goes with it.
        (
            "client_secret=abc123&grant_type=x",
            "client_secret=[REDACTED:secret]",
        ),
        ("secret: s3cr3t", "secret: [REDACTED:secret]"),
        ("access_token=abc.def", "access_token=[REDACTED:token]"),
        ("X-Auth-Token: 0f1e2d3c", "X-Auth-Token: [REDACTED:token]"),
        ("api_key=k-123", "api_key=[REDACTED:key]"),
        ("apikey: k-123", "apikey: [REDACTED:key]"),
        ("x-api-key: k-123", "x-api-key: [REDACTED:key]"),
        ("--api-key=k-123", "--api-key=[REDACTED:key]"),
        (
            "DefaultEndpointsProtocol=https;AccountName=a;AccountKey=bXlrZXk=;EndpointSuffix=core",
            "DefaultEndpointsProtocol=https;AccountName=a;AccountKey=[REDACTED:key]",
        ),
        ("password=\"unterminated", "password=\"[REDACTED:password]"),
        (
            "level=info token=abc password=def",
            "level=info token=[REDACTED:token] password=[REDACTED:password]",
        ),
    ] {
        let n = want.matches("[REDACTED").count();
        assert_eq!(redacted(line, n), want, "{line}");
    }
}

#[test]
fn values_with_punctuation_and_other_separators_are_hidden_whole() {
    for (line, want) in [
        ("password=Xy7&kL9!z", "password=[REDACTED:password]"),
        ("password=abc,def;ghi", "password=[REDACTED:password]"),
        (
            "{Password:p(a)ss User:x}",
            "{Password:[REDACTED:password] User:x}",
        ),
        ("map[password:hunter2]", "map[password:[REDACTED:password]]"),
        (
            r#"{"password":123,"user":"a"}"#,
            r#"{"password":[REDACTED:password],"user":"a"}"#,
        ),
        (
            r#"{"password"=>"hunter2"}"#,
            r#"{"password"=>"[REDACTED:password]"}"#,
        ),
        (
            r#"{:password=>"hunter2"}"#,
            r#"{:password=>"[REDACTED:password]"}"#,
        ),
        (
            r#"password := "hunter2""#,
            r#"password := "[REDACTED:password]""#,
        ),
        (
            "login?password%3Dhunter2",
            "login?password%3D[REDACTED:password]",
        ),
        (
            r#"{\\\"password\\\":\\\"hunter2\\\"}"#,
            r#"{\\\"password\\\":\\\"[REDACTED:password]\\\"}"#,
        ),
        (
            r#""password":"ab\"cd""#,
            r#""password":"[REDACTED:password]""#,
        ),
        (
            "mysql -u root --password hunter2",
            "mysql -u root --password [REDACTED:password]",
        ),
        (
            "cli --api-key abc123 --verbose",
            "cli --api-key [REDACTED:key] --verbose",
        ),
        (
            "postgres://user:p@ss@db:5432/app",
            "postgres://user:[REDACTED:password]@db:5432/app",
        ),
        (
            "redis://:pa/ss@redis:6379",
            "redis://:[REDACTED:password]@redis:6379",
        ),
        (
            "amqp://guest:gu#est@rabbit",
            "amqp://guest:[REDACTED:password]@rabbit",
        ),
        (
            "aws_key_<<aws>>IOSFODNN7EXAMPLE",
            "aws_key_[REDACTED:aws-access-key-id]",
        ),
        (
            "GH_TOKEN_<<gh>>p_16C7e42F292c6912E7710c838347Ae178B4a",
            "GH_TOKEN_[REDACTED:github-token]",
        ),
    ] {
        let n = want.matches("[REDACTED").count();
        assert_eq!(redacted(line, n), want, "{line}");
    }
    // Masks other tools wrote, and YAML block scalars, are not values.
    for line in [
        "password=[REDACTED]",
        "password: ********",
        "password: |",
        "token=<redacted>",
        "--password-stdin",
        "docker login --password-stdin < file",
    ] {
        same(line);
    }
}

#[test]
fn ordinary_lines_stay_intact() {
    for line in [
        "2026-11-20T10:15:30.123456789Z INFO request id=3fa85f64-5717-4562-b3fc-2c963f66afa6 done",
        "commit 3f786850e387550fdab836ed7e6dc881de23001b merged",
        "trace_id=4bf92f3577b34da6a3ce929d0e0e4736 span_id=00f067aa0bc902b7",
        "img data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
        "GET https://api.example.com/v1/items?page=2&limit=50 200 12ms",
        "llm call finished token_count=5 prompt_tokens=120",
        "password reset email sent to user 42",
        "Password changed successfully",
        "watch resourceVersion=123456789 kind=Pod",
        "I1120 10:15:30.123456       1 reflector.go:351] Caches populated for *v1.Secret",
        "MountVolume.SetUp failed for volume \"certs\" : secret \"tls\" not found",
        "Successfully assigned demo/web-69c6f74b8b-qv4xv to node-1",
        "keyboard layout loaded; monkey patching disabled",
        "secretName: web-tls",
        "tokens remaining: 0",
        "the authorization header is missing",
        "",
    ] {
        same(line);
    }
}

#[test]
fn values_already_redacted_are_not_counted_again() {
    let once = redact_credentials("password=hunter2 Authorization: Bearer abcdefgh12345678");
    assert_eq!(once.count, 2);
    let twice = redact_credentials(&once.text);
    assert_eq!(twice.count, 0, "{}", twice.text);
    assert_eq!(twice.text, once.text);

    for line in [
        "password: correct horse battery",
        "Cookie: session=abc123",
        "root:hunter2@tcp(db:3306)/app",
        r#"{"auth":"dXNlcjpwYXNz"}"#,
        "token: Bearer abc",
    ] {
        let once = redact_credentials(line);
        assert_eq!(once.count, 1, "{line} -> {}", once.text);
        let twice = redact_credentials(&once.text);
        assert_eq!((twice.count, &twice.text), (0, &once.text), "{line}");
    }
}

#[test]
fn multi_byte_text_around_values_is_kept() {
    assert_eq!(
        redacted("überprüft: password=hunter2 ✓ — token: 秘密", 2),
        "überprüft: password=[REDACTED:password] ✓ — token: [REDACTED:token]"
    );
}

#[test]
fn parameterized_authorization_values_are_hidden_to_their_last_parameter() {
    let digest = r#"Authorization: Digest username="alice", realm="api", nonce="dcd98b7102dd2f0e", uri="/x", qop=auth, nc=00000001, cnonce="0a4f113b", response="6629fae49393a05397450978507c4ef1" next=1"#;
    // Unquoted, a Digest value runs to the end of the line.
    assert_eq!(
        redacted(digest, 1),
        "Authorization: Digest [REDACTED:authorization]"
    );
    for line in [
        // An escaped quote inside a parameter.
        r#"Authorization: Digest username="a\"b", response="6629fae49393a05397450978507c4ef1""#,
        // Parameters separated by spaces, not commas.
        r#"Authorization: Digest username="a" realm="api" response="6629fae49393a05397450978507c4ef1""#,
    ] {
        assert_eq!(
            redacted(line, 1),
            "Authorization: Digest [REDACTED:authorization]",
            "{line}"
        );
    }
    // Quoted in JSON, with an escaped quote in a parameter: up to the header's closing quote.
    let json =
        r#"{"Authorization":"Digest username=\"a\\\"b\" response=\"6629fae4\"","Accept":"*/*"}"#;
    assert_eq!(
        redacted(json, 1),
        r#"{"Authorization":"Digest [REDACTED:authorization]","Accept":"*/*"}"#
    );
    // Built at run time: a SigV4 header names an access key id.
    let sigv4 = format!(
        "authorization={}4-HMAC-SHA256 Credential={}/20261120/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=fe5f80f77d5fa3beca038a248ff027d0445342fe2855ddc963176630326f1024 status=403",
        "AWS", "<<aws>>0123456789ABCDEF"
    );
    let out = redacted(&sigv4, 1);
    assert_eq!(
        out,
        "authorization=AWS4-HMAC-SHA256 [REDACTED:authorization] status=403"
    );
    // Inside JSON, with escaped quotes around the parameters.
    let json = r#"{"Authorization":"Digest username=\"a\", response=\"6629fae49393a05397450978507c4ef1\"","Accept":"*/*"}"#;
    let out = redacted(json, 1);
    assert!(!out.contains("6629fae4"), "{out}");
    assert!(out.ends_with(r#","Accept":"*/*"}"#), "{out}");
}

#[test]
fn quoted_values_run_to_their_closing_quote_at_any_escape_level() {
    for (line, want) in [
        // JSON inside a JSON string, with a quote inside the password.
        (
            r#"{"log":"{\"password\":\"ab\\\"cd\",\"user\":\"x\"}"}"#,
            r#"{"log":"{\"password\":\"[REDACTED:password]\",\"user\":\"x\"}"}"#,
        ),
        (
            r#"{"password":"ab\"cd","token":"t1"}"#,
            r#"{"password":"[REDACTED:password]","token":"[REDACTED:token]"}"#,
        ),
        // Punctuation inside quotes is part of the value.
        (
            r#"password="abc);" next"#,
            r#"password="[REDACTED:password]" next"#,
        ),
        (r#"password=")]""#, r#"password="[REDACTED:password]""#),
        // Unterminated: the rest of the line.
        (r#"secret="abc def"#, r#"secret="[REDACTED:secret]"#),
    ] {
        let n = want.matches("[REDACTED").count();
        assert_eq!(redacted(line, n), want, "{line}");
    }
}

#[test]
fn an_at_sign_after_the_authority_is_not_a_password() {
    for line in [
        "https://api.example:8443/users/alice@example.com",
        "GET https://example.com:8443/path?email=a@b.com 200",
        "https://example.com/a:b@c",
    ] {
        same(line);
    }
    assert_eq!(
        redacted(
            "redis://:pa/ss@h:6379 and https://u:pw@host/users/a@b.com",
            2
        ),
        "redis://:[REDACTED:password]@h:6379 and https://u:[REDACTED:password]@host/users/a@b.com"
    );
}

#[test]
fn prose_that_names_a_private_key_footer_is_kept() {
    same("invalid PEM: missing -----END PRIVATE KEY----- in tls.key");
    same("line one\nexpected -----END RSA PRIVATE KEY-----\nline three");
    // A short word before the footer is not a key.
    same("ok\n-----END PRIVATE KEY-----");
    // A real cut-off block still is, also after ordinary lines.
    assert_eq!(
        redacted(
            "starting\nMIIEowIBAAKCAQEAq83kd0abcdMIIEowIBAAKCAQEAq83kd0abcdMIIEowIBAAKCAQEA\nq83kd0==\n-----END PRIVATE KEY-----\nnext",
            1
        ),
        "starting\n\n[REDACTED:private-key]\n-----END PRIVATE KEY-----\nnext"
    );
    assert_eq!(
        redacted(
            r#"{"k": "MIIEowIBAAKCAQEAq83kd0\nabc\n-----END PRIVATE KEY-----\n"}"#,
            1
        ),
        r#"{"k": "[REDACTED:private-key]-----END PRIVATE KEY-----\n"}"#
    );
}

#[test]
fn redacting_twice_does_not_count_private_keys_again() {
    let pem = unfold("<<begin>> PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----");
    let once = redact_credentials(&pem);
    assert_eq!(once.count, 1);
    let twice = redact_credentials(&once.text);
    assert_eq!((twice.count, twice.text.as_ref()), (0, once.text.as_ref()));
}

#[test]
fn more_credential_key_names() {
    for (line, want) in [
        (
            "secret_key_base: abc123",
            "secret_key_base: [REDACTED:secret]",
        ),
        ("ENCRYPTION_KEY=k1", "ENCRYPTION_KEY=[REDACTED:key]"),
        ("signing_key: k1", "signing_key: [REDACTED:key]"),
        ("master-key=k1", "master-key=[REDACTED:key]"),
        ("ssh_key=k1", "ssh_key=[REDACTED:key]"),
        (
            r#"{"credentials":"c1"}"#,
            r#"{"credentials":"[REDACTED:secret]"}"#,
        ),
        ("DB_PASS=hunter2", "DB_PASS=[REDACTED:password]"),
        ("pwd=hunter2", "pwd=[REDACTED:password]"),
        ("smtp.pass: hunter2", "smtp.pass: [REDACTED:password]"),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    for line in [
        "PWD=/app",
        "HOME=/root PWD=/app SHLVL=1",
        "bypass=true for cache",
        "compass=north",
        "passed=12 failed=0",
        // Counts in a test summary.
        "pass=1 failed=0",
        "PASS: 12",
        // Paths to credentials files and working directories.
        "credentials=/etc/creds.json",
        "GOOGLE_APPLICATION_CREDENTIALS=/var/run/secrets/key.json",
        "aws_credentials: ~/.aws/credentials",
        "OLDPWD=/home/app",
    ] {
        same(line);
    }
    // The path exemption is for PWD alone; a password that starts with `/` is hidden.
    for (line, want) in [
        ("DB_PWD=/abcdef123", "DB_PWD=[REDACTED:password]"),
        ("db_pass=12x45", "db_pass=[REDACTED:password]"),
        // Numbers are exempt only after a bare `pass` key.
        ("DB_PASS=123456", "DB_PASS=[REDACTED:password]"),
        ("MYSQL_PWD=4321", "MYSQL_PWD=[REDACTED:password]"),
        ("user.pwd: 99", "user.pwd: [REDACTED:password]"),
        ("db-pass=7", "db-pass=[REDACTED:password]"),
        ("password=123456", "password=[REDACTED:password]"),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
}

#[test]
fn more_vendor_prefixes() {
    // Built at run time so the source holds no complete key.
    for (prefix, rest, kind) in [
        ("<<sk>>_live_", "4eC39HqLyjWDarjtT1zdp7dc", "stripe-key"),
        ("<<rk>>_live_", "4eC39HqLyjWDarjtT1zdp7dc", "stripe-key"),
        ("<<sk>>-proj-", "abcDEF123456ghiJKL789012mno", "openai-key"),
        (
            "<<sk>>-ant-",
            "api03-abcDEF123456ghiJKL789012",
            "anthropic-key",
        ),
        ("<<gl>>rt-", "abcDEF123456ghiJKL7890", "gitlab-token"),
        ("<<gl>>dt-", "abcDEF123456ghiJKL7890", "gitlab-token"),
    ] {
        let line = format!("using {prefix}{rest} now");
        assert_eq!(
            redacted(&line, 1),
            format!("using [REDACTED:{kind}] now"),
            "{prefix}"
        );
    }
    same("the <<sk>>_live_ prefix and <<sk>>-ant- docs");
}

#[test]
fn credential_flags_name_the_next_argument() {
    for (arg, kind) in [
        ("--password", Some("password")),
        ("-token", Some("token")),
        ("--db-pass", Some("password")),
        ("--api-key", Some("key")),
        ("--client-secret", Some("secret")),
        ("--password-file", None),
        ("--password=x", None),
        ("--verbose", None),
        ("password", None),
    ] {
        assert_eq!(credential_flag(arg), kind, "{arg}");
    }
}

#[test]
fn unquoted_passwords_after_a_colon_run_to_the_end_of_the_line() {
    for (line, want) in [
        (
            "password: correct horse battery",
            "password: [REDACTED:password]",
        ),
        (
            "  db_password:   correct horse battery  ",
            "  db_password:   [REDACTED:password]  ",
        ),
        (
            r#"{"msg":"config password: two words","level":"info"}"#,
            r#"{"msg":"config password: [REDACTED:password]","level":"info"}"#,
        ),
        (
            r#"{"msg":"passwd: two words\nnext line"}"#,
            r#"{"msg":"passwd: [REDACTED:password]\nnext line"}"#,
        ),
        // An escaped quote is part of the value; in escaped JSON the value runs on.
        (
            r#"{"msg":"password: correct \"horse\" battery"}"#,
            r#"{"msg":"password: [REDACTED:password]"}"#,
        ),
        (
            r#"{\"msg\":\"password: two words\"}"#,
            r#"{\"msg\":\"password: [REDACTED:password]"#,
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    // One word after `=`, `:` with no space, a quoted key, or a key other than a password
    // (after `token: ` the rest is more often an error message).
    for (line, want) in [
        (
            "failed to fetch oauth token: unexpected status: 401 Unauthorized",
            "failed to fetch oauth token: [REDACTED:token] status: 401 Unauthorized",
        ),
        ("secret: two words", "secret: [REDACTED:secret] words"),
        (
            "password=hunter2 user=app",
            "password=[REDACTED:password] user=app",
        ),
        (
            "{Password:p(a)ss User:x}",
            "{Password:[REDACTED:password] User:x}",
        ),
        (
            r#"{"password": 123, "user": "a"}"#,
            r#"{"password": [REDACTED:password], "user": "a"}"#,
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    for line in [
        "password: ********",
        "password: |",
        "password: null",
        "pass: 3 fail: 0",
        "password reset: email sent to user 42",
        "secretName: web-tls",
    ] {
        same(line);
    }
}

#[test]
fn cookie_headers() {
    for (line, want) in [
        (
            "Cookie: session=abc123; theme=dark",
            "Cookie: [REDACTED:cookie]",
        ),
        (
            "set-cookie: sid=31d4d96e407aad42; Path=/; Secure; HttpOnly",
            "set-cookie: [REDACTED:cookie]",
        ),
        (
            r#"{"headers":{"Cookie":"session=abc123; theme=dark","Accept":"*/*"}}"#,
            r#"{"headers":{"Cookie":"[REDACTED:cookie]","Accept":"*/*"}}"#,
        ),
        (
            r#"msg="request" cookie="session=abc123; theme=dark" status=200"#,
            r#"msg="request" cookie="[REDACTED:cookie]" status=200"#,
        ),
        (
            "map[Accept:[*/*] Cookie:[sid=abc123; theme=dark] User-Agent:[curl]]",
            "map[Accept:[*/*] Cookie:[[REDACTED:cookie]] User-Agent:[curl]]",
        ),
        (
            r#"{"Set-Cookie":["a=s3cr3tA; Path=/","b=s3cr3tB; Path=/"],"Vary":"Accept"}"#,
            r#"{"Set-Cookie":["[REDACTED:cookie]"],"Vary":"Accept"}"#,
        ),
        (
            r#"http.Header{"Cookie":[]string{"sid=abc123"}}"#,
            r#"http.Header{"Cookie":[]string{"[REDACTED:cookie]"}}"#,
        ),
        // A request dumped into a JSON string, and the same for `Authorization`.
        (
            r#"{"msg":"GET / HTTP/1.1\nHost: x\nCookie: sid=abc123\n"}"#,
            r#"{"msg":"GET / HTTP/1.1\nHost: x\nCookie: [REDACTED:cookie]\n"}"#,
        ),
        (
            r#"{"msg":"GET /\nAuthorization: Bearer abcdefghijkl\n"}"#,
            r#"{"msg":"GET /\nAuthorization: Bearer [REDACTED:authorization]\n"}"#,
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    for line in [
        "cookie consent banner shown",
        "no session cookie in request",
        "cookies: 3",
        "cookie_name=sid",
        "Cookie:",
    ] {
        same(line);
    }
}

#[test]
fn docker_config_auth_values() {
    for (line, want) in [
        (
            r#"{"auths":{"registry.example.com":{"auth":"dXNlcjpwYXNz"}}}"#,
            r#"{"auths":{"registry.example.com":{"auth":"[REDACTED:registry-auth]"}}}"#,
        ),
        (
            r#"config: {\"auths\":{\"r.io\":{\"username\":\"u\",\"auth\": \"dXNlcjpwYXNzd29yZA==\"}}}"#,
            r#"config: {\"auths\":{\"r.io\":{\"username\":\"u\",\"auth\": \"[REDACTED:registry-auth]\"}}}"#,
        ),
        (
            "{'auth': 'dXNlcjpwYXNz'}",
            "{'auth': '[REDACTED:registry-auth]'}",
        ),
        ("auth: dXNlcjpwYXNz", "auth: [REDACTED:registry-auth]"),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    for line in [
        r#"{"auth":"none"}"#,
        r#"{"auth": "oidc", "authority": "https://login.example.com"}"#,
        r#"{"auth":{"type":"basic"}}"#,
        r#"{"auth":true}"#,
        r#"{"author":"dXNlcjpwYXNz"}"#,
        "auth: enabled",
        "oauth=dXNlcjpwYXNz",
    ] {
        same(line);
    }
}

#[test]
fn mysql_dsns_without_a_scheme() {
    for (line, want) in [
        (
            "dsn=root:hunter2@tcp(db:3306)/app?parseTime=true",
            "dsn=root:[REDACTED:password]@tcp(db:3306)/app?parseTime=true",
        ),
        (
            r#"connecting to "app:p@ss:w0rd@tcp(10.0.0.5:3306)/app""#,
            r#"connecting to "app:[REDACTED:password]@tcp(10.0.0.5:3306)/app""#,
        ),
        (
            "app:pw@unix(/var/run/mysqld.sock)/app",
            "app:[REDACTED:password]@unix(/var/run/mysqld.sock)/app",
        ),
        (
            "mysql://root:hunter2@tcp(db:3306)/app",
            "mysql://root:[REDACTED:password]@tcp(db:3306)/app",
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    for line in [
        "dial tcp(10.0.0.5:3306): connection refused",
        "dsn=root@tcp(db:3306)/app",
        "10:15:30 user@tcp(db:3306) connected",
    ] {
        same(line);
    }
}

#[test]
fn short_bearer_tokens() {
    // After a credential key the scheme is hidden with its token, so neither shows.
    for (line, want) in [
        ("token: Bearer abc", "token: [REDACTED:token]"),
        ("token=Bearer abc", "token=[REDACTED:token]"),
        ("X-Auth-Token: bearer xy", "X-Auth-Token: [REDACTED:token]"),
        ("api_key=Basic Zm9v", "api_key=[REDACTED:key]"),
        (
            "using Bearer ab12 upstream",
            "using Bearer [REDACTED:bearer] upstream",
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    for line in [
        "missing bearer token in request",
        "Bearer authentication is enabled",
        "auth scheme Bearer",
    ] {
        same(line);
    }
}

#[test]
fn a_scheme_is_not_continued_by_the_next_field() {
    for (line, want) in [
        ("token=Bearer user=app", "token=[REDACTED:token] user=app"),
        // A credential key after the scheme, with any separator the pair rule accepts.
        (
            r#"token=Basic password= "example""#,
            r#"token=[REDACTED:token] password= "[REDACTED:password]""#,
        ),
        (
            "token: Bearer secret : s3cr3t",
            "token: [REDACTED:token] secret : [REDACTED:secret]",
        ),
        (
            r#"token=Basic api_key:="k-1""#,
            r#"token=[REDACTED:token] api_key:="[REDACTED:key]""#,
        ),
        (
            "auth_token=Basic --verbose",
            "auth_token=[REDACTED:token] --verbose",
        ),
        // What follows `Bearer` looks like a token: it is hidden with its quoted value.
        (
            r#"token=Bearer password="hunter2""#,
            r#"token=[REDACTED:token] [REDACTED:bearer]""#,
        ),
        (
            r#"Bearer password="hunter2""#,
            r#"Bearer [REDACTED:bearer]""#,
        ),
        // A padded token before a closing quote is still the credential.
        (
            r#"["Bearer abcdef12=="]"#,
            r#"["Bearer [REDACTED:bearer]"]"#,
        ),
        (
            r#"msg="token=Basic Zm9vYmFy==" next=1"#,
            r#"msg="token=[REDACTED:token]" next=1"#,
        ),
    ] {
        let n = want.matches("[REDACTED").count();
        assert_eq!(redacted(line, n), want, "{line}");
    }
}

#[test]
fn cookie_lists_end_at_their_closing_bracket_outside_quotes() {
    for (line, want) in [
        (
            r#"{"Set-Cookie":["a=1; Path=/x]y","b=s3cr3tB; Path=/"],"Vary":"Accept"}"#,
            r#"{"Set-Cookie":["[REDACTED:cookie]"],"Vary":"Accept"}"#,
        ),
        (
            r#"{\"Set-Cookie\":[\"a=1; Path=/]\",\"b=s3cr3tB\"],\"Vary\":\"Accept\"}"#,
            r#"{\"Set-Cookie\":[\"[REDACTED:cookie]\"],\"Vary\":\"Accept\"}"#,
        ),
        (
            r#"http.Header{"Cookie":[]string{"a=}b", "sid=s3cr3t"}, "Accept":[]string{"*/*"}}"#,
            r#"http.Header{"Cookie":[]string{"[REDACTED:cookie]"}, "Accept":[]string{"*/*"}}"#,
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
}

#[test]
fn mysql_dsn_passwords_with_spaces_and_brackets() {
    for (line, want) in [
        (
            r#"{"dsn":"app:p w[1]@tcp(db:3306)/app"}"#,
            r#"{"dsn":"app:[REDACTED:password]@tcp(db:3306)/app"}"#,
        ),
        (
            "dsn: 'root:a b@tcp(db)/x' ok",
            "dsn: 'root:[REDACTED:password]@tcp(db)/x' ok",
        ),
        (
            r#""dsn": root:pw@tcp(db)/x"#,
            r#""dsn": root:[REDACTED:password]@tcp(db)/x"#,
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    for line in [
        r#"{"ts":"10:15:30","addr":"user@tcp(db:3306)"}"#,
        "connected at 10:15 to user@tcp(db)",
    ] {
        same(line);
    }
}

#[test]
fn a_passphrase_with_a_hidden_first_word_is_hidden_whole() {
    assert_eq!(
        redacted("password: <<aws>>IOSFODNN7EXAMPLE and more words", 2),
        "password: [REDACTED:aws-access-key-id] [REDACTED:password]"
    );
    same("password: [REDACTED:password]");
}

#[test]
fn digest_after_an_escaped_newline_ends_with_its_header() {
    assert_eq!(
        redacted(
            r#"{"msg":"GET /\nAuthorization: Digest username=\"a\", response=\"6629fae4\"\nHost: x","level":"info"}"#,
            1
        ),
        r#"{"msg":"GET /\nAuthorization: Digest [REDACTED:authorization]\nHost: x","level":"info"}"#
    );
    same(r#"{"msg":"GET /\nHost: x\nAccept: */*"}"#);
}

#[test]
fn single_quoted_headers_end_at_the_closing_quote() {
    for (line, want) in [
        (
            "curl -H 'Cookie: sid=abc123; theme=dark' https://example.com/api",
            "curl -H 'Cookie: [REDACTED:cookie]' https://example.com/api",
        ),
        (
            "curl -H 'X-Password: two words' -d x",
            "curl -H 'X-Password: [REDACTED:password]' -d x",
        ),
        // Not a quoted header: an apostrophe inside the passphrase is part of it.
        ("password: it's a secret", "password: [REDACTED:password]"),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    same("curl -H 'Accept: */*' https://example.com/api");
}

#[test]
fn yaml_auth_with_a_space_before_the_colon() {
    assert_eq!(
        redacted("auth : dXNlcjpwYXNz", 1),
        "auth : [REDACTED:registry-auth]"
    );
    same("auth : enabled");
}

#[test]
fn quoted_cookie_values_inside_a_raw_header() {
    for (line, want) in [
        (
            r#"Cookie: session="example-value"; theme=dark"#,
            "Cookie: [REDACTED:cookie]",
        ),
        (
            r#"{"msg":"Cookie: session=\"v\"; theme=dark","level":"info"}"#,
            r#"{"msg":"Cookie: [REDACTED:cookie]","level":"info"}"#,
        ),
        (
            r#"curl -H 'Cookie: session="v"; theme=dark' https://example.com"#,
            "curl -H 'Cookie: [REDACTED:cookie]' https://example.com",
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    same(r#"{"msg":"cookie consent \"ok\""}"#);
}

#[test]
fn a_pass_count_is_only_a_bare_number() {
    for (line, want) in [
        (
            r#"pass="123 example phrase""#,
            r#"pass="[REDACTED:password]""#,
        ),
        ("pass: 123 abc", "pass: 123 abc"),
        (
            "pass: 3 fail: 0 token=example",
            "pass: 3 fail: 0 token=[REDACTED:token]",
        ),
        (
            "PWD: /app password: two words",
            "PWD: /app password: [REDACTED:password]",
        ),
    ] {
        let n = want.matches("[REDACTED").count();
        assert_eq!(redacted(line, n), want, "{line}");
    }
    for line in ["pass: 3 fail: 0", "pass=1 failed=0", "PWD: /app"] {
        same(line);
    }
}

#[test]
fn a_double_quoted_dsn_with_an_apostrophe_in_its_password() {
    assert_eq!(
        redacted(r#"dsn="app:it's private@tcp(db)/app""#, 1),
        r#"dsn="app:[REDACTED:password]@tcp(db)/app""#
    );
}

#[test]
fn cookie_list_quote_level_skips_leading_spaces() {
    assert_eq!(
        redacted(
            r#"{\"Set-Cookie\":[ \"a=1; Path=/]\",\"b=s3cr3tB\"],\"Vary\":\"Accept\"}"#,
            1
        ),
        r#"{\"Set-Cookie\":[ \"[REDACTED:cookie]\"],\"Vary\":\"Accept\"}"#
    );
}

#[test]
fn a_dsn_opens_where_its_closing_delimiter_says() {
    for (line, want) in [
        (
            "dsn=app:it's-private@tcp(db)/app",
            "dsn=app:[REDACTED:password]@tcp(db)/app",
        ),
        (
            r#"{"dsn":"app:p\"x@tcp(db)/app"}"#,
            r#"{"dsn":"app:[REDACTED:password]@tcp(db)/app"}"#,
        ),
        (
            r#"{\"dsn\":\"app:p\\\"x@tcp(db)/app\"}"#,
            r#"{\"dsn\":\"app:[REDACTED:password]@tcp(db)/app\"}"#,
        ),
        (
            "dsn='app:a\"b@tcp(db)/app' ok",
            "dsn='app:[REDACTED:password]@tcp(db)/app' ok",
        ),
    ] {
        assert_eq!(redacted(line, 1), want, "{line}");
    }
    for line in [
        r#"{"msg":"it's up","addr":"user@tcp(db:3306)"}"#,
        "dial user@tcp(db) it's fine",
    ] {
        same(line);
    }
}

#[test]
fn set_cookie_arrays_stay_valid_json() {
    let one_line =
        r#"{"headers":{"Set-Cookie":["a=s3cr3tA; Path=/x]y","b=s3cr3tB"],"Vary":"Accept"}}"#;
    let pretty = "{\n  \"headers\": {\n    \"Set-Cookie\": [\n      \"a=s3cr3tA; Path=/\",\n      \"b=s3cr3tB; HttpOnly\"\n    ],\n    \"Vary\": \"Accept\"\n  }\n}\n";
    for (text, count) in [(one_line, 1), (pretty, 2)] {
        let out = redact_credentials(text);
        assert_eq!(out.count, count, "{}", out.text);
        assert!(!out.text.contains("s3cr3t"), "{}", out.text);
        let parsed: serde_json::Value = serde_json::from_str(&out.text).expect("still JSON");
        let cookies = parsed["headers"]["Set-Cookie"].as_array().expect("array");
        assert!(
            cookies.iter().all(|c| c == "[REDACTED:cookie]"),
            "{cookies:?}"
        );
        assert_eq!(parsed["headers"]["Vary"], "Accept");
        // Redacting again changes nothing.
        let again = redact_credentials(&out.text);
        assert_eq!((again.count, again.text.as_ref()), (0, out.text.as_ref()));
    }
    // Other arrays, and a cookie array of numbers, are left alone.
    same("{\n  \"Accept\": [\n    \"*/*\"\n  ]\n}");
    same("{\n  \"cookies\": [\n    \"a=1\"\n  ]\n}");
}

#[test]
fn a_single_quoted_cookie_value_ends_at_its_apostrophe() {
    assert_eq!(
        redacted("Cookie='sid=example' status=200", 1),
        "Cookie='[REDACTED:cookie]' status=200"
    );
    same("cookie='' status=200");
}

#[test]
fn a_cookie_starting_with_dashes_is_hidden_whole() {
    assert_eq!(
        redacted("Cookie: -----sid=example; session=sample", 1),
        "Cookie: [REDACTED:cookie]"
    );
    // After a credential key, a value of dashes is still one word, left alone.
    same("password: ----- user=app");
}

#[test]
fn a_cookie_list_with_a_masked_item_hides_the_rest() {
    assert_eq!(
        redacted(r#"{"Set-Cookie":["[REDACTED:cookie]","b=s3cr3tB"]}"#, 1),
        r#"{"Set-Cookie":["[REDACTED:cookie]"]}"#
    );
    assert_eq!(
        redacted(
            r#"{"Set-Cookie":["a=s3cr3tA","[REDACTED:cookie]","c=s3cr3tC"]}"#,
            1
        ),
        r#"{"Set-Cookie":["[REDACTED:cookie]"]}"#
    );
    same(r#"{"Set-Cookie":["[REDACTED:cookie]"]}"#);
    same(r#"{"Set-Cookie":["[REDACTED:cookie]","[REDACTED:cookie]"]}"#);
}

#[test]
fn dsns_with_spaces_in_the_address_and_lists_of_dsns() {
    for (line, want) in [
        (
            r#"{"dsn":"app:p w@unix(/var/run/my sql.sock)/app"}"#,
            r#"{"dsn":"app:[REDACTED:password]@unix(/var/run/my sql.sock)/app"}"#,
        ),
        (
            "dsns=root:a@tcp(db)/x,root:b@tcp(db)/y",
            "dsns=root:[REDACTED:password]@tcp(db)/x,root:[REDACTED:password]@tcp(db)/y",
        ),
        (
            "dsns=root:a@tcp(db)/x, root:b@tcp(db)/y",
            "dsns=root:[REDACTED:password]@tcp(db)/x, root:[REDACTED:password]@tcp(db)/y",
        ),
        (
            r#"{"dsn":root:pw@tcp(db)/app}"#,
            r#"{"dsn":root:[REDACTED:password]@tcp(db)/app}"#,
        ),
        (
            r#""auth":"x" app:p w@unix(/tmp/my sql.sock)/app"#,
            r#""auth":"x" app:[REDACTED:password]@unix(/tmp/my sql.sock)/app"#,
        ),
    ] {
        let n = want.matches("[REDACTED").count();
        assert_eq!(redacted(line, n), want, "{line}");
    }
    for line in [
        r#"{"addr":"user@unix(/var/run/my sql.sock)/app"}"#,
        "dsns=root@tcp(db)/x,root@tcp(db)/y",
        r#"{"dsn":root@tcp(db)/app}"#,
    ] {
        same(line);
    }
}

#[test]
fn cookie_items_ending_in_an_escaped_backslash_stay_valid_json() {
    let compact = r#"{"Set-Cookie":["a=x\\","b=s3cr3tB\\"]}"#;
    let pretty = "{\n  \"Set-Cookie\": [\n    \"a=x\\\\\",\n    \"b=s3cr3tB\\\\\"\n  ]\n}";
    for (text, count) in [(compact, 1), (pretty, 2)] {
        let out = redact_credentials(text);
        assert_eq!(out.count, count, "{}", out.text);
        assert!(!out.text.contains("s3cr3t"), "{}", out.text);
        let parsed: serde_json::Value = serde_json::from_str(&out.text).expect("still JSON");
        let cookies = parsed["Set-Cookie"].as_array().expect("array");
        assert!(
            cookies.iter().all(|c| c == "[REDACTED:cookie]"),
            "{cookies:?}"
        );
    }
}

#[test]
fn only_cookie_headers_are_redacted_as_cookie_arrays() {
    same(
        "{\n  \"trackingCookie\": [\n    \"t=1\"\n  ],\n  \"cookieNames\": [\n    \"sid\"\n  ]\n}",
    );
}

/// About 10 MB of realistic log lines, with a few secrets.
fn synthetic_log() -> String {
    let lines = [
        "2026-11-20T10:15:30.123456789Z INFO  http request method=GET path=/api/v1/items status=200 duration=12ms trace_id=4bf92f3577b34da6a3ce929d0e0e4736",
        "I1120 10:15:30.123456       1 reflector.go:351] Caches populated for *v1.ConfigMap from k8s.io/client-go/informers/factory.go:160",
        r#"{"level":"info","ts":1763633730.123,"logger":"controller","msg":"Reconciling","name":"web","namespace":"demo","resourceVersion":"123456"}"#,
        "10.0.0.12 - - [20/Nov/2026:10:15:30 +0000] \"GET https://api.example.com/v1/items?page=2 HTTP/1.1\" 200 512 \"-\" \"kube-probe/1.31\"",
        "2026-11-20 10:15:30,123 DEBUG cache key=items:page:2 hit=true size=4096",
        "llm call finished token_count=512 prompt_tokens=120 model=small",
        "WARN  retrying upstream connection attempt=2 backoff=200ms",
    ];
    let secrets = [
        "DEBUG outgoing headers map[Authorization:[Bearer s3cr3t-t0ken-value-0123]]",
        "INFO connecting to postgres://app:hunter2@db.prod:5432/app",
        "WARN using AWS key <<aws>>IOSFODNN7EXAMPLE",
    ];
    let mut out = String::with_capacity(10 * 1024 * 1024 + 1024);
    let mut i = 0;
    while out.len() < 10 * 1024 * 1024 {
        let line = if i % 10_000 == 0 {
            secrets[(i / 10_000) % secrets.len()]
        } else {
            lines[i % lines.len()]
        };
        out.push_str(line);
        out.push('\n');
        i += 1;
    }
    unfold(&out)
}

/// True when the memory workflow runs the tests under AddressSanitizer, which slows code by a
/// large factor: the speed limits below are skipped there, every output check still runs.
fn under_sanitizer() -> bool {
    std::env::var("ASAN_OPTIONS").is_ok_and(|options| !options.is_empty())
}

/// Throughput over a 10 MB log. The floor is low so unoptimized builds pass; run with
/// `--release -- --nocapture` for the number that matters.
#[test]
fn ten_megabytes_of_logs_are_fast() {
    let log = synthetic_log();
    // Warm the lazily built searchers outside the timing.
    let _ = redact_credentials("password=x");
    let start = std::time::Instant::now();
    let out = redact_credentials(&log);
    let secs = start.elapsed().as_secs_f64();
    let mb = log.len() as f64 / (1024.0 * 1024.0);
    println!(
        "redacted {mb:.1} MB in {:.1} ms: {:.0} MB/s, {} values",
        secs * 1000.0,
        mb / secs,
        out.count
    );
    assert!(out.count > 0 && !out.text.contains("hunter2"));
    assert!(
        under_sanitizer() || mb / secs > 20.0,
        "{:.1} MB/s",
        mb / secs
    );
}

/// One long line full of quoted values stays linear: each value is scanned once, so a line as
/// long as the largest log fetch (1 MiB) takes milliseconds, not hours.
#[test]
fn long_lines_of_quoted_values_are_linear() {
    const MIB: usize = 1024 * 1024;
    for unit in [
        r#"token="a" "#,
        r#""password":"x","#,
        r#"password: a" "#,
        r#"password: a\n"#,
        r#"token: Bearer a\n"#,
        r#"Cookie: a" "#,
        "a:b@tcp( ",
        r#""auth":"YTpi","#,
        r#"Cookie:["a]","b"] "#,
        r#""a:b c@tcp(" "#,
        r#"\nAuthorization: Digest a"#,
        r#"token=Bearer x="y" "#,
        "-H 'Cookie: a' ",
        r#"-H 'Cookie: a="b"' "#,
        "Cookie='a' ",
        "a:b'c@tcp(x)/y ",
        "a:b@tcp(x)/y,",
        "a:b@unix(/s s ",
        r#""a:b\"c@tcp(x)/y" "#,
    ] {
        let line = unit.repeat(MIB / unit.len());
        let start = std::time::Instant::now();
        let out = redact_credentials(&line);
        let secs = start.elapsed().as_secs_f64();
        println!("{unit}: 1 MiB line in {:.1} ms", secs * 1000.0);
        assert_eq!(out.count, MIB / unit.len(), "{unit}");
        assert!(
            under_sanitizer() || secs < 2.0,
            "{unit}: {secs:.2} s for 1 MiB"
        );
    }
    // Values the pair rule leaves alone are not scanned to the end of the line again and again.
    for unit in [
        "pass: 3 ",
        "PWD: /a ",
        "password: ***** ",
        "password: null ",
    ] {
        let line = unit.repeat(MIB / unit.len());
        let start = std::time::Instant::now();
        redact_credentials(&line);
        let secs = start.elapsed().as_secs_f64();
        println!("{unit}: 1 MiB line in {:.1} ms", secs * 1000.0);
        assert!(
            under_sanitizer() || secs < 2.0,
            "{unit}: {secs:.2} s for 1 MiB"
        );
    }
}
