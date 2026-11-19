//! What the recorded API server serves, from its aggregated discovery: every resource with its
//! kind, scope and verbs, and each group's preferred version.

use std::collections::BTreeMap;

use kube::core::discovery::v2::APIGroupDiscoveryList;

use super::fixtures::Gvr;

/// One top-level resource (no subresources).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceInfo {
    pub gvr: Gvr,
    pub kind: String,
    pub namespaced: bool,
    pub verbs: Vec<String>,
    /// Whether this is the group's preferred version (the first one discovery lists).
    pub preferred: bool,
}

impl ResourceInfo {
    pub fn supports(&self, verb: &str) -> bool {
        self.verbs.iter().any(|v| v == verb)
    }

    /// `PodList` for `Pod`.
    pub fn list_kind(&self) -> String {
        format!("{}List", self.kind)
    }
}

/// The resources of a server, by group, version and resource.
#[derive(Debug, Clone, Default)]
pub struct ResourceIndex {
    by_gvr: BTreeMap<Gvr, ResourceInfo>,
}

impl ResourceIndex {
    /// From the aggregated discovery of `/api` and `/apis`.
    pub fn from_discovery(core: &APIGroupDiscoveryList, groups: &APIGroupDiscoveryList) -> Self {
        let mut by_gvr = BTreeMap::new();
        for group in core.items.iter().chain(&groups.items) {
            let name = group
                .metadata
                .as_ref()
                .and_then(|m| m.name.clone())
                .unwrap_or_default();
            for (index, version) in group.versions.iter().enumerate() {
                let Some(v) = &version.version else { continue };
                for resource in &version.resources {
                    let (Some(plural), Some(kind)) = (
                        &resource.resource,
                        resource.response_kind.as_ref().and_then(|k| k.kind.clone()),
                    ) else {
                        continue;
                    };
                    let gvr = Gvr::new(&name, v, plural);
                    by_gvr.insert(
                        gvr.clone(),
                        ResourceInfo {
                            gvr,
                            kind,
                            namespaced: resource.scope.as_deref() == Some("Namespaced"),
                            verbs: resource.verbs.clone(),
                            preferred: index == 0,
                        },
                    );
                }
            }
        }
        ResourceIndex { by_gvr }
    }

    pub fn get(&self, gvr: &Gvr) -> Option<&ResourceInfo> {
        self.by_gvr.get(gvr)
    }

    /// Every resource, in group, version, resource order.
    pub fn iter(&self) -> impl Iterator<Item = &ResourceInfo> {
        self.by_gvr.values()
    }

    /// The listable resources at their group's preferred version: what clients list.
    pub fn preferred_listable(&self) -> impl Iterator<Item = &ResourceInfo> {
        self.iter().filter(|r| r.preferred && r.supports("list"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn discovery(value: serde_json::Value) -> APIGroupDiscoveryList {
        serde_json::from_value(value).unwrap()
    }

    fn resource(plural: &str, kind: &str, scope: &str, verbs: &[&str]) -> serde_json::Value {
        json!({"resource": plural, "responseKind": {"group": "", "version": "", "kind": kind},
               "scope": scope, "verbs": verbs})
    }

    #[test]
    fn indexes_resources_with_scope_verbs_and_preferred_version() {
        let core = discovery(
            json!({"kind": "APIGroupDiscoveryList", "apiVersion": "apidiscovery.k8s.io/v2",
            "metadata": {}, "items": [{"metadata": {"name": ""}, "versions": [{"version": "v1",
            "resources": [resource("pods", "Pod", "Namespaced", &["get", "list", "watch"]),
                          resource("nodes", "Node", "Cluster", &["get", "list", "watch"])]}]}]}),
        );
        let groups = discovery(
            json!({"kind": "APIGroupDiscoveryList", "apiVersion": "apidiscovery.k8s.io/v2",
            "metadata": {}, "items": [{"metadata": {"name": "autoscaling"}, "versions": [
                {"version": "v2", "resources": [resource("horizontalpodautoscalers", "HorizontalPodAutoscaler", "Namespaced", &["list"])]},
                {"version": "v1", "resources": [resource("horizontalpodautoscalers", "HorizontalPodAutoscaler", "Namespaced", &["list"])]}]},
              {"metadata": {"name": "metrics.k8s.io"}, "versions": [{"version": "v1beta1",
                "resources": [resource("pods", "PodMetrics", "Namespaced", &["get", "list"])]}]}]}),
        );
        let index = ResourceIndex::from_discovery(&core, &groups);
        let pods = index.get(&Gvr::core("pods")).unwrap();
        assert!(pods.namespaced && pods.supports("watch") && pods.preferred);
        assert_eq!(pods.list_kind(), "PodList");
        assert!(!index.get(&Gvr::core("nodes")).unwrap().namespaced);
        let metrics = index
            .get(&Gvr::new("metrics.k8s.io", "v1beta1", "pods"))
            .unwrap();
        assert!(!metrics.supports("watch"));
        let hpa_v1 = Gvr::new("autoscaling", "v1", "horizontalpodautoscalers");
        assert!(!index.get(&hpa_v1).unwrap().preferred);
        let listable: Vec<String> = index
            .preferred_listable()
            .map(|r| r.gvr.to_string())
            .collect();
        assert_eq!(
            listable,
            [
                "v1/nodes",
                "v1/pods",
                "autoscaling/v2/horizontalpodautoscalers",
                "metrics.k8s.io/v1beta1/pods"
            ]
        );
    }
}
