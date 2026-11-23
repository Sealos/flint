//! The core's event queue (#255): what the manager tells the front end, bounded however slowly
//! (or whether) it is read.
//!
//! Events that state something (the cluster list, the active cluster, a cluster's connection or
//! discovery, the settings, the agent activity log) matter only in their latest form, so the
//! queue keeps one per key (a connection keeps two: its latest attempt starting, and where that
//! attempt settled): a newer one replaces the queued one, and moves to the back, so the
//! order of what is left is the order of the last changes, and applying them leaves the reader
//! where applying every event would have. (Readers that compare successive cluster lists see
//! fewer of them; the manager sends a discovery reset for a context that starts over, so they do
//! not need to.) [`Event::Error`]s are kept in order, at most
//! [`MAX_ERRORS`]; past that the oldest goes, counted and logged. The queue therefore holds at
//! most a few events per cluster, whatever bursts the manager sends.
//!
//! The reader owns the queue: [`Events`] (from [`crate::CoreHandle::events`]) keeps it open, a
//! [`crate::CoreHandle`] does not. Events sent before the first reader comes are kept (bounded as
//! above), so none from the start is lost; once the last reader is gone, nothing more is queued.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::cluster::ClusterId;
use crate::handle::{ConnectionState, Event};

/// The most [`Event::Error`]s queued; past that the oldest is dropped.
pub const MAX_ERRORS: usize = 64;

/// The phase of a connection state ([`Key::Connection`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Phase {
    Connecting,
    Settled,
}

/// What an event replaces when it is queued.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Clusters,
    Active,
    /// A cluster's connection, in two phases: an attempt starting, and where it settled. Kept
    /// apart, so a front end sees each attempt start even when it fails before being read (a
    /// refused server); the last of the two is still the current state.
    Connection(ClusterId, Phase),
    Progress,
    Settings,
    Discovery(ClusterId),
    Activity,
    KeepWatching,
    /// Errors are not replaced: each has its own number.
    Error(u64),
}

fn key(event: &Event, next_error: &mut u64) -> Key {
    match event {
        Event::ClustersChanged(_) => Key::Clusters,
        Event::ActiveChanged(_) => Key::Active,
        Event::ConnectionChanged { cluster, state } => {
            let phase = match state {
                ConnectionState::Connecting => Phase::Connecting,
                _ => Phase::Settled,
            };
            Key::Connection(cluster.clone(), phase)
        }
        Event::ConnectionProgress(_) => Key::Progress,
        Event::SettingsChanged { .. } => Key::Settings,
        Event::DiscoveryChanged { cluster, .. } => Key::Discovery(cluster.clone()),
        Event::AgentActivity(..) => Key::Activity,
        Event::KeepWatchingChanged(_) => Key::KeepWatching,
        Event::Error(_) => {
            *next_error += 1;
            Key::Error(*next_error)
        }
    }
}

#[derive(Default)]
struct Pending {
    /// Keys in the order their events go out.
    order: VecDeque<Key>,
    events: HashMap<Key, Event>,
    errors: usize,
    next_error: u64,
    /// Errors dropped since the queue was made.
    dropped_errors: u64,
    /// Readers alive, and whether there ever was one.
    readers: usize,
    had_reader: bool,
    /// The manager is gone: nothing more will come.
    closed: bool,
}

#[derive(Default)]
struct Queue {
    pending: Mutex<Pending>,
    ready: Notify,
}

impl Queue {
    fn pending(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A queue's sending half, which the manager holds. Dropping it ends the stream.
pub(crate) struct EventSender(Arc<Queue>);

/// The reading half: [`Events::recv`] gives the next event. Clones share one queue, each event
/// going to one of them. While one exists, the queue is open.
pub struct Events(Arc<Queue>);

/// The manager is gone and every event was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the core is not running")]
pub struct Closed;

/// Where readers of a queue come from, without being one: what a [`crate::CoreHandle`] holds.
#[derive(Clone)]
pub(crate) struct EventSource(Arc<Queue>);

impl EventSource {
    /// A reader of the queue.
    pub(crate) fn events(&self) -> Events {
        Events::new(self.0.clone())
    }
}

/// A new queue, with no reader yet.
pub(crate) fn queue() -> (EventSender, EventSource) {
    let queue = Arc::new(Queue::default());
    (EventSender(queue.clone()), EventSource(queue))
}

/// A new queue and a reader of it.
#[cfg(test)]
pub(crate) fn channel() -> (EventSender, Events) {
    let (sender, source) = queue();
    (sender, source.events())
}

impl EventSender {
    /// Queues `event`, replacing the queued one of its kind (and cluster). Never blocks. Does
    /// nothing once every reader is gone.
    pub(crate) fn send(&self, event: Event) {
        // The contexts a new list keeps, gathered before the lock that readers wait on.
        let kept: Option<std::collections::HashSet<ClusterId>> = match &event {
            Event::ClustersChanged(snapshot) => Some(
                snapshot
                    .clusters
                    .iter()
                    .map(|c| c.info.name.clone())
                    .collect(),
            ),
            _ => None,
        };
        let mut guard = self.0.pending();
        if guard.had_reader && guard.readers == 0 {
            return;
        }
        let pending = &mut *guard;
        let key = key(&event, &mut pending.next_error);
        let mut dropped_errors = None;
        if let Key::Error(_) = key {
            if pending.errors == MAX_ERRORS {
                if let Some(at) = pending
                    .order
                    .iter()
                    .position(|k| matches!(k, Key::Error(_)))
                {
                    let oldest = pending.order.remove(at).expect("found");
                    pending.events.remove(&oldest);
                    pending.errors -= 1;
                }
                pending.dropped_errors += 1;
                dropped_errors = Some(pending.dropped_errors);
            }
            pending.errors += 1;
        } else if pending.events.contains_key(&key) {
            pending.order.retain(|k| k != &key);
        }
        // A new attempt: the previous attempt's result, not read yet, no longer says anything.
        if let Key::Connection(context, Phase::Connecting) = &key {
            let settled = Key::Connection(context.clone(), Phase::Settled);
            if pending.events.remove(&settled).is_some() {
                pending.order.retain(|k| k != &settled);
            }
        }
        // A context the new list no longer has: its queued connection and discovery events go
        // (the reader forgets a context that left the list anyway), so contexts that come and go
        // under new names do not pile up.
        if let Some(kept) = &kept {
            let gone = |key: &Key| match key {
                Key::Connection(context, _) | Key::Discovery(context) => !kept.contains(context),
                _ => false,
            };
            pending.order.retain(|k| !gone(k));
            pending.events.retain(|k, _| !gone(k));
        }
        pending.order.push_back(key.clone());
        pending.events.insert(key, event);
        drop(guard);
        if let Some(dropped) = dropped_errors {
            tracing::warn!(dropped, "core errors not read in time; dropped the oldest");
        }
        self.0.ready.notify_waiters();
    }

    /// Events queued now.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.0.pending().order.len()
    }
}

impl Drop for EventSender {
    fn drop(&mut self) {
        self.0.pending().closed = true;
        self.0.ready.notify_waiters();
    }
}

impl Events {
    fn new(queue: Arc<Queue>) -> Self {
        {
            let mut pending = queue.pending();
            pending.readers += 1;
            pending.had_reader = true;
        }
        Events(queue)
    }

    /// The next event; [`Closed`] once the manager is gone and none is left. Cancel-safe.
    pub async fn recv(&self) -> Result<Event, Closed> {
        loop {
            let ready = self.0.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            {
                let mut pending = self.0.pending();
                if let Some(key) = pending.order.pop_front() {
                    if let Key::Error(_) = key {
                        pending.errors -= 1;
                    }
                    let event = pending.events.remove(&key).expect("queued with its key");
                    return Ok(event);
                }
                if pending.closed {
                    return Err(Closed);
                }
            }
            ready.await;
        }
    }

    /// Events queued now.
    pub fn len(&self) -> usize {
        self.0.pending().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Clone for Events {
    fn clone(&self) -> Self {
        Events::new(self.0.clone())
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        let mut pending = self.0.pending();
        pending.readers -= 1;
        if pending.readers == 0 {
            // Nobody will read them: let go of what they hold.
            pending.order.clear();
            pending.events.clear();
            pending.errors = 0;
        }
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
