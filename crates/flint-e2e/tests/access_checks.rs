//! Access checks against the fake API server: what an identity limited to one namespace is told,
//! that a check is never a write, and that a check that fails says nothing.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use flint_core::access::{Access, AccessCheck};
use flint_core::events::Events;
use flint_core::handle::{
    ClustersSnapshot, Command, ConnectionState, CoreConfig, CoreHandle, Event,
};
use flint_core::settings::ClusterPin;
use flint_core::{Settings, SlotBinding, WatchMode};
use flint_e2e::fake_api::{
    AccessPolicy, CONTEXT, FakeApiServer, Fault, Gvr, Verb, write_kubeconfigs,
};

const WAIT: Duration = Duration::from_secs(30);

struct Core {
    handle: CoreHandle,
    server: Option<String>,
    events: Events,
    /// The slot and definition the cluster list showed last, as a pane follows them.
    shown: Mutex<Option<SlotBinding>>,
    kubeconfig: PathBuf,
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
    let shown = Mutex::new(None);
    handle.send(Command::Activate(CONTEXT.to_owned()));
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.expect("core running") {
                Event::ClustersChanged(list) => note_shown(&shown, &list),
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
        events,
        shown,
        kubeconfig,
        _dir: dir,
    }
}

fn check(namespace: Option<&str>, resource: &str, verb: &str) -> AccessCheck {
    AccessCheck {
        namespace: namespace.map(str::to_owned),
        group: "apps".into(),
        resource: resource.into(),
        subresource: None,
        name: None,
        verb: verb.into(),
    }
}

fn note_shown(shown: &Mutex<Option<SlotBinding>>, list: &ClustersSnapshot) {
    let now = list.clusters.iter().find(|c| c.info.name == CONTEXT);
    *shown.lock().unwrap() = now.map(|c| c.slot_binding());
}

impl Core {
    /// The slot and definition shown now, from the cluster lists the core has sent so far.
    async fn on(&self) -> Option<SlotBinding> {
        while !self.events.is_empty() {
            if let Event::ClustersChanged(list) = self.events.recv().await.expect("core running") {
                note_shown(&self.shown, &list);
            }
        }
        *self.shown.lock().unwrap()
    }

    async fn ask(&self, check: AccessCheck) -> Access {
        let on = self.on().await;
        self.handle
            .check_access(CONTEXT, self.server.clone(), on, check)
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

// covers #267: a Role limited by `resourceNames` allows the object it names, though the resource
// as a whole is refused.
#[tokio::test]
async fn a_role_limited_to_one_name_is_asked_by_that_name() {
    let api = FakeApiServer::start().await.unwrap();
    api.set_access(AccessPolicy::default().only_named("update", "deployments", "web"));
    let core = connect(&api).await;
    let named = |name: &str| AccessCheck {
        name: Some(name.into()),
        ..check(Some("demo"), "deployments", "update")
    };
    assert_eq!(core.ask(named("web")).await, Access::Allowed);
    assert_eq!(core.ask(named("api")).await, Access::Forbidden);
    let unnamed = check(Some("demo"), "deployments", "update");
    assert_eq!(core.ask(unnamed).await, Access::Forbidden);
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
            core.on().await,
            check(Some("demo"), "deployments", "update"),
        )
        .await;
    assert_eq!(answer, Access::Unknown);
    assert_eq!(reviews(&api), before);
}

/// Asks until the reconnected context gives a clear answer.
async fn settled(core: &Core, check: AccessCheck) -> Access {
    tokio::time::timeout(WAIT, async {
        loop {
            match core.ask(check.clone()).await {
                Access::Unknown => tokio::time::sleep(Duration::from_millis(50)).await,
                clear => return clear,
            }
        }
    })
    .await
    .expect("a clear answer in time")
}

// covers #267: a context re-pointed at another server gets that server's answer, and the old
// server is asked nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repointed_context_does_not_reuse_an_answer() {
    let first = FakeApiServer::start().await.unwrap();
    first.set_access(AccessPolicy::default().deny("update", "deployments"));
    let second = FakeApiServer::start().await.unwrap();
    let mut core = connect(&first).await;
    let update = check(Some("demo"), "deployments", "update");
    assert_eq!(core.ask(update.clone()).await, Access::Forbidden);
    let asked_first = reviews(&first);

    second
        .write_kubeconfig(&core.kubeconfig, Some("demo"))
        .unwrap();
    core.handle.reload_kubeconfig().await.unwrap();
    assert_eq!(core.ask(update.clone()).await, Access::Unknown, "old pin");

    core.handle.send(Command::Activate(CONTEXT.to_owned()));
    core.handle.unlock(CONTEXT, &second.url());
    core.server = Some(second.url());
    assert_eq!(settled(&core, update).await, Access::Allowed);
    assert_eq!(reviews(&first), asked_first);
    assert!(second.writes().is_empty(), "a check is not a write");
}

// covers #267: a changed definition on the same server starts with no answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_definition_does_not_reuse_an_answer() {
    let api = FakeApiServer::start().await.unwrap();
    api.set_access(AccessPolicy::default().deny("update", "deployments"));
    let core = connect(&api).await;
    let update = check(Some("demo"), "deployments", "update");
    assert_eq!(core.ask(update.clone()).await, Access::Forbidden);

    api.set_access(AccessPolicy::default());
    api.write_kubeconfig(&core.kubeconfig, Some("other"))
        .unwrap();
    core.handle.reload_kubeconfig().await.unwrap();
    core.handle.send(Command::Activate(CONTEXT.to_owned()));
    assert_eq!(settled(&core, update).await, Access::Allowed);
}

// covers #267: while a kubeconfig file cannot be read the definition is provisional, so a cached
// answer is not shown and nothing is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provisional_context_sends_no_check() {
    let api = FakeApiServer::start().await.unwrap();
    api.set_access(AccessPolicy::default().deny("update", "deployments"));
    let core = connect(&api).await;
    let update = check(Some("demo"), "deployments", "update");
    assert_eq!(core.ask(update.clone()).await, Access::Forbidden);
    let before = reviews(&api);

    std::fs::write(&core.kubeconfig, "not: [valid").unwrap();
    core.handle.reload_kubeconfig().await.unwrap();
    assert_eq!(core.ask(update).await, Access::Unknown);
    let other = check(Some("demo"), "deployments", "delete");
    assert_eq!(core.ask(other).await, Access::Unknown);
    assert_eq!(reviews(&api), before);
}

/// Replaces the context's user with a plugin that logs each launch and whose token is already
/// expired, so every request that uses it launches it again. Returns the log's path.
fn write_plugin_user(api: &FakeApiServer, second: &FakeApiServer, core: &Core) -> PathBuf {
    let dir = core._dir.path();
    let (ca, log, plugin) = (dir.join("ca"), dir.join("launches"), dir.join("plugin"));
    let second_ca = dir.join("second-ca");
    std::fs::write(&ca, api.ca_pem()).unwrap();
    std::fs::write(&second_ca, second.ca_pem()).unwrap();
    let credential = r#"{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"replacement","expirationTimestamp":"2020-01-01T00:00:00Z"}}"#;
    std::fs::write(
        &plugin,
        format!(
            "#!/bin/sh\necho launched >> '{}'\nprintf '%s' '{credential}'\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(
        &core.kubeconfig,
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: {CONTEXT}\nclusters:\n- name: c\n  \
             cluster:\n    server: {}\n    certificate-authority: {}\n- name: d\n  \
             cluster:\n    server: {}\n    certificate-authority: {}\nusers:\n- name: u\n  \
             user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      \
             command: {}\n      interactiveMode: Never\n- name: v\n  user: {{}}\ncontexts:\n\
             - name: {CONTEXT}\n  context:\n    cluster: c\n    user: u\n    namespace: demo\n\
             - name: second\n  context:\n    cluster: d\n    user: v\n",
            api.url(),
            ca.display(),
            second.url(),
            second_ca.display(),
            plugin.display(),
        ),
    )
    .unwrap();
    log
}

fn launches(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |text| text.lines().count())
}

// covers #267: a set's row whose context now has another user at the same URL asks nothing with
// the replacement credential, and does not launch its plugin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_member_credential_sends_no_check() {
    let api = FakeApiServer::start().await.unwrap();
    let second = FakeApiServer::start().await.unwrap();
    let core = connect(&api).await;
    write_kubeconfigs(
        &core.kubeconfig,
        &[(&api, CONTEXT, Some("demo")), (&second, "second", None)],
    )
    .unwrap();
    core.handle.reload_kubeconfig().await.unwrap();
    core.handle.send(Command::Activate(CONTEXT.to_owned()));
    let update = check(Some("demo"), "deployments", "update");
    assert_eq!(settled(&core, update.clone()).await, Access::Allowed);
    let pins = vec![
        ClusterPin::new(CONTEXT, api.url()),
        ClusterPin::new("second", second.url()),
    ];
    let admitted = core.handle.admit_running_set(pins).await.unwrap();
    let member = admitted
        .into_iter()
        .find(|m| m.context() == CONTEXT)
        .unwrap();
    let row = core.handle.bound_to(member);
    let on = core.on().await;
    let asked = row.check_access(CONTEXT, core.server.clone(), on, update.clone());
    assert_eq!(asked.await, Access::Allowed);

    let log = write_plugin_user(&api, &second, &core);
    core.handle.reload_kubeconfig().await.unwrap();
    core.handle.send(Command::Activate(CONTEXT.to_owned()));
    let delete = check(Some("demo"), "deployments", "delete");
    assert_eq!(settled(&core, delete).await, Access::Allowed);
    let (sent, launched) = (reviews(&api), launches(&log));
    assert!(launched > 0, "the replacement is what the context uses now");

    let other = check(Some("demo"), "deployments", "patch");
    let on = core.on().await;
    let answer = row
        .check_access(CONTEXT, core.server.clone(), on, other)
        .await;
    assert_eq!(answer, Access::Unknown);
    let cached = row
        .check_access(CONTEXT, core.server.clone(), on, update)
        .await;
    assert_eq!(cached, Access::Unknown);
    assert_eq!(reviews(&api), sent);
    assert_eq!(launches(&log), launched);
}

// covers #267: a denial that arrives after the context's connection was replaced says nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denial_from_a_replaced_connection_is_dropped() {
    let api = FakeApiServer::start().await.unwrap();
    api.set_access(AccessPolicy::default().deny("update", "deployments"));
    let core = connect(&api).await;
    let (fault, hold) = Fault::hold();
    api.inject(fault.verb(Verb::Other).times(1));
    let update = check(Some("demo"), "deployments", "update");
    let on = core.on().await;
    let asked = tokio::spawn(
        core.handle
            .check_access(CONTEXT, core.server.clone(), on, update),
    );
    hold.reached().await;

    api.write_kubeconfig(&core.kubeconfig, Some("other"))
        .unwrap();
    core.handle.reload_kubeconfig().await.unwrap();
    hold.release();
    assert_eq!(asked.await.unwrap(), Access::Unknown);
}

/// Writes the kubeconfig `api` gives, with a token for its user: the same URL under another user.
fn write_other_user(api: &FakeApiServer, path: &std::path::Path) {
    let mut config = api.kubeconfig(Some("demo"));
    for user in &mut config.auth_infos {
        let auth = user.auth_info.get_or_insert_with(Default::default);
        auth.token = Some("another-user".to_owned().into());
    }
    flint_e2e::fake_api::kubeconfig::write(path, &config).unwrap();
}

// covers #735: a pane's check bound to a connection since replaced by another user at the same
// URL sends nothing, even once the core is connected as that user; a check bound to the
// connection shown now still asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_bound_to_a_replaced_user_sends_nothing() {
    let api = FakeApiServer::start().await.unwrap();
    let core = connect(&api).await;
    let update = check(Some("demo"), "deployments", "update");
    let old = core.on().await;
    assert!(old.is_some());
    let answer = core
        .handle
        .check_access(CONTEXT, core.server.clone(), old, update.clone())
        .await;
    assert_eq!(answer, Access::Allowed, "a matching binding asks");

    write_other_user(&api, &core.kubeconfig);
    core.handle.reload_kubeconfig().await.unwrap();
    core.handle.send(Command::Activate(CONTEXT.to_owned()));
    let delete = check(Some("demo"), "deployments", "delete");
    assert_eq!(settled(&core, delete).await, Access::Allowed);
    let now = core.on().await;
    assert_ne!(now, old, "another user is another definition");
    let before = reviews(&api);

    let patch = check(Some("demo"), "deployments", "patch");
    let stale = core
        .handle
        .check_access_all(
            CONTEXT,
            core.server.clone(),
            old,
            vec![patch.clone(), update.clone()],
        )
        .await;
    assert!(stale.iter().all(|(_, access)| *access == Access::Unknown));
    let unbound = core
        .handle
        .check_access(CONTEXT, core.server.clone(), None, patch.clone())
        .await;
    assert_eq!(unbound, Access::Unknown);
    assert_eq!(reviews(&api), before, "nothing went out with the new user");

    let answer = core
        .handle
        .check_access(CONTEXT, core.server.clone(), now, patch)
        .await;
    assert_eq!(answer, Access::Allowed);
    assert_eq!(reviews(&api), before + 1);
}
