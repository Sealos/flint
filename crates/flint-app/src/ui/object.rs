//! What the generic detail view shows of an object, read from its JSON. GPUI-free so it can be
//! unit tested.

use flint_core::gitops::{self, ManagedBy};
use flint_core::redact::{self, is_safe_secret_annotation};
use serde_json::Value;

/// The parts of an object the Overview tab shows.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Overview {
    pub name: String,
    pub namespace: Option<String>,
    pub kind: String,
    pub api_version: String,
    pub uid: String,
    pub created: Option<String>,
    pub resource_version: String,
    pub generation: Option<i64>,
    pub labels: Vec<(String, String)>,
    pub annotations: Vec<(String, String)>,
    pub finalizers: Vec<String>,
    pub owners: Vec<Owner>,
    pub conditions: Vec<Condition>,
    /// Set for Secrets: the keys of `data` (and `stringData`), with decoded values to reveal.
    pub secret: Option<SecretData>,
    /// For Pods: the containers whose logs can be read, init containers first.
    pub containers: Vec<PodContainer>,
    /// The Argo CD or Flux object that manages this one, from its labels and shown annotations.
    pub managed_by: Option<ManagedBy>,
}

/// One container of a Pod, for the Logs action and its container picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodContainer {
    pub name: String,
    /// An init container that runs to completion before the app starts. A native sidecar (an
    /// init container with `restartPolicy: Always`) runs beside the app, so it is not one.
    pub init: bool,
    /// Its status says it is waiting (crash-looping, pulling, creating) or terminated, so a
    /// shell cannot start in it. Without a status saying so it is taken to run.
    pub not_running: bool,
    /// It ran before (it restarted or terminated), so its previous instance has logs.
    pub has_previous: bool,
    /// The container `kubectl logs` picks without `-c`: the
    /// `kubectl.kubernetes.io/default-container` annotation, or the first app container.
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub kind: String,
    pub name: String,
    pub controller: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub type_: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub last_transition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretData {
    pub type_: String,
    pub entries: Vec<SecretEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEntry {
    pub key: String,
    pub value: SecretValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretValue {
    Text(String),
    /// Not UTF-8: only the size is shown.
    Binary(usize),
    /// `data` held something that is not base64.
    Invalid,
}

impl SecretValue {
    /// The decoded size in bytes, when known.
    pub fn len(&self) -> Option<usize> {
        match self {
            SecretValue::Text(s) => Some(s.len()),
            SecretValue::Binary(n) => Some(*n),
            SecretValue::Invalid => None,
        }
    }
}

/// What a hidden Secret value is replaced with, in the Overview and the YAML.
pub const MASK: &str = "••••••••";

fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |v, key| v.get(key))
        .and_then(Value::as_str)
}

fn string_map(value: Option<&Value>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = value
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    let v = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                    (k.clone(), v)
                })
                .collect()
        })
        .unwrap_or_default();
    pairs.sort();
    pairs
}

fn text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Reads an object's overview. `is_secret` decodes `data` for the per-key reveal.
pub fn overview(object: &Value, is_secret: bool) -> Overview {
    let meta = object.get("metadata").unwrap_or(&Value::Null);
    let owners = meta
        .get("ownerReferences")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .map(|r| Owner {
                    kind: text(r, "kind"),
                    name: text(r, "name"),
                    controller: r.get("controller").and_then(Value::as_bool) == Some(true),
                })
                .collect()
        })
        .unwrap_or_default();
    let conditions = object
        .get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .map(|c| Condition {
                    type_: text(c, "type"),
                    status: text(c, "status"),
                    reason: text(c, "reason"),
                    message: text(c, "message"),
                    last_transition: {
                        let t = text(c, "lastTransitionTime");
                        if t.is_empty() {
                            text(c, "lastUpdateTime")
                        } else {
                            t
                        }
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    let labels = string_map(meta.get("labels"));
    let mut annotations = string_map(meta.get("annotations"));
    if is_secret {
        mask_secret_annotations(&mut annotations);
    }
    let managed_by = if is_secret {
        gitops::managed_by_secret(object)
    } else {
        gitops::managed_by(&gitops::Identity::of(object), |key| {
            labels
                .iter()
                .chain(&annotations)
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        })
    };
    Overview {
        name: str_at(meta, &["name"]).unwrap_or_default().to_owned(),
        namespace: str_at(meta, &["namespace"]).map(str::to_owned),
        kind: text(object, "kind"),
        api_version: text(object, "apiVersion"),
        uid: text(meta, "uid"),
        created: str_at(meta, &["creationTimestamp"]).map(str::to_owned),
        resource_version: text(meta, "resourceVersion"),
        generation: meta.get("generation").and_then(Value::as_i64),
        labels,
        annotations,
        finalizers: meta
            .get("finalizers")
            .and_then(Value::as_array)
            .map(|f| {
                f.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        owners,
        conditions,
        secret: is_secret.then(|| secret_data(object)),
        containers: if text(object, "kind") == "Pod" {
            pod_containers(object)
        } else {
            Vec::new()
        },
        managed_by,
    }
}

/// A Pod's init and app containers, in spec order, with which one is the default and which
/// ran before. Ephemeral (debug) containers are left out.
pub fn pod_containers(pod: &Value) -> Vec<PodContainer> {
    let specs = |path: &str| -> Vec<Value> {
        pod.get("spec")
            .and_then(|s| s.get(path))
            .and_then(Value::as_array)
            .map(|cs| {
                cs.iter()
                    .filter(|c| c.get("name").and_then(Value::as_str).is_some())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };
    let name_of = |c: &Value| c["name"].as_str().unwrap_or_default().to_owned();
    let status = |status_path: &str, name: &str| -> Option<Value> {
        pod.get("status")
            .and_then(|s| s.get(status_path))
            .and_then(Value::as_array)
            .and_then(|ss| {
                ss.iter()
                    .find(|s| s.get("name").and_then(Value::as_str) == Some(name))
            })
            .cloned()
    };
    let ran_before = |status_path: &str, name: &str| -> bool {
        status(status_path, name).is_some_and(|s| {
            s.get("restartCount").and_then(Value::as_i64).unwrap_or(0) > 0
                || s.get("lastState")
                    .and_then(|l| l.get("terminated"))
                    .is_some()
        })
    };
    let not_running = |status_path: &str, name: &str| -> bool {
        status(status_path, name).is_some_and(|s| {
            s.get("state")
                .is_some_and(|st| st.get("waiting").is_some() || st.get("terminated").is_some())
        })
    };
    let apps: Vec<String> = specs("containers").iter().map(name_of).collect();
    // Native sidecars run beside the app: the annotation may name one, as kubectl allows.
    let sidecars: Vec<String> = specs("initContainers")
        .iter()
        .filter(|c| c["restartPolicy"].as_str() == Some("Always"))
        .map(name_of)
        .collect();
    let annotated = str_at(
        pod,
        &[
            "metadata",
            "annotations",
            "kubectl.kubernetes.io/default-container",
        ],
    )
    .filter(|name| apps.iter().chain(&sidecars).any(|a| a == name));
    let default = annotated
        .or(apps.first().map(String::as_str))
        .map(str::to_owned);
    let mut out: Vec<PodContainer> = specs("initContainers")
        .iter()
        .map(|c| {
            let name = name_of(c);
            PodContainer {
                has_previous: ran_before("initContainerStatuses", &name),
                not_running: not_running("initContainerStatuses", &name),
                init: c["restartPolicy"].as_str() != Some("Always"),
                default: default.as_deref() == Some(name.as_str()),
                name,
            }
        })
        .collect();
    out.extend(apps.into_iter().map(|name| PodContainer {
        has_previous: ran_before("containerStatuses", &name),
        not_running: not_running("containerStatuses", &name),
        default: default.as_deref() == Some(name.as_str()),
        name,
        init: false,
    }));
    out
}

fn secret_data(object: &Value) -> SecretData {
    let mut entries: Vec<SecretEntry> = Vec::new();
    if let Some(data) = object.get("data").and_then(Value::as_object) {
        for (key, value) in data {
            let decoded = value.as_str().and_then(decode_base64);
            let value = match decoded {
                None => SecretValue::Invalid,
                Some(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => SecretValue::Text(s),
                    Err(e) => SecretValue::Binary(e.into_bytes().len()),
                },
            };
            entries.push(SecretEntry {
                key: key.clone(),
                value,
            });
        }
    }
    // `stringData` is write-only and never comes back from the server, but a value read from
    // elsewhere could carry it.
    if let Some(data) = object.get("stringData").and_then(Value::as_object) {
        for (key, value) in data {
            entries.push(SecretEntry {
                key: key.clone(),
                value: SecretValue::Text(value.as_str().unwrap_or_default().to_owned()),
            });
        }
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    SecretData {
        type_: text(object, "type"),
        entries,
    }
}

/// Hides Secret annotation values that may hold copies of the data (`kubectl apply`'s
/// last-applied configuration, kapp, Rancher), with the MCP server's allowlist.
fn mask_secret_annotations(annotations: &mut [(String, String)]) {
    for (key, value) in annotations {
        if !is_safe_secret_annotation(key) {
            *value = MASK.to_owned();
        }
    }
}

/// The YAML tab's text: the object as fetched (managedFields removed), and with managedFields
/// put back when `managed_fields` is set. For a Secret, every `data` and `stringData` value,
/// and every annotation value that may copy them, is replaced by [`MASK`], keys kept
/// ([`redact::mask_secret`], as the MCP server does), so values never appear unless revealed
/// one by one in the Overview.
pub fn render_yaml(
    detail: &flint_core::ObjectDetail,
    is_secret: bool,
    managed_fields: bool,
) -> String {
    if !is_secret && !managed_fields {
        return detail.yaml.clone();
    }
    let mut value = if managed_fields {
        flint_core::read::with_managed_fields(&detail.object, detail.managed_fields.as_ref())
    } else {
        detail.object.clone()
    };
    if is_secret {
        redact::mask_secret(&mut value, MASK);
    }
    serde_saphyr::to_string(&value).unwrap_or_else(|e| format!("# cannot render YAML: {e}"))
}

/// Decodes standard base64 (with or without padding), as Kubernetes encodes Secret `data`.
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let bytes = text.trim_end_matches('=').as_bytes();
    if bytes.len() % 4 == 1 || text.len() - bytes.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= sextet(c)? << (18 - 6 * i);
        }
        let n = chunk.len() * 6 / 8;
        out.extend_from_slice(&acc.to_be_bytes()[1..1 + n]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // covers UP-40
    #[test]
    fn pod_containers_name_the_default_and_those_with_previous_logs() {
        let pod = json!({
            "kind": "Pod",
            "metadata": {"name": "web-1", "annotations": {
                "kubectl.kubernetes.io/default-container": "app"
            }},
            "spec": {
                "initContainers": [{"name": "migrate"}],
                "containers": [{"name": "sidecar"}, {"name": "app"}],
                "ephemeralContainers": [{"name": "debugger"}]
            },
            "status": {
                "initContainerStatuses": [{"name": "migrate", "restartCount": 0,
                    "lastState": {}}],
                "containerStatuses": [
                    {"name": "sidecar", "restartCount": 0},
                    {"name": "app", "restartCount": 2}
                ]
            }
        });
        let containers = overview(&pod, false).containers;
        let summary: Vec<(&str, bool, bool, bool)> = containers
            .iter()
            .map(|c| (c.name.as_str(), c.init, c.has_previous, c.default))
            .collect();
        assert_eq!(
            summary,
            [
                ("migrate", true, false, false),
                ("sidecar", false, false, false),
                ("app", false, true, true),
            ]
        );

        // Without the annotation (or naming no app container), the first app container.
        let mut plain = pod.clone();
        plain["metadata"]["annotations"] =
            json!({"kubectl.kubernetes.io/default-container": "nope"});
        let first = pod_containers(&plain);
        assert!(first.iter().find(|c| c.name == "sidecar").unwrap().default);
        // A terminated last state also means previous logs.
        plain["status"]["containerStatuses"][0]["lastState"] = json!({"terminated": {}});
        assert!(pod_containers(&plain)[1].has_previous);
        // Only Pods have containers here.
        assert!(overview(&deployment(), false).containers.is_empty());
    }

    // covers UP-40, UP-41: a native sidecar (an init container that keeps running) is an app
    // container for logs and a shell; a container waiting or terminated is not running.
    #[test]
    fn native_sidecars_are_app_containers_and_waiting_ones_do_not_run() {
        let pod = json!({
            "kind": "Pod",
            "metadata": {"name": "web-1"},
            "spec": {
                "initContainers": [
                    {"name": "migrate"},
                    {"name": "mesh", "restartPolicy": "Always"}
                ],
                "containers": [{"name": "app"}, {"name": "worker"}]
            },
            "status": {
                "initContainerStatuses": [
                    {"name": "migrate", "state": {"terminated": {"exitCode": 0}}},
                    {"name": "mesh", "state": {"running": {}}}
                ],
                "containerStatuses": [
                    {"name": "app", "state": {"running": {}}},
                    {"name": "worker", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]
            }
        });
        let summary: Vec<(String, bool, bool, bool)> = pod_containers(&pod)
            .into_iter()
            .map(|c| (c.name, c.init, c.not_running, c.default))
            .collect();
        let row =
            |name: &str, init, not_running, default| (name.to_owned(), init, not_running, default);
        assert_eq!(
            summary,
            [
                row("migrate", true, true, false),
                row("mesh", false, false, false),
                row("app", false, false, true),
                row("worker", false, true, false),
            ]
        );
        // The annotation may name a native sidecar, but not a plain init container.
        let mut annotated = pod.clone();
        annotated["metadata"]["annotations"] =
            json!({"kubectl.kubernetes.io/default-container": "mesh"});
        let defaults: Vec<String> = pod_containers(&annotated)
            .into_iter()
            .filter(|c| c.default)
            .map(|c| c.name)
            .collect();
        assert_eq!(defaults, ["mesh"]);
        annotated["metadata"]["annotations"] =
            json!({"kubectl.kubernetes.io/default-container": "migrate"});
        assert!(
            pod_containers(&annotated)
                .iter()
                .any(|c| c.name == "app" && c.default)
        );
    }

    // covers UP-23
    #[test]
    fn managed_fields_are_hidden_until_asked_for_and_secrets_stay_masked() {
        let managed = json!([{"manager": "kubectl", "operation": "Update"}]);
        let detail = flint_core::ObjectDetail {
            object: secret(),
            yaml: "kind: Secret\n".into(),
            managed_fields: Some(managed),
            events: Vec::new(),
            events_truncated: false,
            events_error: None,
        };
        let hidden = render_yaml(&detail, true, false);
        assert!(!hidden.contains("managedFields"));
        assert!(!hidden.contains("hunter2") && hidden.contains(MASK));
        let shown = render_yaml(&detail, true, true);
        assert!(shown.contains("managedFields") && shown.contains("manager: kubectl"));
        assert!(!shown.contains("aHVudGVyMg"), "values stay masked: {shown}");
        // Not a Secret, nothing toggled: the core's YAML as is.
        assert_eq!(render_yaml(&detail, false, false), "kind: Secret\n");
    }

    fn deployment() -> Value {
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "name": "web",
                "namespace": "demo",
                "uid": "u-1",
                "resourceVersion": "42",
                "generation": 3,
                "creationTimestamp": "2026-01-01T00:00:00Z",
                "labels": {"app": "web", "tier": "frontend"},
                "annotations": {"deployment.kubernetes.io/revision": "2"},
                "ownerReferences": [{"kind": "Thing", "name": "parent", "controller": true}]
            },
            "status": {
                "conditions": [
                    {"type": "Available", "status": "True", "reason": "MinimumReplicasAvailable",
                     "message": "ok", "lastTransitionTime": "2026-01-01T00:01:00Z"},
                    {"type": "Progressing", "status": "True", "lastUpdateTime": "2026-01-01T00:02:00Z"}
                ]
            }
        })
    }

    // covers UP-23
    #[test]
    fn overview_reads_metadata_owners_and_conditions() {
        let o = overview(&deployment(), false);
        assert_eq!(o.name, "web");
        assert_eq!(o.namespace.as_deref(), Some("demo"));
        assert_eq!(
            (o.kind.as_str(), o.api_version.as_str()),
            ("Deployment", "apps/v1")
        );
        assert_eq!(o.generation, Some(3));
        assert_eq!(
            o.labels,
            [
                ("app".into(), "web".into()),
                ("tier".into(), "frontend".into())
            ]
        );
        assert_eq!(o.annotations.len(), 1);
        assert_eq!(
            o.owners,
            [Owner {
                kind: "Thing".into(),
                name: "parent".into(),
                controller: true
            }]
        );
        assert_eq!(o.conditions.len(), 2);
        assert_eq!(o.conditions[0].reason, "MinimumReplicasAvailable");
        assert_eq!(o.conditions[1].last_transition, "2026-01-01T00:02:00Z");
        assert!(o.secret.is_none());
    }

    #[test]
    fn overview_of_an_almost_empty_object_does_not_panic() {
        let o = overview(&json!({"kind": "X"}), true);
        assert_eq!(o.kind, "X");
        assert!(o.name.is_empty() && o.labels.is_empty() && o.conditions.is_empty());
        assert_eq!(o.secret.unwrap().entries, []);
    }

    fn secret() -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "type": "Opaque",
            "metadata": {"name": "creds", "namespace": "demo", "annotations": {
                "kubectl.kubernetes.io/last-applied-configuration":
                    "{\"data\":{\"password\":\"aHVudGVyMg==\"}}",
                "meta.helm.sh/release-name": "app"
            }},
            // "hunter2", 3 non-UTF-8 bytes, and garbage.
            "data": {"password": "aHVudGVyMg==", "blob": "/wD+", "bad": "%%%"}
        })
    }

    // covers UP-23
    #[test]
    fn secret_values_decode_for_reveal_and_never_reach_the_yaml() {
        let o = overview(&secret(), true);
        let data = o.secret.unwrap();
        assert_eq!(data.type_, "Opaque");
        let by_key: Vec<(&str, &SecretValue)> = data
            .entries
            .iter()
            .map(|e| (e.key.as_str(), &e.value))
            .collect();
        assert_eq!(
            by_key,
            [
                ("bad", &SecretValue::Invalid),
                ("blob", &SecretValue::Binary(3)),
                ("password", &SecretValue::Text("hunter2".into())),
            ]
        );

        let detail = flint_core::ObjectDetail {
            object: secret(),
            yaml: String::new(),
            managed_fields: None,
            events: Vec::new(),
            events_truncated: false,
            events_error: None,
        };
        let yaml = render_yaml(&detail, true, false);
        assert!(!yaml.contains("aHVudGVyMg"), "{yaml}");
        assert!(!yaml.contains("hunter2"), "{yaml}");
        assert!(yaml.contains(&format!("password: {MASK}")), "{yaml}");
        assert!(yaml.contains("name: creds"), "the rest is kept: {yaml}");
        assert!(
            yaml.contains("meta.helm.sh/release-name: app"),
            "allowlisted annotations stay: {yaml}"
        );

        // The last-applied copy is hidden in the Overview too.
        let annotations = overview(&secret(), true).annotations;
        assert!(
            annotations.iter().all(|(_, v)| !v.contains("aHVudGVyMg")),
            "{annotations:?}"
        );
        assert!(annotations.contains(&("meta.helm.sh/release-name".into(), "app".into())));
    }

    #[test]
    fn base64_decodes_standard_padding_variants() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("YQ==").unwrap(), b"a");
        assert_eq!(decode_base64("YWI=").unwrap(), b"ab");
        assert_eq!(decode_base64("YWJj").unwrap(), b"abc");
        assert_eq!(decode_base64("YWJjZA").unwrap(), b"abcd");
        assert_eq!(decode_base64("YQ").unwrap(), b"a");
        assert!(decode_base64("Y").is_none());
        assert!(decode_base64("YQ===").is_none());
        assert!(decode_base64("a b").is_none());
    }
}
