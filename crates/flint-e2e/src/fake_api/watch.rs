//! Watch streams: scripted steps and live events, encoded as the API server sends Table watches
//! (see `docs/spikes/table-watch.md`):
//!
//! - one JSON line per event, `{"type": ..., "object": <Table with one row>}`;
//! - only the first event of a stream carries `columnDefinitions`, later ones `null`;
//! - a `BOOKMARK` is a Table with a fake row of empty cells and the resource version;
//! - an expired resource version is an `ERROR` event holding a 410 `Status`, inside HTTP 200.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hyper::body::Frame;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::fixtures::Gvr;
use super::route::Query;
use super::row::FakeRow;
use super::selector::{FieldSelector, LabelSelector};
use super::state::{State, status};

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

/// What a watch stream is told to send.
#[derive(Debug, Clone)]
pub(crate) enum Outgoing {
    /// A scripted step; row steps change the server's rows when they run.
    Step(WatchStep),
    /// A row event already applied to the server's rows (sent to every open watch).
    Event(&'static str, FakeRow),
    /// The end of a streaming list's initial events.
    InitialEventsEnd,
}

/// Which events a watch request sees.
#[derive(Debug, Clone)]
pub(crate) struct Filter {
    pub gvr: Gvr,
    pub namespace: Option<String>,
    pub query: Query,
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
}

/// Encodes events of one stream; remembers whether columns were sent yet.
pub(crate) struct Encoder {
    gvr: Gvr,
    sent_columns: bool,
}

impl Encoder {
    pub fn new(gvr: Gvr) -> Self {
        Encoder {
            gvr,
            sent_columns: false,
        }
    }

    fn line(ty: &str, object: Value) -> Bytes {
        let mut text = json!({"type": ty, "object": object}).to_string();
        text.push('\n');
        Bytes::from(text)
    }

    pub fn event(&mut self, state: &State, ty: &str, row: &FakeRow) -> Bytes {
        let meta = json!({"resourceVersion": row.resource_version()});
        let table = state.table(
            &self.gvr,
            vec![row.as_json().clone()],
            meta,
            !self.sent_columns,
        );
        self.sent_columns = true;
        Encoder::line(ty, table)
    }

    pub fn bookmark(&mut self, state: &State, initial_events_end: bool) -> Bytes {
        let rv = state.resource_version().to_string();
        let width = state
            .columns(&self.gvr)
            .as_array()
            .map_or(1, Vec::len)
            .max(1);
        let mut cells = vec![Value::Null; width];
        cells[0] = "".into();
        let mut metadata = json!({"resourceVersion": rv});
        if initial_events_end {
            metadata["annotations"] = json!({"k8s.io/initial-events-end": "true"});
        }
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

/// Applies a scripted row step to the rows; returns the event to send.
fn apply(state: &mut State, gvr: &Gvr, step: &WatchStep) -> Option<(&'static str, FakeRow)> {
    match step {
        WatchStep::Added(row) => Some(("ADDED", state.upsert_row(gvr, row.clone()))),
        WatchStep::Modified(row) => Some(("MODIFIED", state.upsert_row(gvr, row.clone()))),
        WatchStep::Deleted(row) => {
            let last = state
                .remove_row(gvr, row.namespace(), row.name())
                .unwrap_or_else(|| {
                    let mut gone = row.clone();
                    gone.set_resource_version(state.bump());
                    gone
                });
            Some(("DELETED", last))
        }
        _ => None,
    }
}

pub(crate) type BodySender = mpsc::Sender<Result<Frame<Bytes>, io::Error>>;

/// How a stream ended.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Continue,
    End,
    Drop,
}

/// Runs one watch stream until it ends, the client goes away or the server stops.
pub(crate) async fn run(
    state: Arc<Mutex<State>>,
    filter: Filter,
    mut steps: mpsc::UnboundedReceiver<Outgoing>,
    body: BodySender,
    mut shutdown: watch::Receiver<()>,
) {
    let mut encoder = Encoder::new(filter.gvr.clone());
    let deadline = filter
        .query
        .timeout_seconds
        .map(|s| Instant::now() + Duration::from_secs(s));
    loop {
        let outgoing = tokio::select! {
            o = steps.recv() => o,
            () = body.closed() => return,
            _ = shutdown.changed() => return,
            () = sleep_until_opt(deadline) => return,
        };
        let Some(outgoing) = outgoing else { return };
        if let Outgoing::Step(WatchStep::Wait(delay)) = outgoing {
            tokio::select! {
                () = tokio::time::sleep(delay) => continue,
                () = body.closed() => return,
                _ = shutdown.changed() => return,
            }
        }
        let (bytes, next) = encode(&state, &filter, &mut encoder, outgoing);
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

fn encode(
    state: &Mutex<State>,
    filter: &Filter,
    encoder: &mut Encoder,
    outgoing: Outgoing,
) -> (Option<Bytes>, Next) {
    let mut state = state.lock().expect("state lock");
    match outgoing {
        Outgoing::Event(ty, row) => {
            let bytes = filter.admits(&row).then(|| encoder.event(&state, ty, &row));
            (bytes, Next::Continue)
        }
        Outgoing::InitialEventsEnd => (Some(encoder.bookmark(&state, true)), Next::Continue),
        Outgoing::Step(step) => match step {
            WatchStep::Added(_) | WatchStep::Modified(_) | WatchStep::Deleted(_) => {
                let (ty, row) = apply(&mut state, &filter.gvr, &step).expect("a row step");
                let bytes = filter.admits(&row).then(|| encoder.event(&state, ty, &row));
                (bytes, Next::Continue)
            }
            WatchStep::Bookmark => {
                let bytes = filter
                    .query
                    .allow_watch_bookmarks
                    .then(|| encoder.bookmark(&state, false));
                (bytes, Next::Continue)
            }
            WatchStep::Expired => {
                let requested = filter.query.resource_version.as_deref();
                (Some(Encoder::expired(requested, &state)), Next::End)
            }
            WatchStep::Wait(_) => (None, Next::Continue),
            WatchStep::End => (None, Next::End),
            WatchStep::Drop => (None, Next::Drop),
        },
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

    fn state() -> State {
        State::new(&Fixtures::load_default().unwrap())
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
        }
    }

    #[test]
    fn only_the_first_event_carries_columns() {
        let state = state();
        let mut encoder = Encoder::new(Gvr::core("configmaps"));
        let first = parse(&encoder.event(&state, "ADDED", &row("a")));
        let second = parse(&encoder.bookmark(&state, false));
        assert_eq!(first["type"], "ADDED");
        assert!(first["object"]["columnDefinitions"].is_array());
        assert_eq!(
            first["object"]["rows"][0]["object"]["metadata"]["name"],
            "a"
        );
        assert!(second["object"]["columnDefinitions"].is_null());
        let cells = second["object"]["rows"][0]["cells"].as_array().unwrap();
        assert_eq!(cells[0], "");
        assert!(cells[1..].iter().all(Value::is_null));
        assert!(
            second["object"]["rows"][0]["object"]["metadata"]
                .get("annotations")
                .is_none()
        );
        let end = parse(&Encoder::new(Gvr::core("pods")).bookmark(&state, true));
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
    fn deleting_an_unknown_row_still_sends_it() {
        let mut state = state();
        let gvr = Gvr::core("configmaps");
        let (ty, gone) = apply(&mut state, &gvr, &WatchStep::Deleted(row("ghost"))).unwrap();
        assert_eq!(
            (ty, gone.name(), gone.resource_version()),
            ("DELETED", "ghost", "1001")
        );
        assert!(apply(&mut state, &gvr, &WatchStep::Bookmark).is_none());
    }

    async fn collect(steps: Vec<Outgoing>, query: Query) -> Vec<Result<Value, String>> {
        let (tx, rx) = mpsc::unbounded_channel();
        for s in steps {
            tx.send(s).unwrap();
        }
        let (body_tx, mut body_rx) = mpsc::channel(16);
        let (_shutdown_tx, shutdown) = watch::channel(());
        let state = Arc::new(Mutex::new(state()));
        let task = tokio::spawn(run(state, filter(None, query), rx, body_tx, shutdown));
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
                Outgoing::Step(WatchStep::Bookmark),
                Outgoing::Step(WatchStep::End),
                Outgoing::Step(WatchStep::Added(row("b"))),
            ],
            bookmarks,
        )
        .await;
        assert_eq!(out.len(), 2);
        // Without allowWatchBookmarks, bookmarks are not sent.
        let out = collect(
            vec![
                Outgoing::Step(WatchStep::Bookmark),
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
