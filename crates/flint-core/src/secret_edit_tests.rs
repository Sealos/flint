use serde_json::{Value, json};

use super::*;
use crate::KindId;

const OLD: &str = "b2xkLXNlY3JldA=="; // old-secret
const KEEP: &str = "a2VlcC1tZQ=="; // keep-me
const NEW: &str = "bmV3LXNlY3JldA=="; // new-secret

fn target() -> WriteTarget {
    WriteTarget {
        cluster: "dev".into(),
        server: Some("https://example.invalid:6443".to_owned()),
        kind: KindId::new("", "secrets"),
        namespace: Some("demo".to_owned()),
        name: "db".to_owned(),
        uid: Some("uid-1".to_owned()),
        resource_version: Some("10".to_owned()),
    }
}

fn live() -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": "db",
            "namespace": "demo",
            "uid": "uid-1",
            "resourceVersion": "10",
            "creationTimestamp": "2026-01-01T00:00:00Z",
            "managedFields": [{"manager": "kubectl"}],
            "annotations": {"last-applied": "{\"data\":{\"password\":\"b2xk\"}}"},
        },
        "type": "Opaque",
        "data": {"password": OLD, "user": KEEP},
    })
}

/// The editor's text as parsed: the live object masked, without managed fields, at its version.
fn opened() -> Value {
    let mut edited = live();
    redact::mask_secret(&mut edited, MASK);
    edited["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("managedFields");
    edited
}

fn text_of(value: &Value) -> String {
    value.to_string()
}

fn refusal(edited: &Value) -> String {
    match patch(edited, &live(), &target()) {
        Err(WriteError::InvalidYaml(why)) => why,
        other => panic!("not refused: {other:?}"),
    }
}

fn assert_no_value(text: &str) {
    for value in [OLD, KEEP, NEW, "old-secret", "keep-me", "b2xk", MASK] {
        assert!(!text.contains(value), "{value} in {text}");
    }
}

#[test]
fn an_untouched_edit_patches_only_the_preconditions() {
    let patch = patch(&opened(), &live(), &target()).unwrap();
    assert_eq!(
        patch,
        json!({"metadata": {"uid": "uid-1", "resourceVersion": "10"}})
    );
}

#[test]
fn an_edited_value_is_sent_and_untouched_masks_are_not() {
    let mut edited = opened();
    edited["data"]["password"] = NEW.into();
    let patch = patch(&edited, &live(), &target()).unwrap();
    assert_eq!(
        patch,
        json!({
            "metadata": {"uid": "uid-1", "resourceVersion": "10"},
            "data": {"password": NEW},
        })
    );
    let text = text_of(&patch);
    assert!(!text.contains(MASK), "{text}");
    assert!(
        !text.contains(KEEP),
        "the untouched value is not sent: {text}"
    );
    assert!(!text.contains("last-applied"), "{text}");
}

#[test]
fn a_removed_key_is_removed() {
    let mut edited = opened();
    edited["data"].as_object_mut().unwrap().remove("user");
    edited["metadata"]["annotations"]
        .as_object_mut()
        .unwrap()
        .remove("last-applied");
    let patch = patch(&edited, &live(), &target()).unwrap();
    assert_eq!(patch["data"], json!({"user": null}));
    assert_eq!(
        patch["metadata"]["annotations"],
        json!({"last-applied": null})
    );
}

#[test]
fn a_new_key_and_plain_text_under_string_data_are_sent() {
    let mut edited = opened();
    edited["data"]["token"] = NEW.into();
    edited["stringData"] = json!({"url": "postgres://db"});
    let patch = patch(&edited, &live(), &target()).unwrap();
    assert_eq!(patch["data"], json!({"token": NEW}));
    assert_eq!(patch["stringData"], json!({"url": "postgres://db"}));
}

#[test]
fn a_mask_for_a_key_the_cluster_does_not_have_is_refused() {
    for (field, key) in [("data", "token"), ("stringData", "password")] {
        let mut edited = opened();
        edited[field] = json!({ key: MASK });
        let why = refusal(&edited);
        assert!(why.contains(field) && why.contains(key), "{why}");
        assert_no_value(&why);
    }
    // Renamed, the mask has nothing to keep.
    let mut edited = opened();
    let map = edited["data"].as_object_mut().unwrap();
    map.remove("password");
    map.insert("pass".to_owned(), MASK.into());
    assert!(refusal(&edited).contains("\"pass\""));
    // A new annotation still masked.
    let mut edited = opened();
    edited["metadata"]["annotations"]["copied"] = MASK.into();
    assert!(refusal(&edited).contains("copied"));
}

#[test]
fn a_placeholder_or_a_partial_mask_is_refused() {
    for typed in [
        HIDDEN_VALUE.to_owned(),
        HIDDEN_CHANGED.to_owned(),
        format!("x{MASK}"),
        // A bullet deleted by accident is still not a value.
        MASK.chars().skip(1).collect::<String>(),
    ] {
        let mut edited = opened();
        edited["stringData"] = json!({ "password": typed });
        let why = refusal(&edited);
        assert!(why.contains("password"), "{why}");
        assert_no_value(&why);
    }
}

#[test]
fn a_changed_data_value_must_be_base64() {
    let mut edited = opened();
    edited["data"]["password"] = "new-secret".into();
    let why = refusal(&edited);
    assert!(
        why.contains("base64") && why.contains("stringData"),
        "{why}"
    );
    assert!(!why.contains("new-secret"), "{why}");
    // Line breaks in a long value are not part of it.
    let mut edited = opened();
    edited["data"]["password"] = "bmV3LXNl\nY3JldA==".into();
    assert!(patch(&edited, &live(), &target()).is_ok());
}

#[test]
fn a_value_that_is_not_a_string_is_refused() {
    for value in [json!(5432), Value::Null, json!({"a": 1})] {
        let mut edited = opened();
        edited["stringData"] = json!({ "port": value });
        let why = refusal(&edited);
        assert!(why.contains("port") && why.contains("string"), "{why}");
    }
    let mut edited = opened();
    edited["data"] = json!(["x"]);
    assert!(refusal(&edited).contains("map"));
}

#[test]
fn a_mask_left_in_another_field_is_refused() {
    let mut edited = opened();
    edited["metadata"]["labels"] = json!({ "app": MASK });
    refusal(&edited);
}

#[test]
fn a_patch_is_bound_to_the_reviewed_object() {
    let mut unbound = target();
    unbound.resource_version = None;
    assert_eq!(
        patch(&opened(), &live(), &unbound).unwrap_err(),
        WriteError::Conflict
    );
    let mut edited = opened();
    edited["data"]["password"] = NEW.into();
    let mut moved = target();
    moved.resource_version = Some("11".to_owned());
    let patch = patch(&edited, &live(), &moved).unwrap();
    assert_eq!(
        patch["metadata"]["resourceVersion"], "11",
        "the precondition is the reviewed version, which the server checks"
    );
}

#[test]
fn live_values_fill_masks_for_comparison_only() {
    let mut edited = opened();
    edited["data"]["password"] = NEW.into();
    edited["data"]["token"] = MASK.into();
    let resolved = with_live_values(&edited, &live());
    assert_eq!(resolved["data"]["user"], KEEP);
    assert_eq!(resolved["data"]["password"], NEW);
    assert_eq!(resolved["data"]["token"], MASK, "nothing to fill it with");
}

#[test]
fn removing_the_whole_data_map_removes_every_key() {
    let mut edited = opened();
    edited.as_object_mut().unwrap().remove("data");
    let patch = patch(&edited, &live(), &target()).unwrap();
    assert_eq!(patch["data"], Value::Null);
    assert!(patch.as_object().unwrap().contains_key("data"));
}
