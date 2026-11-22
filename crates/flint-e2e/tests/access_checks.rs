//! Access checks against the fake API server: what an identity limited to one namespace is told,
//! that a check is never a write, and that a check that fails says nothing.

use std::path::PathBuf;
use std::time::Duration;

use flint_core::access::{Access, AccessCheck};
use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::{Settings, WatchMode};
use flint_e2e::fake_api::{AccessPolicy, CONTEXT, FakeApiServer, Fault, Gvr};

const WAIT: Duration = Duration::from_secs(30);

struct Core {
    handle: CoreHandle,
    server: Option<String>,
    _kubeconfig: PathBuf,
    _dir: tempfile::TempDir,
}

async fn connect(api: &FakeApiServer) -> Core {
    let dir = tempfile::tempdir().unwrap();
    let kubeconfig = dir.path().join("kubeconfig");
    api.write_kubeconfig(&kubeconfig, Some("demo")).unwrap();
    let handle = CoreHandle::start(CoreConfig {
        config_dir: dir.path().join("config"),
        settings: Settings {
            watch_mode: WatchMode::OnView,
            ..Settings::default()
        },
        settings_error: None,
        kubeconfig_base: vec![kubeconfig.clone()],
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    })
    .unwrap();
    let events = handle.events();
    handle.send(Command::Activate(CONTEXT.to_owned()));
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.expect("core running") {
                Event::ConnectionChanged {
                    state: ConnectionState::Failed(e),
                    ..
                } => panic!("connection failed: {e}"),
                Event::DiscoveryChanged {
                    discovery: Some(_), ..
                } => return,
                _ => {}
            }
        }
    })
    .await
    .expect("connected in time");
    let server = Some(api.url());
    handle.unlock(CONTEXT, &api.url());
    Core {
        handle,
        server,
        _kubeconfig: kubeconfig,
        _dir: dir,
    }
}

fn check(namespace: Option<&str>, resource: &str, verb: &str) -> AccessCheck {
    AccessCheck {
        namespace: namespace.map(str::to_owned),
        group: "apps".into(),
        resource: resource.into(),
        subresource: None,
        verb: verb.into(),
    }
}

impl Core {
    async fn ask(&self, check: AccessCheck) -> Access {
        self.handle
            .check_access(CONTEXT, self.server.clone(), check)
            .await
    }
}

fn reviews(api: &FakeApiServer) -> usize {
    api.requests()
        .iter()
        .filter(|r| r.path.ends_with("/selfsubjectaccessreviews"))
        .count()
}

// covers #267: an identity limited to one namespace is told what it may do there, and nothing
// about a check is recorded as a write.
#[tokio::test]
async fn a_namespace_limited_identity_sees_what_it_may_do() {
    let api = FakeApiServer::start().await.unwrap();
    api.set_access(AccessPolicy::only_namespace("demo").deny("delete", "deployments"));
    let core = connect(&api).await;

    let update = check(Some("demo"), "deployments", "update");
    assert_eq!(core.ask(update).await, Access::Allowed);
    let delete = check(Some("demo"), "deployments", "delete");
    assert_eq!(core.ask(delete).await, Access::Forbidden);
    let elsewhere = check(Some("other"), "deployments", "update");
    assert_eq!(core.ask(elsewhere).await, Access::Forbidden);
    let cluster_wide = check(None, "nodes", "patch");
    assert_eq!(core.ask(cluster_wide).await, Access::Forbidden);
    assert!(api.writes().is_empty(), "a check is not a write");
}

// covers #267: the same question is asked once while its answer is fresh.
#[tokio::test]
async fn a_repeated_check_is_answered_from_the_cache() {
    let api = FakeApiServer::start().await.unwrap();
    let core = connect(&api).await;
    let update = check(Some("demo"), "deployments", "update");
    for _ in 0..3 {
        assert_eq!(core.ask(update.clone()).await, Access::Allowed);
    }
    assert_eq!(reviews(&api), 1);
}

// covers #267: a check the server fails is not a refusal.
#[tokio::test]
async fn a_failed_check_is_unknown() {
    let api = FakeApiServer::start().await.unwrap();
    let core = connect(&api).await;
    api.inject(Fault::status(500).on(&Gvr::new(
        "authorization.k8s.io",
        "v1",
        "selfsubjectaccessreviews",
    )));
    let update = check(Some("demo"), "deployments", "update");
    assert_eq!(core.ask(update).await, Access::Unknown);
}

// covers #267: a check the gate refuses (the context shows another server) is not sent.
#[tokio::test]
async fn a_check_the_gate_refuses_is_not_sent() {
    let api = FakeApiServer::start().await.unwrap();
    let core = connect(&api).await;
    let before = reviews(&api);
    let answer = core
        .handle
        .check_access(
            CONTEXT,
            Some("https://moved.example.invalid".into()),
            check(Some("demo"), "deployments", "update"),
        )
        .await;
    assert_eq!(answer, Access::Unknown);
    assert_eq!(reviews(&api), before);
}
