//! One sign-in of a cluster kept watching, from its fleet home card (`member_sign_in.rs`).

use std::time::{Duration, Instant};

use super::super::{ClusterManager, Internal};
use super::tests::{open_set, set_manager, sync, token_plugin};
use crate::auth_plugin::tests::FakePlugin;
use crate::credential::CredentialSource;
use crate::keep_watching::{BackgroundSignIn, KeepWatching};

const SERVER: &str = "http://127.0.0.1:1";

/// A manager with `dev` signing in with `plugin` and kept watching without background sign-in
/// consent, and `other` selected: `dev`'s card says it needs a sign-in, and nothing runs.
async fn waiting_dev(plugin: &FakePlugin, config_dir: std::path::PathBuf) -> ClusterManager {
    let mut manager = set_manager(plugin, config_dir, "other");
    let definition = manager.keep_watching_views()["dev"].definition;
    manager
        .set_keep_watching("dev".to_owned(), definition)
        .await;
    sync(&mut manager);
    assert_eq!(manager.keep_status("dev"), KeepWatching::NeedsConfirming);
    assert!(!manager.keep.has_feed("dev"));
    manager
}

/// What `dev`'s Sign in click sends: the server and definitions its card was drawn with.
fn click(manager: &mut ClusterManager) -> Result<(), String> {
    let view = manager.keep_watching_views()["dev"].clone();
    let revoked = revoked(manager);
    manager.sign_in_watched("dev", SERVER, view.definition, view.shown_sign_in, revoked)
}

/// How many times `dev`'s grant was ended, as the handle reads it at a click.
fn revoked(manager: &ClusterManager) -> u64 {
    manager.kept_grants().revocations("dev")
}

async fn wait_for_a_start(plugin: &FakePlugin) {
    for _ in 0..250 {
        if !plugin.starts().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the plugin did not start");
}

/// Waits long enough for a start that should not happen.
async fn nothing_starts(plugin: &FakePlugin) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(plugin.starts().is_empty(), "the plugin ran");
}

// The click runs the plugin once, with no input, on the cluster's kept connection, and grants
// nothing more: no consent is kept, the background sign-in stays off, and a second start is
// refused.
#[tokio::test]
async fn a_watched_cluster_signs_in_once_from_its_card() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(plugin.starts().is_empty(), "nothing runs before the click");

    click(&mut manager).expect("dev can sign in");

    let kept = &manager.keep.feeds["dev"];
    assert!(kept.once.is_some());
    assert!(kept.access.signed_in_once_access());
    assert!(
        kept.access.takes_no_input(),
        "its program can't prompt and gets a null stdin"
    );
    assert!(!kept.access.unattended(), "no background consent");
    assert_eq!(manager.keep_status("dev"), KeepWatching::Watching);
    assert!(click(&mut manager).is_err(), "a second click is refused");
    wait_for_a_start(&plugin).await;

    let again = manager.keep.feeds["dev"]
        .access
        .grant(CredentialSource::Plugin)
        .lease();
    assert!(again.launch_refusal().await.is_some(), "no second start");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(plugin.starts().len(), 1);
    assert!(!manager.keep.consents.contains_key("dev"), "nothing kept");
    let view = &manager.keep_watching_views()["dev"];
    assert_eq!(view.sign_in, BackgroundSignIn::Off);
}

// The refused second start stops the feed with no retry: the card needs a sign-in again, and
// offers it.
#[tokio::test]
async fn a_spent_sign_in_waits_for_another_click() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    click(&mut manager).unwrap();
    let attempt = manager.keep.feeds["dev"].attempt;

    manager
        .handle_internal(Internal::KeepWatchFailed {
            cluster: "dev".into(),
            attempt,
        })
        .await;
    sync(&mut manager);

    assert!(!manager.keep.has_feed("dev"), "nothing retries it");
    assert_eq!(manager.keep_status("dev"), KeepWatching::NeedsConfirming);
    assert!(click(&mut manager).is_ok(), "Sign in is offered again");
}

// A click made on a card that no longer shows the definition, plugin or server is refused, and
// runs nothing.
#[tokio::test]
async fn a_stale_card_click_is_refused() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    let view = manager.keep_watching_views()["dev"].clone();
    let (definition, shown) = (view.definition.unwrap(), view.shown_sign_in.unwrap());
    let r = revoked(&manager);

    let refused = [
        manager.sign_in_watched("dev", SERVER, Some(definition), Some(shown ^ 1), r),
        manager.sign_in_watched("dev", SERVER, Some(definition ^ 1), Some(shown), r),
        manager.sign_in_watched("dev", SERVER, Some(definition), None, r),
        manager.sign_in_watched(
            "dev",
            "http://127.0.0.1:2",
            Some(definition),
            Some(shown),
            r,
        ),
    ];

    for refused in refused {
        assert!(refused.unwrap_err().contains("changed"));
    }
    assert!(!manager.keep.has_feed("dev"));
    nothing_starts(&plugin).await;
}

// A definition changed between the click and the plugin's start drops the connection before it
// runs anything.
#[tokio::test]
async fn a_change_before_the_start_runs_nothing() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    click(&mut manager).unwrap();

    let slot = manager.cluster_mut("dev").unwrap();
    slot.fingerprint = slot.fingerprint.map(|f| f ^ 1);
    sync(&mut manager);

    assert!(!manager.keep.has_feed("dev"));
    assert_ne!(manager.keep_status("dev"), KeepWatching::Watching);
    nothing_starts(&plugin).await;
}

// The files are read again right before the plugin starts: an edit on disk the manager has not
// loaded yet refuses the start.
#[tokio::test]
async fn an_edit_on_disk_refuses_the_start() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    let path = manager.kubeconfig_paths[0].clone();
    click(&mut manager).unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    let edited = original.replace("user: u\n", "user: u\n    namespace: elsewhere\n");
    assert_ne!(original, edited);
    std::fs::write(&path, edited).unwrap();

    nothing_starts(&plugin).await;
}

// A click whose plugin would start after the sign-in window runs nothing.
#[tokio::test]
async fn a_click_too_old_at_the_start_runs_nothing() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    let shown = manager.keep_watching_views()["dev"].shown_sign_in.unwrap();
    let consent = manager.seal_shown_plugin("dev", shown).unwrap();
    let asked = Instant::now().checked_sub(Duration::from_secs(61)).unwrap();

    let r = revoked(&manager);

    assert!(manager.start_kept_once("dev", consent, asked, r));

    nothing_starts(&plugin).await;
}

// A cluster held for a proxy change, paused behind an unreadable file, disconnected, selected or
// in the open set refuses the click, and nothing runs.
#[tokio::test]
async fn a_held_cluster_refuses_the_click() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;

    let current = crate::connection::proxy::generation();
    manager.cluster_mut("dev").unwrap().proxy_generation = current.wrapping_add(1);
    assert!(click(&mut manager).unwrap_err().contains("proxy"));
    manager.cluster_mut("dev").unwrap().proxy_generation = current;

    manager.held.insert("dev".to_owned());
    assert!(click(&mut manager).unwrap_err().contains("unreadable"));
    manager.held.remove("dev");

    manager.active = Some("dev".to_owned());
    assert!(click(&mut manager).unwrap_err().contains("selected"));
    manager.active = Some("other".to_owned());

    open_set(&mut manager);
    assert!(click(&mut manager).unwrap_err().contains("open set"));
    manager.set_running_set(Vec::new());

    manager.disconnect_cluster("dev".to_owned()).await;
    assert!(click(&mut manager).unwrap_err().contains("disconnected"));

    assert!(!manager.keep.has_feed("dev"));
    nothing_starts(&plugin).await;
}

// Disconnecting a cluster signed in once ends its connection and its plugin's access.
#[tokio::test]
async fn disconnecting_ends_a_sign_in_once() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    click(&mut manager).unwrap();
    let access = manager.keep.feeds["dev"].access.clone();

    manager.disconnect_cluster("dev".to_owned()).await;
    sync(&mut manager);

    assert!(!manager.keep.has_feed("dev"));
    assert!(!access.grant(CredentialSource::Plugin).allowed());
    nothing_starts(&plugin).await;
}

// A stop or disconnect sent after the click, which ends grants at once outside the queues,
// refuses a click still queued, or ends the access of one already admitted.
#[tokio::test]
async fn a_stop_after_the_click_wins() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    let view = manager.keep_watching_views()["dev"].clone();
    let at_click = revoked(&manager);

    manager.kept_grants().revoke("dev");
    let refused =
        manager.sign_in_watched("dev", SERVER, view.definition, view.shown_sign_in, at_click);
    assert!(
        refused.unwrap_err().contains("stopped"),
        "queued behind the stop"
    );

    let shown = view.shown_sign_in.unwrap();
    let consent = manager.seal_shown_plugin("dev", shown).unwrap();
    let before = revoked(&manager) - 1;
    assert!(manager.start_kept_once("dev", consent, Instant::now(), before));
    let access = manager.keep.feeds["dev"].access.clone();
    assert!(
        !access.grant(CredentialSource::Plugin).allowed(),
        "ended at once"
    );
    nothing_starts(&plugin).await;
}

// A token the server refuses ends the access signed in once, as a failure: the feed stops with
// no retry, and the card needs a sign-in again.
#[tokio::test]
async fn a_refused_token_ends_the_sign_in() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    click(&mut manager).unwrap();
    let access = manager.keep.feeds["dev"].access.clone();

    access.grant(CredentialSource::Plugin).note_refused();

    assert!(!access.grant(CredentialSource::Plugin).allowed());
    let attempt = manager.keep.feeds["dev"].attempt;
    manager
        .handle_internal(Internal::KeepWatchFailed {
            cluster: "dev".into(),
            attempt,
        })
        .await;
    sync(&mut manager);
    assert!(!manager.keep.has_feed("dev"));
    assert_eq!(manager.keep_status("dev"), KeepWatching::NeedsConfirming);
}

// Background consent a change voided stays void, and a fresh click still watches on its own
// connection.
#[tokio::test]
async fn a_click_watches_after_voided_consent() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = waiting_dev(&plugin, config.path().to_owned()).await;
    manager.keep.voided.insert("dev".to_owned());

    click(&mut manager).unwrap();
    sync(&mut manager);

    assert!(manager.keep.has_feed("dev"), "the click's feed stays");
    assert_eq!(manager.keep_status("dev"), KeepWatching::Watching);
    assert!(manager.keep.voided.contains("dev"));
    assert!(!manager.keep.consents.contains_key("dev"));
}
