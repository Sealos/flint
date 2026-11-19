//! A fake Kubernetes API server for tests, over HTTPS with a CA generated per server.
//!
//! It replays the fixtures recorded from the kind dev cluster (`scripts/record-fixtures.sh`):
//! version, aggregated discovery, OpenAPI v3, Table lists with `includeObject=Metadata`, plain
//! lists, objects and pod logs, with timestamps shifted to the server's start
//! ([`clock`]). Watches (including streaming lists) are served from the same rows and
//! can be scripted ([`FakeApiServer::script_watch`]) or fed live
//! ([`FakeApiServer::send_watch`]). Faults ([`Fault`]) refuse, delay or drop matching
//! requests.
//!
//! ```no_run
//! # async fn example() -> anyhow::Result<()> {
//! use flint_e2e::fake_api::{FakeApiServer, Fault, Gvr, Verb, WatchStep};
//!
//! let server = FakeApiServer::start().await?;
//! let dir = tempfile::tempdir()?;
//! server.write_kubeconfig(&dir.path().join("kubeconfig"), Some("demo"))?;
//! let configmaps = Gvr::core("configmaps");
//! server.inject(Fault::forbidden().on(&Gvr::core("secrets")).verb(Verb::List).cluster_wide());
//! server.script_watch(&configmaps, vec![WatchStep::Bookmark, WatchStep::Expired]);
//! # Ok(())
//! # }
//! ```

pub mod clock;
mod faults;
pub mod fixtures;
pub mod kubeconfig;
pub mod resources;
mod route;
mod row;
mod selector;
mod server;
mod state;
mod tls;
mod watch;

pub use faults::{Effect, Fault};
pub use fixtures::{Fixtures, Gvr};
pub use kubeconfig::CONTEXT;
pub use route::{Query, Verb};
pub use row::FakeRow;
pub use server::{FakeApiServer, Options, RequestRecord, write_kubeconfigs};
pub use watch::WatchStep;
