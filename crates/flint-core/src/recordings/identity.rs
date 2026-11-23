//! The identity a recording belongs to: a keyed digest of a context's whole connection definition.

use kube::config::Kubeconfig;

use crate::activity::ServerKeys;

/// Keeps these digests apart from any other made with the same key.
const LABEL: &[u8] = b"flint-recording-identity\0";

/// One exact connection definition: the context, its cluster entry and its user entry. Any change
/// to them is another identity. It is a keyed digest, so the database holds neither the server URL
/// nor a credential, and nothing can be learned from it without the key file.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionIdentity([u8; 32]);

impl std::fmt::Debug for ConnectionIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnectionIdentity(..)")
    }
}

impl ConnectionIdentity {
    /// The identity of `context` in `kubeconfig`. `None` when the context, its cluster entry or
    /// its server is missing, or there is no key: such a cluster cannot be recorded. Pass only a
    /// confirmed definition, never a provisional one.
    pub fn of(keys: &ServerKeys, kubeconfig: &Kubeconfig, context: &str) -> Option<Self> {
        let named = kubeconfig.contexts.iter().find(|c| c.name == context)?;
        let entry = named.context.as_ref()?;
        let cluster = kubeconfig
            .clusters
            .iter()
            .find(|c| c.name == entry.cluster)?;
        cluster.cluster.as_ref()?.server.as_ref()?;
        let user = entry
            .user
            .as_ref()
            .and_then(|u| kubeconfig.auth_infos.iter().find(|n| &n.name == u));
        // Sorted because exec `env` items serialize in a different order at each load. The text
        // holds credentials, so it is digested right away and never kept.
        let mut value = serde_json::to_value((named, cluster, user)).ok()?;
        value.sort_all_objects();
        let mut message = LABEL.to_vec();
        message.extend_from_slice(value.to_string().as_bytes());
        keys.digest(&message).map(ConnectionIdentity)
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[cfg(test)]
    pub(super) fn for_tests(byte: u8) -> Self {
        ConnectionIdentity([byte; 32])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kubeconfig(server: &str, token: &str) -> Kubeconfig {
        let text = format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: {server}\n\
             users:\n- name: u\n  user:\n    token: {token}\ncontexts:\n- name: dev\n  context:\n    \
             cluster: c\n    user: u\n"
        );
        Kubeconfig::from_yaml(&text).unwrap()
    }

    fn keys() -> ServerKeys {
        ServerKeys::with_secret(&[7; 32])
    }

    #[test]
    fn the_same_definition_keeps_its_identity() {
        let a = ConnectionIdentity::of(&keys(), &kubeconfig("https://a:6443", "t"), "dev");
        let b = ConnectionIdentity::of(&keys(), &kubeconfig("https://a:6443", "t"), "dev");
        assert!(a.is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn a_re_pointed_context_is_another_identity() {
        let a = ConnectionIdentity::of(&keys(), &kubeconfig("https://a:6443", "t"), "dev");
        let b = ConnectionIdentity::of(&keys(), &kubeconfig("https://b:6443", "t"), "dev");
        assert_ne!(a.unwrap(), b.unwrap());
    }

    #[test]
    fn a_changed_user_is_another_identity() {
        let a = ConnectionIdentity::of(&keys(), &kubeconfig("https://a:6443", "t"), "dev");
        let b = ConnectionIdentity::of(&keys(), &kubeconfig("https://a:6443", "other"), "dev");
        assert_ne!(a.unwrap(), b.unwrap());
    }

    #[test]
    fn no_key_or_no_context_has_no_identity() {
        let config = kubeconfig("https://a:6443", "t");
        assert!(ConnectionIdentity::of(&ServerKeys::default(), &config, "dev").is_none());
        assert!(ConnectionIdentity::of(&keys(), &config, "missing").is_none());
    }

    #[test]
    fn debug_shows_nothing_of_the_digest() {
        let id = ConnectionIdentity::for_tests(0xab);
        assert_eq!(format!("{id:?}"), "ConnectionIdentity(..)");
    }
}
