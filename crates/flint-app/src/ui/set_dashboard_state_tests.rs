//! The set dashboard as the automation state reports it: the order, the totals, a member that
//! cannot be read, and the partial note.

use flint_core::dashboard::{ProblemCategory, ProblemCounts};
use flint_core::thresholds::ThresholdLevel;

use super::super::feed::FeedId;
use super::super::set::{CardState, SetCard, assemble};
use super::super::{ProblemTile, category_label};

fn card(cluster: &str, state: CardState, failing: Option<u32>) -> SetCard {
    let counting = state.counts();
    let count = move |category| {
        if !counting {
            None
        } else if category == ProblemCategory::FailingPods {
            failing
        } else {
            Some(0)
        }
    };
    SetCard {
        cluster: cluster.to_owned(),
        shown: cluster.to_owned(),
        tag: None,
        tiles: ProblemCategory::ALL
            .into_iter()
            .map(|category| ProblemTile {
                category,
                label: category_label(category),
                count: count(category),
                level: ThresholdLevel::Healthy,
            })
            .collect(),
        counts: ProblemCounts::from_fn(count),
        level: ThresholdLevel::Healthy,
        cpu: None,
        memory: None,
        metrics_notice: None,
        short_kinds: vec!["pods"],
        feed: counting.then(|| FeedId::for_test(cluster)),
        state,
    }
}

#[test]
fn cards_are_reported_worst_first() {
    let state = assemble(vec![
        card("calm", CardState::Live, Some(0)),
        card("bad", CardState::Live, Some(3)),
        card("worse", CardState::Live, Some(5)),
    ])
    .view_state();
    let order: Vec<_> = state.cards.iter().map(|c| c.cluster.as_str()).collect();
    assert_eq!(order, ["worse", "bad", "calm"]);
    let failing = &state.cards[0].counts[0];
    assert_eq!(
        (failing.category, failing.count, failing.opens),
        ("failing_pods", Some(5), true)
    );
}

#[test]
fn a_member_that_cannot_be_read_has_a_card_that_counts_nothing() {
    let state = assemble(vec![
        card("up", CardState::Live, Some(1)),
        card("down", CardState::Unavailable("gone".into()), None),
    ])
    .view_state();
    let down = state.cards.iter().find(|c| c.cluster == "down").unwrap();
    assert_eq!(down.state, "unavailable");
    assert_eq!(down.note.as_deref(), Some("gone"));
    assert!(down.counts.iter().all(|c| c.count.is_none() && !c.opens));
    assert_eq!(state.cards[0].cluster, "up");
}

#[test]
fn a_partially_checked_member_reports_its_metrics_notice_too() {
    let mut member = card(
        "member",
        CardState::Partial("Could not check pods.".into()),
        Some(0),
    );
    member.metrics_notice = Some("No metrics-server installed.".into());
    let state = assemble(vec![member]).view_state();
    assert_eq!(
        state.cards[0].note.as_deref(),
        Some("Could not check pods. No metrics-server installed.")
    );
}

#[test]
fn totals_say_how_many_members_they_lack() {
    let state = assemble(vec![
        card("a", CardState::Live, Some(2)),
        card("b", CardState::Live, Some(1)),
        card("c", CardState::Blocked("Paused".into()), None),
    ])
    .view_state();
    let failing = state
        .totals
        .iter()
        .find(|t| t.category == "failing_pods")
        .unwrap();
    assert_eq!(
        (failing.value.as_str(), failing.known, failing.missing),
        ("3+", 3, 1)
    );
    assert!(failing.opens);
}

#[test]
fn the_partial_note_names_the_members_not_counted() {
    let state = assemble(vec![
        card("a", CardState::Live, Some(0)),
        card("c", CardState::Blocked("Paused".into()), None),
    ])
    .view_state();
    let note = state.partial.expect("a member is not counted");
    assert!(note.contains("1 of 2") && note.contains('c'), "{note}");
    let whole = assemble(vec![card("a", CardState::Live, Some(0))]).view_state();
    assert_eq!(whole.partial, None);
    assert_eq!(whole.totals[0].value, "0");
}
