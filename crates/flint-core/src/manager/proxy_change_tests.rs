//! Tests of putting a new proxy setting in use. A test that replaces the setting runs in a child
//! process of its own, since the setting in use is the process's.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{Pending, ProxyChange};
use crate::cluster::{SafeErrorText, ServerVersion};
use crate::connection::proxy;
use crate::handle::{ConnectionState, CoreConfig, ProxyReconnectReport};
use crate::manager::ClusterManager;
use crate::protection::SessionUnlocks;
use crate::recheck::Pinned;
use crate::settings::{ProxySetting, Settings};
use crate::watch::HubRegistry;

/// Set for the child process a test runs in.
const CHILD: &str = "FLINT_TEST_PROXY_CHANGE_CHILD";

/// Runs `test` of this module in a child process with no proxy in its environment.
fn in_child(test: &str) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            format!("manager::proxy_change::tests::{test}").as_str(),
            "--exact",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed in its child process");
}

fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// `dev` takes the proxy setting; `own` has a `proxy-url` of its own, which always wins.
const KUBECONFIG: &str = "apiVersion: v1\nkind: Config\nclusters:\n- name: d\n  cluster:\n    \
     server: https://dev\n- name: o\n  cluster:\n    server: https://own\n    proxy-url: \
     http://own.example:3128\nusers:\n- name: u\n  user: {}\ncontexts:\n- name: dev\n  \
     context:\n    cluster: d\n    user: u\n- name: own\n  context:\n    cluster: o\n    user: u\n";

/// A manager that has read [`KUBECONFIG`], with its settings in `dir`. Nothing connects.
async fn manager_in(dir: &Path) -> ClusterManager {
    let path = dir.join("kubeconfig");
    std::fs::write(&path, KUBECONFIG).unwrap();
    let (events, _rx) = crate::events::channel();
    let config = CoreConfig {
        config_dir: dir.to_path_buf(),
        settings: Settings::default(),
        settings_error: None,
        kubeconfig_base: vec![path],
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    };
    let mut manager = ClusterManager::new(
        config,
        events,
        HubRegistry::default(),
        SessionUnlocks::default(),
    );
    manager.reload_kubeconfig().await;
    for id in ["dev", "own"] {
        manager.cluster_mut(id).expect("loaded").state =
            ConnectionState::Failed(SafeErrorText::fixed("offline"));
    }
    manager
}

fn setting(url: &str) -> ProxySetting {
    ProxySetting {
        url: Some(url.to_owned()),
        no_proxy: Vec::new(),
    }
}

fn settings_file(dir: &Path) -> PathBuf {
    dir.join("settings.toml")
}

// covers UP-125: a proxy Flint could not use is refused before anything is saved or changes,
// and the refusal holds none of its credentials.
#[tokio::test]
async fn a_setting_that_does_not_check_out_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager_in(dir.path()).await;
    let generation = proxy::generation();
    let before = manager.cluster("dev").unwrap().generation;

    for refused in [
        setting("ftp://user:hunter2@proxy.example:21"),
        setting("http://user:hunter2@[::1]junk:3128"),
        ProxySetting {
            url: None,
            no_proxy: vec!["a.example, b.example".into()],
        },
    ] {
        let error = manager.apply_proxy(refused).await.unwrap_err();
        assert!(!error.contains("hunter2"), "{error}");
    }
    assert!(!settings_file(dir.path()).exists(), "nothing was saved");
    assert_eq!(proxy::generation(), generation);
    assert_eq!(manager.cluster("dev").unwrap().generation, before);
}

#[test]
fn a_supported_setting_checks_out() {
    for url in [
        "http://proxy.example:3128",
        "http://user:pa%40ss@proxy.example:3128",
        "socks5://proxy.example",
        "SOCKS5H://proxy.example:1080",
    ] {
        assert_eq!(setting(url).check(), Ok(()), "{url}");
    }
    let unsupported = setting("https://user:hunter2@proxy.example:443")
        .check()
        .unwrap_err();
    assert!(unsupported.contains("https"), "{unsupported}");
    assert!(!unsupported.contains("hunter2"), "{unsupported}");
    let exempt = ProxySetting {
        url: None,
        no_proxy: vec![".example.com".into(), " 10.0.0.0/8 ".into(), "*".into()],
    };
    assert_eq!(exempt.check(), Ok(()));
}

#[test]
fn saving_a_setting_replaces_its_keys_and_keeps_the_file_to_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        settings_file(dir.path()),
        "appearance = \"dark\"\n[proxy]\nurl = \"http://old.example:3128\"\nlater = 1\n",
    )
    .unwrap();
    let saved = crate::settings::set_proxy(
        dir.path(),
        &ProxySetting {
            url: Some(" socks5h://new.example:1080 ".into()),
            no_proxy: vec!["internal.example".into(), " ".into()],
        },
    )
    .unwrap();
    assert_eq!(
        saved.proxy.url.as_deref(),
        Some("socks5h://new.example:1080")
    );
    assert_eq!(saved.proxy.no_proxy, ["internal.example"]);
    assert_eq!(saved.appearance, crate::settings::Appearance::Dark);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(settings_file(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{mode:o}: it can hold the proxy's password"
        );
    }

    let cleared = crate::settings::set_proxy(dir.path(), &ProxySetting::default()).unwrap();
    assert!(cleared.proxy.is_empty());
    let text = std::fs::read_to_string(settings_file(dir.path())).unwrap();
    assert!(
        !text.contains("url") && !text.contains("no_proxy"),
        "{text}"
    );
    assert!(
        text.contains("later = 1"),
        "a key of a later version survives: {text}"
    );

    std::fs::write(
        settings_file(dir.path()),
        "[proxy]\nurl = \"http://a.example:1\"\n",
    )
    .unwrap();
    crate::settings::set_proxy(dir.path(), &ProxySetting::default()).unwrap();
    let text = std::fs::read_to_string(settings_file(dir.path())).unwrap();
    assert!(!text.contains("proxy"), "an empty table goes: {text}");
}

// covers UP-125: a connection attempt started before a change goes on under the setting its
// load had, from its first request to its credential plugin.
#[test]
fn an_attempt_keeps_the_setting_it_started_under() {
    in_child("child_an_attempt_keeps_the_setting_it_started_under");
}

#[tokio::test]
async fn child_an_attempt_keeps_the_setting_it_started_under() {
    if !is_child() {
        return;
    }
    proxy::use_setting(&setting("http://old.example:3128"));
    proxy::replace_setting(&setting("http://new.example:3128")).unwrap();
    let chosen = |environment: proxy::ProxyEnvironment| environment.https_proxy;

    assert_eq!(
        chosen(proxy::ProxyEnvironment::current()).as_deref(),
        Some("http://new.example:3128")
    );
    let (generation, old) = proxy::pinned_to(0, async {
        proxy::ProxyEnvironment::current_with_generation()
    })
    .await;
    assert_eq!(generation, 0);
    assert_eq!(chosen(old).as_deref(), Some("http://old.example:3128"));
    let variables = proxy::pinned_to(0, async { proxy::plugin_setting_variables(None) }).await;
    assert!(
        variables
            .iter()
            .any(|(_, value)| value.as_deref() == Some("http://old.example:3128")),
        "the plugin is told the old proxy too"
    );
    // A generation this process never had takes no route through the setting.
    let unknown = proxy::pinned_to(7, async { proxy::ProxyEnvironment::current() }).await;
    let unknown = chosen(unknown).unwrap();
    assert!(proxy::ProxyTarget::parse(&unknown).is_err());
}

// covers UP-125: a proxy change saves the setting, rebuilds every slot on it, and leaves none
// held once a load under it is in. Only a context the setting reaches another way loses what
// was pinned to its old route.
#[test]
fn every_slot_is_rebuilt_on_the_new_setting() {
    in_child("child_every_slot_is_rebuilt_on_the_new_setting");
}

#[tokio::test]
async fn child_every_slot_is_rebuilt_on_the_new_setting() {
    if !is_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager_in(dir.path()).await;
    let generations: Vec<u64> = ["dev", "own"]
        .map(|id| manager.cluster(id).unwrap().generation)
        .to_vec();
    let dev_pin = manager.session_pin("dev");
    let own_pin = manager.session_pin("own");
    assert_eq!(dev_pin.proxy(), None, "dev starts direct");

    let applied = manager
        .apply_proxy(setting("http://user:hunter2@proxy.example:3128"))
        .await
        .unwrap();

    assert!(applied.loaded);
    assert_eq!(applied.rerouted, ["dev"]);
    assert_eq!(applied.forwards_ended, 0);
    assert!(!format!("{applied:?}").contains("hunter2"));
    assert_eq!(proxy::generation(), 1);
    for (id, before) in ["dev", "own"].into_iter().zip(generations) {
        let slot = manager.cluster(id).unwrap();
        assert_ne!(slot.generation, before, "{id} has a new slot");
        assert_eq!(
            slot.proxy_generation, 1,
            "{id} was loaded under the new setting"
        );
        assert!(
            slot.client.is_none(),
            "{id} kept no client of the old setting"
        );
        assert!(!manager.held(id), "{id} is not held once loaded");
    }
    let route = manager.cluster("dev").unwrap().info.proxy_url.clone();
    let route = route.expect("dev goes through the setting now");
    assert!(route.starts_with("http://proxy.example:3128#"), "{route}");
    assert!(
        !route.contains("hunter2") && !route.contains("user"),
        "{route}"
    );

    // A log stream on dev was pinned to its old route: it ends, saying why. One on own, whose
    // route did not move, goes on with the new slot's client.
    let ended = manager.contexts.check(Pinned::Logs, "dev", &dev_pin);
    assert!(
        ended
            .as_deref()
            .is_some_and(|why| why.contains("another proxy")),
        "{ended:?}"
    );
    assert_eq!(manager.contexts.check(Pinned::Logs, "own", &own_pin), None);

    let saved = std::fs::read_to_string(settings_file(dir.path())).unwrap();
    assert!(saved.contains("proxy.example:3128"), "{saved}");
    assert_eq!(
        manager.settings.proxy.shown_url().as_deref(),
        Some("http://proxy.example:3128")
    );
}

// covers UP-125: a slot loaded under the old setting starts nothing until a load under the new
// one replaces it, so no connection takes a route its slot does not show.
#[test]
fn a_slot_of_the_old_setting_is_held_until_a_load_replaces_it() {
    in_child("child_a_slot_of_the_old_setting_is_held_until_a_load_replaces_it");
}

#[tokio::test]
async fn child_a_slot_of_the_old_setting_is_held_until_a_load_replaces_it() {
    if !is_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager_in(dir.path()).await;
    let pin = manager.session_pin("own");
    assert!(!manager.held("own"));

    proxy::replace_setting(&setting("http://proxy.example:3128")).unwrap();
    for id in ["dev", "own"] {
        assert!(manager.held(id), "{id} is held");
        assert!(manager.paused(id), "{id} starts nothing");
        assert!(manager.unsettled(id), "{id} takes no write");
    }
    assert!(manager.log_client("own", &pin).is_err(), "no log client");

    manager.reload_kubeconfig().await;
    for id in ["dev", "own"] {
        assert!(!manager.held(id), "{id} is loaded under the setting in use");
    }
}

// covers UP-125: every port-forward ends when the proxy changes, saying why.
#[test]
fn every_forward_ends_when_the_proxy_changes() {
    in_child("child_every_forward_ends_when_the_proxy_changes");
}

#[tokio::test]
async fn child_every_forward_ends_when_the_proxy_changes() {
    if !is_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager_in(dir.path()).await;
    let dev = manager.forward_for_tests("dev");
    let own = manager.forward_for_tests("own");

    let applied = manager
        .apply_proxy(setting("socks5h://proxy.example:1080"))
        .await
        .unwrap();

    assert_eq!(applied.forwards_ended, 2);
    for ended in [dev, own] {
        let state = ended.borrow().clone();
        let Some(crate::port_forward::ForwardState::Failed(failure)) = state else {
            panic!("the forward did not end: {state:?}");
        };
        assert!(failure.message.contains("proxy"), "{}", failure.message);
    }
}

// covers UP-125: a proxy change refused at startup's unreadable settings.toml changes nothing.
#[test]
fn an_unreadable_start_keeps_the_setting_until_a_restart() {
    in_child("child_an_unreadable_start_keeps_the_setting_until_a_restart");
}

#[tokio::test]
async fn child_an_unreadable_start_keeps_the_setting_until_a_restart() {
    if !is_child() {
        return;
    }
    proxy::use_unreadable_setting();
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager_in(dir.path()).await;

    let error = manager
        .apply_proxy(setting("http://proxy.example:3128"))
        .await
        .unwrap_err();

    assert!(error.contains("restart"), "{error}");
    assert!(!settings_file(dir.path()).exists());
    assert!(proxy::setting_unreadable_at_startup());
    assert_eq!(proxy::generation(), 0);
}

fn version() -> ServerVersion {
    ServerVersion {
        git_version: "v1.30.0".into(),
        platform: "linux/arm64".into(),
    }
}

fn waiting_on(
    manager: &mut ClusterManager,
    ids: &[&str],
) -> tokio::sync::oneshot::Receiver<ProxyReconnectReport> {
    let (settled, rx) = tokio::sync::oneshot::channel();
    manager.proxy_change = ProxyChange {
        seq: 1,
        pending: Some(Pending {
            seq: 1,
            routes: Default::default(),
            waiting: ids
                .iter()
                .map(|id| (*id).to_owned())
                .collect::<BTreeSet<_>>(),
            report: ProxyReconnectReport::default(),
            settled,
        }),
    };
    rx
}

// covers UP-125: the report names each cluster that came back and each that failed, with why,
// once none is still connecting.
#[tokio::test]
async fn the_report_waits_for_each_reconnection() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager_in(dir.path()).await;
    manager.cluster_mut("dev").unwrap().state = ConnectionState::Connecting;
    let mut settled = waiting_on(&mut manager, &["dev", "own"]);

    manager.sync_proxy_report();
    assert!(settled.try_recv().is_err(), "dev is still connecting");

    manager.cluster_mut("dev").unwrap().state = ConnectionState::Connected(version());
    manager.sync_proxy_report();
    let report = settled.try_recv().expect("reported");
    assert_eq!(report.reconnected, ["dev"]);
    assert_eq!(report.failed, [("own".to_owned(), "offline".to_owned())]);
}

#[tokio::test]
async fn a_report_that_runs_out_of_time_counts_the_rest_as_failed() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager_in(dir.path()).await;
    manager.cluster_mut("dev").unwrap().state = ConnectionState::Connecting;
    let mut settled = waiting_on(&mut manager, &["dev"]);

    manager.proxy_report_due(0);
    assert!(
        settled.try_recv().is_err(),
        "an earlier change's timer does nothing"
    );
    manager.proxy_report_due(1);
    let report = settled.try_recv().expect("reported");
    assert!(report.reconnected.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert!(report.failed[0].1.contains("in time"), "{report:?}");
}
