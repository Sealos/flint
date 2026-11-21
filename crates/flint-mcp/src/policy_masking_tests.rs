//! Masking across whole responses and log tails, with placeholder values only.

use serde_json::json;

use super::*;

const VALUE: &str = "placeholder-value";
const KEY_LINE: &str = "ExampleKeyLine0123456789abcdefExampleKeyLine0123456789abcdefABCD";

fn secret() -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": "s", "annotations": {"note": VALUE}},
        "data": {"k": VALUE},
        "stringData": {"t": VALUE},
    })
}

fn assert_masked(value: &Value) {
    assert!(!value.to_string().contains(VALUE), "{value}");
}

#[test]
fn secrets_nested_in_a_custom_resource_are_masked() {
    let mut bundle = json!({
        "apiVersion": "example.io/v1",
        "kind": "Bundle",
        "metadata": {"name": "b"},
        "spec": {"resources": [secret(), {"deep": {"er": [secret()]}}]},
    });
    sanitize(&mut bundle);
    assert_masked(&bundle);
    assert_eq!(bundle["spec"]["resources"][0]["data"]["k"], REDACTED);
    assert_eq!(bundle["kind"], "Bundle");
}

#[test]
fn secrets_inside_lists_are_masked() {
    let mut list = json!({
        "apiVersion": "v1",
        "kind": "List",
        "items": [
            {"kind": "Wrapper", "spec": {"inner": {"kind": "List", "items": [secret()]}}},
            {"kind": "Wrapper", "spec": {"apiVersion": "v1", "kind": "SecretList",
                "items": [{"data": {"k": VALUE}}, VALUE]}},
        ],
    });
    sanitize(&mut list);
    assert_masked(&list);
}

#[test]
fn a_manifest_without_api_version_is_masked_and_other_groups_are_not() {
    let mut value = json!({"spec": {"a": {"kind": "Secret", "data": {"k": VALUE}}}});
    sanitize(&mut value);
    assert_masked(&value);

    let mut other = json!({"spec": {"a": {
        "apiVersion": "example.io/v1", "kind": "Secret", "data": {"k": "shown"},
    }}});
    sanitize(&mut other);
    assert_eq!(other["spec"]["a"]["data"]["k"], "shown");
}

#[test]
fn malformed_secret_fields_are_masked() {
    let mut value = json!({"spec": {"kind": "Secret", "data": VALUE, "stringData": [VALUE],
        "metadata": {"annotations": VALUE}}});
    sanitize(&mut value);
    assert_masked(&value);
}

#[test]
fn references_to_a_secret_are_kept() {
    let mut value = json!({"spec": {"ref": {"apiVersion": "v1", "kind": "Secret", "name": "s"}}});
    let expected = value.clone();
    sanitize(&mut value);
    assert_eq!(value, expected);
}

#[test]
fn serialized_secrets_in_annotations_are_masked() {
    let wrapper = json!({"kind": "Bundle", "spec": {"resources": [secret()]}});
    let mut value = json!({
        "apiVersion": "example.io/v1",
        "kind": "Bundle",
        "metadata": {"annotations": {
            LAST_APPLIED: format!("{wrapper}\n"),
            "example.io/copy": secret().to_string(),
            "example.io/yaml": format!("apiVersion: v1\nkind: Secret\ndata:\n  k: {VALUE}\n"),
            "example.io/plain": "kind: Secret was not found",
        }},
    });
    sanitize(&mut value);
    assert_masked(&value);
    let annotations = &value["metadata"]["annotations"];
    let applied = annotations[LAST_APPLIED].as_str().unwrap();
    assert!(applied.ends_with('\n'));
    let applied: Value = serde_json::from_str(applied).unwrap();
    assert_eq!(applied["spec"]["resources"][0]["data"]["k"], REDACTED);
    assert_eq!(annotations["example.io/yaml"], REDACTED);
    assert_eq!(
        annotations["example.io/plain"],
        "kind: Secret was not found"
    );
}

#[test]
fn json_nested_past_the_parse_depth_is_still_masked() {
    let mut text = secret().to_string();
    for _ in 0..=MAX_SERIALIZED_DEPTH {
        text = json!({ "inner": text }).to_string();
    }
    let mut value = json!({"spec": {"blob": text}});
    sanitize(&mut value);
    assert_masked(&value);
}

#[test]
fn invalid_json_naming_a_secret_is_replaced_whole() {
    let text = format!("{{\"kind\":\"Secret\",\"data\":{{\"k\":\"{VALUE}\"}}");
    let mut value = json!({"spec": {"blob": text}});
    sanitize(&mut value);
    assert_eq!(value["spec"]["blob"], REDACTED);
}

fn pem(prefix: &str, body_lines: usize) -> String {
    let mut text = format!("{prefix}-----BEGIN PRIVATE KEY-----\n");
    for _ in 0..body_lines {
        text.push_str(&format!("{prefix}{KEY_LINE}\n"));
    }
    text.push_str(&format!("{prefix}-----END PRIVATE KEY-----\n"));
    text
}

#[test]
fn a_block_opening_before_the_tail_is_redacted() {
    for prefix in ["", "2026-01-01T00:00:00Z app | "] {
        let log = format!("before\n{}after\n", pem(prefix, 8));
        let out = log_output(&log, 4, 64 * 1024, false);
        assert!(!out.text.contains(KEY_LINE), "{out:?}");
        assert!(out.truncated);
    }
}

#[test]
fn leading_key_lines_are_left_out_when_the_start_was_not_fetched() {
    for prefix in ["", "2026-01-01T00:00:00Z app | "] {
        let block = pem(prefix, 8);
        let cut = block.split_once('\n').unwrap().1;
        let log = format!("{cut}after\n");
        let out = log_output(&log, 100, 64 * 1024, true);
        assert!(!out.text.contains(KEY_LINE), "{out:?}");
        assert!(out.text.ends_with("after\n"), "{out:?}");
    }
}

#[test]
fn a_body_with_no_end_or_start_is_left_out() {
    let log = format!("{KEY_LINE}\n{KEY_LINE}\n{KEY_LINE}\n");
    let out = log_output(&log, 100, 64 * 1024, true);
    assert_eq!(out.text, "");
    assert!(out.truncated);
    assert!(out.note.unwrap().contains("starts earlier"));
}

#[test]
fn leading_cookie_items_are_left_out_when_the_opener_was_not_fetched() {
    let log = format!("    \"session={VALUE}; Path=/\",\n    \"id={VALUE}\"\n  ],\nnext\n");
    let out = log_output(&log, 100, 64 * 1024, true);
    assert!(!out.text.contains(VALUE), "{out:?}");
    assert!(out.text.ends_with("next\n"));
}

#[test]
fn a_cookie_array_opening_before_the_tail_is_redacted() {
    let log = format!("\"set-cookie\": [\n  \"a={VALUE}\",\n  \"b={VALUE}\"\n],\nnext\n");
    let out = log_output(&log, 3, 64 * 1024, false);
    assert!(!out.text.contains(VALUE), "{out:?}");
}

#[test]
fn ordinary_leading_lines_are_kept() {
    let log = "commit 0123456789abcdef0123456789abcdef01234567\n\
               loaded /var/lib/example/configuration/settings\n\
               plain line\n";
    let out = log_output(log, 100, 64 * 1024, true);
    assert_eq!((out.text.as_str(), out.truncated), (log, false));
}

#[test]
fn a_complete_log_is_not_trimmed() {
    let log = format!("{KEY_LINE}\nplain\n");
    assert_eq!(log_output(&log, 100, 1_024, false).text, log);
}

#[test]
fn the_redacted_count_covers_only_the_returned_tail() {
    let log = "token=one\ntoken=two\nplain\n";
    let out = log_output(log, 2, 1_024, false);
    assert_eq!(out.redacted, 1);
}

#[test]
fn the_fetch_stays_bounded_and_keeps_the_requested_lines() {
    assert_eq!(log_fetch_tail(100), 100 + i64::from(LOG_LOOK_BEHIND) + 1);
    let max = log_fetch_tail(LOG_LINES.max);
    assert_eq!(
        next_log_tail(max, LOG_LINES.max, 10),
        i64::from(LOG_LINES.max) + 1
    );
    assert_eq!(next_log_tail(101, 100, 80), 50);
    assert_eq!(next_log_tail(2, 100, 0), 1);
}

#[test]
fn yaml_with_the_kind_on_the_next_line_is_masked() {
    let text = format!("apiVersion: v1\nkind:\n  Secret\ndata:\n  k: {VALUE}\n");
    let mut value = json!({"spec": {"blob": text}});
    sanitize(&mut value);
    assert_eq!(value["spec"]["blob"], REDACTED);
}

#[test]
fn yaml_structure_decides_what_is_a_secret() {
    for text in [
        format!("base: &k Secret\nkind: *k\ndata:\n  k: {VALUE}\n"),
        format!("---\nkind: ConfigMap\n---\nkind: Secret\nstringData:\n  k: {VALUE}\n"),
        format!("{{kind: Secret, data: {{k: {VALUE}}}}}"),
        format!("kind: Bundle\nresources:\n  - kind: Secret\n    data:\n      k: {VALUE}\n"),
    ] {
        let mut value = json!({"spec": {"blob": text.clone()}});
        sanitize(&mut value);
        assert_eq!(value["spec"]["blob"], REDACTED, "{text}");
    }
    for text in [
        format!("apiVersion: example.io/v1\nkind: Secret\ndata:\n  k: {VALUE}\n"),
        "secretRef:\n  kind: Secret\n  name: s\n".to_owned(),
    ] {
        let mut value = json!({"spec": {"blob": text.clone()}});
        sanitize(&mut value);
        assert_eq!(value["spec"]["blob"], text.as_str());
    }
}

#[test]
fn yaml_past_the_parse_limits_that_names_a_secret_is_replaced_whole() {
    let mut aliases = String::from("a0: &a0 [x, x]\n");
    for i in 1..40 {
        let p = i - 1;
        aliases.push_str(&format!("a{i}: &a{i} [*a{p}, *a{p}, *a{p}, *a{p}]\n"));
    }
    let long = "x".repeat(MAX_YAML_BYTES);
    let deep = "[".repeat(MAX_YAML_DEPTH + 1);
    for text in [
        format!("{aliases}kind: Secret\ndata:\n  k: {VALUE}\n"),
        format!("kind:\n  Secret\ndata:\n  k: {VALUE}\n# {long}\n"),
        format!("{deep}kind: Secret\ndata: {VALUE}\n"),
        format!("kind: Secret\ndata: {VALUE}\n  broken: [\n"),
    ] {
        let mut value = json!({"spec": {"blob": text}});
        sanitize(&mut value);
        assert_eq!(value["spec"]["blob"], REDACTED);
    }
}

#[test]
fn free_text_holding_a_secret_is_masked() {
    for mut text in [
        secret().to_string(),
        format!("apiVersion: v1\nkind:\n  Secret\ndata:\n  k: {VALUE}\n"),
    ] {
        redact_free_text(&mut text);
        assert!(!text.contains(VALUE), "{text}");
    }
    let mut plain = "Back-off pulling image: example.io/app:1.0".to_owned();
    assert_eq!(redact_free_text(&mut plain), 0);
    assert_eq!(plain, "Back-off pulling image: example.io/app:1.0");
}

#[test]
fn a_final_cookie_item_on_the_closing_line_is_left_out() {
    for close in ["]", "],", " ]", ",]"] {
        let log = format!("  \"session={VALUE}; Path=/\",\n  \"id={VALUE}\"{close}\nnext\n");
        let out = log_output(&log, 100, 64 * 1024, true);
        assert_eq!(out.text, "next\n", "{close}");
    }
}

#[test]
fn the_tail_counts_the_lines_of_the_fetched_log() {
    let log = format!("ordinary\n{}one\ntwo\n", pem("", 8));
    let out = log_output(&log, 3, 64 * 1024, false);
    assert_eq!(out.text, "-----END PRIVATE KEY-----\none\ntwo\n");
    assert!(out.truncated);

    let inside = log_output(&log, 5, 64 * 1024, false);
    assert!(!inside.text.contains("BEGIN"), "{inside:?}");
    assert!(!inside.text.contains(KEY_LINE), "{inside:?}");
    assert!(
        inside.text.starts_with("[REDACTED:private-key]\n"),
        "{inside:?}"
    );
    assert!(inside.truncated);

    let whole = log_output(&log, 100, 64 * 1024, false);
    assert!(whole.text.starts_with("ordinary\n") && !whole.truncated);
}

#[test]
fn checksum_logs_are_kept_when_the_start_was_not_fetched() {
    let mut log = String::new();
    for i in 0..6 {
        log.push_str(&format!(
            "2026-01-01T00:00:0{i}Z app verified block {i} checksum {KEY_LINE}\n"
        ));
    }
    let out = log_output(&log, 100, 64 * 1024, true);
    assert_eq!((out.text.as_str(), out.truncated), (log.as_str(), false));

    let digest = "Q2hlY2tzdW1QbGFjZWhvbGRlcjAxMjM0NTY3ODlhYmNk";
    let digests = format!("{digest}\n{digest}\n{digest}\n");
    let out = log_output(&digests, 100, 64 * 1024, true);
    assert_eq!(out.text, digests);
}

#[test]
fn a_cut_off_body_with_a_prefix_and_a_short_last_line_is_left_out() {
    let prefix = "2026-01-01T00:00:00Z app | ";
    let log = format!("{prefix}{KEY_LINE}\n{prefix}{KEY_LINE}\n{prefix}ExampleLastLine0123\n");
    let out = log_output(&log, 100, 64 * 1024, true);
    assert_eq!(out.text, "");
    assert!(out.truncated);
}
