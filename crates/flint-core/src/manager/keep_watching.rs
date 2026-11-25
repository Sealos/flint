//! Feeds for clusters the person chose to keep watching (ADR-0012).
//!
//! Each runs on a connection of its own with only a read client, separate from the cluster's slot:
//! the slot's role, its write credential and the write gate never see it. Its credential access
//! allows the background role only, and ending the access kills a plugin that is running.
//!
//! [`ClusterManager::sync_keep_watching`] compares what should run with what does after every
//! message of the manager's loop, so no path that changes a definition has to remember it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::AbortHandle;

use super::telemetry::reads_token_file;
use super::{ClusterManager, Internal};
use crate::ClusterId;
use crate::cluster;
use crate::connection::member::MemberState;
use crate::connection::policy::{CredentialAccess, PluginRuns, Role};
use crate::credential::CredentialSource;
use crate::handle::{ConnectionState, Event};
use crate::keep_watching::{
    BackgroundSignIn, KeepWatchView, KeepWatching, MAX_WATCHED_CLUSTERS, OfferTicket,
    PluginApproval, PluginConsent, PluginOffer, SignIn, ViewKey, WatchedCluster, sign_in_of,
    too_many_watched,
};
use crate::kubeconfig::LoadedKubeconfig;
use crate::telemetry::{DiscoveryPace, KeptDiscovery, Telemetry};

/// How long a connection that failed for a reason worth retrying waits before the next.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How many background connections are made at once, so a launch starts no burst of plugins.
const CONCURRENT_CONNECTS: usize = 4;
/// How long a quit waits for plugins to be killed: inside GPUI's 200 ms for quit hooks.
pub(crate) const QUIT_WAIT: Duration = Duration::from_millis(100);

/// An offer a confirmation is showing, until it is approved or replaced.
struct PendingOffer {
    ticket: OfferTicket,
    consent: PluginConsent,
    fingerprint: Option<u64>,
    server: String,
}

/// The grants in force, shared with [`crate::CoreHandle`] so that ending one never waits for the
/// manager's queue, which a contended settings save can hold up.
#[derive(Clone, Default)]
pub(crate) struct KeptGrants(Arc<GrantsShared>);

#[derive(Default)]
struct GrantsShared {
    runs: PluginRuns,
    live: Mutex<LiveGrants>,
}

#[derive(Default)]
struct LiveGrants {
    quitting: bool,
    consents: HashMap<ClusterId, Arc<PluginConsent>>,
    /// Each running feed's access, by the attempt that made it.
    feeds: HashMap<ClusterId, (u64, CredentialAccess)>,
    /// The access of each running-set member's latest connection made under consent.
    members: HashMap<ClusterId, CredentialAccess>,
    /// How many times each cluster's grant was ended, so a save under way can't bring it back.
    revocations: HashMap<ClusterId, u64>,
    /// Other accesses under a cluster's consent, such as a Helm run's sign-in, by a key of their
    /// own, while their [`HeldAccess`] lives.
    held: HashMap<u64, (ClusterId, CredentialAccess)>,
    next_held: u64,
}

/// An access held with the grants until dropped ([`KeptGrants::hold_access`]).
pub(crate) struct HeldAccess {
    grants: KeptGrants,
    key: u64,
}

impl Drop for HeldAccess {
    fn drop(&mut self) {
        self.grants.live().held.remove(&self.key);
    }
}

impl KeptGrants {
    fn live(&self) -> std::sync::MutexGuard<'_, LiveGrants> {
        self.0.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ends `id`'s consent and kills its plugin, now.
    pub(crate) fn revoke(&self, id: &str) {
        let mut live = self.live();
        *live.revocations.entry(id.to_owned()).or_default() += 1;
        if let Some(consent) = live.consents.get(id) {
            consent.void();
        }
        if let Some((_, access)) = live.feeds.get(id) {
            access.revoke();
        }
        if let Some(access) = live.members.get(id) {
            access.revoke();
        }
        live.held
            .values()
            .filter(|(held, _)| held == id)
            .for_each(|(_, access)| access.revoke());
    }

    /// The count of running plugins a quit waits for.
    pub(crate) fn runs(&self) -> PluginRuns {
        self.0.runs.clone()
    }

    /// Holds `access`, made under `id`'s consent, until the result is dropped: ending that
    /// consent or quitting revokes it, and kills its plugin.
    pub(crate) fn hold_access(&self, id: &str, access: &CredentialAccess) -> HeldAccess {
        let mut live = self.live();
        if live.quitting {
            access.revoke();
        }
        let key = live.next_held;
        live.next_held += 1;
        live.held.insert(key, (id.to_owned(), access.clone()));
        HeldAccess {
            grants: self.clone(),
            key,
        }
    }

    /// Ends every grant now. The future resolves once their plugins are gone, or after
    /// [`QUIT_WAIT`], and needs a tokio runtime.
    pub(crate) fn quit(&self) -> impl Future<Output = ()> + Send + 'static {
        {
            let mut live = self.live();
            live.quitting = true;
            live.consents.values().for_each(|consent| consent.void());
            live.feeds.values().for_each(|(_, access)| access.revoke());
            live.members.values().for_each(CredentialAccess::revoke);
            live.held.values().for_each(|(_, access)| access.revoke());
        }
        let runs = self.0.runs.clone();
        async move {
            let _ = tokio::time::timeout(QUIT_WAIT, runs.ended()).await;
        }
    }

    fn quitting(&self) -> bool {
        self.live().quitting
    }

    fn revocations(&self, id: &str) -> u64 {
        self.live().revocations.get(id).copied().unwrap_or_default()
    }

    fn hold(&self, id: &str, consent: &Arc<PluginConsent>) {
        let mut live = self.live();
        if live.quitting {
            consent.void();
        }
        live.consents.insert(id.to_owned(), Arc::clone(consent));
    }

    /// Forgets every consent but those the manager still has.
    fn keep_only(&self, held: &HashMap<ClusterId, Arc<PluginConsent>>) {
        self.live()
            .consents
            .retain(|id, consent| held.get(id).is_some_and(|kept| Arc::ptr_eq(kept, consent)));
    }

    fn admit(&self, id: &str, attempt: u64, access: &CredentialAccess) {
        let mut live = self.live();
        if live.quitting {
            access.revoke();
        }
        live.feeds.insert(id.to_owned(), (attempt, access.clone()));
    }

    /// Holds a member connection's `access`, in place of the one before it, so that ending the
    /// consent or quitting ends it too.
    fn admit_member(&self, id: &str, access: &CredentialAccess) {
        let mut live = self.live();
        if live.quitting {
            access.revoke();
        }
        live.members.insert(id.to_owned(), access.clone());
    }

    /// Ends every member connection's access made under consent, and kills their plugins.
    fn revoke_members(&self) {
        self.live()
            .members
            .values()
            .for_each(CredentialAccess::revoke);
    }

    fn release(&self, id: &str, attempt: u64) {
        let mut live = self.live();
        if live.feeds.get(id).is_some_and(|(at, _)| *at == attempt) {
            live.feeds.remove(id);
        }
    }

    /// One more plugin running, until the result is dropped.
    #[cfg(test)]
    pub(crate) fn track_for_tests(&self) -> crate::connection::policy::PluginRun {
        self.0.runs.track()
    }
}

/// What a launch found in the files as they are now.
enum LaunchVerdict {
    Same,
    /// The files were read, and do not define what was agreed to.
    Changed,
    /// Something could not be read, which confirms nothing.
    Unconfirmed,
}

/// Compares the file list the saved settings name, then the definition those files give.
fn launch_verdict(config: &LaunchFiles, context: &str, consent: &PluginConsent) -> LaunchVerdict {
    let Ok(settings) = crate::settings::load_settings(&config.settings_dir) else {
        return LaunchVerdict::Unconfirmed;
    };
    let listed =
        crate::kubeconfig::paths_for_settings(&config.base, &settings, config.home.as_deref());
    if listed != config.paths {
        return LaunchVerdict::Changed;
    }
    let loaded = crate::kubeconfig::load(&config.paths);
    if !loaded.errors.is_empty() {
        return LaunchVerdict::Unconfirmed;
    }
    match &sign_in_of(&loaded.merged, context) {
        SignIn::Plugin(definition) if consent.verify(definition) => LaunchVerdict::Same,
        _ => LaunchVerdict::Changed,
    }
}

/// Where a launch check reads the current definition from.
#[derive(Clone)]
struct LaunchFiles {
    paths: Vec<PathBuf>,
    base: Vec<PathBuf>,
    home: Option<PathBuf>,
    settings_dir: PathBuf,
}

/// One watched cluster's connection and feed. Dropping it ends both, and kills its plugin.
struct KeptFeed {
    id: ClusterId,
    grants: KeptGrants,
    attempt: u64,
    /// The slot this connection was made for.
    generation: Option<u64>,
    fingerprint: Option<u64>,
    runs_plugin: bool,
    /// A detected file change ended its plugin: a failed connection says nothing of the sign-in.
    interrupted: bool,
    access: CredentialAccess,
    connecting: Option<AbortHandle>,
    feed: Option<Telemetry>,
    /// The connection's read client, once up. The slot's resource watches may read through it
    /// ([`ClusterManager::sync_warm_lists`]); it has no write client.
    client: Option<kube::Client>,
    /// The definition its connection was started from, which labels the feed it leads to.
    pin: crate::recheck::ContextPin,
}

impl Drop for KeptFeed {
    fn drop(&mut self) {
        self.access.revoke();
        self.grants.release(&self.id, self.attempt);
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
    /// Stopped by the person while the settings still list them, because the write failed or
    /// the settings could not be read.
    turned_off: HashSet<ClusterId>,
    grants: KeptGrants,
    retry_at: HashMap<ClusterId, Instant>,
    next_attempt: u64,
    connects: Arc<Semaphore>,
    discovery_pace: Arc<DiscoveryPace>,
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
            grants: KeptGrants::default(),
            retry_at: HashMap::new(),
            next_attempt: 1,
            connects: Arc::new(Semaphore::new(CONCURRENT_CONNECTS)),
            discovery_pace: Arc::default(),
            shown: BTreeMap::new(),
            sign_ins: None,
        }
    }
}

impl KeepWatch {
    /// Ends `id`'s consent and kills its plugin, now.
    pub(super) fn grants_revoke(&self, id: &str) {
        self.grants.revoke(id);
    }

    /// Whether `id`'s board entry belongs to a feed here, or to its last values.
    pub(super) fn holds_board_entry(&self, id: &str) -> bool {
        self.stopped.contains_key(id) || self.feeds.get(id).is_some_and(|kept| kept.feed.is_some())
    }
}

impl ClusterManager {
    /// The grants, for [`crate::CoreHandle`] to end without going through the queue.
    pub(crate) fn kept_grants(&self) -> KeptGrants {
        self.keep.grants.clone()
    }

    /// The kubeconfig file list changed: what was agreed to was read from the old one. Ends every
    /// plugin grant and open approval before the new files are read.
    pub(super) fn retire_plugin_grants(&mut self) {
        for consent in self.keep.consents.values() {
            consent.void();
        }
        self.keep.offers.clear();
        self.keep.feeds.retain(|_, kept| !kept.runs_plugin);
        self.keep.grants.revoke_members();
        self.void_broken_consents();
    }

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
        if self.keep.voided.contains(id)
            || slot.info.grant_server().as_deref() != Some(entry.server.as_str())
        {
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

    /// Whether `id` is watched and may be read now, selected or not.
    pub(super) fn keep_watching(&self, id: &str) -> bool {
        self.keep_status(id) == KeepWatching::Watching
    }

    /// Whether `id` needs a feed of its own: its slot has none and can't have one, or it was just
    /// selected and its warm feed is handed over.
    fn wants_kept_feed(&self, id: &str) -> bool {
        self.kept_feed_allowed(id)
            && (self.kept_feed_handed_over(id)
                || (self.active.as_deref() != Some(id) && !self.wants_telemetry(id)))
    }

    /// Whether a kept feed may run for `id` at all.
    fn kept_feed_allowed(&self, id: &str) -> bool {
        !self.keep.grants.quitting()
            && !self.disconnects.holds(id)
            && self.keep_watching(id)
            && !self.member_serves_keep(id)
    }

    /// Whether `id` is selected and its kept feed, live on the slot and definition it was started
    /// for, serves it in place of a feed on the slot. Selecting a watched cluster then lists
    /// nothing again, and leaving it keeps the same feed running. A connection still being made
    /// is not handed over, and a changed definition or ended consent drops the feed, so the slot
    /// starts one of its own. The feed has only a read client: writes still go through the slot.
    pub(super) fn kept_feed_handed_over(&self, id: &str) -> bool {
        self.active.as_deref() == Some(id) && self.live_kept_feed(id).is_some()
    }

    /// `id`'s kept feed, if it is up on the slot and whole definition it was started for and may
    /// run now.
    fn live_kept_feed(&self, id: &str) -> Option<&KeptFeed> {
        if !self.kept_feed_allowed(id) {
            return None;
        }
        let slot = self.cluster(id);
        let fingerprint = slot.and_then(|slot| slot.fingerprint);
        let generation = slot.map(|slot| slot.generation);
        self.keep.feeds.get(id).filter(|kept| {
            kept.feed.is_some() && kept.fingerprint == fingerprint && kept.generation == generation
        })
    }

    /// The read client of `id`'s live kept feed ([`Self::live_kept_feed`]), with the attempt that
    /// made it. Reads only: the kept connection has no write client.
    pub(super) fn kept_read_client(&self, id: &str) -> Option<(u64, kube::Client)> {
        let kept = self.live_kept_feed(id)?;
        Some((kept.attempt, kept.client.clone()?))
    }

    /// Whether `id` is a member of the open set with background sign-in consent that is up or
    /// still connecting. Its slot then connects under that consent and its feed serves keep
    /// watching, so its plugin runs once. A member waiting for a sign-in makes no connection, and
    /// leaving the set ends it: the kept feed takes over in both cases.
    fn member_serves_keep(&self, id: &str) -> bool {
        self.member_on_its_server(id)
            && self.member_consent(id).is_some()
            && self
                .members
                .get(id)
                .is_some_and(|member| !matches!(member.state(), MemberState::SignInNeeded { .. }))
    }

    /// Member `id`'s connection made under consent in the background failed. Like a kept feed's
    /// first failure, keep watching then waits for a sign-in, so no kept feed runs the plugin
    /// again until the person acts.
    pub(super) fn member_signed_out(&mut self, id: &str) {
        if self.keep_watching(id) && self.member_consent(id).is_some() {
            self.stop_kept(id);
        }
    }

    /// Whether no change of the kubeconfig files is waiting to be read.
    pub(super) fn definitions_settled(&self) -> bool {
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
        // A member's too. One still connecting ends: its result reports the ended consent, so
        // no token from before the read brings it up.
        for id in self.consented_members(true) {
            if let Some(slot) = self.cluster(&id) {
                slot.access.set_role(Role::Paused);
            }
        }
    }

    /// Members whose connection was made under consent: up, and also those still connecting
    /// with `connecting`.
    fn consented_members(&self, connecting: bool) -> Vec<ClusterId> {
        self.clusters
            .iter()
            .filter(|slot| {
                slot.access.unattended()
                    && self.members.contains_key(&slot.info.name)
                    && match slot.state {
                        ConnectionState::Connected(_) => true,
                        ConnectionState::Connecting => connecting,
                        _ => false,
                    }
            })
            .map(|slot| slot.info.name.clone())
            .collect()
    }

    /// Stops the feeds no longer wanted, starts those missing, and reports any change.
    pub(super) fn sync_keep_watching(&mut self) {
        self.refresh_sign_ins();
        self.void_broken_consents();
        self.end_unconsented_members();
        let running: Vec<ClusterId> = self.keep.feeds.keys().cloned().collect();
        for id in running {
            let slot = self.cluster(&id);
            let current = slot.and_then(|slot| slot.fingerprint);
            let generation = slot.map(|slot| slot.generation);
            let same =
                self.keep.feeds.get(&id).is_some_and(|kept| {
                    kept.fingerprint == current && kept.generation == generation
                });
            if !same || !self.wants_kept_feed(&id) {
                self.keep.feeds.remove(&id);
            }
        }
        let settled = self.definitions_settled();
        if settled {
            for kept in self.keep.feeds.values().filter(|kept| kept.runs_plugin) {
                kept.access.set_role(Role::Background);
            }
            for id in self.consented_members(false) {
                if let Some(slot) = self.cluster(&id) {
                    slot.access.set_role(self.connection_role(&id));
                }
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
        self.sync_warm_lists();
        self.publish_keep_watching();
    }

    /// Removes consent a confirmed definition no longer matches, or whose entry is gone, for
    /// good. A paused context is not judged: what it shows may only be a fallback.
    fn void_broken_consents(&mut self) {
        let settings = &self.settings;
        self.keep
            .consents
            .retain(|id, _| settings.watched_cluster(id).is_some());
        // Unreadable settings hide every entry: what was turned off stays off once they are back.
        if self.settings_error.is_none() {
            self.keep
                .turned_off
                .retain(|id| settings.watched_cluster(id).is_some());
        }
        let clusters = &self.clusters;
        self.keep
            .offers
            .retain(|id, _| clusters.iter().any(|slot| slot.info.name == *id));
        let broken: Vec<ClusterId> = self
            .keep
            .consents
            .iter()
            .filter(|(id, consent)| {
                consent.is_void() || self.keep_status(id) == KeepWatching::NeedsConfirming
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in broken {
            self.keep.consents.remove(&id);
            self.keep.offers.remove(&id);
            self.keep.voided.insert(id);
        }
        self.keep.grants.keep_only(&self.keep.consents);
    }

    /// Asked right before each start of `id`'s plugin: the saved file list and the files as they
    /// are now must still define what was agreed to, whether or not a change has been noticed.
    /// A confirmed difference ends the consent at once, so undoing it later brings nothing back.
    pub(super) fn launch_check(&self, id: &str, consent: Arc<PluginConsent>) -> cluster::SendCheck {
        let files = LaunchFiles {
            paths: self.kubeconfig_paths.clone(),
            base: self.config.kubeconfig_base.clone(),
            home: self.config.home.clone(),
            settings_dir: self.config.config_dir.clone(),
        };
        let context = id.to_owned();
        let wake = self.internal_tx.clone();
        cluster::SendCheck::new(move || {
            let (files, context, consent) = (files.clone(), context.clone(), consent.clone());
            let wake = wake.clone();
            Box::pin(async move {
                let judged = Arc::clone(&consent);
                let verdict =
                    tokio::task::spawn_blocking(move || launch_verdict(&files, &context, &judged))
                        .await
                        .unwrap_or(LaunchVerdict::Unconfirmed);
                match verdict {
                    LaunchVerdict::Same => return Ok(()),
                    LaunchVerdict::Changed => {
                        consent.void();
                        let _ = wake.send(Internal::KeepWatchRetry);
                    }
                    LaunchVerdict::Unconfirmed => {}
                }
                Err("its definition is no longer the one confirmed".to_owned())
            })
        })
    }

    /// `id`'s background sign-in consent, if it holds for the definition shown now. A pause does
    /// not end it: a paused connection runs nothing anyway.
    pub(super) fn member_consent(&self, id: &str) -> Option<Arc<PluginConsent>> {
        let consent = self.keep.consents.get(id).filter(|c| !c.is_void())?;
        match &*self.sign_in(id) {
            SignIn::Plugin(definition) if consent.holds(definition) => Some(Arc::clone(consent)),
            _ => None,
        }
    }

    /// Access for a connection of running-set member `id` made in the background under
    /// `consent`: the keep-watching rules, with the same launch check and plugin count as a
    /// kept feed, and ended with that consent. It stays allowed while the member is selected.
    pub(super) fn member_access(
        &self,
        id: &str,
        attempt: u64,
        consent: Arc<PluginConsent>,
    ) -> CredentialAccess {
        let failed = self.internal_tx.clone();
        let cluster = id.to_owned();
        let access = CredentialAccess::consented_member(
            Arc::new(move || {
                let _ = failed.send(Internal::MemberCredentialFailed {
                    cluster: cluster.clone(),
                    attempt,
                });
            }),
            Some(self.launch_check(id, consent)),
            self.keep.grants.0.runs.clone(),
        );
        self.keep.grants.admit_member(id, &access);
        access
    }

    /// Ends the connections members made under a consent that no longer holds: turned off,
    /// voided by a change, or gone with its entry.
    fn end_unconsented_members(&mut self) {
        let mut ended = self.consented_members(true);
        ended.retain(|id| self.member_consent(id).is_none());
        let now = Instant::now();
        for id in ended {
            // Ended here whatever the credential shows now: one that no longer runs a command
            // is not demoted below, and its access would otherwise go on.
            if let Some(slot) = self.cluster(&id) {
                slot.access.revoke();
            }
            if self.active.as_deref() == Some(id.as_str()) {
                self.reconnect_selected(&id);
            } else {
                self.demote_member(&id, now);
            }
        }
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
            self.keep.grants.0.runs.clone(),
        );
        self.keep.grants.admit(id, attempt, &access);
        let mut kubeconfig = self.kubeconfig.merged.clone();
        cluster::disable_interactive_exec(&mut kubeconfig);
        // Read with the config it connects from: the connection may finish after a reload.
        let shown = self.cluster(id).and_then(|slot| slot.info.server.clone());
        let pin = self.contexts.pin(id, shown.as_deref());
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
        let generation = self.cluster(id).map(|slot| slot.generation);
        self.keep.feeds.insert(
            id.to_owned(),
            KeptFeed {
                id: id.to_owned(),
                grants: self.keep.grants.clone(),
                attempt,
                generation,
                fingerprint,
                runs_plugin,
                interrupted: false,
                access,
                connecting: Some(task.abort_handle()),
                feed: None,
                client: None,
                pin,
            },
        );
    }

    /// Gives each background feed its Pending threshold.
    pub(super) fn apply_kept_pending_thresholds(&self) {
        for (id, kept) in &self.keep.feeds {
            if let Some(feed) = &kept.feed {
                feed.set_pending_after(self.pending_after(id));
            }
        }
    }

    /// A background connection finished. A plugin's cluster never retries by itself.
    pub(super) fn kept_connected(
        &mut self,
        id: &str,
        attempt: u64,
        result: Result<kube::Client, bool>,
    ) {
        let pending_after = self.pending_after(id);
        let current = self.cluster(id).and_then(|slot| slot.fingerprint);
        let hours = self.history_hours(id);
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
                // The cluster's history, which the slot's feed may have written until now. A
                // connection made for a definition the slot no longer has writes into none: the
                // next sync drops it.
                let fingerprint = kept.fingerprint.filter(|print| current == Some(*print));
                let trends = self.histories.store(id, fingerprint);
                super::history::set_hours(&trends, hours);
                let discovery = KeptDiscovery {
                    client: client.clone(),
                    requests: requests.clone(),
                    grant: kept.access.grant(CredentialSource::Plugin),
                    pace: Arc::clone(&self.keep.discovery_pace),
                };
                kept.client = Some(client.clone());
                let mut feed = Telemetry::start(
                    id.into(),
                    client,
                    target,
                    Arc::clone(&trends),
                    self.telemetry_timing,
                    requests,
                    pending_after,
                );
                // A slot gone by now has no generation a summary could match.
                let generation = kept.generation.unwrap_or(u64::MAX);
                self.telemetry.publish_kept(
                    id,
                    generation,
                    kept.pin.clone(),
                    &mut feed,
                    trends,
                    discovery,
                );
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

    /// The credential of attempt `attempt` failed or was refused. A feed handed over to the
    /// selected cluster only ends: the slot's foreground connection, which works on its own,
    /// starts a feed in its place, so alerts go on and keep watching stays on. Leaving the
    /// cluster starts a kept feed again, and a failure then waits for a sign-in as usual.
    pub(super) fn kept_failed(&mut self, id: &str, attempt: u64) {
        if self
            .keep
            .feeds
            .get(id)
            .is_none_or(|kept| kept.attempt != attempt)
        {
            return;
        }
        if self.kept_feed_handed_over(id) {
            self.keep.feeds.remove(id);
        } else {
            self.stop_kept(id);
        }
    }

    /// Stops `id` until the person acts, keeping its last values in memory, marked with their age.
    fn stop_kept(&mut self, id: &str) {
        self.telemetry.hold(id);
        let fingerprint = self.cluster(id).and_then(|slot| slot.fingerprint);
        self.keep.stopped.insert(id.to_owned(), fingerprint);
        self.keep.feeds.remove(id);
        self.drop_stale_warm_lists(id);
    }

    /// Keeps `id` off while unreadable settings hide its entry, as if the entry were removed.
    pub(super) fn hold_keep_watching_off(&mut self, id: &str) {
        self.keep.turned_off.insert(id.to_owned());
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
            // Before the settings write below: nothing reads through the kept connection after.
            self.drop_stale_warm_lists(&id);
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
        let revocations = self.keep.grants.revocations(&id);
        self.write_settings("keep watching", move |dir| {
            crate::settings::set_watched_cluster(dir, &name, Some(entry))
        })
        .await;
        let saved = self
            .settings
            .watched_cluster(&id)
            .is_some_and(|entry| entry.server == server);
        // Turned off while this was being saved: the later choice wins.
        let current = self.keep.grants.revocations(&id) == revocations;
        if let (true, true, Some(consent)) = (saved, current, consent) {
            let consent = Arc::new(consent);
            self.keep.grants.hold(&id, &consent);
            self.keep.consents.insert(id, consent);
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
        self.drop_stale_warm_lists(id);
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
        self.void_broken_consents();
        self.keep.offers.remove(id);
        let Some(slot) = self.cluster(id) else {
            return Err(format!("cluster \"{id}\" no longer exists"));
        };
        let fingerprint = slot.fingerprint;
        let server = slot.info.grant_server();
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
        self.void_broken_consents();
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
            || slot.info.grant_server().as_deref() != Some(offer.server.as_str())
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
        let Some(server) = slot.info.grant_server() else {
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
        if self.disconnects.holds(id) {
            return Some(format!(
                "\"{id}\" is disconnected. Select it to connect first."
            ));
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
pub(super) mod tests;
