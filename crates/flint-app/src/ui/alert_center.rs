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
//!   [`investigate_prompt`]. It writes nothing and starts no agent.

// module-tests: alert_center_tests.rs

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use flint_core::alerts::{AlertConfig, AlertEngine, AlertEvent, ClusterSnapshot};
use flint_core::discovery::ClusterDiscovery;
use flint_core::handle::{ClusterSummary, ConnectionState, ReadPin};
use flint_core::problems::ProblemKind;
use flint_core::read::DiscoveredKinds;
use flint_core::settings::WatchMode;
use flint_core::telemetry::TelemetrySnapshot;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::*;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use super::clusters::ClustersModel;
use super::registry;
use crate::notifications::investigate_prompt::{InvestigatePrompt, investigate_prompt};
use crate::notifications::{
    DeliveryError, Notification, NotificationSender, clean, platform_sender,
};

/// How many past alerts the app keeps to open from.
const MAX_RECENT: usize = 20;

/// How many recent alerts the menu lists.
const MENU_ALERTS: usize = 5;

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
pub const COVERAGE: &str = "Only clusters on Live: all kinds are checked in the background.";

/// What it does not cover.
pub const NOT_COVERED: &str = "Clusters on Live: open views only have no background detection.";

/// Every kind of problem.
const ALL_KINDS: [ProblemKind; 6] = [
    ProblemKind::CrashLoop,
    ProblemKind::OomKilled,
    ProblemKind::ImagePull,
    ProblemKind::NodeNotReady,
    ProblemKind::JobFailed,
    ProblemKind::WorkloadShort,
];

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
    let label = clean(label);
    let Some(summary) = clusters.iter().find(|c| c.info.name == context) else {
        return Err(format!(
            "\"{label}\" was removed from kubeconfig, so this alert cannot be opened."
        ));
    };
    if !origin.holds(summary) {
        return Err(format!(
            "\"{label}\" changed since this alert was raised, so it was not opened."
        ));
    }
    if summary.paused {
        return Err(format!(
            "\"{label}\" is paused until its kubeconfig is confirmed, so it was not opened."
        ));
    }
    Ok(summary)
}

/// What is known about a cluster in one pass.
#[derive(Clone, Copy, Debug)]
pub enum Reading<'a> {
    /// Its feed is running.
    Feed {
        snapshot: &'a TelemetrySnapshot,
        discovery: Option<&'a ClusterDiscovery>,
    },
    /// It should be readable and is not: its connection failed or it is paused.
    Lost,
    /// Flint is not keeping it watched.
    NotWatched,
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

/// The API groups a kind of problem is read from. A pod's owner is read through its ReplicaSet.
fn groups_of(kind: ProblemKind) -> &'static [&'static str] {
    match kind {
        ProblemKind::CrashLoop | ProblemKind::OomKilled | ProblemKind::ImagePull => &["", "apps"],
        ProblemKind::NodeNotReady => &[""],
        ProblemKind::JobFailed => &["batch"],
        ProblemKind::WorkloadShort => &["apps"],
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
        .filter(|kind| groups_of(*kind).iter().any(|group| incomplete(group)))
        .collect()
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
    /// The OS accepted it, which does not prove it was shown.
    Accepted,
    Refused(DeliveryError),
}

impl Delivery {
    fn text(&self) -> String {
        match self {
            Self::Sending => "Sending the last notification…".to_owned(),
            Self::Accepted => "The system accepted the last notification.".to_owned(),
            Self::Refused(error) => error.message(),
        }
    }

    fn state(&self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::Accepted => "accepted",
            Self::Refused(DeliveryError::NotPermitted) => "not_permitted",
            Self::Refused(DeliveryError::NeedsAppBundle) => "needs_app_bundle",
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
    /// rather than a backlog.
    pub fn reset(&mut self) {
        self.engine = AlertEngine::new(AlertConfig::default());
        self.origins.clear();
        self.unwatched_since.clear();
        self.checked.clear();
    }

    fn forget(&mut self, cluster: &str) {
        self.engine.forget(cluster);
        self.origins.remove(cluster);
        self.unwatched_since.remove(cluster);
        self.checked.remove(cluster);
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
                Reading::Feed {
                    snapshot,
                    discovery,
                } => {
                    self.unwatched_since.remove(&input.cluster);
                    let mut unchecked = snapshot.unchecked.clone();
                    for kind in out_of_scope_kinds(discovery) {
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
                    tracing::warn!("desktop notifications: {limit}");
                    continue;
                }
            };
            self.origins
                .insert(Arc::clone(&input.cluster), input.origin.clone());
            for event in events {
                sent.push(self.record(event, input));
            }
        }
        sent
    }

    /// Drops a cluster's history once it has had no feed for [`UNWATCHED_GRACE`].
    fn note_unwatched(&mut self, cluster: &Arc<str>, now: Timestamp) {
        if !self.origins.contains_key(cluster) {
            return;
        }
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
            self.recent.pop_front();
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlertsState {
    pub enabled: bool,
    /// What the OS last answered: `none`, `sending`, `accepted`, `not_permitted`,
    /// `needs_app_bundle` or `failed`.
    pub delivery: &'static str,
    /// Clusters whose last pass could check every kind of problem.
    pub checked: Vec<String>,
    pub recent: Vec<AlertRowState>,
}

/// One alert in [`AlertsState`].
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
    enabled: bool,
    /// What the OS last answered, from turning on or from a notification.
    delivery: Option<Delivery>,
    note: Option<&'static str>,
    _note: Task<()>,
    _tasks: Vec<Task<()>>,
    _observe: Subscription,
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
        let observe = cx.observe(&model, |this, _, cx| this.sync_enabled(cx));
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
            delivery: None,
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
        let enabled = model.settings.desktop_notifications && model.settings_error.is_none();
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        self.tracker.reset();
        self.delivery = None;
        cx.notify();
        if !enabled {
            return;
        }
        let sender = Arc::clone(&self.sender);
        cx.spawn(async move |this, cx| {
            let answer = cx.background_spawn(async move { sender.prepare() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.enabled
                    && let Err(error) = answer
                {
                    this.delivery = Some(Delivery::Refused(error));
                    cx.notify();
                }
            });
        })
        .detach();
        self.pass(cx);
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
        let inputs: Vec<TrackerInput<'_>> = model
            .clusters
            .iter()
            .zip(&feeds)
            .map(|(summary, feed)| {
                let id = summary.info.name.as_str();
                let discovery = model
                    .discovery
                    .get(id)
                    .filter(|d| d.generation == summary.generation)
                    .and_then(|d| d.discovery.as_deref());
                let snapshot = feed.as_ref().map(|feed| &*feed.snapshot);
                TrackerInput {
                    cluster: Arc::from(id),
                    origin: AlertOrigin::of(summary),
                    label: names.with_real(id),
                    reading: reading(
                        summary,
                        model.settings.watch_mode_for(id),
                        snapshot,
                        discovery,
                    ),
                }
            })
            .collect();
        let notifications = self.tracker.observe(&inputs, Timestamp::now());
        drop(inputs);
        if notifications.is_empty() {
            return;
        }
        self.delivery = Some(Delivery::Sending);
        cx.notify();
        let sender = Arc::clone(&self.sender);
        cx.spawn(async move |this, cx| {
            let answers: Vec<(String, Result<(), DeliveryError>)> = cx
                .background_spawn(async move {
                    notifications
                        .iter()
                        .map(|n| (n.tag.clone(), sender.send(n)))
                        .collect()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
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

    /// Opens the cluster that raised the alert, by its exact context.
    pub fn open(&mut self, tag: &str, cx: &mut Context<Self>) {
        if let Some((_, context)) = self.checked_cluster(tag, cx) {
            self.model.update(cx, |model, _| model.activate(context));
        }
    }

    /// Opens the alert's cluster and copies a read-only prompt, when its exposure allows one.
    pub fn investigate(&mut self, tag: &str, cx: &mut Context<Self>) {
        let Some((recent, context)) = self.checked_cluster(tag, cx) else {
            return;
        };
        let AlertEvent::Raised(alert) = &recent.event else {
            self.refuse(
                "Only a single raised alert can be investigated. Open the cluster instead."
                    .to_owned(),
                cx,
            );
            return;
        };
        let model = self.model.read(cx);
        let discovered = model
            .discovery
            .get(&context)
            .filter(|d| d.generation == recent.origin.generation)
            .and_then(|d| d.discovery.as_deref())
            .map(|d| DiscoveredKinds {
                kinds: d.kinds.clone(),
                skipped_groups: d.incomplete_groups.clone(),
                stale_groups: Vec::new(),
            });
        let prompt = investigate_prompt(
            alert,
            &model.settings,
            recent.origin.pin.server.as_deref(),
            discovered.as_ref(),
        );
        match prompt {
            InvestigatePrompt::Ready(prompt) => {
                cx.write_to_clipboard(ClipboardItem::new_string(prompt));
                self.model.update(cx, |model, _| model.activate(context));
                self.show_note("Prompt copied", cx);
            }
            InvestigatePrompt::Disabled(reason) => self.refuse(reason.message().to_owned(), cx),
        }
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

    pub fn view_state(&self) -> AlertsState {
        AlertsState {
            enabled: self.enabled,
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
    cx.on_system_notification_response(move |response, cx| {
        cx.activate(true);
        center.update(cx, |center, cx| center.open(&response.tag, cx));
    });
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
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, cx| {
                let state = center.read(cx);
                let mut menu = menu
                    .item(
                        PopupMenuItem::new("Desktop notifications")
                            .checked(state.enabled)
                            .on_click(registry::on_click(ToggleDesktopNotifications)),
                    )
                    .label(COVERAGE)
                    .label(NOT_COVERED);
                if let Some(delivery) = &state.delivery {
                    menu = menu.separator().label(delivery.text());
                }
                let recent: Vec<&RecentAlert> = state.tracker.recent().take(MENU_ALERTS).collect();
                if !recent.is_empty() {
                    menu = menu.separator().label("Recent alerts");
                }
                for alert in recent {
                    let tag = alert.notification.tag.clone();
                    let text = clean(&format!(
                        "{}: {}",
                        alert.notification.title, alert.notification.body
                    ));
                    menu = menu.item(
                        PopupMenuItem::new(text)
                            .on_click(registry::on_click(OpenAlert { tag: tag.clone() })),
                    );
                    if matches!(alert.event, AlertEvent::Raised(_)) {
                        menu = menu.item(
                            PopupMenuItem::new("Investigate with agent (copies a prompt)")
                                .on_click(registry::on_click(InvestigateAlert { tag })),
                        );
                    }
                }
                menu
            }),
    )
}

#[cfg(test)]
#[path = "alert_center_tests.rs"]
mod tests;
