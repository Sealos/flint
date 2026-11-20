//! Which API server an e2e test talks to: [`Backend::Fake`] (the fake API server, the default,
//! runs anywhere) or [`Backend::Kind`] (the kind dev cluster named by `FLINT_TEST_KUBECONFIG`).
//!
//! Tests that only browse pick [`Backend::from_env`], so `FLINT_E2E_BACKEND=kind` runs them on
//! kind. Tests that script watches or inject faults use [`Backend::Fake`] directly.
//!
//! Safety: a Kind backend refuses a kubeconfig whose current context is not served from this
//! machine (127.0.0.1, localhost or ::1), so a mistyped variable cannot reach a real cluster, and
//! gives the app a copy holding only that context, never the whole file. Before a test writes to
//! kind, [`TestCluster::verify_local_kind`] also checks that the context is the kind cluster
//! `kind` itself knows, not just a local URL (a tunnel to another cluster can be one).

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

    /// The kind cluster in `kubeconfig`: its current context, copied with its cluster and user
    /// into a one-context kubeconfig in a scratch directory, so the app sees nothing else from
    /// that file. Refuses a context whose server is not on this machine.
    pub fn kind(kubeconfig: &Path) -> Result<TestCluster> {
        let loaded = flint_core::kubeconfig::load(&[kubeconfig.to_owned()]);
        if let Some((path, error)) = loaded.errors.first() {
            bail!("{}: {error}", path.display());
        }
        let merged = &loaded.merged;
        let context = merged
            .current_context
            .clone()
            .with_context(|| format!("{} has no current context", kubeconfig.display()))?;
        let named = merged
            .contexts
            .iter()
            .find(|c| c.name == context)
            .with_context(|| format!("context {context} is not defined"))?;
        let inner = named
            .context
            .as_ref()
            .with_context(|| format!("context {context} is empty"))?;
        let cluster = merged
            .clusters
            .iter()
            .find(|c| c.name == inner.cluster)
            .with_context(|| format!("context {context} names no defined cluster"))?;
        let server = cluster
            .cluster
            .as_ref()
            .and_then(|c| c.server.clone())
            .with_context(|| format!("context {context} has no server"))?;
        if !is_local(&server) {
            bail!(
                "refusing {}: context {context} points at {server}, not at a local kind \
                 cluster",
                kubeconfig.display()
            );
        }
        let user = inner
            .user
            .as_ref()
            .and_then(|u| merged.auth_infos.iter().find(|a| &a.name == u));
        let one = kube::config::Kubeconfig {
            api_version: Some("v1".into()),
            kind: Some("Config".into()),
            current_context: Some(context.clone()),
            clusters: vec![cluster.clone()],
            contexts: vec![named.clone()],
            auth_infos: user.into_iter().cloned().collect(),
            ..kube::config::Kubeconfig::default()
        };
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("kubeconfig");
        crate::fake_api::kubeconfig::write(&path, &one)?;
        Ok(TestCluster {
            backend: Backend::Kind,
            kubeconfig: path,
            context,
            server,
            fake: None,
            _dir: Some(dir),
        })
    }

    /// Before a test writes to this cluster: it is a kind cluster on this machine, as `kind`
    /// knows it. The context is `kind-<name>`, its cluster has no `proxy-url`, and its server is
    /// the one `kind get kubeconfig --name <name>` gives. Always an error on the fake, which
    /// tests change through its own API.
    pub fn verify_local_kind(&self) -> Result<()> {
        if self.backend != Backend::Kind {
            bail!("not a kind cluster");
        }
        let name = self.context.strip_prefix("kind-").with_context(|| {
            format!("refusing to write to {}: not a kind context", self.context)
        })?;
        let output = std::process::Command::new("kind")
            .args(["get", "kubeconfig", "--name", name])
            .output()
            .context("running `kind get kubeconfig` to check the cluster before writing to it")?;
        if !output.status.success() {
            bail!(
                "refusing to write to {}: kind knows no cluster {name:?} ({})",
                self.context,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let ours = kube::config::Kubeconfig::read_from(&self.kubeconfig)
            .with_context(|| format!("reading {}", self.kubeconfig.display()))?;
        let kinds = kube::config::Kubeconfig::from_yaml(&String::from_utf8_lossy(&output.stdout))
            .context("parsing `kind get kubeconfig`")?;
        same_kind_cluster(&ours, &self.context, &kinds)
    }

    /// The fake API server, for scripting watches and faults. `None` on kind.
    pub fn fake_server(&self) -> Option<&FakeApiServer> {
        self.fake.as_ref()
    }
}

/// Whether `context` in `ours` is the cluster in `kinds` (`kind get kubeconfig`'s output): no
/// proxy, and the same server.
fn same_kind_cluster(
    ours: &kube::config::Kubeconfig,
    context: &str,
    kinds: &kube::config::Kubeconfig,
) -> Result<()> {
    let cluster_of = |config: &kube::config::Kubeconfig, context: &str| {
        let name = config
            .contexts
            .iter()
            .find(|c| c.name == context)
            .and_then(|c| c.context.as_ref())
            .map(|c| c.cluster.clone())?;
        config
            .clusters
            .iter()
            .find(|c| c.name == name)
            .and_then(|c| c.cluster.clone())
    };
    let ours = cluster_of(ours, context)
        .with_context(|| format!("refusing to write to {context}: it names no cluster"))?;
    if let Some(proxy) = &ours.proxy_url {
        bail!("refusing to write to {context}: its cluster goes through a proxy ({proxy})");
    }
    let theirs = kinds
        .contexts
        .iter()
        .find_map(|c| cluster_of(kinds, &c.name))
        .context("`kind get kubeconfig` names no cluster")?;
    let server = |c: &kube::config::Cluster| {
        c.server
            .as_deref()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_owned()
    };
    if server(&ours) != server(&theirs) {
        bail!(
            "refusing to write to {context}: its server {} is not the kind cluster's {}",
            server(&ours),
            server(&theirs)
        );
    }
    Ok(())
}

/// An `https` server on this machine: the URL's host is exactly 127.0.0.1, localhost or ::1,
/// with no user info (`https://localhost@prod.example.com` is prod).
fn is_local(server: &str) -> bool {
    let Ok(uri) = server.parse::<http::Uri>() else {
        return false;
    };
    let Some(authority) = uri.authority() else {
        return false;
    };
    if uri.scheme_str() != Some("https") || authority.as_str().contains('@') {
        return false;
    }
    matches!(
        uri.host(),
        Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
    )
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
        assert!(!is_local("https://localhost:6443@prod.example.com"));
        assert!(!is_local("https://user@127.0.0.1:6443"));
        assert!(!is_local("https://prod.example.com/127.0.0.1"));
        assert!(!is_local("not a url"));
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
    fn kind_passes_only_the_local_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        std::fs::write(
            &path,
            "apiVersion: v1\nkind: Config\ncurrent-context: kind-dev\nclusters:\n- name: kind\n  cluster: {server: \"https://127.0.0.1:6443\"}\n- name: prod\n  cluster: {server: \"https://10.0.0.1:6443\"}\nusers:\n- name: kind\n  user: {token: kind-token}\n- name: prod\n  user: {token: prod-token}\ncontexts:\n- name: kind-dev\n  context: {cluster: kind, user: kind}\n- name: prod\n  context: {cluster: prod, user: prod}\n",
        )
        .unwrap();
        let cluster = TestCluster::kind(&path).unwrap();
        assert_ne!(cluster.kubeconfig, path);
        let written = std::fs::read_to_string(&cluster.kubeconfig).unwrap();
        assert!(written.contains("kind-token"), "{written}");
        assert!(
            !written.contains("prod") && !written.contains("10.0.0.1"),
            "{written}"
        );
        let loaded = flint_core::kubeconfig::load(std::slice::from_ref(&cluster.kubeconfig));
        let names: Vec<&str> = loaded.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["kind-dev"]);
    }

    fn config(context: &str, server: &str, proxy: Option<&str>) -> kube::config::Kubeconfig {
        let proxy = proxy.map_or(String::new(), |p| format!(", proxy-url: \"{p}\""));
        kube::config::Kubeconfig::from_yaml(&format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: {context}\nclusters:\n- name: c\n  cluster: {{server: \"{server}\"{proxy}}}\nusers:\n- name: u\n  user: {{}}\ncontexts:\n- name: {context}\n  context: {{cluster: c, user: u}}\n"
        ))
        .unwrap()
    }

    #[test]
    fn only_the_cluster_kind_knows_may_be_written_to() {
        let kinds = config("kind-flint-dev", "https://127.0.0.1:6443", None);
        let ours = config("kind-flint-dev", "https://127.0.0.1:6443/", None);
        same_kind_cluster(&ours, "kind-flint-dev", &kinds).unwrap();
        // A tunnel to another cluster on another local port.
        let tunnel = config("kind-flint-dev", "https://127.0.0.1:16443", None);
        let error = same_kind_cluster(&tunnel, "kind-flint-dev", &kinds).unwrap_err();
        assert!(
            error.to_string().contains("not the kind cluster's"),
            "{error}"
        );
        // The same URL through a proxy.
        let proxied = config(
            "kind-flint-dev",
            "https://127.0.0.1:6443",
            Some("socks5://127.0.0.1:1080"),
        );
        let error = same_kind_cluster(&proxied, "kind-flint-dev", &kinds).unwrap_err();
        assert!(error.to_string().contains("proxy"), "{error}");
        // The fake is never written to this way.
        let fake = TestCluster::fake(Options::default()).unwrap();
        assert!(fake.verify_local_kind().is_err());
    }

    #[test]
    fn fake_clusters_have_a_kubeconfig_and_a_server() {
        let cluster = TestCluster::fake(Options::default()).unwrap();
        assert_eq!(cluster.context, CONTEXT);
        assert!(cluster.kubeconfig.is_file());
        assert_eq!(cluster.server, cluster.fake_server().unwrap().url());
    }
}
