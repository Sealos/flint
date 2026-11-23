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
