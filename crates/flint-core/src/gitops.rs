//! Which GitOps controller manages an object, read from the labels and annotations Argo CD and
//! Flux set on what they apply (#211). Metadata only: no API calls, and sync state, paused syncs
//! and Argo CD's `ignoreDifferences` are not evaluated.

use serde_json::Value;

const ARGO_TRACKING_ID: &str = "argocd.argoproj.io/tracking-id";
const ARGO_INSTANCE: &str = "argocd.argoproj.io/instance";
const APP_INSTANCE: &str = "app.kubernetes.io/instance";
/// What names an Argo CD application whose name cannot be read from its tracking id.
const UNNAMED_APP: &str = "unknown application";
/// What names a Flux Kustomization whose name cannot be read.
const UNNAMED_KUSTOMIZATION: &str = "unknown kustomization";
/// What names a Flux HelmRelease whose name cannot be read.
const UNNAMED_RELEASE: &str = "unknown release";
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

/// Which object is being asked about, to compare with what a tracking id names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Identity<'a> {
    /// `None` when the object has no `apiVersion`; the core group is `Some("")`.
    pub group: Option<&'a str>,
    /// Empty when unknown, and then not compared.
    pub kind: &'a str,
    pub namespace: Option<&'a str>,
    pub name: &'a str,
}

impl<'a> Identity<'a> {
    /// Reads it from an object as JSON.
    pub fn of(object: &'a Value) -> Self {
        let metadata = object.get("metadata");
        let text = |value: Option<&'a Value>| value.and_then(Value::as_str);
        Identity {
            group: text(object.get("apiVersion"))
                .map(|version| version.rsplit_once('/').map_or("", |(group, _)| group)),
            kind: text(object.get("kind")).unwrap_or_default(),
            namespace: text(metadata.and_then(|m| m.get("namespace"))),
            name: text(metadata.and_then(|m| m.get("name"))).unwrap_or_default(),
        }
    }
}

/// What an Argo CD tracking id says about the object carrying it.
#[derive(Debug, PartialEq, Eq)]
enum Tracking<'a> {
    /// It names this object, tracked by this application.
    App(&'a str),
    /// It names another object, tracked by this application: copied, as a Deployment's
    /// annotations are to its ReplicaSets, or by a controller into another namespace.
    Copied(&'a str),
    /// Replaced by a redaction mask, so it says nothing.
    Masked,
    Unreadable,
}

/// Whether `value` is what the app or the MCP server put in place of a hidden value.
fn is_masked(value: &str) -> bool {
    value == "<redacted>" || value.starts_with("[REDACTED") || value.contains('\u{2022}')
}

/// Reads `<app>:<group>/<kind>:<namespace>/<name>`; a name may hold colons (`shop:reader`). A
/// cluster-scoped object's id may carry a namespace or none, so only a namespaced object's is
/// compared.
fn tracking<'a>(id: &'a str, object: &Identity<'_>) -> Tracking<'a> {
    if is_masked(id) {
        return Tracking::Masked;
    }
    let mut parts = id.splitn(3, ':');
    let (Some(app), Some(group_kind), Some(namespace_name)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Tracking::Unreadable;
    };
    let (Some((group, kind)), Some((namespace, name))) =
        (group_kind.rsplit_once('/'), namespace_name.split_once('/'))
    else {
        return Tracking::Unreadable;
    };
    let same = name == object.name
        && (object.kind.is_empty() || kind == object.kind)
        && object.group.is_none_or(|own| own == group)
        && (namespace.is_empty() || object.namespace.is_none_or(|own| own == namespace));
    if !same {
        Tracking::Copied(app)
    } else if is_plain_name(app) {
        Tracking::App(app)
    } else {
        Tracking::Unreadable
    }
}

/// Who manages `object`, given a lookup in its labels and annotations: Argo CD by a tracking id
/// naming this object or its instance label, then Flux's Kustomization, then its HelmRelease.
/// Any other readable or unreadable tracking id still counts as Argo CD, so a change is warned
/// about when in doubt; a masked one says nothing.
pub fn managed_by<'a>(
    object: &Identity<'_>,
    read: impl Fn(&str) -> Option<&'a str>,
) -> Option<ManagedBy> {
    let tracked = read(ARGO_TRACKING_ID).map(|id| tracking(id, object));
    manager_from(tracked, read)
}

/// [`managed_by`] for an object as the API server returned it. Nothing has masked such an object,
/// so an id that looks masked is an unreadable one, and any other marker that is present but
/// cannot be read still counts, as an unknown manager.
pub(crate) fn managed_by_live<'a>(
    object: &Identity<'_>,
    read: impl Fn(&str) -> Option<&'a str>,
) -> Option<ManagedBy> {
    let tracked = read(ARGO_TRACKING_ID).map(|id| match tracking(id, object) {
        Tracking::Masked => Tracking::Unreadable,
        other => other,
    });
    manager_from(tracked, &read).or_else(|| unreadable_marker(&read))
}

/// The manager a present marker points to when none could be named. The common instance label
/// is not a marker: alone it says nothing.
fn unreadable_marker<'a>(read: &impl Fn(&str) -> Option<&'a str>) -> Option<ManagedBy> {
    let unnamed = |manager, name: &str| ManagedBy {
        manager,
        name: name.to_owned(),
        namespace: None,
    };
    if read(ARGO_INSTANCE).is_some() {
        return Some(unnamed(Manager::ArgoCd, UNNAMED_APP));
    }
    [
        (
            Manager::FluxKustomization,
            FLUX_KUSTOMIZE,
            UNNAMED_KUSTOMIZATION,
        ),
        (Manager::FluxHelmRelease, FLUX_HELM, UNNAMED_RELEASE),
    ]
    .into_iter()
    .find(|(_, (name_key, namespace_key), _)| {
        read(name_key).is_some() || read(namespace_key).is_some()
    })
    .map(|(manager, _, name)| unnamed(manager, name))
}

fn manager_from<'a>(
    tracked: Option<Tracking<'_>>,
    read: impl Fn(&str) -> Option<&'a str>,
) -> Option<ManagedBy> {
    let plain = |key: &str| read(key).filter(|value| is_plain_name(value));
    let argo = |name: &str| ManagedBy {
        manager: Manager::ArgoCd,
        name: name.to_owned(),
        namespace: None,
    };
    if let Some(Tracking::App(app)) = tracked {
        return Some(argo(app));
    }
    if let Some(app) = plain(ARGO_INSTANCE) {
        return Some(argo(app));
    }
    // This common label says nothing alone: it counts only beside a readable-looking id.
    if tracked == Some(Tracking::Unreadable)
        && let Some(app) = plain(APP_INSTANCE)
    {
        return Some(argo(app));
    }
    let flux = [
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
    });
    flux.or_else(|| match tracked? {
        Tracking::Copied(app) if is_plain_name(app) => Some(argo(app)),
        Tracking::Copied(_) | Tracking::Unreadable => Some(argo(UNNAMED_APP)),
        Tracking::App(_) | Tracking::Masked => None,
    })
}

/// [`managed_by`] for an object as JSON. Reads what it is given: pass it after redaction, so a
/// masked annotation counts as absent.
pub fn managed_by_object(object: &Value) -> Option<ManagedBy> {
    let metadata = object.get("metadata")?;
    managed_by(&Identity::of(object), |key| {
        label_or_annotation(metadata, key)
    })
}

/// [`managed_by_live`] for an object as JSON.
pub(crate) fn managed_by_live_object(object: &Value) -> Option<ManagedBy> {
    managed_by_live_metadata(&Identity::of(object), object.get("metadata")?)
}

/// The one reader of a live object's `metadata` (its labels and annotations), so every caller
/// reads the same markers the same way.
pub(crate) fn managed_by_live_metadata(
    identity: &Identity<'_>,
    metadata: &Value,
) -> Option<ManagedBy> {
    managed_by_live(identity, |key| live_label_or_annotation(metadata, key))
}

/// A marker on a live object: an empty label gives way to an annotation of the same key, and a
/// value that is not text is present but empty.
fn live_label_or_annotation<'a>(metadata: &'a Value, key: &str) -> Option<&'a str> {
    let field = |map: &str| {
        let value = metadata.get(map)?.get(key)?;
        Some(value.as_str().unwrap_or_default())
    };
    let label = field("labels");
    label
        .filter(|value| !value.is_empty())
        .or_else(|| field("annotations"))
        .or(label)
}

fn label_or_annotation<'a>(metadata: &'a Value, key: &str) -> Option<&'a str> {
    let field = |map: &str| metadata.get(map)?.get(key)?.as_str();
    field("labels").or_else(|| field("annotations"))
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

    // covers UP-23: a ReplicaSet carrying its Deployment's tracking id belongs to that application,
    // and the common instance label alone is not Argo CD's.
    #[test]
    fn a_copied_tracking_id_names_its_app_and_a_lone_instance_label_does_not() {
        let copied = json!({"kind": "ReplicaSet", "metadata": {
            "name": "web-5d8",
            "labels": {APP_INSTANCE: "other"},
            "annotations": {ARGO_TRACKING_ID: "shop:apps/Deployment:demo/web"},
        }});
        assert_eq!(shown(&copied).as_deref(), Some("Argo CD: shop"));
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

    // covers UP-23: an id naming another object still points to Argo CD. Its instance label names
    // the application, and Flux's own labels take precedence over the id alone.
    #[test]
    fn a_tracking_id_for_another_object_still_counts() {
        let id = "shop:apps/Deployment:demo/web";
        let at = |version: &str, namespace: &str, labels: Value| {
            json!({"apiVersion": version, "kind": "Deployment", "metadata": {
                "name": "web", "namespace": namespace, "labels": labels,
                "annotations": {ARGO_TRACKING_ID: id},
            }})
        };
        assert_eq!(
            shown(&at("apps/v1", "demo", json!({}))).as_deref(),
            Some("Argo CD: shop")
        );
        assert_eq!(
            shown(&at("apps/v1", "other", json!({}))).as_deref(),
            Some("Argo CD: shop")
        );
        assert_eq!(
            shown(&at("example.com/v1", "demo", json!({}))).as_deref(),
            Some("Argo CD: shop")
        );
        assert_eq!(
            shown(&at("apps/v1", "other", json!({ARGO_INSTANCE: "cart"}))).as_deref(),
            Some("Argo CD: cart")
        );
        assert_eq!(
            shown(&at("apps/v1", "other", json!({FLUX_KUSTOMIZE.0: "apps"}))).as_deref(),
            Some("Flux: apps")
        );
    }

    // covers UP-23: an id that cannot be read still points to Argo CD, without repeating its text.
    #[test]
    fn a_malformed_tracking_id_is_argo_cd_without_a_name() {
        for id in ["not a tracking id", "a b:apps/Deployment:demo/web"] {
            let malformed = object(json!({}), json!({ARGO_TRACKING_ID: id}));
            assert_eq!(
                shown(&malformed).as_deref(),
                Some("Argo CD: unknown application"),
                "{id}"
            );
        }
        let copied = object(
            json!({}),
            json!({ARGO_TRACKING_ID: "a b:apps/Deployment:demo/other"}),
        );
        assert_eq!(
            shown(&copied).as_deref(),
            Some("Argo CD: unknown application")
        );
    }

    // covers UP-23: Kubernetes allows colons in some names, such as RBAC roles.
    #[test]
    fn a_tracked_name_may_hold_colons() {
        let role = json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
        "metadata": {"name": "shop:reader", "annotations": {
            ARGO_TRACKING_ID: "shop:rbac.authorization.k8s.io/ClusterRole:/shop:reader",
        }}});
        assert_eq!(shown(&role).as_deref(), Some("Argo CD: shop"));
    }

    // covers UP-23: a masked id says nothing, so the common label beside it does not count.
    #[test]
    fn a_masked_tracking_id_does_not_activate_the_instance_label() {
        for masked in [
            "<redacted>",
            "[REDACTED:secret]",
            "\u{2022}\u{2022}\u{2022}",
        ] {
            let secret = object(
                json!({APP_INSTANCE: "shop"}),
                json!({ARGO_TRACKING_ID: masked}),
            );
            assert_eq!(shown(&secret), None, "{masked}");
        }
    }

    // covers UP-23: an object straight from the API server has no masks, so such an id is unreadable.
    #[test]
    fn a_live_object_with_a_masked_looking_id_counts_as_argo_cd() {
        for id in [
            "<redacted>",
            "[REDACTED]:apps/Deployment:demo/web",
            "shop\u{2022}:apps/Deployment:demo/web",
        ] {
            let live = object(json!({}), json!({ARGO_TRACKING_ID: id}));
            assert_eq!(
                managed_by_live_object(&live)
                    .map(|m| m.to_string())
                    .as_deref(),
                Some("Argo CD: unknown application"),
                "{id}"
            );
            assert_eq!(shown(&live), None, "{id}");
        }
        let labelled = object(
            json!({APP_INSTANCE: "shop"}),
            json!({ARGO_TRACKING_ID: "<redacted>"}),
        );
        assert_eq!(
            managed_by_live_object(&labelled)
                .map(|m| m.to_string())
                .as_deref(),
            Some("Argo CD: shop")
        );
    }

    // covers UP-23: other ids read the same either way.
    #[test]
    fn a_live_object_with_a_plain_id_reads_as_usual() {
        let tracked = object(
            json!({}),
            json!({ARGO_TRACKING_ID: "shop:apps/Deployment:demo/web"}),
        );
        assert_eq!(
            managed_by_live_object(&tracked),
            managed_by_object(&tracked)
        );
        let none = object(json!({}), json!({}));
        assert_eq!(managed_by_live_object(&none), None);
    }

    fn live(object: &Value) -> Option<String> {
        managed_by_live_object(object).map(|m| m.to_string())
    }

    // covers UP-51: a marker that is present but cannot be read counts on a live object, and
    // stays out of what is shown from a redacted one.
    #[test]
    fn a_live_marker_that_cannot_be_read_is_an_unknown_manager() {
        for (key, expected) in [
            (ARGO_INSTANCE, "Argo CD: unknown application"),
            (FLUX_KUSTOMIZE.0, "Flux: unknown kustomization"),
            (FLUX_KUSTOMIZE.1, "Flux: unknown kustomization"),
            (FLUX_HELM.0, "Flux HelmRelease: unknown release"),
            (FLUX_HELM.1, "Flux HelmRelease: unknown release"),
        ] {
            for value in [json!("team/shop"), json!("a b"), json!(""), json!(7)] {
                for marked in [
                    object(json!({key: value}), json!({})),
                    object(json!({}), json!({key: value})),
                ] {
                    assert_eq!(live(&marked).as_deref(), Some(expected), "{key}: {value}");
                    assert_eq!(shown(&marked), None, "{key}: {value}");
                }
            }
        }
    }

    // covers UP-51: a readable marker still names the manager beside an unreadable one.
    #[test]
    fn a_readable_marker_wins_over_an_unreadable_one() {
        let both = object(
            json!({ARGO_INSTANCE: "team/shop", FLUX_KUSTOMIZE.0: "apps"}),
            json!({}),
        );
        assert_eq!(live(&both).as_deref(), Some("Flux: apps"));
        let helm = object(
            json!({FLUX_KUSTOMIZE.0: "a b", FLUX_HELM.0: "web"}),
            json!({}),
        );
        assert_eq!(live(&helm).as_deref(), Some("Flux HelmRelease: web"));
    }

    // covers UP-51: an empty label gives way to the annotation of the same key on a live object.
    #[test]
    fn an_empty_live_label_does_not_hide_an_annotation() {
        let marked = object(json!({ARGO_INSTANCE: ""}), json!({ARGO_INSTANCE: "shop"}));
        assert_eq!(live(&marked).as_deref(), Some("Argo CD: shop"));
        assert_eq!(shown(&marked), None, "the shown result reads as before");
        let tracked = object(
            json!({ARGO_TRACKING_ID: ""}),
            json!({ARGO_TRACKING_ID: "shop:apps/Deployment:demo/web"}),
        );
        assert_eq!(live(&tracked).as_deref(), Some("Argo CD: shop"));
    }

    // covers UP-23: the common instance label alone is no marker, whatever it holds.
    #[test]
    fn a_lone_instance_label_gives_no_live_warning() {
        for value in ["shop", "team/shop", ""] {
            let lone = object(json!({APP_INSTANCE: value}), json!({}));
            assert_eq!(live(&lone), None, "{value}");
        }
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
            let got = shown(&object(json!({}), json!({ARGO_TRACKING_ID: id})));
            if is_masked(value) {
                assert_eq!(got, None, "{value}");
            } else {
                assert_eq!(got.as_deref(), Some("Argo CD: unknown application"));
            }
        }
        assert_eq!(shown(&object(json!({}), json!({}))), None);
    }
}
