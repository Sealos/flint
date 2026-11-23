//! Feeds for clusters the person chose to keep watching (ADR-0012).
//!
//! Each runs on a connection of its own with only a read client, separate from the cluster's slot:
//! the slot's role, its write credential and the write gate never see it. Its credential access
//! allows the background role only, and ending the access kills a plugin that is running.
//!
//! [`ClusterManager::sync_keep_watching`] compares what should run with what does after every
//! message of the manager's loop, so no path that changes a definition has to remember it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::AbortHandle;

use super::telemetry::reads_token_file;
use super::{ClusterManager, Internal};
use crate::ClusterId;
use crate::cluster;
use crate::connection::policy::{CredentialAccess, PluginRuns, Role};
use crate::handle::Event;
use crate::keep_watching::{
    BackgroundSignIn, KeepWatchView, KeepWatching, MAX_WATCHED_CLUSTERS, OfferTicket,
    PluginApproval, PluginConsent, PluginOffer, SignIn, ViewKey, WatchedCluster, sign_in_of,
    too_many_watched,
};
use crate::kubeconfig::LoadedKubeconfig;
use crate::metrics_trend::TrendStore;
use crate::telemetry::Telemetry;

/// How long a connection that failed for a reason worth retrying waits before the next.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How many background connections are made at once, so a launch starts no burst of plugins.
const CONCURRENT_CONNECTS: usize = 4;
/// How long a quit waits for plugins to be killed: inside GPUI's 200 ms for quit hooks.
const QUIT_WAIT: Duration = Duration::from_millis(100);

/// An offer a confirmation is showing, until it is approved or replaced.
struct PendingOffer {
    ticket: OfferTicket,
    consent: PluginConsent,
    fingerprint: Option<u64>,
    server: String,
}

/// One watched cluster's connection and feed. Dropping it ends both, and kills its plugin.
struct KeptFeed {
    attempt: u64,
    fingerprint: Option<u64>,
    runs_plugin: bool,
    /// A detected file change ended its plugin: a failed connection says nothing of the sign-in.
    interrupted: bool,
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
    /// Agreement to run a plugin, for the running app only. Nothing restores an entry removed
    /// from here but a new approval.
    consents: HashMap<ClusterId, Arc<PluginConsent>>,
    /// The newest offer of each cluster.
    offers: HashMap<ClusterId, PendingOffer>,
    /// Names definitions in views. Without one, no toggle can be turned on.
    view_key: Option<ViewKey>,
    /// Consent a confirmed change broke. It stays void even if the change is undone later.
    voided: HashSet<ClusterId>,
    /// Stopped by the person while the settings still list them, because the write failed.
    turned_off: HashSet<ClusterId>,
    /// The app is quitting: nothing starts any more.
    quitting: bool,
    plugin_runs: PluginRuns,
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
            consents: HashMap::new(),
            offers: HashMap::new(),
            view_key: ViewKey::new(),
            voided: HashSet::new(),
            turned_off: HashSet::new(),
            quitting: false,
            plugin_runs: PluginRuns::default(),
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
        if self.keep.turned_off.contains(id) {
            return KeepWatching::Off;
        }
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
            SignIn::Static => !self.keep.consents.contains_key(id),
            SignIn::Plugin(definition) => self
                .keep
                .consents
                .get(id)
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
        !self.keep.quitting
            && self.active.as_deref() != Some(id)
            && self.keep_watching(id)
            && !self.wants_telemetry(id)
    }

    /// Whether no change of the kubeconfig files is waiting to be read.
    fn definitions_settled(&self) -> bool {
        !self.change_queued() && !self.reader.busy(&self.kubeconfig_paths)
    }

    /// A kubeconfig file changed: no kept plugin runs until the new definitions are read, and one
    /// running now is killed. [`Self::sync_keep_watching`] lets those still agreed to go on.
    pub(super) fn suspend_kept_plugins(&mut self) {
        for kept in self.keep.feeds.values_mut() {
            if kept.runs_plugin {
                kept.interrupted = true;
                kept.access.set_role(Role::Paused);
            }
        }
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
        let settled = self.definitions_settled();
        if settled {
            for kept in self.keep.feeds.values().filter(|kept| kept.runs_plugin) {
                kept.access.set_role(Role::Background);
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
            if settled && due && !self.keep.feeds.contains_key(id) && self.wants_kept_feed(id) {
                self.keep.retry_at.remove(id);
                self.start_kept(id);
            }
        }
        self.publish_keep_watching();
    }

    /// Removes consent a confirmed definition no longer matches, or whose entry is gone, for
    /// good. A paused context is not judged: what it shows may only be a fallback.
    fn void_broken_consents(&mut self) {
        let settings = &self.settings;
        self.keep
            .consents
            .retain(|id, _| settings.watched_cluster(id).is_some());
        self.keep
            .turned_off
            .retain(|id| settings.watched_cluster(id).is_some());
        let clusters = &self.clusters;
        self.keep
            .offers
            .retain(|id, _| clusters.iter().any(|slot| slot.info.name == *id));
        let broken: Vec<ClusterId> = self
            .keep
            .consents
            .keys()
            .filter(|id| self.keep_status(id) == KeepWatching::NeedsConfirming)
            .cloned()
            .collect();
        for id in broken {
            self.keep.consents.remove(&id);
            self.keep.offers.remove(&id);
            self.keep.voided.insert(id);
        }
    }

    /// Asked right before each start of `id`'s plugin: the files as they are now must still
    /// define what was agreed to, whether or not a change has been noticed yet.
    fn launch_check(&self, id: &str, consent: Arc<PluginConsent>) -> cluster::SendCheck {
        let paths = self.kubeconfig_paths.clone();
        let context = id.to_owned();
        cluster::SendCheck::new(move || {
            let (paths, context, consent) = (paths.clone(), context.clone(), consent.clone());
            Box::pin(async move {
                let same = tokio::task::spawn_blocking(move || {
                    let loaded = crate::kubeconfig::load(&paths);
                    loaded.errors.is_empty()
                        && matches!(
                            &sign_in_of(&loaded.merged, &context),
                            SignIn::Plugin(definition) if consent.holds(definition)
                        )
                })
                .await
                .unwrap_or(false);
                if same {
                    Ok(())
                } else {
                    Err("its definition is no longer the one confirmed".to_owned())
                }
            })
        })
    }

    fn start_kept(&mut self, id: &str) {
        let attempt = self.keep.next_attempt;
        self.keep.next_attempt += 1;
        let runs_plugin = matches!(&*self.sign_in(id), SignIn::Plugin(_));
        let check = self
            .keep
            .consents
            .get(id)
            .filter(|_| runs_plugin)
            .map(|consent| self.launch_check(id, Arc::clone(consent)));
        let failed = self.internal_tx.clone();
        let cluster_name = id.to_owned();
        let access = CredentialAccess::consented(
            Arc::new(move || {
                let _ = failed.send(Internal::KeepWatchFailed {
                    cluster: cluster_name.clone(),
                    attempt,
                });
            }),
            check,
            self.keep.plugin_runs.clone(),
        );
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
                interrupted: false,
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
            Err(sign_in) if !kept.interrupted && (sign_in || kept.runs_plugin) => {
                self.stop_kept(id);
            }
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

    /// Turns watching off (`None`), at once and whether or not the settings can be written, or on
    /// for a credential that runs nothing, on the definition `shown`.
    pub(super) async fn set_keep_watching(&mut self, id: ClusterId, shown: Option<u64>) {
        let Some(shown) = shown else {
            self.keep.consents.remove(&id);
            self.keep.offers.remove(&id);
            self.keep.feeds.remove(&id);
            self.keep_watching_acted_on(&id);
            self.keep.voided.remove(&id);
            self.keep.turned_off.insert(id.clone());
            let name = id.clone();
            self.write_settings("keep watching", move |dir| {
                crate::settings::set_watched_cluster(dir, &name, None)
            })
            .await;
            if self.settings.watched_cluster(&id).is_none() {
                self.keep.turned_off.remove(&id);
            }
            return;
        };
        let entry = match self.watch_entry(&id, shown) {
            Ok(entry) => entry,
            Err(why) => {
                self.emit(Event::Error(why));
                return;
            }
        };
        self.save_watched(id, entry, None).await;
    }

    /// Writes `id`'s entry, and holds `consent` only if the entry is there afterwards.
    async fn save_watched(
        &mut self,
        id: ClusterId,
        entry: WatchedCluster,
        consent: Option<PluginConsent>,
    ) {
        self.keep_watching_acted_on(&id);
        self.keep.voided.remove(&id);
        self.keep.turned_off.remove(&id);
        let server = entry.server.clone();
        let name = id.clone();
        self.write_settings("keep watching", move |dir| {
            crate::settings::set_watched_cluster(dir, &name, Some(entry))
        })
        .await;
        let saved = self
            .settings
            .watched_cluster(&id)
            .is_some_and(|entry| entry.server == server);
        if let (true, Some(consent)) = (saved, consent) {
            self.keep.consents.insert(id, Arc::new(consent));
        }
    }

    /// Whether `id` is on the list of clusters to keep watching and was not turned off.
    fn is_kept(&self, id: &str) -> bool {
        self.settings.watched_cluster(id).is_some() && !self.keep.turned_off.contains(id)
    }

    /// The background sign-in toggle was turned off: no new launch, and the watch stops.
    pub(super) fn revoke_background_sign_in(&mut self, id: &str) {
        self.keep.consents.remove(id);
        self.keep.offers.remove(id);
        self.keep.feeds.remove(id);
        self.keep.stopped.remove(id);
        self.keep.retry_at.remove(id);
    }

    /// An offer to turn on `id`'s background sign-in, if `shown` still names its definition.
    /// Replaces any earlier offer. A refused one leaves nothing that could be approved later.
    pub(super) fn offer_keep_watching(
        &mut self,
        id: &str,
        shown: u64,
    ) -> Result<PluginOffer, String> {
        self.keep.offers.remove(id);
        let Some(slot) = self.cluster(id) else {
            return Err(format!("cluster \"{id}\" no longer exists"));
        };
        let fingerprint = slot.fingerprint;
        let server = slot.info.server.clone();
        if let Some(why) = self.keep_refusal(id) {
            return Err(why);
        }
        if !self.is_kept(id) {
            return Err(format!("Turn on Keep Watching for \"{id}\" first."));
        }
        let Some(server) = server else {
            return Err(format!("\"{id}\" names no server."));
        };
        let sign_in = self.sign_in(id);
        let SignIn::Plugin(definition) = &*sign_in else {
            return Err(format!("\"{id}\" does not sign in through a command."));
        };
        if self.keep.view_key.as_ref().map(|key| key.name(definition)) != Some(shown) {
            return Err(format!(
                "\"{id}\" changed. Check its sign-in and try again."
            ));
        }
        let unrecorded = || "Flint could not record this choice.".to_owned();
        let consent = PluginConsent::seal(definition).ok_or_else(unrecorded)?;
        let ticket = OfferTicket::new().ok_or_else(unrecorded)?;
        self.keep.offers.insert(
            id.to_owned(),
            PendingOffer {
                ticket: ticket.clone(),
                consent,
                fingerprint,
                server,
            },
        );
        Ok(PluginOffer {
            context: id.to_owned(),
            ticket,
        })
    }

    /// The person turned the toggle on. The offer is used up here, whether or not it still holds.
    pub(super) async fn approve_keep_watching(&mut self, approval: PluginApproval) {
        let PluginApproval { context, ticket } = approval;
        if self
            .keep
            .offers
            .get(&context)
            .is_none_or(|offer| offer.ticket != ticket)
        {
            self.emit(Event::Error(format!(
                "\"{context}\": turn its background sign-in on again."
            )));
            return;
        }
        let Some(offer) = self.keep.offers.remove(&context) else {
            return;
        };
        match self.approved_entry(&context, &offer) {
            Ok(entry) => {
                self.save_watched(context, entry, Some(offer.consent)).await;
            }
            Err(why) => self.emit(Event::Error(why)),
        }
    }

    /// The entry for `id`, if it is still exactly what `offer` showed and nothing refuses it.
    fn approved_entry(&self, id: &str, offer: &PendingOffer) -> Result<WatchedCluster, String> {
        let changed = || format!("\"{id}\" changed. Check its sign-in and try again.");
        let Some(slot) = self.cluster(id) else {
            return Err(format!("cluster \"{id}\" no longer exists"));
        };
        if !self.is_kept(id) {
            return Err(format!("Turn on Keep Watching for \"{id}\" first."));
        }
        if slot.fingerprint != offer.fingerprint
            || slot.info.server.as_deref() != Some(offer.server.as_str())
        {
            return Err(changed());
        }
        if let Some(why) = self.keep_refusal(id) {
            return Err(why);
        }
        match &*self.sign_in(id) {
            SignIn::Plugin(definition) if offer.consent.holds(definition) => Ok(WatchedCluster {
                context: id.to_owned(),
                server: offer.server.clone(),
            }),
            _ => Err(changed()),
        }
    }

    /// The app is quitting: ends every grant, and answers once their plugins are gone, or after
    /// [`QUIT_WAIT`]. The wait runs apart from the manager's loop.
    pub(super) fn stop_keep_watching(&mut self, reply: tokio::sync::oneshot::Sender<()>) {
        self.keep.quitting = true;
        self.keep.consents.clear();
        self.keep.offers.clear();
        self.keep.feeds.clear();
        let runs = self.keep.plugin_runs.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(QUIT_WAIT, runs.ended()).await;
            let _ = reply.send(());
        });
    }

    /// The entry for watching `id`, if the definition is still the one the request was made on.
    /// For an exec plugin this runs nothing until its background sign-in toggle is turned on.
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
        Ok(WatchedCluster {
            context: id.to_owned(),
            server,
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
                let found = self.sign_in(&id);
                let (sign_in, command, shown_sign_in) = match &*found {
                    SignIn::Static => (BackgroundSignIn::NotNeeded, None, None),
                    SignIn::Provider | SignIn::Unknown => {
                        (BackgroundSignIn::Unavailable, None, None)
                    }
                    SignIn::Plugin(definition) => {
                        let on = self
                            .keep
                            .consents
                            .get(&id)
                            .is_some_and(|consent| consent.holds(definition));
                        (
                            if on {
                                BackgroundSignIn::On
                            } else {
                                BackgroundSignIn::Off
                            },
                            Some(definition.command.clone()),
                            self.keep.view_key.as_ref().map(|key| key.name(definition)),
                        )
                    }
                };
                let view = KeepWatchView {
                    status: self.keep_status(&id),
                    sign_in,
                    command,
                    shown_sign_in,
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

#[cfg(test)]
#[path = "keep_watching_tests.rs"]
mod tests;
