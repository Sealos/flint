//! Watch streams: scripted steps and live events, encoded as the API server sends Table watches
//! (see `docs/spikes/table-watch.md`):
//!
//! - one JSON line per event, `{"type": ..., "object": <Table with one row>}`;
//! - only the first event of a stream carries `columnDefinitions`, later ones `null`;
//! - a `BOOKMARK` is a Table with a fake row of empty cells and the resource version;
//! - an expired resource version is an `ERROR` event holding a 410 `Status`, inside HTTP 200;
//! - a `MODIFIED` object that leaves the watch's selector arrives as `DELETED`, one that enters
//!   it as `ADDED`.
//!
//! Resources the server answers without Tables ([`super::Options::plain_only`]) get plain
//! objects in their events instead.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hyper::body::Frame;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::clock::Clock;
use super::fixtures::Gvr;
use super::route::Query;
use super::row::FakeRow;
use super::selector::{FieldSelector, LabelSelector};
use super::state::{Change, State, status};

/// One step of a watch stream.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchStep {
    /// The object is created: added to the server's rows and sent as `ADDED`.
    Added(FakeRow),
    /// The object changes: replaced in the server's rows and sent as `MODIFIED`.
    Modified(FakeRow),
    /// The object (matched by namespace and name) is removed and its last state sent as
    /// `DELETED`.
    Deleted(FakeRow),
    /// A `BOOKMARK` at the current resource version (only if the client allows bookmarks).
    Bookmark,
    /// An `ERROR` event with a 410 `Expired` status; the stream then ends, as on a real server.
    Expired,
    /// Nothing is sent for a while.
    Wait(Duration),
    /// The stream ends normally, as when the server-side watch timeout passes.
    End,
    /// The connection breaks mid-stream.
    Drop,
}

impl WatchStep {
    /// The event type and row of a row step.
    pub(crate) fn row_change(&self) -> Option<(&'static str, &FakeRow)> {
        match self {
            WatchStep::Added(row) => Some(("ADDED", row)),
            WatchStep::Modified(row) => Some(("MODIFIED", row)),
            WatchStep::Deleted(row) => Some(("DELETED", row)),
            _ => None,
        }
    }
}

/// What a watch stream is told to send.
#[derive(Debug, Clone)]
pub(crate) enum Outgoing {
    /// A scripted step; row steps change the server's rows when they run.
    Step(WatchStep),
    /// A change already applied to the server's rows.
    Event(Change),
    /// The end of a streaming list's initial events.
    InitialEventsEnd,
}

/// Which events a watch request sees, and how they are encoded.
#[derive(Debug, Clone)]
pub(crate) struct Filter {
    pub gvr: Gvr,
    pub namespace: Option<String>,
    pub query: Query,
    /// Events carry plain objects instead of Tables.
    pub plain: bool,
}

impl Filter {
    fn admits(&self, row: &FakeRow) -> bool {
        let object = &row.as_json()["object"];
        self.namespace
            .as_deref()
            .is_none_or(|ns| row.namespace() == Some(ns))
            && self
                .query
                .field_selector
                .as_deref()
                .is_none_or(|s| FieldSelector::parse(s).matches(object))
            && self
                .query
                .label_selector
                .as_deref()
                .is_none_or(|s| LabelSelector::parse(s).matches(object))
    }

    /// The event type this watch sees for a change, if any.
    fn event_type(&self, change: &Change) -> Option<&'static str> {
        let now = self.admits(&change.row);
        if change.ty != "MODIFIED" {
            return now.then_some(change.ty);
        }
        let before = change.previous.as_ref().map_or(now, |p| self.admits(p));
        match (before, now) {
            (true, true) => Some("MODIFIED"),
            (true, false) => Some("DELETED"),
            (false, true) => Some("ADDED"),
            (false, false) => None,
        }
    }
}

/// Encodes events of one stream; remembers whether columns were sent yet.
pub(crate) struct Encoder {
    gvr: Gvr,
    plain: bool,
    sent_columns: bool,
}

impl Encoder {
    pub fn new(gvr: Gvr, plain: bool) -> Self {
        Encoder {
            gvr,
            plain,
            sent_columns: false,
        }
    }

    fn line(ty: &str, object: Value) -> Bytes {
        let mut text = json!({"type": ty, "object": object}).to_string();
        text.push('\n');
        Bytes::from(text)
    }

    pub fn event(&mut self, state: &State, clock: &Clock, ty: &str, row: &FakeRow) -> Bytes {
        let mut object = if self.plain {
            state.object_of(&self.gvr, row)
        } else {
            let meta = json!({"resourceVersion": row.resource_version()});
            let table = state.table(
                &self.gvr,
                vec![row.as_json().clone()],
                meta,
                !self.sent_columns,
            );
            self.sent_columns = true;
            table
        };
        clock.rebase(&mut object);
        Encoder::line(ty, object)
    }

    pub fn bookmark(&mut self, state: &State, initial_events_end: bool) -> Bytes {
        let rv = state.resource_version().to_string();
        let mut metadata = json!({"resourceVersion": rv});
        if initial_events_end {
            metadata["annotations"] = json!({"k8s.io/initial-events-end": "true"});
        }
        if self.plain {
            let object = json!({"kind": state.kind(&self.gvr), "apiVersion": self.gvr.api_version(),
                                "metadata": metadata});
            return Encoder::line("BOOKMARK", object);
        }
        let width = state
            .columns(&self.gvr)
            .as_array()
            .map_or(1, Vec::len)
            .max(1);
        let mut cells = vec![Value::Null; width];
        cells[0] = "".into();
        let row = json!({"cells": cells, "object": {"kind": "PartialObjectMetadata",
                         "apiVersion": "meta.k8s.io/v1", "metadata": metadata}});
        let table = state.table(
            &self.gvr,
            vec![row],
            json!({"resourceVersion": rv}),
            !self.sent_columns,
        );
        self.sent_columns = true;
        Encoder::line("BOOKMARK", table)
    }

    pub fn expired(requested: Option<&str>, state: &State) -> Bytes {
        let message = format!(
            "too old resource version: {} ({})",
            requested.unwrap_or("0"),
            state.resource_version()
        );
        Encoder::line("ERROR", status(410, "Expired", &message))
    }
}

pub(crate) type BodySender = mpsc::Sender<Result<Frame<Bytes>, io::Error>>;

/// How a stream goes on after a step.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Continue,
    End,
    Drop,
}

/// What a stream runs on.
pub(crate) struct Stream {
    pub state: Arc<Mutex<State>>,
    pub clock: Clock,
    pub filter: Filter,
    pub steps: mpsc::UnboundedReceiver<Outgoing>,
    pub body: BodySender,
    pub shutdown: watch::Receiver<()>,
}

/// Runs one watch stream until it ends, the client goes away or the server stops.
pub(crate) async fn run(mut stream: Stream) {
    let mut encoder = Encoder::new(stream.filter.gvr.clone(), stream.filter.plain);
    let deadline = stream
        .filter
        .query
        .timeout_seconds
        .map(|s| Instant::now() + Duration::from_secs(s));
    let body = stream.body.clone();
    loop {
        let outgoing = tokio::select! {
            o = stream.steps.recv() => o,
            () = body.closed() => return,
            _ = stream.shutdown.changed() => return,
            () = sleep_until_opt(deadline) => return,
        };
        let Some(outgoing) = outgoing else { return };
        if let Outgoing::Step(WatchStep::Wait(delay)) = outgoing {
            tokio::select! {
                () = tokio::time::sleep(delay) => continue,
                () = body.closed() => return,
                _ = stream.shutdown.changed() => return,
            }
        }
        let (bytes, next) = encode(&stream, &mut encoder, outgoing);
        if let Some(bytes) = bytes
            && body.send(Ok(Frame::data(bytes))).await.is_err()
        {
            return;
        }
        match next {
            Next::Continue => {}
            Next::End => return,
            Next::Drop => {
                let _ = body
                    .send(Err(io::Error::other("fault: watch connection dropped")))
                    .await;
                return;
            }
        }
    }
}

fn encode(stream: &Stream, encoder: &mut Encoder, outgoing: Outgoing) -> (Option<Bytes>, Next) {
    let filter = &stream.filter;
    let mut state = stream.state.lock().expect("state lock");
    let send = |encoder: &mut Encoder, state: &State, change: &Change| {
        filter
            .event_type(change)
            .map(|ty| encoder.event(state, &stream.clock, ty, &change.row))
    };
    match outgoing {
        Outgoing::Event(change) => (send(encoder, &state, &change), Next::Continue),
        Outgoing::InitialEventsEnd => (
            filter
                .query
                .allow_watch_bookmarks
                .then(|| encoder.bookmark(&state, true)),
            Next::Continue,
        ),
        Outgoing::Step(step) => {
            if let Some((ty, row)) = step.row_change() {
                let change = state.change(&filter.gvr, ty, row.clone());
                return (send(encoder, &state, &change), Next::Continue);
            }
            match step {
                WatchStep::Bookmark => (
                    filter
                        .query
                        .allow_watch_bookmarks
                        .then(|| encoder.bookmark(&state, false)),
                    Next::Continue,
                ),
                WatchStep::Expired => {
                    let requested = filter.query.resource_version.as_deref();
                    (Some(Encoder::expired(requested, &state)), Next::End)
                }
                WatchStep::End => (None, Next::End),
                WatchStep::Drop => (None, Next::Drop),
                _ => (None, Next::Continue),
            }
        }
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_api::fixtures::Fixtures;
    use std::time::SystemTime;

    fn state() -> State {
        State::new(&Fixtures::shared_default().unwrap())
    }

    fn row(name: &str) -> FakeRow {
        FakeRow::build(Some("demo"), name, vec![name.into(), 0.into(), "1d".into()])
    }

    fn parse(bytes: &Bytes) -> Value {
        let text = std::str::from_utf8(bytes).unwrap();
        assert!(text.ends_with('\n'));
        serde_json::from_str(text).unwrap()
    }

    fn filter(namespace: Option<&str>, query: Query) -> Filter {
        Filter {
            gvr: Gvr::core("configmaps"),
            namespace: namespace.map(str::to_owned),
            query,
            plain: false,
        }
    }

    fn change(ty: &'static str, row: FakeRow, previous: Option<FakeRow>) -> Change {
        Change {
            gvr: Gvr::core("configmaps"),
            resource_version: 1,
            ty,
            row,
            previous,
        }
    }

    #[test]
    fn only_the_first_event_carries_columns() {
        let state = state();
        let clock = Clock::new(SystemTime::now());
        let mut encoder = Encoder::new(Gvr::core("configmaps"), false);
        let first = parse(&encoder.event(&state, &clock, "ADDED", &row("a")));
        let second = parse(&encoder.bookmark(&state, false));
        assert_eq!(first["type"], "ADDED");
        assert!(first["object"]["columnDefinitions"].is_array());
        assert_eq!(
            first["object"]["rows"][0]["object"]["metadata"]["name"],
            "a"
        );
        // A row built in a test was created "now".
        assert_eq!(first["object"]["rows"][0]["cells"][2], "0s");
        assert!(second["object"]["columnDefinitions"].is_null());
        let cells = second["object"]["rows"][0]["cells"].as_array().unwrap();
        assert_eq!(cells[0], "");
        assert!(cells[1..].iter().all(Value::is_null));
        let end = parse(&Encoder::new(Gvr::core("pods"), false).bookmark(&state, true));
        assert!(end["object"]["columnDefinitions"].is_array());
        assert_eq!(
            end["object"]["rows"][0]["object"]["metadata"]["annotations"]["k8s.io/initial-events-end"],
            "true"
        );
        let expired = parse(&Encoder::expired(None, &state));
        assert_eq!(
            (expired["type"].as_str(), expired["object"]["code"].as_u64()),
            (Some("ERROR"), Some(410))
        );
        let mut plain = Encoder::new(Gvr::core("configmaps"), true);
        let event = parse(&plain.event(&state, &clock, "ADDED", &row("a")));
        assert_eq!(event["object"]["kind"], "ConfigMap");
        let bookmark = parse(&plain.bookmark(&state, false));
        assert_eq!(bookmark["object"]["metadata"]["resourceVersion"], "1000");
    }

    #[test]
    fn filters_by_namespace_and_selectors() {
        let labelled = row("a").with_label("app", "web");
        assert!(filter(None, Query::default()).admits(&labelled));
        assert!(filter(Some("demo"), Query::default()).admits(&labelled));
        assert!(!filter(Some("other"), Query::default()).admits(&labelled));
        let by_label = Query {
            label_selector: Some("app=web".into()),
            ..Query::default()
        };
        assert!(filter(None, by_label.clone()).admits(&labelled));
        assert!(!filter(None, by_label).admits(&row("b")));
        let by_name = Query {
            field_selector: Some("metadata.name=b".into()),
            ..Query::default()
        };
        assert!(!filter(None, by_name).admits(&labelled));
    }

    #[test]
    fn leaving_or_entering_a_selector_is_a_delete_or_an_add() {
        let web = Query {
            label_selector: Some("app=web".into()),
            ..Query::default()
        };
        let f = filter(None, web);
        let inside = row("a").with_label("app", "web");
        let outside = row("a").with_label("app", "db");
        let ty = |c: Change| f.event_type(&c);
        assert_eq!(
            ty(change("MODIFIED", outside.clone(), Some(inside.clone()))),
            Some("DELETED")
        );
        assert_eq!(
            ty(change("MODIFIED", inside.clone(), Some(outside.clone()))),
            Some("ADDED")
        );
        assert_eq!(
            ty(change("MODIFIED", inside.clone(), Some(inside.clone()))),
            Some("MODIFIED")
        );
        assert_eq!(
            ty(change("MODIFIED", outside.clone(), Some(outside.clone()))),
            None
        );
        assert_eq!(
            ty(change("MODIFIED", inside.clone(), None)),
            Some("MODIFIED")
        );
        assert_eq!(ty(change("ADDED", outside, None)), None);
        assert_eq!(ty(change("DELETED", inside, None)), Some("DELETED"));
    }

    async fn collect(steps: Vec<Outgoing>, query: Query) -> Vec<Result<Value, String>> {
        let (tx, rx) = mpsc::unbounded_channel();
        for s in steps {
            tx.send(s).unwrap();
        }
        let (body_tx, mut body_rx) = mpsc::channel(16);
        let (_shutdown_tx, shutdown) = watch::channel(());
        let task = tokio::spawn(run(Stream {
            state: Arc::new(Mutex::new(state())),
            clock: Clock::new(SystemTime::now()),
            filter: filter(None, query),
            steps: rx,
            body: body_tx,
            shutdown,
        }));
        let mut out = Vec::new();
        while let Some(frame) = body_rx.recv().await {
            out.push(match frame {
                Ok(f) => Ok(parse(f.data_ref().unwrap())),
                Err(e) => Err(e.to_string()),
            });
        }
        task.await.unwrap();
        drop(tx);
        out
    }

    #[tokio::test]
    async fn streams_end_drop_or_time_out() {
        let bookmarks = Query {
            allow_watch_bookmarks: true,
            ..Query::default()
        };
        let out = collect(
            vec![
                Outgoing::Step(WatchStep::Added(row("a"))),
                Outgoing::Step(WatchStep::Wait(Duration::from_millis(1))),
                Outgoing::Step(WatchStep::Bookmark),
                Outgoing::InitialEventsEnd,
                Outgoing::Step(WatchStep::End),
                Outgoing::Step(WatchStep::Added(row("b"))),
            ],
            bookmarks,
        )
        .await;
        assert_eq!(out.len(), 3);
        // Without allowWatchBookmarks, bookmarks are not sent, not even the end marker.
        let out = collect(
            vec![
                Outgoing::Step(WatchStep::Bookmark),
                Outgoing::InitialEventsEnd,
                Outgoing::Step(WatchStep::Drop),
            ],
            Query::default(),
        )
        .await;
        assert_eq!(out, vec![Err("fault: watch connection dropped".to_owned())]);
        let timeout = Query {
            timeout_seconds: Some(0),
            ..Query::default()
        };
        assert!(collect(vec![], timeout).await.is_empty());
    }
}
