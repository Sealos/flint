use super::*;

#[test]
fn ending_a_clusters_logs_leaves_another_clusters_running() {
    let mut marks = Disconnects::default();
    let a = marks.track_logs(1, "a", 0, |_| true);
    let b = marks.track_logs(2, "b", 0, |_| true);

    marks.end_logs("a");

    assert!(*a.borrow());
    assert!(!*b.borrow());
    assert!(!marks.log_stops.contains_key(&1), "nothing kept for it");
    assert!(marks.log_stops.contains_key(&2));
}

#[test]
fn a_finished_sessions_signal_is_not_kept() {
    let mut marks = Disconnects::default();
    let _first = marks.track_logs(1, "a", 0, |_| true);
    let _second = marks.track_logs(2, "a", 0, |_| false);
    assert_eq!(marks.log_stops.keys().copied().collect::<Vec<_>>(), [2]);
}

#[test]
fn only_choosing_the_cluster_lifts_the_mark() {
    let mut marks = Disconnects::default();
    marks.clusters.insert("a".to_owned());
    assert!(marks.holds("a"));
    marks.lift("b");
    assert!(marks.holds("a"));
    marks.lift("a");
    assert!(!marks.holds("a"));
}

#[test]
fn a_log_request_made_before_a_disconnect_starts_ended() {
    let mut marks = Disconnects::default();
    let asked = marks.count.of("a");
    marks.count.bump("a");
    assert!(*marks.track_logs(1, "a", asked, |_| true).borrow());
    let now = marks.count.of("a");
    assert!(!*marks.track_logs(2, "a", now, |_| true).borrow());
}
