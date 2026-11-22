use super::*;

/// Jitter returning a fixed value.
struct Fixed(u32);

impl Jitter for Fixed {
    fn sample(&mut self) -> u32 {
        self.0
    }
}

const STEP: Duration = Duration::from_millis(50);

/// Runs `schedule` in small steps, finishing each attempt `run_for` after it starts, and returns
/// the admissions in order with their start times.
fn drain(
    schedule: &mut Schedule<u32>,
    start: Instant,
    run_for: Duration,
    steps: u32,
) -> Vec<(u32, Instant)> {
    let mut started = Vec::new();
    let mut running: Vec<(AttemptId, Instant)> = Vec::new();
    for step in 0..steps {
        let now = start + STEP * step;
        running.retain(|(attempt, done)| {
            let finished = now >= *done;
            if finished {
                schedule.finish(*attempt);
            }
            !finished
        });
        if let Some(admission) = schedule.admit(now) {
            running.push((admission.attempt, now + run_for));
            started.push((admission.key, now));
        }
    }
    started
}

#[test]
fn at_most_two_attempts_hold_slots() {
    let now = Instant::now();
    let mut schedule = Schedule::default();
    (0..3).for_each(|key| schedule.enqueue(key));
    assert_eq!(schedule.admit(now).map(|a| a.key), Some(0));
    assert_eq!(
        schedule.admit(now + ATTEMPT_STAGGER).map(|a| a.key),
        Some(1)
    );
    assert_eq!(schedule.admit(now + ATTEMPT_STAGGER * 2), None);
}

#[test]
fn attempts_start_at_least_the_stagger_apart() {
    let now = Instant::now();
    let mut schedule = Schedule::default();
    (0..2).for_each(|key| schedule.enqueue(key));
    assert!(schedule.admit(now).is_some());
    let just_before = now + ATTEMPT_STAGGER - Duration::from_millis(1);
    assert_eq!(schedule.admit(just_before), None);
    assert!(schedule.admit(now + ATTEMPT_STAGGER).is_some());
}

#[test]
fn a_clock_going_backwards_admits_nothing() {
    let now = Instant::now() + Duration::from_secs(5);
    let mut schedule = Schedule::default();
    (0..2).for_each(|key| schedule.enqueue(key));
    assert!(schedule.admit(now).is_some());
    assert_eq!(schedule.admit(now - Duration::from_secs(1)), None);
}

#[test]
fn a_result_releases_the_slot() {
    let now = Instant::now();
    let mut schedule = Schedule::default();
    (0..3).for_each(|key| schedule.enqueue(key));
    let first = schedule.admit(now).expect("first");
    schedule.admit(now + ATTEMPT_STAGGER).expect("second");
    schedule.finish(first.attempt);
    let third = schedule.admit(now + ATTEMPT_STAGGER * 2);
    assert_eq!(third.map(|a| a.key), Some(2));
}

#[test]
fn a_silent_attempt_loses_its_slot_at_ten_seconds() {
    let now = Instant::now();
    let mut schedule = Schedule::default();
    (0..3).for_each(|key| schedule.enqueue(key));
    schedule.admit(now).expect("first");
    let second_at = now + ATTEMPT_STAGGER;
    schedule.admit(second_at).expect("second");

    let just_before = now + SLOT_RELEASE_AFTER - Duration::from_millis(1);
    assert_eq!(schedule.admit(just_before), None);
    let third = schedule.admit(now + SLOT_RELEASE_AFTER);
    assert_eq!(third.map(|a| a.key), Some(2));
}

#[test]
fn a_late_result_does_not_release_a_newer_slot() {
    let now = Instant::now();
    let mut schedule = Schedule::default();
    (0..4).for_each(|key| schedule.enqueue(key));
    let silent = schedule.admit(now).expect("first");
    schedule
        .admit(now + Duration::from_secs(5))
        .expect("second");
    let at = now + SLOT_RELEASE_AFTER;
    schedule.admit(at).expect("takes the expired slot");

    schedule.finish(silent.attempt);
    assert_eq!(schedule.admit(at + ATTEMPT_STAGGER), None);
}

#[test]
fn next_admission_names_when_admit_can_succeed() {
    let now = Instant::now();
    let mut schedule = Schedule::default();
    assert_eq!(schedule.next_admission(now), None);

    (0..3).for_each(|key| schedule.enqueue(key));
    assert_eq!(schedule.next_admission(now), Some(now));
    schedule.admit(now).expect("first");
    assert_eq!(schedule.next_admission(now), Some(now + ATTEMPT_STAGGER));

    let second_at = now + ATTEMPT_STAGGER;
    schedule.admit(second_at).expect("second");
    let free = schedule.next_admission(second_at).expect("queued");
    assert_eq!(free, now + SLOT_RELEASE_AFTER);
    assert!(schedule.admit(free).is_some());
}

#[test]
fn the_active_cluster_bypasses_queue_slots_and_stagger() {
    let now = Instant::now();
    let mut schedule = Schedule::default();
    (0..4).for_each(|key| schedule.enqueue(key));
    schedule.admit(now).expect("first");
    schedule.admit(now + ATTEMPT_STAGGER).expect("second");

    let active = schedule.start_active(&3);
    schedule.finish(active);
    assert_eq!(schedule.queue, VecDeque::from([2]));
    assert_eq!(schedule.slots.len(), 2);
    assert_eq!(schedule.last_start, Some(now + ATTEMPT_STAGGER));
}

#[test]
fn enqueue_keeps_a_waiting_members_place_and_dequeue_removes_it() {
    let mut schedule = Schedule::default();
    [1, 2, 1].into_iter().for_each(|key| schedule.enqueue(key));
    assert_eq!(schedule.queue, VecDeque::from([1, 2]));
    schedule.dequeue(&1);
    assert_eq!(schedule.queue, VecDeque::from([2]));
}

#[test]
fn backoff_doubles_to_the_cap() {
    // A sample of zero removes no jitter.
    let nominal: Vec<u64> = (1..=8)
        .map(|failures| backoff_delay(failures, &mut Fixed(0)).as_secs())
        .collect();
    assert_eq!(nominal, [2, 4, 8, 16, 32, 60, 60, 60]);
    assert_eq!(backoff_delay(0, &mut Fixed(0)), BACKOFF_INITIAL);
    assert_eq!(backoff_delay(u32::MAX, &mut Fixed(0)), BACKOFF_MAX);
}

#[test]
fn jitter_stays_within_a_quarter_below_nominal() {
    for failures in [1, 2, 3, 5, 6, 7, 40, u32::MAX] {
        let nominal = backoff_delay(failures, &mut Fixed(0));
        let floor = nominal * 3 / 4;
        let samples = (0..2_000u32)
            .map(|n| n.wrapping_mul(2_654_435_761))
            .chain([u32::MAX, u32::MAX - 1]);
        for sample in samples {
            let delay = backoff_delay(failures, &mut Fixed(sample));
            assert!(delay >= floor && delay <= nominal, "{failures}: {delay:?}");
            assert!(delay <= BACKOFF_MAX);
        }
    }
}

#[test]
fn jitter_reaches_both_ends_of_its_range() {
    let nominal = backoff_delay(1, &mut Fixed(0));
    let spread = (nominal.as_millis() as u32) / 4;
    assert_eq!(backoff_delay(1, &mut Fixed(spread)), nominal * 3 / 4);
}

#[test]
fn a_mass_outage_reconnects_in_order_and_staggered() {
    let start = Instant::now();
    let mut schedule = Schedule::default();
    (0..20).for_each(|key| schedule.enqueue(key));

    let started = drain(&mut schedule, start, Duration::from_millis(900), 1_000);

    let order: Vec<u32> = started.iter().map(|(key, _)| *key).collect();
    assert_eq!(order, (0..20).collect::<Vec<_>>());
    for pair in started.windows(2) {
        assert!(pair[1].1 - pair[0].1 >= ATTEMPT_STAGGER);
    }
}

#[test]
fn no_more_than_two_run_during_a_mass_outage() {
    let start = Instant::now();
    let run_for = Duration::from_secs(3);
    let mut schedule = Schedule::default();
    (0..12).for_each(|key| schedule.enqueue(key));

    let started = drain(&mut schedule, start, run_for, 1_000);

    assert_eq!(started.len(), 12);
    for (_, at) in &started {
        let overlapping = started
            .iter()
            .filter(|(_, other)| other <= at && *at < *other + run_for)
            .count();
        assert!(overlapping <= MAX_BACKGROUND_ATTEMPTS);
    }
}

#[test]
fn a_member_that_keeps_failing_does_not_starve_the_others() {
    let start = Instant::now();
    let mut schedule = Schedule::default();
    (0..6).for_each(|key| schedule.enqueue(key));
    let mut admitted = [0u32; 6];

    for step in 0..2_000 {
        let now = start + STEP * step;
        if let Some(admission) = schedule.admit(now) {
            admitted[admission.key as usize] += 1;
            // Every attempt fails at once and goes straight back in the queue.
            schedule.finish(admission.attempt);
            schedule.enqueue(admission.key);
        }
    }

    let fewest = admitted.iter().min().expect("members");
    let most = admitted.iter().max().expect("members");
    assert!(*fewest > 0);
    assert!(most - fewest <= 1, "{admitted:?}");
}

#[test]
fn hung_attempts_do_not_starve_the_queue() {
    let start = Instant::now();
    let mut schedule = Schedule::default();
    (0..5).for_each(|key| schedule.enqueue(key));

    // No attempt ever reports a result.
    let mut started = Vec::new();
    for step in 0..1_000 {
        if let Some(admission) = schedule.admit(start + STEP * step) {
            started.push(admission.key);
        }
    }

    assert_eq!(started, [0, 1, 2, 3, 4]);
}
