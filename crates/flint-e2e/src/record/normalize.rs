//! Makes recorded responses safe to commit and stable across recordings: Secret values, Helm
//! release ConfigMaps and anything that looks like a credential are scrubbed; uids, resource
//! versions and server-rendered ages are replaced with fixed or derived values; timestamps are
//! stored relative to [`FIXTURE_NOW`], rounded (see [`clock::round_age`]).

use std::collections::BTreeMap;

use flint_core::redact;
use serde_json::{Map, Value};

#[cfg(doc)]
use crate::fake_api::clock::FIXTURE_NOW;
use crate::fake_api::clock::{self, FIXTURE_NOW_SECONDS};

/// Replaces every Secret `data`/`stringData` value. Base64 of `redacted`, so values still decode.
pub const SECRET_MARKER: &str = "cmVkYWN0ZWQ=";

/// Replaces strings that hold a certificate, key or token.
pub const CREDENTIAL_MARKER: &str = "<redacted>";

/// Every resource version becomes this. The fake server sets its own on lists and events.
pub const FIXED_RESOURCE_VERSION: &str = "1";

/// Table columns whose cells are ages rendered by the server at send time.
const AGE_COLUMNS: &[&str] = &[
    "Age",
    "Last Seen",
    "First Seen",
    "Last Schedule",
    "Duration",
];

/// What an `Age` cell becomes. The fake server renders `Age` cells itself from the creation time.
const FIXED_AGE: &str = "1d";

/// Node fields that identify the machine the cluster ran on.
const MACHINE_FIELDS: &[&str] = &["bootID", "machineID", "systemUUID"];

/// Substrings that must never appear in a committed fixture.
const LEAKS: &[&str] = &[
    "/Users/",
    "/home/",
    "BEGIN CERTIFICATE",
    "PRIVATE KEY",
    "LS0tLS1",    // base64 of "-----", a PEM block
    "eyJhbGciOi", // base64 of {"alg":, a JWT
    "H4sI",       // base64 of a gzip stream (Helm releases)
    "MII",        // base64 DER: a certificate or key without PEM armour
    "Bearer ",
];

/// The leak markers found in a fixture's text.
pub fn find_leaks(text: &str) -> Vec<&'static str> {
    LEAKS.iter().copied().filter(|l| text.contains(l)).collect()
}

/// A stable, uuid-shaped id derived from `identity` (FNV-1a with two seeds, each finished with
/// the SplitMix64 mixer so similar names give unrelated ids).
pub fn stable_uid(identity: &str) -> String {
    let mix = |mut z: u64| {
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    let hash = |seed: u64| {
        mix(identity.bytes().fold(seed, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        }))
    };
    let hex = format!(
        "{:016x}{:016x}",
        hash(0xcbf2_9ce4_8422_2325),
        hash(0x8422_2325_cbf2_9ce4)
    );
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Maps the uids of a recording to stable ones derived from what the object is.
#[derive(Debug, Default)]
pub struct UidMap {
    map: BTreeMap<String, String>,
}

impl UidMap {
    /// Registers an object's uid under `identity` (group, resource, namespace and name).
    pub fn register(&mut self, uid: &str, identity: &str) {
        self.map
            .entry(uid.to_owned())
            .or_insert_with(|| stable_uid(identity));
    }

    /// Registers every object in a Table (`rows[].object.metadata`) or plain list (`items[]`).
    pub fn register_all(&mut self, resource: &str, value: &Value) {
        let metas = value
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| row.pointer("/object/metadata"))
            .chain(
                value
                    .get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item.get("metadata")),
            );
        for meta in metas {
            let field = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or("");
            if !field("uid").is_empty() {
                let identity = format!("{resource}/{}/{}", field("namespace"), field("name"));
                self.register(field("uid"), &identity);
            }
        }
    }

    /// The stable replacement of a uid (unknown uids are hashed as themselves).
    fn replace(&self, uid: &str) -> String {
        self.map
            .get(uid)
            .cloned()
            .unwrap_or_else(|| stable_uid(&format!("unknown/{uid}")))
    }

    /// Replaces every uid-shaped substring of `text`.
    fn replace_in(&self, text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        let mut copied = 0;
        while i + 36 <= bytes.len() {
            if is_uuid(&bytes[i..i + 36])
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && bytes.get(i + 36).is_none_or(|b| !b.is_ascii_alphanumeric())
            {
                out.push_str(&text[copied..i]);
                out.push_str(&self.replace(&text[i..i + 36]));
                i += 36;
                copied = i;
            } else {
                i += 1;
            }
        }
        out.push_str(&text[copied..]);
        out
    }

    /// Normalizes every value in `value` (see the module docs).
    pub fn normalize(&self, value: &mut Value) {
        match value {
            Value::String(s) => *s = self.normalize_string(s),
            Value::Array(items) => items.iter_mut().for_each(|v| self.normalize(v)),
            Value::Object(map) => {
                for (key, v) in map.iter_mut() {
                    match (key.as_str(), &v) {
                        ("resourceVersion", Value::String(_)) => {
                            *v = FIXED_RESOURCE_VERSION.into();
                        }
                        (k, Value::String(_)) if MACHINE_FIELDS.contains(&k) => {
                            *v = "normalized".into();
                        }
                        ("containerID", Value::String(s)) => {
                            let runtime = s.split_once("://").map_or("", |(r, _)| r);
                            *v = format!("{runtime}://normalized").into();
                        }
                        _ => self.normalize(v),
                    }
                }
                normalize_age_cells(map);
            }
            _ => {}
        }
    }

    fn normalize_string(&self, s: &str) -> String {
        if looks_like_credential(s) {
            return CREDENTIAL_MARKER.to_owned();
        }
        self.replace_in(s)
    }
}

fn is_uuid(b: &[u8]) -> bool {
    b.iter().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => *c == b'-',
        _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
    })
}

/// Stores every timestamp relative to [`FIXTURE_NOW`]: its age at `recorded_at` (Unix
/// seconds), rounded so re-recordings stay stable, before the fixture's "now".
pub fn to_fixture_time(value: &mut Value, recorded_at: i64) {
    clock::map_timestamps(value, &|s| {
        FIXTURE_NOW_SECONDS - clock::round_age(recorded_at - s)
    });
}

/// Certificates, keys and tokens, inline or base64-encoded.
fn looks_like_credential(s: &str) -> bool {
    ["-----BEGIN ", "LS0tLS1", "eyJhbGciOi", "H4sI", "MII"]
        .iter()
        .any(|m| s.contains(m))
}

/// A kubectl-style duration (`81s`, `2m3s`, `5h`, `3d`), rounded like timestamps
/// ([`clock::round_age`]) and rendered the same way again, so it stays stable across recordings.
fn rounded_duration(text: &str) -> Option<String> {
    let mut seconds = 0i64;
    let mut number = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'y' => 365 * 86_400,
            _ => return None,
        };
        seconds += number.parse::<i64>().ok()? * unit;
        number.clear();
    }
    if !number.is_empty() || text.is_empty() {
        return None;
    }
    Some(flint_core::table::human_duration(clock::round_age(seconds)))
}

/// In a Table, replaces the server-rendered `Age` with a placeholder (the fake server renders
/// it), and rounds other ages (`Last Seen`, "(2m ago)" in restart counts).
fn normalize_age_cells(table: &mut Map<String, Value>) {
    let Some(columns) = table.get("columnDefinitions").and_then(Value::as_array) else {
        return;
    };
    let ages: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| AGE_COLUMNS.contains(&n))
        })
        .map(|(i, _)| i)
        .collect();
    let age = columns.iter().position(|c| c["name"] == "Age");
    let Some(rows) = table.get_mut("rows").and_then(Value::as_array_mut) else {
        return;
    };
    for cells in rows
        .iter_mut()
        .filter_map(|r| r.get_mut("cells").and_then(Value::as_array_mut))
    {
        for (i, cell) in cells.iter_mut().enumerate() {
            let Value::String(text) = cell else { continue };
            if ages.contains(&i) {
                if Some(i) == age {
                    // The fake server renders Age from the creation time.
                    if !text.is_empty() && text != "<unknown>" {
                        *cell = FIXED_AGE.into();
                    }
                } else if let Some(rounded) = rounded_duration(text) {
                    *cell = rounded.into();
                }
            } else if let Some((count, rest)) = text.split_once(" (")
                && let Some(ago) = rest.strip_suffix(" ago)")
                && let Some(rounded) = rounded_duration(ago)
            {
                *cell = format!("{count} ({rounded} ago)").into();
            }
        }
    }
}

/// Hides the values of every Secret in a plain list, as `flint_core::redact` does for display.
pub fn scrub_secret_list(list: &mut Value) {
    for item in list
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        redact::mask_secret(item, SECRET_MARKER);
    }
}

/// Hides the values of ConfigMaps labelled `owner=helm`: Helm's ConfigMap storage driver keeps
/// whole releases there (gzip, base64), values and rendered Secrets included. Other ConfigMaps
/// are kept: their content is useful to tests, and credential-shaped strings in them are caught
/// by the generic scrub.
pub fn scrub_helm_configmaps(list: &mut Value) {
    for item in list
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if item
            .pointer("/metadata/labels/owner")
            .and_then(Value::as_str)
            != Some("helm")
        {
            continue;
        }
        for field in ["data", "binaryData"] {
            if let Some(map) = item.get_mut(field).and_then(Value::as_object_mut) {
                map.values_mut()
                    .for_each(|v| *v = Value::String(SECRET_MARKER.to_owned()));
            }
        }
    }
}

/// Hides annotation values and every cell outside the safe columns of a Secret Table.
pub fn scrub_secret_table(table: &mut Value) {
    let definitions: Vec<(&str, &str)> = table
        .get("columnDefinitions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            let text = |field| c.get(field).and_then(Value::as_str).unwrap_or_default();
            (text("name"), text("format"))
        })
        .collect();
    let safe = redact::safe_secret_wire_columns(&definitions);
    for row in table
        .get_mut("rows")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(object) = row.get_mut("object") {
            redact::mask_secret(object, SECRET_MARKER);
        }
        if let Some(cells) = row.get_mut("cells").and_then(Value::as_array_mut) {
            for (i, cell) in cells.iter_mut().enumerate() {
                if !safe.get(i).copied().unwrap_or(false) {
                    *cell = Value::String(CREDENTIAL_MARKER.to_owned());
                }
            }
        }
    }
}

/// Drops `managedFields` from every Table row: clients strip it on arrival, and it is most of
/// the bytes.
pub fn strip_row_managed_fields(table: &mut Value) {
    for row in table
        .get_mut("rows")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(meta) = row
            .pointer_mut("/object/metadata")
            .and_then(Value::as_object_mut)
        {
            meta.remove("managedFields");
        }
    }
}

/// Sorts Table rows and list items by namespace and name, as most servers return them, so a
/// server that answers in another order (componentstatuses) gives stable fixtures.
pub fn sort_by_identity(value: &mut Value) {
    fn key(meta: Option<&Value>) -> (String, String) {
        let field = |k: &str| {
            meta.and_then(|m| m.get(k))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        (field("namespace"), field("name"))
    }
    if let Some(rows) = value.get_mut("rows").and_then(Value::as_array_mut) {
        rows.sort_by_cached_key(|r| key(r.pointer("/object/metadata")));
    }
    if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
        items.sort_by_cached_key(|i| key(i.get("metadata")));
    }
}

/// Keeps only the Table rows or list items in `namespaces`.
pub fn retain_namespaces(value: &mut Value, namespaces: &[&str]) {
    let keep = |meta: Option<&Value>| {
        meta.and_then(|m| m.get("namespace"))
            .and_then(Value::as_str)
            .is_some_and(|ns| namespaces.contains(&ns))
    };
    if let Some(rows) = value.get_mut("rows").and_then(Value::as_array_mut) {
        rows.retain(|r| keep(r.pointer("/object/metadata")));
    }
    if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
        items.retain(|i| keep(i.get("metadata")));
    }
}

/// Drops `managedFields` from every item of a plain list.
pub fn strip_item_managed_fields(list: &mut Value) {
    for item in list
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(meta) = item.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.remove("managedFields");
        }
    }
}

/// Drops every `description` field (OpenAPI documents are mostly prose).
pub fn strip_descriptions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("description").is_some_and(Value::is_string) {
                map.remove("description");
            }
            map.values_mut().for_each(strip_descriptions);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_descriptions),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const UID_A: &str = "0f5b6c1e-9d2a-4c1b-8e7f-1a2b3c4d5e6f";
    const UID_B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    #[test]
    fn stable_uids_are_uuid_shaped_and_depend_only_on_identity() {
        let a = stable_uid("apps/deployments/demo/web");
        assert_eq!(a, stable_uid("apps/deployments/demo/web"));
        assert_ne!(a, stable_uid("apps/deployments/demo/podinfo"));
        assert!(is_uuid(a.as_bytes()));
    }

    #[test]
    fn uids_are_replaced_everywhere_by_identity() {
        let mut map = UidMap::default();
        let list =
            json!({"items": [{"metadata": {"name": "web", "namespace": "demo", "uid": UID_A}}]});
        map.register_all("apps/deployments", &list);
        let mut value = json!({
            "uid": UID_A,
            "ownerReferences": [{"uid": UID_A}],
            "labels": {"controller-uid": UID_B},
            "holder": format!("node_{UID_A}"),
            "notUid": format!("x{UID_A}"),
        });
        map.normalize(&mut value);
        let stable = stable_uid("apps/deployments/demo/web");
        assert_eq!(value["uid"], stable);
        assert_eq!(value["ownerReferences"][0]["uid"], stable);
        assert_eq!(value["holder"], format!("node_{stable}"));
        assert_eq!(
            value["labels"]["controller-uid"],
            stable_uid(&format!("unknown/{UID_B}"))
        );
        assert_eq!(value["notUid"], format!("x{UID_A}"));
    }

    #[test]
    fn versions_and_machine_ids_are_fixed_and_timestamps_relative() {
        let map = UidMap::default();
        let mut value = json!({
            "metadata": {"creationTimestamp": "2026-11-19T07:23:45Z", "resourceVersion": "4711"},
            "eventTime": "2026-11-19T10:23:40.123456Z",
            "message": "started at 2026-11-19T10:23:45Z",
            "status": {"nodeInfo": {"bootID": "abc", "machineID": "def", "systemUUID": "ghi"},
                       "containerStatuses": [{"containerID": "containerd://f00d"}]},
        });
        map.normalize(&mut value);
        let recorded_at = clock::parse("2026-11-19T10:23:45Z").unwrap().0;
        to_fixture_time(&mut value, recorded_at);
        // Three hours before the recording, and five seconds (rounded to ten).
        assert_eq!(
            value["metadata"]["creationTimestamp"],
            "2025-12-31T21:00:00Z"
        );
        assert_eq!(value["metadata"]["resourceVersion"], "1");
        assert_eq!(value["eventTime"], "2025-12-31T23:59:50.000000Z");
        assert_eq!(value["message"], "started at 2026-11-19T10:23:45Z");
        assert_eq!(value["status"]["nodeInfo"]["machineID"], "normalized");
        assert_eq!(
            value["status"]["containerStatuses"][0]["containerID"],
            "containerd://normalized"
        );
    }

    #[test]
    fn age_cells_and_restart_times_are_fixed() {
        let map = UidMap::default();
        let mut table = json!({
            "columnDefinitions": [{"name": "Name"}, {"name": "Restarts"}, {"name": "Age"}],
            "rows": [{"cells": ["web", "1 (2m3s ago)", "81s"]}, {"cells": ["x", "0", "<unknown>"]}],
        });
        map.normalize(&mut table);
        assert_eq!(
            table["rows"][0]["cells"],
            json!(["web", "1 (2m ago)", "1d"])
        );
        assert_eq!(table["rows"][1]["cells"], json!(["x", "0", "<unknown>"]));
        let mut events = json!({
            "columnDefinitions": [{"name": "Last Seen"}, {"name": "Message"}],
            "rows": [{"cells": ["14s", "x"]}, {"cells": ["<unknown>", "y"]}],
        });
        map.normalize(&mut events);
        assert_eq!(events["rows"][0]["cells"], json!(["10s", "x"]));
        assert_eq!(events["rows"][1]["cells"], json!(["<unknown>", "y"]));
        assert_eq!(rounded_duration("3h25m"), Some("3h".into()));
        assert_eq!(rounded_duration("5"), None);
        assert_eq!(rounded_duration(""), None);
    }

    #[test]
    fn credentials_are_replaced_and_detected() {
        let map = UidMap::default();
        let mut value = json!({
            "ca.crt": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
            "kubeconfig": "certificate-authority-data: LS0tLS1CRUdJTg==",
            "token": "eyJhbGciOiJSUzI1NiJ9.e30.sig",
            "release": "H4sIAAAAAAAC/+x9",
            "der": "MIIBkTCB+wIJ",
        });
        assert_eq!(find_leaks(&value.to_string()).len(), 5);
        map.normalize(&mut value);
        for key in ["ca.crt", "kubeconfig", "token", "release", "der"] {
            assert_eq!(value[key], CREDENTIAL_MARKER);
        }
        assert!(find_leaks(&value.to_string()).is_empty());
        assert_eq!(find_leaks("path /Users/me/x"), ["/Users/"]);
    }

    #[test]
    fn secrets_keep_keys_and_safe_columns_only() {
        let mut list = json!({"items": [{
            "metadata": {"name": "s", "annotations": {"meta.helm.sh/release-name": "web", "other": "v"}},
            "data": {"release": "SDRzSQ=="},
        }]});
        scrub_secret_list(&mut list);
        let item = &list["items"][0];
        assert_eq!(item["data"]["release"], SECRET_MARKER);
        assert_eq!(item["metadata"]["annotations"]["other"], SECRET_MARKER);
        assert_eq!(
            item["metadata"]["annotations"]["meta.helm.sh/release-name"],
            "web"
        );

        let mut table = json!({
            "columnDefinitions": [{"name": "Name"}, {"name": "Type"}, {"name": "Value"}],
            "rows": [{"cells": ["s", "Opaque", "hunter2"],
                      "object": {"metadata": {"annotations": {"kubectl.kubernetes.io/last-applied-configuration": "{}"}}}}],
        });
        scrub_secret_table(&mut table);
        assert_eq!(
            table["rows"][0]["cells"],
            json!(["s", "Opaque", CREDENTIAL_MARKER])
        );

        // A column is not safe because of its header alone.
        let mut repeated = json!({
            "columnDefinitions": [
                {"name": "Name"}, {"name": "Type"}, {"name": "Data"}, {"name": "Age"},
                {"name": "Name"}, {"name": "Namespace"}, {"name": "Type"}, {"name": "Data"},
                {"name": "Age"},
            ],
            "rows": [{"cells": ["s", "Opaque", 1, "1d", "a", "b", "c", "d", "e"]}],
        });
        scrub_secret_table(&mut repeated);
        let m = CREDENTIAL_MARKER;
        assert_eq!(
            repeated["rows"][0]["cells"],
            json!(["s", m, m, "1d", m, m, m, m, m])
        );
        assert_eq!(
            table["rows"][0]["object"]["metadata"]["annotations"]["kubectl.kubernetes.io/last-applied-configuration"],
            SECRET_MARKER
        );
    }

    #[test]
    fn helm_configmaps_are_masked_and_others_kept() {
        let mut list = json!({"items": [
            {"metadata": {"name": "podinfo.v1", "labels": {"owner": "helm"}},
             "data": {"release": "x"}, "binaryData": {"b": "eA=="}},
            {"metadata": {"name": "app-config"}, "data": {"port": "8080"}},
        ]});
        scrub_helm_configmaps(&mut list);
        assert_eq!(list["items"][0]["data"]["release"], SECRET_MARKER);
        assert_eq!(list["items"][0]["binaryData"]["b"], SECRET_MARKER);
        assert_eq!(list["items"][1]["data"]["port"], "8080");
    }

    #[test]
    fn strips_row_managed_fields_and_descriptions() {
        let mut table =
            json!({"rows": [{"object": {"metadata": {"name": "a", "managedFields": [{}]}}}]});
        strip_row_managed_fields(&mut table);
        assert_eq!(table["rows"][0]["object"]["metadata"], json!({"name": "a"}));
        let mut table = json!({"rows": [{"object": {"metadata": {"namespace": "demo"}}},
                                         {"object": {"metadata": {"namespace": "kube-system"}}}]});
        retain_namespaces(&mut table, &["demo"]);
        assert_eq!(table["rows"].as_array().unwrap().len(), 1);
        let mut list = json!({"items": [{"metadata": {"name": "b"}}, {"metadata": {"name": "a"}},
                                        {"metadata": {"namespace": "x", "name": "a"}}]});
        sort_by_identity(&mut list);
        let names: Vec<_> = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                format!(
                    "{}/{}",
                    i["metadata"]["namespace"].as_str().unwrap_or(""),
                    i["metadata"]["name"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(names, ["/a", "/b", "x/a"]);
        let mut list = json!({"items": [{"metadata": {"name": "e", "managedFields": [{}]}}]});
        strip_item_managed_fields(&mut list);
        assert_eq!(list["items"][0]["metadata"], json!({"name": "e"}));
        retain_namespaces(&mut list, &["demo"]);
        assert!(list["items"].as_array().unwrap().is_empty());
        let mut doc = json!({"description": "x", "properties": {"a": {"description": "y", "type": "string"}},
                             "items": [{"description": "z"}], "kept": {"description": {"type": "string"}}});
        strip_descriptions(&mut doc);
        assert_eq!(
            doc,
            json!({"properties": {"a": {"type": "string"}}, "items": [{}],
                               "kept": {"description": {"type": "string"}}})
        );
    }
}
