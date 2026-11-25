use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};

use super::*;
use crate::keep_watching::KeepWatching;
use crate::manager::keep_watching::tests::{
    allow, land, land_dev, land_kept, open_set, select_and_land, set_manager, sync, token_plugin,
    watched_other,
};
use crate::metrics::{Cpu, Memory, MetricsList, NodeMetrics, POLL_INTERVAL, Usage};
use crate::metrics_trend::{TrendResource, TrendScope};

/// When the first sample of each test was taken.
fn start() -> Timestamp {
    Timestamp::from_second(1_800_000_000).unwrap()
}

/// `steps` poll intervals after [`start`].
fn after(steps: i64) -> Timestamp {
    let interval = SignedDuration::try_from(POLL_INTERVAL).unwrap();
    start() + interval * i32::try_from(steps).unwrap()
}

/// Records one node's poll at `at` into `id`'s history.
fn sample(manager: &ClusterManager, id: &str, at: Timestamp, cpu: u64) {
    let list = Ok(MetricsList {
        items: vec![NodeMetrics {
            name: "a".into(),
            timestamp: None,
            window: None,
            usage: Usage {
                cpu: Cpu::from_nanos(cpu),
                memory: Memory::from_bytes(1 << 30),
            },
        }],
        skipped: 0,
    });
    let trends = manager.histories.get(id).expect("a history");
    trends.lock().unwrap().record(id, at, &list);
}

/// `id`'s CPU total up to `now`, from its history, oldest first.
fn cpu(manager: &ClusterManager, id: &str, now: Timestamp) -> Vec<Option<u64>> {
    let trends = manager.histories.get(id).expect("a history");
    let store = trends.lock().unwrap();
    store
        .series(id, TrendScope::Total, TrendResource::Cpu, now)
        .expect("a series")
        .collect()
}

/// The samples of `series`, without its gaps.
fn values(series: &[Option<u64>]) -> Vec<u64> {
    series.iter().flatten().copied().collect()
}

/// Whether `series` ends in a sample after a gap of at least one step. A failed poll of the test's
/// unreachable server between two samples only widens it.
fn resumes_after_a_gap(series: &[Option<u64>]) -> bool {
    series.last().is_some_and(Option::is_some) && series[series.len() - 2].is_none()
}

/// The feed listed for `id` now reads the same samples as its history.
fn feed_reads_history(manager: &ClusterManager, id: &str, now: Timestamp) {
    let read = manager.telemetry.get(id).expect("a running feed");
    let shown: Vec<Option<u64>> = read.with_trends(|store| {
        store
            .series(id, TrendScope::Total, TrendResource::Cpu, now)
            .expect("a series")
            .collect()
    });
    assert_eq!(shown, cpu(manager, id, now), "the feed shows the history");
}

// A store is shared on its definition, and a different definition starts empty.
#[test]
fn a_history_is_kept_per_definition() {
    let mut histories = TrendHistories::default();
    let first = histories.store("one", Some(1));
    assert!(Arc::ptr_eq(&first, &histories.store("one", Some(1))));
    assert!(!Arc::ptr_eq(&first, &histories.store("two", Some(1))));

    let other = histories.store("one", Some(2));
    assert!(!Arc::ptr_eq(&first, &other), "never another definition's");
    assert!(Arc::ptr_eq(&other, histories.get("one").unwrap()));

    let unknown = histories.store("one", None);
    assert!(
        !Arc::ptr_eq(&unknown, &other),
        "an unknown definition shares nothing"
    );
    assert!(!Arc::ptr_eq(&unknown, &histories.store("one", None)));
    assert!(Arc::ptr_eq(&other, histories.get("one").unwrap()));
}

// The window of a history follows the settings: the global value, then the cluster's own, applied
// to a history that already holds samples.
#[tokio::test]
async fn a_history_window_follows_the_settings() {
    use crate::settings::ClusterPin;
    use crate::thresholds::ThresholdMetric::HistoryHours;

    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    select_and_land(&mut manager, "other").await;
    let window = |manager: &ClusterManager| {
        manager
            .histories
            .get("other")
            .expect("a history")
            .lock()
            .unwrap()
            .window()
    };
    sample(&manager, "other", start(), 100);
    assert_eq!(window(&manager), 6 * 240);

    let server = manager
        .cluster("other")
        .unwrap()
        .info
        .server
        .clone()
        .unwrap();
    let mut settings = (*manager.settings).clone();
    settings
        .thresholds
        .set(None, HistoryHours, Some(12))
        .unwrap();
    manager.settings = Arc::new(settings.clone());
    manager.apply_history_windows();
    assert_eq!(window(&manager), 12 * 240);

    let pin = ClusterPin::new("other", server);
    settings
        .thresholds
        .set(Some(&pin), HistoryHours, Some(1))
        .unwrap();
    manager.settings = Arc::new(settings);
    manager.apply_history_windows();
    assert_eq!(window(&manager), 240);
    assert_eq!(values(&cpu(&manager, "other", start())), [100]);
}

// Opened before its kept feed was up, the slot's feed records the history. Leaving the cluster
// stops that feed, and the kept feed that takes over goes on with the same series, with a gap
// for the time no poll ran.
#[tokio::test]
async fn the_kept_feed_continues_the_slots_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    select_and_land(&mut manager, "other").await;
    assert!(manager.cluster("other").unwrap().telemetry.is_some());
    sample(&manager, "other", start(), 100);
    let definition = manager.keep_watching_views()["other"].definition;
    manager
        .set_keep_watching("other".to_owned(), definition)
        .await;
    sync(&mut manager);

    manager.activate("dev".to_owned());
    sync(&mut manager);
    land_kept(&mut manager, "other").await;
    sync(&mut manager);
    assert!(manager.cluster("other").unwrap().telemetry.is_none());
    sample(&manager, "other", after(3), 200);

    let series = cpu(&manager, "other", after(3));
    assert_eq!(values(&series), [100, 200]);
    assert!(resumes_after_a_gap(&series));
    feed_reads_history(&manager, "other", after(3));
}

// A set member's feed served keep watching. Leaving the set hands its history to the kept feed.
#[tokio::test]
async fn leaving_the_set_keeps_a_members_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    open_set(&mut manager);
    land_dev(&mut manager).await;
    sync(&mut manager);
    assert!(manager.cluster("dev").unwrap().telemetry.is_some());
    sample(&manager, "dev", start(), 100);

    manager.set_running_set(Vec::new());
    sync(&mut manager);
    land_kept(&mut manager, "dev").await;
    sync(&mut manager);
    sample(&manager, "dev", after(2), 200);

    let series = cpu(&manager, "dev", after(2));
    assert_eq!(values(&series), [100, 200]);
    assert!(resumes_after_a_gap(&series));
    feed_reads_history(&manager, "dev", after(2));
}

// A new connection attempt of the selected cluster stops its feed until the attempt lands; the
// feed on the new connection goes on with the same series.
#[tokio::test]
async fn a_reconnect_keeps_the_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    select_and_land(&mut manager, "other").await;
    sample(&manager, "other", start(), 100);

    manager.reconnect_selected("other");
    sync(&mut manager);
    assert!(manager.cluster("other").unwrap().telemetry.is_none());
    assert!(manager.histories.get("other").is_some(), "kept meanwhile");
    land(&mut manager, "other").await;
    sync(&mut manager);
    assert!(manager.cluster("other").unwrap().telemetry.is_some());
    sample(&manager, "other", after(2), 200);

    let series = cpu(&manager, "other", after(2));
    assert_eq!(values(&series), [100, 200]);
    assert!(resumes_after_a_gap(&series));
    feed_reads_history(&manager, "other", after(2));
}

// A slot that started over on the same definition, as a proxy change does to every slot, gets a
// new kept connection, which goes on with the same series.
#[tokio::test]
async fn a_new_slot_on_the_same_definition_keeps_the_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    let first = watched_other(&mut manager).await;
    sample(&manager, "other", start(), 100);

    manager.cluster_mut("other").unwrap().generation += 1;
    sync(&mut manager);
    assert_ne!(land_kept(&mut manager, "other").await, first);
    sync(&mut manager);
    sample(&manager, "other", after(1), 200);

    let series = cpu(&manager, "other", after(1));
    assert_eq!(values(&series), [100, 200]);
    feed_reads_history(&manager, "other", after(1));
}

// A lapsed sign-in stops the kept feed until the person acts. The history waits with it, and the
// feed after the sign-in goes on with it.
#[tokio::test]
async fn a_lapsed_sign_in_keeps_the_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    sync(&mut manager);
    let attempt = land_kept(&mut manager, "dev").await;
    sample(&manager, "dev", start(), 100);

    manager.kept_failed("dev", attempt);
    sync(&mut manager);
    assert_eq!(manager.keep_status("dev"), KeepWatching::SignInNeeded);
    assert!(
        manager.histories.get("dev").is_some(),
        "kept while signed out"
    );

    manager.keep_watching_acted_on("dev");
    sync(&mut manager);
    land_kept(&mut manager, "dev").await;
    sync(&mut manager);
    sample(&manager, "dev", after(5), 200);

    let series = cpu(&manager, "dev", after(5));
    assert_eq!(values(&series), [100, 200]);
    assert!(resumes_after_a_gap(&series));
    feed_reads_history(&manager, "dev", after(5));
}

// A handed-over kept feed that fails while its cluster is selected falls back to the slot's
// feed, which goes on with the same series, and so does the kept feed after leaving again.
#[tokio::test]
async fn falling_back_to_the_slot_keeps_the_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    sync(&mut manager);
    let attempt = land_kept(&mut manager, "dev").await;
    select_and_land(&mut manager, "dev").await;
    assert!(manager.kept_feed_handed_over("dev"));
    sample(&manager, "dev", start(), 100);

    manager.kept_failed("dev", attempt);
    sync(&mut manager);
    assert!(manager.cluster("dev").unwrap().telemetry.is_some());
    sample(&manager, "dev", after(1), 200);
    feed_reads_history(&manager, "dev", after(1));

    manager.activate("other".to_owned());
    sync(&mut manager);
    land_kept(&mut manager, "dev").await;
    sync(&mut manager);
    sample(&manager, "dev", after(3), 300);

    let series = cpu(&manager, "dev", after(3));
    assert_eq!(values(&series), [100, 200, 300]);
    assert!(resumes_after_a_gap(&series));
    feed_reads_history(&manager, "dev", after(3));
}

// Turning keep watching off for a cluster in the background drops its history.
#[tokio::test]
async fn turning_keep_watching_off_drops_the_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    watched_other(&mut manager).await;
    sample(&manager, "other", start(), 100);

    manager.set_keep_watching("other".to_owned(), None).await;
    sync(&mut manager);

    assert!(manager.histories.get("other").is_none());
}

// Ending a plugin's background sign-in consent drops the history of a cluster in the background.
#[tokio::test]
async fn ending_consent_drops_the_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "other");
    allow(&mut manager).await;
    sync(&mut manager);
    land_kept(&mut manager, "dev").await;
    sample(&manager, "dev", start(), 100);

    manager.kept_grants().revoke("dev");
    manager.revoke_background_sign_in("dev");
    sync(&mut manager);

    assert!(manager.histories.get("dev").is_none());
}

// A changed definition drops the history, and the slot's own feed starts an empty one: another
// server or credential never inherits the samples.
#[tokio::test]
async fn a_changed_definition_starts_an_empty_history() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    watched_other(&mut manager).await;
    select_and_land(&mut manager, "other").await;
    sample(&manager, "other", start(), 100);
    let before = Arc::clone(manager.histories.get("other").unwrap());

    let slot = manager.cluster_mut("other").unwrap();
    slot.fingerprint = slot.fingerprint.map(|print| print.wrapping_add(1));
    sync(&mut manager);

    assert!(manager.cluster("other").unwrap().telemetry.is_some());
    let after = manager.histories.get("other").expect("the new feed's");
    assert!(!Arc::ptr_eq(&before, after));
    let store = after.lock().unwrap();
    assert!(
        store
            .series("other", TrendScope::Total, TrendResource::Cpu, start())
            .is_none(),
        "nothing carried over"
    );
}

// A cluster that is not kept watching drops its history once it is left, as before.
#[tokio::test]
async fn an_unwatched_cluster_drops_its_history_when_left() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    select_and_land(&mut manager, "other").await;
    sample(&manager, "other", start(), 100);

    manager.activate("dev".to_owned());
    sync(&mut manager);

    assert!(manager.histories.get("other").is_none());
}

// Dropping a history empties it, so last values still listed for a stopped feed show none of it.
#[test]
fn a_dropped_history_is_emptied() {
    let mut histories = TrendHistories::default();
    let held = histories.store("one", Some(1));
    let list = Ok(MetricsList {
        items: vec![NodeMetrics {
            name: "a".into(),
            timestamp: None,
            window: None,
            usage: Usage::default(),
        }],
        skipped: 0,
    });
    held.lock().unwrap().record("one", start(), &list);

    histories.forget("one");

    let store = held.lock().unwrap();
    assert!(
        store
            .series("one", TrendScope::Total, TrendResource::Cpu, start())
            .is_none()
    );
}

// A paused context showing a fallback from another file is no confirmed change: the history
// waits for the files to settle, and is dropped if the change is confirmed.
#[tokio::test]
async fn a_paused_fallback_keeps_the_history_until_confirmed() {
    let plugin = token_plugin();
    let config = tempfile::tempdir().unwrap();
    let mut manager = set_manager(&plugin, config.path().to_owned(), "dev");
    watched_other(&mut manager).await;
    sample(&manager, "other", start(), 100);
    let slot = manager.cluster_mut("other").unwrap();
    slot.fingerprint = slot.fingerprint.map(|print| print.wrapping_add(1));

    manager.held.insert("other".to_owned());
    sync(&mut manager);
    assert!(
        manager.histories.get("other").is_some(),
        "kept while paused"
    );

    manager.held.remove("other");
    sync(&mut manager);
    assert!(
        manager.histories.get("other").is_none(),
        "dropped once confirmed"
    );
}
