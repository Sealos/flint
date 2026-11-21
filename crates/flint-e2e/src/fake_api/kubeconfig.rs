//! A kubeconfig that points at the fake server and trusts its CA.

use std::path::Path;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use kube::config::{
    AuthInfo, Cluster, Context, Kubeconfig, NamedAuthInfo, NamedCluster, NamedContext,
};

/// The default context (and cluster and user) name in the kubeconfig.
pub const CONTEXT: &str = "kind-flint-e2e-fake";

/// A kubeconfig with one context named `context` (cluster and user share the name) for
/// `server`, trusting `ca_pem`, without credentials (the fake server does not authenticate).
pub fn kubeconfig(
    server: &str,
    ca_pem: &str,
    context: &str,
    namespace: Option<&str>,
) -> Kubeconfig {
    Kubeconfig {
        api_version: Some("v1".into()),
        kind: Some("Config".into()),
        current_context: Some(context.into()),
        clusters: vec![NamedCluster {
            name: context.into(),
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
            name: context.into(),
            context: Some(Context {
                cluster: context.into(),
                user: Some(context.into()),
                namespace: namespace.map(str::to_owned),
                ..Context::default()
            }),
            ..NamedContext::default()
        }],
        auth_infos: vec![NamedAuthInfo {
            name: context.into(),
            auth_info: Some(AuthInfo::default()),
            ..NamedAuthInfo::default()
        }],
        ..Kubeconfig::default()
    }
}

/// One kubeconfig with the clusters, contexts and users of all of `configs`; the current
/// context is the first one's. Panics if two use the same name.
pub fn merge(configs: Vec<Kubeconfig>) -> Kubeconfig {
    let mut merged = Kubeconfig {
        api_version: Some("v1".into()),
        kind: Some("Config".into()),
        current_context: configs.first().and_then(|c| c.current_context.clone()),
        ..Kubeconfig::default()
    };
    for config in configs {
        for context in &config.contexts {
            assert!(
                !merged.contexts.iter().any(|c| c.name == context.name),
                "two fake servers use the context name {:?}; give each its own",
                context.name
            );
        }
        merged.clusters.extend(config.clusters);
        merged.contexts.extend(config.contexts);
        merged.auth_infos.extend(config.auth_infos);
    }
    merged
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
    fn merged_kubeconfigs_keep_every_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let a = kubeconfig("https://127.0.0.1:1", "ca-a", "kind-a", None);
        let b = kubeconfig("https://127.0.0.1:2", "ca-b", "kind-b", Some("demo"));
        write(&path, &merge(vec![a.clone(), b])).unwrap();
        let loaded = flint_core::kubeconfig::load(std::slice::from_ref(&path));
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let names: Vec<&str> = loaded.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["kind-a", "kind-b"]);
        assert_eq!(
            loaded.context("kind-b").unwrap().server.as_deref(),
            Some("https://127.0.0.1:2")
        );
        assert_eq!(loaded.merged.current_context.as_deref(), Some("kind-a"));
        let twice = std::panic::catch_unwind(|| merge(vec![a.clone(), a]));
        assert!(twice.is_err());
    }

    #[test]
    fn written_kubeconfigs_load_with_flint_core() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let config = kubeconfig(
            "https://127.0.0.1:4443",
            "-----BEGIN CERTIFICATE-----\n",
            CONTEXT,
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
