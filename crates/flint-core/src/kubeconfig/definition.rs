//! The canonical kubeconfig definition shared by client fingerprints and recording identities.

use kube::config::{Kubeconfig, NamedAuthInfo, NamedCluster, NamedContext};

pub(crate) struct ContextDefinition<'a> {
    named: &'a NamedContext,
    cluster: Option<&'a NamedCluster>,
    user: Option<&'a NamedAuthInfo>,
}

impl<'a> ContextDefinition<'a> {
    /// Resolves the first entry with each name, keeping partial definitions for fingerprints.
    pub(crate) fn find(kubeconfig: &'a Kubeconfig, context: &str) -> Option<Self> {
        let named = kubeconfig.contexts.iter().find(|c| c.name == context)?;
        let entry = named.context.as_ref();
        let cluster = entry.and_then(|c| kubeconfig.clusters.iter().find(|n| n.name == c.cluster));
        let user = entry
            .and_then(|c| c.user.as_ref())
            .and_then(|u| kubeconfig.auth_infos.iter().find(|n| &n.name == u));
        Some(Self {
            named,
            cluster,
            user,
        })
    }

    /// Whether the definition names a server, as required for recording.
    pub(crate) fn has_server(&self) -> bool {
        self.cluster
            .and_then(|c| c.cluster.as_ref())
            .is_some_and(|c| c.server.is_some())
    }

    /// The serialized definition includes credentials. Hash it immediately and discard it.
    pub(crate) fn canonical_json(&self) -> Option<String> {
        let mut value = serde_json::to_value((self.named, self.cluster, self.user)).ok()?;
        // Exec `env` maps serialize in varying order, and the app enables `preserve_order`.
        // Sort object keys recursively while keeping the order of arrays such as exec arguments.
        value.sort_all_objects();
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_nested_maps_and_keeps_array_order() {
        let config = Kubeconfig::from_yaml(
            "contexts: [{name: dev, context: {user: u, cluster: c}}]\n\
             clusters: [{name: c, cluster: {server: 'https://a'}}]\n\
             users: [{name: u, user: {exec: {args: [z, a], env: [{value: v, name: N}]}}}]",
        )
        .unwrap();
        let definition = ContextDefinition::find(&config, "dev").unwrap();
        assert_eq!(
            definition.canonical_json().unwrap(),
            concat!(
                r#"[{"context":{"cluster":"c","user":"u"},"name":"dev"},"#,
                r#"{"cluster":{"server":"https://a"},"name":"c"},"#,
                r#"{"name":"u","user":{"exec":{"args":["z","a"],"env":[{"name":"N","value":"v"}],"provideClusterInfo":false}}}]"#,
            )
        );
    }
}
