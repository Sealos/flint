//! Which GitOps controller manages an object, read from the labels and annotations Argo CD and
//! Flux set on what they apply (#211). Metadata only: no API calls, and sync state, paused syncs
//! and Argo CD's `ignoreDifferences` are not evaluated.

use serde_json::Value;

const ARGO_TRACKING_ID: &str = "argocd.argoproj.io/tracking-id";
const ARGO_INSTANCE: &str = "argocd.argoproj.io/instance";
const APP_INSTANCE: &str = "app.kubernetes.io/instance";
const FLUX_KUSTOMIZE: (&str, &str) = (
    "kustomize.toolkit.fluxcd.io/name",
    "kustomize.toolkit.fluxcd.io/namespace",
);
const FLUX_HELM: (&str, &str) = (
    "helm.toolkit.fluxcd.io/name",
    "helm.toolkit.fluxcd.io/namespace",
);

/// The kind of thing that manages an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    ArgoCd,
    FluxKustomization,
    FluxHelmRelease,
}

/// The controller's object that manages this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedBy {
    pub manager: Manager,
    pub name: String,
    pub namespace: Option<String>,
}

impl std::fmt::Display for ManagedBy {
    /// `Argo CD: web`, `Flux: flux-system/apps` or `Flux HelmRelease: flux-system/web`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.manager {
            Manager::ArgoCd => "Argo CD: ",
            Manager::FluxKustomization => "Flux: ",
            Manager::FluxHelmRelease => "Flux HelmRelease: ",
        })?;
        if let Some(namespace) = &self.namespace {
            write!(f, "{namespace}/")?;
        }
        f.write_str(&self.name)
    }
}

/// A value safe to show as a name. Annotations hold any text, and this one is shown to agents on
/// a line of its own, so anything but a plain Kubernetes-style name counts as absent.
fn is_plain_name(value: &str) -> bool {
    (1..=253).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// What an Argo CD tracking id says about the object carrying it.
#[derive(Debug, PartialEq, Eq)]
enum Tracking<'a> {
    /// It names this object, tracked by this application.
    App(&'a str),
    /// It names another object: copied, as a Deployment's annotations are to its ReplicaSets.
    Copied,
    Unreadable,
}

/// Reads `<app>:<group>/<kind>:<namespace>/<name>`. An empty `kind` is not compared.
fn tracking<'a>(id: &'a str, kind: &str, name: &str) -> Tracking<'a> {
    let mut parts = id.split(':');
    let (Some(app), Some(group_kind), Some(namespace_name), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Tracking::Unreadable;
    };
    let (Some((_, tracked_kind)), Some((_, tracked_name))) =
        (group_kind.rsplit_once('/'), namespace_name.rsplit_once('/'))
    else {
        return Tracking::Unreadable;
    };
    if tracked_name != name || (!kind.is_empty() && tracked_kind != kind) {
        Tracking::Copied
    } else if is_plain_name(app) {
        Tracking::App(app)
    } else {
        Tracking::Unreadable
    }
}

/// Who manages the object of this `kind` and `name`, given a lookup in its labels and
/// annotations. Argo CD is checked first, then Flux's Kustomization, then its HelmRelease.
pub fn managed_by<'a>(
    kind: &str,
    name: &str,
    read: impl Fn(&str) -> Option<&'a str>,
) -> Option<ManagedBy> {
    let plain = |key: &str| read(key).filter(|value| is_plain_name(value));
    let argo = |name: &str| ManagedBy {
        manager: Manager::ArgoCd,
        name: name.to_owned(),
        namespace: None,
    };
    let tracked = read(ARGO_TRACKING_ID).map(|id| tracking(id, kind, name));
    if let Some(Tracking::App(app)) = tracked {
        return Some(argo(app));
    }
    if let Some(app) = plain(ARGO_INSTANCE) {
        return Some(argo(app));
    }
    // This common label says nothing alone: it counts only beside a tracking id for this object.
    if tracked == Some(Tracking::Unreadable)
        && let Some(app) = plain(APP_INSTANCE)
    {
        return Some(argo(app));
    }
    [
        (Manager::FluxKustomization, FLUX_KUSTOMIZE),
        (Manager::FluxHelmRelease, FLUX_HELM),
    ]
    .into_iter()
    .find_map(|(manager, (name_key, namespace_key))| {
        Some(ManagedBy {
            manager,
            name: plain(name_key)?.to_owned(),
            namespace: plain(namespace_key).map(str::to_owned),
        })
    })
}

/// [`managed_by`] for an object as JSON. Reads what it is given: pass it after redaction, so a
/// masked annotation counts as absent.
pub fn managed_by_object(object: &Value) -> Option<ManagedBy> {
    let metadata = object.get("metadata")?;
    let field = |map: &str, key: &str| metadata.get(map)?.get(key)?.as_str();
    managed_by(
        object
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        |key| field("labels", key).or_else(|| field("annotations", key)),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(labels: Value, annotations: Value) -> Value {
        json!({"kind": "Deployment", "metadata": {
            "name": "web", "labels": labels, "annotations": annotations,
        }})
    }

    fn shown(object: &Value) -> Option<String> {
        managed_by_object(object).map(|m| m.to_string())
    }

    // covers UP-23
    #[test]
    fn argo_cd_is_read_from_its_tracking_id_and_instance_label() {
        let tracked = object(
            json!({}),
            json!({ARGO_TRACKING_ID: "shop:apps/Deployment:demo/web"}),
        );
        assert_eq!(shown(&tracked).as_deref(), Some("Argo CD: shop"));
        let labelled = object(json!({ARGO_INSTANCE: "shop"}), json!({}));
        assert_eq!(shown(&labelled).as_deref(), Some("Argo CD: shop"));
        let cluster_scoped = json!({"kind": "Namespace", "metadata": {"name": "demo",
            "annotations": {ARGO_TRACKING_ID: "shop:/Namespace:/demo"}}});
        assert_eq!(shown(&cluster_scoped).as_deref(), Some("Argo CD: shop"));
    }

    // covers UP-23: a Deployment's annotations are copied to its ReplicaSets, which Argo CD does
    // not manage, and the common instance label alone is not Argo CD's.
    #[test]
    fn a_copied_tracking_id_or_a_lone_instance_label_is_not_argo_cd() {
        let copied = json!({"kind": "ReplicaSet", "metadata": {
            "name": "web-5d8",
            "labels": {APP_INSTANCE: "shop"},
            "annotations": {ARGO_TRACKING_ID: "shop:apps/Deployment:demo/web"},
        }});
        assert_eq!(shown(&copied), None);
        assert_eq!(
            shown(&object(json!({APP_INSTANCE: "shop"}), json!({}))),
            None
        );
    }

    // covers UP-23
    #[test]
    fn an_unreadable_tracking_id_falls_back_to_the_instance_label() {
        let unreadable = object(
            json!({APP_INSTANCE: "shop"}),
            json!({ARGO_TRACKING_ID: "not a tracking id"}),
        );
        assert_eq!(shown(&unreadable).as_deref(), Some("Argo CD: shop"));
    }

    // covers UP-23
    #[test]
    fn flux_is_read_from_its_labels() {
        let kustomized = object(
            json!({FLUX_KUSTOMIZE.0: "apps", FLUX_KUSTOMIZE.1: "flux-system"}),
            json!({}),
        );
        assert_eq!(
            shown(&kustomized).as_deref(),
            Some("Flux: flux-system/apps")
        );
        let released = object(
            json!({FLUX_HELM.0: "web", FLUX_HELM.1: "flux-system"}),
            json!({}),
        );
        assert_eq!(
            shown(&released).as_deref(),
            Some("Flux HelmRelease: flux-system/web")
        );
        let no_namespace = object(json!({FLUX_KUSTOMIZE.0: "apps"}), json!({}));
        assert_eq!(shown(&no_namespace).as_deref(), Some("Flux: apps"));
    }

    // covers UP-10: an annotation holds any text, and the name reaches an agent on its own line.
    #[test]
    fn a_value_that_is_not_a_plain_name_counts_as_absent() {
        for value in ["a\n# injected", "a b", "", "[REDACTED:secret]", "••••"] {
            assert_eq!(
                shown(&object(json!({}), json!({ARGO_INSTANCE: value}))),
                None
            );
            let id = format!("{value}:apps/Deployment:demo/web");
            assert_eq!(
                shown(&object(json!({}), json!({ARGO_TRACKING_ID: id}))),
                None
            );
        }
        assert_eq!(shown(&object(json!({}), json!({}))), None);
    }
}
