//! The watch manager (ADR-0008): which kinds of a cluster are watched, given the watch mode,
//! whether the cluster wants watches (it is active or a member of the running set), and what the
//! front end has open.
//!
//! - **All**: while the cluster wants watches, every listable and watchable kind is watched.
//! - **On view**: only kinds with an open subscription are watched.
//! - In both modes a subscribed kind is watched (or polled, for APIs that cannot watch), and
//!   nothing is watched while the cluster does not want watches. Stopping a kind drops its rows, so
//!   coming back starts from a fresh list. A view closed on the active cluster during an outage
//!   keeps its last rows until the cluster lists again or goes cold, so reopening it shows them
//!   as stale rather than an empty list. A background member never retains rows this way.

mod engine;
mod hub;
mod source;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use crate::cluster::ClusterId;
use crate::read::{KindId, KindInfo};
use crate::settings::WatchMode;
use crate::store::{Feed, KindSnapshot, SyncState};

pub(crate) use engine::EngineConfig;
pub(crate) use hub::{Hub, HubRegistry};
#[cfg(test)]
pub(crate) use source::WatchRequest;
pub(crate) use source::{KubeSource, Source};

/// Initial lists running at once per cluster. Activation in All mode starts dozens of kinds;
/// this keeps the burst polite to the API server (and to exec auth plugins).
const CONCURRENT_LISTS: usize = 8;

/// Kinds left out of All mode because another kind shows the same objects.
const NOT_IN_ALL_MODE: &[(&str, &str)] = &[("events.k8s.io", "events")];

/// Whether All mode watches this kind.
pub(crate) fn in_all_mode(kind: &KindInfo) -> bool {
    kind.is_listable()
        && kind.is_watchable()
        && !NOT_IN_ALL_MODE
            .iter()
            .any(|(g, p)| kind.group() == *g && kind.plural() == *p)
}

struct Running {
    task: JoinHandle<()>,
    feed: Feed,
    kind: KindInfo,
    hub: Arc<Hub>,
}

/// The watches of one cluster. Owned by the cluster manager; all methods are called from its
/// task, so it needs no locks of its own.
pub(crate) struct ClusterWatches {
    cluster: ClusterId,
    hubs: HubRegistry,
    config: Arc<EngineConfig>,
    limiter: Arc<Semaphore>,
    source: Option<Arc<dyn Source>>,
    /// From discovery; `None` until it ran.
    kinds: Option<HashMap<KindId, KindInfo>>,
    running: HashMap<KindId, Running>,
    /// Kinds watched whenever the cluster wants watches, in both modes (the CRD list, which keeps
    /// discovery fresh).
    always: HashSet<KindId>,
    /// The namespaces a kind the user may not list in all namespaces is listed in, one by one
    /// (see [`ClusterWatches::set_scope`]).
    scope: watch::Sender<Arc<[String]>>,
    /// Watching as a member while another cluster is active: closed views keep no rows here, so
    /// the outage cache stays bounded to the active cluster.
    background: bool,
}

impl ClusterWatches {
    pub fn new(cluster: ClusterId, hubs: HubRegistry, config: Arc<EngineConfig>) -> Self {
        ClusterWatches {
            cluster,
            hubs,
            config,
            limiter: Arc::new(Semaphore::new(CONCURRENT_LISTS)),
            source: None,
            kinds: None,
            running: HashMap::new(),
            always: HashSet::from([KindId::custom_resource_definitions()]),
            scope: watch::Sender::new(Arc::from(Vec::new())),
            background: false,
        }
    }

    /// Says whether another cluster is active; call before [`ClusterWatches::reconcile`] or
    /// [`ClusterWatches::stop_unwanted`], which drop any rows a background cluster still keeps.
    pub fn set_background(&mut self, background: bool) {
        self.background = background;
    }

    /// Sets the namespaces to list kinds in when the user may not list them in all namespaces.
    /// Running namespace-limited lists follow the change at once.
    pub fn set_scope(&mut self, namespaces: Vec<String>) {
        let namespaces: Arc<[String]> = namespaces.into();
        self.scope.send_if_modified(|current| {
            if **current == *namespaces {
                false
            } else {
                *current = namespaces;
                true
            }
        });
    }

    #[cfg(test)]
    pub fn scope(&self) -> Arc<[String]> {
        self.scope.borrow().clone()
    }

    /// Sets the connection (or clears it); call [`ClusterWatches::reconcile`] after.
    pub fn set_source(&mut self, source: Option<Arc<dyn Source>>) {
        self.stop_all();
        // Rows from the previous connection say nothing about this one.
        self.hubs.forget_outage(&self.cluster);
        self.source = source;
    }

    /// Sets discovery results; call [`ClusterWatches::reconcile`] after.
    pub fn set_kinds(&mut self, kinds: &[KindInfo]) {
        let next: HashMap<KindId, KindInfo> = kinds.iter().map(|k| (k.id(), k.clone())).collect();
        // Rows kept through an outage belong to the resource they were listed from.
        for (id, old) in self.kinds.iter().flatten() {
            if next.get(id).is_none_or(|k| k.resource != old.resource) {
                self.hubs.forget_kept(&self.cluster, id);
            }
        }
        self.kinds = Some(next);
    }

    pub fn kind(&self, id: &KindId) -> Option<&KindInfo> {
        self.kinds.as_ref()?.get(id)
    }

    /// What should run, by kind.
    fn wanted(
        &self,
        watching: bool,
        mode: WatchMode,
        subscribed: &HashSet<KindId>,
    ) -> HashMap<KindId, Feed> {
        let mut wanted = HashMap::new();
        let (true, Some(_), Some(kinds)) = (watching, &self.source, &self.kinds) else {
            return wanted;
        };
        for (id, kind) in kinds {
            if !kind.is_listable() {
                continue;
            }
            let open = subscribed.contains(id) || self.always.contains(id);
            if kind.is_watchable() {
                if open || (mode == WatchMode::All && in_all_mode(kind)) {
                    wanted.insert(id.clone(), Feed::Watch);
                }
            } else if open {
                wanted.insert(id.clone(), Feed::Poll);
            }
        }
        wanted
    }

    /// Starts and stops watches to match the inputs.
    pub fn reconcile(&mut self, watching: bool, mode: WatchMode, subscribed: &HashSet<KindId>) {
        let wanted = self.stop_unwanted(watching, mode, subscribed);
        let mut starting: Vec<(&KindId, &Feed)> = wanted
            .iter()
            .filter(|(id, _)| !self.running.contains_key(*id))
            .collect();
        // Open views first: they take the list slots before the All-mode bulk.
        starting.sort_by_key(|(id, _)| !subscribed.contains(*id));
        let starting: Vec<(KindId, Feed)> =
            starting.into_iter().map(|(i, f)| (i.clone(), *f)).collect();
        for (id, feed) in starting {
            self.start(&id, feed);
        }
        // Tell open views about kinds this cluster does not serve.
        if watching && let Some(kinds) = &self.kinds {
            for id in subscribed.iter().filter(|id| !kinds.contains_key(*id)) {
                let hub = self.hubs.get_or_create(&self.cluster, id);
                if hub.latest().state != SyncState::Unavailable {
                    hub.take_over(KindSnapshot::empty(
                        self.cluster.clone(),
                        id.clone(),
                        SyncState::Unavailable,
                    ));
                }
            }
        }
    }

    /// Stops what the inputs no longer want and starts nothing. Returns what they want.
    pub fn stop_unwanted(
        &mut self,
        watching: bool,
        mode: WatchMode,
        subscribed: &HashSet<KindId>,
    ) -> HashMap<KindId, Feed> {
        let wanted = self.wanted(watching, mode, subscribed);
        let stale: Vec<KindId> = self
            .running
            .iter()
            .filter(|(id, r)| {
                wanted.get(*id) != Some(&r.feed)
                    || self.kind(id).is_none_or(|k| k.resource != r.kind.resource)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.stop(&id);
        }
        if !watching || self.background {
            self.hubs.release_kept_rows(&self.cluster);
        }
        wanted
    }

    fn start(&mut self, id: &KindId, feed: Feed) {
        let (Some(source), Some(kind)) = (self.source.clone(), self.kind(id).cloned()) else {
            return;
        };
        let hub = self.hubs.get_or_create(&self.cluster, id);
        let resume = self.hubs.resume(&self.cluster, id);
        let mut first = match &resume {
            Some(hub::Resume {
                state,
                rows: Some(rows),
            }) => KindSnapshot {
                state: state.clone(),
                ..(**rows).clone()
            },
            Some(resume) => {
                KindSnapshot::empty(self.cluster.clone(), id.clone(), resume.state.clone())
            }
            None => KindSnapshot::empty(self.cluster.clone(), id.clone(), SyncState::Loading),
        };
        first.namespaced = kind.namespaced;
        first.feed = feed;
        let epoch = hub.take_over(first);
        let engine = engine::Engine::new(
            self.cluster.clone(),
            kind.clone(),
            source,
            hub.clone(),
            epoch,
            self.config.clone(),
            self.limiter.clone(),
        )
        .with_scope(Some(self.scope.subscribe()))
        .resuming(resume);
        let task = tokio::spawn(engine.run(feed));
        self.running.insert(
            id.clone(),
            Running {
                task,
                feed,
                kind,
                hub,
            },
        );
    }

    fn stop(&mut self, id: &KindId) {
        let Some(running) = self.running.remove(id) else {
            return;
        };
        running.task.abort();
        // A kind whose resource changed would show the old one's rows. A background cluster keeps
        // the error too: `stop_unwanted` then drops its rows.
        if self
            .kind(id)
            .is_some_and(|k| k.resource == running.kind.resource)
        {
            self.hubs.keep_for_outage(&self.cluster, id, &running.hub);
        }
        let state = if self.kind(id).is_some() {
            SyncState::Idle
        } else {
            SyncState::Unavailable
        };
        let mut idle = KindSnapshot::empty(self.cluster.clone(), id.clone(), state);
        idle.namespaced = running.kind.namespaced;
        running.hub.take_over(idle);
        self.hubs.release(&self.cluster, id);
    }

    pub fn stop_all(&mut self) {
        let ids: Vec<KindId> = self.running.keys().cloned().collect();
        for id in ids {
            self.stop(&id);
        }
    }

    /// Kinds with a running task, and how they are fed right now: a kind whose watch was
    /// refused is polled even though a watch was asked for.
    pub fn running(&self) -> HashMap<KindId, Feed> {
        self.running
            .iter()
            .map(|(id, r)| (id.clone(), r.hub.feed()))
            .collect()
    }

    /// Whether a task runs for `kind`.
    pub fn is_running(&self, kind: &KindId) -> bool {
        self.running.contains_key(kind)
    }
}

impl Drop for ClusterWatches {
    fn drop(&mut self) {
        self.stop_all();
        self.hubs.forget_outage(&self.cluster);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cluster_tests;
