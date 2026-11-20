use std::sync::Arc;

use super::*;
use crate::handle::ConnectionState;

fn connection(cluster: &str, state: ConnectionState) -> Event {
    Event::ConnectionChanged {
        cluster: cluster.into(),
        state,
    }
}

fn name(event: &Event) -> String {
    match event {
        Event::ActiveChanged(active) => format!("active {active:?}"),
        Event::ConnectionChanged { cluster, state } => format!("{cluster} {state:?}"),
        Event::Error(message) => format!("error {message}"),
        other => format!("{other:?}"),
    }
}

/// Everything queued now, in order.
async fn drain(events: &Events) -> Vec<String> {
    let mut out = Vec::new();
    while !events.is_empty() {
        out.push(name(&events.recv().await.unwrap()));
    }
    out
}

// covers UP-01: a state event replaces the queued one of its kind and cluster, and goes to the
// back; reading what is left ends where reading everything would.
#[tokio::test]
async fn state_events_keep_only_their_latest() {
    let (sender, events) = channel();
    sender.send(Event::ActiveChanged(Some("a".into())));
    sender.send(connection("a", ConnectionState::Connecting));
    sender.send(connection("b", ConnectionState::Connecting));
    for _ in 0..100 {
        sender.send(Event::ActiveChanged(Some("b".into())));
    }
    sender.send(connection("a", ConnectionState::Disconnected));
    assert_eq!(sender.queued(), 4);
    assert_eq!(
        drain(&events).await,
        [
            "a Connecting",
            "b Connecting",
            "active Some(\"b\")",
            "a Disconnected",
        ]
    );
}

// covers UP-01: errors are kept in order, at most MAX_ERRORS; past that the oldest go.
#[tokio::test]
async fn errors_are_bounded_dropping_the_oldest() {
    let (sender, events) = channel();
    for n in 0..MAX_ERRORS + 10 {
        sender.send(Event::Error(format!("{n}")));
    }
    sender.send(Event::ActiveChanged(None));
    assert_eq!(sender.queued(), MAX_ERRORS + 1);
    let read = drain(&events).await;
    assert_eq!(read[0], "error 10");
    assert_eq!(read[MAX_ERRORS - 1], format!("error {}", MAX_ERRORS + 9));
    assert_eq!(read[MAX_ERRORS], "active None");
}

// covers UP-01: events sent before any reader are kept (bounded); once every reader is gone,
// nothing is queued and what was queued is let go; a new reader starts afresh.
#[tokio::test]
async fn the_reader_owns_the_queue() {
    let (sender, source) = queue();
    sender.send(Event::ActiveChanged(Some("a".into())));
    let events = source.events();
    let second = events.clone();
    assert_eq!(drain(&second).await, ["active Some(\"a\")"]);
    drop(events);
    sender.send(Event::ActiveChanged(Some("b".into())));
    assert_eq!(sender.queued(), 1, "one reader is left");
    drop(second);
    assert_eq!(sender.queued(), 0, "nobody reads them");
    for _ in 0..10 {
        sender.send(Event::Error("x".into()));
    }
    assert_eq!(sender.queued(), 0, "nothing is queued for nobody");
    let again = source.events();
    sender.send(Event::ActiveChanged(None));
    assert_eq!(drain(&again).await, ["active None"]);
}

// covers UP-01: a reader waits for the next event, and hears the end once the manager is gone
// and everything was read.
#[tokio::test]
async fn readers_wait_and_hear_the_end() {
    let (sender, events) = channel();
    let events = Arc::new(events);
    let reading = tokio::spawn({
        let events = events.clone();
        async move { events.recv().await.map(|e| name(&e)) }
    });
    tokio::task::yield_now().await;
    sender.send(Event::Error("late".into()));
    assert_eq!(reading.await.unwrap(), Ok("error late".to_owned()));
    sender.send(Event::ActiveChanged(None));
    drop(sender);
    assert!(events.recv().await.is_ok(), "what was queued is still read");
    assert_eq!(events.recv().await.err(), Some(Closed));
}

// covers UP-04: an attempt that starts and fails before being read is still seen starting
// (the app shows it connecting), and the last state read is the current one.
#[tokio::test]
async fn each_connection_attempt_is_seen_starting() {
    let (sender, events) = channel();
    for _ in 0..5 {
        sender.send(connection("b", ConnectionState::Connecting));
        sender.send(connection(
            "b",
            ConnectionState::Failed(crate::SafeErrorText::fixed("refused")),
        ));
    }
    let read = drain(&events).await;
    assert_eq!(read.len(), 2, "{read:?}");
    assert_eq!(read[0], "b Connecting");
    assert!(read[1].starts_with("b Failed"), "{read:?}");
    sender.send(connection("b", ConnectionState::Connecting));
    assert_eq!(drain(&events).await, ["b Connecting"]);
}
