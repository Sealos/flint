use std::path::PathBuf;

use super::*;
use crate::auth_plugin::tests::{FakePlugin, live};
use crate::handle::CoreConfig;
use crate::manager::{Changed, Cluster};
use crate::protection::SessionUnlocks;
use crate::settings::Settings;
use crate::watch::HubRegistry;

/// A manager with one context, `dev`, signing in with `plugin`, and nothing selected.
fn manager_with(plugin: &FakePlugin, config_dir: PathBuf) -> (ClusterManager, PathBuf) {
    let path = plugin.dir.path().join("kubeconfig");
    let yaml = format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
         http://127.0.0.1:1\nusers:\n- name: u\n  user:\n{}contexts:\n- name: dev\n  \
         context:\n    cluster: c\n    user: u\n",
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
    let slot = Cluster::new(
        loaded.contexts[0].clone(),
        cluster::context_fingerprint(&loaded.merged, "dev"),
        &hubs,
        &manager.engine_config,
    );
    manager.contexts.record(&loaded);
    manager.kubeconfig = Arc::new(loaded);
    manager.kubeconfig_paths = vec![path.clone()];
    manager.clusters.push(slot);
    (manager, path)
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
