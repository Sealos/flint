use std::time::{Duration, Instant};

use super::*;
use crate::api_error::Tls;

struct NoJitter;

impl Jitter for NoJitter {
    fn sample(&mut self) -> u32 {
        0
    }
}

fn target(definition: u64) -> RetryTarget {
    RetryTarget {
        cluster: "prod".into(),
        generation: 1,
        definition,
    }
}

fn failed(attempt: u64) -> RetryPhase {
    RetryPhase::Failed {
        attempt,
        transient: true,
        may_retry: true,
    }
}

fn see(retry: &mut SelectedRetry, phase: RetryPhase, now: Instant) {
    retry.observe(Some((target(7), phase)), now, &mut NoJitter);
}

fn failing(now: Instant) -> SelectedRetry {
    let mut retry = SelectedRetry::default();
    see(&mut retry, RetryPhase::Connecting, now);
    see(&mut retry, failed(1), now);
    retry
}

fn next_retry(retry: &SelectedRetry) -> Option<Instant> {
    retry.progress().and_then(|p| p.next_retry)
}

#[test]
fn only_failures_that_time_can_fix_are_transient() {
    let transport = |problem| ClusterError::Transport {
        problem,
        server: None,
    };
    let http = |code| ClusterError::Http { code, server: None };
    assert!(transient(&transport(Transport::Refused)));
    assert!(transient(&transport(Transport::Unresolved)));
    assert!(transient(&http(503)));
    assert!(!transient(&transport(Transport::Tls(Tls::UntrustedCa))));
    assert!(!transient(&http(401)));
    assert!(!transient(&http(403)));
    assert!(!transient(&ClusterError::Kube("unknown".into())));
    assert!(!transient(&ClusterError::ExecPluginNotFound {
        command: None
    }));
}

#[test]
fn a_transient_failure_waits_with_backoff_and_keeps_its_start() {
    let start = Instant::now();
    let mut retry = failing(start);
    assert_eq!(next_retry(&retry), Some(start + Duration::from_secs(2)));
    assert!(retry.take_due());
    assert!(!retry.take_due());

    let later = start + Duration::from_secs(2);
    see(&mut retry, RetryPhase::Connecting, later);
    see(&mut retry, failed(2), later);
    let progress = retry.progress().unwrap();
    assert_eq!(progress.since, start);
    assert_eq!(progress.retries, 2);
    assert_eq!(progress.next_retry, Some(later + Duration::from_secs(4)));
}

#[test]
fn the_same_failure_seen_again_schedules_nothing_new() {
    let start = Instant::now();
    let mut retry = failing(start);
    see(&mut retry, failed(1), start + Duration::from_secs(1));
    let progress = retry.progress().unwrap();
    assert_eq!(progress.retries, 1);
    assert_eq!(progress.next_retry, Some(start + Duration::from_secs(2)));
}

#[test]
fn a_failure_a_retry_cannot_fix_stops() {
    let now = Instant::now();
    let mut retry = SelectedRetry::default();
    see(&mut retry, RetryPhase::Connecting, now);
    let phase = RetryPhase::Failed {
        attempt: 1,
        transient: false,
        may_retry: true,
    };
    see(&mut retry, phase, now);
    assert_eq!(next_retry(&retry), None);
    assert!(!retry.take_due());
}

#[test]
fn a_credential_that_runs_a_program_or_a_pause_stops_a_pending_retry() {
    let now = Instant::now();
    let mut retry = failing(now);
    let phase = RetryPhase::Failed {
        attempt: 1,
        transient: true,
        may_retry: false,
    };
    see(&mut retry, phase, now);
    assert!(!retry.take_due());
    // It stays stopped when the context can be retried again.
    see(&mut retry, failed(2), now);
    assert_eq!(next_retry(&retry), None);
}

#[test]
fn cancel_stops_until_the_next_selection() {
    let now = Instant::now();
    let mut retry = failing(now);
    retry.cancel();
    assert!(!retry.take_due());
    see(&mut retry, RetryPhase::Connecting, now);
    see(&mut retry, failed(2), now);
    assert_eq!(next_retry(&retry), None);

    retry.selected(true);
    see(&mut retry, RetryPhase::Connecting, now);
    see(&mut retry, failed(3), now);
    assert!(next_retry(&retry).is_some());
}

#[test]
fn a_changed_definition_gets_no_retry() {
    let now = Instant::now();
    let mut retry = failing(now);
    retry.observe(Some((target(8), failed(1))), now, &mut NoJitter);
    assert!(!retry.take_due());
    retry.observe(
        Some((target(8), RetryPhase::Connecting)),
        now,
        &mut NoJitter,
    );
    retry.observe(Some((target(8), failed(2))), now, &mut NoJitter);
    assert_eq!(next_retry(&retry), None);
}

#[test]
fn a_definition_changed_across_a_settled_state_gets_no_retry() {
    let now = Instant::now();
    let mut retry = failing(now);
    see(&mut retry, RetryPhase::Settled, now);
    retry.observe(
        Some((target(8), RetryPhase::Connecting)),
        now,
        &mut NoJitter,
    );
    retry.observe(Some((target(8), failed(2))), now, &mut NoJitter);
    assert_eq!(next_retry(&retry), None);
}

#[test]
fn a_removed_or_deselected_context_stops() {
    let now = Instant::now();
    let mut retry = failing(now);
    retry.observe(None, now, &mut NoJitter);
    assert!(retry.progress().is_none());
    assert!(!retry.take_due());
}

#[test]
fn retries_end_at_the_cap() {
    let now = Instant::now();
    let mut retry = SelectedRetry::default();
    for attempt in 1..=u64::from(MAX_AUTOMATIC_RETRIES) {
        see(&mut retry, RetryPhase::Connecting, now);
        see(&mut retry, failed(attempt), now);
        assert!(retry.take_due(), "retry {attempt}");
    }
    see(&mut retry, RetryPhase::Connecting, now);
    see(
        &mut retry,
        failed(u64::from(MAX_AUTOMATIC_RETRIES) + 1),
        now,
    );
    assert!(!retry.take_due());
}
