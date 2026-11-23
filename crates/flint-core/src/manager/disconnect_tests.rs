use super::*;

#[test]
fn ending_a_clusters_logs_leaves_another_clusters_running() {
    let mut marks = Disconnects::default();
    let a = marks.track_logs(1, "a", |_| true);
    let b = marks.track_logs(2, "b", |_| true);

    marks.end_logs("a");

    assert!(*a.borrow());
    assert!(!*b.borrow());
    assert!(!marks.log_stops.contains_key(&1), "nothing kept for it");
    assert!(marks.log_stops.contains_key(&2));
}

#[test]
fn a_finished_sessions_signal_is_not_kept() {
    let mut marks = Disconnects::default();
    let _first = marks.track_logs(1, "a", |_| true);
    let _second = marks.track_logs(2, "a", |_| false);
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
