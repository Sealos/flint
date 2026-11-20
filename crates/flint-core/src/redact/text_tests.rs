//! Unit tests for [`super::redact_credentials`]: each rule in several real-world shapes, lines
//! that must stay as they are, and the throughput over a large synthetic log.

use super::*;

/// Redacts `text`, checks the count, and returns the text.
fn redacted(text: &str, count: usize) -> String {
    let out = redact_credentials(text);
    assert_eq!(out.count, count, "count for {text:?} -> {:?}", out.text);
    out.text.into_owned()
}

fn same(text: &str) {
    let out = redact_credentials(text);
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
        redacted("using key AKIAIOSFODNN7EXAMPLE for s3", 1),
        "using key [REDACTED:aws-access-key-id] for s3"
    );
    assert_eq!(
        redacted("AWS_ACCESS_KEY_ID=ASIAY34FZKBOKMUTVV7A", 1),
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
            "ci,AKIAIOSFODNN7EXAMPLE,wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            2
        ),
        "ci,[REDACTED:aws-access-key-id],[REDACTED:aws-secret-access-key]"
    );
    // A git SHA next to a key id is lowercase hex, not a secret.
    assert_eq!(
        redacted(
            "AKIAIOSFODNN7EXAMPLE commit 3f786850e387550fdab836ed7e6dc881de23001b",
            1
        ),
        "[REDACTED:aws-access-key-id] commit 3f786850e387550fdab836ed7e6dc881de23001b"
    );
    // Too short, or a word that only starts like a key id.
    same("AKIA1234 is not a key, nor is Asia/Tokyo or ASIAN");
}

#[test]
fn vendor_tokens() {
    // GitLab and Slack tokens are built at run time so the source holds nothing that
    // secret scanners would flag as a real credential.
    let gitlab = format!("remote: {}-xxxxXXXX1234567890yy rejected", "glpat");
    let slack_bot = format!(
        "SLACK_TOKEN {}-123456789012-1234567890123-AbCdEfGhIjKlMnOp",
        "xoxb"
    );
    let slack_user = format!("{}-123456789012-123456789012-123456789012-abcdef", "xoxp");
    for (line, kind) in [
        (
            "token ghp_16C7e42F292c6912E7710c838347Ae178B4a in env",
            "github-token",
        ),
        ("gho_16C7e42F292c6912E7710c838347Ae178B4a", "github-token"),
        ("ghs_16C7e42F292c6912E7710c838347Ae178B4a", "github-token"),
        ("ghu_16C7e42F292c6912E7710c838347Ae178B4a", "github-token"),
        ("ghr_16C7e42F292c6912E7710c838347Ae178B4a", "github-token"),
        (
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ",
            "github-token",
        ),
        (gitlab.as_str(), "gitlab-token"),
        (slack_bot.as_str(), "slack-token"),
        (slack_user.as_str(), "slack-token"),
        ("xoxa-2-123456789012-abcdefghij", "slack-token"),
        (
            "app token xapp-1-A0123456789-1234567890123-abcdef0123",
            "slack-token",
        ),
        (
            "maps key AIzaSyDdI0hCZtE6vySjMm-WEfRq3CPzqKqqsHI ok",
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
        "posting to https://hooks.slack.com/services/[REDACTED:slack-webhook] done"
    );
    assert_eq!(
        redacted(
            "GET https://acct.blob.core.windows.net/c/b?sv=2022-11-02&se=2026-01-01&sp=r&sig=AbC%2Bd%3D 200",
            1
        ),
        "GET https://acct.blob.core.windows.net/c/b?sv=2022-11-02&se=2026-01-01&sp=r&sig=[REDACTED:azure-sas-signature] 200"
    );
    same("pushed ghp_short and xoxb- and the glpat- docs");
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
    let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nq83kd0==\n-----END RSA PRIVATE KEY-----";
    assert_eq!(
        redacted(&format!("loaded key:\n{pem}\nready\n"), 1),
        "loaded key:\n-----BEGIN RSA PRIVATE KEY-----\n[REDACTED:private-key]\n-----END RSA PRIVATE KEY-----\nready\n"
    );
    // Escaped inside JSON, as a service account key file is logged.
    assert_eq!(
        redacted(
            r#"{"private_key": "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----\n"}"#,
            1
        ),
        r#"{"private_key": "-----BEGIN PRIVATE KEY-----[REDACTED:private-key]-----END PRIVATE KEY-----\n"}"#
    );
    for kind in ["EC", "OPENSSH", "ENCRYPTED", "DSA"] {
        let text =
            format!("-----BEGIN {kind} PRIVATE KEY-----\nabc\n-----END {kind} PRIVATE KEY-----");
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
        redacted("-----BEGIN PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nq83", 1),
        "-----BEGIN PRIVATE KEY-----\n[REDACTED:private-key]\n"
    );
    // Lines around a block still get the line rules.
    assert_eq!(
        redacted(&format!("password=x1\n{pem}\ntoken: y2"), 3),
        "password=[REDACTED:password]\n-----BEGIN RSA PRIVATE KEY-----\n[REDACTED:private-key]\n-----END RSA PRIVATE KEY-----\ntoken: [REDACTED:token]"
    );
    same("-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\n-----END CERTIFICATE-----");
    same("-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYI\n-----END PUBLIC KEY-----");
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
            "aws_key_AKIAIOSFODNN7EXAMPLE",
            "aws_key_[REDACTED:aws-access-key-id]",
        ),
        (
            "GH_TOKEN_ghp_16C7e42F292c6912E7710c838347Ae178B4a",
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
}

#[test]
fn multi_byte_text_around_values_is_kept() {
    assert_eq!(
        redacted("überprüft: password=hunter2 ✓ — token: 秘密", 2),
        "überprüft: password=[REDACTED:password] ✓ — token: [REDACTED:token]"
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
        "WARN using AWS key AKIAIOSFODNN7EXAMPLE",
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
    out
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
    assert!(mb / secs > 20.0, "{:.1} MB/s", mb / secs);
}
