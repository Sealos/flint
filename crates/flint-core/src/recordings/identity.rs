//! The identity a recording belongs to: a keyed digest of a context's whole connection definition.

use kube::config::Kubeconfig;
use secrecy::ExposeSecret as _;

use crate::activity::ServerKeys;
use crate::kubeconfig::definition::ContextDefinition;

/// Keeps these digests apart from any other made with the same key.
const LABEL: &[u8] = b"flint-recording-identity\0";

/// Adds `part` to `message`, length first, so two parts can never read as one.
fn push_part(message: &mut Vec<u8>, part: &[u8]) {
    message.extend_from_slice(&(part.len() as u64).to_be_bytes());
    message.extend_from_slice(part);
}

/// One exact connection: the context, its cluster and user entries, and the TLS material the
/// prepared client uses. Any change to them is another identity. It is a keyed digest, so the
/// database holds no server URL, certificate or credential, and nothing can be learned from it
/// without the key file.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionIdentity([u8; 32]);

impl std::fmt::Debug for ConnectionIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnectionIdentity(..)")
    }
}

impl ConnectionIdentity {
    /// The identity of `context` in `kubeconfig` as connected through `prepared`, the config its
    /// client is built from after any exec plugin ran, so a rotated certificate is another
    /// identity. `None` when the context, its cluster entry or its server is missing, `prepared`
    /// still names a file instead of the bytes in use, or there is no key: such a cluster cannot
    /// be recorded. Pass only a confirmed definition, never a provisional one.
    pub fn of(
        keys: &ServerKeys,
        kubeconfig: &Kubeconfig,
        context: &str,
        prepared: &kube::Config,
    ) -> Option<Self> {
        let definition = ContextDefinition::find(kubeconfig, context)?;
        let auth = &prepared.auth_info;
        if !definition.has_server()
            || prepared.root_cert_file.is_some()
            || (auth.client_certificate.is_some() && auth.client_certificate_data.is_none())
            || (auth.client_key.is_some() && auth.client_key_data.is_none())
        {
            return None;
        }
        let mut message = LABEL.to_vec();
        push_part(&mut message, definition.canonical_json()?.as_bytes());
        let roots = prepared.root_cert.as_deref().unwrap_or_default();
        message.extend_from_slice(&(roots.len() as u64).to_be_bytes());
        for root in roots {
            push_part(&mut message, root);
        }
        let certificate = auth.client_certificate_data.as_deref().unwrap_or_default();
        push_part(&mut message, certificate.as_bytes());
        let key = auth.client_key_data.as_ref();
        push_part(
            &mut message,
            key.map_or("", |key| key.expose_secret()).as_bytes(),
        );
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

    /// A prepared config holding this TLS material, as a client is built from.
    fn prepared(root: &[u8], certificate: &str, key: &str) -> kube::Config {
        let mut config = kube::Config::new("https://a:6443".parse().unwrap());
        config.root_cert = Some(vec![root.to_vec()]);
        config.auth_info.client_certificate_data = Some(certificate.to_owned());
        config.auth_info.client_key_data = Some(key.to_owned().into());
        config
    }

    fn plain() -> kube::Config {
        prepared(b"ca", "cert", "key")
    }

    fn identity(config: &Kubeconfig, tls: &kube::Config) -> ConnectionIdentity {
        ConnectionIdentity::of(&keys(), config, "dev", tls).unwrap()
    }

    #[test]
    fn the_same_definition_keeps_its_identity() {
        let a = identity(&kubeconfig("https://a:6443", "t"), &plain());
        let b = identity(&kubeconfig("https://a:6443", "t"), &plain());
        assert_eq!(a, b);
    }

    #[test]
    fn existing_recording_identity_keeps_its_digest() {
        let id = identity(&kubeconfig("https://a:6443", "t"), &plain());
        assert_eq!(
            id.0,
            [
                0x21, 0xce, 0x4b, 0xda, 0xfd, 0x61, 0x91, 0x78, 0xbf, 0x30, 0xa8, 0x0e, 0x5d, 0xf4,
                0x3b, 0x4c, 0x3b, 0x70, 0xa7, 0x69, 0xb6, 0x47, 0x3d, 0x42, 0xef, 0x69, 0xc4, 0x31,
                0xc4, 0x49, 0x3b, 0x91,
            ]
        );
    }

    #[test]
    fn an_incomplete_cluster_has_no_identity() {
        let mut config = kubeconfig("https://a:6443", "t");
        config.clusters[0].cluster.as_mut().unwrap().server = None;
        assert!(ConnectionIdentity::of(&keys(), &config, "dev", &plain()).is_none());
        config.clusters[0].cluster = None;
        assert!(ConnectionIdentity::of(&keys(), &config, "dev", &plain()).is_none());
        config.clusters.clear();
        assert!(ConnectionIdentity::of(&keys(), &config, "dev", &plain()).is_none());
        config.contexts[0].context = None;
        assert!(ConnectionIdentity::of(&keys(), &config, "dev", &plain()).is_none());
    }

    #[test]
    fn a_re_pointed_context_is_another_identity() {
        let a = identity(&kubeconfig("https://a:6443", "t"), &plain());
        let b = identity(&kubeconfig("https://b:6443", "t"), &plain());
        assert_ne!(a, b);
    }

    #[test]
    fn a_changed_user_is_another_identity() {
        let a = identity(&kubeconfig("https://a:6443", "t"), &plain());
        let b = identity(&kubeconfig("https://a:6443", "other"), &plain());
        assert_ne!(a, b);
    }

    #[test]
    fn rotated_tls_material_is_another_identity() {
        let config = kubeconfig("https://a:6443", "t");
        let before = identity(&config, &plain());
        for rotated in [
            prepared(b"new ca", "cert", "key"),
            prepared(b"ca", "new cert", "key"),
            prepared(b"ca", "cert", "new key"),
        ] {
            assert_ne!(identity(&config, &rotated), before);
        }
    }

    #[test]
    fn tls_parts_cannot_run_into_each_other() {
        let config = kubeconfig("https://a:6443", "t");
        assert_ne!(
            identity(&config, &prepared(b"ca", "ab", "c")),
            identity(&config, &prepared(b"ca", "a", "bc"))
        );
    }

    #[test]
    fn material_still_behind_a_path_has_no_identity() {
        let config = kubeconfig("https://a:6443", "t");
        let mut certificate = plain();
        certificate.auth_info.client_certificate_data = None;
        certificate.auth_info.client_certificate = Some("/client.crt".into());
        let mut key = plain();
        key.auth_info.client_key_data = None;
        key.auth_info.client_key = Some("/client.key".into());
        let mut root = plain();
        root.root_cert_file = Some("/ca.crt".into());
        for tls in [certificate, key, root] {
            assert!(ConnectionIdentity::of(&keys(), &config, "dev", &tls).is_none());
        }
    }

    #[test]
    fn no_key_or_no_context_has_no_identity() {
        let config = kubeconfig("https://a:6443", "t");
        let tls = plain();
        assert!(ConnectionIdentity::of(&ServerKeys::default(), &config, "dev", &tls).is_none());
        assert!(ConnectionIdentity::of(&keys(), &config, "missing", &tls).is_none());
    }

    #[test]
    fn debug_shows_nothing_of_the_digest() {
        let id = ConnectionIdentity::for_tests(0xab);
        assert_eq!(format!("{id:?}"), "ConnectionIdentity(..)");
    }
}
