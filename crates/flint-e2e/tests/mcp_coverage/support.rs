//! The setup these tests share: one fake cluster to expose beside one that stays hidden.

use anyhow::Result;
use flint_e2e::fake_api::{FakeApiServer, Options, write_kubeconfigs};
use flint_e2e::mcp::Mcp;
use flint_e2e::scratch::Scratch;

pub const EXPOSED: &str = "fake-exposed";
pub const HIDDEN: &str = "fake-hidden";
/// A pod of the default fixtures, in [`NAMESPACE`].
pub const POD: &str = "web-69c6f74b8b-qv4xv";
pub const NAMESPACE: &str = "demo";

/// Two fake servers in one kubeconfig, with a scratch config directory.
pub struct Setup {
    pub api: FakeApiServer,
    pub hidden: FakeApiServer,
    pub scratch: Scratch,
}

impl Setup {
    /// Both clusters, with `EXPOSED` exposed whole.
    pub fn exposed() -> Result<Setup> {
        let api = FakeApiServer::start_blocking(Options::default())?;
        let hidden = FakeApiServer::start_blocking(Options::default())?;
        let scratch = Scratch::new()?;
        write_kubeconfigs(
            &scratch.path().join("kubeconfig"),
            &[(&api, EXPOSED, None), (&hidden, HIDDEN, None)],
        )?;
        let setup = Setup {
            api,
            hidden,
            scratch,
        };
        setup.expose(&[], &[])?;
        Ok(setup)
    }

    /// Exposes `EXPOSED`, limited to `namespaces` and `kinds` (empty: no limit).
    pub fn expose(&self, namespaces: &[&str], kinds: &[&str]) -> Result<()> {
        let server = self.api.url();
        let mut settings = format!("\n[[ai_exposed_clusters]]\ncontext = \"{EXPOSED}\"\n");
        settings.push_str(&format!("server = \"{server}\"\n"));
        if !namespaces.is_empty() {
            settings.push_str(&format!("namespaces = {namespaces:?}\n"));
        }
        if !kinds.is_empty() {
            settings.push_str(&format!("kinds = {kinds:?}\n"));
        }
        self.scratch.write_settings(&settings)
    }

    /// A new server process on this setup's kubeconfig and config directory.
    pub fn mcp(&self) -> Result<Mcp> {
        Mcp::start(
            self.scratch.path().join("kubeconfig"),
            &self.scratch.config_dir(),
        )
    }
}
