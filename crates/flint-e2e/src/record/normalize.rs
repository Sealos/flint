//! Makes recorded responses safe to commit and stable across recordings: Secret values and
//! anything that looks like a credential are scrubbed, and uids, timestamps, resource versions
//! and server-rendered ages are replaced with fixed or derived values.

use std::collections::BTreeMap;

use flint_core::redact;
use serde_json::{Map, Value};

/// Replaces every Secret `data`/`stringData` value. Base64 of `redacted`, so values still decode.
pub const SECRET_MARKER: &str = "cmVkYWN0ZWQ=";

/// Replaces strings that hold a certificate, key or token.
pub const CREDENTIAL_MARKER: &str = "<redacted>";

/// Every timestamp becomes this instant (fractional seconds are kept as zeros).
pub const FIXED_TIME: &str = "2026-01-01T00:00:00";

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

/// What an age cell becomes.
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
        if let Some(fraction) = timestamp_fraction(s) {
            return match fraction {
                0 => format!("{FIXED_TIME}Z"),
                n => format!("{FIXED_TIME}.{}Z", "0".repeat(n)),
            };
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

/// For an RFC 3339 UTC timestamp (`2026-11-19T10:00:00Z`, `...00.123456Z`), the number of
/// fractional digits.
fn timestamp_fraction(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let shape = b"dddd-dd-ddTdd:dd:dd";
    if b.len() < shape.len() + 1 || *b.last()? != b'Z' {
        return None;
    }
    let head_ok = shape.iter().zip(b).all(|(p, c)| match p {
        b'd' => c.is_ascii_digit(),
        p => p == c,
    });
    let rest = &b[shape.len()..b.len() - 1];
    match rest {
        [] if head_ok => Some(0),
        [b'.', digits @ ..]
            if head_ok && !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) =>
        {
            Some(digits.len())
        }
        _ => None,
    }
}

/// Certificates, keys and tokens, inline or base64-encoded.
fn looks_like_credential(s: &str) -> bool {
    s.contains("-----BEGIN ") || s.contains("LS0tLS1") || s.contains("eyJhbGciOi")
}

/// In a Table, replaces server-rendered ages with a fixed one, and "(2m ago)" in restart counts.
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
    let Some(rows) = table.get_mut("rows").and_then(Value::as_array_mut) else {
        return;
    };
    for cells in rows
        .iter_mut()
        .filter_map(|r| r.get_mut("cells").and_then(Value::as_array_mut))
    {
        for (i, cell) in cells.iter_mut().enumerate() {
            let Value::String(text) = cell else { continue };
            if ages.contains(&i) && !text.is_empty() && text != "<unknown>" {
                *cell = FIXED_AGE.into();
            } else if let Some((count, _)) = text.split_once(" (")
                && text.ends_with(" ago)")
            {
                *cell = format!("{count} ({FIXED_AGE} ago)").into();
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

/// Hides annotation values and every cell outside the safe columns of a Secret Table.
pub fn scrub_secret_table(table: &mut Value) {
    let safe: Vec<bool> = table
        .get("columnDefinitions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            c.get("name")
                .and_then(Value::as_str)
                .is_some_and(redact::is_safe_secret_column)
        })
        .collect();
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
    fn timestamps_versions_and_machine_ids_are_fixed() {
        let map = UidMap::default();
        let mut value = json!({
            "metadata": {"creationTimestamp": "2026-11-19T10:23:45Z", "resourceVersion": "4711"},
            "eventTime": "2026-11-19T10:23:45.123456Z",
            "message": "started at 2026-11-19T10:23:45Z",
            "status": {"nodeInfo": {"bootID": "abc", "machineID": "def", "systemUUID": "ghi"},
                       "containerStatuses": [{"containerID": "containerd://f00d"}]},
        });
        map.normalize(&mut value);
        assert_eq!(
            value["metadata"]["creationTimestamp"],
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(value["metadata"]["resourceVersion"], "1");
        assert_eq!(value["eventTime"], "2026-01-01T00:00:00.000000Z");
        assert_eq!(value["message"], "started at 2026-11-19T10:23:45Z");
        assert_eq!(value["status"]["nodeInfo"]["machineID"], "normalized");
        assert_eq!(
            value["status"]["containerStatuses"][0]["containerID"],
            "containerd://normalized"
        );
        assert_eq!(timestamp_fraction("2026-11-19T10:23:45"), None);
        assert_eq!(timestamp_fraction("2026-11-19T10:23:45.Z"), None);
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
            json!(["web", "1 (1d ago)", "1d"])
        );
        assert_eq!(table["rows"][1]["cells"], json!(["x", "0", "<unknown>"]));
    }

    #[test]
    fn credentials_are_replaced_and_detected() {
        let map = UidMap::default();
        let mut value = json!({
            "ca.crt": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
            "kubeconfig": "certificate-authority-data: LS0tLS1CRUdJTg==",
            "token": "eyJhbGciOiJSUzI1NiJ9.e30.sig",
        });
        assert_eq!(find_leaks(&value.to_string()).len(), 3);
        map.normalize(&mut value);
        for key in ["ca.crt", "kubeconfig", "token"] {
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
        assert_eq!(
            table["rows"][0]["object"]["metadata"]["annotations"]["kubectl.kubernetes.io/last-applied-configuration"],
            SECRET_MARKER
        );
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
