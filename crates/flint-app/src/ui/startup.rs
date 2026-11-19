//! `FLINT_VIEW`: a development and testing aid that opens a list, or an object, at startup, so
//! agents can take screenshots of views they cannot click to. It only acts on the cluster that
//! `FLINT_CLUSTER` activated.

/// Names a view to open once the active cluster's discovery arrives.
pub const FLINT_VIEW: &str = "FLINT_VIEW";

/// What `FLINT_VIEW` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupView {
    /// A kind as `plural` (core group) or `plural.group`, matched like `kubectl get` would
    /// match the plural: `pods`, `deployments`, `deployments.apps`, `widgets.example.com`.
    pub kind: String,
    pub namespace: Option<String>,
    pub name: Option<String>,
    /// Open the detail view on its YAML tab instead of Overview.
    pub yaml: bool,
}

/// Parses `kind`, `kind/name` (cluster-scoped), or `namespace/kind/name`, optionally followed by
/// `#yaml`. Returns `None` for anything else.
pub fn parse(value: &str) -> Option<StartupView> {
    let (path, tab) = match value.split_once('#') {
        Some((path, tab)) => (path, Some(tab)),
        None => (value, None),
    };
    let yaml = match tab {
        None | Some("overview") => false,
        Some("yaml") => true,
        Some(_) => return None,
    };
    let parts: Vec<&str> = path.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let (namespace, kind, name) = match parts.as_slice() {
        [kind] => (None, *kind, None),
        [kind, name] => (None, *kind, Some(*name)),
        [namespace, kind, name] => (Some(*namespace), *kind, Some(*name)),
        _ => return None,
    };
    if yaml && name.is_none() {
        return None;
    }
    Some(StartupView {
        kind: kind.to_owned(),
        namespace: namespace.map(str::to_owned),
        name: name.map(str::to_owned),
        yaml,
    })
}

/// Whether `id` (a `KindId` string, `plural` or `plural.group`) is the kind `wanted` names.
/// `wanted` may leave the group out.
pub fn kind_matches(id: &str, wanted: &str) -> bool {
    id == wanted
        || id
            .split_once('.')
            .is_some_and(|(plural, _)| plural == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lists_objects_and_tabs() {
        assert_eq!(
            parse("pods"),
            Some(StartupView {
                kind: "pods".into(),
                namespace: None,
                name: None,
                yaml: false
            })
        );
        let web = parse("demo/deployments/web#yaml").unwrap();
        assert_eq!(web.namespace.as_deref(), Some("demo"));
        assert_eq!(
            (web.kind.as_str(), web.name.as_deref()),
            ("deployments", Some("web"))
        );
        assert!(web.yaml);
        let node = parse("nodes/kind-control-plane").unwrap();
        assert_eq!(
            (node.namespace, node.name.as_deref()),
            (None, Some("kind-control-plane"))
        );

        for bad in [
            "",
            "a//b",
            "a/b/c/d",
            "pods#yaml",
            "demo/pods/x#logs",
            "/pods",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn kinds_match_with_or_without_group() {
        assert!(kind_matches("pods", "pods"));
        assert!(kind_matches("deployments.apps", "deployments"));
        assert!(kind_matches("deployments.apps", "deployments.apps"));
        assert!(!kind_matches("deployments.apps", "deploy"));
        assert!(!kind_matches("pods", "pods.metrics.k8s.io"));
    }
}
