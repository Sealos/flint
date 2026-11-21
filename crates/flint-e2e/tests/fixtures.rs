//! The committed fixtures are safe to publish: no credentials, local paths or Secret values.

use flint_core::redact;
use flint_e2e::fake_api::fixtures::{FIXTURES_DIR, json_files};
use flint_e2e::fake_api::{Fixtures, Gvr, clock};
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
    let definitions: Vec<(&str, &str)> = table["columnDefinitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap(),
                c["format"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    let safe = redact::safe_secret_wire_columns(&definitions);
    for row in table["rows"].as_array().unwrap() {
        assert!(annotations_ok(&row["object"]["metadata"]));
        for (i, cell) in row["cells"].as_array().unwrap().iter().enumerate() {
            assert!(
                safe.get(i).copied().unwrap_or(false) || cell == "<redacted>",
                "column {i}: {cell}"
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
                // Stored as an age before the recording instant, rounded to at least 10 s.
                let (seconds, _) = clock::parse(created).expect("an RFC 3339 timestamp");
                assert!(seconds <= clock::FIXTURE_NOW_SECONDS, "{gvr}: {created}");
                assert_eq!(seconds % 10, 0, "{gvr}: {created}");
            }
            if let Some(rv) = meta["resourceVersion"].as_str() {
                assert_eq!(rv, "1", "{gvr}");
            }
        }
    }
}
