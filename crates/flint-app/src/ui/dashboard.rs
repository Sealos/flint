//! A cluster's dashboard (#101, ADR-0017): what needs attention now, CPU and memory over the last
//! hour, and recent Warning events.
//!
//! [`DashboardModel`] is a plain view model built from core types, so every state can be tested
//! without a window. [`DashboardView`] only draws it, and asks for a filtered list through
//! [`DashboardEvent`].
//!
//! Missing data is never shown as zero: a count that could not be read says so, and its tile does
//! not open a list.

use std::cmp::Reverse;
use std::sync::Arc;

use flint_core::activity::is_invisible;
use flint_core::dashboard::{
    ProblemCategory, WARNING_EVENT_WINDOW, count_pending_claims, count_recent_warnings,
};
use flint_core::metrics::MetricsError;
use flint_core::metrics_trend::{TrendResource, TrendScope, TrendStore};
use flint_core::telemetry::{
    Availability, CheckFailure, FailureCause, KindCheck, MetricsCheck, TelemetrySnapshot,
    WatchedKind,
};
use flint_core::thresholds::{ThresholdLevel, Thresholds};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;
use k8s_openapi::api::core::v1::{Event as KubeEvent, PersistentVolumeClaim};
use serde::{Deserialize, Deserializer, de};

use super::registry::{Bind, OnView};

#[path = "dashboard_feed.rs"]
mod feed;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "only tests ask a sparkline if it is empty")
)]
#[path = "sparkline.rs"]
mod sparkline;
#[path = "dashboard_tiles.rs"]
mod tiles;

pub use feed::{ProblemList, ProblemLists};
use sparkline::SparklineShape;

/// The most Warning events listed.
pub const MAX_WARNING_ROWS: usize = 20;
/// The longest event message kept, in characters.
const MAX_MESSAGE_CHARS: usize = 300;
/// How many of the newest steps may hold the current value. The series allows for one poll still
/// on its way; a second covers a slow poll without passing off an old sample as current.
const CURRENT_VALUE_STEPS: usize = 2;
/// What a tile shows in place of a number it does not have.
pub const NO_VALUE: &str = "—";
/// What a tile says when its count could not be read.
pub const CANNOT_BE_CHECKED: &str = "Cannot be checked";
/// How to install metrics-server, shown when the cluster does not serve the metrics API.
pub const INSTALL_COMMAND: &str = "kubectl apply -f https://github.com/kubernetes-sigs/\
                                   metrics-server/releases/latest/download/components.yaml";
/// What the hint says above [`INSTALL_COMMAND`].
pub const INSTALL_HINT: &str = "CPU and memory need metrics-server, which this cluster does not \
                                have. To install it:";
/// What the Warning events list says when there are none.
pub const NO_WARNINGS: &str = "No Warning events in the last hour.";
/// What the Warning events list says when events could not be read.
pub const WARNINGS_UNAVAILABLE: &str = "Warning events cannot be checked.";
/// What the metrics section says before the first poll has finished.
pub const METRICS_WAITING: &str = "Waiting for the first metrics sample.";

/// Open the list behind a problem tile.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = dashboard, no_json)]
pub struct OpenProblemList {
    pub category: ProblemCategory,
}

/// The registry probes an action with its default.
impl Default for OpenProblemList {
    fn default() -> Self {
        Self {
            category: ProblemCategory::ALL[0],
        }
    }
}

impl<'de> Deserialize<'de> for OpenProblemList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            category: String,
        }
        let args = Args::deserialize(deserializer)?;
        ProblemCategory::ALL
            .into_iter()
            .find(|category| category_name(*category) == args.category)
            .map(|category| Self { category })
            .ok_or_else(|| de::Error::custom(format!("unknown category {:?}", args.category)))
    }
}

/// Each category as [`OpenProblemList`] names it, in [`ProblemCategory::ALL`] order.
pub const CATEGORY_NAMES: [&str; ProblemCategory::ALL.len()] = [
    category_name(ProblemCategory::ALL[0]),
    category_name(ProblemCategory::ALL[1]),
    category_name(ProblemCategory::ALL[2]),
    category_name(ProblemCategory::ALL[3]),
    category_name(ProblemCategory::ALL[4]),
    category_name(ProblemCategory::ALL[5]),
];

const fn category_name(category: ProblemCategory) -> &'static str {
    match category {
        ProblemCategory::FailingPods => "failing_pods",
        ProblemCategory::NotReadyNodes => "not_ready_nodes",
        ProblemCategory::WarningEvents => "warning_events",
        ProblemCategory::PendingClaims => "pending_claims",
        ProblemCategory::FailedJobs => "failed_jobs",
        ProblemCategory::ShortWorkloads => "short_workloads",
    }
}

/// What the dashboard asks of whoever hosts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DashboardEvent {
    /// Show the list filtered to this category.
    OpenFilteredList(ProblemCategory),
}

/// What the dashboard is built from. Events and claims are read apart from the telemetry feed, so
/// the caller passes `None` for a list whose own read failed or is stale.
#[derive(Clone, Copy, Debug)]
pub struct DashboardInput<'a> {
    pub snapshot: &'a TelemetrySnapshot,
    pub trends: &'a TrendStore,
    pub thresholds: &'a Thresholds,
    pub warning_events: Option<&'a [KubeEvent]>,
    pub pending_claims: Option<&'a [PersistentVolumeClaim]>,
    pub now: Timestamp,
}

/// Everything the view draws.
#[derive(Clone, Debug, PartialEq)]
pub struct DashboardModel {
    pub status: DashboardStatus,
    pub tiles: Vec<ProblemTile>,
    pub metrics: MetricsPanel,
    pub warnings: WarningsPanel,
}

/// Whether the cluster could be checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DashboardStatus {
    Starting,
    Checked,
    /// Some kinds could not be read; the text names them.
    Partial(String),
    /// Nothing could be read; the text says why, when that is known.
    CannotBeChecked(String),
}

/// One problem count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemTile {
    pub category: ProblemCategory,
    pub label: &'static str,
    /// `None` when it could not be read.
    pub count: Option<u32>,
    pub level: ThresholdLevel,
}

impl ProblemTile {
    /// The number as shown.
    pub fn value(&self) -> String {
        self.count
            .map_or_else(|| NO_VALUE.to_owned(), |count| count.to_string())
    }

    /// Whether the tile opens a list. A count that could not be read has no list to trust.
    pub fn opens_list(&self) -> bool {
        self.count.is_some()
    }

    /// The tile in one line, for accessibility and tests.
    pub fn description(&self) -> String {
        match self.count {
            Some(count) => format!("{}: {count}", self.label),
            None => format!("{}: {}", self.label, CANNOT_BE_CHECKED.to_lowercase()),
        }
    }
}

/// The CPU and memory section.
#[derive(Clone, Debug, PartialEq)]
pub enum MetricsPanel {
    /// No poll has finished.
    Waiting,
    /// The cluster does not serve the metrics API.
    NotInstalled,
    /// The two tiles, with why they are out of date when the last poll failed.
    Tiles {
        cpu: MetricTile,
        memory: MetricTile,
        notice: Option<String>,
    },
}

/// One resource's current value and last hour.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricTile {
    pub label: &'static str,
    /// `None` when there is no current sample.
    pub value: Option<String>,
    pub shape: Arc<SparklineShape>,
}

impl MetricTile {
    /// The value as shown.
    pub fn shown_value(&self) -> &str {
        self.value.as_deref().unwrap_or(NO_VALUE)
    }

    /// The tile in one line, for accessibility and tests.
    pub fn description(&self) -> String {
        format!("{}: {}", self.label, self.shown_value())
    }
}

/// The recent Warning events section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarningsPanel {
    Unavailable,
    /// Newest first, at most [`MAX_WARNING_ROWS`], out of `total`.
    Rows {
        rows: Vec<WarningRow>,
        total: usize,
    },
}

/// One Warning event. All text from the cluster is cleaned for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarningRow {
    pub age: String,
    pub reason: String,
    /// `Pod shop/web-0`.
    pub object: String,
    pub message: String,
    /// How many times it happened.
    pub occurrences: u32,
}

impl WarningRow {
    /// The row in one line, for accessibility and tests.
    pub fn label(&self) -> String {
        let repeats = if self.occurrences > 1 {
            format!(" (x{})", self.occurrences)
        } else {
            String::new()
        };
        format!(
            "{} {} {}{repeats}: {}",
            self.age, self.reason, self.object, self.message
        )
    }
}

impl DashboardModel {
    /// The dashboard of `input`.
    pub fn build(input: &DashboardInput<'_>) -> Self {
        Self {
            status: status(&input.snapshot.availability, input.snapshot),
            tiles: ProblemCategory::ALL
                .into_iter()
                .map(|category| tile(category, input))
                .collect(),
            metrics: metrics(input),
            warnings: warnings(input),
        }
    }
}

fn status(availability: &Availability, snapshot: &TelemetrySnapshot) -> DashboardStatus {
    match availability {
        Availability::Starting => DashboardStatus::Starting,
        Availability::Checked => DashboardStatus::Checked,
        Availability::Partial => {
            let missing: Vec<String> = snapshot
                .kinds
                .iter()
                .filter_map(|(kind, check)| {
                    Some(format!("{} ({})", kind_label(*kind), check_label(check)?))
                })
                .collect();
            DashboardStatus::Partial(format!(
                "Not everything could be checked: {}.",
                missing.join(", ")
            ))
        }
        Availability::CannotBeChecked(failure) => DashboardStatus::CannotBeChecked(match failure {
            Some(failure) => format!(
                "This cluster cannot be checked: {}.",
                failure_label(failure)
            ),
            None => "This cluster cannot be checked.".to_owned(),
        }),
    }
}

const fn kind_label(kind: WatchedKind) -> &'static str {
    match kind {
        WatchedKind::Pods => "Pods",
        WatchedKind::Nodes => "Nodes",
        WatchedKind::Jobs => "Jobs",
        WatchedKind::Deployments => "Deployments",
        WatchedKind::StatefulSets => "StatefulSets",
        WatchedKind::DaemonSets => "DaemonSets",
        WatchedKind::ReplicaSets => "ReplicaSets",
    }
}

/// Why a kind is not current, or `None` if it is.
fn check_label(check: &KindCheck) -> Option<String> {
    match check {
        KindCheck::Current => None,
        KindCheck::Starting => Some("still loading".to_owned()),
        KindCheck::TooLarge => Some("too many objects".to_owned()),
        KindCheck::Failed(failure) => Some(failure_label(failure)),
    }
}

fn failure_label(failure: &CheckFailure) -> String {
    match failure.cause {
        FailureCause::LoginFailed => "login failed".to_owned(),
        FailureCause::Forbidden => "no permission".to_owned(),
        FailureCause::NotServed => "not served by this cluster".to_owned(),
        FailureCause::Failed => failure.detail.to_string(),
    }
}

const fn category_label(category: ProblemCategory) -> &'static str {
    match category {
        ProblemCategory::FailingPods => "Failing pods",
        ProblemCategory::NotReadyNodes => "Nodes not ready",
        ProblemCategory::WarningEvents => "Warning events",
        ProblemCategory::PendingClaims => "Pending claims",
        ProblemCategory::FailedJobs => "Failed jobs",
        ProblemCategory::ShortWorkloads => "Short workloads",
    }
}

fn tile(category: ProblemCategory, input: &DashboardInput<'_>) -> ProblemTile {
    let count = tile_count(category, input);
    let level = match (category.threshold(), count) {
        (Some(metric), _) => input.thresholds.evaluate(metric, count.map(u64::from)),
        (None, None) => ThresholdLevel::Unknown,
        (None, Some(0)) => ThresholdLevel::Healthy,
        (None, Some(_)) => ThresholdLevel::Warning,
    };
    ProblemTile {
        category,
        label: category_label(category),
        count,
        level,
    }
}

/// The count of `category`. The telemetry feed does not watch events or claims, so those depend
/// only on the caller's own reads. Every other count needs a readable feed.
fn tile_count(category: ProblemCategory, input: &DashboardInput<'_>) -> Option<u32> {
    let snapshot = input.snapshot;
    match category {
        ProblemCategory::WarningEvents => input
            .warning_events
            .map(|events| count_recent_warnings(events, input.now)),
        ProblemCategory::PendingClaims => input.pending_claims.map(count_pending_claims),
        _ => matches!(
            snapshot.availability,
            Availability::Checked | Availability::Partial
        )
        .then(|| snapshot.counts.get(category))
        .flatten(),
    }
}

/// The cluster total of the newest poll that produced a sample, or `None` if that poll could not
/// total every node. A time before that poll makes the series end exactly at its step.
fn latest_poll_total(input: &DashboardInput<'_>, resource: TrendResource) -> Option<u64> {
    input
        .trends
        .series(
            &input.snapshot.cluster,
            TrendScope::Total,
            resource,
            Timestamp::MIN,
        )?
        .last()
        .flatten()
}

fn metrics(input: &DashboardInput<'_>) -> MetricsPanel {
    let notice = match &input.snapshot.metrics {
        MetricsCheck::Waiting => return MetricsPanel::Waiting,
        MetricsCheck::Unavailable(MetricsError::NotInstalled) => {
            return MetricsPanel::NotInstalled;
        }
        MetricsCheck::Current { .. } => None,
        MetricsCheck::Unavailable(MetricsError::Forbidden(_)) => {
            Some("CPU and memory cannot be checked: no permission to read metrics.".to_owned())
        }
        MetricsCheck::Unavailable(MetricsError::Failed(detail)) => {
            Some(format!("CPU and memory cannot be checked: {detail}"))
        }
    };
    // An empty answer records nothing in the trend, so only the poll's own node count shows it.
    let current = matches!(input.snapshot.metrics, MetricsCheck::Current { nodes } if nodes > 0);
    let tile = |label, resource, format: fn(u64) -> String| {
        let samples: Vec<Option<u64>> = input
            .trends
            .series(
                &input.snapshot.cluster,
                TrendScope::Total,
                resource,
                input.now,
            )
            .map(Iterator::collect)
            .unwrap_or_default();
        let recent = samples
            .iter()
            .rev()
            .take(CURRENT_VALUE_STEPS)
            .any(Option::is_some);
        let value = (current && recent)
            .then(|| latest_poll_total(input, resource))
            .flatten()
            .map(format);
        MetricTile {
            label,
            value,
            shape: Arc::new(SparklineShape::new(&samples)),
        }
    };
    MetricsPanel::Tiles {
        cpu: tile("CPU", TrendResource::Cpu, format_cpu),
        memory: tile("Memory", TrendResource::Memory, format_memory),
        notice,
    }
}

/// `nanos` of CPU in millicores, or cores from one core up.
fn format_cpu(nanos: u64) -> String {
    let millis = nanos / 1_000_000;
    if millis < 1000 {
        format!("{millis}m")
    } else {
        format!("{:.2} cores", nanos as f64 / 1e9)
    }
}

/// `bytes` in binary units.
fn format_memory(bytes: u64) -> String {
    const KIB: f64 = 1024.;
    let value = bytes as f64;
    if value < KIB * KIB {
        format!("{:.0} KiB", value / KIB)
    } else if value < KIB * KIB * KIB {
        format!("{:.0} MiB", value / (KIB * KIB))
    } else {
        format!("{:.1} GiB", value / (KIB * KIB * KIB))
    }
}

fn warnings(input: &DashboardInput<'_>) -> WarningsPanel {
    let Some(events) = input.warning_events else {
        return WarningsPanel::Unavailable;
    };
    let mut recent: Vec<(Timestamp, &KubeEvent)> = events
        .iter()
        .filter(|event| event.type_.as_deref() == Some("Warning"))
        .filter_map(|event| Some((last_seen(event)?, event)))
        .filter(|(seen, _)| input.now.duration_since(*seen) < WARNING_EVENT_WINDOW)
        .collect();
    let total = recent.len();
    recent.sort_by_key(|(seen, event)| {
        (
            Reverse(*seen),
            event.involved_object.namespace.clone(),
            event.involved_object.name.clone(),
            event.reason.clone(),
        )
    });
    let rows = recent
        .into_iter()
        .take(MAX_WARNING_ROWS)
        .map(|(seen, event)| warning_row(event, seen, input.now))
        .collect();
    WarningsPanel::Rows { rows, total }
}

fn warning_row(event: &KubeEvent, seen: Timestamp, now: Timestamp) -> WarningRow {
    let object = &event.involved_object;
    let name = match (&object.namespace, &object.name) {
        (Some(namespace), Some(name)) if !namespace.is_empty() => {
            format!("{}/{}", cleaned(namespace), cleaned(name))
        }
        (_, Some(name)) => cleaned(name),
        _ => NO_VALUE.to_owned(),
    };
    let object = match &object.kind {
        Some(kind) => format!("{} {name}", cleaned(kind)),
        None => name,
    };
    let occurrences = event
        .series
        .as_ref()
        .and_then(|series| series.count)
        .or(event.count)
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or(1)
        .max(1);
    WarningRow {
        age: age(now, seen),
        reason: event.reason.as_deref().map_or_else(String::new, cleaned),
        object,
        message: event.message.as_deref().map_or_else(String::new, cleaned),
        occurrences,
    }
}

/// The latest time `event` records, as the core's warning count reads it.
fn last_seen(event: &KubeEvent) -> Option<Timestamp> {
    let series = event.series.as_ref();
    [
        series
            .and_then(|s| s.last_observed_time.as_ref())
            .map(|t| t.0),
        event.last_timestamp.as_ref().map(|t| t.0),
        event.event_time.as_ref().map(|t| t.0),
        event.metadata.creation_timestamp.as_ref().map(|t| t.0),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// How long ago `seen` was. A time ahead of `now` (clock skew) reads as now.
fn age(now: Timestamp, seen: Timestamp) -> String {
    let seconds = now.duration_since(seen).as_secs();
    match seconds {
        ..=0 => "now".to_owned(),
        1..60 => format!("{seconds}s"),
        _ => format!("{}m", seconds / 60),
    }
}

/// `text` from the cluster, safe to draw on one line: control and invisible characters (which
/// could break a row or reorder its text) become spaces, and its length is bounded.
fn cleaned(text: &str) -> String {
    let mut chars = text.chars();
    let mut out: String = chars
        .by_ref()
        .take(MAX_MESSAGE_CHARS)
        .map(|c| {
            if c.is_control() || is_invisible(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Draws a [`DashboardModel`].
pub struct DashboardView {
    model: DashboardModel,
    /// The objects behind each tile, read together with `model`.
    lists: ProblemLists,
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// Keeps the model current while the view is open.
    feed: feed::Feed,
}

impl EventEmitter<DashboardEvent> for DashboardView {}

impl DashboardView {
    pub fn new(model: DashboardModel, cx: &mut Context<Self>) -> Self {
        Self {
            model,
            lists: ProblemLists::new(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            feed: feed::Feed::default(),
        }
    }

    /// The registered actions of an open dashboard, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<OpenProblemList>(Self::open_list)
    }

    /// The objects behind `category`'s tile, as last read.
    pub fn problem_list(&self, category: ProblemCategory) -> Option<&ProblemList> {
        self.lists.get(&category)
    }

    /// Shows `model` and the objects behind its tiles.
    pub fn set_data(&mut self, model: DashboardModel, lists: ProblemLists, cx: &mut Context<Self>) {
        self.lists = lists;
        self.set_model(model, cx);
    }

    /// Shows `model`, redrawing only if it differs.
    pub fn set_model(&mut self, model: DashboardModel, cx: &mut Context<Self>) {
        if self.model != model {
            self.model = model;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn model(&self) -> &DashboardModel {
        &self.model
    }

    /// A click may come from a frame older than the model, so the tile is checked again.
    fn open_list(&mut self, action: &OpenProblemList, _: &mut Window, cx: &mut Context<Self>) {
        let opens = self
            .model
            .tiles
            .iter()
            .any(|tile| tile.category == action.category && tile.opens_list());
        if opens {
            cx.emit(DashboardEvent::OpenFilteredList(action.category));
        }
    }
}

impl Focusable for DashboardView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DashboardView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .id("dashboard")
            .key_context("Dashboard")
            .track_focus(&self.focus)
            .map(|el| Self::bind_actions(OnView::new(el, cx)).into_element())
            .size_full()
            .min_w_0()
            .p_4()
            .gap_4()
            .bg(theme.background)
            .text_color(theme.foreground)
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .children(tiles::status_banner(&self.model.status, theme))
            .child(tiles::problem_tiles(
                &self.model.tiles,
                self.focus.clone(),
                theme,
            ))
            .child(tiles::metrics_section(&self.model.metrics, theme))
            .child(tiles::warnings_section(&self.model.warnings, theme))
    }
}

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;
