//! Which API server an e2e test talks to: [`Backend::Fake`] (the fake API server, the default,
//! runs anywhere) or [`Backend::Kind`] (the kind dev cluster named by `FLINT_TEST_KUBECONFIG`).
//!
//! Tests that only browse pick [`Backend::from_env`], so `FLINT_E2E_BACKEND=kind` runs them on
//! kind. Tests that script watches or inject faults use [`Backend::Fake`] directly.
//!
//! Safety: a Kind backend refuses a kubeconfig whose current context is not served from this
//! machine (127.0.0.1, localhost or ::1), so a mistyped variable cannot reach a real cluster.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::fake_api::{CONTEXT, FakeApiServer, Options};

/// Names the kind dev cluster's kubeconfig (`scripts/dev-cluster.sh`).
pub const KIND_KUBECONFIG_ENV: &str = "FLINT_TEST_KUBECONFIG";
/// `fake` (default) or `kind`: the backend of tests that run on either.
pub const BACKEND_ENV: &str = "FLINT_E2E_BACKEND";

/// Where a test's cluster comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The kind dev cluster, from `FLINT_TEST_KUBECONFIG`.
    Kind,
    /// A fake API server replaying the fixtures recorded from kind.
    Fake,
}

impl Backend {
    /// `FLINT_E2E_BACKEND`: `kind` or `fake` (the default).
    pub fn from_env() -> Backend {
        match std::env::var(BACKEND_ENV).as_deref() {
            Ok("kind") => Backend::Kind,
            _ => Backend::Fake,
        }
    }

    /// Starts (or finds) the cluster. `Ok(None)` means the test should be skipped: a Kind
    /// backend without `FLINT_TEST_KUBECONFIG`. That is printed, so a skip is never silent.
    pub fn start(self) -> Result<Option<TestCluster>> {
        match self {
            Backend::Fake => TestCluster::fake(Options::default()).map(Some),
            Backend::Kind => {
                match std::env::var_os(KIND_KUBECONFIG_ENV).filter(|v| !v.is_empty()) {
                    Some(path) => TestCluster::kind(Path::new(&path)).map(Some),
                    None => {
                        eprintln!(
                            "skipped: Backend::Kind needs {KIND_KUBECONFIG_ENV} (the kind dev \
                         cluster's kubeconfig, scripts/dev-cluster.sh)"
                        );
                        Ok(None)
                    }
                }
            }
        }
    }
}

/// A cluster for one test: a kubeconfig with one context, and the fake server behind it.
pub struct TestCluster {
    pub backend: Backend,
    /// A kubeconfig whose current context is [`TestCluster::context`].
    pub kubeconfig: PathBuf,
    pub context: String,
    /// The API server URL.
    pub server: String,
    fake: Option<FakeApiServer>,
    _dir: Option<tempfile::TempDir>,
}

impl TestCluster {
    /// A fake API server with these options, context [`CONTEXT`].
    pub fn fake(options: Options) -> Result<TestCluster> {
        let server = FakeApiServer::start_blocking(options)?;
        let dir = tempfile::tempdir()?;
        let kubeconfig = dir.path().join("kubeconfig");
        server.write_kubeconfig(&kubeconfig, None)?;
        Ok(TestCluster {
            backend: Backend::Fake,
            kubeconfig,
            context: CONTEXT.to_owned(),
            server: server.url(),
            fake: Some(server),
            _dir: Some(dir),
        })
    }

    /// The kind cluster in `kubeconfig`, its current context. Refuses one not on this machine.
    pub fn kind(kubeconfig: &Path) -> Result<TestCluster> {
        let loaded = flint_core::kubeconfig::load(&[kubeconfig.to_owned()]);
        if let Some((path, error)) = loaded.errors.first() {
            bail!("{}: {error}", path.display());
        }
        let context = loaded
            .merged
            .current_context
            .clone()
            .with_context(|| format!("{} has no current context", kubeconfig.display()))?;
        let server = loaded
            .context(&context)
            .and_then(|c| c.server.clone())
            .with_context(|| format!("context {context} has no server"))?;
        if !is_local(&server) {
            bail!(
                "refusing {}: context {context} points at {server}, not at a local kind \
                 cluster",
                kubeconfig.display()
            );
        }
        Ok(TestCluster {
            backend: Backend::Kind,
            kubeconfig: kubeconfig.to_owned(),
            context,
            server,
            fake: None,
            _dir: None,
        })
    }

    /// The fake API server, for scripting watches and faults. `None` on kind.
    pub fn fake_server(&self) -> Option<&FakeApiServer> {
        self.fake.as_ref()
    }
}

/// An `https` server on this machine.
fn is_local(server: &str) -> bool {
    let Some(rest) = server.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_servers_count_as_kind() {
        assert!(is_local("https://127.0.0.1:6443"));
        assert!(is_local("https://localhost:6443/"));
        assert!(is_local("https://[::1]:6443"));
        assert!(!is_local("http://127.0.0.1:6443"));
        assert!(!is_local("https://eks.amazonaws.com"));
        assert!(!is_local("https://127.0.0.1.example.com:443"));
    }

    #[test]
    fn kind_refuses_a_remote_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        std::fs::write(
            &path,
            "apiVersion: v1\nkind: Config\ncurrent-context: prod\nclusters:\n- name: c\n  cluster: {server: \"https://10.0.0.1:6443\"}\nusers:\n- name: u\n  user: {}\ncontexts:\n- name: prod\n  context: {cluster: c, user: u}\n",
        )
        .unwrap();
        let error = TestCluster::kind(&path).err().unwrap();
        assert!(error.to_string().contains("refusing"), "{error}");
    }

    #[test]
    fn fake_clusters_have_a_kubeconfig_and_a_server() {
        let cluster = TestCluster::fake(Options::default()).unwrap();
        assert_eq!(cluster.context, CONTEXT);
        assert!(cluster.kubeconfig.is_file());
        assert_eq!(cluster.server, cluster.fake_server().unwrap().url());
    }
}
