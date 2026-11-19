//! Which cluster the recorder may talk to: only the kind dev cluster, named explicitly.

use std::ffi::OsStr;
use std::path::PathBuf;

use anyhow::{Result, bail};
use flint_core::kubeconfig::{self, LoadedKubeconfig};

/// The only context the recorder records from (`scripts/dev-cluster.sh`).
pub const DEV_CONTEXT: &str = "kind-flint-dev";

/// The checked recording target.
#[derive(Debug)]
pub struct Target {
    pub path: PathBuf,
    pub loaded: LoadedKubeconfig,
}

/// Checks `KUBECONFIG` (passed in as `env`): it must name exactly one existing file whose
/// current context is [`DEV_CONTEXT`] and whose API server is on this machine. Anything else is
/// refused, so a recording can never read a real cluster.
pub fn check(env: Option<&OsStr>) -> Result<Target> {
    let Some(env) = env.filter(|e| !e.is_empty()) else {
        bail!(
            "KUBECONFIG must be set explicitly to the dev cluster's kubeconfig (.dev/kubeconfig)"
        );
    };
    let paths: Vec<PathBuf> = std::env::split_paths(env).collect();
    let [path] = paths.as_slice() else {
        bail!("KUBECONFIG must name exactly one file, not a list");
    };
    if !path.is_file() {
        bail!("KUBECONFIG {} does not exist", path.display());
    }
    let loaded = kubeconfig::load(std::slice::from_ref(path));
    if let Some((_, error)) = loaded.errors.first() {
        bail!("cannot read KUBECONFIG: {error}");
    }
    let current = loaded.merged.current_context.as_deref().unwrap_or("");
    if current != DEV_CONTEXT {
        bail!("refusing to record: the current context is {current:?}, not {DEV_CONTEXT:?}");
    }
    let context = loaded.context(DEV_CONTEXT);
    let server = context.and_then(|c| c.server.clone()).unwrap_or_default();
    if !is_local_server(&server) {
        bail!("refusing to record: {DEV_CONTEXT} points at {server:?}, not a local kind cluster");
    }
    let proxied = loaded
        .merged
        .clusters
        .iter()
        .filter(|c| Some(&c.name) == context.and_then(|c| c.cluster.as_ref()))
        .any(|c| c.cluster.as_ref().is_some_and(|c| c.proxy_url.is_some()));
    if proxied {
        bail!("refusing to record: {DEV_CONTEXT} goes through a proxy-url");
    }
    Ok(Target {
        path: path.clone(),
        loaded,
    })
}

/// Whether `server` is an `https://` URL on this machine (127.0.0.1, localhost or ::1), with no
/// user info.
fn is_local_server(server: &str) -> bool {
    let Ok(uri) = server.parse::<http::Uri>() else {
        return false;
    };
    let Some(authority) = uri.authority() else {
        return false;
    };
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    uri.scheme_str() == Some("https")
        && !authority.as_str().contains('@')
        && ["127.0.0.1", "localhost", "::1"].contains(&host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kubeconfig(context: &str, server: &str) -> String {
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: {context}\n\
             clusters:\n- name: c\n  cluster:\n    server: {server}\n\
             contexts:\n- name: {context}\n  context:\n    cluster: c\n    user: u\n\
             users:\n- name: u\n  user: {{}}\n"
        )
    }

    fn check_file(text: &str) -> Result<Target> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, text).unwrap();
        check(Some(path.as_os_str()))
    }

    #[test]
    fn accepts_only_the_local_dev_context() {
        let target = check_file(&kubeconfig(DEV_CONTEXT, "https://127.0.0.1:6443")).unwrap();
        assert!(target.loaded.context(DEV_CONTEXT).is_some());
        let err = check_file(&kubeconfig("prod", "https://127.0.0.1:6443")).unwrap_err();
        assert!(err.to_string().contains("current context"));
        let err = check_file(&kubeconfig(DEV_CONTEXT, "https://k8s.example.com")).unwrap_err();
        assert!(err.to_string().contains("not a local kind cluster"));
    }

    #[test]
    fn only_local_https_servers_without_user_info_pass() {
        assert!(is_local_server("https://127.0.0.1:6443"));
        assert!(is_local_server("https://localhost:6443"));
        assert!(is_local_server("https://[::1]:6443"));
        assert!(!is_local_server("https://127.0.0.1.example.com:6443"));
        assert!(!is_local_server("https://localhost.evil:6443"));
        assert!(!is_local_server("https://user@127.0.0.1:6443"));
        assert!(!is_local_server("https://127.0.0.1:6443@k8s.example.com"));
        assert!(!is_local_server("http://127.0.0.1:6443"));
        assert!(!is_local_server("not a url"));
        let proxied = kubeconfig(DEV_CONTEXT, "https://127.0.0.1:6443").replace(
            "    server:",
            "    proxy-url: http://proxy.example.com:3128\n    server:",
        );
        let err = check_file(&proxied).unwrap_err();
        assert!(err.to_string().contains("proxy-url"), "{err}");
    }

    #[test]
    fn requires_one_explicit_file() {
        assert!(check(None).is_err());
        assert!(check(Some(OsStr::new(""))).is_err());
        assert!(check(Some(OsStr::new("/nonexistent/flint-e2e/config"))).is_err());
        let two = std::env::join_paths(["/a", "/b"]).unwrap();
        let err = check(Some(&two)).unwrap_err();
        assert!(err.to_string().contains("exactly one"));
    }
}
