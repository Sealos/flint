//! The bridge between a UI (or any other front end) and the core (ADR-0002).
//!
//! The core runs on its own multi-thread tokio runtime. Front ends send [`Command`]s (and the
//! requests behind the other methods) over unbounded queues ([`crate::core_queue`]), so none is
//! lost to a full queue, and read [`Event`]s from a bounded queue ([`crate::events`]) that
//! keeps the latest state of each kind; both work with any executor, so a GPUI task can await
//! them directly. Nothing here blocks the caller.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{oneshot, watch};

use crate::cluster::{ClusterId, ServerVersion};
use crate::credential::WriteCredential;
use crate::discovery::ClusterDiscovery;
use crate::exec::{ContextCheck, ContextWatch, ExecRequest, ExecSession, LaunchCheck};
use crate::helm::{HelmError, HelmOutput, HelmPin, HelmQuery, HelmTarget};
use crate::kubeconfig::ContextInfo;
use crate::logs::{
    Freshness, LogFilter, LogLines, LogOptions, LogSearch, LogSnapshot, LogSubject, LogTarget,
    PodSet, WorkloadPod,
};
use crate::manager::ClusterManager;
use crate::port_forward::{ForwardRequest, PortForward};
use crate::protection::{GatedClient, SessionUnlocks, WriteRefusal};
use crate::read::{KindId, ObjectDetail};
use crate::recheck::{ContextPin, RECHECK_EVERY};
use crate::settings::{Appearance, ClusterPin, Settings, WatchMode};
use crate::store::{Feed, KindSnapshot, SyncState};
use crate::view::{self, ViewQuery};
use crate::watch::HubRegistry;
use crate::write::WriteError;

/// What the core needs to start. Usually built from [`crate::bootstrap::Bootstrap`].
#[derive(Debug, Clone)]
pub struct CoreConfig {
    /// Flint's config directory (`settings.toml`, `state.toml`).
    pub config_dir: PathBuf,
    /// Settings loaded at startup.
    pub settings: Settings,
    /// Why `settings.toml` could not be loaded at startup, if it could not.
    pub settings_error: Option<String>,
    /// Kubeconfig files from `KUBECONFIG` or `~/.kube/config`, in merge order. The settings'
    /// `kubeconfig_paths` are appended, and recomputed when settings change.
    pub kubeconfig_base: Vec<PathBuf>,
    /// The home directory, for `~` in the settings' `kubeconfig_paths`.
    pub home: Option<PathBuf>,
    /// Watch the kubeconfig files and `settings.toml`, and reload on change.
    pub watch_files: bool,
    /// Retry timings of the watch engine. Leave at the default; tests against a fake API
    /// server shorten them.
    pub watch_timing: WatchTiming,
}

/// How long the watch engine waits before retrying. The defaults suit real API servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchTiming {
    /// First wait after a broken watch or failed list; doubles up to `backoff_max`.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// First wait after a 403 or 404, which is retried a few times before it is believed.
    pub denied_backoff_min: Duration,
}

impl Default for WatchTiming {
    fn default() -> Self {
        WatchTiming {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            denied_backoff_min: Duration::from_millis(500),
        }
    }
}

/// Requests from the front end.
#[derive(Debug, Clone)]
pub enum Command {
    /// Make a cluster the active one, connecting it if it is not connected. Selecting a cluster
    /// whose connection failed retries it.
    Activate(ClusterId),
    /// Ends the selected cluster's connection attempt and its automatic retries, if it is still
    /// the slot `generation`. It stays failed until selected or retried.
    CancelConnection { cluster: ClusterId, generation: u64 },
    /// Stops everything Flint runs for a cluster (watches, feeds, retries, log streams, clients)
    /// and ends its keep-watching consent. Nothing connects it again until it is selected or a
    /// set with it is opened. Shells are the front end's to end.
    DisconnectCluster(ClusterId),
    /// Expose a cluster to AI agents at `server` (`Some`), limited to `scope` (ADR-0013), or hide
    /// it (`None`), in `settings.toml`. `server` is the URL the user confirmed; if the context
    /// no longer points there, nothing is written and an error is reported. Exposing an exposed
    /// cluster again rewrites its entry with the new scope in one write.
    SetAiExposed {
        cluster: ClusterId,
        server: Option<String>,
        scope: crate::settings::AiScope,
    },
    /// Change what agents may read of a cluster exposed at `server` to `scope` (Edit AI Scope),
    /// in `settings.toml`. Never exposes anything: if the context no longer points at
    /// `server`, or the file no longer exposes it there (hidden meanwhile), nothing is written
    /// and an error is reported.
    SetAiScope {
        cluster: ClusterId,
        server: String,
        /// The scope the user started editing: if it changed on disk since, nothing is
        /// written.
        from: crate::settings::AiScope,
        scope: crate::settings::AiScope,
    },
    /// Trust a cluster at `server` (`Some`), or stop trusting it (`None`), in `settings.toml`
    /// (ADR-0015). As with [`Command::SetAiExposed`], `server` is the URL the user confirmed; if
    /// the context no longer points there, nothing is written and an error is reported.
    ///
    /// `request` numbers the request among the context's Trust and Stop Trusting requests;
    /// [`CoreHandle::trust`] and [`CoreHandle::untrust`] take it from the session's state, so
    /// use those rather than sending this yourself.
    SetTrusted {
        cluster: ClusterId,
        server: Option<String>,
        request: u64,
    },
    /// Reload the kubeconfig files now.
    ReloadKubeconfig,
    /// Set the watch mode and write it to `settings.toml` (ADR-0008). `cluster: None` sets the
    /// global mode; `Some` sets that cluster's override, and `mode: None` then removes the
    /// override. The change applies at once and arrives back as `Event::SettingsChanged`.
    SetWatchMode {
        cluster: Option<ClusterId>,
        mode: Option<WatchMode>,
    },
    /// Set the custom name shown for `cluster` (`None` removes it) and write it to
    /// `settings.toml`. Display only. The change arrives back as `Event::SettingsChanged`.
    SetClusterName {
        cluster: ClusterId,
        name: Option<String>,
    },
    /// Set the colour tag of `cluster` at `server` (`None` removes it) and write it to
    /// `settings.toml`. Cosmetic. The change arrives back as `Event::SettingsChanged`.
    SetClusterTag {
        cluster: ClusterId,
        server: String,
        tag: Option<crate::settings::ClusterTag>,
    },
    /// Set the appearance (light, dark, or following macOS) and write it to `settings.toml`. The
    /// change arrives back as `Event::SettingsChanged`.
    SetAppearance(Appearance),
    /// Turn desktop notifications on or off and write it to `settings.toml`. The change arrives
    /// back as `Event::SettingsChanged`.
    SetDesktopNotifications(bool),
    /// Set whether `cluster` at `server` posts desktop alerts (`None` follows the global setting)
    /// and write it to `settings.toml`. The change arrives back as `Event::SettingsChanged`.
    SetClusterAlerts {
        cluster: ClusterId,
        server: String,
        enabled: Option<bool>,
    },
    /// The namespaces the user filters `cluster` on (empty: all). When the user may not list a
    /// kind in all namespaces, it is listed in these one by one; without a filter, in the
    /// context's default namespace, else in every namespace the user can list. The filter
    /// itself is applied by the front end and is not stored by the core.
    SetNamespaceFilter {
        cluster: ClusterId,
        namespaces: Vec<String>,
    },
    /// Set one threshold, for every cluster (`cluster: None`) or one context at one server, and
    /// write it to `settings.toml`. `value: None` clears it. A value out of range is reported as
    /// [`Event::Error`] and changes nothing. The change arrives back as `Event::SettingsChanged`.
    SetThreshold {
        cluster: Option<ClusterPin>,
        metric: crate::thresholds::ThresholdMetric,
        value: Option<u32>,
    },
    /// Set the lines each log session keeps and write it to `settings.toml`. Sessions opened
    /// afterwards use it. The change arrives back as `Event::SettingsChanged`.
    SetLogBufferLines(u32),
    /// Set the MiB each log session's lines may take and write it to `settings.toml`. Sessions
    /// opened afterwards use it. The change arrives back as `Event::SettingsChanged`.
    SetLogBufferMib(u32),
    /// Keep exactly these clusters (2 to 5, or none to clear) connected and watched beside the
    /// active one. Each reports its connection as [`Event::ConnectionChanged`], and
    /// [`CoreHandle::running_set`] says more. Removed members' watches stop. A list that cannot
    /// be a set changes nothing and is reported as [`Event::Error`]. Grants no write access.
    SetRunningSet(Vec<ClusterId>),
    /// Keep watching `cluster` in the background (ADR-0012), or stop (`definition: None`).
    /// `definition` is the [`KeepWatchView::definition`](crate::keep_watching::KeepWatchView)
    /// the request was made on: if the context has changed since, nothing is written and an
    /// error is reported. This never lets an exec plugin run: that takes
    /// [`Command::ApproveKeepWatching`].
    SetKeepWatching {
        cluster: ClusterId,
        definition: Option<u64>,
    },
    /// The person turned on `cluster`'s background sign-in toggle
    /// ([`CoreHandle::offer_keep_watching`]): its plugin may run until the app quits. Accepted
    /// once, for the definition the toggle showed.
    ApproveKeepWatching(crate::keep_watching::PluginApproval),
    /// Turns the background sign-in toggle off: no new launch, and its background watch stops.
    RevokeBackgroundSignIn(ClusterId),
}

/// What a member of the running set is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberStatus {
    Connecting,
    Live,
    /// Its watches lost contact and none has synced again.
    Reconnecting {
        error: crate::SafeErrorText,
    },
    /// Not retried until the cluster is selected.
    SignInNeeded {
        reason: crate::SafeErrorText,
    },
    /// Retried after a backoff.
    Failed {
        error: crate::SafeErrorText,
    },
}

/// One member of the running set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberReport {
    pub cluster: ClusterId,
    pub status: MemberStatus,
    /// A kubeconfig file that defines it can't be read: what runs goes on and nothing new starts.
    pub paused: bool,
}

/// Why a pin stayed out of a set being opened. It gets no request, watch or plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftOutReason {
    Removed,
    Paused,
    Provisional,
    Changed,
}

impl LeftOutReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Removed => "Removed",
            Self::Paused => "Paused",
            Self::Provisional => "Provisional",
            Self::Changed => "Changed",
        }
    }
}

/// A pin left out of a set being opened, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftOutMember {
    pub context: ClusterId,
    pub reason: LeftOutReason,
}

/// The answer to opening a set. With too few eligible pins, `members` is empty and the running
/// set did not change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedSet {
    pub members: Vec<AdmittedMember>,
    pub left_out: Vec<LeftOutMember>,
}

/// A running-set member as the core admitted it: its context, at the exact server it was checked
/// on. Whatever comes from a set carries this, and is refused once the context has left that
/// server, even if it later comes back. Grants nothing on the write path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedMember {
    pub(crate) pin: ClusterPin,
    pub(crate) at: ContextPin,
    /// [`ClusterSummary::endpoint`] when admitted.
    pub(crate) endpoint: u64,
}

impl AdmittedMember {
    pub fn context(&self) -> &str {
        &self.pin.context
    }

    pub fn server(&self) -> &str {
        &self.pin.server
    }

    pub fn pin(&self) -> &ClusterPin {
        &self.pin
    }

    /// Whether `summary`, if confirmed, still shows the endpoint this member was admitted on. A
    /// context moved and put back has the same URL but another endpoint number.
    pub fn same_endpoint(&self, summary: &ClusterSummary) -> bool {
        !summary.confirmed || summary.endpoint == self.endpoint
    }

    /// What a log session on `cluster` is pinned to: stale for any context but this member's.
    pub(crate) fn log_pin(&self, cluster: &str) -> ContextPin {
        if self.context() == cluster {
            self.at.clone()
        } else {
            ContextPin::stale()
        }
    }

    /// Whether `pin`, read with a client of `cluster`, is the exact definition admitted.
    pub(crate) fn holds(&self, cluster: &str, pin: &ContextPin) -> bool {
        self.context() == cluster && self.at == *pin
    }
}

/// Requests that carry a reply channel or come from a [`Subscription`].
pub(crate) enum Request {
    /// Each cluster's keep-watching toggle.
    KeepWatching {
        reply: oneshot::Sender<
            std::collections::BTreeMap<ClusterId, crate::keep_watching::KeepWatchView>,
        >,
    },
    /// An offer to turn on `cluster`'s background sign-in, for the definition named `shown`.
    OfferKeepWatching {
        cluster: ClusterId,
        shown: u64,
        reply: oneshot::Sender<Result<crate::keep_watching::PluginOffer, String>>,
    },
    /// Each member of the running set and its state.
    RunningSet {
        reply: oneshot::Sender<Vec<MemberReport>>,
    },
    /// Makes the running set exactly these pins, or changes nothing if any does not hold.
    AdmitRunningSet {
        pins: Vec<ClusterPin>,
        reply: oneshot::Sender<Result<Vec<AdmittedMember>, String>>,
    },
    /// Makes the running set the pins that can join now, and says why the others can't.
    OpenRunningSet {
        pins: Vec<ClusterPin>,
        reply: oneshot::Sender<Result<OpenedSet, String>>,
    },
    /// Why `member` no longer stands for its context, or `None`. Never contacts the server.
    CheckMember {
        member: AdmittedMember,
        reply: oneshot::Sender<Option<String>>,
    },
    /// The read client of `member`, checked in the same step.
    MemberClient {
        member: AdmittedMember,
        reply: oneshot::Sender<Result<kube::Client, String>>,
    },
    /// Reload the kubeconfig files, and say when that is done.
    ReloadKubeconfig {
        reply: oneshot::Sender<bool>,
    },
    /// Activates `cluster` only if it is still the slot and definition given, in the same step.
    ActivatePinned {
        cluster: ClusterId,
        pin: ReadPin,
        generation: u64,
        reply: oneshot::Sender<Result<(), ActivationRefused>>,
    },
    Subscribe {
        id: u64,
        cluster: ClusterId,
        kind: KindId,
    },
    /// [`Self::Subscribe`] for a running-set member: nothing starts unless it still holds.
    SubscribeMember {
        id: u64,
        cluster: ClusterId,
        member: AdmittedMember,
        kind: KindId,
        feed: crate::watch::MemberFeed,
    },
    Unsubscribe {
        id: u64,
    },
    /// An overview of `cluster` is showing.
    OpenOverview {
        id: u64,
        cluster: ClusterId,
    },
    /// [`Self::OpenOverview`] only if `cluster` is still the slot `generation`.
    OpenOverviewOn {
        id: u64,
        cluster: ClusterId,
        generation: u64,
        /// True while the overview is registered on that slot.
        live: watch::Sender<bool>,
    },
    CloseOverview {
        id: u64,
    },
    FetchObject {
        cluster: ClusterId,
        kind: KindId,
        namespace: Option<String>,
        name: String,
        /// Set for a row of a running set: checked before a client is taken.
        member: Option<AdmittedMember>,
        reply: oneshot::Sender<Result<Arc<ObjectDetail>, String>>,
    },
    /// One object, no events, refused unless the context still matches `pin`.
    FetchPinnedObject {
        cluster: ClusterId,
        pin: ReadPin,
        kind: KindId,
        namespace: Option<String>,
        name: String,
        reply: oneshot::Sender<Result<Arc<ObjectDetail>, String>>,
    },
    Discovery {
        cluster: ClusterId,
        reply: oneshot::Sender<Option<Arc<ClusterDiscovery>>>,
    },
    WatchedKinds {
        cluster: ClusterId,
        reply: oneshot::Sender<Vec<(KindId, Feed)>>,
    },
    OpenLogs {
        id: u64,
        cluster: ClusterId,
        /// The server the session is for, as the context showed it when the tab opened (its
        /// label); `None` for whatever the context shows when the core takes the request.
        server: Option<Option<String>>,
        /// Set for a running set's tab: the session is pinned to what was admitted.
        member: Option<AdmittedMember>,
        /// How many times `cluster` had been disconnected when the tab asked.
        disconnects: u64,
        subject: LogSubject,
        options: LogOptions,
        snapshots: watch::Sender<Arc<LogSnapshot>>,
        paused: watch::Receiver<bool>,
        /// Registered when the session was opened, so whether it is shown counts from the
        /// start; the session keeps it while it runs.
        budget: crate::logs::Budgeted,
    },
    CloseLogs {
        id: u64,
    },
    /// The cluster's client, if it is connected. For sessions, which run on their own tasks.
    Client {
        cluster: ClusterId,
        reply: oneshot::Sender<Option<kube::Client>>,
    },
    /// The cluster's client for an action that changes the cluster or runs code in it, if the
    /// cluster is connected and not protected (ADR-0015). Every such action asks through this.
    /// Answered with the context's pin, read in the same step as the client, which a session
    /// started with it is pinned to.
    WriteClient {
        cluster: ClusterId,
        /// The server the session is for (its tab's), if it names one: refused if the context
        /// shows another.
        server: Option<Option<String>>,
        reply: oneshot::Sender<Result<(GatedClient, ContextPin), WriteRefusal>>,
    },
    /// [`Self::WriteClient`] for one request of a reviewed write: also the credential of the same
    /// connection as the client.
    BoundWriteClient {
        cluster: ClusterId,
        server: Option<Option<String>>,
        reply: oneshot::Sender<Result<BoundWriteClient, WriteError>>,
    },
    /// What the write gate says now about `cluster` as pinned at `pin`, for a session about to
    /// run a command.
    CheckWriteGate {
        cluster: ClusterId,
        pin: ContextPin,
        reply: oneshot::Sender<Result<(), WriteRefusal>>,
    },
    /// [`Self::CheckWriteGate`] for a write, which also needs the definition pinned at `pin`.
    CheckWritePin {
        cluster: ClusterId,
        pin: ContextPin,
        reply: oneshot::Sender<Result<(), WriteError>>,
    },
    /// Admits a port-forward on the cluster's current connection.
    AdmitForward {
        cluster: ClusterId,
        server: Option<Option<String>>,
        admitted: Option<Arc<AdmittedMember>>,
        reply: oneshot::Sender<Result<crate::port_forward::ForwardGrant, WriteError>>,
    },
    /// Whether admitted forward `id` may carry on. Answered from memory.
    CheckForward {
        id: u64,
        reply: oneshot::Sender<Result<(), crate::port_forward::ForwardFailure>>,
    },
    /// Why a session opened on `cluster` at `pin` must end (the context was re-pointed or
    /// removed since), or `None`. Never contacts the server.
    CheckContext {
        cluster: ClusterId,
        pin: ContextPin,
        reply: oneshot::Sender<Option<String>>,
    },
    /// The pin of a Helm view opening on `cluster` now, whether or not it can run yet.
    HelmPin {
        cluster: ClusterId,
        reply: oneshot::Sender<HelmPin>,
    },
    /// What a `helm` run on `cluster` at `pin` is held to, if it may run.
    HelmTarget {
        cluster: ClusterId,
        pin: HelmPin,
        member: Option<AdmittedMember>,
        reply: oneshot::Sender<Result<HelmTarget, HelmError>>,
    },
}

/// The client for one request of a reviewed write, its context's pin, and its connection's
/// credential, all read in one step.
pub(crate) type BoundWriteClient = (GatedClient, ContextPin, WriteCredential);

/// Connection state of one cluster.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// Nothing has been tried yet. No network traffic happens until a cluster is activated.
    #[default]
    Disconnected,
    Connecting,
    Connected(ServerVersion),
    /// Why it failed, as safe to show ([`crate::cluster::ClusterError::summary`]).
    Failed(crate::SafeErrorText),
    /// Paused with no connection: the core starts none until a kubeconfig file that may define
    /// this cluster can be read. Nothing went wrong with the connection, and the load that ends
    /// the pause clears this. A context that is not paused never shows it.
    AwaitingConfirmation,
}

/// How the selected cluster's connection has been going since it was selected or last retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionProgress {
    pub cluster: ClusterId,
    /// The slot it is for ([`ClusterSummary::generation`]).
    pub generation: u64,
    /// When the first attempt started.
    pub since: std::time::Instant,
    /// Automatic retries scheduled so far.
    pub retries: u32,
    /// When the next automatic retry starts, if one is waiting.
    pub next_retry: Option<std::time::Instant>,
}

/// A cluster as the sidebar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterSummary {
    pub info: ContextInfo,
    pub state: ConnectionState,
    /// The core's slot for the context: a new one (re-pointed or otherwise changed, back after
    /// being removed) has a higher generation. What came from an older one (its discovery, a
    /// session opened on it) belongs to a cluster that is gone, whatever lists a front end skipped
    /// (#255).
    pub generation: u64,
    /// The context's confirmed definition. A move changes it, even one a later reload undoes;
    /// an edit that keeps the server (a rotated token, a namespace) does not.
    pub definition: u64,
    /// The same for the endpoint: an edit that only adds or drops a trailing slash keeps it, as
    /// it keeps a log session running (a shell, which the write gate checks, ends).
    pub endpoint: u64,
    /// Moves when nothing confirmed or reviewed before may be written any more on this context:
    /// a disconnect, or a fallback showing another server. Never goes back.
    pub write_epoch: u64,
    /// A load confirmed the server shown. Otherwise it may be a fallback, showing only while a
    /// kubeconfig file before it can't be read.
    pub confirmed: bool,
    /// A kubeconfig file that can't be read could still change this definition, so the write gate
    /// refuses every write. Not the opposite of `confirmed`: a confirmed server can be provisional
    /// again while a file is unreadable.
    pub provisional: bool,
    /// The core starts nothing new on this context: its definition differs from the confirmed
    /// one, or only an unreadable file still defines it. A first-seen context is never paused.
    pub paused: bool,
}

impl ClusterSummary {
    /// What a read opened on this cluster now stays pinned to.
    pub fn read_pin(&self) -> ReadPin {
        ReadPin {
            definition: self.definition,
            server: self.info.server.clone(),
        }
    }
}

/// A context's identity when a read-only view opened on it: its confirmed definition and exact
/// server. A move changes the definition even if a later reload puts the server back. Grants
/// nothing on the write path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPin {
    pub definition: u64,
    pub server: Option<String>,
}

/// Why a pinned activation did not happen. The active cluster is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationRefused {
    /// The context is no longer in kubeconfig.
    Removed,
    /// The context now has another slot, definition or server.
    Changed,
    /// The context is paused until its kubeconfig is confirmed.
    Paused,
    /// The core stopped without answering.
    Stopped,
}

/// What the core's answer to a reload means: `None` when it stopped without answering.
fn reload_outcome(applied: Option<bool>) -> Result<(), ReloadError> {
    match applied {
        Some(true) => Ok(()),
        Some(false) => Err(ReloadError::Failed),
        None => Err(ReloadError::Stopped),
    }
}

#[cfg(test)]
#[test]
fn a_reload_that_was_not_applied_is_an_error() {
    assert_eq!(reload_outcome(Some(true)), Ok(()));
    assert_eq!(reload_outcome(Some(false)), Err(ReloadError::Failed));
    assert_eq!(reload_outcome(None), Err(ReloadError::Stopped));
}

/// Why [`CoreHandle::reload_kubeconfig`] did not apply a reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReloadError {
    #[error("the core stopped before it reloaded kubeconfig")]
    Stopped,
    #[error("the kubeconfig files could not be loaded")]
    Failed,
}

/// The full cluster list. Sent whole whenever kubeconfig is (re)loaded.
#[derive(Debug, Clone, Default)]
pub struct ClustersSnapshot {
    pub clusters: Vec<ClusterSummary>,
    /// Kubeconfig files that could not be read or parsed. The error text may quote the file;
    /// show it through [`crate::redact::safe_error_summary`].
    pub errors: Vec<(PathBuf, String)>,
    /// Every kubeconfig path considered, and whether it existed when it was loaded.
    pub paths: Vec<(PathBuf, bool)>,
}

/// Notifications from the core. Snapshots are shared and read-only.
#[derive(Debug, Clone)]
pub enum Event {
    ClustersChanged(Arc<ClustersSnapshot>),
    /// Each cluster's keep-watching toggle, when any of them changed.
    KeepWatchingChanged(
        Arc<std::collections::BTreeMap<ClusterId, crate::keep_watching::KeepWatchView>>,
    ),
    ActiveChanged(Option<ClusterId>),
    ConnectionChanged {
        cluster: ClusterId,
        state: ConnectionState,
    },
    /// Nothing reviewed or confirmed on `cluster` before now may be written: its
    /// [`ClusterSummary::write_epoch`] moved to `write_epoch`.
    WritesEnded {
        cluster: ClusterId,
        write_epoch: u64,
    },
    /// The selected cluster's connection attempts, while it is not connected. `None` once there
    /// is nothing to show.
    ConnectionProgress(Option<ConnectionProgress>),
    /// Settings were loaded, reloaded, or written. When `settings.toml` cannot be read, `error`
    /// says why and `settings` are the defaults (nothing exposed), as the MCP server also treats
    /// an unreadable file as nothing exposed.
    SettingsChanged {
        settings: Arc<Settings>,
        error: Option<String>,
        /// The newest Trust or Stop Trusting request whose write has finished: settings events
        /// can be merged, so a front end tells by this that one it asked for is done.
        trust_settled: u64,
    },
    /// Discovery ran for a cluster: at connect, and again when its CRDs change. On failure,
    /// `error` is set and `discovery` holds the previous result, if any.
    DiscoveryChanged {
        cluster: ClusterId,
        /// The generation of the slot it was discovered for ([`ClusterSummary::generation`]):
        /// one for an older slot is stale.
        generation: u64,
        discovery: Option<Arc<ClusterDiscovery>>,
        error: Option<String>,
    },
    /// The agent activity log ([`crate::activity`]), at start and whenever `flint mcp` adds to
    /// it: every record kept, oldest first, for all clusters, with the keys its servers are keyed
    /// by (read with it, off the UI thread).
    AgentActivity(
        Arc<Vec<crate::activity::ActivityRecord>>,
        crate::activity::ServerKeys,
    ),
    /// A problem worth showing that is not tied to one cluster's connection.
    Error(String),
}

/// What [`CoreHandle::store_for_tests`] returns: keep it while the test reads the stores.
#[cfg(any(test, feature = "test-support"))]
pub struct StoredForTests(
    #[expect(dead_code, reason = "held, not read")] watch::Receiver<Arc<KindSnapshot>>,
);

/// How long [`CoreHandle::stored_snapshots`] waits for the watches nobody subscribes to, to
/// build their snapshots: a few of their 100 ms publish intervals.
pub const STORED_WAIT: Duration = Duration::from_millis(300);

/// Core work is mostly waiting on the network; blocking work (file reads, exec plugins) runs on
/// tokio's separate blocking pool.
const CORE_WORKER_THREADS: usize = 4;

/// The front end's handle to the core. Cheap to clone; the runtime stops when the last clone is
/// dropped.
#[derive(Clone)]
pub struct CoreHandle {
    commands: crate::core_queue::CoreQueue<Command>,
    requests: crate::core_queue::CoreQueue<Request>,
    events: crate::events::EventSource,
    hubs: HubRegistry,
    next_subscription: Arc<AtomicU64>,
    unlocks: SessionUnlocks,
    holds: crate::recheck::DefinitionHolds,
    log_budget: Arc<crate::logs::LogBudget>,
    telemetry: crate::telemetry::TelemetryBoard,
    kept: crate::manager::KeptGrants,
    disconnects: crate::manager::DisconnectCount,
    forwards: crate::manager::ForwardSignals,
    access: Arc<crate::access::AccessCache>,
    /// Set by [`Self::bound_to`]: one-off reads go only to this member.
    bound: Option<Arc<AdmittedMember>>,
    runtime: tokio::runtime::Handle,
    _runtime: Arc<RuntimeGuard>,
}

impl CoreHandle {
    /// The cached access checks ([`crate::access`]).
    pub(crate) fn access_cache(&self) -> Arc<crate::access::AccessCache> {
        self.access.clone()
    }

    /// Starts the tokio runtime on background threads and the cluster manager on it.
    pub fn start(config: CoreConfig) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(CORE_WORKER_THREADS)
            .thread_name("flint-core")
            .enable_all()
            .build()?;
        let (cmd_tx, cmd_rx) = crate::core_queue::CoreQueue::new();
        let (req_tx, req_rx) = crate::core_queue::CoreQueue::new();
        let (event_tx, event_rx) = crate::events::queue();
        let hubs = HubRegistry::default();
        let unlocks = SessionUnlocks::default();
        let helm_runs = crate::helm::runs_root(&config.config_dir);
        runtime.spawn_blocking(move || crate::helm::reap_stale(&helm_runs));
        let manager = ClusterManager::new(config, event_tx, hubs.clone(), unlocks.clone());
        let log_budget = manager.log_budget();
        let telemetry = manager.telemetry_board();
        let holds = manager.definition_holds();
        let kept = manager.kept_grants();
        let disconnects = manager.disconnect_count();
        let forwards = manager.forward_signals();
        runtime.spawn(manager.run(cmd_rx, req_rx));
        Ok(Self {
            commands: cmd_tx,
            requests: req_tx,
            events: event_rx,
            hubs,
            next_subscription: Arc::new(AtomicU64::new(1)),
            unlocks,
            holds,
            log_budget,
            telemetry,
            kept,
            disconnects,
            forwards,
            access: Arc::default(),
            bound: None,
            runtime: runtime.handle().clone(),
            _runtime: Arc::new(RuntimeGuard(Some(runtime))),
        })
    }

    /// Each member of the running set and its state, in kubeconfig order. Empty if the core
    /// stopped.
    pub fn running_set(&self) -> impl Future<Output = Vec<MemberReport>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.push(Request::RunningSet { reply });
        async move { rx.await.unwrap_or_default() }
    }

    /// Each cluster's keep-watching toggle. Empty if the core stopped.
    pub fn keep_watching(
        &self,
    ) -> impl Future<
        Output = std::collections::BTreeMap<ClusterId, crate::keep_watching::KeepWatchView>,
    > + Send
    + 'static {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.push(Request::KeepWatching { reply });
        async move { rx.await.unwrap_or_default() }
    }

    /// The first step of turning on `cluster`'s background sign-in toggle, for the definition the
    /// toggle showed ([`KeepWatchView::shown_sign_in`](crate::keep_watching::KeepWatchView)). A
    /// newer offer replaces an older one. `Err` says why it can't be turned on.
    pub fn offer_keep_watching(
        &self,
        cluster: &str,
        shown: u64,
    ) -> impl Future<Output = Result<crate::keep_watching::PluginOffer, String>> + Send + use<>
    {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.push(Request::OfferKeepWatching {
            cluster: cluster.to_owned(),
            shown,
            reply,
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err("Flint is shutting down.".to_owned()))
        }
    }

    /// Ends every keep-watching grant for the quit under way, before this returns and without
    /// the manager's queue. Resolves once their plugins are killed, after a short bounded wait
    /// at most, or at once if the core stopped.
    pub fn stop_keep_watching(&self) -> impl Future<Output = ()> + Send + use<> {
        self.forwards.quit();
        let wait = self.runtime.spawn(self.kept.quit());
        async move {
            let _ = wait.await;
        }
    }

    /// What the feed of `cluster` knows. `None` while it has none: it is neither kept watching,
    /// selected nor in the running set, is in watch mode On view with no overview showing, is not
    /// connected, is paused, or its sign-in runs a program and it is in the background without
    /// consent. A kept cluster that stopped or can't be reached shows its last values, with
    /// their age in the status. Never blocks.
    pub fn telemetry(&self, cluster: &str) -> Option<crate::telemetry::ClusterTelemetry> {
        self.telemetry.get(cluster)
    }

    /// [`Self::telemetry`] of `member`, only from a feed that started on the endpoint it was
    /// admitted on. Starts nothing and never blocks.
    pub fn member_telemetry(
        &self,
        member: &AdmittedMember,
    ) -> Option<crate::telemetry::ClusterTelemetry> {
        self.telemetry.get_on(member.context(), &member.at)
    }

    /// [`Self::telemetry`] of every cluster with a running feed, by cluster name.
    pub fn all_telemetry(&self) -> Vec<crate::telemetry::ClusterTelemetry> {
        self.telemetry.all()
    }

    /// Wakes when any cluster's telemetry changes, or a feed starts or stops.
    pub fn telemetry_updates(&self) -> crate::telemetry::TelemetryUpdates {
        self.telemetry.updates()
    }

    /// Opens a live list of `kind` on `cluster`. While the subscription lives, the kind is
    /// watched whenever the cluster is active, in both watch modes. The first
    /// [`Subscription::changed`] resolves at once; later ones at most about every 100 ms, however
    /// fast the cluster changes. Dropping the subscription closes it. Never blocks.
    pub fn subscribe(&self, cluster: &str, kind: &KindId) -> Subscription {
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        let (mut rx, request) = match self.bound.as_deref() {
            // Bound to a member: its own channel, which the core connects only while the member
            // holds, so it can't read what a watch of a replacement server publishes.
            Some(member) => {
                let empty = KindSnapshot::empty(cluster.to_owned(), kind.clone(), SyncState::Idle);
                let (feed, rx) = watch::channel(Arc::new(empty));
                let request = Request::SubscribeMember {
                    id,
                    cluster: cluster.to_owned(),
                    member: member.clone(),
                    kind: kind.clone(),
                    feed,
                };
                (rx, request)
            }
            None => {
                let (hub, rx) = self.hubs.subscribe(cluster, kind);
                // A running watch publishes what it held back while nobody listened.
                hub.wake.notify_one();
                let request = Request::Subscribe {
                    id,
                    cluster: cluster.to_owned(),
                    kind: kind.clone(),
                };
                (rx, request)
            }
        };
        rx.mark_changed();
        let _ = self.requests.push(request);
        Subscription {
            id,
            cluster: cluster.to_owned(),
            kind: kind.clone(),
            rx: Some(rx),
            requests: self.requests.clone(),
        }
    }

    /// Marks `cluster`'s overview as showing until the result is dropped, so its telemetry feed
    /// runs in watch mode On view too, while the cluster is active and not paused. Reads only.
    /// A handle bound to a member starts nothing. Never blocks.
    pub fn open_overview(&self, cluster: &str) -> OverviewInterest {
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        if matches!(self.read_binding(cluster), Ok(None)) {
            let _ = self.requests.push(Request::OpenOverview {
                id,
                cluster: cluster.to_owned(),
            });
        }
        OverviewInterest {
            id,
            requests: self.requests.clone(),
            bound: None,
        }
    }

    /// [`Self::open_overview`] for the slot `generation` of `cluster`. The core compares them as
    /// it registers, so a context re-pointed meanwhile registers nothing, and
    /// [`OverviewInterest::telemetry`] never reads the feed of a later slot.
    pub fn open_overview_on(&self, cluster: &str, generation: u64) -> OverviewInterest {
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        let (live, seen) = watch::channel(false);
        if matches!(self.read_binding(cluster), Ok(None)) {
            let _ = self.requests.push(Request::OpenOverviewOn {
                id,
                cluster: cluster.to_owned(),
                generation,
                live,
            });
        }
        OverviewInterest {
            id,
            requests: self.requests.clone(),
            bound: Some(BoundOverview {
                cluster: cluster.to_owned(),
                board: self.telemetry.clone(),
                live: seen,
            }),
        }
    }

    /// Classifies every pin and runs those that can join, in one step of the core. Too few
    /// leaves the running set as it was. Taken in the order sent, whether or not awaited.
    pub fn open_running_set(
        &self,
        pins: Vec<ClusterPin>,
    ) -> impl Future<Output = Result<OpenedSet, String>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.push(Request::OpenRunningSet { pins, reply });
        async move {
            if sent.is_err() {
                return Err("the core is not running".to_owned());
            }
            rx.await
                .unwrap_or_else(|_| Err("the core stopped before answering".to_owned()))
        }
    }

    /// Makes the running set exactly `pins`; none clears it. Every pin is checked against the
    /// context's confirmed server first: if any fails, no member joins or leaves and the error
    /// says which. Taken in the order sent, whether or not the future is awaited.
    pub fn admit_running_set(
        &self,
        pins: Vec<ClusterPin>,
    ) -> impl Future<Output = Result<Vec<AdmittedMember>, String>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.push(Request::AdmitRunningSet { pins, reply });
        async move {
            if sent.is_err() {
                return Err("the core is not running".to_owned());
            }
            rx.await
                .unwrap_or_else(|_| Err("the core stopped before answering".to_owned()))
        }
    }

    /// Why `member` no longer stands for its context, or `None`. A stopped core counts as changed.
    pub fn check_member(
        &self,
        member: &AdmittedMember,
    ) -> impl Future<Output = Option<String>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.push(Request::CheckMember {
            member: member.clone(),
            reply,
        });
        async move {
            if sent.is_err() {
                return Some("the core is not running".to_owned());
            }
            rx.await
                .unwrap_or_else(|_| Some("the core stopped before answering".to_owned()))
        }
    }

    /// This handle with what it starts bound to `member`: one-off reads, subscriptions, sessions
    /// and reviewed writes are refused if the context is no longer the definition admitted.
    pub fn bound_to(&self, member: AdmittedMember) -> Self {
        Self {
            bound: Some(Arc::new(member)),
            ..self.clone()
        }
    }

    /// How many times `cluster` was asked to disconnect, for a write to stamp when it starts.
    pub(crate) fn disconnects_of(&self, cluster: &str) -> u64 {
        self.disconnects.of(cluster)
    }

    /// Whether a write stamped with `asked` must stop ([`Self::disconnects_of`]).
    pub(crate) fn disconnect_ends_write(&self, cluster: &str, asked: u64) -> bool {
        self.disconnects.ends_write(cluster, asked)
    }

    /// Counts a disconnect of `cluster` as asked for, without sending it to the core.
    #[cfg(test)]
    pub(crate) fn ask_disconnect_for_test(&self, cluster: &str) {
        self.disconnects.bump(cluster);
    }

    pub(crate) fn is_bound(&self) -> bool {
        self.bound.is_some()
    }

    /// Refuses a write whose client, read at `pin`, is not on the definition this handle's
    /// member was admitted on. Asked before the credential is used.
    pub(crate) fn check_admitted(&self, cluster: &str, pin: &ContextPin) -> Result<(), WriteError> {
        match self.bound.as_deref() {
            Some(member) if !member.holds(cluster, pin) => Err(WriteError::ConnectionChanged),
            _ => Ok(()),
        }
    }

    /// The member a read of `cluster` is bound to; an error if this handle is bound to another.
    fn read_binding(&self, cluster: &str) -> Result<Option<AdmittedMember>, String> {
        match self.bound.as_deref() {
            Some(member) if member.context() != cluster => Err(format!(
                "\"{cluster}\" is not the cluster this view belongs to"
            )),
            bound => Ok(bound.cloned()),
        }
    }

    /// Fetches one object (managedFields removed, rendered as YAML too) and the events about
    /// it, for a detail view. The future works on any executor and fails if the cluster is not
    /// connected or does not serve `kind`.
    pub fn fetch_object(
        &self,
        cluster: &str,
        kind: &KindId,
        namespace: Option<&str>,
        name: &str,
    ) -> impl Future<Output = Result<Arc<ObjectDetail>, String>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let binding = self.read_binding(cluster);
        let refused = binding.as_ref().err().cloned();
        let sent = match binding {
            Ok(member) => self
                .requests
                .push(Request::FetchObject {
                    cluster: cluster.to_owned(),
                    kind: kind.clone(),
                    namespace: namespace.map(str::to_owned),
                    name: name.to_owned(),
                    member,
                    reply,
                })
                .map_err(drop),
            Err(_) => Err(()),
        };
        async move {
            if let Some(refused) = refused {
                return Err(refused);
            }
            if sent.is_err() {
                return Err("the core is not running".to_owned());
            }
            rx.await
                .unwrap_or_else(|_| Err("the core stopped before answering".to_owned()))
        }
    }

    /// Fetches one object and nothing else (no events), only while `cluster` is not paused and is
    /// still the definition `pin` was taken from. The core checks that as it takes the client, so
    /// a refused read sends no request. Starts no connection. Works on any executor.
    pub fn fetch_pinned_object(
        &self,
        cluster: &str,
        pin: &ReadPin,
        kind: &KindId,
        namespace: Option<&str>,
        name: &str,
    ) -> impl Future<Output = Result<Arc<ObjectDetail>, String>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.push(Request::FetchPinnedObject {
            cluster: cluster.to_owned(),
            pin: pin.clone(),
            kind: kind.clone(),
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
            reply,
        });
        async move {
            if sent.is_err() {
                return Err("the core is not running".to_owned());
            }
            rx.await
                .unwrap_or_else(|_| Err("the core stopped before answering".to_owned()))
        }
    }

    /// The cluster's current discovery results (navigation and kinds), if discovery has run.
    /// The same value also arrives as [`Event::DiscoveryChanged`]; this serves views that open
    /// later. Works on any executor.
    pub fn discovery(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Option<Arc<ClusterDiscovery>>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.push(Request::Discovery {
            cluster: cluster.to_owned(),
            reply,
        });
        async move { rx.await.ok().flatten() }
    }

    /// Activates `cluster` if it is still the slot `generation` with identity `pin`. The core
    /// compares them in the step that activates, so a reload cannot repoint the context between
    /// the check and the switch. Works on any executor.
    pub fn activate_pinned(
        &self,
        cluster: &str,
        pin: ReadPin,
        generation: u64,
    ) -> impl Future<Output = Result<(), ActivationRefused>> + Send + 'static + use<> {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.push(Request::ActivatePinned {
            cluster: cluster.to_owned(),
            pin,
            generation,
            reply,
        });
        async move { rx.await.unwrap_or(Err(ActivationRefused::Stopped)) }
    }

    /// Reloads the kubeconfig files now, and resolves once the reload is applied, or with why it
    /// was not: an acknowledgement that does not depend on the event stream or the file watcher.
    /// A file that fails to parse does not fail the reload: it is reported in the cluster list.
    /// Works on any executor.
    pub fn reload_kubeconfig(
        &self,
    ) -> impl Future<Output = Result<(), ReloadError>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.push(Request::ReloadKubeconfig { reply });
        async move { reload_outcome(rx.await.ok()) }
    }

    /// The kinds of `cluster` being watched (or polled) right now, sorted. For status displays
    /// ("watching 52 kinds") and tests. Works on any executor.
    pub fn watched_kinds(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Vec<(KindId, Feed)>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let _ = self.requests.push(Request::WatchedKinds {
            cluster: cluster.to_owned(),
            reply,
        });
        async move { rx.await.unwrap_or_default() }
    }

    /// The sync state of each kind of `cluster` that is watched, polled or subscribed right now,
    /// sorted by kind, read without subscribing (so it never starts a watch in On view mode).
    /// For status displays such as the navigation's per-kind markers. Takes a short lock and
    /// never waits on core work, so the UI thread may call it.
    pub fn sync_states(&self, cluster: &str) -> Vec<(KindId, SyncState)> {
        self.hubs.states(cluster)
    }

    /// The rows the watches of `cluster` hold in memory now, one snapshot per kind that has
    /// listed, for a search over every object (the resource switcher). Never reaches the API
    /// server and never starts a watch, in either watch mode: kinds not watched now are left
    /// out. Answers within about [`STORED_WAIT`]. Works on any executor; dropping the future
    /// stops the work.
    pub fn stored_snapshots(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Vec<Arc<KindSnapshot>>> + Send + 'static {
        let hubs = self.hubs.clone();
        let cluster = cluster.to_owned();
        let task = spawn_aborting(&self.runtime, async move {
            hubs.stored(&cluster, STORED_WAIT).await
        });
        async move { task.await.unwrap_or_default() }
    }

    /// Puts `snapshot` in its kind's store as a watch would, for front-end tests of what reads
    /// the stores without subscribing ([`CoreHandle::stored_snapshots`]). While the returned
    /// value is kept, the kind counts as subscribed, so reading it does not wait for a watch
    /// task to build a snapshot (there is none).
    #[cfg(any(test, feature = "test-support"))]
    pub fn store_for_tests(&self, snapshot: KindSnapshot) -> StoredForTests {
        let hub = self.hubs.get_or_create(&snapshot.cluster, &snapshot.kind);
        hub.take_over(snapshot);
        StoredForTests(hub.subscribe())
    }

    /// Runs [`view::compute`] (namespace filter, text filter, sort) on the core's threads, so a
    /// list of 10,000 rows never stalls the UI thread. Resolves to indexes into
    /// `snapshot.rows`. Works on any executor.
    pub fn compute_view(
        &self,
        snapshot: Arc<KindSnapshot>,
        query: ViewQuery,
    ) -> impl Future<Output = Arc<[u32]>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn_blocking(move || {
            let rows = view::compute(&snapshot, &query, crate::table::unix_now());
            let _ = tx.send(Arc::<[u32]>::from(rows));
        });
        async move { rx.await.unwrap_or_else(|_| Arc::from(Vec::new())) }
    }

    /// Opens a log session: streams the log of `subject` on `cluster` into a ring buffer of
    /// [`crate::Settings::log_buffer_limit`] lines and [`crate::Settings::log_buffer_bytes`]
    /// bytes, reconnecting when the stream drops. The
    /// session keeps running when another cluster becomes active, until the returned
    /// [`LogSession`] is dropped. Snapshots arrive at most about every 100 ms. Never blocks.
    ///
    /// `subject` is a pod's container ([`LogTarget`]) or a workload's pods
    /// ([`crate::WorkloadTarget`]): every container of every matching pod, merged into one
    /// buffer with each line tagged by pod and container, following pods as they come and go
    /// (see [`crate::PodSetStatus`] for the streams' states).
    pub fn open_logs(
        &self,
        cluster: &str,
        subject: impl Into<LogSubject>,
        options: LogOptions,
    ) -> LogSession {
        self.open_logs_with(cluster, None, subject.into(), options)
    }

    /// As [`CoreHandle::open_logs`], for the context's definition at `server`: the server the
    /// tab showing it is labelled with, as the cluster list gave it. A session for a server that
    /// is not the context's confirmed one (a fallback, or one the context has since left) ends
    /// at its first client lookup, so it never reads from another server (UP-06).
    pub fn open_logs_at(
        &self,
        cluster: &str,
        server: Option<&str>,
        subject: impl Into<LogSubject>,
        options: LogOptions,
    ) -> LogSession {
        self.open_logs_with(
            cluster,
            Some(server.map(str::to_owned)),
            subject.into(),
            options,
        )
    }

    fn open_logs_with(
        &self,
        cluster: &str,
        server: Option<Option<String>>,
        subject: LogSubject,
        options: LogOptions,
    ) -> LogSession {
        let (snapshots, mut rx) = watch::channel(Arc::new(LogSnapshot::connecting()));
        rx.mark_changed();
        let (paused, paused_rx) = watch::channel(false);
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        let budget = crate::logs::Budgeted::new(self.log_budget.clone(), id);
        let trim_cutoff = budget.cutoff();
        let _ = self.requests.push(Request::OpenLogs {
            id,
            cluster: cluster.to_owned(),
            server,
            member: self.bound.as_deref().cloned(),
            disconnects: self.disconnects.of(cluster),
            subject: subject.clone(),
            options: options.clone(),
            snapshots,
            paused: paused_rx,
            budget,
        });
        LogSession {
            id,
            cluster: cluster.to_owned(),
            subject,
            options,
            rx,
            paused,
            requests: self.requests.clone(),
            budget: self.log_budget.clone(),
            trim_cutoff,
        }
    }

    /// The memory budget all log sessions share (`log_total_mib`), for tests and diagnostics.
    pub fn log_budget(&self) -> &crate::logs::LogBudget {
        &self.log_budget
    }

    /// Brings `search` up to date with `lines` on the core's threads (see [`LogSearch`]). Use it
    /// for the first scan of a new query over a full buffer; later updates while following
    /// scan only new lines and are cheap enough to run anywhere. Works on any executor.
    pub fn search_logs(
        &self,
        mut search: LogSearch,
        lines: LogLines,
    ) -> impl Future<Output = LogSearch> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let query = search.query().clone();
        self.runtime.spawn_blocking(move || {
            search.update(&lines);
            let _ = tx.send(search);
        });
        async move { rx.await.unwrap_or_else(|_| LogSearch::new(query)) }
    }

    /// Brings `filter` up to date with `lines` on the core's threads (see [`LogFilter`]): for
    /// the first scan of a new filter over a full buffer. Later updates while following scan
    /// only new lines. Works on any executor.
    pub fn filter_logs(
        &self,
        mut filter: LogFilter,
        lines: LogLines,
    ) -> impl Future<Output = LogFilter> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let spec = filter.spec().clone();
        self.runtime.spawn_blocking(move || {
            filter.update(&lines);
            let _ = tx.send(filter);
        });
        async move { rx.await.unwrap_or_else(|_| LogFilter::new(spec)) }
    }

    /// Opens an exec session (a shell in a container, by default) on a connected cluster. The
    /// session runs on its own task: it keeps running when the active cluster changes, and ends
    /// when it is closed or dropped. When the cluster is not connected, it fails at once. Never
    /// blocks.
    pub fn exec(&self, request: ExecRequest) -> ExecSession {
        // Someone opened or restarted a shell: as a retry, it may run the cluster's resting exec
        // auth plugin again (the cached client may hold it back after a timed-out refresh).
        crate::auth_plugin::retry_now(&request.cluster);
        let (session, channels) = ExecSession::new(request.clone());
        let client = self.write_client(&request.cluster, request.server.clone());
        let requests = self.requests.clone();
        let cluster = request.cluster.clone();
        let admitted = self.bound.clone();
        self.runtime.spawn(async move {
            let client = client.await.and_then(|(client, pin)| match &admitted {
                Some(member) if !member.holds(&cluster, &pin) => Err(WriteRefusal::Stale {
                    cluster: cluster.to_string(),
                }),
                _ => Ok((client, pin)),
            });
            let client = client.map(|(client, pin)| {
                let launch = launch_check(requests.clone(), cluster.clone(), pin.clone());
                let check = context_check(requests, cluster, pin);
                (client, ContextWatch::new(check, launch, RECHECK_EVERY))
            });
            crate::exec::start(client, request, channels).await;
        });
        session
    }

    /// Starts a port-forward on a connected cluster, through the same gate as [`Self::exec`]. It
    /// listens on `127.0.0.1` only and ends when stopped or dropped, or when the gate refuses.
    /// Never blocks.
    pub fn port_forward(&self, request: ForwardRequest) -> PortForward {
        let (forward, channels) = PortForward::new(request.clone());
        let requests = self.requests.clone();
        let admitted = self.bound.clone();
        self.runtime.spawn(async move {
            let (reply, answer) = oneshot::channel();
            let asked = requests.send(Request::AdmitForward {
                cluster: request.cluster.clone(),
                server: request.server.clone(),
                admitted: admitted.clone(),
                reply,
            });
            let admission = async {
                asked
                    .await
                    .map_err(|_| WriteError::from(WriteRefusal::Stopped))?;
                answer.await.unwrap_or(Err(WriteRefusal::Stopped.into()))
            };
            let Some(admission) = channels.unless_stopped(admission).await else {
                channels.say_stopped();
                return;
            };
            match admission {
                Ok(grant) => {
                    let dialer = Arc::new(crate::port_forward::KubeDialer::new(
                        requests,
                        request.clone(),
                        grant,
                        admitted,
                    ));
                    crate::port_forward::run(dialer, request, channels).await;
                }
                Err(refusal) => crate::port_forward::refuse(&channels, &refusal),
            }
        });
        forward
    }

    /// What a `helm` run on `cluster` would be held to, in one step with checking it may run.
    fn helm_target(
        &self,
        cluster: &str,
        pin: HelmPin,
    ) -> impl Future<Output = Result<HelmTarget, HelmError>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.push(Request::HelmTarget {
            cluster: cluster.to_owned(),
            pin,
            member: self.bound.as_deref().cloned(),
            reply,
        });
        async move {
            if sent.is_err() {
                return Err(HelmError::Stopped);
            }
            rx.await.unwrap_or(Err(HelmError::Stopped))
        }
    }

    /// Pins a Helm view to `cluster`'s definition as of this call. It holds even if the cluster
    /// can't run `helm` yet: every read is checked against it, so a view never moves to a
    /// definition that came later.
    pub fn helm_pin(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Result<HelmPin, HelmError>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.push(Request::HelmPin {
            cluster: cluster.to_owned(),
            reply,
        });
        async move {
            if sent.is_err() {
                return Err(HelmError::Stopped);
            }
            rx.await.map_err(|_| HelmError::Stopped)
        }
    }

    /// Runs one read-only `helm` query on `cluster`, only while it is still the definition `pin`
    /// was taken on: checked before the run, throughout it, and before its answer is given. A
    /// change ends the run, and so does dropping the future. Works on any executor.
    pub fn helm_read(
        &self,
        cluster: &str,
        pin: &HelmPin,
        query: HelmQuery,
    ) -> impl Future<Output = Result<HelmOutput, HelmError>> + Send + 'static {
        let target = self.helm_target(cluster, pin.clone());
        let after = self.clone();
        let cluster = cluster.to_owned();
        let pin = pin.clone();
        let run = spawn_aborting(&self.runtime, async move {
            let target = target.await?;
            let output = tokio::select! {
                output = crate::helm::run(&target, &query) => output?,
                refused = after.helm_refused(&cluster, &pin) => return Err(refused),
            };
            after.helm_target(&cluster, pin).await?;
            Ok(output)
        });
        async move { run.await.unwrap_or(Err(HelmError::Stopped)) }
    }

    /// What a Helm view should warn of the `helm` a run at `pin` would start, if anything.
    pub fn helm_warning(
        &self,
        cluster: &str,
        pin: &HelmPin,
    ) -> impl Future<Output = Option<String>> + Send + 'static {
        let target = self.helm_target(cluster, pin.clone());
        let warning = spawn_aborting(&self.runtime, async move {
            crate::helm::warning(&target.await.ok()?).await
        });
        async move { warning.await.flatten() }
    }

    /// Resolves once a `helm` run on `cluster` at `pin` may no longer go on.
    async fn helm_refused(&self, cluster: &str, pin: &HelmPin) -> HelmError {
        loop {
            tokio::time::sleep(crate::helm::RECHECK_EVERY).await;
            if let Err(refused) = self.helm_target(cluster, pin.clone()).await {
                return refused;
            }
        }
    }

    /// The client for an action that changes `cluster` or runs code in it, with the context's
    /// pin: refused while the cluster is protected (ADR-0015) or not connected.
    /// Never blocks.
    pub(crate) fn write_client(
        &self,
        cluster: &str,
        server: Option<Option<String>>,
    ) -> impl Future<Output = Result<(GatedClient, ContextPin), WriteRefusal>> + Send + 'static
    {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.push(Request::WriteClient {
            cluster: cluster.to_owned(),
            server,
            reply,
        });
        async move {
            if sent.is_err() {
                return Err(WriteRefusal::Stopped);
            }
            rx.await.unwrap_or(Err(WriteRefusal::Stopped))
        }
    }

    /// [`Self::write_client`] for one request of a reviewed write, with the credential of the
    /// connection that client belongs to, read in the same step.
    pub(crate) fn bound_write_client(
        &self,
        cluster: &str,
        server: Option<Option<String>>,
    ) -> impl Future<Output = Result<BoundWriteClient, WriteError>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.requests.push(Request::BoundWriteClient {
            cluster: cluster.to_owned(),
            server,
            reply,
        });
        async move {
            if sent.is_err() {
                return Err(WriteRefusal::Stopped.into());
            }
            rx.await.unwrap_or(Err(WriteRefusal::Stopped.into()))
        }
    }

    /// Whether a write may go out on `pin`: the context still names its server, confirmed, on
    /// the pinned definition, and the gate allows changes there.
    pub(crate) fn check_write_pin(
        &self,
        cluster: &ClusterId,
        pin: ContextPin,
    ) -> impl Future<Output = Result<(), WriteError>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let requests = self.requests.clone();
        let cluster = cluster.clone();
        async move {
            requests
                .send(Request::CheckWritePin {
                    cluster,
                    pin,
                    reply,
                })
                .await
                .map_err(|_| WriteRefusal::Stopped)?;
            rx.await.unwrap_or(Err(WriteRefusal::Stopped.into()))
        }
    }

    /// Runs `task` on the core's runtime, for callers on another executor. Dropping the returned
    /// future aborts it.
    pub(crate) fn on_runtime<T: Send + 'static>(
        &self,
        task: impl Future<Output = T> + Send + 'static,
    ) -> impl Future<Output = Option<T>> + Send + 'static {
        spawn_aborting(&self.runtime, task)
    }

    /// Unlocks `cluster` at `server` (the URL the user confirmed) until Flint quits or
    /// [`CoreHandle::lock`] (ADR-0015). It takes effect at once. If the context now points
    /// elsewhere, the unlock does not cover it: the pin is to `server`.
    pub fn unlock(&self, cluster: &str, server: &str) {
        self.unlocks.unlock(cluster, server);
    }

    /// Locks `cluster` again (Lock Again). Sessions already running keep running; nothing new
    /// that changes the cluster starts. Returns whether it was unlocked.
    pub fn lock(&self, cluster: &str) -> bool {
        self.unlocks.lock(cluster)
    }

    /// Trusts `cluster` at `server` (the URL the user confirmed), written to `settings.toml`
    /// through [`Command::SetTrusted`]. A revocation of its trust earlier in the session is
    /// lifted once the core has checked the server and saved the pin, not before.
    pub fn trust(&self, cluster: &str, server: &str) {
        let request = self.unlocks.request_trust(cluster);
        self.send(Command::SetTrusted {
            cluster: cluster.to_owned(),
            server: Some(server.to_owned()),
            request,
        });
    }

    /// Stops trusting `cluster` and locks it: from this moment nothing new that changes the
    /// cluster starts, before `settings.toml` is written (commands and the core's gate take
    /// separate queues), and no Trust written before then lifts it. Shells already open keep
    /// running.
    ///
    /// Returns the request's number, which `Event::SettingsChanged` reports once it is done.
    pub fn untrust(&self, cluster: &str) -> u64 {
        let request = self.unlocks.revoke_trust(cluster);
        self.send(Command::SetTrusted {
            cluster: cluster.to_owned(),
            server: None,
            request,
        });
        request
    }

    /// The session's unlocks, for showing each cluster's [`crate::Protection`].
    pub fn unlocks(&self) -> &SessionUnlocks {
        &self.unlocks
    }

    /// Which contexts the core holds now, for showing. The write gate does not read this.
    pub fn definition_holds(&self) -> &crate::recheck::DefinitionHolds {
        &self.holds
    }

    /// Lists the pods of a workload (or a label selector) in `namespace` on a connected
    /// cluster, once and sorted by name, so a front end can open a shell in one of them
    /// ([`WorkloadPod::first_ready`]). [`Freshness::Cached`] reads the API server's cache, for
    /// showing; [`Freshness::Current`] is a quorum read, for acting. Never blocks; the future
    /// works on any executor and fails if the cluster is not connected, the workload is missing,
    /// the selector is empty or the list is refused. Dropping the future stops the request.
    pub fn workload_pods(
        &self,
        cluster: &str,
        namespace: &str,
        pods: PodSet,
        freshness: Freshness,
    ) -> impl Future<Output = Result<Vec<WorkloadPod>, String>> + Send + 'static {
        let client = self.read_client(cluster);
        let namespace = namespace.to_owned();
        let listed = spawn_aborting(&self.runtime, async move {
            crate::logs::list_workload_pods(client.await?, &namespace, &pods, freshness)
                .await
                .map_err(|e| e.message)
        });
        async move {
            listed
                .await
                .unwrap_or_else(|| Err("the core stopped before answering".to_owned()))
        }
    }

    /// The client for a one-off read of `cluster`, through the member this handle is bound to.
    fn read_client(
        &self,
        cluster: &str,
    ) -> impl Future<Output = Result<kube::Client, String>> + Send + 'static {
        let requests = self.requests.clone();
        let binding = self.read_binding(cluster);
        let cluster = cluster.to_owned();
        async move {
            const STOPPED: &str = "the core stopped before answering";
            const NOT_RUNNING: &str = "the core is not running";
            match binding? {
                Some(member) => {
                    let (reply, rx) = oneshot::channel();
                    requests
                        .push(Request::MemberClient { member, reply })
                        .map_err(|_| NOT_RUNNING.to_owned())?;
                    rx.await.unwrap_or_else(|_| Err(STOPPED.to_owned()))
                }
                None => {
                    let (reply, rx) = oneshot::channel();
                    requests
                        .push(Request::Client {
                            cluster: cluster.clone(),
                            reply,
                        })
                        .map_err(|_| NOT_RUNNING.to_owned())?;
                    rx.await
                        .map_err(|_| STOPPED.to_owned())?
                        .ok_or_else(|| format!("\"{cluster}\" is not connected"))
                }
            }
        }
    }

    /// Sends a command. Never blocks, and is never dropped for a full queue: commands are taken
    /// in the order sent ([`crate::core_queue`]). Commands sent after the core stopped are
    /// dropped.
    pub fn send(&self, command: Command) {
        // Ending a grant does not wait its turn behind a save the queue may be stuck on.
        if let Command::RevokeBackgroundSignIn(cluster)
        | Command::DisconnectCluster(cluster)
        | Command::SetKeepWatching {
            cluster,
            definition: None,
        } = &command
        {
            self.kept.revoke(cluster);
            self.forwards.end_cluster(cluster);
        }
        if let Command::DisconnectCluster(cluster) = &command {
            self.disconnects.bump(cluster);
        }
        if self.commands.push(command).is_err() {
            tracing::warn!("core is not running; command dropped");
        }
    }

    /// The event stream ([`crate::events`]): latest state per kind and cluster, and recent
    /// errors. Each event goes to one reader. The handle does not keep the stream open: once
    /// every reader is dropped, the core stops queueing events.
    pub fn events(&self) -> crate::events::Events {
        self.events.events()
    }
}

/// A live list of one kind on one cluster, from [`CoreHandle::subscribe`].
///
/// The core publishes immutable snapshots; this only ever holds the newest, so a slow reader
/// skips intermediate states instead of queueing them. Dropping it unsubscribes.
pub struct Subscription {
    id: u64,
    cluster: ClusterId,
    kind: KindId,
    /// `None` only while dropping, so the receiver is gone before the manager hears about it.
    rx: Option<watch::Receiver<Arc<KindSnapshot>>>,
    requests: crate::core_queue::CoreQueue<Request>,
}

impl Subscription {
    fn rx(&self) -> &watch::Receiver<Arc<KindSnapshot>> {
        self.rx.as_ref().expect("set until drop")
    }

    fn rx_mut(&mut self) -> &mut watch::Receiver<Arc<KindSnapshot>> {
        self.rx.as_mut().expect("set until drop")
    }

    /// The cluster this subscription lists.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    /// The kind this subscription lists.
    pub fn kind(&self) -> &KindId {
        &self.kind
    }

    /// Waits until a snapshot newer than the last one read with [`Subscription::latest`] is
    /// published. Returns `false` once the core has stopped. Works on any executor.
    pub async fn changed(&mut self) -> bool {
        self.rx_mut().changed().await.is_ok()
    }

    /// The newest snapshot, marking it as seen.
    pub fn latest(&mut self) -> Arc<KindSnapshot> {
        self.rx_mut().borrow_and_update().clone()
    }

    /// The newest snapshot, without marking it as seen.
    pub fn peek(&self) -> Arc<KindSnapshot> {
        self.rx().borrow().clone()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Drop the receiver first: the manager releases the hub only when nobody listens.
        self.rx = None;
        let _ = self.requests.push(Request::Unsubscribe { id: self.id });
    }
}

/// An overview on show, from [`CoreHandle::open_overview`]. Dropping it ends its watches.
#[derive(Debug)]
pub struct OverviewInterest {
    id: u64,
    requests: crate::core_queue::CoreQueue<Request>,
    /// Set by [`CoreHandle::open_overview_on`].
    bound: Option<BoundOverview>,
}

/// Where an overview opened on one slot reads its feed.
#[derive(Debug)]
struct BoundOverview {
    cluster: ClusterId,
    board: crate::telemetry::TelemetryBoard,
    live: watch::Receiver<bool>,
}

impl OverviewInterest {
    /// The feed of the slot this overview was opened on. `None` until the core registers it, and
    /// for good once that slot is replaced. The core clears the mark before it lists a later
    /// slot's feed, so the mark is read after the feed.
    pub fn telemetry(&self) -> Option<crate::telemetry::ClusterTelemetry> {
        let bound = self.bound.as_ref()?;
        let telemetry = bound.board.get(&bound.cluster);
        (*bound.live.borrow()).then_some(telemetry).flatten()
    }

    /// Resolves once the core has registered this overview, or will never do so.
    pub fn registered(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        let live = self.bound.as_ref().map(|bound| bound.live.clone());
        async move {
            if let Some(mut live) = live {
                let _ = live.wait_for(|registered| *registered).await;
            }
        }
    }
}

impl Drop for OverviewInterest {
    fn drop(&mut self) {
        let _ = self.requests.push(Request::CloseOverview { id: self.id });
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("cluster", &self.cluster)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A running log session, from [`CoreHandle::open_logs`]. Holds only the newest snapshot, so a
/// slow reader skips intermediate ones. Dropping it stops the session.
pub struct LogSession {
    id: u64,
    cluster: ClusterId,
    subject: LogSubject,
    options: LogOptions,
    rx: watch::Receiver<Arc<LogSnapshot>>,
    paused: watch::Sender<bool>,
    requests: crate::core_queue::CoreQueue<Request>,
    budget: Arc<crate::logs::LogBudget>,
    trim_cutoff: Arc<AtomicU64>,
}

impl LogSession {
    /// The cluster the session reads from, whichever cluster is active.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    /// What the session reads.
    pub fn subject(&self) -> &LogSubject {
        &self.subject
    }

    /// The pod container a pod session reads; `None` for a workload session.
    pub fn target(&self) -> Option<&LogTarget> {
        match &self.subject {
            LogSubject::Pod(target) => Some(target),
            LogSubject::Workload(_) => None,
        }
    }

    pub fn options(&self) -> &LogOptions {
        &self.options
    }

    /// Waits until a snapshot newer than the last one read with [`LogSession::latest`] is
    /// published. The first call resolves at once. Returns `false` once the core has stopped.
    /// Works on any executor.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    /// The newest snapshot, marking it as seen.
    pub fn latest(&mut self) -> Arc<LogSnapshot> {
        self.rx.borrow_and_update().clone()
    }

    /// The newest snapshot, without marking it as seen.
    pub fn peek(&self) -> Arc<LogSnapshot> {
        self.rx.borrow().clone()
    }

    /// A second reader of this session's snapshots, for a task that waits on them while the
    /// owner keeps the session (to pause it, and to stop it by dropping it). It starts with the
    /// newest snapshot unseen, and it does not keep the session running: once the session is
    /// dropped, [`LogUpdates::changed`] returns `false`.
    pub fn updates(&self) -> LogUpdates {
        let mut rx = self.rx.clone();
        rx.mark_changed();
        LogUpdates { rx }
    }

    /// Pauses (`true`) or resumes delivery. While paused, snapshots keep the lines shown when
    /// the session was paused and count the lines held back; the buffer keeps filling (and
    /// evicting), so nothing is lost unless it overflows. Resuming shows everything at once.
    pub fn set_paused(&self, paused: bool) {
        self.paused.send_replace(paused);
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }

    /// Whether this session's tab is the one shown. The shared log memory budget trims the
    /// sessions viewed least recently first and never the shown one (see
    /// [`crate::logs::LogBudget`]).
    pub fn set_shown(&self, shown: bool) {
        self.budget.set_shown(self.id, shown);
    }

    /// The first line the shared budget left this session. A snapshot older than the latest
    /// trim can still start before it, so a front end drops those rows before drawing them.
    /// Takes no lock. Once [`LogSession::set_shown`] has returned, no trim of this tab is under
    /// way, so the value read then holds for the frame drawn next.
    pub fn trim_cutoff(&self) -> u64 {
        self.trim_cutoff.load(Ordering::Acquire)
    }

    /// Whether [`LogSession::set_shown`] last said this tab is shown (once the core has started
    /// the session).
    pub fn is_shown(&self) -> bool {
        self.budget.is_shown(self.id)
    }
}

/// Reads a [`LogSession`]'s snapshots from elsewhere; see [`LogSession::updates`].
#[derive(Debug)]
pub struct LogUpdates {
    rx: watch::Receiver<Arc<LogSnapshot>>,
}

impl LogUpdates {
    /// Waits until a snapshot newer than the last one read with [`LogUpdates::latest`] is
    /// published. Returns `false` once the session is closed or the core has stopped. Works on
    /// any executor.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    /// The newest snapshot, marking it as seen.
    pub fn latest(&mut self) -> Arc<LogSnapshot> {
        self.rx.borrow_and_update().clone()
    }
}

impl Drop for LogSession {
    fn drop(&mut self) {
        let _ = self.requests.push(Request::CloseLogs { id: self.id });
    }
}

impl std::fmt::Debug for LogSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogSession")
            .field("cluster", &self.cluster)
            .field("subject", &self.subject)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

struct RuntimeGuard(Option<tokio::runtime::Runtime>);

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        // `shutdown_background` does not block, so dropping the handle is safe from any context,
        // including the UI thread and async tests.
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// Asks the cluster manager whether `cluster` changed since `pin`, for a session pinned there.
/// While the core is busy or stopped, the session goes on: it has its own client.
fn context_check(
    requests: crate::core_queue::CoreQueue<Request>,
    cluster: ClusterId,
    pin: ContextPin,
) -> ContextCheck {
    Arc::new(move || {
        let (reply, rx) = oneshot::channel();
        let sent = requests.push(Request::CheckContext {
            cluster: cluster.clone(),
            pin: pin.clone(),
            reply,
        });
        Box::pin(async move {
            if sent.is_err() {
                return None;
            }
            rx.await.ok().flatten()
        })
    })
}

/// Asks the cluster manager's write gate about `cluster` as pinned at `pin`, each time it is
/// called. With the core gone, the answer is a refusal.
pub(crate) fn launch_check(
    requests: crate::core_queue::CoreQueue<Request>,
    cluster: ClusterId,
    pin: ContextPin,
) -> LaunchCheck {
    Arc::new(move || {
        let requests = requests.clone();
        let cluster = cluster.clone();
        let pin = pin.clone();
        Box::pin(async move {
            let (reply, rx) = oneshot::channel();
            requests
                .send(Request::CheckWriteGate {
                    cluster,
                    pin,
                    reply,
                })
                .await
                .map_err(|_| WriteRefusal::Stopped)?;
            rx.await.unwrap_or(Err(WriteRefusal::Stopped))
        })
    })
}

/// Runs `task` on `runtime`. The returned future yields its output (`None` if the runtime
/// dropped it), and dropping that future aborts the task, so an answer nobody waits for any
/// more is not worked on.
fn spawn_aborting<T: Send + 'static>(
    runtime: &tokio::runtime::Handle,
    task: impl Future<Output = T> + Send + 'static,
) -> impl Future<Output = Option<T>> + Send + 'static {
    let mut task = crate::exec::AbortOnDrop(runtime.spawn(task));
    async move { (&mut task.0).await.ok() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn start(dir: &std::path::Path) -> CoreHandle {
        CoreHandle::start(CoreConfig {
            config_dir: dir.to_owned(),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: Vec::new(),
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        })
        .unwrap()
    }

    // #730: a flood of commands and requests loses nothing behind it. A settings change and a
    // write's gate check sent among them each arrive once, the settings change in the order sent.
    #[tokio::test]
    async fn a_flood_loses_no_settings_change_or_write_check() {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        let mut contents = String::from(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: https://127.0.0.1:1\nusers:\n- name: u\n  user:\n    token: t\ncontexts:\n",
        );
        for name in ["one", "two"] {
            contents += &format!("- name: {name}\n  context:\n    cluster: c\n    user: u\n");
        }
        std::fs::write(&kubeconfig, contents).unwrap();
        let config = dir.path().join("config");
        let core = CoreHandle::start(CoreConfig {
            config_dir: config.clone(),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: vec![kubeconfig],
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        })
        .unwrap();
        let events = core.events();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(events.recv().await, Ok(Event::ClustersChanged(_))) {}
        })
        .await
        .expect("the kubeconfig loads");

        let flood = |core: &CoreHandle| {
            for _ in 0..20_000 {
                core.send(Command::CancelConnection {
                    cluster: "nowhere".into(),
                    generation: 0,
                });
                drop(core.keep_watching());
            }
        };
        // Refused with an error naming the cluster: a marker in the event order.
        let marker = |cluster: &str| Command::SetAiExposed {
            cluster: cluster.into(),
            server: Some("https://elsewhere".into()),
            scope: Default::default(),
        };
        flood(&core);
        core.send(marker("one"));
        core.send(Command::SetAppearance(Appearance::Dark));
        let write = core.write_client("one", None);
        core.send(marker("two"));
        flood(&core);

        let answer = tokio::time::timeout(Duration::from_secs(30), write)
            .await
            .expect("the gate answers");
        // Answered by the gate (a protected, unconnected cluster), not lost with the core.
        assert!(
            matches!(&answer, Err(WriteRefusal::Protected { cluster }) if cluster == "one"),
            "{:?}",
            answer.map(|_| ())
        );

        let mut seen: Vec<String> = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            while !seen.iter().any(|e| e.contains("\"two\"")) {
                match events.recv().await.expect("the core runs") {
                    Event::Error(message) => seen.push(message),
                    Event::SettingsChanged { settings, .. }
                        if settings.appearance == Appearance::Dark =>
                    {
                        seen.push("dark".to_owned());
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("both markers arrive");
        let first = |want: &dyn Fn(&String) -> bool| seen.iter().position(want).unwrap();
        let one = first(&|e| e.contains("\"one\""));
        let dark = first(&|e| e == "dark");
        let two = first(&|e| e.contains("\"two\""));
        assert!(one < dark && dark < two, "{seen:?}");
        assert_eq!(seen.iter().filter(|e| e.contains("\"one\"")).count(), 1);
        assert_eq!(seen.iter().filter(|e| e.contains("\"two\"")).count(), 1);
        let on_disk = crate::settings::load_settings(&config).unwrap();
        assert_eq!(on_disk.appearance, Appearance::Dark);
    }

    // Quit returns only after waiting for a plugin still running, and no longer than its bound.
    #[tokio::test]
    async fn quitting_waits_for_a_running_plugin_within_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let _running = core.kept.track_for_tests();
        let began = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(2), core.stop_keep_watching())
            .await
            .expect("the wait is bounded");
        assert!(began.elapsed() >= crate::manager::QUIT_WAIT);
    }

    #[tokio::test]
    async fn a_subscription_resolves_at_once_and_releases_its_hub_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let mut sub = core.subscribe("nowhere", &KindId::pods());
        assert_eq!(sub.cluster(), "nowhere");
        tokio::time::timeout(Duration::from_secs(5), sub.changed())
            .await
            .expect("the first change resolves at once");
        assert_eq!(sub.latest().state, SyncState::Idle);
        assert_eq!(core.sync_states("nowhere").len(), 1);
        drop(sub);
        // The manager releases the hub once it hears about the drop.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !core.sync_states("nowhere").is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("hub released");
    }

    // covers UP-31: the stores are read through the hubs alone (`HubRegistry::stored`), which
    // hold no channel to the manager, so no watch can start.
    #[tokio::test]
    async fn stored_snapshots_read_without_subscribing() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        assert!(core.stored_snapshots("nowhere").await.is_empty());
        let _kept = core.store_for_tests(KindSnapshot {
            cluster: "nowhere".into(),
            kind: KindId::pods(),
            state: SyncState::Synced,
            ..Default::default()
        });
        let stored = core.stored_snapshots("nowhere").await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].kind, KindId::pods());
    }

    #[tokio::test]
    async fn requests_for_an_unknown_cluster_answer_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let err = core
            .fetch_object("nowhere", &KindId::pods(), Some("ns"), "p")
            .await
            .unwrap_err();
        assert!(err.contains("not connected"), "{err}");
        assert!(core.discovery("nowhere").await.is_none());
        assert!(core.watched_kinds("nowhere").await.is_empty());
    }

    // covers UP-44: opening or restarting a shell is a retry: it ends the rest of the cluster's
    // exec auth plugin, as Retry does.
    #[tokio::test]
    async fn a_shell_ends_the_rest_of_its_clusters_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let before = crate::auth_plugin::retry_epoch("shell-rest");
        let _session = core.exec(crate::exec::ExecRequest::shell("shell-rest", "ns", "p"));
        assert_eq!(crate::auth_plugin::retry_epoch("shell-rest"), before + 1);
    }

    // covers UP-47: a pod list nobody waits for any more stops: dropping the future aborts
    // the task doing the request.
    #[tokio::test]
    async fn dropping_a_spawned_request_aborts_it() {
        struct Flag(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Flag {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let (dropped, aborted) = tokio::sync::oneshot::channel();
        let flag = Flag(Some(dropped));
        let waiting = spawn_aborting(&tokio::runtime::Handle::current(), async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        // Not even polled once.
        drop(waiting);
        tokio::time::timeout(Duration::from_secs(5), aborted)
            .await
            .expect("the task was aborted, dropping what it held")
            .unwrap();

        let answered = spawn_aborting(&tokio::runtime::Handle::current(), async { 7 });
        assert_eq!(answered.await, Some(7));
    }

    #[tokio::test]
    async fn workload_pods_on_an_unknown_cluster_answer_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let err = core
            .workload_pods(
                "nowhere",
                "ns",
                PodSet::Selector("app=web".into()),
                Freshness::Current,
            )
            .await
            .unwrap_err();
        assert!(err.contains("not connected"), "{err}");
    }

    // covers UP-40
    #[tokio::test]
    async fn a_log_session_on_an_unknown_cluster_ends_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let target = LogTarget {
            namespace: "ns".into(),
            pod: "p".into(),
            container: None,
        };
        let mut session = core.open_logs("nowhere", target.clone(), LogOptions::default());
        assert_eq!(session.cluster(), "nowhere");
        assert_eq!(session.target(), Some(&target));
        assert_eq!(session.subject(), &LogSubject::Pod(target.clone()));
        assert!(session.options().follow);
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(session.changed().await, "core running");
                let snapshot = session.latest();
                if let crate::logs::LogState::Ended { error, .. } = &snapshot.state {
                    return error.clone();
                }
            }
        })
        .await
        .expect("the session ended");
        assert!(ended.unwrap().contains("nowhere"));
        session.set_paused(true);
        assert!(session.is_paused());
        // An ended session still answers, until it is dropped.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !session.latest().paused {
                assert!(session.changed().await);
            }
        })
        .await
        .expect("pause shown");
    }

    // covers UP-42: a view waits on a second reader while it keeps (and later drops) the session.
    #[tokio::test]
    async fn a_second_reader_follows_the_session_until_it_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let target = LogTarget {
            namespace: "ns".into(),
            pod: "p".into(),
            container: None,
        };
        let session = core.open_logs("nowhere", target, LogOptions::default());
        let mut updates = session.updates();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(updates.changed().await, "session open");
                if matches!(updates.latest().state, crate::logs::LogState::Ended { .. }) {
                    break;
                }
            }
        })
        .await
        .expect("the reader sees the session end");
        session.set_paused(true);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !updates.latest().paused {
                assert!(updates.changed().await);
            }
        })
        .await
        .expect("the reader sees the pause");
        drop(session);
        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            while updates.changed().await {}
        })
        .await;
        assert!(
            closed.is_ok(),
            "the reader ends once the session is dropped"
        );
    }

    #[tokio::test]
    async fn search_logs_runs_off_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let mut buffer = crate::logs::LogBuffer::new(10);
        buffer.push(crate::logs::LogLine::parse("needle", false));
        buffer.push(crate::logs::LogLine::parse("hay", false));
        let search = core
            .search_logs(
                LogSearch::new(crate::logs::SearchQuery::new("NEEDLE")),
                buffer.view(),
            )
            .await;
        assert_eq!(search.matches(), &[0]);
    }

    // covers UP-41
    #[tokio::test]
    async fn exec_on_a_cluster_that_is_not_connected_fails_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let mut session = core.exec(ExecRequest::shell("nowhere", "ns", "pod"));
        let state = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = session.latest().state.clone();
                if state.is_finished() {
                    return state;
                }
                assert!(session.changed().await);
            }
        })
        .await
        .expect("fails at once");
        assert_eq!(
            state,
            crate::exec::ExecState::Failed(crate::exec::ExecFailure::new(
                crate::exec::ExecFailureKind::Unreachable,
                "\"nowhere\" is not connected"
            ))
        );
    }

    #[tokio::test]
    async fn compute_view_runs_off_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let core = start(dir.path());
        let rows = core
            .compute_view(Arc::new(KindSnapshot::default()), ViewQuery::default())
            .await;
        assert!(rows.is_empty());
    }
}
