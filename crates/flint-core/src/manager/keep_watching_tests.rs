use std::path::PathBuf;

use super::*;
use crate::auth_plugin::tests::{FakePlugin, live};
use crate::connection::member::MemberState;
use crate::handle::CoreConfig;
use crate::manager::keep_watching_lists::{WARM_KINDS, WARM_LIST_CLUSTERS};
use crate::manager::{Changed, Cluster};
use crate::protection::SessionUnlocks;
use crate::read::KindId;
use crate::settings::Settings;
use crate::watch::HubRegistry;

/// A manager with one context, `dev`, signing in with `plugin`, and nothing selected.
fn manager_with(plugin: &FakePlugin, config_dir: PathBuf) -> (ClusterManager, PathBuf) {
    manager_on(plugin, config_dir, "")
}

/// [`manager_with`], with `more` (YAML) appended to its contexts, each with a slot of its own.
fn manager_on(plugin: &FakePlugin, config_dir: PathBuf, more: &str) -> (ClusterManager, PathBuf) {
    let path = plugin.dir.path().join("kubeconfig");
    let yaml = format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
         http://127.0.0.1:1\nusers:\n- name: u\n  user:\n{}- name: s\n  user: {{}}\n\
         contexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\n{more}",
        plugin.user_yaml()
    );
    std::fs::write(&path, yaml).unwrap();
    let (events, _rx) = crate::events::channel();
    let config = CoreConfig {
        config_dir,
        settings: Settings::default(),
        settings_error: None,
        kubeconfig_base: vec![path.clone()],
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    };
    let hubs = HubRegistry::default();
    let mut manager = ClusterManager::new(config, events, hubs.clone(), SessionUnlocks::default());
    let loaded = crate::kubeconfig::load(std::slice::from_ref(&path));
    for info in &loaded.contexts {
        let slot = Cluster::new(
            info.clone(),
            cluster::context_fingerprint(&loaded.merged, &info.name),
            &hubs,
            &manager.engine_config,
        );
        manager.clusters.push(slot);
    }
    manager.contexts.record(&loaded);
    manager.kubeconfig = Arc::new(loaded);
    manager.kubeconfig_paths = vec![path.clone()];
    (manager, path)
}

/// A manager with `dev` signing in with `plugin` and `other` with nothing to run, `active`
/// selected and nothing connected.
fn set_manager(plugin: &FakePlugin, config_dir: PathBuf, active: &str) -> ClusterManager {
    let other = "- name: other\n  context:\n    cluster: c\n    user: s\n";
    let (mut manager, _) = manager_on(plugin, config_dir, other);
    manager.active = Some(active.to_owned());
    manager.sync_roles();
    manager
}

/// Opens the set of `dev` and `other`.
fn open_set(manager: &mut ClusterManager) {
    manager.set_running_set(vec!["dev".to_owned(), "other".to_owned()]);
}

fn dev_state(manager: &ClusterManager) -> MemberState {
    manager.members["dev"].state().clone()
}

/// Whether `dev`'s current connection may run its plugin now.
fn dev_may_run(manager: &ClusterManager) -> bool {
    let slot = manager.cluster("dev").unwrap();
    slot.access.grant(CredentialSource::Plugin).allowed()
}

/// Starts the queued background attempts, past the stagger after the set's first ones.
fn pump_later(manager: &mut ClusterManager) {
    manager.pump_members(Instant::now() + Duration::from_secs(60));
}

// With background sign-in consent, a member that is not selected connects at once, under the
// consent's rules, and runs its plugin with no one to answer it.
#[tokio::test]
async fn a_consented_member_connects_in_the_background() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;

    open_set(&mut manager);

    assert_eq!(dev_state(&manager), MemberState::Connecting);
    let slot = manager.cluster("dev").unwrap();
    assert!(slot.access.unattended(), "connected under the consent");
    assert!(matches!(slot.state, ConnectionState::Connecting));
    assert!(dev_may_run(&manager));
    plugin.wait_started().await;
}

// Without consent, a member that is not selected waits for a sign-in and runs nothing.
#[tokio::test]
async fn a_member_without_consent_waits_for_a_sign_in() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");

    open_set(&mut manager);

    assert!(matches!(
        dev_state(&manager),
        MemberState::SignInNeeded { .. }
    ));
    assert!(manager.cluster("dev").unwrap().connect_task.is_none());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(plugin.starts().is_empty(), "the plugin never ran");
}

// Switching away from a consented member replaces its foreground connection with one under the
// consent, which later switches keep.
#[tokio::test]
async fn a_switch_keeps_a_consented_member_connected() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    allow(&mut manager).await;
    open_set(&mut manager);
    let foreground = manager
        .cluster("dev")
        .unwrap()
        .access
        .grant(CredentialSource::Plugin);
    assert!(!manager.cluster("dev").unwrap().access.unattended());
    plugin.wait_started().await;

    manager.activate("other".to_owned());
    pump_later(&mut manager);

    assert!(!foreground.allowed(), "the foreground connection ended");
    assert_eq!(dev_state(&manager), MemberState::Connecting);
    let slot = manager.cluster("dev").unwrap();
    assert!(slot.access.unattended(), "replaced under the consent");
    assert!(dev_may_run(&manager));
    let attempt = slot.attempt;

    manager.activate("dev".to_owned());
    assert!(dev_may_run(&manager), "selected, it goes on");
    manager.activate("other".to_owned());
    assert!(dev_may_run(&manager), "and again in the background");
    assert_eq!(
        manager.cluster("dev").unwrap().attempt,
        attempt,
        "not replaced"
    );
    assert_eq!(dev_state(&manager), MemberState::Connecting);
}

// Without consent, switching away ends the member's connection, and nothing it is replaced
// with may run the plugin.
#[tokio::test]
async fn a_switch_demotes_a_member_without_consent() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    open_set(&mut manager);
    let foreground = manager
        .cluster("dev")
        .unwrap()
        .access
        .grant(CredentialSource::Plugin);
    plugin.wait_started().await;

    manager.activate("other".to_owned());
    pump_later(&mut manager);

    assert!(!foreground.allowed(), "the foreground connection ended");
    assert!(!manager.cluster("dev").unwrap().access.unattended());
    assert!(!dev_may_run(&manager));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(plugin.starts().len(), 1, "only the foreground run");
}

// Selected, a member's connection made under consent connects again in the foreground when the
// consent ends, rather than stay signed out.
#[tokio::test]
async fn a_selected_member_reconnects_when_its_consent_ends() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    manager.activate("dev".to_owned());
    assert!(manager.cluster("dev").unwrap().access.unattended());

    manager.kept_grants().revoke("dev");
    manager.set_keep_watching("dev".to_owned(), None).await;
    manager.sync_keep_watching();

    let slot = manager.cluster("dev").unwrap();
    assert!(!slot.access.unattended());
    assert!(matches!(slot.state, ConnectionState::Connecting));
    assert!(dev_may_run(&manager), "in the foreground");
}

// Leaving the set drops a connection made under consent: it covers members only.
#[tokio::test]
async fn leaving_the_set_drops_a_consented_connection() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    let consented = manager
        .cluster("dev")
        .unwrap()
        .access
        .grant(CredentialSource::Plugin);

    manager.set_running_set(Vec::new());

    assert!(!consented.allowed());
    let slot = manager.cluster("dev").unwrap();
    assert!(matches!(slot.state, ConnectionState::Disconnected));
    assert!(slot.connect_task.is_none());
}

/// A plugin that notes each start in `starts` and answers with a token.
fn token_plugin() -> FakePlugin {
    FakePlugin::new(
        "echo $$ >> \"$PLUGIN_DIR/starts\"\n\
         printf '{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
         \"kind\":\"ExecCredential\",\"status\":{\"token\":\"t\"}}'\n",
    )
}

/// Waits for `plugin`'s first start.
async fn wait_for_a_start(plugin: &FakePlugin) {
    for _ in 0..250 {
        if !plugin.starts().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the plugin did not start");
}

/// Lands `dev`'s attempt in flight as connected, on a client that contacts nothing until used.
/// The slot keeps the access the attempt was made under; the client itself is built without it.
async fn land_dev(manager: &mut ClusterManager) {
    land(manager, "dev").await;
}

/// [`land_dev`] for `id`.
async fn land(manager: &mut ClusterManager, id: &str) {
    let slot = manager.cluster_mut(id).unwrap();
    if let Some(task) = slot.connect_task.take() {
        task.abort();
    }
    let attempt = slot.attempt;
    let access = CredentialAccess::always();
    let connection = cluster::connect_gated_with_policy(&manager.kubeconfig.merged, id, &access)
        .await
        .expect("building a client contacts nothing");
    let version = cluster::ServerVersion {
        git_version: "v1.30.0".into(),
        platform: "linux/arm64".into(),
    };
    manager
        .handle_internal(Internal::Connected {
            cluster: id.into(),
            attempt,
            result: Ok((connection, version)),
        })
        .await;
}

// A consented member of the open set runs its plugin once: its slot's connection, made under the
// consent, also serves keep watching, so no kept connection runs the plugin a second time.
#[tokio::test]
async fn one_plugin_run_serves_the_member_and_keep_watching() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    manager.sync_keep_watching();
    wait_for_a_start(&plugin).await;

    assert!(
        !manager.keep.feeds.contains_key("dev"),
        "no kept connection"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(plugin.starts().len(), 1, "only the member's run");

    land_dev(&mut manager).await;
    manager.sync_telemetry();
    manager.sync_keep_watching();
    assert!(manager.cluster("dev").unwrap().access.unattended());
    assert!(
        manager.cluster("dev").unwrap().telemetry.is_some(),
        "the slot's feed serves keep watching"
    );
    assert!(!manager.keep.feeds.contains_key("dev"));
    assert_eq!(manager.keep_status("dev"), KeepWatching::Watching);
}

// A member that waits for a sign-in makes no connection, so consent given later starts the kept
// feed rather than leave the cluster unwatched.
#[tokio::test]
async fn a_member_waiting_for_a_sign_in_leaves_keep_watching_to_the_kept_feed() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    open_set(&mut manager);
    assert!(matches!(
        dev_state(&manager),
        MemberState::SignInNeeded { .. }
    ));

    allow(&mut manager).await;
    manager.sync_keep_watching();

    assert!(matches!(
        dev_state(&manager),
        MemberState::SignInNeeded { .. }
    ));
    assert!(manager.keep.feeds.contains_key("dev"), "the kept feed runs");
}

// A consented member's credential failing in the background waits for a sign-in, and keep
// watching with it: no kept feed runs the plugin again until the person acts.
#[tokio::test]
async fn a_members_failed_credential_stops_keep_watching_too() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    land_dev(&mut manager).await;
    let attempt = manager.cluster("dev").unwrap().attempt;

    manager.member_credential_failed("dev", attempt);
    manager.sync_keep_watching();

    assert!(matches!(
        dev_state(&manager),
        MemberState::SignInNeeded { .. }
    ));
    assert_eq!(manager.keep_status("dev"), KeepWatching::SignInNeeded);
    assert!(!manager.keep.feeds.contains_key("dev"), "no second run");

    manager.set_running_set(Vec::new());
    manager.sync_keep_watching();
    assert!(!manager.keep.feeds.contains_key("dev"), "nor after leaving");
}

// The same for a background attempt under consent that fails to connect.
#[tokio::test]
async fn a_members_failed_attempt_stops_keep_watching_too() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    let slot = manager.cluster_mut("dev").unwrap();
    if let Some(task) = slot.connect_task.take() {
        task.abort();
    }
    let attempt = slot.attempt;

    manager
        .handle_internal(Internal::Connected {
            cluster: "dev".into(),
            attempt,
            result: Err(cluster::SafeErrorText::reduced("sign-in failed")),
        })
        .await;
    manager.sync_keep_watching();

    assert!(matches!(
        dev_state(&manager),
        MemberState::SignInNeeded { .. }
    ));
    assert_eq!(manager.keep_status("dev"), KeepWatching::SignInNeeded);
    assert!(!manager.keep.feeds.contains_key("dev"), "no second run");
}

// When the member leaves the set, its connection ends and the kept feed takes over.
#[tokio::test]
async fn the_kept_feed_resumes_when_the_member_leaves_the_set() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    land_dev(&mut manager).await;
    manager.sync_telemetry();
    manager.sync_keep_watching();
    assert!(manager.cluster("dev").unwrap().telemetry.is_some());
    assert!(!manager.keep.feeds.contains_key("dev"));

    manager.set_running_set(Vec::new());
    manager.sync_telemetry();
    manager.sync_keep_watching();

    assert!(manager.cluster("dev").unwrap().telemetry.is_none());
    let kept = &manager.keep.feeds["dev"];
    assert!(
        kept.runs_plugin,
        "the kept feed runs the plugin under the consent"
    );
    assert!(kept.access.unattended());
}

// A kubeconfig file change ends a member's sign-in still running, so a token from before the new
// definitions are read never brings its connection up.
#[tokio::test]
async fn a_file_change_ends_a_members_sign_in_in_progress() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    let consented = manager
        .cluster("dev")
        .unwrap()
        .access
        .grant(CredentialSource::Plugin);
    let (leader, _) = plugin.wait_started().await;
    let sign_in = consented.lease();

    manager
        .handle_internal(Internal::FilesChanged(Changed {
            kubeconfig: true,
            settings: false,
            activity: false,
            rearm: false,
            kubeconfig_rewritten: true,
        }))
        .await;

    assert!(!sign_in.holds(), "the sign-in under way ended");
    tokio::time::timeout(Duration::from_secs(5), async {
        while live(leader) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the plugin should have been killed");
    // Whatever the attempt reports, the member does not come up on it.
    tokio::time::timeout(Duration::from_secs(5), async {
        while matches!(
            manager.cluster("dev").unwrap().state,
            ConnectionState::Connecting
        ) {
            let internal = manager.internal_rx.as_mut().unwrap();
            if let Ok(Some(message)) =
                tokio::time::timeout(Duration::from_millis(50), internal.recv()).await
            {
                manager.handle_internal(message).await;
            }
        }
    })
    .await
    .expect("the attempt should have ended");
    assert!(!matches!(
        manager.cluster("dev").unwrap().state,
        ConnectionState::Connected(_)
    ));
    assert_ne!(dev_state(&manager), MemberState::Live);
}

// A definition that no longer runs a command still ends the access of a connection made under
// consent: nothing demotes it, so the access must not stay live.
#[tokio::test]
async fn a_member_whose_credential_stops_running_a_command_loses_its_consent_access() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    let consented = manager
        .cluster("dev")
        .unwrap()
        .access
        .grant(CredentialSource::Plugin);
    assert!(consented.allowed());

    // The definition now shows a static token, read without replacing the slot.
    let path = plugin.dir.path().join("kubeconfig");
    let yaml = std::fs::read_to_string(&path)
        .unwrap()
        .replace(&plugin.user_yaml(), "    token: t\n");
    std::fs::write(&path, yaml).unwrap();
    manager.kubeconfig = Arc::new(crate::kubeconfig::load(std::slice::from_ref(&path)));
    assert!(!manager.runs_a_command("dev"));
    manager.sync_keep_watching();

    assert!(!consented.allowed());
    assert!(!manager.cluster("dev").unwrap().access.unattended() || !dev_may_run(&manager));
}

// Turning the consent off ends a member's connection made under it, and kills its plugin.
#[tokio::test]
async fn turning_consent_off_ends_a_members_connection() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    let consented = manager
        .cluster("dev")
        .unwrap()
        .access
        .grant(CredentialSource::Plugin);
    let (leader, _) = plugin.wait_started().await;

    manager.kept_grants().revoke("dev");
    manager.set_keep_watching("dev".to_owned(), None).await;
    manager.sync_keep_watching();
    pump_later(&mut manager);

    assert!(!consented.allowed());
    assert!(!manager.cluster("dev").unwrap().access.unattended());
    assert!(!dev_may_run(&manager));
    tokio::time::timeout(Duration::from_secs(5), async {
        while live(leader) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the plugin should have been killed");
}

/// Turns on keep watching, then `dev`'s background sign-in toggle.
async fn allow(manager: &mut ClusterManager) {
    let view = manager.keep_watching_views()["dev"].clone();
    manager
        .set_keep_watching("dev".to_owned(), view.definition)
        .await;
    let shown = view.shown_sign_in.expect("a plugin definition");
    let offer = manager.offer_keep_watching("dev", shown).unwrap();
    manager.approve_keep_watching(offer.approve()).await;
}

// A file event ends a running plugin before the reload, and an unrelated edit is no failure.
#[tokio::test]
async fn a_detected_edit_ends_a_running_plugin_without_calling_it_a_failure() {
    let plugin = FakePlugin::hanging();
    let config = tempfile::tempdir().unwrap();
    let (mut manager, _path) = manager_with(&plugin, config.path().to_owned());
    allow(&mut manager).await;
    manager.sync_keep_watching();
    let (leader, _) = plugin.wait_started().await;

    manager
        .handle_internal(Internal::FilesChanged(Changed {
            kubeconfig: true,
            settings: false,
            activity: false,
            rearm: false,
            kubeconfig_rewritten: true,
        }))
        .await;

    tokio::time::timeout(Duration::from_secs(5), async {
        while live(leader) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the plugin should have been killed");
    let internal = manager.internal_rx.as_mut().unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), internal.recv())
        .await
        .expect("the attempt reports")
        .expect("a message");
    manager.handle_internal(message).await;
    assert_ne!(manager.keep_status("dev"), KeepWatching::SignInNeeded);
}

// What was written before new watches started went unseen, so a forward can't vouch for it.
#[tokio::test]
async fn new_file_watches_end_running_forwards() {
    let plugin = FakePlugin::new("exit 1\n");
    let config = tempfile::tempdir().unwrap();
    let (mut manager, _path) = manager_with(&plugin, config.path().to_owned());
    let ended = manager.forward_for_tests("dev");

    manager
        .handle_internal(Internal::WatchesStarted(Default::default()))
        .await;

    assert!(ended.borrow().is_some());
}

// A saved entry holds for the route it was turned on for: another proxy needs confirming.
#[tokio::test]
async fn another_proxy_needs_confirming_again() {
    let plugin = FakePlugin::new("exit 1\n");
    let config = tempfile::tempdir().unwrap();
    let (mut manager, _path) = manager_with(&plugin, config.path().to_owned());
    allow(&mut manager).await;
    assert_eq!(manager.keep_status("dev"), KeepWatching::Watching);

    manager.clusters[0].info.proxy_url = Some("http://proxy.example:3128".to_owned());

    assert_eq!(manager.keep_status("dev"), KeepWatching::NeedsConfirming);
}

/// An empty kubeconfig file next to the plugin.
fn extra_file(plugin: &FakePlugin) -> PathBuf {
    let extra = plugin.dir.path().join("extra");
    std::fs::write(&extra, "apiVersion: v1\nkind: Config\n").unwrap();
    extra
}

// Another file list can define the context differently: the grant ends with the old list.
#[tokio::test]
async fn a_changed_file_list_retires_the_grant() {
    let plugin = FakePlugin::new("exit 1\n");
    let config = tempfile::tempdir().unwrap();
    let (mut manager, _path) = manager_with(&plugin, config.path().to_owned());
    allow(&mut manager).await;
    let shown = manager.keep_watching_views()["dev"].shown_sign_in.unwrap();
    let pending = manager.offer_keep_watching("dev", shown).unwrap().approve();

    let settings = Settings {
        kubeconfig_paths: vec![extra_file(&plugin)],
        ..(*manager.settings).clone()
    };
    manager.apply_settings(Ok(settings)).await;
    manager.approve_keep_watching(pending).await;
    manager.sync_keep_watching();

    assert!(!manager.keep.consents.contains_key("dev"));
    assert_eq!(manager.keep_status("dev"), KeepWatching::NeedsConfirming);
}

// A saved list the manager has not read yet already decides what a launch may trust.
#[tokio::test]
async fn a_file_list_changed_on_disk_refuses_the_launch() {
    let plugin = FakePlugin::new("exit 1\n");
    let config = tempfile::tempdir().unwrap();
    let (mut manager, _path) = manager_with(&plugin, config.path().to_owned());
    allow(&mut manager).await;
    let consent = Arc::clone(&manager.keep.consents["dev"]);
    let check = manager.launch_check("dev", Arc::clone(&consent));
    assert!(check.refusal().await.is_ok(), "the same list launches");

    let extra = extra_file(&plugin);
    crate::settings::edit_settings(config.path(), |table, _| {
        let path = toml::Value::String(extra.display().to_string());
        table.insert("kubeconfig_paths".into(), toml::Value::Array(vec![path]));
        Ok(())
    })
    .unwrap();

    assert!(check.refusal().await.is_err());
}

// Undoing an edit a launch already caught brings back neither the consent nor an open approval.
#[tokio::test]
async fn a_mismatch_at_launch_ends_consent_and_pending_approvals() {
    let plugin = FakePlugin::new("exit 1\n");
    let config = tempfile::tempdir().unwrap();
    let (mut manager, path) = manager_with(&plugin, config.path().to_owned());
    allow(&mut manager).await;
    let shown = manager.keep_watching_views()["dev"].shown_sign_in.unwrap();
    let pending = manager.offer_keep_watching("dev", shown).unwrap().approve();
    let consent = Arc::clone(&manager.keep.consents["dev"]);
    let check = manager.launch_check("dev", consent);

    let original = std::fs::read_to_string(&path).unwrap();
    let edited = original.replace("user: u\n", "user: u\n    namespace: elsewhere\n");
    assert_ne!(original, edited);
    std::fs::write(&path, edited).unwrap();
    assert!(check.refusal().await.is_err());
    std::fs::write(&path, original).unwrap();

    assert!(check.refusal().await.is_err(), "undoing it changes nothing");
    manager.approve_keep_watching(pending).await;
    manager.sync_keep_watching();
    assert!(!manager.keep.consents.contains_key("dev"));
    assert_eq!(manager.keep_status("dev"), KeepWatching::NeedsConfirming);
}

// A file that can't be read confirms no change: the launch waits, and the consent stands.
#[tokio::test]
async fn an_unreadable_file_refuses_the_launch_and_keeps_consent() {
    let plugin = FakePlugin::new("exit 1\n");
    let config = tempfile::tempdir().unwrap();
    let (mut manager, path) = manager_with(&plugin, config.path().to_owned());
    allow(&mut manager).await;
    let check = manager.launch_check("dev", Arc::clone(&manager.keep.consents["dev"]));

    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, "not: [valid").unwrap();
    assert!(check.refusal().await.is_err());
    std::fs::write(&path, original).unwrap();

    assert!(check.refusal().await.is_ok());
    manager.sync_keep_watching();
    assert!(manager.keep.consents.contains_key("dev"));
}

// A slot that started over with the same definition gets a connection of its own: a feed of the
// earlier slot would be refused by every alert pass and never replaced.
#[tokio::test]
async fn a_new_slot_generation_replaces_the_kept_connection() {
    let plugin = FakePlugin::new("exit 1\n");
    let config = tempfile::tempdir().unwrap();
    let (mut manager, _path) = manager_with(&plugin, config.path().to_owned());
    allow(&mut manager).await;
    manager.sync_keep_watching();
    let first = manager.keep.feeds["dev"].attempt;

    manager.sync_keep_watching();
    assert_eq!(manager.keep.feeds["dev"].attempt, first, "nothing changed");

    manager.clusters[0].generation += 1;
    manager.sync_keep_watching();
    assert_ne!(manager.keep.feeds["dev"].attempt, first);
    assert_eq!(
        manager.keep.feeds["dev"].generation,
        Some(manager.clusters[0].generation)
    );
}

// No path brings consent back: an entry removed by hand and put back has none.
#[tokio::test]
async fn consent_is_not_restored_with_its_entry() {
    let plugin = FakePlugin::new("exit 1\n");
    let config = tempfile::tempdir().unwrap();
    let (mut manager, _path) = manager_with(&plugin, config.path().to_owned());
    allow(&mut manager).await;
    assert!(manager.keep.consents.contains_key("dev"));

    let with_entry = std::mem::replace(&mut manager.settings, Arc::new(Settings::default()));
    manager.sync_keep_watching();
    assert!(!manager.keep.consents.contains_key("dev"));
    manager.settings = with_entry;
    manager.sync_keep_watching();
    assert_eq!(manager.keep_status("dev"), KeepWatching::NeedsConfirming);
}

/// Lands `id`'s kept connection in flight, on a client that contacts nothing until used, and
/// returns the attempt it was made by.
async fn land_kept(manager: &mut ClusterManager, id: &str) -> u64 {
    let kept = manager.keep.feeds.get_mut(id).expect("a kept feed");
    if let Some(task) = kept.connecting.take() {
        task.abort();
    }
    let attempt = kept.attempt;
    let access = CredentialAccess::always();
    let client = cluster::connect_gated_with_policy(&manager.kubeconfig.merged, id, &access)
        .await
        .expect("building a client contacts nothing")
        .clients
        .read;
    manager.kept_connected(id, attempt, Ok(client));
    assert!(manager.keep.feeds[id].feed.is_some(), "the kept feed runs");
    attempt
}

/// The loop's syncs after a message.
fn sync(manager: &mut ClusterManager) {
    manager.sync_keep_watching();
    manager.sync_telemetry();
}

/// Turns on keep watching for `other`, whose credential runs nothing, and lands its kept feed.
/// Returns the feed's attempt.
async fn watched_other(manager: &mut ClusterManager) -> u64 {
    let definition = manager.keep_watching_views()["other"].definition;
    manager
        .set_keep_watching("other".to_owned(), definition)
        .await;
    sync(manager);
    assert_eq!(manager.keep_status("other"), KeepWatching::Watching);
    land_kept(manager, "other").await
}

/// Selects `id` and lands its slot's connection, as the loop would.
async fn select_and_land(manager: &mut ClusterManager, id: &str) {
    manager.activate(id.to_owned());
    sync(manager);
    land(manager, id).await;
    sync(manager);
}

// Selecting a watched cluster hands its warm feed over: the slot starts no feed of its own, so
// nothing is listed again.
#[tokio::test]
async fn selecting_a_watched_cluster_hands_its_feed_over() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    let attempt = watched_other(&mut manager).await;

    select_and_land(&mut manager, "other").await;

    assert!(manager.kept_feed_handed_over("other"));
    let kept = &manager.keep.feeds["other"];
    assert_eq!(kept.attempt, attempt, "the same feed");
    assert!(kept.feed.is_some());
    let slot = manager.cluster("other").unwrap();
    assert!(matches!(slot.state, ConnectionState::Connected(_)));
    assert!(slot.telemetry.is_none(), "no second feed on the slot");
    assert!(
        slot.write_credential.is_some(),
        "writes still go through the slot"
    );
}

// Leaving it again keeps the same feed running in the background.
#[tokio::test]
async fn leaving_a_handed_over_cluster_keeps_its_feed_warm() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    let attempt = watched_other(&mut manager).await;
    select_and_land(&mut manager, "other").await;

    manager.activate("dev".to_owned());
    sync(&mut manager);

    assert!(!manager.kept_feed_handed_over("other"));
    let kept = &manager.keep.feeds["other"];
    assert_eq!(kept.attempt, attempt, "no new connection");
    assert!(kept.feed.is_some());
    assert!(manager.cluster("other").unwrap().telemetry.is_none());
}

// A changed definition drops the handed-over feed, and the slot starts a feed of its own.
#[tokio::test]
async fn a_definition_change_drops_a_handed_over_feed() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    watched_other(&mut manager).await;
    select_and_land(&mut manager, "other").await;
    assert!(manager.kept_feed_handed_over("other"));

    let slot = manager.cluster_mut("other").unwrap();
    slot.fingerprint = slot.fingerprint.map(|print| print.wrapping_add(1));
    assert!(
        !manager.kept_feed_handed_over("other"),
        "never handed over on another definition"
    );
    sync(&mut manager);

    assert!(
        !manager.keep.feeds.contains_key("other"),
        "the feed is dropped"
    );
    assert!(
        manager.cluster("other").unwrap().telemetry.is_some(),
        "the slot's own feed takes over"
    );
}

// Ending a plugin's background sign-in consent drops the handed-over feed, and none starts in
// the background without it.
#[tokio::test]
async fn ending_consent_drops_a_handed_over_feed() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    sync(&mut manager);
    let attempt = land_kept(&mut manager, "dev").await;
    select_and_land(&mut manager, "dev").await;
    assert!(manager.kept_feed_handed_over("dev"));
    assert_eq!(manager.keep.feeds["dev"].attempt, attempt);
    assert!(manager.cluster("dev").unwrap().telemetry.is_none());

    manager.kept_grants().revoke("dev");
    manager.revoke_background_sign_in("dev");
    sync(&mut manager);

    assert!(!manager.kept_feed_handed_over("dev"));
    assert!(
        !manager.keep.feeds.contains_key("dev"),
        "the feed is dropped"
    );
    manager.activate("other".to_owned());
    sync(&mut manager);
    assert!(
        !manager.keep.feeds.contains_key("dev"),
        "no background feed without consent"
    );
}

/// Gives `id` discovery of pods, config maps and events, in watch mode All, as its connect would.
/// Returns those kinds.
fn discovered(manager: &mut ClusterManager, id: &str) -> [KindId; 3] {
    use crate::discovery::tests::kind;
    let verbs = &["get", "list", "watch"];
    let kinds = vec![
        kind("", "Pod", "pods", true, verbs),
        kind("", "ConfigMap", "configmaps", true, verbs),
        kind("", "Event", "events", true, verbs),
    ];
    let ids = [kinds[0].id(), kinds[1].id(), kinds[2].id()];
    let slot = manager.cluster_mut(id).unwrap();
    slot.watches.set_kinds(&kinds);
    slot.discovery = Some(Arc::new(crate::discovery::ClusterDiscovery::new(kinds)));
    manager.reconcile(id);
    ids
}

/// The task watching `kind` on `id`, if one runs.
fn task(manager: &ClusterManager, id: &str, kind: &KindId) -> Option<tokio::task::Id> {
    manager
        .cluster(id)
        .unwrap()
        .watches
        .tasks()
        .get(kind)
        .copied()
}

// A watched cluster's resource watches read through its kept connection: leaving it keeps them
// running, without the high-churn kinds, and selecting it again starts none of them over, so
// nothing is listed again. Writes still go through the slot.
#[tokio::test]
async fn a_switch_keeps_a_watched_clusters_lists_warm() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    watched_other(&mut manager).await;
    select_and_land(&mut manager, "other").await;
    let [pods, config_maps, events] = discovered(&mut manager, "other");
    assert!(manager.lists_warm("other"));
    let pods_task = task(&manager, "other", &pods).expect("pods are watched");
    let maps_task = task(&manager, "other", &config_maps).expect("config maps are watched");
    assert!(task(&manager, "other", &events).is_some());
    // A view showed config maps a moment ago.
    open_view(&mut manager, 1, "other", &config_maps);
    close_view(&mut manager, 1, "other");

    manager.activate("dev".to_owned());
    sync(&mut manager);
    assert!(manager.lists_warm("other"), "warm in the background");
    assert_eq!(
        task(&manager, "other", &pods),
        Some(pods_task),
        "a core kind"
    );
    assert_eq!(
        task(&manager, "other", &config_maps),
        Some(maps_task),
        "shown last"
    );
    assert!(
        task(&manager, "other", &events).is_none(),
        "high-churn kinds stop in the background"
    );

    manager.activate("other".to_owned());
    sync(&mut manager);
    assert_eq!(
        task(&manager, "other", &pods),
        Some(pods_task),
        "not listed again"
    );
    assert_eq!(task(&manager, "other", &config_maps), Some(maps_task));
    assert!(task(&manager, "other", &events).is_some(), "back in front");
    let slot = manager.cluster("other").unwrap();
    assert!(matches!(slot.state, ConnectionState::Connected(_)));
    assert!(
        slot.write_credential.is_some(),
        "writes go through the slot"
    );
}

/// Opens view `view` on `kind` of `id`, as a subscription would.
fn open_view(manager: &mut ClusterManager, view: u64, id: &str, kind: &KindId) {
    manager
        .subscriptions
        .insert(view, (id.to_owned(), kind.clone(), None));
    manager.reconcile(id);
}

/// Closes view `view` of `id`.
fn close_view(manager: &mut ClusterManager, view: u64, id: &str) {
    manager.subscriptions.remove(&view);
    manager.reconcile(id);
}

// In the background a cluster keeps at most a few kinds: those its views showed last, before
// the core kinds, and none that no view showed.
#[tokio::test]
async fn a_background_cluster_keeps_only_its_last_shown_kinds() {
    use crate::discovery::tests::kind;
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    watched_other(&mut manager).await;
    select_and_land(&mut manager, "other").await;
    let verbs = &["get", "list", "watch"];
    let mut kinds = vec![kind("", "Pod", "pods", true, verbs)];
    for n in 0..=WARM_KINDS + 1 {
        let plural = format!("widgets{n}");
        kinds.push(kind("example.com", "Widget", &plural, true, verbs));
    }
    let ids: Vec<KindId> = kinds.iter().map(|kind| kind.id()).collect();
    let slot = manager.cluster_mut("other").unwrap();
    slot.watches.set_kinds(&kinds);
    slot.discovery = Some(Arc::new(crate::discovery::ClusterDiscovery::new(kinds)));
    manager.reconcile("other");
    assert_eq!(
        manager.cluster("other").unwrap().watches.tasks().len(),
        ids.len()
    );
    // Views show every widget but the last, one after another.
    let widgets = &ids[1..ids.len() - 1];
    for (view, widget) in widgets.iter().enumerate() {
        open_view(&mut manager, view as u64, "other", widget);
        close_view(&mut manager, view as u64, "other");
    }

    manager.activate("dev".to_owned());
    sync(&mut manager);

    let running = manager.cluster("other").unwrap().watches.tasks();
    assert_eq!(running.len(), WARM_KINDS, "{running:?}");
    for widget in &widgets[widgets.len() - WARM_KINDS..] {
        assert!(running.contains_key(widget), "{widget} was shown last");
    }
    assert!(!running.contains_key(&ids[0]), "no room left for pods");
    assert!(!running.contains_key(&ids[1]), "shown first");
    assert!(!running.contains_key(&ids[ids.len() - 1]), "never shown");
}

// Disconnecting a watched cluster in the background stops its warm lists before the settings
// are written: nothing reads through its kept connection once the disconnect is under way.
#[tokio::test]
async fn disconnect_stops_warm_lists_at_once() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    watched_other(&mut manager).await;
    select_and_land(&mut manager, "other").await;
    let [pods, ..] = discovered(&mut manager, "other");
    manager.activate("dev".to_owned());
    sync(&mut manager);
    assert!(task(&manager, "other", &pods).is_some(), "warm");

    manager.disconnect_cluster("other".to_owned()).await;

    assert!(!manager.lists_warm("other"));
    assert!(manager.cluster("other").unwrap().watches.tasks().is_empty());
}

// Ending background sign-in consent, or keep watching, stops warm lists in the background before
// the loop's next sync.
#[tokio::test]
async fn ending_consent_or_keep_watching_stops_warm_lists_at_once() {
    for turn_off in [false, true] {
        let plugin = token_plugin();
        let config = tempfile::tempdir().unwrap();
        let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
        allow(&mut manager).await;
        sync(&mut manager);
        land_kept(&mut manager, "dev").await;
        select_and_land(&mut manager, "dev").await;
        let [pods, ..] = discovered(&mut manager, "dev");
        manager.activate("other".to_owned());
        sync(&mut manager);
        assert!(task(&manager, "dev", &pods).is_some(), "warm");

        if turn_off {
            manager.set_keep_watching("dev".to_owned(), None).await;
        } else {
            manager.kept_grants().revoke("dev");
            manager.revoke_background_sign_in("dev");
        }

        assert!(!manager.lists_warm("dev"), "turn_off={turn_off}");
        assert!(manager.cluster("dev").unwrap().watches.tasks().is_empty());
    }
}

// A cluster that is not watched drops its watches when it is left, as before.
#[tokio::test]
async fn an_unwatched_cluster_keeps_no_lists_warm() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    select_and_land(&mut manager, "other").await;
    let [pods, ..] = discovered(&mut manager, "other");
    assert!(!manager.lists_warm("other"));
    assert!(task(&manager, "other", &pods).is_some());

    manager.activate("dev".to_owned());
    sync(&mut manager);
    assert!(manager.cluster("other").unwrap().watches.tasks().is_empty());
}

// A changed definition drops warm lists: the selected cluster's watches start over on the slot's
// own connection, and none reads through a connection made for the old definition.
#[tokio::test]
async fn a_definition_change_drops_warm_lists() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    watched_other(&mut manager).await;
    select_and_land(&mut manager, "other").await;
    let [pods, ..] = discovered(&mut manager, "other");
    let warm = task(&manager, "other", &pods).unwrap();

    let slot = manager.cluster_mut("other").unwrap();
    slot.fingerprint = slot.fingerprint.map(|print| print.wrapping_add(1));
    sync(&mut manager);

    assert!(!manager.lists_warm("other"));
    let now = task(&manager, "other", &pods).expect("still watched on the slot");
    assert_ne!(now, warm, "started over on the slot's connection");
}

// Ending a plugin's background sign-in consent drops the warm lists of a cluster in the
// background: its watches stop at once.
#[tokio::test]
async fn ending_consent_stops_warm_lists_in_the_background() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    sync(&mut manager);
    land_kept(&mut manager, "dev").await;
    select_and_land(&mut manager, "dev").await;
    let [pods, ..] = discovered(&mut manager, "dev");
    assert!(manager.lists_warm("dev"));
    manager.activate("other".to_owned());
    sync(&mut manager);
    assert!(
        task(&manager, "dev", &pods).is_some(),
        "warm in the background"
    );

    manager.kept_grants().revoke("dev");
    manager.revoke_background_sign_in("dev");
    sync(&mut manager);

    assert!(!manager.lists_warm("dev"));
    assert!(manager.cluster("dev").unwrap().watches.tasks().is_empty());
}

// At most a few clusters keep warm lists in the background, the most recently left.
#[tokio::test]
async fn only_the_most_recently_left_clusters_keep_warm_lists() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..=WARM_LIST_CLUSTERS).map(|n| format!("w{n}")).collect();
    let more: String = names
        .iter()
        .map(|name| format!("- name: {name}\n  context:\n    cluster: c\n    user: s\n"))
        .collect();
    let (mut manager, _) = manager_on(&plugin, config.path().to_owned(), &more);
    manager.active = Some("dev".to_owned());
    manager.sync_roles();
    for name in &names {
        let definition = manager.keep_watching_views()[name].definition;
        manager.set_keep_watching(name.clone(), definition).await;
        sync(&mut manager);
        land_kept(&mut manager, name).await;
        select_and_land(&mut manager, name).await;
        discovered(&mut manager, name);
    }
    manager.activate("dev".to_owned());
    sync(&mut manager);

    assert!(!manager.lists_warm(&names[0]), "the least recent goes cold");
    assert!(
        manager
            .cluster(&names[0])
            .unwrap()
            .watches
            .tasks()
            .is_empty()
    );
    for name in &names[1..] {
        assert!(manager.lists_warm(name), "{name}");
    }
}

// A handed-over feed whose credential fails in the background while its cluster is selected
// ends without stopping keep watching: the slot's working connection starts a feed in its place,
// even in On view with no overview open, and leaving the cluster starts a kept feed again.
#[tokio::test]
async fn a_handed_over_feed_failing_while_selected_falls_back_to_the_slot() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    manager.settings = Arc::new(Settings {
        watch_mode: crate::settings::WatchMode::OnView,
        ..(*manager.settings).clone()
    });
    allow(&mut manager).await;
    sync(&mut manager);
    let attempt = land_kept(&mut manager, "dev").await;
    select_and_land(&mut manager, "dev").await;
    assert!(manager.kept_feed_handed_over("dev"));

    manager.kept_failed("dev", attempt);
    sync(&mut manager);

    assert_eq!(manager.keep_status("dev"), KeepWatching::Watching);
    assert!(
        !manager.keep.feeds.contains_key("dev"),
        "none in the background"
    );
    assert!(
        manager.cluster("dev").unwrap().telemetry.is_some(),
        "the slot's feed keeps the alerts going"
    );

    manager.activate("other".to_owned());
    sync(&mut manager);
    let kept = manager.keep.feeds.get("dev").expect("a kept feed again");
    assert_ne!(kept.attempt, attempt);
    assert_eq!(manager.keep_status("dev"), KeepWatching::Watching);
}

// The same failure while the cluster is in the background waits for a sign-in, as before.
#[tokio::test]
async fn a_kept_feed_failing_in_the_background_waits_for_a_sign_in() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    sync(&mut manager);
    let attempt = land_kept(&mut manager, "dev").await;

    manager.kept_failed("dev", attempt);
    sync(&mut manager);

    assert_eq!(manager.keep_status("dev"), KeepWatching::SignInNeeded);
    assert!(!manager.keep.feeds.contains_key("dev"));
}
