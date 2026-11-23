//! Feeds for clusters the person chose to keep watching (ADR-0012).
//!
//! Each runs on a connection of its own with only a read client, separate from the cluster's slot:
//! the slot's role, its write credential and the write gate never see it. Its credential access
//! allows the background role only, and ending the access kills a plugin that is running.
//!
//! [`ClusterManager::sync_keep_watching`] compares what should run with what does after every
//! message of the manager's loop, so no path that changes a definition has to remember it.

// module-tests: ../manager.rs (and flint-e2e's telemetry.rs, on the fake API)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::AbortHandle;

use super::telemetry::reads_token_file;
use super::{ClusterManager, Internal};
use crate::ClusterId;
use crate::cluster;
use crate::connection::policy::CredentialAccess;
use crate::handle::Event;
use crate::keep_watching::{
    KeepWatchView, KeepWatching, MAX_WATCHED_CLUSTERS, PluginConsent, SignIn, WatchedCluster,
    sign_in_of, too_many_watched,
};
use crate::kubeconfig::LoadedKubeconfig;
use crate::metrics_trend::TrendStore;
use crate::telemetry::Telemetry;

/// How long a credential that runs nothing waits before connecting again.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How many background connections are made at once, so a launch starts no burst of plugins.
const CONCURRENT_CONNECTS: usize = 4;

/// One watched cluster's connection and feed. Dropping it ends both, and kills its plugin.
struct KeptFeed {
    attempt: u64,
    fingerprint: Option<u64>,
    runs_plugin: bool,
    access: CredentialAccess,
    connecting: Option<AbortHandle>,
    feed: Option<Telemetry>,
}

impl Drop for KeptFeed {
    fn drop(&mut self) {
        self.access.revoke();
        if let Some(task) = self.connecting.take() {
            task.abort();
        }
    }
}

/// What the manager keeps for watched clusters.
pub(super) struct KeepWatch {
    feeds: HashMap<ClusterId, KeptFeed>,
    /// Stopped after a failure, until the person acts.
    stopped: HashMap<ClusterId, Option<u64>>,
    /// Consent a confirmed change broke. It stays void even if the change is undone later.
    voided: HashSet<ClusterId>,
    /// Of those, the ones whose saved consent is still to be removed.
    to_clear: Vec<ClusterId>,
    retry_at: HashMap<ClusterId, Instant>,
    next_attempt: u64,
    connects: Arc<Semaphore>,
    shown: BTreeMap<ClusterId, KeepWatchView>,
    /// How each context signs in, for the load it was read from: this is asked on every message
    /// of the manager's loop, and working it out serializes the context's entries.
    sign_ins: Option<(Arc<LoadedKubeconfig>, SignIns)>,
}

/// How each context signs in.
type SignIns = HashMap<ClusterId, Arc<SignIn>>;

impl Default for KeepWatch {
    fn default() -> Self {
        KeepWatch {
            feeds: HashMap::new(),
            stopped: HashMap::new(),
            voided: HashSet::new(),
            to_clear: Vec::new(),
            retry_at: HashMap::new(),
            next_attempt: 1,
            connects: Arc::new(Semaphore::new(CONCURRENT_CONNECTS)),
            shown: BTreeMap::new(),
            sign_ins: None,
        }
    }
}

impl KeepWatch {
    /// Whether `id`'s board entry belongs to a feed here, or to its last values.
    pub(super) fn holds_board_entry(&self, id: &str) -> bool {
        self.stopped.contains_key(id) || self.feeds.get(id).is_some_and(|kept| kept.feed.is_some())
    }
}

impl ClusterManager {
    /// How `id` signs in on the current load. An answer read from an older load is never used.
    fn sign_in(&self, id: &str) -> Arc<SignIn> {
        self.keep
            .sign_ins
            .as_ref()
            .filter(|(load, _)| Arc::ptr_eq(load, &self.kubeconfig))
            .and_then(|(_, known)| known.get(id).cloned())
            .unwrap_or_else(|| Arc::new(sign_in_of(&self.kubeconfig.merged, id)))
    }

    /// Reads every context's sign-in once per load.
    fn refresh_sign_ins(&mut self) {
        let current = self
            .keep
            .sign_ins
            .as_ref()
            .is_some_and(|(load, _)| Arc::ptr_eq(load, &self.kubeconfig));
        if current {
            return;
        }
        let known = self
            .clusters
            .iter()
            .map(|slot| {
                let id = slot.info.name.clone();
                let sign_in = Arc::new(sign_in_of(&self.kubeconfig.merged, &id));
                (id, sign_in)
            })
            .collect();
        self.keep.sign_ins = Some((Arc::clone(&self.kubeconfig), known));
    }

    /// The toggle's state for `id` now.
    pub(super) fn keep_status(&self, id: &str) -> KeepWatching {
        let Some(entry) = self.settings.watched_cluster(id) else {
            return KeepWatching::Off;
        };
        let Some(slot) = self.cluster(id) else {
            // Missing from a load that could not read every file says nothing about it.
            return if self.contexts.removed(id) {
                KeepWatching::NeedsConfirming
            } else {
                KeepWatching::Paused
            };
        };
        let shown = slot.info.server.as_deref();
        if self.paused(id) || self.unsettled(id) || !self.contexts.shown(id, shown).confirmed {
            return KeepWatching::Paused;
        }
        if self.keep.voided.contains(id) || shown != Some(entry.server.as_str()) {
            return KeepWatching::NeedsConfirming;
        }
        let agreed = match &*self.sign_in(id) {
            SignIn::Static => entry.plugin_consent.is_none(),
            SignIn::Plugin(definition) => entry
                .plugin_consent
                .as_ref()
                .is_some_and(|consent| consent.holds(definition)),
            SignIn::Provider | SignIn::Unknown => false,
        };
        if !agreed {
            KeepWatching::NeedsConfirming
        } else if self.keep.stopped.contains_key(id) {
            KeepWatching::SignInNeeded
        } else {
            KeepWatching::Watching
        }
    }

    /// Whether `id` is watched and may be read now. It overrides On view.
    pub(super) fn keep_watching(&self, id: &str) -> bool {
        self.keep_status(id) == KeepWatching::Watching
    }

    /// Whether `id` needs a feed of its own: its slot has none and can't have one.
    fn wants_kept_feed(&self, id: &str) -> bool {
        self.active.as_deref() != Some(id) && self.keep_watching(id) && !self.wants_telemetry(id)
    }

    /// Stops the feeds no longer wanted, starts those missing, and reports any change.
    pub(super) fn sync_keep_watching(&mut self) {
        self.refresh_sign_ins();
        self.void_broken_consents();
        let running: Vec<ClusterId> = self.keep.feeds.keys().cloned().collect();
        for id in running {
            let current = self.cluster(&id).and_then(|slot| slot.fingerprint);
            let same = self
                .keep
                .feeds
                .get(&id)
                .is_some_and(|kept| kept.fingerprint == current);
            if !same || !self.wants_kept_feed(&id) {
                self.keep.feeds.remove(&id);
            }
        }
        // Over the cap, a file edited by hand starts only the first ones.
        let watched: Vec<ClusterId> = self
            .settings
            .watched_clusters
            .iter()
            .take(MAX_WATCHED_CLUSTERS)
            .map(|entry| entry.context.clone())
            .collect();
        self.keep
            .stopped
            .retain(|id, _| watched.iter().any(|name| name == id));
        let now = Instant::now();
        for id in &watched {
            let due = self.keep.retry_at.get(id).is_none_or(|at| now >= *at);
            if due && !self.keep.feeds.contains_key(id) && self.wants_kept_feed(id) {
                self.keep.retry_at.remove(id);
                self.start_kept(id);
            }
        }
        self.publish_keep_watching();
    }

    /// Records consent that a confirmed definition no longer matches. A paused context is not
    /// judged: what it shows may only be a fallback.
    fn void_broken_consents(&mut self) {
        let broken: Vec<ClusterId> = self
            .settings
            .watched_clusters
            .iter()
            .filter(|entry| entry.plugin_consent.is_some())
            .map(|entry| entry.context.clone())
            .filter(|id| {
                !self.keep.voided.contains(id)
                    && self.keep_status(id) == KeepWatching::NeedsConfirming
            })
            .collect();
        for id in broken {
            self.keep.voided.insert(id.clone());
            self.keep.to_clear.push(id);
        }
    }

    /// Removes voided consent from `settings.toml`, so it stays void across a restart.
    pub(super) async fn clear_voided_consents(&mut self) {
        for id in std::mem::take(&mut self.keep.to_clear) {
            let Some(entry) = self.settings.watched_cluster(&id).cloned() else {
                continue;
            };
            let entry = WatchedCluster {
                plugin_consent: None,
                ..entry
            };
            self.write_settings("keep watching", move |dir| {
                crate::settings::set_watched_cluster(dir, &id, Some(entry))
            })
            .await;
        }
    }

    fn start_kept(&mut self, id: &str) {
        let attempt = self.keep.next_attempt;
        self.keep.next_attempt += 1;
        let runs_plugin = matches!(&*self.sign_in(id), SignIn::Plugin(_));
        let failed = self.internal_tx.clone();
        let cluster_name = id.to_owned();
        let access = CredentialAccess::consented(Arc::new(move || {
            let _ = failed.send(Internal::KeepWatchFailed {
                cluster: cluster_name.clone(),
                attempt,
            });
        }));
        let mut kubeconfig = self.kubeconfig.merged.clone();
        cluster::disable_interactive_exec(&mut kubeconfig);
        let (name, task_access) = (id.to_owned(), access.clone());
        let connects = Arc::clone(&self.keep.connects);
        let internal = self.internal_tx.clone();
        let task = tokio::spawn(async move {
            let Ok(_turn) = connects.acquire_owned().await else {
                return;
            };
            let result = cluster::connect_read_only_with_policy(
                &kubeconfig,
                &name,
                cluster::CONNECT_TIMEOUT,
                &task_access,
            )
            .await
            .map_err(|error| super::running_set::needs_sign_in(&error));
            let _ = internal.send(Internal::KeepWatchConnected {
                cluster: name,
                attempt,
                result,
            });
        });
        let fingerprint = self.cluster(id).and_then(|slot| slot.fingerprint);
        self.keep.feeds.insert(
            id.to_owned(),
            KeptFeed {
                attempt,
                fingerprint,
                runs_plugin,
                access,
                connecting: Some(task.abort_handle()),
                feed: None,
            },
        );
    }

    /// A background connection finished. A plugin's cluster never retries by itself.
    pub(super) fn kept_connected(
        &mut self,
        id: &str,
        attempt: u64,
        result: Result<kube::Client, bool>,
    ) {
        let Some(kept) = self.keep.feeds.get_mut(id) else {
            return;
        };
        if kept.attempt != attempt {
            return;
        }
        kept.connecting = None;
        match result {
            Ok(client) => {
                let target = cluster::Target::of(&self.kubeconfig.merged, id);
                let requests = self
                    .telemetry
                    .request_gate(reads_token_file(&self.kubeconfig.merged, id));
                let trends = Arc::new(Mutex::new(TrendStore::new()));
                let mut feed = Telemetry::start(
                    id.into(),
                    client,
                    target,
                    Arc::clone(&trends),
                    self.telemetry_timing,
                    requests,
                );
                self.telemetry.publish(id, &mut feed, trends);
                kept.feed = Some(feed);
            }
            Err(sign_in) if sign_in || kept.runs_plugin => self.stop_kept(id),
            Err(_) => {
                self.keep.feeds.remove(id);
                self.keep
                    .retry_at
                    .insert(id.to_owned(), Instant::now() + RETRY_AFTER);
                let internal = self.internal_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(RETRY_AFTER).await;
                    let _ = internal.send(Internal::KeepWatchRetry);
                });
            }
        }
    }

    /// The credential of attempt `attempt` failed or was refused.
    pub(super) fn kept_failed(&mut self, id: &str, attempt: u64) {
        if self
            .keep
            .feeds
            .get(id)
            .is_some_and(|kept| kept.attempt == attempt)
        {
            self.stop_kept(id);
        }
    }

    /// Stops `id` until the person acts, keeping its last values in memory, marked with their age.
    fn stop_kept(&mut self, id: &str) {
        self.telemetry.hold(id);
        let fingerprint = self.cluster(id).and_then(|slot| slot.fingerprint);
        self.keep.stopped.insert(id.to_owned(), fingerprint);
        self.keep.feeds.remove(id);
    }

    /// The person selected `id`, which is acting on a stopped watch.
    pub(super) fn keep_watching_acted_on(&mut self, id: &str) {
        self.keep.stopped.remove(id);
        self.keep.retry_at.remove(id);
    }

    /// Turns watching off (`None`), or on for the exact definition the confirmation showed.
    pub(super) async fn set_keep_watching(&mut self, id: ClusterId, shown: Option<u64>) {
        let entry = match shown {
            None => None,
            Some(shown) => match self.watch_entry(&id, shown) {
                Ok(entry) => Some(entry),
                Err(why) => {
                    self.emit(Event::Error(why));
                    return;
                }
            },
        };
        self.keep_watching_acted_on(&id);
        self.keep.voided.remove(&id);
        self.keep.to_clear.retain(|pending| *pending != id);
        self.write_settings("keep watching", move |dir| {
            crate::settings::set_watched_cluster(dir, &id, entry)
        })
        .await;
    }

    /// The entry for watching `id`, if the definition is still the one confirmed.
    fn watch_entry(&self, id: &str, shown: u64) -> Result<WatchedCluster, String> {
        let Some(slot) = self.cluster(id) else {
            return Err(format!("cluster \"{id}\" no longer exists"));
        };
        if slot.fingerprint != Some(shown) {
            return Err(format!(
                "\"{id}\" changed since this was confirmed. Confirm it again."
            ));
        }
        if let Some(why) = self.keep_refusal(id) {
            return Err(why);
        }
        let Some(server) = slot.info.server.clone() else {
            return Err(format!("\"{id}\" names no server."));
        };
        let plugin_consent = match &*self.sign_in(id) {
            SignIn::Plugin(definition) => Some(
                PluginConsent::seal(definition)
                    .ok_or_else(|| "Flint could not record this confirmation.".to_owned())?,
            ),
            _ => None,
        };
        Ok(WatchedCluster {
            context: id.to_owned(),
            server,
            plugin_consent,
        })
    }

    /// Why `id` cannot be turned on now, if so.
    fn keep_refusal(&self, id: &str) -> Option<String> {
        let slot = self.cluster(id)?;
        if slot.info.server.is_none() {
            return Some(format!("\"{id}\" names no server."));
        }
        if self.paused(id)
            || self.unsettled(id)
            || !self
                .contexts
                .shown(id, slot.info.server.as_deref())
                .confirmed
        {
            return Some(format!(
                "\"{id}\" can't be watched while a kubeconfig file defining it is unreadable."
            ));
        }
        match &*self.sign_in(id) {
            SignIn::Provider => {
                return Some(format!(
                    "\"{id}\" signs in through an auth provider, which Flint runs only for the \
                     selected cluster."
                ));
            }
            SignIn::Unknown => {
                return Some(format!("\"{id}\" has a user Flint can't read."));
            }
            SignIn::Static | SignIn::Plugin(_) => {}
        }
        let others = self
            .settings
            .watched_clusters
            .iter()
            .filter(|entry| entry.context != id)
            .count();
        (others >= MAX_WATCHED_CLUSTERS).then(too_many_watched)
    }

    /// Every cluster's toggle as it stands.
    pub(super) fn keep_watching_views(&self) -> BTreeMap<ClusterId, KeepWatchView> {
        self.clusters
            .iter()
            .map(|slot| {
                let id = slot.info.name.clone();
                let command = match &*self.sign_in(&id) {
                    SignIn::Plugin(definition) => Some(definition.command.clone()),
                    _ => None,
                };
                let view = KeepWatchView {
                    status: self.keep_status(&id),
                    command,
                    definition: slot.fingerprint,
                    refusal: self.keep_refusal(&id),
                };
                (id, view)
            })
            .collect()
    }

    fn publish_keep_watching(&mut self) {
        let views = self.keep_watching_views();
        if views != self.keep.shown {
            self.keep.shown = views.clone();
            self.emit(Event::KeepWatchingChanged(Arc::new(views)));
        }
    }
}
