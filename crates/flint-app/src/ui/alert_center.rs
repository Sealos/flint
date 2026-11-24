//! Desktop notifications for problems across clusters (#210).
//!
//! [`AlertTracker`] feeds each cluster's telemetry and discovery to the alert engine and turns
//! what it decides into notifications. [`AlertCenter`] runs it in the app, reports what the OS
//! answered, and opens or investigates an alert.
//!
//! - **Reads nothing itself.** It uses what the feeds already hold, so a paused cluster, or one in
//!   watch mode On view, gets no request from here.
//! - **Fails closed.** A kind of problem whose API group discovery could not vouch for is
//!   unchecked: it neither raises nor resolves.
//! - **Pinned.** An alert keeps the identity of the cluster that raised it. Opening it after that
//!   context was removed or re-pointed is refused, whichever cluster is active now.
//! - **Investigate** only copies a prompt, through the exposure and scope checks in
//!   [`event_prompt`]. It writes nothing and starts no agent. It is in the app's menu for every
//!   alert kept, because a banner has no action button without a signed app, and is disabled
//!   there with its reason when no prompt can be made.
//! - **Per cluster.** A cluster's own choice overrides the global setting. A cluster with alerts
//!   off is not fed to the engine, and what it had on the way is not sent.

// module-tests: alert_center_tests.rs

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flint_core::alerts::{AlertConfig, AlertEngine, AlertEvent, ClusterSnapshot};
use flint_core::discovery::ClusterDiscovery;
use flint_core::handle::{ActivationRefused, ClusterSummary, ConnectionState, ReadPin};
use flint_core::problems::ProblemKind;
use flint_core::read::DiscoveredKinds;
use flint_core::settings::{Settings, WatchMode, load_settings};
use flint_core::telemetry::{ClusterTelemetry, FeedDiscovery, KeptCoverage, TelemetrySnapshot};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::*;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
#[cfg(any(test, feature = "automation"))]
use serde::Serialize;

use super::clusters::{ClustersModel, Discovered};
use super::workspace::SettingsDir;
use super::{menu_fit, registry};
use crate::notifications::investigate_prompt::{InvestigatePrompt, event_prompt};
use crate::notifications::{
    ACCEPTED_TEXT, DeliveryError, Notification, NotificationSender, clean, distinct_labels,
    platform_sender,
};

/// How many past alerts the app keeps to open from. The menu lists them all.
const MAX_RECENT: usize = 20;

/// How often a pass runs with no change, since the engine's cooldowns pass with time.
const PASS_EVERY: Duration = Duration::from_secs(15);

/// The shortest time between two passes, so a burst of feed changes is one pass.
const PASS_GAP: Duration = Duration::from_secs(1);

/// How long a cluster may have no feed before its history is dropped. A feed restarting on a
/// reconnect is back well within this, so it keeps its open alerts and does not raise them again.
const UNWATCHED_GRACE: SignedDuration = SignedDuration::from_secs(30);

/// How long the button says the prompt was copied.
const NOTE_TIME: Duration = Duration::from_secs(4);

/// What the setting promises, shown beside it.
pub const COVERAGE: &str =
    "Checks connected clusters with active watches and clusters actively kept watching.";

/// What it does not cover.
pub const NOT_COVERED: &str = "Other clusters have no background detection.";

/// Every kind of problem.
const ALL_KINDS: [ProblemKind; 6] = ProblemKind::ALL;

/// Turn desktop notifications on or off.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleDesktopNotifications;

/// Open the exact cluster that raised an alert.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenAlert {
    pub tag: String,
}

/// Open the cluster that raised an alert and copy a read-only prompt for an agent.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct InvestigateAlert {
    pub tag: String,
}

/// What a cluster does about alerts, whatever the global setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterAlertChoice {
    #[default]
    Default,
    On,
    Off,
}

impl ClusterAlertChoice {
    fn stored(self) -> Option<bool> {
        match self {
            Self::Default => None,
            Self::On => Some(true),
            Self::Off => Some(false),
        }
    }

    fn of(stored: Option<bool>) -> Self {
        match stored {
            None => Self::Default,
            Some(true) => Self::On,
            Some(false) => Self::Off,
        }
    }
}

/// Choose whether one cluster posts desktop notifications.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SetClusterAlerts {
    pub cluster: String,
    pub choice: ClusterAlertChoice,
}

/// The identity of the cluster that raised an alert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertOrigin {
    pin: ReadPin,
    generation: u64,
}

impl AlertOrigin {
    pub fn of(summary: &ClusterSummary) -> Self {
        Self {
            pin: summary.read_pin(),
            generation: summary.generation,
        }
    }

    fn holds(&self, summary: &ClusterSummary) -> bool {
        *self == Self::of(summary)
    }
}

/// Why an alert can no longer be acted on, or the cluster that raised it.
pub fn check_origin<'a>(
    context: &str,
    origin: &AlertOrigin,
    clusters: &'a [ClusterSummary],
    label: &str,
) -> Result<&'a ClusterSummary, String> {
    let Some(summary) = clusters.iter().find(|c| c.info.name == context) else {
        return Err(refusal_text(ActivationRefused::Removed, label));
    };
    if !origin.holds(summary) {
        return Err(refusal_text(ActivationRefused::Changed, label));
    }
    if summary.paused {
        return Err(refusal_text(ActivationRefused::Paused, label));
    }
    Ok(summary)
}

/// What to tell the person when an alert's cluster was not opened.
fn refusal_text(refused: ActivationRefused, label: &str) -> String {
    let label = clean(label);
    match refused {
        ActivationRefused::Removed => {
            format!("\"{label}\" was removed from kubeconfig, so this alert cannot be opened.")
        }
        ActivationRefused::Changed => {
            format!("\"{label}\" changed since this alert was raised, so it was not opened.")
        }
        ActivationRefused::Paused => {
            format!(
                "\"{label}\" is paused until its kubeconfig is confirmed, so it was not opened."
            )
        }
        ActivationRefused::Stopped => {
            format!("\"{label}\" was not opened because Flint's core stopped.")
        }
    }
}

/// The catalog that may vouch for a cluster's kinds: from its current slot, and only if the last
/// discovery succeeded. After a failure the event still carries the previous result.
fn verified_discovery(found: Option<&Discovered>, generation: u64) -> Option<&ClusterDiscovery> {
    found
        .filter(|d| d.generation == generation && d.error.is_none())
        .and_then(|d| d.discovery.as_deref())
}

/// Sends `notifications` in order, stopping as soon as `live` says they were cancelled and
/// skipping those of a cluster `muted` says has alerts off now.
fn send_batch(
    sender: &dyn NotificationSender,
    notifications: &[Notification],
    live: impl Fn() -> bool,
    muted: impl Fn(&str) -> bool,
) -> Vec<(String, Result<(), DeliveryError>)> {
    let mut answers = Vec::with_capacity(notifications.len());
    for notification in notifications {
        if !live() {
            break;
        }
        if muted(&notification.cluster) {
            continue;
        }
        answers.push((notification.tag.clone(), sender.send(notification)));
    }
    answers
}

/// What is known about a cluster in one pass.
#[derive(Clone, Copy, Debug)]
pub enum Reading<'a> {
    /// Its feed is running.
    Feed {
        snapshot: &'a TelemetrySnapshot,
        discovery: Option<&'a ClusterDiscovery>,
    },
    /// Its feed runs on a kept-watching connection, whose own discovery vouches for its kinds.
    Kept {
        snapshot: &'a TelemetrySnapshot,
        coverage: KeptCoverage,
    },
    /// It should be readable and is not: its connection failed or it is paused.
    Lost,
    /// Flint is not keeping it watched.
    NotWatched,
    /// Its alerts are turned off.
    Muted,
}

/// What `summary`'s state and feed say about it.
pub fn reading<'a>(
    summary: &ClusterSummary,
    mode: WatchMode,
    snapshot: Option<&'a TelemetrySnapshot>,
    discovery: Option<&'a ClusterDiscovery>,
) -> Reading<'a> {
    if summary.paused {
        return Reading::Lost;
    }
    if mode != WatchMode::All {
        return Reading::NotWatched;
    }
    if let Some(snapshot) = snapshot {
        return Reading::Feed {
            snapshot,
            discovery,
        };
    }
    match summary.state {
        ConnectionState::Connecting
        | ConnectionState::Failed(_)
        | ConnectionState::AwaitingConfirmation => Reading::Lost,
        ConnectionState::Connected(_) | ConnectionState::Disconnected => Reading::NotWatched,
    }
}

/// [`reading`] of `summary` from `feed`, if that feed was started for the slot `summary` describes.
/// A feed of an earlier slot can still be listed when a new summary arrives, and what it read is
/// about another server.
fn feed_reading<'a>(
    summary: &ClusterSummary,
    mode: WatchMode,
    feed: Option<&'a ClusterTelemetry>,
    slot: Option<&'a ClusterDiscovery>,
) -> Reading<'a> {
    let feed = feed.filter(|feed| feed.generation == summary.generation);
    let snapshot = feed.map(|feed| &*feed.snapshot);
    match (reading(summary, mode, snapshot, slot), feed) {
        (
            Reading::Feed { snapshot, .. },
            Some(ClusterTelemetry {
                discovery: FeedDiscovery::Kept(coverage),
                ..
            }),
        ) => Reading::Kept {
            snapshot,
            coverage: *coverage,
        },
        (reading, _) => reading,
    }
}

/// The kinds of problem discovery cannot vouch for: all of them until it has run.
pub fn out_of_scope_kinds(discovery: Option<&ClusterDiscovery>) -> Vec<ProblemKind> {
    let Some(discovery) = discovery else {
        return ALL_KINDS.to_vec();
    };
    let incomplete = |group: &str| {
        discovery.incomplete_groups.iter().any(|name| {
            name.eq_ignore_ascii_case(group)
                || (group.is_empty() && name.eq_ignore_ascii_case("core"))
        })
    };
    ALL_KINDS
        .into_iter()
        .filter(|kind| kind.api_groups().iter().any(|group| incomplete(group)))
        .collect()
}

/// Clusters with alerts off now, and how often each was turned off, shared with batches being
/// sent. A batch stays cancelled for a cluster turned off since it started, even if back on.
#[derive(Debug, Default)]
struct MutedClusters {
    now: HashSet<String>,
    turned_off: HashMap<String, u64>,
}

impl MutedClusters {
    /// Makes `muted` the clusters with alerts off. Returns those just turned off, and whether
    /// any came back on.
    fn set(&mut self, muted: HashSet<String>) -> (Vec<String>, bool) {
        let newly: Vec<String> = muted.difference(&self.now).cloned().collect();
        let back_on = self.now.difference(&muted).next().is_some();
        for cluster in &newly {
            *self.turned_off.entry(cluster.clone()).or_default() += 1;
        }
        self.now = muted;
        (newly, back_on)
    }

    /// Whether a batch that started at `since` must not send for `cluster`.
    fn blocks(&self, cluster: &str, since: &HashMap<String, u64>) -> bool {
        self.now.contains(cluster) || self.turned_off.get(cluster) != since.get(cluster)
    }
}

/// One cluster's input to a pass.
#[derive(Clone, Debug)]
pub struct TrackerInput<'a> {
    pub cluster: Arc<str>,
    pub origin: AlertOrigin,
    /// What the notification calls it ([`flint_core::display::ClusterNames::with_real`]).
    pub label: String,
    pub reading: Reading<'a>,
}

/// What the OS last said about a notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Not answered yet.
    Sending,
    /// Held until the OS answers the permission prompt.
    AwaitingPermission,
    /// The OS accepted it, which does not prove it was shown.
    Accepted,
    /// Alerts were turned off before an answer arrived.
    Cancelled,
    Refused(DeliveryError),
}

impl Delivery {
    fn text(&self) -> String {
        match self {
            Self::Sending => "Sending the last notification…".to_owned(),
            Self::AwaitingPermission => "Waiting for permission to notify…".to_owned(),
            Self::Accepted => ACCEPTED_TEXT.to_owned(),
            Self::Cancelled => "Not sent: alerts were turned off.".to_owned(),
            Self::Refused(error) => error.message(),
        }
    }

    #[cfg(any(test, feature = "automation"))]
    fn state(&self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::AwaitingPermission => "awaiting_permission",
            Self::Accepted => "accepted",
            Self::Cancelled => "cancelled",
            Self::Refused(DeliveryError::NotPermitted) => "not_permitted",
            Self::Refused(DeliveryError::NeedsAppBundle) => "needs_app_bundle",
            Self::Refused(DeliveryError::Unanswered) => "unknown",
            Self::Refused(DeliveryError::Failed(_)) => "failed",
        }
    }
}

/// An alert the app can still open.
#[derive(Clone, Debug)]
pub struct RecentAlert {
    pub notification: Notification,
    pub event: AlertEvent,
    pub origin: AlertOrigin,
    pub delivery: Delivery,
}

/// Decides what to notify about from each pass; see the [module docs](self).
#[derive(Debug)]
pub struct AlertTracker {
    engine: AlertEngine,
    origins: HashMap<Arc<str>, AlertOrigin>,
    unwatched_since: HashMap<Arc<str>, Timestamp>,
    /// Clusters whose last pass could check every kind of problem.
    checked: BTreeSet<Arc<str>>,
    recent: VecDeque<RecentAlert>,
    /// Unique to this process, so a notification left from an earlier run matches nothing.
    run: String,
    next: u64,
}

impl AlertTracker {
    pub fn new(run: String) -> Self {
        Self {
            engine: AlertEngine::new(AlertConfig::default()),
            origins: HashMap::new(),
            unwatched_since: HashMap::new(),
            checked: BTreeSet::new(),
            recent: VecDeque::new(),
            run,
            next: 0,
        }
    }

    /// Forgets every cluster, so turning notifications on again starts from a new baseline
    /// rather than a backlog. What was still on its way is cancelled.
    pub fn reset(&mut self) {
        self.engine = AlertEngine::new(AlertConfig::default());
        self.origins.clear();
        self.unwatched_since.clear();
        self.checked.clear();
        for recent in &mut self.recent {
            if matches!(
                recent.delivery,
                Delivery::Sending | Delivery::AwaitingPermission
            ) {
                recent.delivery = Delivery::Cancelled;
            }
        }
    }

    fn forget(&mut self, cluster: &str) {
        self.engine.forget(cluster);
        self.origins.remove(cluster);
        self.unwatched_since.remove(cluster);
        self.checked.remove(cluster);
    }

    /// Drops the alerts of contexts that left kubeconfig, whether or not they were still tracked.
    /// Returns whether any went.
    fn purge_removed(&mut self, present: impl Fn(&str) -> bool) -> bool {
        let before = self.recent.len();
        self.recent.retain(|r| present(&r.notification.cluster));
        self.recent.len() != before
    }

    /// Stops following `cluster`, so turning it on again starts from a new baseline. Its alerts
    /// stay listed, and what was still on its way is cancelled.
    fn mute(&mut self, cluster: &str) {
        self.forget(cluster);
        for recent in &mut self.recent {
            if &*recent.notification.cluster == cluster
                && matches!(
                    recent.delivery,
                    Delivery::Sending | Delivery::AwaitingPermission
                )
            {
                recent.delivery = Delivery::Cancelled;
            }
        }
    }

    /// Applies one pass and returns what to send.
    pub fn observe(&mut self, inputs: &[TrackerInput<'_>], now: Timestamp) -> Vec<Notification> {
        let gone: Vec<Arc<str>> = self
            .origins
            .keys()
            .filter(|known| !inputs.iter().any(|input| input.cluster == **known))
            .cloned()
            .collect();
        for cluster in gone {
            self.forget(&cluster);
        }
        self.purge_removed(|cluster| inputs.iter().any(|i| &*i.cluster == cluster));
        let mut sent = Vec::new();
        for input in inputs {
            // A context that now means another cluster shares no history with the old one.
            if self
                .origins
                .get(&input.cluster)
                .is_some_and(|origin| *origin != input.origin)
            {
                self.forget(&input.cluster);
            }
            let events = match input.reading {
                Reading::Muted => {
                    self.mute(&input.cluster);
                    continue;
                }
                Reading::NotWatched => {
                    self.note_unwatched(&input.cluster, now);
                    continue;
                }
                Reading::Lost => {
                    self.unwatched_since.remove(&input.cluster);
                    self.checked.remove(&input.cluster);
                    self.engine
                        .observe(&input.cluster, now, ClusterSnapshot::Unavailable)
                }
                Reading::Feed { snapshot, .. } | Reading::Kept { snapshot, .. } => {
                    self.unwatched_since.remove(&input.cluster);
                    let out_of_scope = match input.reading {
                        Reading::Kept { coverage, .. } => coverage.unchecked(),
                        Reading::Feed { discovery, .. } => out_of_scope_kinds(discovery),
                        _ => ALL_KINDS.to_vec(),
                    };
                    let mut unchecked = snapshot.unchecked.clone();
                    for kind in out_of_scope {
                        if !unchecked.contains(&kind) {
                            unchecked.push(kind);
                        }
                    }
                    let readable =
                        matches!(snapshot.alert_snapshot(), ClusterSnapshot::Available { .. });
                    if readable && unchecked.is_empty() {
                        self.checked.insert(Arc::clone(&input.cluster));
                    } else {
                        self.checked.remove(&input.cluster);
                    }
                    let pass = match snapshot.alert_snapshot() {
                        ClusterSnapshot::Available { problems, .. } => ClusterSnapshot::Available {
                            problems,
                            unchecked: &unchecked,
                        },
                        ClusterSnapshot::Unavailable => ClusterSnapshot::Unavailable,
                    };
                    self.engine.observe(&input.cluster, now, pass)
                }
            };
            let events = match events {
                Ok(events) => events,
                Err(limit) => {
                    self.checked.remove(&input.cluster);
                    tracing::warn!("desktop notifications: {limit}");
                    continue;
                }
            };
            // A cluster the engine did not admit holds no entry here either.
            if self.engine.tracks(&input.cluster) {
                self.origins
                    .insert(Arc::clone(&input.cluster), input.origin.clone());
            }
            for event in events {
                sent.push(self.record(event, input));
            }
        }
        sent
    }

    /// A cluster with no feed is unreadable at once, so its groups are unknown and no cooldown
    /// runs. Its history is dropped once that has lasted [`UNWATCHED_GRACE`].
    fn note_unwatched(&mut self, cluster: &Arc<str>, now: Timestamp) {
        self.checked.remove(cluster);
        if !self.origins.contains_key(cluster) {
            return;
        }
        self.engine.mark_unknown(cluster);
        let since = *self
            .unwatched_since
            .entry(Arc::clone(cluster))
            .or_insert(now);
        if now.duration_since(since) >= UNWATCHED_GRACE {
            self.forget(cluster);
        }
    }

    fn record(&mut self, event: AlertEvent, input: &TrackerInput<'_>) -> Notification {
        self.next += 1;
        let tag = format!("flint-alert-{}-{}", self.run, self.next);
        let notification = Notification::from_event(&event, &input.label).tagged(tag);
        if self.recent.len() == MAX_RECENT {
            // A raised alert is the one that can be investigated, so it is kept the longest.
            let oldest = self
                .recent
                .iter()
                .position(|r| !matches!(r.event, AlertEvent::Raised(_)))
                .unwrap_or(0);
            self.recent.remove(oldest);
        }
        self.recent.push_back(RecentAlert {
            notification: notification.clone(),
            event,
            origin: input.origin.clone(),
            delivery: Delivery::Sending,
        });
        notification
    }

    pub fn find(&self, tag: &str) -> Option<&RecentAlert> {
        self.recent.iter().find(|r| r.notification.tag == tag)
    }

    /// Records what the OS answered.
    pub fn delivered(&mut self, tag: &str, delivery: Delivery) {
        if let Some(recent) = self.recent.iter_mut().find(|r| r.notification.tag == tag) {
            recent.delivery = delivery;
        }
    }

    /// Newest first.
    pub fn recent(&self) -> impl Iterator<Item = &RecentAlert> {
        self.recent.iter().rev()
    }
}

/// What the automation channel reports.
#[cfg(any(test, feature = "automation"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlertsState {
    pub enabled: bool,
    /// Clusters whose own choice or the global setting has alerts off.
    pub muted: Vec<String>,
    /// What the OS last answered: `none`, `sending`, `awaiting_permission`, `accepted`,
    /// `cancelled`, `not_permitted`, `needs_app_bundle`, `unknown` or `failed`.
    pub delivery: &'static str,
    /// Clusters whose last pass could check every kind of problem.
    pub checked: Vec<String>,
    pub recent: Vec<AlertRowState>,
}

/// One alert in [`AlertsState`].
#[cfg(any(test, feature = "automation"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlertRowState {
    pub tag: String,
    pub cluster: String,
    pub title: String,
    pub body: String,
    pub delivery: &'static str,
}

/// The running notifications of the app.
pub struct AlertCenter {
    model: Entity<ClustersModel>,
    sender: Arc<dyn NotificationSender>,
    tracker: AlertTracker,
    /// Whether any cluster may alert.
    enabled: bool,
    /// The global setting, which a cluster's own choice overrides.
    global: bool,
    /// Clusters with alerts off, shared with a batch being sent.
    muted: Arc<Mutex<MutedClusters>>,
    /// An overview to show once the workspace is on this cluster.
    overview: Option<AlertOverview>,
    /// What the OS last answered, from turning on or from a notification.
    delivery: Option<Delivery>,
    /// Changes at each toggle: work started under an older value is cancelled.
    session: Arc<AtomicU64>,
    /// The OS has not answered the permission request yet, so nothing can be sent.
    awaiting_permission: bool,
    /// What arose meanwhile, oldest first and at most [`MAX_RECENT`].
    held: VecDeque<Notification>,
    note: Option<&'static str>,
    _note: Task<()>,
    _tasks: Vec<Task<()>>,
    _observe: Subscription,
}

/// The overview an opened alert asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertOverview {
    pub context: String,
    pub generation: u64,
}

/// What follows a successful activation.
enum AfterActivation {
    ShowOverview,
    /// The prompt is built only then, from the settings as saved by that time.
    CopyPrompt(Box<RecentAlert>),
}

/// The app's [`AlertCenter`].
pub struct Alerts(pub Entity<AlertCenter>);

impl Global for Alerts {}

impl AlertCenter {
    pub fn new(
        model: Entity<ClustersModel>,
        sender: Arc<dyn NotificationSender>,
        cx: &mut Context<Self>,
    ) -> Self {
        let run = format!(
            "{}-{}",
            std::process::id(),
            Timestamp::now().as_millisecond()
        );
        let observe = cx.observe(&model, |this, model, cx| {
            // At once, and with alerts off too: no pass may come to clear a removed context.
            let clusters = &model.read(cx).clusters;
            if this
                .tracker
                .purge_removed(|id| clusters.iter().any(|c| c.info.name == id))
            {
                cx.notify();
            }
            this.sync_enabled(cx);
        });
        let mut updates = model.read(cx).core().telemetry_updates();
        let on_change = cx.spawn(async move |this, cx| {
            while updates.changed().await {
                if this.update(cx, |this, cx| this.pass(cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(PASS_GAP).await;
            }
        });
        let on_timer = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PASS_EVERY).await;
                if this.update(cx, |this, cx| this.pass(cx)).is_err() {
                    return;
                }
            }
        });
        let mut this = Self {
            model,
            sender,
            tracker: AlertTracker::new(run),
            enabled: false,
            global: false,
            muted: Arc::default(),
            overview: None,
            delivery: None,
            session: Arc::new(AtomicU64::new(0)),
            awaiting_permission: false,
            held: VecDeque::new(),
            note: None,
            _note: Task::ready(()),
            _tasks: vec![on_change, on_timer],
            _observe: observe,
        };
        this.sync_enabled(cx);
        this
    }

    /// Follows the setting. Turning on asks the OS for permission and reports its answer.
    fn sync_enabled(&mut self, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let readable = model.settings_error.is_none();
        let global = model.settings.desktop_notifications && readable;
        let muted: HashSet<String> = model
            .clusters
            .iter()
            .filter(|c| {
                !readable
                    || !model
                        .settings
                        .alerts_on(&c.info.name, c.info.server.as_deref())
            })
            .map(|c| c.info.name.clone())
            .collect();
        let enabled = global || (readable && muted.len() < model.clusters.len());
        if global != self.global {
            self.global = global;
            cx.notify();
        }
        let (newly_muted, back_on) = self.muted.lock().expect("the muted set").set(muted);
        if back_on || !newly_muted.is_empty() {
            cx.notify();
        }
        for cluster in &newly_muted {
            self.tracker.mute(cluster);
            self.held.retain(|n| &*n.cluster != cluster.as_str());
        }
        if enabled == self.enabled {
            // Its baseline is what it shows now, so a failure after this still alerts.
            if back_on {
                self.pass(cx);
            }
            return;
        }
        self.enabled = enabled;
        let session = self.session.fetch_add(1, Ordering::SeqCst) + 1;
        self.tracker.reset();
        self.held.clear();
        self.awaiting_permission = enabled;
        self.delivery = None;
        cx.notify();
        if !enabled {
            return;
        }
        let sender = Arc::clone(&self.sender);
        cx.spawn(async move |this, cx| {
            let answer = cx.background_spawn(async move { sender.prepare() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.session.load(Ordering::SeqCst) == session {
                    this.permission_answered(answer, cx);
                }
            });
        })
        .detach();
        self.pass(cx);
    }

    /// Sends what waited for the permission prompt, or records why it cannot be sent.
    fn permission_answered(&mut self, answer: Result<(), DeliveryError>, cx: &mut Context<Self>) {
        self.awaiting_permission = false;
        let held: Vec<Notification> = self
            .held
            .drain(..)
            .filter(|n| self.tracker.find(&n.tag).is_some())
            .collect();
        match answer {
            Ok(()) if held.is_empty() => {
                self.delivery = None;
            }
            Ok(()) => {
                for notification in &held {
                    self.tracker.delivered(&notification.tag, Delivery::Sending);
                }
                self.submit(held, cx);
            }
            Err(error) => {
                for notification in &held {
                    self.tracker
                        .delivered(&notification.tag, Delivery::Refused(error.clone()));
                }
                self.delivery = Some(Delivery::Refused(error));
            }
        }
        cx.notify();
    }

    /// One pass over every cluster. Reads only what the feeds and the model already hold.
    pub fn pass(&mut self, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        let model = self.model.read(cx);
        let names = model.names();
        let core = model.core();
        let feeds: Vec<_> = model
            .clusters
            .iter()
            .map(|summary| core.telemetry(&summary.info.name))
            .collect();
        let labels: Vec<(&str, String)> = model
            .clusters
            .iter()
            .map(|summary| {
                let id = summary.info.name.as_str();
                (id, names.with_real(id))
            })
            .collect();
        let labels = distinct_labels(&labels);
        let inputs: Vec<TrackerInput<'_>> = model
            .clusters
            .iter()
            .zip(&feeds)
            .zip(labels)
            .map(|((summary, feed), label)| {
                let id = summary.info.name.as_str();
                let slot = verified_discovery(model.discovery.get(id), summary.generation);
                let on = model.settings.alerts_on(id, summary.info.server.as_deref());
                TrackerInput {
                    cluster: Arc::from(id),
                    origin: AlertOrigin::of(summary),
                    label,
                    reading: if on {
                        feed_reading(
                            summary,
                            model.settings.alert_mode_for(id),
                            feed.as_ref(),
                            slot,
                        )
                    } else {
                        Reading::Muted
                    },
                }
            })
            .collect();
        let notifications = self.tracker.observe(&inputs, Timestamp::now());
        drop(inputs);
        if notifications.is_empty() {
            return;
        }
        if self.awaiting_permission {
            for notification in notifications {
                self.tracker
                    .delivered(&notification.tag, Delivery::AwaitingPermission);
                if self.held.len() == MAX_RECENT {
                    self.held.pop_front();
                }
                self.held.push_back(notification);
            }
            self.delivery = Some(Delivery::AwaitingPermission);
            cx.notify();
            return;
        }
        self.submit(notifications, cx);
    }

    /// Sends a batch off the UI thread. Turning alerts off stops it before its next notification,
    /// and whatever it was still doing then is not reported.
    fn submit(&mut self, notifications: Vec<Notification>, cx: &mut Context<Self>) {
        self.delivery = Some(Delivery::Sending);
        cx.notify();
        let sender = Arc::clone(&self.sender);
        let session = Arc::clone(&self.session);
        let started = session.load(Ordering::SeqCst);
        let muted = Arc::clone(&self.muted);
        let since = muted.lock().expect("the muted set").turned_off.clone();
        cx.spawn(async move |this, cx| {
            let answers = cx
                .background_spawn(async move {
                    send_batch(
                        &*sender,
                        &notifications,
                        || session.load(Ordering::SeqCst) == started,
                        |cluster| muted.lock().expect("the muted set").blocks(cluster, &since),
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.session.load(Ordering::SeqCst) != started {
                    return;
                }
                if answers.is_empty() && this.delivery == Some(Delivery::Sending) {
                    this.delivery = None;
                }
                for (tag, answer) in answers {
                    let delivery = match answer {
                        Ok(()) => Delivery::Accepted,
                        Err(error) => Delivery::Refused(error),
                    };
                    this.tracker.delivered(&tag, delivery.clone());
                    this.delivery = Some(delivery);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The alert's cluster if it is still the one that raised it. Otherwise shows why not.
    fn checked_cluster(&self, tag: &str, cx: &mut Context<Self>) -> Option<(RecentAlert, String)> {
        let Some(recent) = self.tracker.find(tag).cloned() else {
            self.refuse("This alert is no longer available.".to_owned(), cx);
            return None;
        };
        let context = recent.notification.cluster.to_string();
        let model = self.model.read(cx);
        let label = model.names().exact(&context);
        match check_origin(&context, &recent.origin, &model.clusters, &label) {
            Ok(_) => Some((recent, context)),
            Err(why) => {
                self.refuse(why, cx);
                None
            }
        }
    }

    fn refuse(&self, why: String, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.last_error = Some(why);
            cx.notify();
        });
    }

    /// Opens the overview of the cluster that raised the alert, by its exact context.
    pub fn open(&mut self, tag: &str, cx: &mut Context<Self>) {
        if let Some((recent, context)) = self.checked_cluster(tag, cx) {
            self.activate(&recent.origin, context, AfterActivation::ShowOverview, cx);
        }
    }

    /// The overview waiting for the workspace, if any.
    pub fn wanted_overview(&self) -> Option<&AlertOverview> {
        self.overview.as_ref()
    }

    /// The workspace showed the overview, or moved to another cluster first.
    pub fn clear_overview(&mut self) {
        self.overview = None;
    }

    #[cfg(test)]
    pub(in crate::ui) fn want_overview(&mut self, context: &str, generation: u64) {
        self.overview = Some(AlertOverview {
            context: context.to_owned(),
            generation,
        });
    }

    /// Has the core activate `context` if it still is `origin`, and only then does `after`.
    fn activate(
        &mut self,
        origin: &AlertOrigin,
        context: String,
        after: AfterActivation,
        cx: &mut Context<Self>,
    ) {
        self.overview = None;
        let label = self.model.read(cx).names().exact(&context);
        let overview = AlertOverview {
            context: context.clone(),
            generation: origin.generation,
        };
        let answer = self.model.update(cx, |model, _| {
            model.activate_pinned(&context, origin.pin.clone(), origin.generation)
        });
        cx.spawn(async move |this, cx| {
            let answer = answer.await;
            let _ = this.update(cx, |this, cx| match answer {
                Ok(()) => match after {
                    AfterActivation::ShowOverview => {
                        this.overview = Some(overview);
                        cx.notify();
                    }
                    AfterActivation::CopyPrompt(recent) => this.copy_prompt(*recent, cx),
                },
                Err(refused) => this.refuse(refusal_text(refused, &label), cx),
            });
        })
        .detach();
    }

    /// The prompt for `recent`, or why Investigate is unavailable. Checks the alert's identity,
    /// exposure and scope as they are now, so the menu and the click cannot disagree.
    fn investigation(&self, recent: &RecentAlert, cx: &App) -> Result<String, String> {
        self.prompt_under(recent, &self.model.read(cx).settings, cx)
    }

    /// Copies `recent`'s prompt if the alert's identity and both the settings the app holds and
    /// those saved on disk still allow it. A scope narrowed meanwhile may not have reached the
    /// app yet, so what is saved decides too, and a file that cannot be read allows nothing.
    fn copy_prompt(&mut self, recent: RecentAlert, cx: &mut Context<Self>) {
        const UNCONFIRMED: &str = "Flint could not confirm this cluster's AI access scope.";
        let Some(dir) = cx.try_global::<SettingsDir>().map(|dir| dir.0.clone()) else {
            self.refuse(UNCONFIRMED.to_owned(), cx);
            return;
        };
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn(async move { load_settings(&dir) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let prompt = match saved {
                    Ok(saved) => this
                        .investigation(&recent, cx)
                        .and_then(|_| this.prompt_under(&recent, &saved, cx)),
                    Err(_) => Err(UNCONFIRMED.to_owned()),
                };
                match prompt {
                    Ok(prompt) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(prompt));
                        this.show_note("Prompt copied", cx);
                    }
                    Err(why) => this.refuse(why, cx),
                }
            });
        })
        .detach();
    }

    /// The prompt for `recent` under `settings`, or why there is none.
    fn prompt_under(
        &self,
        recent: &RecentAlert,
        settings: &Settings,
        cx: &App,
    ) -> Result<String, String> {
        let context = &*recent.notification.cluster;
        let model = self.model.read(cx);
        let label = model.names().exact(context);
        check_origin(context, &recent.origin, &model.clusters, &label)?;
        let discovered = verified_discovery(model.discovery.get(context), recent.origin.generation)
            .map(|d| DiscoveredKinds {
                kinds: d.kinds.clone(),
                skipped_groups: d.incomplete_groups.clone(),
                stale_groups: Vec::new(),
            });
        // Exposure is pinned to the server and its proxy, as the context shows them now.
        let granted = model
            .clusters
            .iter()
            .find(|c| c.info.name == context && c.info.server == recent.origin.pin.server)
            .and_then(|c| c.info.grant_server());
        match event_prompt(
            &recent.event,
            settings,
            granted.as_deref(),
            discovered.as_ref(),
        ) {
            InvestigatePrompt::Ready(prompt) => Ok(prompt),
            InvestigatePrompt::Disabled(reason) => Err(reason.message().to_owned()),
        }
    }

    /// Opens the alert's cluster and copies a read-only prompt, when its exposure allows one.
    pub fn investigate(&mut self, tag: &str, cx: &mut Context<Self>) {
        let Some((recent, context)) = self.checked_cluster(tag, cx) else {
            return;
        };
        match self.investigation(&recent, cx) {
            Ok(_) => {
                let after = AfterActivation::CopyPrompt(Box::new(recent.clone()));
                self.activate(&recent.origin, context, after, cx);
            }
            Err(why) => self.refuse(why, cx),
        }
    }

    /// Saves `cluster`'s own alert choice, for the server it points at now.
    fn set_cluster_choice(
        &mut self,
        cluster: &str,
        choice: ClusterAlertChoice,
        cx: &mut Context<Self>,
    ) {
        let server = self
            .model
            .read(cx)
            .clusters
            .iter()
            .find(|c| c.info.name == cluster)
            .and_then(|c| c.info.server.clone());
        let Some(server) = server else {
            self.refuse(
                "This cluster has no API server, so it cannot have its own alert setting."
                    .to_owned(),
                cx,
            );
            return;
        };
        let cluster = cluster.to_owned();
        self.model.update(cx, |model, _| {
            model.set_cluster_alerts(cluster, server, choice.stored());
        });
    }

    fn show_note(&mut self, note: &'static str, cx: &mut Context<Self>) {
        self.note = Some(note);
        cx.notify();
        self._note = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE_TIME).await;
            let _ = this.update(cx, |this, cx| {
                this.note = None;
                cx.notify();
            });
        });
    }

    #[cfg(any(test, feature = "automation"))]
    pub fn view_state(&self) -> AlertsState {
        AlertsState {
            enabled: self.enabled,
            muted: {
                let shared = self.muted.lock().expect("the muted set");
                let mut muted: Vec<String> = shared.now.iter().cloned().collect();
                muted.sort();
                muted
            },
            delivery: self.delivery.as_ref().map_or("none", Delivery::state),
            checked: self.tracker.checked.iter().map(|c| c.to_string()).collect(),
            recent: self
                .tracker
                .recent()
                .map(|r| AlertRowState {
                    tag: r.notification.tag.clone(),
                    cluster: r.notification.cluster.to_string(),
                    title: r.notification.title.clone(),
                    body: r.notification.body.clone(),
                    delivery: r.delivery.state(),
                })
                .collect(),
        }
    }
}

/// Starts notifications for the app and routes a click on one back to its alert.
pub fn install(model: &Entity<ClustersModel>, cx: &mut App) {
    let center = cx.new(|cx| AlertCenter::new(model.clone(), platform_sender(), cx));
    cx.set_global(Alerts(center.clone()));
    let toggle_model = model.clone();
    cx.on_action(move |_: &ToggleDesktopNotifications, cx| {
        toggle_model.update(cx, |model, _| {
            let enabled = model.settings.desktop_notifications;
            model.set_desktop_notifications(!enabled);
        });
    });
    let c = center.clone();
    cx.on_action(move |action: &OpenAlert, cx| {
        c.update(cx, |center, cx| center.open(&action.tag, cx));
    });
    let c = center.clone();
    cx.on_action(move |action: &InvestigateAlert, cx| {
        c.update(cx, |center, cx| center.investigate(&action.tag, cx));
    });
    let c = center.clone();
    cx.on_action(move |action: &SetClusterAlerts, cx| {
        c.update(cx, |center, cx| {
            center.set_cluster_choice(&action.cluster, action.choice, cx);
        });
    });
    cx.on_system_notification_response(move |response, cx| {
        cx.activate(true);
        center.update(cx, |center, cx| center.open(&response.tag, cx));
    });
}

/// A menu row: the whole title, whose label tells clusters apart, then the shortened body.
fn menu_row(notification: &Notification) -> String {
    format!("{}: {}", notification.title, clean(&notification.body))
}

/// The header's notifications menu: the setting, what it covers, what the OS last answered, and
/// the latest alerts. `None` in a window without a running center.
pub fn render_button(cx: &App) -> Option<impl IntoElement + use<>> {
    let center = cx.try_global::<Alerts>()?.0.clone();
    let state = center.read(cx);
    let label = state.note.unwrap_or(if state.enabled {
        "Alerts: on"
    } else {
        "Alerts: off"
    });
    Some(
        Button::new("alerts-menu")
            .xsmall()
            .ghost()
            .label(label)
            .tab_stop(false)
            .tooltip("Desktop notifications for problems on watched clusters")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                menu_fit::dismiss_when_shrunk(window, cx);
                let state = center.read(cx);
                let mut menu = menu_fit::fit_menu(menu, window)
                    .item(
                        PopupMenuItem::new("Desktop notifications")
                            .checked(state.global)
                            .on_click(registry::on_click(ToggleDesktopNotifications)),
                    )
                    .label(COVERAGE)
                    .label(NOT_COVERED);
                let model = state.model.read(cx);
                if let Some(active) = model.active_cluster() {
                    let id = active.info.name.clone();
                    let server = active.info.server.as_deref();
                    let own =
                        ClusterAlertChoice::of(model.settings.cluster_alerts_choice(&id, server));
                    let default = if state.global { "on" } else { "off" };
                    menu = menu.separator().label("This cluster");
                    for (text, choice) in [
                        (
                            format!("Use the default ({default})"),
                            ClusterAlertChoice::Default,
                        ),
                        ("Alerts on".to_owned(), ClusterAlertChoice::On),
                        ("Alerts off".to_owned(), ClusterAlertChoice::Off),
                    ] {
                        menu = menu.item(
                            PopupMenuItem::new(text)
                                .checked(own == choice)
                                .disabled(server.is_none())
                                .on_click(registry::on_click(SetClusterAlerts {
                                    cluster: id.clone(),
                                    choice,
                                })),
                        );
                    }
                }
                if let Some(delivery) = &state.delivery {
                    menu = menu.separator().label(delivery.text());
                }
                let recent: Vec<&RecentAlert> = state.tracker.recent().collect();
                if !recent.is_empty() {
                    menu = menu.separator().label("Recent alerts");
                }
                for alert in recent {
                    let tag = alert.notification.tag.clone();
                    let text = menu_row(&alert.notification);
                    menu = menu.item(
                        PopupMenuItem::new(text)
                            .on_click(registry::on_click(OpenAlert { tag: tag.clone() })),
                    );
                    menu = menu.item(match state.investigation(alert, cx) {
                        Ok(_) => PopupMenuItem::new("Investigate with agent (copies a prompt)")
                            .on_click(registry::on_click(InvestigateAlert { tag })),
                        Err(why) => PopupMenuItem::new(format!("Investigate unavailable: {why}"))
                            .disabled(true),
                    });
                }
                menu
            }),
    )
}

#[cfg(test)]
#[path = "alert_center_tests.rs"]
mod tests;
