//! The Thresholds and Cluster overrides sections of Settings (#741, ADR-0017).
//!
//! **Thresholds** edits the global values of every [`ThresholdMetric`]: a number field per
//! metric, empty for the built-in default. **Cluster overrides** is a table of the clusters whose
//! thresholds, desktop alerts or watch mode differ from the global ones, one row per context,
//! with a Reset per row and Add override to give another cluster a row.
//!
//! Every edit goes into the Settings draft as the core's own write
//! ([`flint_core::settings::SettingsWrite`]). A number typed out of its metric's range, or not a
//! whole number, is held in the draft as rejected: it shows with why under the field and blocks
//! Save until it is fixed or reverted.
//!
//! A row is keyed by its context name. Its thresholds and alerts are bound to the server the
//! kubeconfig gives that context now (else the server its entries name), as the core binds
//! them; entries left for a server the context no longer points to apply to nothing and are not
//! shown. The watch mode override is per context name, as `cluster_watch_mode` is.
//!
//! A number field shows its value as a button; working it (click, Enter or Space) puts a text
//! field in its place, which Enter (or leaving it) enters into the draft and Esc drops.

// module-tests: settings_thresholds_tests.rs

use flint_core::WatchMode;
use flint_core::settings::{ClusterPin, Settings};
use flint_core::thresholds::ThresholdMetric;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, Sizable as _, StyledExt as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use super::registry;
use super::settings_draft::Field;
use super::settings_view::{SettingsPage, Stop, group, ring};

actions!(
    settings,
    [
        /// Drop what is typed in a threshold field, keeping the value it had.
        CancelThresholdEntry
    ]
);

/// Set a threshold in the Settings draft: the global value, or with `cluster` that cluster's
/// override. `value` is the text as typed; empty or left out clears it, so it follows the next
/// level down. A value the metric does not accept is kept as rejected and blocks Save.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditThreshold {
    #[serde(default)]
    pub cluster: Option<String>,
    pub metric: String,
    #[serde(default)]
    pub value: Option<String>,
}

/// Put a text field in place of a threshold's value, with focus in it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct TypeThreshold {
    #[serde(default)]
    pub cluster: Option<String>,
    pub metric: String,
}

/// A cluster's own choice of desktop alerts, in the Settings draft.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertsChoice {
    /// Follow the global desktop notifications setting.
    #[default]
    Default,
    On,
    Off,
}

impl AlertsChoice {
    fn of(enabled: Option<bool>) -> Self {
        match enabled {
            None => Self::Default,
            Some(true) => Self::On,
            Some(false) => Self::Off,
        }
    }

    pub fn enabled(self) -> Option<bool> {
        match self {
            Self::Default => None,
            Self::On => Some(true),
            Self::Off => Some(false),
        }
    }

    /// The choice a click moves to.
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::On,
            Self::On => Self::Off,
            Self::Off => Self::Default,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::On => "On",
            Self::Off => "Off",
        }
    }

    #[cfg(any(test, feature = "automation"))]
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// A cluster's own watch mode, in the Settings draft.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchChoice {
    /// Follow the default watch mode.
    #[default]
    Default,
    All,
    OnView,
}

impl WatchChoice {
    fn of(mode: Option<WatchMode>) -> Self {
        match mode {
            None => Self::Default,
            Some(WatchMode::All) => Self::All,
            Some(WatchMode::OnView) => Self::OnView,
        }
    }

    pub fn mode(self) -> Option<WatchMode> {
        match self {
            Self::Default => None,
            Self::All => Some(WatchMode::All),
            Self::OnView => Some(WatchMode::OnView),
        }
    }

    /// The choice a click moves to.
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::All,
            Self::All => Self::OnView,
            Self::OnView => Self::Default,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::All => "All kinds",
            Self::OnView => "Open views",
        }
    }

    #[cfg(any(test, feature = "automation"))]
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::All => "all",
            Self::OnView => "on_view",
        }
    }
}

/// Set a cluster's own desktop alerts in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditClusterAlerts {
    pub cluster: String,
    pub choice: AlertsChoice,
}

/// Set a cluster's own watch mode in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditClusterWatchMode {
    pub cluster: String,
    pub mode: WatchChoice,
}

/// Clear every override of a cluster (thresholds, alerts, watch mode) in the Settings draft,
/// and drop its row.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct ResetClusterOverrides {
    pub cluster: String,
}

/// Give a cluster a row in Cluster overrides. Without `cluster`, show or hide the list of
/// clusters to pick from.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct AddClusterOverride {
    #[serde(default)]
    pub cluster: Option<String>,
}

/// The metric named by its `settings.toml` key.
pub fn metric_named(key: &str) -> Option<ThresholdMetric> {
    ThresholdMetric::ALL.into_iter().find(|m| m.key() == key)
}

/// The metric's name in Settings.
pub fn metric_label(metric: ThresholdMetric) -> &'static str {
    match metric {
        ThresholdMetric::CpuPercent => "CPU",
        ThresholdMetric::MemoryPercent => "Memory",
        ThresholdMetric::RestartsLastHour => "Restarts",
        ThresholdMetric::WarningEventsLastHour => "Warning events",
        ThresholdMetric::PendingPodSeconds => "Pending pods",
    }
}

/// What the metric's number counts.
fn metric_unit(metric: ThresholdMetric) -> &'static str {
    match metric {
        ThresholdMetric::CpuPercent | ThresholdMetric::MemoryPercent => {
            "% of limit, else request (nodes: allocatable)"
        }
        ThresholdMetric::RestartsLastHour => "restarts in the last hour",
        ThresholdMetric::WarningEventsLastHour => "Warning events in the last hour",
        ThresholdMetric::PendingPodSeconds => "seconds Pending",
    }
}

/// The metric's column heading in the override table.
fn metric_heading(metric: ThresholdMetric) -> &'static str {
    match metric {
        ThresholdMetric::CpuPercent => "CPU %",
        ThresholdMetric::MemoryPercent => "Memory %",
        ThresholdMetric::RestartsLastHour => "Restarts/h",
        ThresholdMetric::WarningEventsLastHour => "Warnings/h",
        ThresholdMetric::PendingPodSeconds => "Pending s",
    }
}

/// Reads `text` as typed for `metric`: empty clears the value (`None`); otherwise a whole
/// number in the metric's range, or why not.
pub fn parse_threshold(metric: ThresholdMetric, text: &str) -> Result<Option<u32>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let range = metric.range();
    match text.parse::<u32>() {
        Ok(value) if range.contains(&value) => Ok(Some(value)),
        _ => Err(format!(
            "{} must be a whole number from {} to {}.",
            metric_label(metric),
            range.start(),
            range.end()
        )),
    }
}

/// A threshold the Settings view edits: global, or one cluster's override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThresholdTarget {
    pub cluster: Option<ClusterPin>,
    pub metric: ThresholdMetric,
}

impl ThresholdTarget {
    pub fn field(&self) -> Field {
        Field::threshold(self.cluster.as_ref(), self.metric)
    }

    /// The value set at this level, if any.
    pub fn explicit(&self, shown: &Settings) -> Option<u32> {
        shown
            .thresholds
            .explicit(self.cluster.as_ref(), self.metric)
    }

    /// The value that applies when this level sets none: the built-in default for a global
    /// value, the global value for a cluster's.
    pub fn inherited(&self, shown: &Settings) -> u32 {
        match self.cluster {
            None => self.metric.default_value(),
            Some(_) => shown
                .thresholds
                .global
                .get(self.metric)
                .unwrap_or(self.metric.default_value()),
        }
    }
}

/// A context the kubeconfig defines, and the server it points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownCluster {
    pub context: String,
    pub server: Option<String>,
}

/// One row of Cluster overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideRow {
    pub context: String,
    /// The server its thresholds and alerts are bound to; without one, only its watch mode can
    /// be set.
    pub server: Option<String>,
    /// Whether the kubeconfig defines the context now.
    pub known: bool,
}

impl OverrideRow {
    pub fn pin(&self) -> Option<ClusterPin> {
        Some(ClusterPin::new(self.context.clone(), self.server.clone()?))
    }

    pub fn target(&self, metric: ThresholdMetric) -> Option<ThresholdTarget> {
        Some(ThresholdTarget {
            cluster: Some(self.pin()?),
            metric,
        })
    }

    /// Its own desktop alerts choice in `shown`.
    pub fn alerts(&self, shown: &Settings) -> AlertsChoice {
        AlertsChoice::of(
            self.server
                .as_deref()
                .and_then(|server| shown.cluster_alerts_choice(&self.context, Some(server))),
        )
    }

    /// Its own watch mode in `shown`.
    pub fn watch(&self, shown: &Settings) -> WatchChoice {
        WatchChoice::of(shown.cluster_watch_mode.get(&self.context).copied())
    }

    /// Whether anything of it differs from the global settings.
    fn differs(&self, shown: &Settings) -> bool {
        let thresholds = ThresholdMetric::ALL.into_iter().any(|metric| {
            self.target(metric)
                .is_some_and(|t| t.explicit(shown).is_some())
        });
        thresholds
            || self.alerts(shown) != AlertsChoice::Default
            || self.watch(shown) != WatchChoice::Default
    }
}

/// The rows of Cluster overrides over `shown` (the file with the draft on top): every context
/// with an override, and those `added` by Add override, by context name.
pub fn override_rows(
    shown: &Settings,
    clusters: &[KnownCluster],
    added: &[String],
) -> Vec<OverrideRow> {
    let mut contexts: Vec<&str> = shown
        .thresholds
        .clusters
        .iter()
        .map(|entry| entry.cluster.context.as_str())
        .chain(shown.cluster_alerts.iter().map(|e| e.context.as_str()))
        .chain(shown.cluster_watch_mode.keys().map(String::as_str))
        .chain(added.iter().map(String::as_str))
        .collect();
    contexts.sort_unstable();
    contexts.dedup();
    contexts
        .into_iter()
        .map(|context| row_for(shown, clusters, context))
        .filter(|row| added.contains(&row.context) || row.differs(shown))
        .collect()
}

/// `context`'s row: bound to the kubeconfig's server, else to the server its entries name.
fn row_for(shown: &Settings, clusters: &[KnownCluster], context: &str) -> OverrideRow {
    let known = clusters.iter().find(|c| c.context == context);
    let server = match known {
        Some(known) => known.server.clone(),
        None => shown
            .thresholds
            .clusters
            .iter()
            .find(|entry| entry.cluster.context == context)
            .map(|entry| entry.cluster.server.clone())
            .or_else(|| {
                shown
                    .cluster_alerts
                    .iter()
                    .find(|e| e.context == context)
                    .map(|e| e.server.clone())
            }),
    };
    OverrideRow {
        context: context.to_owned(),
        server,
        known: known.is_some(),
    }
}

/// The clusters Add override offers: those with a server and no row yet, in kubeconfig order.
pub fn add_candidates(clusters: &[KnownCluster], rows: &[OverrideRow]) -> Vec<String> {
    clusters
        .iter()
        .filter(|c| c.server.is_some() && !rows.iter().any(|r| r.context == c.context))
        .map(|c| c.context.clone())
        .collect()
}

/// A control of the two sections the keyboard cursor can stand on. Rows and candidates are by
/// position, as drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdStop {
    Global(ThresholdMetric),
    Cell(usize, ThresholdMetric),
    Alerts(usize),
    Watch(usize),
    Reset(usize),
    Add,
    Pick(usize),
}

impl ThresholdStop {
    /// The row it belongs to, for a control of a Cluster overrides row.
    pub fn row(self) -> Option<usize> {
        match self {
            Self::Cell(row, _) | Self::Alerts(row) | Self::Watch(row) | Self::Reset(row) => {
                Some(row)
            }
            Self::Global(_) | Self::Add | Self::Pick(_) => None,
        }
    }

    /// The same control on row `row`.
    pub fn at_row(self, row: usize) -> Self {
        match self {
            Self::Cell(_, metric) => Self::Cell(row, metric),
            Self::Alerts(_) => Self::Alerts(row),
            Self::Watch(_) => Self::Watch(row),
            Self::Reset(_) => Self::Reset(row),
            other => other,
        }
    }

    /// The element id of the control.
    pub fn id(self) -> SharedString {
        match self {
            Self::Global(metric) => format!("settings-threshold-{}", metric.key()).into(),
            Self::Cell(row, metric) => format!("settings-override-{row}-{}", metric.key()).into(),
            Self::Alerts(row) => format!("settings-override-{row}-alerts").into(),
            Self::Watch(row) => format!("settings-override-{row}-watch").into(),
            Self::Reset(row) => format!("settings-override-{row}-reset").into(),
            Self::Add => "settings-override-add".into(),
            Self::Pick(index) => format!("settings-override-pick-{index}").into(),
        }
    }
}

/// The Thresholds section's controls, in Tab order.
pub fn global_stops() -> Vec<ThresholdStop> {
    ThresholdMetric::ALL.map(ThresholdStop::Global).to_vec()
}

/// What Cluster overrides draws: its rows, and the clusters offered while Add override is open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverridesLayout {
    pub rows: usize,
    pub candidates: Option<usize>,
}

/// The Cluster overrides section's controls, in Tab order.
pub fn override_stops(layout: OverridesLayout) -> Vec<ThresholdStop> {
    let mut stops = Vec::new();
    for row in 0..layout.rows {
        stops.extend(ThresholdMetric::ALL.map(|m| ThresholdStop::Cell(row, m)));
        stops.extend([
            ThresholdStop::Alerts(row),
            ThresholdStop::Watch(row),
            ThresholdStop::Reset(row),
        ]);
    }
    stops.push(ThresholdStop::Add);
    stops.extend((0..layout.candidates.unwrap_or(0)).map(ThresholdStop::Pick));
    stops
}

/// The text field standing in for a threshold's value while it is typed.
pub struct ThresholdEntry {
    pub target: ThresholdTarget,
    pub stop: ThresholdStop,
    pub input: Entity<InputState>,
    /// The text it opened with: left as it is, the field enters nothing.
    pub opened: String,
    /// The settings as the file held them when it opened, which a draft it starts is over.
    pub file: std::sync::Arc<Settings>,
    pub _subscription: Subscription,
}

impl ThresholdEntry {
    /// The text in the field, if it differs from what it opened with.
    pub fn typed(&self, cx: &App) -> Option<String> {
        let text = self.input.read(cx).value().to_string();
        (text.trim() != self.opened.trim()).then_some(text)
    }
}

/// What the two sections hold besides the draft.
#[derive(Default)]
pub struct ThresholdsPage {
    /// Contexts given a row by Add override, which keep it until Reset or Revert.
    pub added: Vec<String>,
    /// Whether Add override's list of clusters is open.
    pub picking: bool,
    /// The threshold being typed.
    pub entry: Option<ThresholdEntry>,
    /// The cluster whose row (or Add override candidate) the keyboard cursor stands on, so the
    /// cursor follows it when rows or candidates before it come or go.
    pub cursor_row: Option<String>,
    /// The Add override candidates, scrolled on their own so the one under the keyboard cursor
    /// is in view however many there are.
    pub pick_scroll: ScrollHandle,
}

/// The Thresholds section, as the children of the scrolling content: its heading, then a
/// field per metric ([`scroll_index`]).
pub fn render_global(page: &SettingsPage, shown: &Settings, theme: &Theme) -> Vec<AnyElement> {
    let edited = ThresholdMetric::ALL
        .into_iter()
        .any(|metric| page.draft.is_edited(&Field::threshold(None, metric)));
    let heading = group(
        "Warning thresholds",
        "The cluster overview warns from these values, on every cluster without its own \
         (Cluster overrides). Leave a field empty for the built-in default.",
        edited,
        theme,
    )
    .into_any_element();
    let fields = ThresholdMetric::ALL.map(|metric| {
        let target = ThresholdTarget {
            cluster: None,
            metric,
        };
        let stop = ThresholdStop::Global(metric);
        let rejected = page.draft.rejected(&target.field());
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .w(px(120.))
                            .text_sm()
                            .child(metric_label(metric)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .child(number_cell(page, stop, &target, None, shown, 88., theme)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(metric_unit(metric)),
                    ),
            )
            .children(rejected.map(|r| error_text(stop.id(), &r.why, theme)))
            .into_any_element()
    });
    std::iter::once(heading).chain(fields).collect()
}

/// Which child of the scrolling content holds `stop`, as [`render_global`] and
/// [`render_overrides`] lay them out: the heading first, then one per field or row, then Add
/// override and its list.
pub fn scroll_index(stop: ThresholdStop, rows: usize) -> usize {
    match stop {
        ThresholdStop::Global(metric) => {
            1 + ThresholdMetric::ALL
                .iter()
                .position(|m| *m == metric)
                .unwrap_or(0)
        }
        ThresholdStop::Cell(row, _)
        | ThresholdStop::Alerts(row)
        | ThresholdStop::Watch(row)
        | ThresholdStop::Reset(row) => 1 + row,
        ThresholdStop::Add => 1 + rows,
        ThresholdStop::Pick(_) => 2 + rows,
    }
}

/// The tallest the Add override candidates get before they scroll, so the list fits in the
/// Settings content and the keyboard cursor can bring any candidate into view.
const PICK_LIST_HEIGHT: f32 = 160.;

/// The Cluster overrides section, as the children of the scrolling content: its heading, a
/// block per row (its controls wrapping to the width there is), Add override and its list.
pub fn render_overrides(
    page: &SettingsPage,
    shown: &Settings,
    rows: &[OverrideRow],
    candidates: &[String],
    theme: &Theme,
) -> Vec<AnyElement> {
    let edited = rows.iter().any(|row| row_edited(page, row));
    let mut heading = group(
        "Cluster overrides",
        "Clusters whose warning thresholds, desktop alerts or watch mode differ from the global \
         settings. An empty threshold shows the global value and follows it. Thresholds and \
         alerts are bound to the server the context points to now.",
        edited,
        theme,
    )
    .id("settings-overrides")
    .max_w(px(TABLE_WIDTH));
    if rows.is_empty() {
        heading = heading.child(
            div()
                .id("settings-overrides-empty")
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("No cluster differs from the global settings.")
                .test_support(),
        );
    }
    let mut items = vec![heading.into_any_element()];
    for (index, row) in rows.iter().enumerate() {
        items.push(
            div()
                .max_w(px(TABLE_WIDTH))
                .child(override_row(page, shown, index, row, theme))
                .into_any_element(),
        );
    }
    let picking = page.thresholds.picking;
    let add = h_flex().gap_2().child(ring(
        page,
        Stop::Threshold(ThresholdStop::Add),
        theme,
        Button::new(ThresholdStop::Add.id())
            .small()
            .label(if picking { "Cancel" } else { "Add override" })
            .on_click(registry::on_click(AddClusterOverride { cluster: None })),
    ));
    let picker = picking.then(|| {
        if candidates.is_empty() {
            return div()
                .id("settings-override-candidates")
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Every cluster in your kubeconfig with a server has a row already.")
                .test_support()
                .into_any_element();
        }
        v_flex()
            .id("settings-override-candidates")
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Choose a cluster:"),
            )
            .child(
                h_flex()
                    .id("settings-override-pick-list")
                    .flex_wrap()
                    .gap_1()
                    .max_h(px(PICK_LIST_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(&page.thresholds.pick_scroll)
                    .children(candidates.iter().enumerate().map(|(index, context)| {
                        let stop = ThresholdStop::Pick(index);
                        ring(
                            page,
                            Stop::Threshold(stop),
                            theme,
                            Button::new(stop.id())
                                .small()
                                .ghost()
                                .label(context.clone())
                                .on_click(registry::on_click(AddClusterOverride {
                                    cluster: Some(context.clone()),
                                })),
                        )
                    })),
            )
            .test_support()
            .into_any_element()
    });
    items.push(add.into_any_element());
    items.extend(picker);
    items
}

/// One row of Cluster overrides: the cluster and Reset, then a labelled control per threshold,
/// alerts and watch mode, then the errors of its rejected values.
fn override_row(
    page: &SettingsPage,
    shown: &Settings,
    index: usize,
    row: &OverrideRow,
    theme: &Theme,
) -> impl IntoElement {
    let bound = row.server.is_some();
    let mut errors = Vec::new();
    let mut controls = h_flex().flex_wrap().gap_2();
    for metric in ThresholdMetric::ALL {
        let stop = ThresholdStop::Cell(index, metric);
        let control = match row.target(metric) {
            Some(target) => {
                if let Some(rejected) = page.draft.rejected(&target.field()) {
                    errors.push(error_text(stop.id(), &rejected.why, theme));
                }
                number_cell(
                    page,
                    stop,
                    &target,
                    Some(&row.context),
                    shown,
                    CELL_WIDTH,
                    theme,
                )
            }
            None => ring(
                page,
                Stop::Threshold(stop),
                theme,
                Button::new(stop.id())
                    .small()
                    .ghost()
                    .w(px(CELL_WIDTH))
                    .label("–")
                    .tooltip("This context has no server to bind an override to.")
                    .disabled(true),
            )
            .into_any_element(),
        };
        controls = controls.child(labelled(metric_heading(metric), control, theme));
    }
    let alerts = row.alerts(shown);
    let watch = row.watch(shown);
    let alerts_stop = ThresholdStop::Alerts(index);
    let watch_stop = ThresholdStop::Watch(index);
    let reset_stop = ThresholdStop::Reset(index);
    controls = controls
        .child(labelled(
            "Alerts",
            ring(
                page,
                Stop::Threshold(alerts_stop),
                theme,
                Button::new(alerts_stop.id())
                    .small()
                    .when(alerts == AlertsChoice::Default, |b| b.ghost())
                    .w(px(CHOICE_WIDTH))
                    .accessibility_label(format!(
                        "Desktop alerts for {}: {}",
                        row.context,
                        alerts.label()
                    ))
                    .label(alerts.label())
                    .tooltip("Desktop notifications for this cluster: Default follows Alerts.")
                    .disabled(!bound)
                    .on_click(registry::on_click(EditClusterAlerts {
                        cluster: row.context.clone(),
                        choice: alerts.next(),
                    })),
            ),
            theme,
        ))
        .child(labelled(
            "Watching",
            ring(
                page,
                Stop::Threshold(watch_stop),
                theme,
                Button::new(watch_stop.id())
                    .small()
                    .when(watch == WatchChoice::Default, |b| b.ghost())
                    .w(px(CHOICE_WIDTH))
                    .accessibility_label(format!("Watching for {}: {}", row.context, watch.label()))
                    .label(watch.label())
                    .tooltip("What this cluster watches: Default follows Watching.")
                    .on_click(registry::on_click(EditClusterWatchMode {
                        cluster: row.context.clone(),
                        mode: watch.next(),
                    })),
            ),
            theme,
        ));
    let name = if row.known {
        row.context.clone()
    } else {
        format!("{} (not in kubeconfig)", row.context)
    };
    v_flex()
        .id(SharedString::from(format!("settings-override-{index}")))
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "settings-override-{index}-cluster"
                        )))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_semibold()
                        .when(!row.known, |el| el.text_color(theme.muted_foreground))
                        .aria_label(name.clone())
                        .child(name)
                        .test_support(),
                )
                .child(ring(
                    page,
                    Stop::Threshold(reset_stop),
                    theme,
                    Button::new(reset_stop.id())
                        .small()
                        .accessibility_label(format!("Reset the overrides of {}", row.context))
                        .label("Reset")
                        .tooltip(
                            "Clear this cluster's overrides, so it follows the global settings.",
                        )
                        .on_click(registry::on_click(ResetClusterOverrides {
                            cluster: row.context.clone(),
                        })),
                )),
        )
        .child(controls)
        .children(errors)
}

/// `control` under its small label.
fn labelled(label: &'static str, control: impl IntoElement, theme: &Theme) -> impl IntoElement {
    v_flex()
        .flex_none()
        .gap_0p5()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(control)
}

const CELL_WIDTH: f32 = 72.;
const CHOICE_WIDTH: f32 = 88.;
const TABLE_WIDTH: f32 = 640.;

/// Whether anything of `row` waits in the draft.
fn row_edited(page: &SettingsPage, row: &OverrideRow) -> bool {
    let draft = &page.draft;
    if draft.is_edited(&Field::ClusterWatchMode(row.context.clone())) {
        return true;
    }
    let Some(pin) = row.pin() else {
        return false;
    };
    draft.is_edited(&Field::cluster_alerts(&pin))
        || ThresholdMetric::ALL
            .into_iter()
            .any(|metric| draft.is_edited(&Field::threshold(Some(&pin), metric)))
}

/// Whether a field of `section` holds an edit, for the section list's dot.
pub fn holds(field: &Field, overrides: bool) -> bool {
    match field {
        Field::Threshold { cluster, .. } => cluster.is_some() == overrides,
        Field::ClusterAlerts(_) | Field::ClusterWatchMode(_) => overrides,
        _ => false,
    }
}

/// A threshold's value as a button that puts a text field in its place, or that text field
/// while it is typed. `context` is the row's, for an override.
fn number_cell(
    page: &SettingsPage,
    stop: ThresholdStop,
    target: &ThresholdTarget,
    context: Option<&str>,
    shown: &Settings,
    width: f32,
    theme: &Theme,
) -> AnyElement {
    if let Some(entry) = page
        .thresholds
        .entry
        .as_ref()
        .filter(|e| e.target.field() == target.field())
    {
        return div()
            .w(px(width))
            .child(
                Input::new(&entry.input)
                    .id(SharedString::from(format!("{}-input", stop.id())))
                    .aria_label(field_name(target.metric, context))
                    .small(),
            )
            .into_any_element();
    }
    let rejected = page.draft.rejected(&target.field());
    let explicit = target.explicit(shown);
    let inherited = target.inherited(shown);
    let label = match (rejected, explicit) {
        (Some(r), _) => r.text.clone(),
        (None, Some(value)) => value.to_string(),
        (None, None) => inherited.to_string(),
    };
    let tooltip = match (rejected, explicit, target.cluster.is_some()) {
        (Some(r), _, _) => r.why.clone(),
        (None, Some(_), _) => "Set here. Clear the field to follow the level below.".to_owned(),
        (None, None, false) => format!("The built-in default, {inherited}."),
        (None, None, true) => format!("Follows the global value, {inherited}."),
    };
    let cell = ring(
        page,
        Stop::Threshold(stop),
        theme,
        Button::new(stop.id())
            .small()
            .when(explicit.is_none() && rejected.is_none(), |b| b.ghost())
            .w(px(width))
            .accessibility_label(format!(
                "{}: {label}. {tooltip}",
                field_name(target.metric, context)
            ))
            .label(label)
            .tooltip(tooltip)
            .on_click(registry::on_click(TypeThreshold {
                cluster: context.map(str::to_owned),
                metric: target.metric.key().to_owned(),
            })),
    );
    match rejected {
        Some(_) => div()
            .rounded_md()
            .border_1()
            .border_color(theme.danger)
            .child(cell)
            .into_any_element(),
        None => cell.into_any_element(),
    }
}

/// The two sections as automation reads them, over `shown`.
#[cfg(any(test, feature = "automation"))]
pub fn view_state(
    page: &SettingsPage,
    shown: &Settings,
    clusters: &[KnownCluster],
) -> super::view_state::ThresholdsState {
    use super::view_state::{OverrideRowState, RejectedState, ThresholdsState};
    let rows = override_rows(shown, clusters, &page.thresholds.added);
    let candidates = page
        .thresholds
        .picking
        .then(|| add_candidates(clusters, &rows));
    ThresholdsState {
        global: ThresholdMetric::ALL
            .into_iter()
            .map(|m| (m.key(), shown.thresholds.global.get(m)))
            .collect(),
        overrides: rows
            .iter()
            .map(|row| OverrideRowState {
                cluster: row.context.clone(),
                known: row.known,
                bound: row.server.is_some(),
                thresholds: ThresholdMetric::ALL
                    .into_iter()
                    .map(|m| (m.key(), row.target(m).and_then(|t| t.explicit(shown))))
                    .collect(),
                alerts: row.alerts(shown).name(),
                watch_mode: row.watch(shown).name(),
            })
            .collect(),
        rejected: page
            .draft
            .all_rejected()
            .map(|(field, rejected)| RejectedState {
                field: field.key(),
                text: rejected.text.clone(),
                why: rejected.why.clone(),
            })
            .collect(),
        candidates,
        typing: page
            .thresholds
            .entry
            .as_ref()
            .map(|entry| entry.stop.id().to_string()),
    }
}

/// What a threshold field is, for a screen reader: `CPU threshold` or `CPU threshold for prod`.
fn field_name(metric: ThresholdMetric, context: Option<&str>) -> String {
    match context {
        None => format!("{} threshold", metric_label(metric)),
        Some(context) => format!("{} threshold for {context}", metric_label(metric)),
    }
}

/// A rejected value's inline error.
fn error_text(id: SharedString, why: &str, theme: &Theme) -> AnyElement {
    div()
        .id(SharedString::from(format!("{id}-error")))
        .text_xs()
        .text_color(theme.danger)
        .aria_label(why.to_owned())
        .child(why.to_owned())
        .test_support()
        .into_any_element()
}

#[cfg(test)]
mod unit_tests {
    use flint_core::WatchMode;
    use flint_core::settings::{ClusterAlerts, ClusterPin, Settings};
    use flint_core::thresholds::ThresholdMetric;

    use super::{
        AlertsChoice, KnownCluster, OverridesLayout, ThresholdStop, ThresholdTarget, WatchChoice,
        add_candidates, global_stops, metric_named, override_rows, override_stops, parse_threshold,
    };

    const PROD: &str = "https://prod.example:6443";

    fn known(context: &str, server: Option<&str>) -> KnownCluster {
        KnownCluster {
            context: context.to_owned(),
            server: server.map(str::to_owned),
        }
    }

    #[test]
    fn a_threshold_is_a_whole_number_in_its_range() {
        let cpu = ThresholdMetric::CpuPercent;
        assert_eq!(parse_threshold(cpu, " 80 "), Ok(Some(80)));
        assert_eq!(parse_threshold(cpu, ""), Ok(None));
        assert_eq!(parse_threshold(cpu, "  "), Ok(None));
        let why = "CPU must be a whole number from 1 to 100.";
        for bad in ["0", "101", "-5", "8.5", "abc", "99999999999"] {
            assert_eq!(parse_threshold(cpu, bad), Err(why.to_owned()), "{bad}");
        }
        let pending = ThresholdMetric::PendingPodSeconds;
        assert_eq!(parse_threshold(pending, "86400"), Ok(Some(86_400)));
        assert_eq!(
            parse_threshold(pending, "86401"),
            Err("Pending pods must be a whole number from 1 to 86400.".to_owned())
        );
        let restarts = ThresholdMetric::RestartsLastHour;
        assert_eq!(parse_threshold(restarts, "10000"), Ok(Some(10_000)));
        assert!(parse_threshold(restarts, "10001").is_err());
    }

    #[test]
    fn every_metric_is_named_by_its_key() {
        for metric in ThresholdMetric::ALL {
            assert_eq!(metric_named(metric.key()), Some(metric));
        }
        assert_eq!(metric_named("cpu"), None);
    }

    #[test]
    fn rows_list_only_clusters_that_differ() {
        let mut shown = Settings::default();
        let prod = ClusterPin::new("prod", PROD);
        shown
            .thresholds
            .set(Some(&prod), ThresholdMetric::CpuPercent, Some(70))
            .unwrap();
        shown.cluster_alerts.push(ClusterAlerts {
            context: "gone".into(),
            server: "https://gone.example".into(),
            enabled: false,
        });
        shown
            .cluster_watch_mode
            .insert("dev".into(), WatchMode::OnView);
        // An entry for a server the context no longer points to applies to nothing.
        shown.cluster_alerts.push(ClusterAlerts {
            context: "stale".into(),
            server: "https://old.example".into(),
            enabled: true,
        });
        let clusters = [
            known("prod", Some(PROD)),
            known("dev", Some("https://dev.example")),
            known("stale", Some("https://new.example")),
            known("quiet", Some("https://quiet.example")),
        ];
        let rows = override_rows(&shown, &clusters, &[]);
        let names: Vec<_> = rows.iter().map(|r| r.context.as_str()).collect();
        assert_eq!(names, ["dev", "gone", "prod"]);
        let gone = &rows[1];
        assert!(!gone.known);
        assert_eq!(gone.server.as_deref(), Some("https://gone.example"));
        assert_eq!(gone.alerts(&shown), AlertsChoice::Off);
        assert_eq!(rows[0].watch(&shown), WatchChoice::OnView);
        assert_eq!(
            rows[2]
                .target(ThresholdMetric::CpuPercent)
                .unwrap()
                .explicit(&shown),
            Some(70)
        );

        let added = override_rows(&shown, &clusters, &["quiet".to_owned()]);
        assert!(added.iter().any(|r| r.context == "quiet"));
        assert_eq!(add_candidates(&clusters, &rows), ["stale", "quiet"]);
        assert_eq!(add_candidates(&clusters, &added), ["stale"]);
    }

    #[test]
    fn an_override_inherits_the_global_value() {
        let mut shown = Settings::default();
        let cpu = ThresholdMetric::CpuPercent;
        let global = ThresholdTarget {
            cluster: None,
            metric: cpu,
        };
        let prod = ThresholdTarget {
            cluster: Some(ClusterPin::new("prod", PROD)),
            metric: cpu,
        };
        assert_eq!(global.inherited(&shown), 90);
        assert_eq!(prod.inherited(&shown), 90);
        shown.thresholds.set(None, cpu, Some(80)).unwrap();
        assert_eq!(global.inherited(&shown), 90);
        assert_eq!(prod.inherited(&shown), 80);
    }

    #[test]
    fn the_overrides_cycle_through_their_choices() {
        let mut choice = AlertsChoice::Default;
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(choice.enabled());
            choice = choice.next();
        }
        assert_eq!(seen, [None, Some(true), Some(false)]);
        assert_eq!(choice, AlertsChoice::Default);
        let mut mode = WatchChoice::Default;
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(mode.mode());
            mode = mode.next();
        }
        assert_eq!(seen, [None, Some(WatchMode::All), Some(WatchMode::OnView)]);
    }

    #[test]
    fn tab_visits_every_cell_of_each_row() {
        let stops = override_stops(OverridesLayout {
            rows: 2,
            candidates: Some(3),
        });
        assert_eq!(stops.len(), 2 * 8 + 1 + 3);
        assert_eq!(
            stops[0],
            ThresholdStop::Cell(0, ThresholdMetric::CpuPercent)
        );
        assert_eq!(
            stops[8],
            ThresholdStop::Cell(1, ThresholdMetric::CpuPercent)
        );
        assert_eq!(stops[16], ThresholdStop::Add);
        assert_eq!(global_stops().len(), ThresholdMetric::ALL.len());
        let ids: std::collections::BTreeSet<_> = stops.iter().map(|s| s.id()).collect();
        assert_eq!(ids.len(), stops.len(), "ids are unique");
    }
}
