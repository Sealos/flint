//! Which kinds have Scale and Rollout Restart: the one list the app and the core share. A built-in
//! kind is scaled at `spec.replicas`; a custom resource only when its definition declares a `scale`
//! subresource whose replica paths Flint can read.

use std::time::Duration;

use futures::StreamExt as _;
use kube::Client;
use kube::api::{Api, ApiResource, DynamicObject};
use serde_json::Value;

use crate::read::{KindId, KindInfo};

/// The built-in kinds that have Scale, whatever their cluster's discovery says.
const SCALED_BUILT_IN: [&str; 4] = [
    "deployments.apps",
    "statefulsets.apps",
    "replicasets.apps",
    "replicationcontrollers",
];

/// The kinds that have Rollout Restart.
const RESTARTED: [&str; 3] = ["deployments.apps", "daemonsets.apps", "statefulsets.apps"];

/// Whether `kind` is a built-in kind with Scale.
pub fn scales_built_in(kind: &KindId) -> bool {
    SCALED_BUILT_IN.contains(&kind.as_str())
}

/// Whether `kind` has Rollout Restart.
pub fn restarts(kind: &KindId) -> bool {
    RESTARTED.contains(&kind.as_str())
}

/// Where a kind's `scale` subresource keeps its replica counts, as JSON pointers into the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalePaths {
    spec_replicas: String,
    status_replicas: String,
}

impl ScalePaths {
    /// The paths of every built-in kind with Scale.
    pub fn built_in() -> Self {
        Self {
            spec_replicas: "/spec/replicas".to_owned(),
            status_replicas: "/status/replicas".to_owned(),
        }
    }

    /// From a CustomResourceDefinition's `specReplicasPath` and `statusReplicasPath`
    /// (`.spec.replicas`). `None` unless both are plain field paths, under `.spec` and `.status`
    /// as the API server requires: no array index, no quoting, no other syntax.
    pub fn from_crd(spec: &str, status: &str) -> Option<Self> {
        Some(Self {
            spec_replicas: pointer(spec, "spec")?,
            status_replicas: pointer(status, "status")?,
        })
    }

    /// The replica count `object` asks for, if it says one that fits the API's `int32`.
    pub fn replicas(&self, object: &Value) -> Option<u32> {
        count_at(object, &self.spec_replicas)
    }

    /// Where the replica count asked for is, as a JSON pointer: what Scale tests and sets.
    pub(crate) fn spec_pointer(&self) -> &str {
        &self.spec_replicas
    }

    /// The replica count `object` reports, if it says one.
    pub fn status_replicas(&self, object: &Value) -> Option<u32> {
        count_at(object, &self.status_replicas)
    }
}

fn count_at(object: &Value, pointer: &str) -> Option<u32> {
    object
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|n| i32::try_from(n).ok())
        .and_then(|n| u32::try_from(n).ok())
}

/// `.spec.a.b` as `/spec/a/b`, if it is a plain field path whose first field is `root`. A field
/// name is taken literally, with `~` and `/` escaped for the pointer; array indexes, quoting,
/// wildcards and filters are not plain.
fn pointer(path: &str, root: &str) -> Option<String> {
    let fields: Vec<&str> = path.strip_prefix('.')?.split('.').collect();
    let plain = |field: &&str| {
        !field.is_empty()
            && !field
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "[]'\"*@$()?,:\\{}".contains(c))
    };
    if fields.len() < 2 || fields[0] != root || !fields.iter().all(plain) {
        return None;
    }
    Some(
        fields
            .iter()
            .map(|f| format!("/{}", f.replace('~', "~0").replace('/', "~1")))
            .collect(),
    )
}

/// What discovery and its definition say of a kind's `scale` subresource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ScaleSubresource {
    /// Discovery lists none.
    #[default]
    Absent,
    /// Discovery lists one, and where it keeps the counts is not known: no Scale is offered.
    Unread,
    /// Its CustomResourceDefinition keeps the counts at these paths.
    Paths(ScalePaths),
}

impl KindInfo {
    /// Where Scale reads and writes this kind's replica count, if it has Scale.
    pub fn scale_paths(&self) -> Option<ScalePaths> {
        if scales_built_in(&self.id()) {
            return Some(ScalePaths::built_in());
        }
        match &self.scale {
            ScaleSubresource::Paths(paths) => Some(paths.clone()),
            ScaleSubresource::Absent | ScaleSubresource::Unread => None,
        }
    }
}

/// How many definitions are read at once.
const DEFINITIONS_AT_ONCE: usize = 8;

/// How many times a definition is read when the reads fail for a reason that may pass.
const DEFINITION_TRIES: u32 = 3;

/// The pause before reading a definition again after a failure that may pass.
const DEFINITION_RETRY_PAUSE: Duration = Duration::from_millis(200);

/// Why a definition was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefinitionUnread {
    /// The server said no: not found, forbidden, or another answer that would not change.
    Refused,
    /// No answer, a timeout, or an answer that may differ on a later try.
    Transient,
}

/// Reads the CustomResourceDefinition of each custom kind discovery lists with a `scale`
/// subresource, and keeps its replica paths. A definition that cannot be read, or does not
/// declare plain paths for the version discovery serves, leaves its kind without Scale.
pub(crate) async fn read_scale_paths(client: &Client, kinds: &mut [KindInfo], limit: Duration) {
    let wanted: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| k.scale == ScaleSubresource::Unread && !scales_built_in(&k.id()))
        .map(|(i, _)| i)
        .collect();
    if wanted.is_empty() {
        return;
    }
    let reads: Vec<(usize, ApiResource)> = wanted
        .into_iter()
        .map(|index| (index, kinds[index].resource.clone()))
        .collect();
    let mut found = futures::stream::iter(reads)
        .map(|(index, resource)| async move {
            let paths = definition_scale_paths_retried(client, &resource, limit).await;
            if paths.is_none() {
                tracing::debug!(
                    kind = format!("{}.{}", resource.plural, resource.group),
                    "the scale paths of a custom resource could not be read; no Scale"
                );
            }
            (index, paths)
        })
        .buffer_unordered(DEFINITIONS_AT_ONCE);
    // Kept as each answers: a caller's deadline keeps what came before it.
    while let Some((index, paths)) = found.next().await {
        if let Some(paths) = paths {
            kinds[index].scale = ScaleSubresource::Paths(paths);
        }
    }
}

/// [`definition_scale_paths`], read again after a failure that may pass, a few times: one
/// failed read would otherwise leave the kind without Scale for as long as discovery stands.
async fn definition_scale_paths_retried(
    client: &Client,
    resource: &ApiResource,
    limit: Duration,
) -> Option<ScalePaths> {
    for attempt in 1..=DEFINITION_TRIES {
        match definition_scale_paths(client, resource, limit).await {
            Ok(paths) => return paths,
            Err(DefinitionUnread::Refused) => return None,
            Err(DefinitionUnread::Transient) if attempt < DEFINITION_TRIES => {
                tokio::time::sleep(DEFINITION_RETRY_PAUSE * attempt).await;
            }
            Err(DefinitionUnread::Transient) => return None,
        }
    }
    None
}

/// The replica paths the CustomResourceDefinition of `resource` declares now for its version,
/// read within `limit`: `Ok(None)` if it declares none Flint can read.
pub(crate) async fn definition_scale_paths(
    client: &Client,
    resource: &ApiResource,
    limit: Duration,
) -> Result<Option<ScalePaths>, DefinitionUnread> {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &definitions());
    let name = format!("{}.{}", resource.plural, resource.group);
    let crd = match tokio::time::timeout(limit, api.get(&name)).await {
        Ok(Ok(crd)) => crd,
        Ok(Err(kube::Error::Api(status))) if !may_pass(status.code) => {
            return Err(DefinitionUnread::Refused);
        }
        Ok(Err(_)) | Err(_) => return Err(DefinitionUnread::Transient),
    };
    let crd = serde_json::to_value(&crd).map_err(|_| DefinitionUnread::Refused)?;
    Ok(crd_scale_paths(
        &crd,
        &resource.group,
        &resource.version,
        &resource.plural,
    ))
}

/// Whether an API error with HTTP status `code` may not recur: throttling, a timeout, or a
/// server error.
fn may_pass(code: u16) -> bool {
    matches!(code, 408 | 429) || code >= 500
}

fn definitions() -> ApiResource {
    ApiResource {
        group: "apiextensions.k8s.io".to_owned(),
        version: "v1".to_owned(),
        api_version: "apiextensions.k8s.io/v1".to_owned(),
        kind: "CustomResourceDefinition".to_owned(),
        plural: "customresourcedefinitions".to_owned(),
    }
}

/// The replica paths `crd` declares for `version`, if it defines `plural` in `group` and serves
/// that version with a `scale` subresource.
fn crd_scale_paths(crd: &Value, group: &str, version: &str, plural: &str) -> Option<ScalePaths> {
    let spec = crd.get("spec")?;
    if spec.get("group")?.as_str()? != group || spec.pointer("/names/plural")?.as_str()? != plural {
        return None;
    }
    let served = spec.get("versions")?.as_array()?.iter().find(|v| {
        v.get("name").and_then(Value::as_str) == Some(version)
            && v.get("served").and_then(Value::as_bool) == Some(true)
    })?;
    let scale = served.pointer("/subresources/scale")?;
    ScalePaths::from_crd(
        scale.get("specReplicasPath")?.as_str()?,
        scale.get("statusReplicasPath")?.as_str()?,
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_kind_lists() {
        for kind in [
            KindId::new("apps", "deployments"),
            KindId::new("apps", "statefulsets"),
            KindId::new("apps", "replicasets"),
            KindId::new("", "replicationcontrollers"),
        ] {
            assert!(scales_built_in(&kind), "{kind}");
        }
        assert!(!scales_built_in(&KindId::new("apps", "daemonsets")));
        assert!(!scales_built_in(&KindId::new("example.dev", "deployments")));
        assert!(restarts(&KindId::new("apps", "daemonsets")));
        assert!(!restarts(&KindId::new("apps", "replicasets")));
    }

    // covers UP-53: only plain paths under spec and status are read.
    #[test]
    fn crd_paths_are_plain_field_paths() {
        let paths = ScalePaths::from_crd(".spec.size", ".status.ready-count").unwrap();
        let object = json!({"spec": {"size": 3}, "status": {"ready-count": 2}});
        assert_eq!(paths.replicas(&object), Some(3));
        assert_eq!(paths.status_replicas(&object), Some(2));
        assert_eq!(paths.replicas(&json!({"spec": {"size": -1}})), None);
        assert_eq!(
            paths.replicas(&json!({"spec": {"size": 3_000_000_000u64}})),
            None
        );
        assert_eq!(paths.replicas(&json!({"spec": {}})), None);
        // A literal `/` or `~` in a field name is that field, not a deeper path.
        let odd = ScalePaths::from_crd(".spec.west/east.a~b", ".status.n").unwrap();
        assert_eq!(
            odd.replicas(&json!({"spec": {"west/east": {"a~b": 5}, "west": {"east": 1}}})),
            Some(5)
        );
        for (spec, status) in [
            ("spec.size", ".status.n"),
            (".spec", ".status.n"),
            (".spec.", ".status.n"),
            (".spec..size", ".status.n"),
            (".spec.sizes[0]", ".status.n"),
            (".spec['size']", ".status.n"),
            (".metadata.size", ".status.n"),
            (".spec.*", ".status.n"),
            (".spec.a b", ".status.n"),
            (".spec.a:b", ".status.n"),
            (".spec.size", ".spec.n"),
            (".spec.size", ""),
        ] {
            assert_eq!(ScalePaths::from_crd(spec, status), None, "{spec} {status}");
        }
    }

    fn crd(version: &str, served: bool, scale: Value) -> Value {
        json!({
            "spec": {
                "group": "example.dev",
                "names": {"plural": "widgets"},
                "versions": [
                    {"name": "v1alpha1", "served": true},
                    {"name": version, "served": served, "subresources": {"scale": scale}},
                ],
            },
        })
    }

    #[test]
    fn a_definition_names_the_paths_of_the_version_served() {
        let scale = json!({"specReplicasPath": ".spec.size", "statusReplicasPath": ".status.size"});
        let read = |crd: &Value| crd_scale_paths(crd, "example.dev", "v1", "widgets");
        assert_eq!(
            read(&crd("v1", true, scale.clone())),
            ScalePaths::from_crd(".spec.size", ".status.size")
        );
        assert_eq!(read(&crd("v1", false, scale.clone())), None, "not served");
        assert_eq!(
            read(&crd("v2", true, scale.clone())),
            None,
            "another version"
        );
        assert_eq!(
            crd_scale_paths(
                &crd("v1", true, scale.clone()),
                "other.dev",
                "v1",
                "widgets"
            ),
            None,
            "another group"
        );
        assert_eq!(
            read(&crd("v1", true, json!({"specReplicasPath": ".spec.size"}))),
            None,
            "no status path"
        );
        assert_eq!(read(&json!({})), None);
    }

    #[test]
    fn only_built_in_kinds_and_read_paths_have_scale() {
        let kind = |group: &str, plural: &str, scale| KindInfo {
            resource: ApiResource {
                group: group.to_owned(),
                version: "v1".to_owned(),
                api_version: format!("{group}/v1"),
                kind: String::new(),
                plural: plural.to_owned(),
            },
            namespaced: true,
            verbs: Vec::new(),
            singular: String::new(),
            short_names: Vec::new(),
            scale,
        };
        assert_eq!(
            kind("apps", "deployments", ScaleSubresource::Absent).scale_paths(),
            Some(ScalePaths::built_in())
        );
        assert_eq!(
            kind("example.dev", "widgets", ScaleSubresource::Unread).scale_paths(),
            None
        );
        let paths = ScalePaths::from_crd(".spec.size", ".status.size").unwrap();
        assert_eq!(
            kind(
                "example.dev",
                "widgets",
                ScaleSubresource::Paths(paths.clone())
            )
            .scale_paths(),
            Some(paths)
        );
    }
}
