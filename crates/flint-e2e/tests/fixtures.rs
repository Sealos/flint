//! The committed fixtures are safe to publish: no credentials, local paths or Secret values.

use flint_core::redact;
use flint_e2e::fake_api::fixtures::{FIXTURES_DIR, json_files};
use flint_e2e::fake_api::{Fixtures, Gvr};
use flint_e2e::record::normalize::{SECRET_MARKER, find_leaks};
use serde_json::Value;

#[test]
fn no_fixture_holds_credentials_or_local_paths() {
    let files = json_files(FIXTURES_DIR.as_ref()).unwrap();
    assert!(files.len() > 20, "fixtures are recorded");
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let leaks = find_leaks(&text);
        assert!(leaks.is_empty(), "{} contains {leaks:?}", file.display());
    }
}

#[test]
fn secrets_show_keys_but_no_values() {
    let fixtures = Fixtures::load_default().unwrap();
    let secrets = Gvr::core("secrets");
    let annotations_ok = |meta: &Value| {
        meta.get("annotations")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .all(|(k, v)| redact::is_safe_secret_annotation(k) || v == SECRET_MARKER)
    };
    for item in fixtures.objects[&secrets]["items"].as_array().unwrap() {
        for field in ["data", "stringData"] {
            for value in item
                .get(field)
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                assert_eq!(value.1, SECRET_MARKER, "{}", item["metadata"]["name"]);
            }
        }
        assert!(annotations_ok(&item["metadata"]));
    }
    let table = &fixtures.tables[&secrets];
    let columns = table["columnDefinitions"].as_array().unwrap();
    for row in table["rows"].as_array().unwrap() {
        assert!(annotations_ok(&row["object"]["metadata"]));
        for (cell, column) in row["cells"].as_array().unwrap().iter().zip(columns) {
            let name = column["name"].as_str().unwrap();
            assert!(
                redact::is_safe_secret_column(name) || cell == "<redacted>",
                "{name}: {cell}"
            );
        }
    }
}

#[test]
fn uids_and_timestamps_are_normalized() {
    let fixtures = Fixtures::load_default().unwrap();
    for (gvr, table) in &fixtures.tables {
        for row in table["rows"].as_array().into_iter().flatten() {
            let meta = &row["object"]["metadata"];
            if let Some(created) = meta["creationTimestamp"].as_str() {
                assert_eq!(created, "2026-01-01T00:00:00Z", "{gvr}");
            }
            if let Some(rv) = meta["resourceVersion"].as_str() {
                assert_eq!(rv, "1", "{gvr}");
            }
        }
    }
}
