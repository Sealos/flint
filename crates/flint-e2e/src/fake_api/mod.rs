//! A fake Kubernetes API server for tests, over HTTPS with a CA generated per server.
//!
//! It replays the fixtures recorded from the kind dev cluster (`scripts/record-fixtures.sh`):
//! version, aggregated discovery, OpenAPI v3, Table lists with `includeObject=Metadata`, plain
//! lists and objects, with timestamps shifted to the server's start ([`clock`]). Watches
//! (including streaming lists) are served from the same rows and can be scripted
//! ([`FakeApiServer::script_watch`]) or fed live ([`FakeApiServer::send_watch`]). Faults
//! ([`Fault`]) refuse, delay, hold until released or drop matching requests.
//!
//! Pod logs behave like a real cluster's for what Flint asks (`logs`): per container and
//! previous instance, `follow`, `tailLines`, `sinceSeconds`/`sinceTime`, `timestamps` and
//! `limitBytes`. Tests write them ([`FakeApiServer::append_log`]), end or break the open streams,
//! and churn pods ([`FakeApiServer::add_pod`], [`FakeApiServer::start_container`],
//! [`FakeApiServer::crash_container`], [`FakeApiServer::delete_pod`],
//! [`FakeApiServer::replace_pod`]) with the watch events a cluster sends.
//!
//! Exec (`exec`) upgrades to the WebSocket kube speaks and runs a scripted shell (`shell`): a
//! bash or busybox prompt that echoes, runs `echo`, `exit N`, `stty size`, `seq` and `yes`, and
//! reports exit codes and missing commands as a real cluster does. Tests read what the client
//! sent ([`FakeExec::stdin`], [`FakeExec::resizes`]), print, end or break sessions
//! ([`FakeApiServer::last_exec`], [`FakeApiServer::wait_for_exec_sessions`]), say which
//! containers lack bash ([`FakeApiServer::set_exec_image`]), and refuse or strip upgrades
//! ([`Fault::refuse_upgrade`], [`Fault::strip_upgrade`]).
//!
//! Writes change the state and tell watches. Every write is recorded with its body
//! ([`FakeApiServer::writes`]), `dryRun=All` stores nothing, and [`Fault::conflict`] and
//! [`Fault::admission_denied`] refuse them. Creating objects is opt-in
//! ([`Options::accept_creates`]). Tests set what metrics.k8s.io reports
//! ([`FakeApiServer::set_metrics`], [`FakeApiServer::script_metrics`]) or drop it
//! ([`Options::no_metrics`]).
//!
//! Each server can differ from the recording: another `/version` ([`Options::version`]) and
//! resources or CRDs it lacks ([`Options::hidden`]), so a test can run a set of unlike clusters.
//! Tests move its time ([`FakeApiServer::advance_clock`], [`FakeApiServer::freeze_clock`],
//! [`FakeApiServer::resume_clock`]).
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
//!
//! use flint_e2e::fake_api::ContainerLog;
//! let web = ContainerLog::new("demo", "web-69c6f74b8b-qv4xv");
//! server.append_log(&web, "GET / 200\n");
//! let sibling = server.pod("demo", "web-69c6f74b8b-qv4xv").unwrap().sibling("web-69c6f74b8b-new");
//! server.replace_pod("demo", "web-69c6f74b8b-x4gss", sibling);
//! # Ok(())
//! # }
//! ```

mod access;
pub mod clock;
mod exec;
mod faults;
pub mod fixtures;
pub mod kubeconfig;
mod logs;
mod pods;
pub mod resources;
mod route;
mod row;
mod selector;
mod server;
mod shell;
mod state;
mod tls;
mod watch;

pub use access::AccessPolicy;
pub use exec::{ExecEnd, ExecImage, ExecRuntime, FakeExec, STDOUT_KEPT, TtySize, V4, V5};
pub use faults::{Effect, Fault, HoldPoint, RequestHold};
pub use fixtures::{Fixtures, Gvr};
pub use kubeconfig::{CONTEXT, REMOTE_CONTEXT};
pub use logs::ContainerLog;
pub use pods::FakePod;
pub use route::{Query, Verb};
pub use row::FakeRow;
pub use server::{
    ArrivalLog, FakeApiServer, MetricsSample, MetricsTarget, Options, RequestRecord, Rollouts,
    WriteRecord, arrival_log_at, arrivals_at, connection_mark_at, write_kubeconfigs,
};
pub use watch::WatchStep;
