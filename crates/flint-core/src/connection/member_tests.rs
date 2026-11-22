use super::super::schedule::{BACKOFF_INITIAL, BACKOFF_MAX, Schedule};
use super::*;

/// Jitter that removes nothing, so retry times are exact.
struct NoJitter;

impl Jitter for NoJitter {
    fn sample(&mut self) -> u32 {
        0
    }
}

const URL: &str = "https://one.example:6443";

fn text(value: &str) -> SafeErrorText {
    SafeErrorText::fixed(value)
}

fn server() -> ServerIdentity {
    ServerIdentity::new(URL)
}

fn other_server() -> ServerIdentity {
    ServerIdentity::new("https://two.example:6443")
}

fn repointed(server: ServerIdentity, confirmed: bool) -> MemberEvent {
    MemberEvent::Repointed { server, confirmed }
}

fn attempt_id() -> AttemptId {
    Schedule::<u32>::default().start_active(&0)
}

fn connecting() -> Member {
    Member::join(server(), None, None).0
}

/// A member with an attempt in flight, and that attempt.
fn attempting() -> (Member, AttemptId) {
    let mut member = connecting();
    let attempt = attempt_id();
    assert!(member.begin_attempt(attempt));
    (member, attempt)
}

fn finish(member: &mut Member, attempt: AttemptId, outcome: AttemptOutcome, at: Instant) {
    let event = MemberEvent::AttemptFinished { attempt, outcome };
    assert_eq!(member.apply(event, at, &mut NoJitter), MemberAction::None);
}

fn live(now: Instant) -> Member {
    let (mut member, attempt) = attempting();
    finish(&mut member, attempt, AttemptOutcome::Connected, now);
    member
}

fn lost_contact() -> MemberEvent {
    MemberEvent::Watches {
        any_synced: false,
        error: Some(text("lost")),
    }
}

fn reconnecting(now: Instant) -> Member {
    let mut member = live(now);
    assert_eq!(
        member.apply(lost_contact(), now, &mut NoJitter),
        MemberAction::None
    );
    member
}

fn failed(now: Instant) -> Member {
    let (mut member, attempt) = attempting();
    let outcome = AttemptOutcome::Failed(text("down"));
    finish(&mut member, attempt, outcome, now);
    member
}

fn sign_in_needed(now: Instant) -> Member {
    let (mut member, attempt) = attempting();
    let outcome = AttemptOutcome::AuthRejected(text("401"));
    finish(&mut member, attempt, outcome, now);
    member
}

/// A member in every state, including connecting with and without an attempt.
fn every_state(now: Instant) -> Vec<Member> {
    vec![
        connecting(),
        attempting().0,
        live(now),
        reconnecting(now),
        failed(now),
        sign_in_needed(now),
    ]
}

/// Every event, with times far enough on that any due transition fires.
fn every_event(attempt: AttemptId) -> Vec<MemberEvent> {
    let finished = |outcome| MemberEvent::AttemptFinished { attempt, outcome };
    vec![
        finished(AttemptOutcome::Connected),
        finished(AttemptOutcome::Failed(text("down"))),
        finished(AttemptOutcome::AuthRejected(text("401"))),
        lost_contact(),
        MemberEvent::Watches {
            any_synced: true,
            error: None,
        },
        MemberEvent::CredentialLost {
            reason: text("401"),
        },
        MemberEvent::Activated,
        MemberEvent::Deactivated,
        repointed(server(), true),
        repointed(other_server(), false),
        MemberEvent::Tick,
    ]
}

#[test]
fn join_queues_when_allowed() {
    let (member, action) = Member::join(server(), None, None);
    assert_eq!(member.state(), &MemberState::Connecting);
    assert_eq!(action, MemberAction::Enqueue);
}

#[test]
fn join_needs_sign_in_when_the_policy_denies() {
    let (member, action) = Member::join(server(), Some(text("denied")), None);
    let reason = text("denied");
    assert_eq!(member.state(), &MemberState::SignInNeeded { reason });
    assert_eq!(action, MemberAction::None);
}

#[test]
fn join_while_paused_starts_nothing_until_resumed() {
    let now = Instant::now();
    for pause in [Pause::Held, Pause::Provisional] {
        let (mut member, action) = Member::join(server(), None, Some(pause));
        assert_eq!(action, MemberAction::None);
        assert_eq!(member.paused(), Some(pause));
        let resumed = member.apply(MemberEvent::Resumed(pause), now, &mut NoJitter);
        assert_eq!(resumed, MemberAction::Enqueue);
    }
}

#[test]
fn connecting_goes_live_on_success() {
    let now = Instant::now();
    assert_eq!(live(now).state(), &MemberState::Live);
    assert_eq!(live(now).failures(), 0);
}

#[test]
fn connecting_fails_with_backoff_on_a_network_error() {
    let now = Instant::now();
    let member = failed(now);
    assert_eq!(
        member.state(),
        &MemberState::Failed {
            error: text("down"),
            retry_at: now + BACKOFF_INITIAL,
        }
    );
    assert_eq!(member.failures(), 1);
}

#[test]
fn connecting_needs_sign_in_on_a_rejected_credential() {
    let now = Instant::now();
    let reason = text("401");
    assert_eq!(
        sign_in_needed(now).state(),
        &MemberState::SignInNeeded { reason }
    );
}

#[test]
fn live_reconnects_only_with_an_error_and_nothing_synced() {
    let now = Instant::now();
    let mut member = live(now);
    let synced = MemberEvent::Watches {
        any_synced: true,
        error: Some(text("one kind")),
    };
    let no_error = MemberEvent::Watches {
        any_synced: false,
        error: None,
    };
    let _ = member.apply(synced, now, &mut NoJitter);
    let _ = member.apply(no_error, now, &mut NoJitter);
    assert_eq!(member.state(), &MemberState::Live);

    let _ = member.apply(lost_contact(), now, &mut NoJitter);
    let expected = MemberState::Reconnecting {
        since: now,
        error: text("lost"),
    };
    assert_eq!(member.state(), &expected);
}

#[test]
fn reconnecting_keeps_its_start_and_latest_error() {
    let now = Instant::now();
    let mut member = reconnecting(now);
    let later = now + Duration::from_secs(30);
    let event = MemberEvent::Watches {
        any_synced: false,
        error: Some(text("newer")),
    };
    let _ = member.apply(event, later, &mut NoJitter);
    let expected = MemberState::Reconnecting {
        since: now,
        error: text("newer"),
    };
    assert_eq!(member.state(), &expected);
}

#[test]
fn reconnecting_goes_live_when_any_kind_syncs() {
    let now = Instant::now();
    let mut member = reconnecting(now);
    let event = MemberEvent::Watches {
        any_synced: true,
        error: None,
    };
    assert_eq!(member.apply(event, now, &mut NoJitter), MemberAction::None);
    assert_eq!(member.state(), &MemberState::Live);
}

#[test]
fn reconnecting_fails_and_stops_watches_after_two_minutes() {
    let now = Instant::now();
    let mut member = reconnecting(now);
    let just_before = now + RECONNECT_TIMEOUT - Duration::from_millis(1);
    assert_eq!(
        member.apply(MemberEvent::Tick, just_before, &mut NoJitter),
        MemberAction::None
    );
    let at = now + RECONNECT_TIMEOUT;
    assert_eq!(
        member.apply(MemberEvent::Tick, at, &mut NoJitter),
        MemberAction::StopWatches
    );
    let expected = MemberState::Failed {
        error: text("lost"),
        retry_at: at + BACKOFF_INITIAL,
    };
    assert_eq!(member.state(), &expected);
}

#[test]
fn a_lost_credential_needs_sign_in_and_stops_watches() {
    let now = Instant::now();
    for mut member in [live(now), reconnecting(now)] {
        let event = MemberEvent::CredentialLost {
            reason: text("401"),
        };
        assert_eq!(
            member.apply(event, now, &mut NoJitter),
            MemberAction::StopWatches
        );
        let reason = text("401");
        assert_eq!(member.state(), &MemberState::SignInNeeded { reason });
    }
}

#[test]
fn a_lost_credential_changes_nothing_in_other_states() {
    let now = Instant::now();
    for mut member in [connecting(), failed(now), sign_in_needed(now)] {
        let before = member.clone();
        let event = MemberEvent::CredentialLost {
            reason: text("other"),
        };
        assert_eq!(member.apply(event, now, &mut NoJitter), MemberAction::None);
        assert_eq!(member, before);
    }
}

#[test]
fn failed_retries_at_its_time() {
    let now = Instant::now();
    let mut member = failed(now);
    let early = now + BACKOFF_INITIAL - Duration::from_millis(1);
    assert_eq!(
        member.apply(MemberEvent::Tick, early, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(
        member.apply(MemberEvent::Tick, now + BACKOFF_INITIAL, &mut NoJitter),
        MemberAction::Enqueue
    );
    assert_eq!(member.state(), &MemberState::Connecting);
    assert_eq!(member.failures(), 1, "the next failure backs off further");
}

#[test]
fn repeated_failures_back_off_to_the_cap_and_success_resets() {
    let mut now = Instant::now();
    let mut member = connecting();
    let mut schedule = Schedule::<u32>::default();
    let mut last_wait = Duration::ZERO;
    for _ in 0..10 {
        let attempt = schedule.start_active(&0);
        assert!(member.begin_attempt(attempt));
        finish(
            &mut member,
            attempt,
            AttemptOutcome::Failed(text("down")),
            now,
        );
        let MemberState::Failed { retry_at, .. } = *member.state() else {
            panic!("not failed");
        };
        last_wait = retry_at - now;
        now = retry_at;
        let _ = member.apply(MemberEvent::Tick, now, &mut NoJitter);
    }
    assert_eq!(last_wait, BACKOFF_MAX);

    let attempt = schedule.start_active(&0);
    assert!(member.begin_attempt(attempt));
    finish(&mut member, attempt, AttemptOutcome::Connected, now);
    assert_eq!(member.failures(), 0);
}

#[test]
fn sign_in_needed_never_retries_by_itself() {
    let now = Instant::now();
    let mut member = sign_in_needed(now);
    let before = member.clone();
    for hours in 0..48 {
        let at = now + Duration::from_secs(3_600 * hours);
        assert_eq!(
            member.apply(MemberEvent::Tick, at, &mut NoJitter),
            MemberAction::None
        );
    }
    assert_eq!(member, before);
}

#[test]
fn activating_retries_sign_in_and_failed_at_once() {
    let now = Instant::now();
    for mut member in [sign_in_needed(now), failed(now)] {
        assert_eq!(
            member.apply(MemberEvent::Activated, now, &mut NoJitter),
            MemberAction::StartNow
        );
        assert_eq!(member.state(), &MemberState::Connecting);
        assert_eq!(member.failures(), 0);
    }
}

#[test]
fn activating_a_queued_member_starts_it_but_not_one_in_flight() {
    let now = Instant::now();
    let mut queued = connecting();
    assert_eq!(
        queued.apply(MemberEvent::Activated, now, &mut NoJitter),
        MemberAction::StartNow
    );
    let (mut running, _) = attempting();
    assert_eq!(
        running.apply(MemberEvent::Activated, now, &mut NoJitter),
        MemberAction::None
    );
}

#[test]
fn activating_a_running_member_changes_nothing() {
    let now = Instant::now();
    for mut member in [live(now), reconnecting(now)] {
        let before = member.clone();
        assert_eq!(
            member.apply(MemberEvent::Activated, now, &mut NoJitter),
            MemberAction::None
        );
        assert_eq!(member, before);
    }
}

#[test]
fn a_repoint_to_another_server_removes_the_member_in_every_state() {
    let now = Instant::now();
    for paused in [false, true] {
        for mut member in every_state(now) {
            if paused {
                let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
            }
            let event = repointed(other_server(), true);
            assert_eq!(
                member.apply(event, now, &mut NoJitter),
                MemberAction::Remove
            );
        }
    }
}

#[test]
fn any_change_to_the_url_counts_as_a_repoint() {
    let now = Instant::now();
    let mut member = live(now);
    let server = ServerIdentity::new(format!("{URL}/"));
    assert_eq!(
        member.apply(repointed(server, true), now, &mut NoJitter),
        MemberAction::Remove
    );
}

#[test]
fn the_same_server_is_not_a_repoint() {
    let now = Instant::now();
    for mut member in every_state(now) {
        for confirmed in [true, false] {
            let before = member.clone();
            let event = repointed(server(), confirmed);
            assert_eq!(member.apply(event, now, &mut NoJitter), MemberAction::None);
            assert_eq!(member, before);
        }
    }
}

#[test]
fn an_unconfirmed_server_only_pauses_the_member_in_every_state() {
    let now = Instant::now();
    for mut member in every_state(now) {
        let expected = if member.state() == &MemberState::Connecting {
            MemberAction::Dequeue
        } else {
            MemberAction::None
        };
        let state = member.state().clone();
        let action = member.apply(repointed(other_server(), false), now, &mut NoJitter);
        assert_eq!(action, expected);
        assert_eq!(member.state(), &state);
        assert_eq!(member.paused(), Some(Pause::Provisional));
        assert_eq!(member.server(), &server());
    }
}

#[test]
fn a_fallback_during_a_provisional_pause_keeps_the_pinned_connection() {
    let now = Instant::now();
    let mut member = live(now);
    let _ = member.apply(MemberEvent::Paused(Pause::Provisional), now, &mut NoJitter);
    assert_eq!(
        member.apply(repointed(other_server(), false), now, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(member.state(), &MemberState::Live);
    assert_eq!(member.server(), &server());
    let _ = member.apply(MemberEvent::Resumed(Pause::Provisional), now, &mut NoJitter);
    assert!(member.checkable());
}

#[test]
fn a_confirmed_change_after_a_fallback_removes_the_member() {
    let now = Instant::now();
    let mut member = live(now);
    let _ = member.apply(repointed(other_server(), false), now, &mut NoJitter);
    assert_eq!(
        member.apply(repointed(other_server(), true), now, &mut NoJitter),
        MemberAction::Remove
    );
}

#[test]
fn an_activation_during_a_pause_starts_in_the_foreground_when_it_ends() {
    let now = Instant::now();
    for pause in [Pause::Held, Pause::Provisional] {
        for mut member in [sign_in_needed(now), failed(now), connecting()] {
            let _ = member.apply(MemberEvent::Paused(pause), now, &mut NoJitter);
            let before = member.state().clone();
            assert_eq!(
                member.apply(MemberEvent::Activated, now, &mut NoJitter),
                MemberAction::None
            );
            assert_eq!(member.state(), &before, "nothing starts during the pause");
            assert_eq!(
                member.apply(MemberEvent::Resumed(pause), now, &mut NoJitter),
                MemberAction::StartNow
            );
            assert_eq!(member.state(), &MemberState::Connecting);
            assert_eq!(member.failures(), 0);
        }
    }
}

#[test]
fn an_activation_waits_for_the_last_pause_in_either_order() {
    let now = Instant::now();
    for (first, last) in [
        (Pause::Held, Pause::Provisional),
        (Pause::Provisional, Pause::Held),
    ] {
        let mut member = sign_in_needed(now);
        let before = member.state().clone();
        let _ = member.apply(MemberEvent::Paused(first), now, &mut NoJitter);
        let _ = member.apply(MemberEvent::Paused(last), now, &mut NoJitter);
        let _ = member.apply(MemberEvent::Activated, now, &mut NoJitter);
        assert_eq!(
            member.apply(MemberEvent::Resumed(first), now, &mut NoJitter),
            MemberAction::None
        );
        assert_eq!(member.state(), &before);
        assert!(!member.begin_attempt(attempt_id()));
        assert_eq!(
            member.apply(MemberEvent::Resumed(last), now, &mut NoJitter),
            MemberAction::StartNow
        );
    }
}

#[test]
fn a_remembered_activation_is_used_once() {
    let now = Instant::now();
    let mut member = sign_in_needed(now);
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    let _ = member.apply(MemberEvent::Activated, now, &mut NoJitter);
    let _ = member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter);
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter),
        MemberAction::Enqueue
    );
}

#[test]
fn an_activation_covers_a_credential_lost_later_in_the_pause() {
    let now = Instant::now();
    let mut member = live(now);
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    let _ = member.apply(MemberEvent::Activated, now, &mut NoJitter);
    let lost = MemberEvent::CredentialLost {
        reason: text("401"),
    };
    let _ = member.apply(lost, now, &mut NoJitter);
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter),
        MemberAction::StartNow
    );
}

#[test]
fn a_confirmed_repoint_clears_a_remembered_activation() {
    let now = Instant::now();
    let mut member = sign_in_needed(now);
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    let _ = member.apply(MemberEvent::Activated, now, &mut NoJitter);
    assert_eq!(
        member.apply(repointed(other_server(), true), now, &mut NoJitter),
        MemberAction::Remove
    );
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter),
        MemberAction::None
    );
    assert!(matches!(member.state(), MemberState::SignInNeeded { .. }));
}

#[test]
fn an_unconfirmed_server_keeps_a_remembered_activation() {
    let now = Instant::now();
    let mut member = sign_in_needed(now);
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    let _ = member.apply(MemberEvent::Activated, now, &mut NoJitter);
    let _ = member.apply(repointed(other_server(), false), now, &mut NoJitter);
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Provisional), now, &mut NoJitter),
        MemberAction::StartNow
    );
    assert_eq!(member.server(), &server());
}

#[test]
fn selecting_another_cluster_clears_a_remembered_activation() {
    let now = Instant::now();
    let mut member = sign_in_needed(now);
    let before = member.state().clone();
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    let _ = member.apply(MemberEvent::Activated, now, &mut NoJitter);
    assert_eq!(
        member.apply(MemberEvent::Deactivated, now, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(member.state(), &before);
}

#[test]
fn only_a_live_unpaused_member_is_checkable() {
    let now = Instant::now();
    for member in every_state(now) {
        assert_eq!(member.checkable(), member.state() == &MemberState::Live);
    }
    let mut paused = live(now);
    let _ = paused.apply(MemberEvent::Paused(Pause::Provisional), now, &mut NoJitter);
    assert!(!paused.checkable());
}

#[test]
fn a_pause_blocks_every_start_in_every_state() {
    let base = Instant::now();
    let far = base + Duration::from_secs(86_400);
    for pause in [Pause::Held, Pause::Provisional] {
        for state in 0..every_state(base).len() {
            for event in 0..every_event(attempt_id()).len() {
                let mut member = every_state(base).remove(state);
                let in_flight = member.attempt;
                let _ = member.apply(MemberEvent::Paused(pause), base, &mut NoJitter);

                let event = every_event(in_flight.unwrap_or_else(attempt_id)).remove(event);
                let action = member.apply(event, far, &mut NoJitter);

                assert!(
                    !matches!(action, MemberAction::Enqueue | MemberAction::StartNow),
                    "state {state}: {action:?}"
                );
                assert!(!member.begin_attempt(attempt_id()), "state {state}");
            }
        }
    }
}

#[test]
fn pausing_a_connecting_member_takes_it_out_of_the_queue() {
    let now = Instant::now();
    for mut member in [connecting(), attempting().0] {
        assert_eq!(
            member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter),
            MemberAction::Dequeue
        );
    }
}

#[test]
fn a_result_landing_during_a_pause_is_discarded_and_requeued() {
    let now = Instant::now();
    for outcome in [
        AttemptOutcome::Connected,
        AttemptOutcome::Failed(text("down")),
        AttemptOutcome::AuthRejected(text("401")),
    ] {
        let (mut member, attempt) = attempting();
        let _ = member.apply(MemberEvent::Paused(Pause::Provisional), now, &mut NoJitter);
        finish(&mut member, attempt, outcome, now);
        assert_eq!(member.state(), &MemberState::Connecting);
        assert_eq!(member.failures(), 0);
        assert_eq!(
            member.apply(MemberEvent::Resumed(Pause::Provisional), now, &mut NoJitter),
            MemberAction::Enqueue
        );
    }
}

#[test]
fn a_result_from_before_a_pause_is_discarded_after_it_too() {
    let now = Instant::now();
    let (mut member, attempt) = attempting();
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    let _ = member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter);
    finish(&mut member, attempt, AttemptOutcome::Connected, now);
    assert_eq!(member.state(), &MemberState::Connecting);
}

#[test]
fn a_stale_attempt_changes_nothing() {
    let now = Instant::now();
    let mut schedule = Schedule::<u32>::default();
    let stale = schedule.start_active(&0);
    let current = schedule.start_active(&0);
    let mut member = connecting();
    assert!(member.begin_attempt(current));
    let before = member.clone();
    finish(&mut member, stale, AttemptOutcome::Connected, now);
    assert_eq!(member, before);
}

#[test]
fn a_member_stays_paused_until_both_pauses_end() {
    let now = Instant::now();
    let mut member = connecting();
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    assert_eq!(
        member.apply(MemberEvent::Paused(Pause::Provisional), now, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(member.paused(), Some(Pause::Provisional));
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Provisional), now, &mut NoJitter),
        MemberAction::Enqueue
    );
    assert_eq!(member.paused(), None);
}

#[test]
fn resuming_a_pause_that_was_not_set_queues_nothing() {
    let now = Instant::now();
    let (mut member, _) = attempting();
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Held), now, &mut NoJitter),
        MemberAction::None
    );
}

#[test]
fn a_retry_due_during_a_pause_waits_for_the_next_tick_after_it() {
    let now = Instant::now();
    let mut member = failed(now);
    let due = now + BACKOFF_INITIAL;
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    assert_eq!(
        member.apply(MemberEvent::Tick, due, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(
        member.apply(MemberEvent::Resumed(Pause::Held), due, &mut NoJitter),
        MemberAction::None
    );
    assert_eq!(
        member.apply(MemberEvent::Tick, due, &mut NoJitter),
        MemberAction::Enqueue
    );
}

#[test]
fn running_members_keep_tracking_health_during_a_pause() {
    let now = Instant::now();
    let mut member = live(now);
    let _ = member.apply(MemberEvent::Paused(Pause::Held), now, &mut NoJitter);
    let _ = member.apply(lost_contact(), now, &mut NoJitter);
    assert!(matches!(member.state(), MemberState::Reconnecting { .. }));
    assert_eq!(
        member.apply(MemberEvent::Tick, now + RECONNECT_TIMEOUT, &mut NoJitter),
        MemberAction::StopWatches
    );
}

#[test]
fn begin_attempt_needs_a_connecting_member_with_none_in_flight() {
    let now = Instant::now();
    for mut member in [
        live(now),
        reconnecting(now),
        failed(now),
        sign_in_needed(now),
    ] {
        assert!(!member.begin_attempt(attempt_id()));
    }
    let (mut running, _) = attempting();
    assert!(!running.begin_attempt(attempt_id()));
}

#[test]
fn server_debug_never_shows_user_info() {
    let identity = ServerIdentity::new("https://user:secret@one.example:6443");
    assert!(!format!("{identity:?}").contains("secret"));
}
