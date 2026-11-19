//! A kubeconfig that points at the fake server and trusts its CA.

use std::path::Path;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use kube::config::{
    AuthInfo, Cluster, Context, Kubeconfig, NamedAuthInfo, NamedCluster, NamedContext,
};

/// The context (and cluster and user) name in the kubeconfig.
pub const CONTEXT: &str = "flint-e2e-fake";

/// A kubeconfig with one context for `server`, trusting `ca_pem`, without credentials (the
/// fake server does not authenticate).
pub fn kubeconfig(server: &str, ca_pem: &str, namespace: Option<&str>) -> Kubeconfig {
    Kubeconfig {
        api_version: Some("v1".into()),
        kind: Some("Config".into()),
        current_context: Some(CONTEXT.into()),
        clusters: vec![NamedCluster {
            name: CONTEXT.into(),
            cluster: Some(Cluster {
                server: Some(server.into()),
                certificate_authority_data: Some(
                    base64::engine::general_purpose::STANDARD.encode(ca_pem),
                ),
                ..Cluster::default()
            }),
            ..NamedCluster::default()
        }],
        contexts: vec![NamedContext {
            name: CONTEXT.into(),
            context: Some(Context {
                cluster: CONTEXT.into(),
                user: Some(CONTEXT.into()),
                namespace: namespace.map(str::to_owned),
                ..Context::default()
            }),
            ..NamedContext::default()
        }],
        auth_infos: vec![NamedAuthInfo {
            name: CONTEXT.into(),
            auth_info: Some(AuthInfo::default()),
            ..NamedAuthInfo::default()
        }],
        ..Kubeconfig::default()
    }
}

/// Writes `config` as YAML to `path`, readable only by its owner.
pub fn write(path: &Path, config: &Kubeconfig) -> Result<()> {
    let text = serde_saphyr::to_string(config).context("serializing the kubeconfig")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_kubeconfigs_load_with_flint_core() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let config = kubeconfig(
            "https://127.0.0.1:4443",
            "-----BEGIN CERTIFICATE-----\n",
            Some("demo"),
        );
        write(&path, &config).unwrap();
        let loaded = flint_core::kubeconfig::load(std::slice::from_ref(&path));
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let context = loaded.context(CONTEXT).unwrap();
        assert_eq!(context.server.as_deref(), Some("https://127.0.0.1:4443"));
        assert_eq!(context.namespace.as_deref(), Some("demo"));
        assert_eq!(loaded.merged.current_context.as_deref(), Some(CONTEXT));
        let ca = loaded.merged.clusters[0]
            .cluster
            .as_ref()
            .and_then(|c| c.certificate_authority_data.clone())
            .unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(ca)
            .unwrap();
        assert!(decoded.starts_with(b"-----BEGIN CERTIFICATE-----"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
